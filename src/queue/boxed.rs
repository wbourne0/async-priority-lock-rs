//! Provides [BoxQueue] - where entries are each allocated via [Box].
//!
//! This is usually slower than [ArenaQueue], however it is usually a bit more memory efficient, as
//! nodes are deallocated when they are removed.
use core::{
    cell::UnsafeCell,
    marker::{PhantomData, PhantomPinned},
    mem::replace,
    pin::Pin,
    ptr::NonNull,
};

#[cfg(not(feature = "std"))]
extern crate alloc;
use super::*;

#[cfg(not(feature = "std"))]
use alloc::boxed::Box;
#[cfg(feature = "std")]
use std::boxed::Box;

#[cfg(feature = "const-default")]
impl<N: BoxQueueNode> const_default::ConstDefault for BoxQueue<N> {
    const DEFAULT: Self = Self { head: None };
}

/// A queue where nodes are independently allocated via Box.
///
/// This is usually much slower than arena queue, but is more memory efficient as the
/// memory used for each node is deallocated once the node is removed.
///
/// Usage is only recommended when memory efficiency is needed.  If possible, [DualLinkBoxQueue]
/// should be preferred over [SingleLinkBoxQueue], as it almost always faster.
///
/// (difference being that [DualLinkBoxQueueNode] has an extra [usize] worth of bytes per node)
pub struct BoxQueue<N: BoxQueueNode> {
    head: Option<Pin<Box<N>>>,
}

/// A single linked (next only) [BoxQueue].
///
/// Short for [`BoxQueue`]`<`[`SingleLinkBoxQueueNode<P>`]`>`
pub type SingleLinkBoxQueue<P> = BoxQueue<SingleLinkBoxQueueNode<P>>;
/// A dual linked [BoxQueue].
///
/// Short for [`ArenaQueue`]`<`[`DualLinkBoxQueueNode<P>`]`>`
pub type DualLinkBoxQueue<P> = BoxQueue<DualLinkBoxQueueNode<P>>;

impl<N: BoxQueueNode> BoxQueue<N> {
    #[inline]
    fn insert(&mut self, new_node: Pin<Box<N>>) -> <Self as PriorityQueue<N::Data>>::Handle {
        let ptr = BoxQueueHandle(NonNull::from_ref(&*new_node));
        if self.head.is_none() {
            self.head = Some(new_node);
            return ptr;
        }

        if new_node
            .data()
            // again, we run into a bug with the rust borrow checker where it fails to realize that
            // we don't hold a reference to curr in this scope after we return, so we can't use the
            // same reference we get from as_ref() -> unwrap as we do below...
            .compare_new(&self.head.as_ref().unwrap().data())
            .is_ge()
        {
            self.head.as_ref().unwrap().set_prev(Some(&*new_node));
            *new_node.next() = self.head.take();
            self.head = Some(new_node);

            return ptr;
        }

        let mut curr = self.head.as_ref().unwrap();
        while let Some(next) = curr.next() {
            if new_node.data().compare_new(&next.data()).is_ge() {
                break;
            }

            curr = next;
        }

        let next = curr.next().take();
        if let Some(nxt) = &next {
            nxt.set_prev(Some(&*new_node));
        }
        *new_node.next() = next;
        new_node.set_prev(Some(&*curr));
        *curr.next() = Some(new_node);

        ptr
    }
}

impl<N: BoxQueueNode> Default for BoxQueue<N> {
    #[inline]
    fn default() -> Self {
        Self {
            head: Default::default(),
        }
    }
}

impl<N: BoxQueueNode> BoxQueueHandle<N> {
    #[inline(always)]
    fn load(&self) -> &N::Data {
        unsafe { self.0.as_ref() }.data()
    }
}

impl<N: BoxQueueNode> From<&Pin<Box<N>>> for SharedBoxQueueHandle<N> {
    fn from(value: &Pin<Box<N>>) -> Self {
        Self(BoxQueueHandle(NonNull::from_ref(&value)))
    }
}

impl<N: BoxQueueNode> PriorityQueueHandle<N::Data> for BoxQueueHandle<N> {
    const LOAD_PURE: Option<unsafe fn(&Self) -> &N::Data> = Some(Self::load);
}

unsafe impl<N: BoxQueueNode> PriorityQueue<N::Data> for BoxQueue<N> {
    type Handle = BoxQueueHandle<N>;
    type SharedHandle = SharedBoxQueueHandle<N>;

    fn enqueue(&mut self, data: N::Data) -> Self::Handle {
        let new_node = Box::pin(N::new(data));

        self.insert(new_node)
    }

    #[inline(always)]
    fn get_by_handle(&self, handle: &Self::Handle) -> &N::Data {
        unsafe { handle.0.as_ref() }.data()
    }

    #[inline(always)]
    fn iter_at<'a>(&'a self, after: Option<&Self::Handle>) -> impl Iterator<Item = &'a N::Data>
    where
        N::Data: 'a,
    {
        if let Some(after) = after {
            return BoxQueueIterator(after.0.as_ptr().cast_const(), PhantomData);
        }

        BoxQueueIterator(
            self.head
                .as_ref()
                .map(|x| &raw const **x)
                .unwrap_or_default(),
            PhantomData,
        )
    }

    #[inline(always)]
    fn dequeue(
        &mut self,
        BoxQueueHandle(ptr): Self::Handle,
    ) -> (Option<&N::Data>, Option<Self::SharedHandle>) {
        let ptr = ptr.as_ptr().cast_const();
        if N::HAS_PREV {
            let node = unsafe { &*ptr };

            let next = node.next().take();
            if let Some(nxt) = &next {
                nxt.set_prev(node.prev());
            }

            let handle = next.as_ref().map(|x| x.into());
            if let Some(prev) = node.prev() {
                *prev.next() = next;
                return (Some(prev.data()), handle);
            }

            // if we didn't have a prev, we must have the head node
            self.head = next;
            return (None, handle);
        }

        if let Some(head) = self.head.as_ref() {
            if ptr == &**head {
                self.head = head.next().take();
                return (None, self.head.as_ref().map(|x| x.into()));
            }
        }

        if let Some(mut node) = self.head.as_ref() {
            while let Some(next) = node.next() {
                if ptr == &**next {
                    *node.next() = next.next().take();

                    return (Some(node.data()), node.next().as_ref().map(|x| x.into()));
                }

                node = &*next;
            }
        }

        (None, None)
    }

    #[inline]
    fn is_empty(&self) -> bool {
        self.head.is_none()
    }

    fn update_node(&mut self, handle: &Self::Handle, update: impl FnOnce(&mut N::Data) -> bool) {
        let ptr = handle.0.as_ptr();

        if !update(unsafe { &mut *ptr }.data_mut()) {
            return;
        }

        if N::HAS_PREV {
            let rf = unsafe { &*ptr };
            // is the current location good? (priority: prev >= curr >= next)
            if rf
                .prev()
                .is_none_or(|x| rf.data().compare(x.data()).is_le())
                && rf
                    .next()
                    .as_ref()
                    .is_none_or(|nxt| rf.data().compare(nxt.data()).is_ge())
            {
                return;
            }

            let next = rf.next().take();

            if let Some(nxt) = &next {
                nxt.set_prev(rf.prev());
            }

            let old_loc = rf.prev().map(|x| x.next()).unwrap_or(&mut self.head);

            rf.set_prev(None);
            let node = replace(old_loc, next).unwrap();

            self.insert(node);
            return;
        }

        if let Some(head) = self.head.as_mut() {
            if &raw const **head == ptr {
                if head
                    .next()
                    .as_ref()
                    .is_some_and(|next| head.data().compare(&next.data()).is_le())
                {
                    let next = head.next().take();
                    let node = replace(&mut self.head, next).unwrap();
                    self.insert(node);
                }

                return;
            }
        }

        if let Some(mut node) = self.head.as_mut() {
            while let Some(next) = node.next() {
                if &raw const **next == ptr {
                    // Check that the updated node is still <= the previous one and >= the next one
                    if !(next.data().compare(&node.data()).is_le()
                        && next
                            .next()
                            .as_ref()
                            .is_none_or(|next_next| next.data().compare(&next_next.data()).is_ge()))
                    {
                        let to_insert = replace(node.next(), next.next().take()).unwrap();

                        self.insert(to_insert);
                    }

                    return;
                }

                node = &mut *next;
            }
        }

        // this check isn't needed for compiling but:
        // 1. we should probably panic if we can't find the node
        // 2. this may allow the compiler to optimize a bit better
        unreachable!("handle must be valid")
    }

    #[inline(always)]
    fn head_handle(&self) -> Option<Self::SharedHandle> {
        self.head.as_ref().map(|x| x.into())
    }

    #[inline]
    fn get_next_handle(&self, handle: &Self::Handle) -> Option<Self::SharedHandle> {
        unsafe { handle.0.as_ref() }
            .next()
            .as_ref()
            .map(|x| x.into())
    }
}

/// An opaque handle to a [BoxQueue] node.
///
/// See [PriorityQueue::Handle].
pub struct BoxQueueHandle<N: BoxQueueNode>(NonNull<N>);

unsafe impl<N: BoxQueueNode> Send for BoxQueueHandle<N> {}
unsafe impl<N: BoxQueueNode> Sync for BoxQueueHandle<N> {}

struct BoxQueueIterator<'a, N: BoxQueueNode>(*const N, PhantomData<&'a ()>);

/// An opaque "shared" handle to a [BoxQueue] node.
///
/// See [PriorityQueue::SharedHandle].
pub struct SharedBoxQueueHandle<N: BoxQueueNode>(BoxQueueHandle<N>);

impl<N: BoxQueueNode> AsRef<BoxQueueHandle<N>> for SharedBoxQueueHandle<N> {
    #[inline(always)]
    fn as_ref(&self) -> &BoxQueueHandle<N> {
        &self.0
    }
}

impl<'a, N: 'a + BoxQueueNode> Iterator for BoxQueueIterator<'a, N> {
    type Item = &'a N::Data;

    #[inline(always)]
    fn next(&mut self) -> Option<Self::Item> {
        let curr = unsafe { self.0.as_ref()? };
        let rf = &*curr;

        self.0 = curr
            .next()
            .as_ref()
            .map(|x| &raw const **x)
            .unwrap_or_default();
        Some(rf.data())
    }
}

/// A helper trait for [BoxQueue] to allow for both single and dual-link queues.
///
/// This is not intended to be used publicly. Use [DualLinkBoxQueueNode] and
/// [SingleLinkBoxQueueNode].
#[doc(hidden)]
pub trait BoxQueueNode {
    type Data: Priority;
    const HAS_PREV: bool;

    fn new(data: Self::Data) -> Self;
    fn data(&self) -> &Self::Data;
    fn data_mut(self: &mut Self) -> &mut Self::Data;

    fn next<'a>(&self) -> &'a mut Option<Pin<Box<Self>>>;

    fn prev<'a>(&self) -> Option<&'a Self>;
    fn set_prev(&self, prev: Option<&Self>);
}

/// A node for [BoxQueue] which only links to next.
///
/// Direct usuage is likely to be a bit verbose; usage of the [SingleLinkBoxQueue] is recommended.
pub struct SingleLinkBoxQueueNode<P: Priority> {
    data: P,
    next: UnsafeCell<Option<Pin<Box<Self>>>>,
    _must_pin: core::marker::PhantomPinned,
}

unsafe impl<P: Priority + Sync> Sync for SingleLinkBoxQueueNode<P> {}

impl<P: Priority> BoxQueueNode for SingleLinkBoxQueueNode<P> {
    type Data = P;
    const HAS_PREV: bool = false;

    #[inline(always)]
    fn new(data: Self::Data) -> Self {
        Self {
            data,
            next: UnsafeCell::new(None),
            _must_pin: PhantomPinned,
        }
    }

    #[inline(always)]
    fn next<'a>(&self) -> &'a mut Option<Pin<Box<Self>>> {
        unsafe { &mut *self.next.get() }
    }

    #[inline(always)]
    fn data_mut(&mut self) -> &mut P {
        &mut self.data
    }

    #[inline(always)]
    fn data(&self) -> &Self::Data {
        &self.data
    }

    #[inline(always)]
    fn prev<'a>(&self) -> Option<&'a Self> {
        unreachable!("single linked box queues do not have prev")
    }

    #[inline(always)]
    fn set_prev<'a>(&self, _: Option<&Self>) {}
}

/// A node for [BoxQueue] which links to both the previous and next nodes.
///
/// Direct usuage is likely to be a bit verbose; usage of the [DualLinkBoxQueue] is recommended.
pub struct DualLinkBoxQueueNode<P: Priority> {
    data: P,
    next: UnsafeCell<Option<Pin<Box<Self>>>>,
    prev: UnsafeCell<Option<NonNull<Self>>>,
    _must_pin: core::marker::PhantomPinned,
}

unsafe impl<P: Priority + Send> Send for DualLinkBoxQueueNode<P> {}
unsafe impl<P: Priority + Sync> Sync for DualLinkBoxQueueNode<P> {}

impl<P: Priority> BoxQueueNode for DualLinkBoxQueueNode<P> {
    type Data = P;
    const HAS_PREV: bool = true;

    #[inline(always)]
    fn new(data: Self::Data) -> Self {
        Self {
            data,
            next: UnsafeCell::new(None),
            prev: UnsafeCell::new(None),
            _must_pin: PhantomPinned,
        }
    }

    #[inline(always)]
    fn next<'a>(&self) -> &'a mut Option<Pin<Box<Self>>> {
        unsafe { &mut *self.next.get() }
    }

    #[inline(always)]
    fn data_mut(&mut self) -> &mut Self::Data {
        &mut self.data
    }

    #[inline(always)]
    fn data(&self) -> &Self::Data {
        &self.data
    }

    #[inline(always)]
    fn prev<'a>(&self) -> Option<&'a Self> {
        let rf = unsafe { &*self.prev.get() };

        rf.map(|x| unsafe { x.as_ref() })
    }

    fn set_prev(&self, prev: Option<&Self>) {
        let rf = unsafe { &mut *self.prev.get() };

        *rf = prev.map(|x| NonNull::from_ref(x))
    }
}
