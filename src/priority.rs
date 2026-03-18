//! Contains the [Priority] trait responsible for establishing entry position in [PriorityQueue].
#[cfg(doc)]
use crate::queue::PriorityQueue;
#[cfg(feature = "const-default")]
use const_default::ConstDefault;
use core::cmp::Ordering;

/// Trait used by [PriorityQueue] to determine the order by which nodes should be enqueued.
pub trait Priority {
    /// Compare a (possibly new) priority to an already queued one.
    ///
    /// The meaning for the results is as follows:
    /// - `Less`: Lower priority (i.e. should place after `other`)
    /// - `Equal`: Same priority - same priority; can be placed before or after `other`
    /// - `Greater`: Higher priority - Higher priority
    ///
    /// This **must** be transitive.
    fn compare(&self, other: &Self) -> Ordering;

    /// Compare a new (not in a queue) priority with an already queued one (old).
    ///
    /// [PriorityQueue]  implementers must always use this function when enqueueing or re-queueing
    /// an entry to compare it with existing ones.
    ///
    /// This does not need to be transitive (and often shouldn't be).
    ///
    /// This **must** return the same as [compare](Self::compare) if the result of [compare](Self::compare)
    /// isn't [Ordering::Equal].
    ///
    /// # Example (logic used in [FIFO]):
    /// ```
    /// fn compare_new(&self, old: &Self) -> Ordering {
    ///     // If priority is Equal, then return Less (as `old` was queued first)
    ///     self.0.compare(&old.0).then(Ordering::Less)
    /// }
    /// ```
    #[inline]
    fn compare_new(&self, old: &Self) -> Ordering {
        self.compare(old)
    }
}

impl<O: Ord> Priority for O {
    #[inline(always)]
    fn compare(&self, other: &Self) -> Ordering {
        self.cmp(other)
    }
}

/// A redundant type alias existing to have parity with LowestFirst (as the default [Priority] impl
/// for [Ord] is highest first)
pub type HighestFirst<O> = O;

/// Reverses the result of [Ord::cmp] for `O`. [Less](Ordering::Less) -> [Greater][Ordering::Greater] and
/// [Greater](Ordering::Greater) -> [Less](Ordering::Less).
///
/// This *would* use [Reverse](core::cmp::Reverse), however that does not impl [`From<O>`] - which would make
/// locking more verbse. e.g. instead of `.acqurie_from(priority)` you'd need
/// `.acquire(core::cmp::Reverse(priority))`.
#[derive(Default, Debug, Clone, Copy)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
#[repr(transparent)]
pub struct LowestFirst<O: Ord>(O);

#[cfg(feature = "const-default")]
impl<O: Ord + ConstDefault> ConstDefault for LowestFirst<O> {
    const DEFAULT: Self = Self(O::DEFAULT);
}

impl<O: Ord> Priority for LowestFirst<O> {
    #[inline]
    fn compare(&self, other: &Self) -> Ordering {
        other.0.cmp(&self.0)
    }
}

impl<O: Ord> From<O> for LowestFirst<O> {
    #[inline]
    fn from(value: O) -> Self {
        Self(value)
    }
}

/// A priority wrapper where newer entries with the same priority are placed after existing entries.
#[derive(Default, Debug, Clone, Copy)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
#[repr(transparent)]
pub struct FIFO<P: Priority>(P);

impl<P: Priority> Priority for FIFO<P> {
    #[inline]
    fn compare(&self, other: &Self) -> Ordering {
        self.0.compare(&other.0)
    }

    #[inline]
    fn compare_new(&self, old: &Self) -> Ordering {
        // specifically use `compare_new` here as the inner logic may have its own logic that
        // either only needs to be run for new nodes.
        //
        // For fifo, if the old entry is the same priority, then we should place after.
        // thus, same priority = lower priority
        self.0.compare_new(&old.0).then(Ordering::Less)
    }
}

impl<O: Ord> From<O> for FIFO<LowestFirst<O>> {
    #[inline]
    fn from(value: O) -> Self {
        Self(value.into())
    }
}

impl<P: Priority> From<P> for FIFO<P> {
    #[inline]
    fn from(value: P) -> Self {
        Self(value)
    }
}

#[cfg(feature = "const-default")]
impl<P: Priority + ConstDefault> ConstDefault for FIFO<P> {
    const DEFAULT: Self = Self(P::DEFAULT);
}

/// A priority wrapper where newer entries with the same priority are placed before existing entries.
#[derive(Default, Debug, Clone, Copy)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
#[repr(transparent)]
pub struct LIFO<P: Priority>(P);

impl<P: Priority> Priority for LIFO<P> {
    #[inline]
    fn compare(&self, other: &Self) -> Ordering {
        self.0.compare(&other.0)
    }

    #[inline]
    fn compare_new(&self, old: &Self) -> Ordering {
        // specifically use `compare_new` here as the inner logic may have its own logic that
        // either only needs to be run for new nodes.
        //
        // Opposite of fifo - if the new entry has the same priority as the old one, then we
        // consider it a higher priority
        self.0.compare_new(&old.0).then(Ordering::Greater)
    }
}

impl<P: Priority> From<P> for LIFO<P> {
    #[inline]
    fn from(value: P) -> Self {
        Self(value)
    }
}

impl<O: Ord> From<O> for LIFO<LowestFirst<O>> {
    #[inline]
    fn from(value: O) -> Self {
        Self(value.into())
    }
}

#[cfg(feature = "const-default")]
impl<P: Priority + ConstDefault> ConstDefault for LIFO<P> {
    const DEFAULT: Self = Self(P::DEFAULT);
}
