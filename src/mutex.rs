//! Contains the [Mutex] type for async, priority-ordered exclusive access to a resource.
#[cfg(doc)]
use crate::{FIFO, LIFO, queue::*};
use crate::{
    Priority, RwLock,
    queue::{PriorityQueue, PriorityQueueHandle},
    waiter::{self, Waiter, WaiterFlagFut},
};
use core::{
    cell::UnsafeCell,
    fmt::{Debug, Display},
    format_args,
    marker::PhantomData,
    mem::ManuallyDrop,
    ops::{Deref, DerefMut},
};

#[cfg(feature = "const-default")]
use const_default::ConstDefault;

impl<'a, P: Priority, T, Q: MutexQueue<P>> waiter::WaiterHandle for MutexGuard<'a, P, T, Q> {
    #[inline]
    fn with_waker<R>(&self, f: impl FnOnce(&Waiter) -> R) -> R {
        // rust optimizer doesn't seem to be smart enough yet to do branch elimination with
        // associated const option (e.g. if let Some(x) = Q::Handle::LOAD_PURE)
        if Self::HAS_PURE_LOAD {
            unsafe { f(&Q::Handle::LOAD_PURE.unwrap_unchecked()(&self.node).waiter) }
        } else {
            let queue = self.mutex.queue.read();

            f(&queue.get_by_handle(&self.node).waiter)
        }
    }
}

#[derive(Debug)]
/// Opaque waiter type used for [PriorityQueue] entries.
///
/// This implements [Priority] and is the entry type used by [Mutex].
pub struct MutexWaiter<P: Priority> {
    priority: P,
    waiter: Waiter,
}

/// Has the same Priority impl as P with the exception of pinning holding entry to head.
impl<P: Priority> Priority for MutexWaiter<P> {
    #[inline]
    fn compare(&self, other: &Self) -> core::cmp::Ordering {
        self.priority.compare(&other.priority)
    }

    #[inline]
    fn compare_new(&self, old: &Self) -> std::cmp::Ordering {
        if old.waiter.has_lock() {
            return core::cmp::Ordering::Less;
        }

        self.priority.compare_new(&old.priority)
    }
}

impl<P: Priority> MutexWaiter<P> {
    #[inline]
    const fn new<'a>(holder: P, has_lock: bool) -> Self {
        Self {
            priority: holder,
            waiter: Waiter::new(has_lock),
        }
    }
}

#[cfg(all(feature = "arena-queue", target_pointer_width = "64"))]
type DefaultMutexQueue_<P> = crate::queue::SingleLinkArenaQueue<MutexWaiter<P>>;

#[cfg(all(not(feature = "arena-queue"), feature = "box-queue"))]
type DefaultMutexQueue_<P> = crate::queue::DualLinkBoxQueue<MutexWaiter<P>>;

/// The default queue used if unspecified for `Mutex`.  
///
/// The actual queue used varies based on flags (and in the future, may even be a new queue)
/// but will always make [Mutex] (and associated fns) [`Send`]` + `[`Sync`] as long as `P` and `T` are.
///
/// Currently, the defualt is as follows:
/// - if `arena-queue` is enabled: [SingleLinkArenaQueue] (it is expected that removed nodes will
/// usually have the lock, and thus will be the head node, and dequeue uses a seq block of memory
/// so the additional overhead added for queueing (e.g. in allocating) is usually not needed)
/// - if `box-queue` is enabled but not `arena-queue`: [DualLinkBoxQueue] - unlike [ArenaQueue],
/// searching for a prev node with [BoxQueue] is significantly slower than dual linking, so
/// we don't use [SingleLinkBoxQueue] .
/// - if neither flag is enabled: no default queue is used
#[cfg(any(feature = "arena-queue", feature = "box-queue"))]
pub type DefaultMutexQueue<P> = DefaultMutexQueue_<P>;

/// A mutex that queues waiters by priority.  Higher priority requesters will receive access first.
///
/// If the `evict` feature is enabled, the current holder of the lock will be notified if a higher
/// priority waiter is queued.
///
/// Note that requesters with the same priority may receive their access in an arbitrary priority -
/// if this is non-desireable, [FIFO] and [LIFO] may be used.
///
/// Example:
/// ```rust
/// use async_priority_lock::{FIFO, HighestFirst, Mutex, MutexGuard};
///
/// static MY_LOCK: Mutex<FIFO<HighestFirst<usize>>, Vec<usize>> =
///     Mutex::const_new(vec![]);
///
/// async fn push_num(num: usize, priority: usize) {
///     // Pushes our number to the queue.  If multiple writers are waiting on this, the waiters
///     // will acquire access by order of priority.  As the priority is FIFO, rquesters with the
///     // same priority will execute oldest first.
///     MY_LOCK.lock_from(priority).await.push(num);
/// }
///
/// async fn wait_and_push(num: usize, priority: usize) {
///     loop {
///         let mut guard = MY_LOCK.lock_from(priority).await;
///         // wait a second or abort and retry if the resource is requested with a higher
///         // priority in the meantime
///         tokio::select! {
///             _ = tokio::time::sleep(tokio::time::Duration::from_secs(1)) => {},
///             _ = MutexGuard::evicted(&mut guard) => {
///                 // drop the guard and try again
///                 continue;
///             }
///         }
///
///         guard.push(num)
///     }
/// }
/// ```
///
pub struct Mutex<
    P: Priority,
    T,
    #[cfg(any(feature = "box-queue", feature = "arena-queue"))] Q: MutexQueue<P> = DefaultMutexQueue<P>,
    #[cfg(not(any(feature = "box-queue", feature = "arena-queue")))] Q: MutexQueue<P>,
> {
    queue: RwLock<Q>,
    data: UnsafeCell<T>,
    _phantom: PhantomData<P>,
}

impl<P: Priority, T: Default, Q: MutexQueue<P>> Default for Mutex<P, T, Q> {
    fn default() -> Self {
        Self {
            queue: Default::default(),
            data: Default::default(),
            _phantom: Default::default(),
        }
    }
}

/// For the mutex to be Sync, T must be Send (Sync is not needed).
///
/// Also see [rust Mutex docs](https://doc.rust-lang.org/std/sync/struct.Mutex.html#impl-Sync-for-Mutex%3CT%3E)
///
/// (however, both `Sync` and `Send` are required for the queue; usually these are implemented if P
/// is `Send` + `Sync`)
unsafe impl<P: Priority, T: Send, Q: Send + Sync + MutexQueue<P>> Sync for Mutex<P, T, Q> {}
unsafe impl<P: Priority, T: Send, Q: Send + MutexQueue<P>> Send for Mutex<P, T, Q> {}
impl<P: Priority, T: Unpin, Q: Unpin + MutexQueue<P>> Unpin for Mutex<P, T, Q> {}

/// Alias trait for [`PriorityQueue`]`<`[`MutexWaiter<P>`]`>`.
pub trait MutexQueue<P: Priority>: PriorityQueue<MutexWaiter<P>> {}
impl<P: Priority, Q: PriorityQueue<MutexWaiter<P>>> MutexQueue<P> for Q {}

#[cfg(feature = "serde")]
impl<'de, P: Priority, T, Q: MutexQueue<P>> serde::Deserialize<'de> for Mutex<P, T, Q>
where
    T: serde::Deserialize<'de>,
{
    #[inline]
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        // todo!();
        Ok(Self::new(T::deserialize(deserializer)?))
    }
}

/// A guard holding access for a [Mutex].  When dropped, the lock is released.
///
/// If the `evict` flag is enabled, higher priority requesters will mark held locks for eviction,
/// which can be subscribed to via [Self::evicted] (associated function).
pub struct MutexGuard<'a, P: Priority, T, Q: MutexQueue<P>> {
    mutex: &'a Mutex<P, T, Q>,
    node: ManuallyDrop<Q::Handle>,
}

unsafe impl<'a, P: Priority, T: Sync, Q: MutexQueue<P>> Sync for MutexGuard<'a, P, T, Q> where
    Mutex<P, T, Q>: Sync
{
}
unsafe impl<'a, P: Priority, T: Send, Q: MutexQueue<P>> Send for MutexGuard<'a, P, T, Q>
where
    Mutex<P, T, Q>: Sync,
    Q::Handle: Send,
{
}

impl<'a, P: Priority, T, Q: MutexQueue<P>> Display for MutexGuard<'a, P, T, Q>
where
    T: Display,
{
    #[inline]
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        self.deref().fmt(f)
    }
}

impl<'a, P: Priority, T, Q: MutexQueue<P>> Debug for MutexGuard<'a, P, T, Q>
where
    T: Debug,
{
    #[inline]
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        self.deref().fmt(f)
    }
}

impl<'a, P: Priority, T, Q: MutexQueue<P>> Deref for MutexGuard<'a, P, T, Q> {
    type Target = T;

    #[inline]
    fn deref(&self) -> &Self::Target {
        unsafe { &*self.mutex.data.get() }
    }
}

impl<'a, P: Priority, T, Q: MutexQueue<P>> DerefMut for MutexGuard<'a, P, T, Q> {
    #[inline]
    fn deref_mut(&mut self) -> &mut Self::Target {
        unsafe { &mut *self.mutex.data.get() }
    }
}

impl<'a, P: Priority, T, Q: MutexQueue<P>> MutexGuard<'a, P, T, Q> {
    /// Returns a future which resolves when/if another, higher priority holder attempts to acquire
    /// the lock.
    ///
    /// Note: this is an associated method to avoid colision with methods of `T`.  Invoke via
    /// [`MutexGuard::evicted`]`(&mut self)`.
    ///
    /// Cancel Safety: This function is cancel safe.
    #[inline]
    #[cfg(feature = "evict")]
    pub fn evicted(this: &mut Self) -> impl Future<Output = ()> {
        waiter::VoidFut(WaiterFlagFut::<_, { waiter::WAITER_FLAG_WANTS_EVICT }>::new(this))
    }

    const HAS_PURE_LOAD: bool = Q::Handle::LOAD_PURE.is_some();
}

impl<'a, P: Priority, T, Q: MutexQueue<P>> Drop for MutexGuard<'a, P, T, Q> {
    #[inline]
    fn drop(&mut self) {
        let mut queue = self.mutex.queue.write();

        let was_head = queue.get_by_handle(&self.node).waiter.has_lock();
        queue.dequeue(unsafe { ManuallyDrop::take(&mut self.node) });

        // interestingly, rust seems to optimize a bit better when we don't use return values from
        // dequeue... best guess is that using the return value causes an extra register or two to
        // be pushed to stack
        if was_head {
            if let Some(handle) = queue.head() {
                handle.waiter.start();
            }
        }
    }
}

/// Opaque marker type for try_lock result
#[derive(Debug, Default, Clone, Copy)]
pub struct TryLockError;

impl Display for TryLockError {
    #[inline]
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        write!(f, "lock is already held")
    }
}

impl core::error::Error for TryLockError {}

impl<P: Priority, T, Q: MutexQueue<P>> Debug for Mutex<P, T, Q>
where
    T: Debug,
    P: Default,
{
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        let mut d = f.debug_tuple("Mutex");
        match self.try_lock(P::default()) {
            Ok(data) => d.field(&data.deref()),
            Err(_) => d.field(&format_args!("<locked>")),
        };

        d.finish()
    }
}

impl<P: Priority, T, Q: MutexQueue<P>> Mutex<P, T, Q> {
    /// Try to acquire the lock without blocking or requesting eviction of the current holder.
    ///
    /// Priority will be stored in guard; higher priority requesters will try to evict the returned
    /// guard if the `evict` flag is enabled.
    ///
    /// i.e. the holder may wait / check for eviction via [MutexGuard::evicted].
    pub fn try_lock(
        &self,
        priority: impl Into<P>,
    ) -> Result<MutexGuard<'_, P, T, Q>, TryLockError> {
        let priority = priority.into();
        let mut queue = self.queue.write();

        if !queue.is_empty() {
            return Err(TryLockError);
        }

        let node = MutexWaiter::new(priority, true);
        let handle = queue.enqueue(node);

        Ok(MutexGuard {
            mutex: self,
            node: ManuallyDrop::new(handle),
        })
    }

    /// Acquire exclusive access to the locked resource, waiting until after higher priority
    /// requesters acquire and release the lock.
    ///
    /// If the `evict` feature is enabled, this will also notify the current holder to request it
    /// to release the lock if the current holder is lower priority.
    ///
    /// i.e. the holder may wait / check for eviction via [MutexGuard::evicted].
    ///
    /// Cancel Safety: This function is cancel safe.
    pub async fn lock(&self, priority: P) -> MutexGuard<'_, P, T, Q> {
        let priority = priority.into();
        let guard = {
            let mut queue = self.queue.write();

            let maybe_head = queue.head();
            if maybe_head.is_none() {
                let handle = queue.enqueue(MutexWaiter::new(priority, true));

                return MutexGuard {
                    mutex: self,
                    node: ManuallyDrop::new(handle),
                };
            }

            #[cfg(feature = "evict")]
            {
                let head = maybe_head.unwrap();

                if priority.compare(&maybe_head.unwrap().priority).is_gt() {
                    head.waiter.evict();
                }
            }

            let handle = queue.enqueue(MutexWaiter::new(priority, false));

            MutexGuard {
                mutex: self,
                node: ManuallyDrop::new(handle),
            }
        };

        WaiterFlagFut::<_, { waiter::WAITER_FLAG_HAS_LOCK }>::new(&guard).await;

        guard
    }

    /// Acquire exclusive access to the locked resource, waiting until higher priority requesters
    /// release the lock.
    ///
    /// Shorthand for [`self.lock`](Self::lock)`(priority.into())`.
    ///
    /// Cancel Safety: This function is cancel safe.
    pub fn lock_from(
        &self,
        priority: impl Into<P>,
    ) -> impl Future<Output = MutexGuard<'_, P, T, Q>> {
        self.lock(priority.into())
    }

    /// Acquire exclusive access to the locked resource, waiting until higher priority requesters
    /// release the lock.
    ///
    /// Shorthand for [`self.lock`](Self::lock)`(`[`Default::default()`]`)`.
    ///
    /// Cancel Safety: This function is cancel safe.
    pub fn lock_default(&self) -> impl Future<Output = MutexGuard<'_, P, T, Q>>
    where
        P: Default,
    {
        self.lock(Default::default())
    }

    #[inline]
    /// Create a new [Mutex], unlocked and ready for use.
    pub fn new(val: T) -> Self {
        Self {
            queue: Default::default(),
            data: UnsafeCell::new(val),
            _phantom: PhantomData,
        }
    }

    /// Create a new [Mutex], unlocked and ready for use.
    ///
    /// Available when the queue is [ConstDefault] if the `const-default` feature is enabled.
    ///
    /// All builtin queues are [ConstDefault].
    #[cfg(feature = "const-default")]
    pub const fn const_new(val: T) -> Self
    where
        Q: const_default::ConstDefault,
    {
        Self {
            queue: RwLock::new(Q::DEFAULT),
            data: UnsafeCell::new(val),
            _phantom: PhantomData,
        }
    }
}

#[cfg(feature = "const-default")]
impl<P: Priority, T: ConstDefault, Q: ConstDefault + MutexQueue<P>> ConstDefault
    for Mutex<P, T, Q>
{
    const DEFAULT: Self = Self::const_new(T::DEFAULT);
}
