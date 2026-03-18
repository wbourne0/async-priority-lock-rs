#![cfg(any(feature = "mutex", feature = "semaphore"))]
use core::ops::{Deref, DerefMut};

#[derive(Default)]
#[repr(transparent)]
pub(crate) struct RwLock<T>(
    #[cfg(all(feature = "spin", not(feature = "std")))] spin::RwLock<T>,
    #[cfg(feature = "std")] std::sync::RwLock<T>,
);

#[cfg(feature = "std")]
#[allow(unused)]
impl<T> RwLock<T> {
    pub(crate) const fn new(value: T) -> Self {
        Self(std::sync::RwLock::new(value))
    }

    #[inline]
    pub(crate) fn read(&self) -> impl Deref<Target = T> {
        self.0.read().unwrap()
    }

    #[inline]
    pub(crate) fn write(&self) -> impl SyncWriteGuard<T> {
        self.0.write().unwrap()
    }
}

#[cfg(all(feature = "spin", not(feature = "std")))]
#[allow(unused)]
impl<T> RwLock<T> {
    pub(crate) const fn new(value: T) -> Self {
        Self(spin::RwLock::new(value))
    }

    #[inline]
    pub(crate) fn read(&self) -> impl Deref<Target = T> {
        self.0.read()
    }

    #[inline]
    pub(crate) fn write(&self) -> impl SyncWriteGuard<T> {
        self.0.write()
    }
}

pub(crate) trait SyncWriteGuard<T>: Deref<Target = T> + DerefMut {
    #[allow(unused)]
    fn downgrade(self) -> impl Deref<Target = T>;
}

#[cfg(all(feature = "spin", not(feature = "std")))]
impl<'a, T> SyncWriteGuard<T> for spin::RwLockWriteGuard<'a, T> {
    #[inline(always)]
    fn downgrade(self) -> impl Deref<Target = T> {
        Self::downgrade(self)
    }
}

#[cfg(feature = "std")]
impl<'a, T> SyncWriteGuard<T> for std::sync::RwLockWriteGuard<'a, T> {
    #[inline(always)]
    fn downgrade(self) -> impl Deref<Target = T> {
        Self::downgrade(self)
    }
}
