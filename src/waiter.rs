use core::{
    marker::PhantomPinned,
    mem::MaybeUninit,
    pin::Pin,
    sync::atomic::{AtomicPtr, Ordering},
    task::{Context, Poll, Waker},
};

// ensure at least 3 free bits so we have a room for the evict flag (bit index 2)
#[cfg_attr(any(feature = "evict", feature = "semaphore-total"), repr(align(8)))]
// or just 2 bits if we don't need that
#[repr(align(4))]
struct AlignedWaker(Waker);

#[derive(Debug)]
pub struct Waiter(AtomicPtr<AlignedWaker>);

/// If true, the waiter has exclusive access to the guarded resource
pub const WAITER_FLAG_HAS_LOCK: usize = 1;
/// This bit is a special bit used to prevent accidentally corrupting the memory of the ptr itself
/// while it's being read.  If set to 1, it is not being read, if set to 0 then it was just read
/// and may still be.
/// We clear this every time we look at the pointer, then set it again if it was set previously.
/// If the data part of this pointer is null we must wait for it to be set back to 1 before
/// modifying the waker itself.
/// This flag can never be false when a waker is present, thus we can take and drop without checking
/// this bit when we have ownership of the waker's storage.
const WAITER_FLAG_CAN_DROP: usize = 2;
/// This flag indicates that a higher priority waiter is queued.  Thus, if possible the current
/// holder should exit early.
/// It is possible for both HAS_LOCK and WANTS_EVICT to be set before the waiter is ever polled.
/// In this case, we still return the lock and it is up to the owner of the guard to release the
/// lock if that is desired.
#[cfg(any(feature = "evict", feature = "semaphore-total"))]
pub const WAITER_FLAG_WANTS_EVICT: usize = 4;
#[cfg(not(any(feature = "evict", feature = "semaphore-total")))]
pub const WAITER_FLAG_WANTS_EVICT: usize = 0;
const WAITER_FLAG_MASK: usize =
    WAITER_FLAG_HAS_LOCK | WAITER_FLAG_CAN_DROP | WAITER_FLAG_WANTS_EVICT;
const WAITER_PTR_MASK: usize = !WAITER_FLAG_MASK;

#[inline(always)]
fn get_flag(w: *mut AlignedWaker) -> usize {
    w as usize & WAITER_FLAG_MASK
}

impl Waiter {
    #[inline(always)]
    pub(crate) const fn new(has_lock: bool) -> Self {
        Self(AtomicPtr::new(core::ptr::without_provenance_mut(
            if has_lock {
                WAITER_FLAG_HAS_LOCK | WAITER_FLAG_CAN_DROP
            } else {
                WAITER_FLAG_CAN_DROP
            },
        )))
    }

    #[inline]
    fn notify(&self) {
        let ptr = self
            .0
            .fetch_and(WAITER_FLAG_MASK ^ WAITER_FLAG_CAN_DROP, Ordering::AcqRel);

        let waker_ptr = ptr.map_addr(|x| x & WAITER_PTR_MASK);
        // if ptr isn't null, read it first
        let maybe_waker = (!waker_ptr.is_null()).then(|| unsafe { waker_ptr.read() });

        // now that we've read the ptr (if it was set) we can restore ownership
        // note that it is impossible to have a state where both the waker ptr and the drop flag
        // are unset - we always set the drop flag when we set the address part of the ptr.
        //
        // PERF: marking this with likely (once it is stable) seems to bring a slight performance
        // increase
        if ptr as usize & WAITER_FLAG_CAN_DROP != 0 {
            self.0.fetch_or(WAITER_FLAG_CAN_DROP, Ordering::AcqRel);
        }

        if let Some(waker) = maybe_waker {
            waker.0.wake();
        }
    }

    #[inline]
    fn add_flag(&self, flag: usize) {
        let recv = self
            .0
            .fetch_or(flag, Ordering::AcqRel)
            .map_addr(|x| x & WAITER_PTR_MASK);

        // no need to notify if waker is not set - if it's not set, either we already took the
        // waker from another tag change, or it hasn't been polled yet. either way when we set
        // the waker we'll check for the bits, so no races here
        if (recv as usize) & WAITER_PTR_MASK != 0 {
            self.notify();
        }
    }

    #[inline]
    pub(crate) fn start(&self) {
        self.add_flag(WAITER_FLAG_HAS_LOCK)
    }

    #[cfg(feature = "evict")]
    #[inline]
    pub(crate) fn evict(&self) {
        self.add_flag(WAITER_FLAG_WANTS_EVICT)
    }

    #[inline]
    /// Clear waiter (must be called by owner of waker location)
    fn clear_waker(&self, storage: &mut MaybeUninit<AlignedWaker>) -> usize {
        let ptr = self.0.fetch_and(WAITER_FLAG_MASK, Ordering::AcqRel);
        let flags = ptr as usize & WAITER_FLAG_MASK;

        // safe to get data before ever looking at the lock, as ptr simply won't be set if
        let waker_ptr = ptr as usize & WAITER_PTR_MASK;

        // we took the data, which means that we have exclusive access to modify
        // (we don't need to touch the can drop flag as we have a mut ref to the storage itself, so
        // the storage can't be dropped)
        if waker_ptr != 0 {
            debug_assert!(
                waker_ptr == storage.as_ptr() as usize,
                "if a waker exists, it must be ours {:p} {:p} ({:p})",
                waker_ptr as *const (),
                storage,
                ptr,
            );
            unsafe { storage.assume_init_drop() };
            return flags;
        }

        // there isn't a waker anymore, but we may still be reading it. if so, we must wait before
        // we can change the value.
        if ptr as usize & WAITER_FLAG_CAN_DROP == 0 {
            while self.0.load(Ordering::Acquire) as usize & WAITER_FLAG_CAN_DROP == 0 {}
        }

        flags
    }

    #[allow(unused)]
    pub(crate) fn flags(&self) -> usize {
        get_flag(self.0.load(Ordering::Relaxed))
    }

    #[inline]
    /// Only called when exclusive access is held for the mutex (thus, the flag cannot be changed)
    pub(crate) fn has_lock(&self) -> bool {
        get_flag(self.0.load(Ordering::Relaxed)) & WAITER_FLAG_HAS_LOCK != 0
    }

    #[inline(always)]
    fn wait_for_flag(
        &self,
        cx: &mut Context<'_>,
        waker: &mut MaybeUninit<AlignedWaker>,
        target: usize,
    ) -> Poll<usize> {
        let flag = self.clear_waker(waker);
        if flag & target == target {
            return Poll::Ready(flag);
        }

        waker.write(AlignedWaker(cx.waker().clone()));
        //
        let existing = self.0.fetch_or(
            waker.as_ptr() as usize | WAITER_FLAG_CAN_DROP,
            Ordering::AcqRel,
        );

        // we set it, but it's possible we updated state in meantime. So we might be ready.
        if get_flag(existing) & target == 0 {
            // if not ready (likely case) return pending and leave waker in place
            return Poll::Pending;
        }

        // ok, so looks like we *did* reach the target state
        // so now we need to remove the waker (if still there - we could have been notified again)
        // and return

        Poll::Ready(self.clear_waker(waker))
    }
}

pub(crate) trait WaiterHandle {
    fn with_waker<T>(&self, f: impl FnOnce(&Waiter) -> T) -> T;
}

pub(crate) struct WaiterFlagFut<'a, H: WaiterHandle, const FLAG: usize> {
    handle: &'a H,
    waker: MaybeUninit<AlignedWaker>,
    _mustpin: PhantomPinned,
}

impl<'a, H: WaiterHandle, const FLAG: usize> WaiterFlagFut<'a, H, FLAG> {
    #[inline]
    pub(crate) fn new(handle: &'a H) -> Self {
        Self {
            handle,
            waker: MaybeUninit::uninit(),
            _mustpin: PhantomPinned,
        }
    }

    #[inline(always)]
    fn waker<'b>(self: Pin<&'b mut Self>) -> &'b mut MaybeUninit<AlignedWaker> {
        unsafe { self.map_unchecked_mut(|x| &mut x.waker) }.get_mut()
    }
}

impl<'a, H: WaiterHandle, const FLAG: usize> Drop for WaiterFlagFut<'a, H, FLAG> {
    #[inline]
    fn drop(&mut self) {
        self.handle.with_waker(|waker| {
            waker.clear_waker(&mut self.waker);
        });
    }
}

impl<'a, H: WaiterHandle, const FLAG: usize> Future for WaiterFlagFut<'a, H, FLAG> {
    type Output = usize;

    #[inline]
    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        self.handle
            .with_waker(move |waker| waker.wait_for_flag(cx, self.waker(), FLAG))
    }
}

#[repr(transparent)]
/// Util type used in mutex to remove the type from WaiterFlagFut.
/// (returning an impl Future seems to be noticeably faster than an await then returning
/// ()):with_waker
#[cfg(feature = "evict")]
pub(crate) struct VoidFut<F: Future>(pub F);

#[cfg(feature = "evict")]
impl<F: Future> Future for VoidFut<F> {
    type Output = ();

    #[inline]
    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        unsafe { self.map_unchecked_mut(|x| &mut x.0) }
            .poll(cx)
            .map(|_| ())
    }
}
