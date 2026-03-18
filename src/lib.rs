//! # Async Priority Lock
//! Primitives for priority-sorted synchronization of resources.
//!
//! Permits / lock guards are granted in order of priority, with the option to request
//! eviction when the `evict` flag is enabled.
//!
//! All [Future]s are cancel-safe.
//!
#![doc = include_str!("../feature-table.md")]
//!
//! Interrupt / signal safety: operations affecting the internal queues (such as
//! locking or releasing) are not safe to call in parallel on the same thread.
//!
//! i.e.: a deadlock may occur if the acquisition or dropping of a guard is interrupted, and the same
//! thread goes on to acquire or drop a guard from the same [Mutex] / [Semaphore].
//!
//! If this is non-desireable / will cause issues, feel free to create an issue.
//!
#![doc = include_str!("../CHANGELOG.md")]
#![cfg_attr(not(feature = "std"), no_std)]

mod internal;
use internal::*;
mod waiter;

#[cfg(all(doc, feature = "alloc"))]
extern crate alloc;

pub mod priority;
pub use priority::*;

pub mod queue;

#[cfg(feature = "mutex")]
pub mod mutex;
#[cfg(feature = "mutex")]
pub use mutex::*;

#[cfg(feature = "semaphore")]
pub mod semaphore;
#[cfg(feature = "semaphore")]
pub use semaphore::*;
