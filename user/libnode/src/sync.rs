//! Userspace synchronisation.

use core::cell::UnsafeCell;
use core::ops::{Deref, DerefMut};
use core::sync::atomic::{AtomicBool, Ordering};

/// A spin lock that yields to the scheduler while it waits.
///
/// Userspace has no `cli`, so a lock here cannot mask anything; the best it can
/// do while contended is stop burning its timeslice, which `yield_now` does.
pub struct SpinLock<T: ?Sized> {
    locked: AtomicBool,
    value: UnsafeCell<T>,
}

// SAFETY: access to `value` is serialised by `locked`.
unsafe impl<T: ?Sized + Send> Sync for SpinLock<T> {}
// SAFETY: the lock adds no thread affinity.
unsafe impl<T: ?Sized + Send> Send for SpinLock<T> {}

impl<T> SpinLock<T> {
    /// An unlocked lock.
    pub const fn new(value: T) -> Self {
        Self { locked: AtomicBool::new(false), value: UnsafeCell::new(value) }
    }
}

impl<T: ?Sized> SpinLock<T> {
    /// Acquire the lock.
    pub fn lock(&self) -> SpinGuard<'_, T> {
        let mut spins = 0u32;
        while self.locked.swap(true, Ordering::Acquire) {
            spins += 1;
            if spins > 64 {
                crate::yield_now();
                spins = 0;
            } else {
                core::hint::spin_loop();
            }
        }
        SpinGuard { lock: self }
    }
}

/// RAII guard for [`SpinLock`].
pub struct SpinGuard<'a, T: ?Sized> {
    lock: &'a SpinLock<T>,
}

impl<T: ?Sized> Deref for SpinGuard<'_, T> {
    type Target = T;
    fn deref(&self) -> &T {
        // SAFETY: we hold the lock.
        unsafe { &*self.lock.value.get() }
    }
}

impl<T: ?Sized> DerefMut for SpinGuard<'_, T> {
    fn deref_mut(&mut self) -> &mut T {
        // SAFETY: we hold the lock exclusively.
        unsafe { &mut *self.lock.value.get() }
    }
}

impl<T: ?Sized> Drop for SpinGuard<'_, T> {
    fn drop(&mut self) {
        self.lock.locked.store(false, Ordering::Release);
    }
}
