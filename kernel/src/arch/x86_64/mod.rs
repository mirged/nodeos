//! x86-64 architecture support.
//!
//! Everything in this module is, by construction, the only part of the kernel
//! allowed to know that the machine is an x86-64. The rest of NodeOS talks to
//! it through the re-exports at the bottom of this file.

pub mod boot;
pub mod cpu;
pub mod port;

pub use boot::{KERNEL_VMA, PHYSMAP_BASE};

/// Size of a base page on this architecture.
pub const PAGE_SIZE: usize = 4096;
/// `log2(PAGE_SIZE)`
pub const PAGE_SHIFT: usize = 12;
/// Size of a large page (PDE with PS set).
pub const LARGE_PAGE_SIZE: usize = 2 * 1024 * 1024;
/// Size of a huge page (PDPTE with PS set).
pub const HUGE_PAGE_SIZE: usize = 1024 * 1024 * 1024;
