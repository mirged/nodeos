//! One-shot initialisation cell.

use core::cell::UnsafeCell;
use core::mem::MaybeUninit;
use core::sync::atomic::{AtomicU8, Ordering};

const UNINIT: u8 = 0;
const INITIALISING: u8 = 1;
const READY: u8 = 2;

/// A value initialised exactly once, then read-only forever.
///
/// Used for kernel singletons that cannot be `const`-constructed (the frame
/// allocator, the ACPI tables, the boot info). Reading before initialisation is
/// a kernel bug and panics rather than returning `Option`, because every such
/// site is on a path where the ordering is a static property of `kmain`.
pub struct Once<T> {
    state: AtomicU8,
    value: UnsafeCell<MaybeUninit<T>>,
}

// SAFETY: the state machine guarantees no reader observes `value` before the
// single writer has published it with a release store.
unsafe impl<T: Send + Sync> Sync for Once<T> {}
// SAFETY: as above.
unsafe impl<T: Send> Send for Once<T> {}

impl<T> Once<T> {
    /// Create an empty cell.
    pub const fn new() -> Self {
        Self { state: AtomicU8::new(UNINIT), value: UnsafeCell::new(MaybeUninit::uninit()) }
    }

    /// Initialise the cell. Panics if called twice.
    pub fn init(&self, value: T) -> &T {
        match self.state.compare_exchange(UNINIT, INITIALISING, Ordering::Acquire, Ordering::Acquire) {
            Ok(_) => {}
            Err(_) => panic!("Once::init called more than once"),
        }
        // SAFETY: we won the CAS, so we are the unique writer and no reader can
        // observe the cell until the release store below.
        unsafe { (*self.value.get()).write(value) };
        self.state.store(READY, Ordering::Release);
        self.get()
    }

    /// Whether the cell has been initialised.
    pub fn is_ready(&self) -> bool {
        self.state.load(Ordering::Acquire) == READY
    }

    /// Get the value. Panics if not yet initialised.
    #[track_caller]
    pub fn get(&self) -> &T {
        assert!(self.is_ready(), "Once::get before init");
        // SAFETY: state == READY was observed with acquire ordering, which
        // synchronises with the release store in `init`.
        unsafe { (*self.value.get()).assume_init_ref() }
    }

    /// Get the value if initialised.
    pub fn try_get(&self) -> Option<&T> {
        if self.is_ready() {
            // SAFETY: see `get`.
            Some(unsafe { (*self.value.get()).assume_init_ref() })
        } else {
            None
        }
    }
}

impl<T> Default for Once<T> {
    fn default() -> Self {
        Self::new()
    }
}
