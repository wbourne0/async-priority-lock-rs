//! # Async Priority Lock
//! Primitive for priority-based locking of resources.
//! Locks are granted in order of priority, with the option for lock holders to allow for eviction
//! (via `evict` flag).
//!
//! Note that this is a "California eviction" - the holder is requested to release the lock, but
//! it's up to the holder of the guard to listen (via .evicted()) and release the lock.
//!
//! For no-std environments, the `no-std` feature can be enabled (although alloc is still needed).
#![cfg_attr(feature = "no-std", no_std)]
use core::{
    cell::UnsafeCell,
    fmt::{Debug, Display},
    format_args,
    mem::MaybeUninit,
    ops::{Deref, DerefMut},
    pin::Pin,
    sync::atomic::{AtomicPtr, Ordering},
    task::{Context, Poll, Waker},
};

#[cfg(feature = "no-std")]
extern crate alloc;
#[cfg(feature = "no-std")]
use alloc::boxed::Box;
#[cfg(not(feature = "no-std"))]
use std::boxed::Box;

#[cfg(not(feature = "no-std"))]
#[derive(Default)]
#[repr(transparent)]
struct Mutex<T>(std::sync::Mutex<T>);

#[cfg(feature = "no-std")]
#[derive(Default)]
#[repr(transparent)]
struct Mutex<T>(spin::Mutex<T>);

impl<T> Mutex<T> {
    const fn new(value: T) -> Self {
        #[cfg(not(feature = "no-std"))]
        return Self(std::sync::Mutex::new(value));

        #[cfg(feature = "no-std")]
        return Self(spin::Mutex::new(value));
    }

    #[cfg(not(feature = "no-std"))]
    #[inline(always)]
    fn lock(&self) -> impl Deref<Target = T> + DerefMut {
        self.0.lock().unwrap()
    }

    #[cfg(feature = "no-std")]
    #[inline(always)]
    fn lock(&self) -> impl Deref<Target = T> + DerefMut {
        self.0.lock()
    }
}

// ensure at least 3 free bits
#[cfg(feature = "evict")]
#[repr(align(8))]
struct AlignedWaker(Waker);
// ensure at least 2 free bits (only 2 are needed if evict flag isn't set)
#[cfg(not(feature = "evict"))]
#[repr(align(4))]
struct AlignedWaker(Waker);

/// If true, the waiter has exclusive access to the guarded resource
const WAITER_FLAG_HAS_LOCK: usize = 1;
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
#[cfg(feature = "evict")]
const WAITER_FLAG_WANTS_EVICT: usize = 4;
#[cfg(not(feature = "evict"))]
const WAITER_FLAG_WANTS_EVICT: usize = 0;
const WAITER_FLAG_MASK: usize =
    WAITER_FLAG_HAS_LOCK | WAITER_FLAG_CAN_DROP | WAITER_FLAG_WANTS_EVICT;
const WAITER_PTR_MASK: usize = !WAITER_FLAG_MASK;

#[inline(always)]
fn get_flag(w: *mut AlignedWaker) -> usize {
    w as usize & WAITER_FLAG_MASK
}

impl<P: Ord> PriorityMutexWaiter<P> {
    #[inline]
    fn notify(&self) {
        let ptr = self
            .waker
            .fetch_and(WAITER_FLAG_MASK ^ WAITER_FLAG_CAN_DROP, Ordering::AcqRel);

        let waker_ptr = ptr.map_addr(|x| x & WAITER_PTR_MASK);
        // if ptr isn't null, read it first
        let maybe_waker = (!waker_ptr.is_null()).then(|| unsafe { waker_ptr.read() });

        // now that we've read the ptr (if it was set) we can restore ownership
        // note that it is impossible to have a state where both the waker ptr and the drop flag
        // are unset - we always set the drop flag when we set the address part of the ptr.
        if ptr as usize & WAITER_FLAG_CAN_DROP != 0 {
            self.waker.fetch_or(WAITER_FLAG_CAN_DROP, Ordering::AcqRel);
        }

        if let Some(waker) = maybe_waker {
            waker.0.wake();
        }
    }

    #[inline]
    fn add_flag(&self, flag: usize) {
        let recv = self
            .waker
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
    fn start(&self) {
        self.add_flag(WAITER_FLAG_HAS_LOCK)
    }

    #[cfg(feature = "evict")]
    #[inline]
    fn evict(&self) {
        self.add_flag(WAITER_FLAG_WANTS_EVICT)
    }

    #[inline]
    /// Clear waiter (must be called by owner of waker location)
    fn clear_waker(&self, storage: &mut MaybeUninit<AlignedWaker>) -> usize {
        let ptr = self.waker.fetch_and(WAITER_FLAG_MASK, Ordering::AcqRel);
        let flags = ptr as usize & WAITER_FLAG_MASK;

        // safe to get data before ever looking at the lock, as ptr simply won't be set if
        let waker_ptr = ptr as usize & WAITER_PTR_MASK;

        // we took the data, which means that we have exclusive access to modify
        // (we don't need to touch the can drop flag as we have a mut ref to the storage itself, so
        // the storage can't be dropped)
        if waker_ptr != 0 {
            debug_assert!(
                waker_ptr == storage.as_ptr() as usize,
                "if a waker exists, it must be ours {:p} {:p}",
                ptr,
                storage
            );
            unsafe { storage.assume_init_drop() };
            return flags;
        }

        // there isn't a waker anymore, but we may still be reading it. if so, we must wait before
        // we can change the value.
        if ptr as usize & WAITER_FLAG_CAN_DROP == 0 {
            while self.waker.load(Ordering::Acquire) as usize & WAITER_FLAG_CAN_DROP == 0 {}
        }

        flags
    }

    #[inline]
    fn wait_for_flag(
        &self,
        cx: &mut Context<'_>,
        waker: &mut MaybeUninit<AlignedWaker>,
        target: usize,
    ) -> Poll<()> {
        if self.clear_waker(waker) & target == target {
            return Poll::Ready(());
        }

        waker.write(AlignedWaker(cx.waker().clone()));
        // Ok, sot the notifier
        let existing = self.waker.fetch_or(
            waker.as_ptr() as usize | WAITER_FLAG_CAN_DROP,
            Ordering::AcqRel,
        );

        // we set it, but it's possible we updated state in meantime. So we might be ready.
        if get_flag(existing) & target != target {
            // if not ready (likely case) return pending and leave waker in place
            return Poll::Pending;
        }

        // ok, so looks like we *did* reach the target state
        // so now we need to remove the waker (if still there - we could have been notified again)
        // and return
        self.clear_waker(waker);

        Poll::Ready(())
    }
}

struct WaiterFlagFut<'a, P: Ord, const FLAG: usize> {
    tracker: &'a PriorityMutexWaiter<P>,
    waker: MaybeUninit<AlignedWaker>,
}

impl<'a, P: Ord, const FLAG: usize> WaiterFlagFut<'a, P, FLAG> {
    fn new(tracker: &'a PriorityMutexWaiter<P>) -> Self {
        Self {
            tracker,
            waker: MaybeUninit::uninit(),
        }
    }
}

impl<'a, P: Ord, const FLAG: usize> Drop for WaiterFlagFut<'a, P, FLAG> {
    #[inline]
    fn drop(&mut self) {
        self.tracker.clear_waker(&mut self.waker);
    }
}

impl<'a, P: Ord, const FLAG: usize> Future for WaiterFlagFut<'a, P, FLAG> {
    type Output = ();

    fn poll(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        self.tracker
            .wait_for_flag(cx, &mut self.as_mut().waker, FLAG)
    }
}

struct PriorityMutexWaiter<P: Ord> {
    priority: P,
    waker: AtomicPtr<AlignedWaker>,
    // semantics:
    // can only be read or written to when lock on list is held
    next: UnsafeCell<Option<Pin<Box<Self>>>>,
    _must_pin: core::marker::PhantomPinned,
}

unsafe impl<P: Ord + Sync> Sync for PriorityMutexWaiter<P> {}

impl<P: Ord> PriorityMutexWaiter<P> {
    #[inline]
    fn next(&self) -> &mut Option<Pin<Box<Self>>> {
        unsafe { &mut *self.next.get() }
    }

    #[inline]
    fn new<'a>(holder: P, has_lock: bool) -> (Pin<Box<Self>>, &'a Self) {
        let pin = Box::pin(Self {
            priority: holder,
            waker: AtomicPtr::new(core::ptr::without_provenance_mut(if has_lock {
                WAITER_FLAG_HAS_LOCK | WAITER_FLAG_CAN_DROP
            } else {
                WAITER_FLAG_CAN_DROP
            })),
            next: UnsafeCell::default(),
            _must_pin: core::marker::PhantomPinned,
        });

        let ptr = &raw const *pin;
        (pin, unsafe { &*ptr })
    }
}

#[derive(Default)]
/// A mutex that distributes access by priority as opposed to just fifo / whoever gets it first.
/// If fifo isn't set, the current behavior is lifo - however this is may not always be the case.
/// Having fifo = false means it doesn't matter the order of items with the same priority (instead,
/// they will be queued in whichever order is fastest - currently, lifo)
///
/// If this is non-desirable, the type alias FIFOPriorityMutex can be used or the const param can
/// FIFO be set manually to true.
///
/// By default, a highor P is higher priorrity, but this can be reversed via setting the
/// LOWEST_FIRST const arg to true (or by using the LowestFirstPriorityMutex alias type.
pub struct PriorityMutex<P: Ord, T, const FIFO: bool = false, const LOWEST_FIRST: bool = false> {
    // PERF: Could later optimize this via using a pre-allocated block to avoid
    // having to hit the allocator every time we enqueue a waiter (probably index by
    // offset instead of pointer). though it'd likely be a bit less memory efficient
    // current thought process on how I'd do this is to have a pre-allocaated (and expandable)
    // slice/array of (Pin<Box<Waiter>>, next: usize) where next being usize::MAX indicates an empty entry.
    // This w/ an associated idx for head.  This would probably not reduce the scale of its
    // allocation.
    // For now not doing this as the complexity isn't needed (in real world, it's probably unlikely
    // to have many waiters for a single lock)
    head: Mutex<Option<Pin<Box<PriorityMutexWaiter<P>>>>>,
    data: UnsafeCell<T>,
}

#[cfg(feature = "serde")]
impl<'de, P: Ord, T, const FIFO: bool, const LOWEST_FIRST: bool> serde::Deserialize<'de>
    for PriorityMutex<P, T, FIFO, LOWEST_FIRST>
where
    T: serde::Deserialize<'de>,
{
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        Ok(Self::new(T::deserialize(deserializer)?))
    }
}

pub type FIFOPriorityMutex<P, T, const LOWEST_FIRST: bool = false> =
    PriorityMutex<P, T, true, LOWEST_FIRST>;
pub type LowestFirstPriorityMutex<P, T, const FIFO: bool = false> = PriorityMutex<P, T, FIFO, true>;

unsafe impl<P: Ord + Sync, T: Sync, const FIFO: bool, const LOWEST_FIRST: bool> Sync
    for PriorityMutex<P, T, FIFO, LOWEST_FIRST>
{
}

pub struct PriorityMutexGuard<'a, P: Ord, T, const FIFO: bool, const LOWEST_FIRST: bool> {
    mutex: &'a PriorityMutex<P, T, FIFO, LOWEST_FIRST>,
    node: &'a PriorityMutexWaiter<P>,
}

impl<'a, P: Ord, T, const FIFO: bool, const LOWEST_FIRST: bool> Display
    for PriorityMutexGuard<'a, P, T, FIFO, LOWEST_FIRST>
where
    T: Display,
{
    #[inline]
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        self.deref().fmt(f)
    }
}

impl<'a, P: Ord, T, const FIFO: bool, const LOWEST_FIRST: bool> Debug
    for PriorityMutexGuard<'a, P, T, FIFO, LOWEST_FIRST>
where
    T: Debug,
{
    #[inline]
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        self.deref().fmt(f)
    }
}

impl<'a, P: Ord, T, const FIFO: bool, const LOWEST_FIRST: bool> Deref
    for PriorityMutexGuard<'a, P, T, FIFO, LOWEST_FIRST>
{
    type Target = T;

    #[inline]
    fn deref(&self) -> &Self::Target {
        unsafe { &*self.mutex.data.get() }
    }
}

impl<'a, P: Ord, T, const FIFO: bool, const LOWEST_FIRST: bool> DerefMut
    for PriorityMutexGuard<'a, P, T, FIFO, LOWEST_FIRST>
{
    #[inline]
    fn deref_mut(&mut self) -> &mut Self::Target {
        unsafe { &mut *self.mutex.data.get() }
    }
}

#[cfg(feature = "evict")]
impl<'a, P: Ord, T, const FIFO: bool, const LOWEST_FIRST: bool>
    PriorityMutexGuard<'a, P, T, FIFO, LOWEST_FIRST>
{
    /// Returns a future which resolves when/if another, higher priority holder attempts to acquire
    /// the lock.
    ///
    /// Note: this is an associated method to avoid colision with `T`.  Invoke via
    /// `PriorityMutexGuard::evicted(&mut self)`.
    ///
    /// Cancel safety: this function is cancel safe
    #[inline]
    pub fn evicted(this: &mut Self) -> impl Future<Output = ()> {
        WaiterFlagFut::<'_, P, WAITER_FLAG_WANTS_EVICT>::new(&this.node)
    }
}

impl<'a, P: Ord, T, const FIFO: bool, const LOWEST_FIRST: bool> Drop
    for PriorityMutexGuard<'a, P, T, FIFO, LOWEST_FIRST>
{
    #[inline]
    fn drop(&mut self) {
        self.mutex.dequeue(self.node);
    }
}

/// Opaque marker type for try_lock result
#[derive(Debug)]
pub struct TryLockError;

impl Display for TryLockError {
    #[inline]
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        write!(f, "lock is already held")
    }
}

impl core::error::Error for TryLockError {}

impl<P: Ord, T, const FIFO: bool, const LOWEST_FIRST: bool> Debug
    for PriorityMutex<P, T, FIFO, LOWEST_FIRST>
where
    T: Debug,
    P: Default,
{
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        let mut d = f.debug_tuple("PriorityMutex");
        match self.try_lock(P::default()) {
            Ok(data) => d.field(&data.deref()),
            Err(_) => d.field(&format_args!("<locked>")),
        };

        d.finish()
    }
}

impl<P: Ord, T, const FIFO: bool, const LOWEST_FIRST: bool>
    PriorityMutex<P, T, FIFO, LOWEST_FIRST>
{
    fn dequeue(&self, item: *const PriorityMutexWaiter<P>) {
        let mut head = self.head.lock();

        if let Some(mut node) = head.as_ref() {
            if &raw const **node == item {
                return {
                    // by using a return block here we get rust to be bit smarter and realize we
                    // don't actually have a ref to node by the time we modify head...
                    *head = node.next().take();

                    if let Some(new_head) = &*head {
                        new_head.start();
                    }
                };
            }

            while let Some(next) = node.next() {
                if &raw const **next == item {
                    *node.next() = next.next().take();
                    return;
                }

                node = &*next;
            }
        }
    }

    #[inline(always)]
    /// returns true if lhs is higher priority than rhs
    fn is_higher_priority(lhs: &P, rhs: &P) -> bool {
        match lhs.cmp(rhs) {
            core::cmp::Ordering::Less => LOWEST_FIRST,
            core::cmp::Ordering::Equal => !FIFO,
            core::cmp::Ordering::Greater => !LOWEST_FIRST,
        }
    }

    /// Create a new mutex
    pub const fn new(data: T) -> Self {
        Self {
            head: Mutex::new(None),
            data: UnsafeCell::new(data),
        }
    }

    /// Try to acquire the lock without blocking or requesting eviction of the current holder.
    /// Priority will be stored in guard; higher priority requesters will try to evict the returned
    /// guard if the `evict` flag is enabled.
    pub fn try_lock(
        &self,
        priority: P,
    ) -> Result<PriorityMutexGuard<'_, P, T, FIFO, LOWEST_FIRST>, TryLockError> {
        let mut queue = self.head.lock();

        if queue.is_some() {
            return Err(TryLockError);
        }

        let (node, rf) = PriorityMutexWaiter::new(priority, true);
        *queue = Some(node);

        Ok(PriorityMutexGuard {
            mutex: self,
            node: rf,
        })
    }

    /// Acquire exclusive access to the locked resource, waiting until after higher priority
    /// requesters acquire and release the lock.
    ///
    /// If the `evict` feature is enabled, this will also notify the current holder to request it
    /// to release the lock if the current holder is lower priority.
    ///
    /// Cancel safety: this function is cancel safe.
    pub async fn lock(&self, priority: P) -> PriorityMutexGuard<'_, P, T, FIFO, LOWEST_FIRST> {
        // rust makes us write this backwards... for some reason the compiler refuses to believe
        // that head is not held over awaits even when we explicitly drop it...
        let guard = {
            let mut head = self.head.lock();

            let mut node = match head.as_ref() {
                Some(x) => x,
                None => {
                    // there's no head node (ie no holder) so we return without doing any waiting
                    let (new_node, new_ref) = PriorityMutexWaiter::new(priority, true);

                    *head = Some(new_node);
                    return PriorityMutexGuard {
                        mutex: self,
                        node: new_ref,
                    };
                }
            };

            #[cfg(feature = "evict")]
            if Self::is_higher_priority(&priority, &node.priority) {
                // if requesting priority is higher than head, request stop
                node.evict();
            }

            let (new_node, new_ref) = PriorityMutexWaiter::new(priority, false);

            // we still need to iterate through children - as the head isn't always the highest
            // priority
            while let Some(next) = node.next() {
                // order of exec if holder is the same is fifo
                if Self::is_higher_priority(&new_ref.priority, &next.priority) {
                    *new_node.next() = node.next().take();
                    break;
                }

                node = &*next;
            }

            *node.next() = Some(new_node);

            // rust gets mad if we inline await here even if we explicitly drop head...
            // so instead we have this contrived return sub-block to make it even more clear that
            // head isn't in scope when we await
            //
            // note: it is important that the guard is created before the subsequent await, as this
            // fn could be cancelled during the await
            PriorityMutexGuard {
                mutex: self,
                node: new_ref,
            }
        };

        WaiterFlagFut::<P, WAITER_FLAG_HAS_LOCK>::new(&guard.node).await;
        return guard;
    }
}

impl<P: Ord, T, const FIFO: bool, const LOWEST_FIRST: bool> From<T>
    for PriorityMutex<P, T, FIFO, LOWEST_FIRST>
{
    #[inline]
    fn from(value: T) -> Self {
        Self::new(value)
    }
}
