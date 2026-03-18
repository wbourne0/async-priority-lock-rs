//! Contains the [PriorityQueue] trait and builtin impls when features are enabled.
//!
//! Provided queues are [BoxQueue] and [ArenaQueue].
#[cfg(feature = "box-queue")]
pub mod boxed;
#[cfg(feature = "box-queue")]
pub use boxed::*;
#[cfg(feature = "arena-queue")]
pub mod arena;
#[cfg(feature = "arena-queue")]
pub use arena::*;

use crate::Priority;

/// A PriorityQueue is a queue which orders entries by priority - higher priority first.
///
/// This is marked as unsafe as some associated types exist which must behave correctly, in
/// particular:
/// - An enqueued node will return a Self::Handle
/// - Until that handle is dequeued via self.dequeue, we must be able to lookup the node - even if
///   that node is repositioned.
/// - "shared" handles - these allow us to lookup nodes without owning them or retaining a
///   reference to the queue (allowing us to update them, but not remove them)
///
/// Any lookups via a handle which was not returned from enqueue or other functions is UB.
/// Implementers can choose to panic or simply assume that all handles provided will be valid.
///
/// Additionally, it is required that all nodes be dequeued before the queue is dropped - so
/// the queue should not have any entries when it is dropped.
pub unsafe trait PriorityQueue<P: Priority>: Default {
    type Handle: PriorityQueueHandle<P>;
    type SharedHandle: AsRef<Self::Handle>;

    /// Enqueue an entry with provided data.  This must return a unique handle for this node.
    fn enqueue(&mut self, data: P) -> Self::Handle;
    /// Dequeue an entry by its handle
    fn dequeue(&mut self, handle: Self::Handle) -> (Option<&P>, Option<Self::SharedHandle>);

    /// Get an entry's data by its handle
    fn get_by_handle(&self, handle: &Self::Handle) -> &P;

    #[inline(always)]
    /// Iterate through the queue from the start.
    fn iter<'a>(&'a self) -> impl Iterator<Item = &'a P>
    where
        P: 'a,
    {
        self.iter_at(None)
    }

    /// Iterate through the queue starting at a provided handle (or None to start with the head
    /// node) (including the provided value)
    fn iter_at<'a>(&'a self, handle: Option<&Self::Handle>) -> impl Iterator<Item = &'a P>
    where
        P: 'a;

    /// Returns true if the queue is empty
    #[inline]
    fn is_empty(&self) -> bool {
        self.iter().next().is_none()
    }

    /// Update a node via handle ref.  `update` is a fn that should return true if the node's
    /// priority changed.  i.e. if `update` returns false, the queue does not need to consider
    /// repositioning the changed node.
    fn update_node(&mut self, handle: &Self::Handle, update: impl FnOnce(&mut P) -> bool);

    #[inline]
    /// Get the first node in the queue
    fn head(&self) -> Option<&P> {
        self.iter().next()
    }

    /// Get a SharedHandle for the first node in the queue
    fn head_handle(&self) -> Option<Self::SharedHandle>;

    /// Get the handle for the next node after an input handle (or None if the provided handle is
    /// the tail node)
    fn get_next_handle(&self, handle: &Self::Handle) -> Option<Self::SharedHandle>;
}

/// The "handle" for an entry in a [PriorityQueue].
///
/// Allows for skipping a read lock on the owning [PriorityQueue] if a reference to the data can be
/// retrieved without a reference to the owning queue.
pub trait PriorityQueueHandle<D: Priority> {
    /// If the entry can be loaded with only the handle (without needing access to the queue
    /// itself), then this can optionally be set to a function which loads it directly.
    ///
    /// It is up to the caller to ensure that none of the referenced data used can be changed while
    /// a ref is held (e.g. make sure that update_node is not called on the node, or if it is
    /// called, none of the data referenced will be mutated)
    const LOAD_PURE: Option<unsafe fn(&Self) -> &D> = None;
}
