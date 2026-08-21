//! Architecture abstraction layer.
//!
//! NodeOS currently targets x86-64 only, but the kernel is written against
//! this module rather than against `arch::x86_64` directly so that the
//! machine-dependent surface stays visible and small.

pub mod x86_64;

pub use x86_64 as target;
pub use x86_64::{HUGE_PAGE_SIZE, LARGE_PAGE_SIZE, PAGE_SHIFT, PAGE_SIZE};
