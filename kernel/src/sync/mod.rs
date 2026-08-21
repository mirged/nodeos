//! Kernel synchronisation primitives.

pub mod once;
pub mod spin;

pub use once::Once;
pub use spin::{IrqSpinLock, SpinLock};
