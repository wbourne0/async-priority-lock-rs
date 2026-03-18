//! Contains the [Semaphore] struct for async, priority-ordered acquisition of permits.
#[cfg(feature = "semaphore-total")]
use crate::SyncWriteGuard;
#[cfg(doc)]
use crate::{FIFO, LIFO, Mutex, queue::*};
use crate::{
    Priority, RwLock,
    queue::{PriorityQueue, PriorityQueueHandle},
    waiter::{self, Waiter, WaiterFlagFut},
};
use core::{
    cmp::Ordering,
    error::Error,
    fmt::{Debug, Display},
    marker::PhantomData,
    mem::ManuallyDrop,
    usize,
};

#[cfg(feature = "const-default")]
use const_default::ConstDefault;
pub trait SemaphoreQueue<P: Priority>: PriorityQueue<SemaphoreWaiter<P>> {}
impl<T: PriorityQueue<SemaphoreWaiter<P>>, P: Priority> SemaphoreQueue<P> for T {}

#[derive(Debug)]
/// Opaque waiter type used for [PriorityQueue] entries.
///
/// This implements [Priority] and is the entry type used by [Semaphore].
pub struct SemaphoreWaiter<P: Priority> {
    priority: P,
    waiter: Waiter,
    count: usize,
}

impl<P: Priority> SemaphoreWaiter<P> {
    #[inline(always)]
    fn count(&self) -> usize {
        if cfg!(feature = "semaphore-total") {
            self.count & !WITHIN_TOTAL_BIT
        } else {
            self.count
        }
    }

    #[cfg(feature = "semaphore-total")]
    #[inline(always)]
    fn count_and_flag(&self) -> (usize, bool) {
        (
            self.count & !WITHIN_TOTAL_BIT,
            self.count & WITHIN_TOTAL_BIT != 0,
        )
    }
}

/// Has the same priority as P except "held" entries always have higher priority than pending requesters
///
/// Additionally constraints order with the same priority to enqueue the lowest request count first
/// to minimize waiting (this applies after the inner priority has compared, so [FIFO] and [LIFO]
/// will prevent this).
impl<P: Priority> Priority for SemaphoreWaiter<P> {
    #[inline]
    fn compare(&self, other: &Self) -> core::cmp::Ordering {
        match (self.waiter.has_lock(), other.waiter.has_lock()) {
            (true, false) => Ordering::Greater,
            (false, true) => Ordering::Less,
            // notably, we don't check count here - this is a hidden unsafe bit we have here
            // based on us not changing count unless the waiter already has the lock.
            //
            // (at which point, we don't care )
            _ => self.priority.compare(&other.priority),
        }
    }

    #[inline]
    fn compare_new(&self, old: &Self) -> Ordering {
        match (self.waiter.has_lock(), old.waiter.has_lock()) {
            (true, false) => Ordering::Greater,
            (false, true) => Ordering::Less,
            (is_held, _) => {
                let ret = self.priority.compare_new(&old.priority);

                if !is_held {
                    // if the priority is the same, place nodes with lower count first
                    // we can skip this once the permits are both held - we only care which has
                    // lower count in the pending part of the queue so we can return those without
                    // waiting to have enough permits for waiters of the same priority which need
                    // more.

                    return ret.then_with(|| self.count().compare(&old.count()).reverse());
                }
                ret
            }
        }
    }
}

#[cfg(feature = "arena-queue")]
type DefaultSemaphoreQueue_<P> = crate::queue::DualLinkArenaQueue<SemaphoreWaiter<P>>;
#[cfg(all(feature = "box-queue", not(feature = "arena-queue")))]
type DefaultSemaphoreQueue_<P> = crate::queue::DualLinkBoxQueue<SemaphoreWaiter<P>>;

/// The default queue used for [Semaphore].
///
/// The actual queue used here may change, however it will always be
/// [`Sync`]` + `[`Send`] if `P` is [`Sync`]` + `[`Send`].
///
/// Currently, the default queue used is as follows:
/// - if `arena-queue` is enabled (default): [DualLinkArenaQueue] - there's more cases than what we
/// have with [Mutex] where nodes won't be the head node when removed, thus the reduced performance
/// in queueing is likely to be made up for by the reduced dequeue time.
/// - if `box-queue` is enabled but not `arena-queue`: [DualLinkBoxQueue] - same rationale as for
/// [DualLinkArenaQueue] except dequeuing is even more expensive for [BoxQueue] so this is almost
/// always prefferred over [SingleLinkBoxQueue]
/// - if neither feature is enabled, no default is provided
#[cfg(any(feature = "arena-queue", feature = "box-queue"))]
pub type DefaultSemaphoreQueue<P> = DefaultSemaphoreQueue_<P>;

#[derive(Default)]
/// The inner queue for this type is a bit complicated:
/// At the start, we have all of the active holders ordered by their priorities.
/// After, we have all of the waiters ordered by their priorities.
///
/// This means that when a waiter is granted permits, it needs to be repositioned to the start of
/// the queue.
struct SemaphoreInner<P: Priority, Q: SemaphoreQueue<P>> {
    // PERF: We actually don't need to retain the waiters which hold permits if neither the
    // `semaphore-total` nor `evict` features are enabled.
    queue: Q,
    #[cfg(feature = "semaphore-total")]
    total: usize,
    available: usize,
    _phantom: PhantomData<P>,
}

impl<P: Priority, Q: SemaphoreQueue<P>> SemaphoreInner<P, Q> {
    #[inline]
    fn activate_waiters(&mut self, mut next: Option<Q::SharedHandle>) {
        while let Some(handle) = next.take() {
            let node = self.queue.get_by_handle(handle.as_ref());
            let flags = node.waiter.flags();
            let is_held = flags & waiter::WAITER_FLAG_HAS_LOCK != 0;

            let count = node.count();
            next = self.queue.get_next_handle(handle.as_ref());

            // we still issue permits here even if the waiter is flagged for eviction (in the case
            // where we started eviction and then raised capacity to sufficient levels before
            // adding more permits)
            if is_held {
                continue;
            }

            if count > self.available {
                // we will only ever evict nodes that aren't held when we evicted due to capacity.
                // in that case, we should enqueue subsequent nodes.
                let will_evict = cfg!(feature = "semaphore-total")
                    && flags & waiter::WAITER_FLAG_WANTS_EVICT != 0;

                // fine to skip this node since it will be evicted anyways
                if will_evict {
                    continue;
                }

                break;
            }

            self.available -= count;
            self.queue.update_node(handle.as_ref(), |x| {
                x.waiter.start();
                true
            });
        }
    }

    #[cfg(feature = "semaphore-total")]
    #[inline]
    fn notify_oversized_waiters(&self, start: Option<&Q::Handle>) {
        for node in self.queue.iter_at(start) {
            // We should NEVER evict a holder here which already has permits, but it's impossible
            // for a holder with permits to have > self.total permits - so we can filter those out
            // implicitly here without having to do an atomic load for .has_lock()
            let (count, should_evict) = node.count_and_flag();

            if should_evict && count > self.total {
                node.waiter.evict();
            }
        }
    }
}

#[cfg(feature = "const-default")]
impl<P: Priority, Q: ConstDefault + SemaphoreQueue<P>> ConstDefault for SemaphoreInner<P, Q> {
    const DEFAULT: Self = Self {
        queue: Q::DEFAULT,
        #[cfg(feature = "semaphore-total")]
        total: 0,
        available: 0,
        _phantom: PhantomData,
    };
}

/// A guard which will conditionally activate subsequent nodes if either it or the previous node
/// has the lock.
///
/// This is the inner type for SemaphorePermit, which has an optimized drop function as it
/// knows that it currently holds the permits when it is dropped.
struct SemaphorePermitWaiter<'a, P: Priority, Q: SemaphoreQueue<P>> {
    semaphore: &'a Semaphore<P, Q>,
    handle: ManuallyDrop<Q::Handle>,
}

unsafe impl<'a, P: Priority, Q: SemaphoreQueue<P>> Sync for SemaphorePermitWaiter<'a, P, Q>
where
    Semaphore<P, Q>: Sync,
    Q::Handle: Sync,
{
}

unsafe impl<'a, P: Priority, Q: SemaphoreQueue<P>> Send for SemaphorePermitWaiter<'a, P, Q>
where
    Semaphore<P, Q>: Sync,
    Q::Handle: Send,
{
}

impl<'a, P: Priority, Q: SemaphoreQueue<P>> SemaphorePermitWaiter<'a, P, Q> {
    const HAS_PURE_LOAD: bool = Q::Handle::LOAD_PURE.is_some();
}

impl<'a, P: Priority, Q: SemaphoreQueue<P>> waiter::WaiterHandle
    for SemaphorePermitWaiter<'a, P, Q>
{
    #[inline]
    fn with_waker<T>(&self, f: impl FnOnce(&Waiter) -> T) -> T {
        if Self::HAS_PURE_LOAD {
            unsafe { f(&Q::Handle::LOAD_PURE.unwrap_unchecked()(&self.handle).waiter) }
        } else {
            let sem = self.semaphore.0.read();
            f(&sem.queue.get_by_handle(&self.handle).waiter)
        }
    }
}

impl<'a, P: Priority, Q: SemaphoreQueue<P>> Drop for SemaphorePermitWaiter<'a, P, Q> {
    #[inline]
    fn drop(&mut self) {
        let mut sem = self.semaphore.0.write();

        let handle = unsafe { ManuallyDrop::take(&mut self.handle) };
        let node = sem.queue.get_by_handle(&handle);
        let is_active = node.waiter.has_lock();
        if is_active {
            sem.available += node.count();
        }

        // PERF: `is_active` is added here because node.count() cannot be zero, thus if
        // node was active, the semaphore must now have permits.  This is used as an
        // alternative to core::hint::assert_unchecked(node.count() > 0);
        let has_available = is_active || sem.available > 0;

        let (prev, next) = sem.queue.dequeue(handle);

        if next.is_none() || !has_available {
            return;
        }

        // we were active, thus we have permits to (potentially) grant
        if is_active {
            return sem.activate_waiters(next);
        }

        // Previous was locked (or was head) but this node wasn't. Since a previous node was, that
        // means the later nodes may have been blocked due to waiting on this one.
        if prev.is_none_or(|x| x.waiter.has_lock()) {
            sem.activate_waiters(next);
        }
    }
}

#[repr(transparent)]
/// A permit (or collection of permits) from a [Semaphore]
///
/// Acquired via [Semaphore::acquire] and associated fns.
pub struct SemaphorePermit<'a, P: Priority, Q: SemaphoreQueue<P>>(
    /// Publicly exposed semaphore guards are definitely loaded, so we can actually skip some of the
    /// checks we need to do for [SemaphorePermitWaiter]
    ManuallyDrop<SemaphorePermitWaiter<'a, P, Q>>,
);

unsafe impl<'a, P: Priority, Q: SemaphoreQueue<P>> Sync for SemaphorePermit<'a, P, Q>
where
    Semaphore<P, Q>: Sync,
    Q::Handle: Sync,
{
}

unsafe impl<'a, P: Priority, Q: SemaphoreQueue<P>> Send for SemaphorePermit<'a, P, Q>
where
    Semaphore<P, Q>: Sync,
    Q::Handle: Send,
{
}

impl<'a, P: Priority, Q: SemaphoreQueue<P>> SemaphorePermit<'a, P, Q> {
    #[cfg(feature = "evict")]
    #[inline]
    /// Returns a future which resolves when / if a higher priority requester is waiting for permit
    /// acquisition.  Available only if the `evict` flag is enabled.
    ///
    /// Cancel Safety: This function is cancel safe.
    pub fn evicted(&mut self) -> impl Future<Output = ()> {
        waiter::VoidFut(WaiterFlagFut::<_, { waiter::WAITER_FLAG_WANTS_EVICT }>::new(&*self.0))
    }

    /// Remove these permits from the pool of permits in the semaphore.
    #[inline]
    pub fn forget(mut self) {
        let mut sem = self.0.semaphore.0.write();
        #[cfg_attr(not(feature = "semaphore-total"), allow(unused))]
        let count = sem.queue.get_by_handle(&self.0.handle).count();

        #[cfg_attr(not(feature = "semaphore-total"), allow(unused))]
        let (_, maybe_next) = sem
            .queue
            .dequeue(unsafe { ManuallyDrop::take(&mut self.0.handle) });

        core::mem::forget(self);

        #[cfg(feature = "semaphore-total")]
        {
            sem.total -= count;

            if let Some(next) = maybe_next {
                sem.downgrade()
                    .notify_oversized_waiters(Some(next.as_ref()));
            }
        }
    }

    #[inline]
    /// The count of permits held
    pub fn permits(&self) -> usize {
        if SemaphorePermitWaiter::<'a, P, Q>::HAS_PURE_LOAD {
            return unsafe { Q::Handle::LOAD_PURE.unwrap_unchecked()(&self.0.handle).count() };
        }
        let sem = self.0.semaphore.0.read();

        sem.queue.get_by_handle(&self.0.handle).count()
    }

    #[inline]
    /// Checks if `self` belongs to `semaphore`.
    pub fn belongs_to(&self, semaphore: &Semaphore<P, Q>) -> bool {
        core::ptr::eq(self.0.semaphore, semaphore)
    }

    /// Split into multiple permit guards / holder.  The new guard will have the same priority for
    /// evictions.
    ///
    /// `P` must implement [Clone] for this to succeed. If `P` is not [Clone],
    /// [split_with_priority](Self::split_with_priority) may be used instead.
    ///
    #[inline]
    pub fn split(&mut self, count: usize) -> Result<Self, InsufficientPermitsError>
    where
        P: Clone,
    {
        assert!(
            count > 0,
            "count must be greater than zero, received {count}"
        );
        let mut sem = self.0.semaphore.0.write();

        let mut priority: Option<P> = None;
        let mut avail = 0;

        sem.queue.update_node(&self.0.handle, |node| {
            avail = node.count();
            // PERF: Try making this likely(avail > count) once likely_unlikely is stable
            if avail > count {
                node.count -= count;
                priority = Some(node.priority.clone());
            }

            false
        });

        if priority.is_none() {
            return Err(InsufficientPermitsError {
                total: avail,
                requested: count,
            });
        }

        let handle = sem.queue.enqueue(SemaphoreWaiter {
            priority: priority.unwrap(),
            waiter: Waiter::new(true),
            count,
        });

        Ok(SemaphorePermitWaiter {
            semaphore: self.0.semaphore,
            handle: ManuallyDrop::new(handle),
        }
        .into())
    }

    pub fn split_with_priority(
        &mut self,
        count: usize,
        priority: P,
    ) -> Result<Self, InsufficientPermitsError> {
        assert!(
            count > 0,
            "count must be greater than zero, received {count}"
        );
        let mut sem = self.0.semaphore.0.write();

        let mut avail = 0;
        let mut has_capacity = false;

        sem.queue.update_node(&self.0.handle, |node| {
            avail = node.count();
            // PERF: Try making this likely(avail > count) once likely_unlikely is stable
            if avail > count {
                node.count -= count;
                has_capacity = true
            }

            false
        });

        if !has_capacity {
            return Err(InsufficientPermitsError {
                total: avail,
                requested: count,
            });
        }

        let handle = sem.queue.enqueue(SemaphoreWaiter {
            priority: priority.into(),
            waiter: Waiter::new(true),
            count,
        });

        Ok(SemaphorePermitWaiter {
            semaphore: self.0.semaphore,
            handle: ManuallyDrop::new(handle),
        }
        .into())
    }

    #[inline]
    /// Merge another permit into this one. Returns an error if the other node belongs to another
    /// semaphore
    ///
    /// Panics if the sum of their permits would exceed the max permit count
    pub fn merge(&mut self, mut other: Self) -> Result<(), ()> {
        if &raw const *self.0.semaphore != other.0.semaphore {
            return Err(());
        }

        let mut sem = self.0.semaphore.0.write();

        let other_count = sem.queue.get_by_handle(&other.0.handle).count();

        let mut would_overflow = false;
        sem.queue.update_node(&self.0.handle, |node| {
            would_overflow = node.count() + other_count > MAX_PERMITS;
            if !would_overflow {
                node.count += other_count
            }

            false
        });

        if would_overflow {
            return Err(());
        }

        let other_handle = unsafe { ManuallyDrop::take(&mut other.0.handle) };
        core::mem::forget(other);

        sem.queue.dequeue(other_handle);

        Ok(())
    }
}

impl<'a, P: Priority, Q: SemaphoreQueue<P>> Drop for SemaphorePermit<'a, P, Q> {
    #[inline]
    /// Releases the permits back to the semaphore.
    fn drop(&mut self) {
        let mut sem = self.0.semaphore.0.write();

        let handle = unsafe { ManuallyDrop::take(&mut self.0.handle) };
        let count = sem.queue.get_by_handle(&handle).count();
        let (_, next) = sem.queue.dequeue(handle);
        if cfg!(feature = "semaphore-total") {
            sem.available += count;
        } else {
            // in terms of panic safety: it's fine to panic here and not activate waiters as we'd
            // fail to add new permits anyways.
            sem.available = match sem.available.checked_add(count) {
                Some(x) => x,
                None => {
                    let avail = sem.available;
                    // drop guard to avoid poisoning it (for std locks)
                    drop(sem);
                    core::panic!(
                        "failed to release {} permits back to semaphore as that would overflow (current available: {})",
                        count,
                        avail
                    );
                }
            }
        }

        sem.activate_waiters(next);
    }
}

impl<'a, P: Priority, Q: SemaphoreQueue<P>> From<SemaphorePermitWaiter<'a, P, Q>>
    for SemaphorePermit<'a, P, Q>
{
    #[inline(always)]
    fn from(value: SemaphorePermitWaiter<'a, P, Q>) -> Self {
        Self(ManuallyDrop::new(value))
    }
}

impl<'a, P: Priority, Q: SemaphoreQueue<P>> Debug for SemaphorePermit<'a, P, Q> {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("SemaphorePermit")
            .field("permits", &self.permits())
            .finish()
    }
}

/// An async semaphore where queued waiters are granted permits by order of priority.
///
/// With the `evict` flag enabled, existing handles will be requested to return their permits back
/// to the queue (which may be echecked / waited for via [evicted](SemaphorePermit::evicted))
///
/// Note that to minimize waiting, requesters with the same priority will additionally be sorted by
/// permit count (lowest first).  If this isn't desireable, use a [Priority] wraper like
/// [FIFO] or [LIFO].  This makes it so requesters won't be blocked by larger requests with
/// the same priority.
///
/// The default queue used may change, however its characteristics will remain the same, notably:
/// - If P is [`Send`]` + `[`Sync`] the queue will always be [`Send`]` + `[`Sync`]
pub struct Semaphore<
    P: Priority,
    #[cfg(any(feature = "arena-queue", feature = "box-queue"))] Q: SemaphoreQueue<P> = DefaultSemaphoreQueue<P>,
    #[cfg(not(any(feature = "arena-queue", feature = "box-queue")))] Q: SemaphoreQueue<P>,
>(RwLock<SemaphoreInner<P, Q>>);

unsafe impl<P: Priority, Q: SemaphoreQueue<P> + Send + Sync> Sync for Semaphore<P, Q> {}
unsafe impl<P: Priority, Q: SemaphoreQueue<P> + Send> Send for Semaphore<P, Q> {}

/// The maximum amount of permits a single holder can have.  This is `usize::MAX >> 1`.
pub const MAX_PERMITS: usize = usize::MAX >> 1;
/// A bit that is set in the count for permits if the `semaphore-total` flag is enabled and the
/// waiter should be evicted when if the total permit count for the semaphore is reduced to below
/// the requested permit count.
// this is the same as !MAX_PERMITS, just a bit more verbose.
const WITHIN_TOTAL_BIT: usize = 1 << (usize::BITS - 1);

/// Error returned when we lack sufficient permits to perform an operation.
///
/// - With the `semaphore-total` flag: [Semaphore] lacks sufficient permits when using
/// [acquire_within_total](Semaphore::acquire_within_total)
///
/// - When more permits were requested via [SemaphorePermit::split].than the permit contains.
///
/// Contains the requested count of permits via [requested](Self::requested) and optionally the total
/// available when the request failed via [total](Self::total).
///
/// Note that [total](Self::total) may be [None] if the requester was evicted after being queued due to
/// insufficient capacity (at which point, we don't know what the total permits for the queue was)
///
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct InsufficientPermitsError {
    // PERF:
    // Ideally we could throw this in an option with a sentinel value and have it retain the size
    // of a usize but this doesn't seem to be possible yet unless the sentinel is zero.
    // (and in our case, a total of zero is very much so valid)
    // Note that while in both contexts this is used, usize::MAX is actually a valid value -
    // but if we had a total of usize::MAX then requested could not be > total.
    total: usize,
    requested: usize,
}

impl InsufficientPermitsError {
    /// The total amount of permits which were available when the waiter was rejected
    /// (or None if we don't know)
    #[inline(always)]
    pub fn total(&self) -> Option<usize> {
        (self.total != usize::MAX).then_some(self.total)
    }

    /// The amount of permits requested
    #[inline(always)]
    pub fn requested(&self) -> usize {
        self.requested
    }
}

impl Display for InsufficientPermitsError {
    #[inline]
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        if self.total == usize::MAX {
            write!(
                f,
                "semaphoer lacks sufficient permits: want {}",
                self.requested
            )
        } else {
            write!(
                f,
                "insufficient total permits: have {} want {}",
                self.total, self.requested
            )
        }
    }
}

impl Error for InsufficientPermitsError {}

impl<P: Priority, Q: SemaphoreQueue<P>> Debug for Semaphore<P, Q> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let sem = self.0.read();
        let mut dbg = f.debug_struct("Semaphore");
        dbg.field("available", &sem.available);

        #[cfg(feature = "semaphore-total")]
        dbg.field("total", &sem.total);
        dbg.finish()
    }
}

impl<P: Priority, Q: SemaphoreQueue<P>> Semaphore<P, Q> {
    #[inline]
    /// Create a new semaphore with `permits` permits available.
    pub fn new(capacity: usize) -> Self {
        Self(RwLock::new(SemaphoreInner {
            queue: Q::default(),
            #[cfg(feature = "semaphore-total")]
            total: capacity,
            available: capacity,
            _phantom: PhantomData,
        }))
    }

    #[cfg(feature = "const-default")]
    /// Create a new semaphore with `permits` permits available.  Usable for const if the
    /// underlying queue is [ConstDefault] and the `const-default` feature is enabled.
    ///
    /// All builtin queues impl [ConstDefault].
    pub const fn const_new(permits: usize) -> Self
    where
        Q: ConstDefault,
    {
        Self(RwLock::new(SemaphoreInner {
            queue: Q::DEFAULT,
            #[cfg(feature = "semaphore-total")]
            total: permits,
            available: permits,
            _phantom: PhantomData,
        }))
    }

    #[inline]
    fn do_acquire(
        &self,
        inner: &mut SemaphoreInner<P, Q>,
        priority: P,
        count: usize,
    ) -> (SemaphorePermitWaiter<'_, P, Q>, bool) {
        let has_available = inner.available >= (count & !WITHIN_TOTAL_BIT);
        let will_acquire = has_available
            && inner
                .queue
                .iter()
                // skip to first waiter which doesn't hold permits
                .skip_while(|x| {
                    let flags = x.waiter.flags();
                    if flags & waiter::WAITER_FLAG_HAS_LOCK != 0 {
                        #[cfg(feature = "evict")]
                        if priority.compare(&x.priority).is_gt() {
                            x.waiter.evict();
                        }
                        return true;
                    }

                    // also skip any evicted flags (as we don't need to wait for those to be
                    // removed from the queue)
                    cfg!(feature = "semaphore-total")
                        && flags & waiter::WAITER_FLAG_WANTS_EVICT == 0
                })
                .next()
                // ge is fine here - if priority is equal, then order doesn't matter; thus we
                // should place first since we can immediately take the permits this way
                .is_none_or(|first_pending| priority.compare(&first_pending.priority).is_ge());

        let handle = inner.queue.enqueue(SemaphoreWaiter {
            priority,
            waiter: Waiter::new(will_acquire),
            count,
        });

        let guard = SemaphorePermitWaiter {
            semaphore: self,
            handle: ManuallyDrop::new(handle),
        };

        if will_acquire {
            inner.available -= count & !WITHIN_TOTAL_BIT;
        }

        #[cfg(feature = "evict")]
        // if we had available, then we would have already marked the pieces that need to be
        // evicted as such
        if !has_available {
            let node = inner.queue.get_by_handle(&guard.handle);
            for ex in inner.queue.iter() {
                if ex.waiter.has_lock() {
                    // break;

                    if node.priority.compare(&ex.priority).is_gt() {
                        ex.waiter.evict();
                    }
                }
            }
        }

        return (guard, will_acquire);
    }

    /// Acquire a single permit, waiting if necessary.  Permits will be issued by order of
    /// priority.
    #[inline]
    pub fn acquire(&self, priority: P) -> impl Future<Output = SemaphorePermit<'_, P, Q>> {
        self.acquire_many(priority, 1)
    }

    /// Acquire a single permit, waiting if necessary.
    ///
    /// Shorthand for [`acquire`][Self::acquire]`(priority.into())`.
    ///
    /// Cancel Safety: This function is cancel safe.
    #[inline(always)]
    pub fn acquire_from(
        &self,
        priority: impl Into<P>,
    ) -> impl Future<Output = SemaphorePermit<'_, P, Q>> {
        self.acquire(priority.into())
    }

    #[inline]
    /// Acquire a permit with the [Default] priority.
    ///
    /// Shorthand for [acquire](Self::acquire) with [`Default::default()`].
    ///
    /// Cancel Safety: This function is cancel safe.
    pub fn acquire_default(&self) -> impl Future<Output = SemaphorePermit<'_, P, Q>>
    where
        P: Default,
    {
        self.acquire_many(Default::default(), 1)
    }

    /// Acquire multiple permits, waiting if necessary.  Permits will be issued by order of
    /// priority.  Lower priority waiters will be blocked until all requested permits are acquired
    /// (and subsequently released).
    ///
    /// Panics if `count >= MAX_PERMITS` (`usize::MAX >> 1`).
    ///
    /// Cancel Safety: This function is cancel safe.
    pub async fn acquire_many(&self, priority: P, count: usize) -> SemaphorePermit<'_, P, Q> {
        assert!(
            count < MAX_PERMITS,
            "count for a single holder must be less than {} and not zero (received {})",
            MAX_PERMITS,
            count
        );
        // need to be a bit explicit here since rust won't realize that we dropped guard otherwise
        let guard = {
            let mut inner = self.0.write();
            let (guard, did_acquire) = self.do_acquire(&mut inner, priority.into(), count);

            if did_acquire {
                return guard.into();
            }
            guard
        };

        WaiterFlagFut::<_, { waiter::WAITER_FLAG_HAS_LOCK }>::new(&guard).await;

        guard.into()
    }

    /// Acquire multiple permits with the [Default] priority.
    ///
    /// Panics if `count >= MAX_PERMITS` (`usize::MAX >> 1`).
    ///
    /// Shorthand for [acquire_many](Self::acquire_many) with [`Default::default()`].
    ///
    /// Cancel Safety: This function is cancel safe.
    #[inline(always)]
    pub async fn acquire_many_default(
        &self,
        count: usize,
    ) -> impl Future<Output = SemaphorePermit<'_, P, Q>>
    where
        P: Default,
    {
        self.acquire_many(Default::default(), count)
    }

    /// Acquire multiple permits with the .
    ///
    /// Panics if `count >= MAX_PERMITS` (`usize::MAX >> 1`).
    ///
    /// Shorthand for [acquire_many](Self::acquire_many) with `priority.into()`.
    ///
    /// Cancel Safety: This function is cancel safe.
    #[inline(always)]
    pub async fn acquire_many_from(
        &self,
        count: usize,
        priority: impl Into<P>,
    ) -> impl Future<Output = SemaphorePermit<'_, P, Q>>
    where
        P: Default,
    {
        self.acquire_many(priority.into(), count)
    }

    #[cfg(feature = "semaphore-total")]
    /// Acquire `count` permits without blocking the queue if the requested count of permits is
    /// more than the total associated.  If the semaphore lacks sufficient associated permits or loses
    /// them while waiting, this returns an InsufficientPermitsError.
    ///
    /// Example:
    /// ```rust
    /// let sem = Semaphore::<usize>::new(10);
    ///
    /// let permit = sem.acquire(0).await;
    /// // try to acquire 10 permits
    /// let mut many_permits_fut = pin!(sem.acquire_within_total(1, 10));
    /// tokio::select! {
    ///     _ = tokio::time::sleep(Duration::from_secs(1)) => {},
    ///     // can't happen
    ///     _ =  many_permits_fut.as_mut() => { panic!("total of 11 tokens held")},
    /// };
    ///
    /// // remove 1 permit from the semaphore
    /// permit.forget();
    ///
    /// assert!(many_permits_fut.await.is_err());
    /// ```
    ///
    /// Cancel Safety: This function is cancel safe.
    pub async fn acquire_within_total(
        &self,
        priority: P,
        count: usize,
    ) -> Result<SemaphorePermit<'_, P, Q>, InsufficientPermitsError> {
        assert!(
            count < MAX_PERMITS,
            "count for a single holder must be less than {} and not zero (received {})",
            MAX_PERMITS,
            count
        );
        let guard = {
            let mut inner = self.0.write();
            if inner.total < count {
                return Err(InsufficientPermitsError {
                    total: inner.total,
                    requested: count,
                });
            }

            let (guard, did_acquire) =
                self.do_acquire(&mut inner, priority.into(), count | WITHIN_TOTAL_BIT);

            if did_acquire {
                return Ok(guard.into());
            }

            guard
        };

        let flags = WaiterFlagFut::<
            _,
            { waiter::WAITER_FLAG_HAS_LOCK | waiter::WAITER_FLAG_WANTS_EVICT },
        >::new(&guard)
        .await;

        if flags & waiter::WAITER_FLAG_HAS_LOCK == 0 {
            // we were evicted before we got the lock (due to running out of capacity)
            // So we need to return an error
            return Err(InsufficientPermitsError {
                total: usize::MAX,
                requested: count,
            });
        }

        // Since we hold no locks, it's possible that we've been evicted at this point - but since
        // we already have the permits, that would mean it was evicted afterwards (so it's up to
        // receiver to decide whether to check for eviction)
        Ok(guard.into())
    }

    #[inline]
    /// Add permits to the semaphore.
    ///
    /// Panics if:
    /// - The count of permits to add would overflow available
    /// - (if the `semaphore-total` feature is enabled) The count of permits plus the current total
    ///   would overflow.
    ///
    /// Use [try_add_permits][Self::try_add_permits] if overflows may occur.
    ///
    /// Note that if the `semaphore-total` feature isn't enabled, the sum of permits issued + permits
    /// available can exceed the max usize and overflow.  If an owned permit is dropped and would cause an
    /// overflow when adding held permits back to available, it will panic.  The semaphore itself
    /// will remain usable, however the permits will be discarded.
    ///
    /// For compatibility, it's best to assume that it's unsafe to exceed `usize::MAX` permits
    /// associated with a semaphore, even if the `semaphore-total` flag isn't enabled (as other
    /// crates may require it) (associated permi.ts being available + sum of permits issued)
    pub fn add_permits(&self, count: usize) -> usize {
        self.try_add_permits(count).expect("must add permits")
    }

    #[cfg(feature = "semaphore-total")]
    #[inline]
    /// Returns the capacity / total count of permits associated with the semaphore..
    pub fn total_permits(&self) -> usize {
        self.0.read().total
    }

    /// Returns the amount of currently available permits.
    #[inline]
    pub fn available_permits(&self) -> usize {
        self.0.read().available
    }

    #[inline]
    /// Try adding permits, returning an error if adding the permits would overflow. See the notes
    /// on [add_permits](Self::add_permits) for more details.
    pub fn try_add_permits(&self, count: usize) -> Result<usize, ()> {
        let mut inner = self.0.write();

        #[cfg(feature = "semaphore-total")]
        {
            inner.total = inner.total.checked_add(count).ok_or(())?;
            // available must be <= total
            inner.available += count
        }
        #[cfg(not(feature = "semaphore-total"))]
        {
            inner.available = inner.available.checked_add(count).ok_or(())?;
        }

        let head = inner.queue.head_handle();
        inner.activate_waiters(head);

        Ok(inner.available)
    }

    /// Forget up to count permits, up to the currently available amount.  Returns the amount of
    /// permits forgotten.
    ///
    /// If `n` permits *need* to be removed, calling `acquire_many` with the highest priority then
    /// calling forget on the returned guard may be a better choice.
    ///
    /// (if the `semaphore-total` flag is enabled, `acquire_within_total` may be a safer choice
    /// than `acquire_many` - if it's possible for any permits to be forgotten elsewhere)
    #[inline]
    pub fn forget_permits(&self, mut count: usize) -> usize {
        let mut inner = self.0.write();

        count = count.min(inner.available);

        inner.available -= count;

        #[cfg(feature = "semaphore-total")]
        if count != 0 {
            unsafe { core::hint::assert_unchecked(inner.total >= count) };
            inner.total -= count;

            inner.downgrade().notify_oversized_waiters(None);
        }

        count
    }
}

#[cfg(feature = "const-default")]
impl<P: Priority, Q: ConstDefault + SemaphoreQueue<P>> ConstDefault for Semaphore<P, Q> {
    const DEFAULT: Self = Self(RwLock::new(ConstDefault::DEFAULT));
}

/// Creates a new [Semaphore] with zero permits.
impl<P: Priority, Q: SemaphoreQueue<P>> Default for Semaphore<P, Q> {
    #[inline]
    fn default() -> Self {
        Self::new(0)
    }
}
