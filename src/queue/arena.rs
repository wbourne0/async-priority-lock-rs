//! Provides [ArenaQueue] - a [PriorityQueue] where entries are placed in an "arena."
//!
//! Instead of having many separate allocations like [BoxQueue], this places all entries in a
//! single, sequential block of memory to leverage cpu cache more effectively.
use super::*;
use crate::priority::Priority;
#[cfg(not(feature = "std"))]
use alloc::alloc::{alloc, dealloc, realloc};
use core::{alloc::Layout, cmp::Ordering, marker::PhantomData, mem::MaybeUninit};
#[cfg(feature = "std")]
use std::alloc::{alloc, dealloc, realloc};

#[cfg(not(feature = "std"))]
extern crate alloc;

/// Helper trait used for [ArenaQueue] to support both single and dual link lists.
///
/// This isn't intended to be used externally.  Use [DualLinkArenaQueueNode] and
/// [SingleLinkArenaQueueNode].
#[doc(hidden)]
pub trait ArenaQueueNode: Sized {
    type Data: Priority;
    fn next(&self) -> usize;
    fn set_next(&mut self, val: usize);
    fn prev(&self) -> usize;
    fn set_prev(&mut self, val: usize);
    fn data(&self) -> &Self::Data;
    fn data_mut(&mut self) -> &mut MaybeUninit<Self::Data>;

    const HAS_PREV: bool;
}

/// A single linked (next only) arena queue.
///
/// Short for [`ArenaQueue`]`<`[`SingleLinkArenaQueueNode<P>`]`>`
pub type SingleLinkArenaQueue<P> = ArenaQueue<SingleLinkArenaQueueNode<P>>;
/// A dual linked [ArenaQueue].
///
/// Short for [`ArenaQueue`]`<`[`DualLinkArenaQueueNode<P>`]`>`
pub type DualLinkArenaQueue<P> = ArenaQueue<DualLinkArenaQueueNode<P>>;

/// An arena queue - instead of having each node as a separate allocation, a block of memory is
/// allocated for all of the nodes.
///
/// The block will never shrink, but will grow to accommodate more slots as needed.
///
/// Currently, the amount of slots allocated is always a power of two and is doubled when capacity is
/// exceeded.
///
/// If this is an issue, [BoxQueue] can be used with the `box-queue` feature flag enabled.
///
/// It's likely easier to use this via the type aliases `SingleLinkArenaQueue` and
/// `DualLinkArenaQueue`.
pub struct ArenaQueue<N: ArenaQueueNode> {
    /// A pointer to the block of nodes. This is null until entries are queued.
    data: *mut N,
    /// The count of slots allocated (always a power of 2)
    size: usize,
    /// The count of slots used
    used: usize,
    /// The last known "good"  / uninitialized slot.  This may not always be accurate (in which
    /// case we iterate to find a free slot).
    good: usize,
    /// The offset of the head node from data.
    head: usize,
}

#[cfg(feature = "const-default")]
impl<N: ArenaQueueNode> const_default::ConstDefault for ArenaQueue<N> {
    const DEFAULT: Self = Self {
        data: std::ptr::null_mut(),
        size: 0,
        used: 0,
        good: 0,
        head: 0,
    };
}

impl<D: ArenaQueueNode> core::fmt::Debug for ArenaQueue<D> {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("SeqQueue")
            .field("data", &self.data)
            .field("size", &self.size)
            .field("used", &self.used)
            .field("good", &self.good)
            .field("head", &self.head)
            .finish()
    }
}

unsafe impl<D: Send + ArenaQueueNode> Send for ArenaQueue<D> {}
unsafe impl<D: Sync + ArenaQueueNode> Sync for ArenaQueue<D> {}

impl<N: ArenaQueueNode> Default for ArenaQueue<N> {
    fn default() -> Self {
        Self {
            data: Default::default(),
            size: Default::default(),
            used: Default::default(),
            good: Default::default(),
            head: 1,
        }
    }
}

/// The sentinel value used in the last node in the queue
const SENTINEL: usize = 1;
/// The value used in uninitialized nodes.
const UNALLOCATED: usize = usize::MAX;

// This is needed because in theory a usize could be 1 byte, and D could be a zero-width type,
// making the size and alignment of this type to be 1.
// (and thus an offset of 1 would be possible, which would conflict with our sentinel value)
//
// In practice this is probably never going to happen.
#[repr(align(2))]

/// A node for an [ArenaQueue] which links only to the next node.
///
/// This is likely a bit verbose for normal use; the [SingleLinkArenaQueue] type alias is
/// recommended instead.
pub struct SingleLinkArenaQueueNode<D: Priority> {
    next: usize,
    data: MaybeUninit<D>,
}

impl<D: Priority> ArenaQueueNode for SingleLinkArenaQueueNode<D> {
    type Data = D;

    #[inline(always)]
    fn next(&self) -> usize {
        self.next
    }

    #[inline(always)]
    fn set_next(&mut self, val: usize) {
        self.next = val
    }

    #[inline(always)]
    fn prev(&self) -> usize {
        unreachable!("this fn should never be compiled")
    }

    #[inline(always)]
    fn set_prev(&mut self, _: usize) {}
    //
    #[inline(always)]
    fn data(&self) -> &Self::Data {
        unsafe { self.data.assume_init_ref() }
    }

    #[inline(always)]
    fn data_mut(&mut self) -> &mut MaybeUninit<Self::Data> {
        &mut self.data
    }

    const HAS_PREV: bool = false;
}

/// A node for an [ArenaQueue] which links to both the previous and next nodes.
///
/// This is likely a bit verbose for normal use; the [DualLinkArenaQueue] type alias is
/// recommended instead.
pub struct DualLinkArenaQueueNode<P: Priority> {
    next: usize,
    prev: usize,
    data: MaybeUninit<P>,
}

impl<P: Priority> ArenaQueueNode for DualLinkArenaQueueNode<P> {
    type Data = P;

    #[inline(always)]
    fn next(&self) -> usize {
        self.next
    }

    #[inline(always)]
    fn set_next(&mut self, val: usize) {
        self.next = val;
    }

    #[inline(always)]
    fn prev(&self) -> usize {
        self.prev
    }

    #[inline(always)]
    fn set_prev(&mut self, val: usize) {
        self.prev = val
    }

    #[inline(always)]
    fn data(&self) -> &Self::Data {
        unsafe { self.data.assume_init_ref() }
    }

    #[inline(always)]
    fn data_mut(&mut self) -> &mut MaybeUninit<Self::Data> {
        &mut self.data
    }

    const HAS_PREV: bool = true;
}

/// An opaque handle to an [ArenaQueue] entry.
pub struct ArenaQueueHandle<N: ArenaQueueNode> {
    offset: usize,
    data: PhantomData<N>,
}

impl<N: ArenaQueueNode> ArenaQueueHandle<N> {
    const fn new(offset: usize) -> Self {
        Self {
            offset,
            data: PhantomData,
        }
    }
}

#[repr(transparent)]
/// An opaque "shared" handle to an [ArenaQueue] entry.
pub struct SharedArenaQueueHandle<N: ArenaQueueNode>(ArenaQueueHandle<N>);

impl<N: ArenaQueueNode> SharedArenaQueueHandle<N> {
    #[inline(always)]
    const fn maybe_new(offset: usize) -> Option<Self> {
        if offset != SENTINEL {
            Some(Self(ArenaQueueHandle::new(offset)))
        } else {
            None
        }
    }
}

impl<N: ArenaQueueNode> AsRef<ArenaQueueHandle<N>> for SharedArenaQueueHandle<N> {
    #[inline(always)]
    fn as_ref(&self) -> &ArenaQueueHandle<N> {
        &self.0
    }
}

impl<N: ArenaQueueNode> From<ArenaQueueHandle<N>> for SharedArenaQueueHandle<N> {
    #[inline(always)]
    fn from(value: ArenaQueueHandle<N>) -> Self {
        Self(value)
    }
}

impl<N: ArenaQueueNode> PriorityQueueHandle<N::Data> for ArenaQueueHandle<N> {
    const LOAD_PURE: Option<unsafe fn(&Self) -> &N::Data> = None;
}

impl<N: ArenaQueueNode> ArenaQueue<N> {
    #[inline]
    fn nodes<'a>(&self) -> &'a mut [N] {
        unsafe { core::slice::from_raw_parts_mut(self.data, self.size) }
    }

    #[inline]
    fn node<'a>(&self, offset: usize) -> &'a mut N {
        unsafe { &mut *self.data.byte_add(offset) }
    }

    #[inline]
    fn offset_of(&self, node: *const N) -> usize {
        node as usize - self.data as usize
    }

    #[inline]
    fn iter_at(&self, start: usize) -> ArenaQueueIterator<'_, N> {
        ArenaQueueIterator {
            queue: self,
            current: if start != SENTINEL {
                Some(self.node(start))
            } else {
                None
            },
        }
    }

    #[inline]
    fn expand(&mut self) {
        let old_size = self.size;
        self.data = if self.size == 0 {
            self.size = 4;

            unsafe {
                // std::alloc::Global
                alloc(Layout::array::<N>(self.size).unwrap())
            }
        } else {
            let old_layout = Layout::array::<N>(self.size).unwrap();
            self.size <<= 1;

            unsafe { realloc(self.data.cast(), old_layout, old_layout.size() << 1) }
        }
        .cast::<N>();

        for item in &mut self.nodes()[old_size..] {
            item.set_next(UNALLOCATED);
        }

        self.good = old_size;
    }

    /// Returns a reference to the new node, plus:
    /// (if found while searching for a location to place the node): a node before this one
    /// (hopefully the latest one) and a bool which indicates that the returned previous node has
    /// the same priority, thus we can place the new node immediately after it
    /// This is only used via PriorityQueue.enqueue; it's a separate fn for readability
    #[inline]
    fn emplace<'a>(&mut self, data: N::Data) -> (&'a mut N, Option<&'a mut N>, bool) {
        let nodes = self.nodes();

        let mut lowest_prio_prev: Option<&mut N> = None;
        let mut found_prev: bool = false;
        // idx is alwayys a power of 2
        let idx_mask = self.size - 1;
        let mut idx_to_try = self.good;

        loop {
            let node = &mut self.nodes()[idx_to_try];
            let node_data = node.data();

            if node.next() == UNALLOCATED {
                break;
            }

            // fast modulo (size is always a power of 2)
            idx_to_try = (idx_to_try + 1) & idx_mask;

            if found_prev {
                continue;
            }

            match data.compare_new(node_data) {
                Ordering::Greater => {}
                Ordering::Equal => {
                    lowest_prio_prev = Some(node);
                    found_prev = true;
                }
                Ordering::Less => {
                    if lowest_prio_prev
                        .as_ref()
                        .is_none_or(|b| node_data.compare(b.data()).is_gt())
                    {
                        lowest_prio_prev = Some(node);
                    }
                }
            }
        }

        let new_node = &mut nodes[idx_to_try];
        self.good = (idx_to_try + 1) & idx_mask;
        self.used += 1;
        new_node.data_mut().write(data);

        (new_node, lowest_prio_prev, found_prev)
    }

    #[inline]
    /// Assumes that the node definitely has a previous node (i.e. is not head)
    fn prev_for<'a>(&self, node: &N, offset: usize) -> &'a mut N {
        if N::HAS_PREV {
            return self.node(node.prev());
        }

        // need to search for previous node
        for x in self.nodes() {
            if x.next() == offset {
                return x;
            }
        }

        unreachable!()
    }

    #[inline]
    /// prev + next must be correlated with node.prev() & node.next()
    fn reposition(
        &mut self,
        node: &mut N,
        node_offset: usize,
        prev: Option<&mut N>,
        next: Option<&mut N>,
    ) {
        if let Some(pr) = prev {
            pr.set_next(node.next());
            if N::HAS_PREV {
                if let Some(nxt) = next {
                    nxt.set_prev(node.prev())
                }
            }

            return self.insert(node, node_offset, None);
        }

        self.head = node.next();
        if let Some(nxt) = next {
            nxt.set_prev(SENTINEL);
        }

        // since this node was just in head and that was invalid, then we can start after head.
        self.insert(node, node_offset, Some(self.node(self.head)));
    }

    #[inline]
    fn insert(&mut self, new_node: &mut N, new_offset: usize, after: Option<&mut N>) {
        let new_data = new_node.data();
        // Now we need to find out where to start searching for a spot
        let mut prev = match after {
            // If, while finding a slot for our new node, we found an existing item with higher
            // priority: we can start with that.
            Some(x) => x,
            None => {
                // We didn't find an existing node with higher priority.
                if self.head == SENTINEL {
                    // The queue is empty (as we have no head node)
                    // So we just need to place and set the head to the new node
                    self.head = new_offset;
                    new_node.set_next(SENTINEL);

                    new_node.set_prev(SENTINEL);

                    return;
                }

                let head = self.node(self.head);

                // Check to see if we can place our new node before the current head (and if so, do
                // it)
                if new_data.compare_new(head.data()).is_ge() {
                    new_node.set_next(self.head);
                    self.head = new_offset;
                    new_node.set_prev(SENTINEL);
                    head.set_prev(new_offset);

                    return;
                }

                head
            }
        };

        // Iterate until we find a node we can place the new one after
        while prev.next() != SENTINEL {
            let next = self.node(prev.next());

            if new_data.compare_new(next.data()).is_ge() {
                // for single-ended nodes this is a no-op which should get optimized out
                next.set_prev(new_offset);
                break;
            }

            prev = next;
        }

        new_node.set_next(prev.next());
        new_node.set_prev(self.offset_of(prev));
        prev.set_next(new_offset);
    }
}

struct ArenaQueueIterator<'a, N: ArenaQueueNode> {
    queue: &'a ArenaQueue<N>,
    current: Option<&'a N>,
}

impl<'a, N: ArenaQueueNode> Iterator for ArenaQueueIterator<'a, N> {
    type Item = &'a N::Data;

    #[inline]
    fn next(&mut self) -> Option<Self::Item> {
        let current = self.current.take()?;

        if current.next() != 1 {
            self.current = Some(self.queue.node(current.next()))
        }

        Some(current.data())
    }
}

impl<N: ArenaQueueNode> Drop for ArenaQueue<N> {
    fn drop(&mut self) {
        if self.used != 0 {
            #[cfg(debug_assertions)]
            // should never happen in our code! but we can't just panic as this is technically a
            // public struct + trait
            panic!("dropped a queue with remaining nodes!!!! {:?}", self);

            #[cfg(not(debug_assertions))]
            for node in self.nodes() {
                if node.next() != UNALLOCATED {
                    unsafe {
                        node.data_mut().assume_init_drop();
                    }
                }
            }
        }

        if self.size != 0 {
            unsafe { dealloc(self.data.cast(), Layout::array::<N>(self.size).unwrap()) };
        };
    }
}

unsafe impl<N: ArenaQueueNode> PriorityQueue<N::Data> for ArenaQueue<N> {
    type Handle = ArenaQueueHandle<N>;
    type SharedHandle = SharedArenaQueueHandle<N>;

    #[inline]
    fn enqueue(&mut self, data: N::Data) -> Self::Handle {
        if self.size == self.used {
            self.expand();
        }

        let (new_node, biggest_prev, found_prev) = self.emplace(data);
        let new_offset = self.offset_of(new_node);
        let handle = ArenaQueueHandle {
            offset: new_offset,
            data: PhantomData,
        };

        // While placing our node, we found a node we can place it directly after
        if found_prev {
            let prev = biggest_prev.unwrap();
            let ex_next = prev.next();

            new_node.set_next(ex_next);
            prev.set_next(new_offset);
            // for single-ended nodes this is a no-op which should get optimized out (including the
            // lookup of ex_next)
            if ex_next != SENTINEL {
                self.node(ex_next).set_prev(new_offset);
            }
            new_node.set_prev(self.offset_of(prev));

            return handle;
        }

        self.insert(new_node, new_offset, biggest_prev);

        handle
    }

    #[inline]
    fn dequeue(&mut self, handle: Self::Handle) -> (Option<&N::Data>, Option<Self::SharedHandle>) {
        let old_node = self.node(handle.offset);
        self.good = handle.offset / core::mem::size_of::<N>();

        unsafe { old_node.data_mut().assume_init_drop() };
        let old_next = old_node.next();

        old_node.set_next(UNALLOCATED);
        self.used -= 1;

        if N::HAS_PREV && old_next != SENTINEL {
            self.node(old_next).set_prev(old_node.prev());
        }

        // is the target node the head node? if so we don't need to search for previous (as it
        // can't have one)
        if handle.offset == self.head {
            self.head = old_next;

            return (None, SharedArenaQueueHandle::maybe_new(old_next));
        };

        // since we just confirmed that old_node wasn't the head node, there must be a previous
        let prev = self.prev_for(&old_node, handle.offset);
        prev.set_next(old_next);

        return (
            Some(prev.data()),
            SharedArenaQueueHandle::maybe_new(old_next),
        );
    }

    #[inline]
    fn get_by_handle(&self, handle: &Self::Handle) -> &N::Data {
        unsafe { &*self.data.byte_add(handle.offset) }.data()
    }

    #[inline(always)]
    fn head_handle(&self) -> Option<Self::SharedHandle> {
        (self.head != SENTINEL).then(|| ArenaQueueHandle::new(self.head).into())
    }

    #[inline]
    fn iter_at<'a>(&'a self, after: Option<&Self::Handle>) -> impl Iterator<Item = &'a N::Data>
    where
        N::Data: 'a,
    {
        self.iter_at(after.map(|x| x.offset).unwrap_or_else(|| self.head))
    }

    fn update_node(&mut self, handle: &Self::Handle, update: impl FnOnce(&mut N::Data) -> bool) {
        let node = self.node(handle.offset);

        if !update(unsafe { node.data_mut().assume_init_mut() }) {
            return;
        }
        let node_off = self.offset_of(node);

        let data = node.data();
        let next_off = node.next();

        let mut maybe_prev =
            (handle.offset != self.head).then(|| self.prev_for(node, handle.offset));

        // check if the current position is valid after the current previous node (if not,
        // reposition)
        if let Some(prev) = maybe_prev.as_mut() {
            if data.compare(prev.data()).is_gt() {
                self.reposition(
                    node,
                    node_off,
                    Some(prev),
                    (next_off != SENTINEL).then(|| self.node(next_off)),
                );

                return;
            }
        }

        // check if the current position is valid before the next (if not, reposition)
        if next_off != SENTINEL {
            let next = self.node(next_off);
            if data.compare(next.data()).is_lt() {
                self.reposition(node, node_off, maybe_prev, Some(next));
            }
        }
    }

    #[inline]
    fn get_next_handle(&self, handle: &Self::Handle) -> Option<Self::SharedHandle> {
        let next = self.node(handle.offset).next();

        (next != SENTINEL).then(|| {
            ArenaQueueHandle {
                offset: next,
                data: PhantomData,
            }
            .into()
        })
    }
}
