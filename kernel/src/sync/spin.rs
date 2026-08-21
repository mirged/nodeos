//! Spin locks.
//!
//! NodeOS deliberately has only two locking primitives in the kernel:
//!
//! * [`SpinLock`] -- a plain ticket-free test-and-test-and-set lock, legal only
//!   where the lock is never taken from an interrupt handler.
//! * [`IrqSpinLock`] -- the same lock, but it disables local interrupts for the
//!   duration of the critical section. Anything an interrupt handler can touch
//!   must use this.
//!
//! There are no sleeping locks in the kernel. A microkernel operation that
//! would need to block is instead expressed as a thread blocking on a
//! *kernel object* (a channel, a futex-like wait queue), which the scheduler
//! understands. That keeps the lock hierarchy shallow enough to reason about.

use core::cell::UnsafeCell;
use core::fmt;
use core::hint;
use core::ops::{Deref, DerefMut};
use core::sync::atomic::{AtomicBool, Ordering};

use crate::arch::x86_64::cpu;

/// A mutual-exclusion primitive that busy-waits.
pub struct SpinLock<T: ?Sized> {
    locked: AtomicBool,
    value: UnsafeCell<T>,
}

// SAFETY: access to `value` is serialised by `locked`.
unsafe impl<T: ?Sized + Send> Sync for SpinLock<T> {}
// SAFETY: the lock itself adds no thread affinity.
unsafe impl<T: ?Sized + Send> Send for SpinLock<T> {}

impl<T> SpinLock<T> {
    /// Create an unlocked spin lock.
    pub const fn new(value: T) -> Self {
        Self { locked: AtomicBool::new(false), value: UnsafeCell::new(value) }
    }

    /// Consume the lock and return the protected value.
    pub fn into_inner(self) -> T {
        self.value.into_inner()
    }
}

impl<T: ?Sized> SpinLock<T> {
    /// Acquire the lock, spinning until it is available.
    pub fn lock(&self) -> SpinGuard<'_, T> {
        loop {
            if !self.locked.swap(true, Ordering::Acquire) {
                return SpinGuard { lock: self };
            }
            // Test-and-test-and-set: spin on a plain load so the cache line
            // stays shared instead of ping-ponging in the Modified state.
            while self.locked.load(Ordering::Relaxed) {
                hint::spin_loop();
            }
        }
    }

    /// Try to acquire the lock without spinning.
    pub fn try_lock(&self) -> Option<SpinGuard<'_, T>> {
        if self.locked.swap(true, Ordering::Acquire) {
            None
        } else {
            Some(SpinGuard { lock: self })
        }
    }

    /// Get a mutable reference without locking, using unique ownership.
    pub fn get_mut(&mut self) -> &mut T {
        self.value.get_mut()
    }

    /// Force-unlock. Used only by the panic path, which must be able to print
    /// diagnostics while another CPU holds the console lock.
    ///
    /// # Safety
    /// The caller must guarantee that no live guard exists, or accept the data
    /// race (the panic path accepts it: the machine is already dead).
    pub unsafe fn force_unlock(&self) {
        self.locked.store(false, Ordering::Release);
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

impl<T: ?Sized + fmt::Debug> fmt::Debug for SpinLock<T> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self.try_lock() {
            Some(g) => f.debug_tuple("SpinLock").field(&&*g).finish(),
            None => f.write_str("SpinLock(<locked>)"),
        }
    }
}

/// A [`SpinLock`] that also masks local interrupts while held.
pub struct IrqSpinLock<T: ?Sized> {
    inner: SpinLock<T>,
}

// SAFETY: delegates to `SpinLock`.
unsafe impl<T: ?Sized + Send> Sync for IrqSpinLock<T> {}
// SAFETY: delegates to `SpinLock`.
unsafe impl<T: ?Sized + Send> Send for IrqSpinLock<T> {}

impl<T> IrqSpinLock<T> {
    /// Create an unlocked interrupt-safe spin lock.
    pub const fn new(value: T) -> Self {
        Self { inner: SpinLock::new(value) }
    }
}

impl<T: ?Sized> IrqSpinLock<T> {
    /// Disable local interrupts and acquire the lock.
    pub fn lock(&self) -> IrqSpinGuard<'_, T> {
        let flags = cpu::interrupts_disable_save();
        let guard = self.inner.lock();
        IrqSpinGuard { guard: Some(guard), flags }
    }

    /// Try to acquire without spinning; interrupts are only masked on success.
    pub fn try_lock(&self) -> Option<IrqSpinGuard<'_, T>> {
        let flags = cpu::interrupts_disable_save();
        match self.inner.try_lock() {
            Some(guard) => Some(IrqSpinGuard { guard: Some(guard), flags }),
            None => {
                // SAFETY: we are restoring exactly the flags we saved.
                unsafe { cpu::interrupts_restore(flags) };
                None
            }
        }
    }

    /// See [`SpinLock::force_unlock`].
    ///
    /// # Safety
    /// Same contract as [`SpinLock::force_unlock`].
    pub unsafe fn force_unlock(&self) {
        // SAFETY: forwarded to the caller's obligation.
        unsafe { self.inner.force_unlock() };
    }
}

/// RAII guard for [`IrqSpinLock`]; restores the saved interrupt flag on drop.
pub struct IrqSpinGuard<'a, T: ?Sized> {
    guard: Option<SpinGuard<'a, T>>,
    flags: u64,
}

impl<T: ?Sized> Deref for IrqSpinGuard<'_, T> {
    type Target = T;
    fn deref(&self) -> &T {
        self.guard.as_ref().unwrap()
    }
}

impl<T: ?Sized> DerefMut for IrqSpinGuard<'_, T> {
    fn deref_mut(&mut self) -> &mut T {
        self.guard.as_mut().unwrap()
    }
}

impl<T: ?Sized> Drop for IrqSpinGuard<'_, T> {
    fn drop(&mut self) {
        // Release the lock *before* re-enabling interrupts, otherwise an
        // interrupt could arrive while we still hold it and deadlock on a
        // recursive acquisition.
        drop(self.guard.take());
        // SAFETY: restoring the flags captured at acquisition time.
        unsafe { cpu::interrupts_restore(self.flags) };
    }
}
