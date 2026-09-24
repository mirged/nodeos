//! x86-64 architecture support.
//!
//! Everything in this module is, by construction, the only part of the kernel
//! allowed to know that the machine is an x86-64. The rest of NodeOS talks to
//! it through this module's exported surface.

pub mod apic;
pub mod boot;
pub mod cpu;
pub mod exception;
pub mod interrupt;
pub mod pci;
pub mod percpu;
pub mod pic;
pub mod pit;
pub mod port;
pub mod qemu;
pub mod segmentation;
pub mod trap;

pub use boot::{KERNEL_VMA, PHYSMAP_BASE};

/// Size of a base page on this architecture.
pub const PAGE_SIZE: usize = 4096;
/// `log2(PAGE_SIZE)`
pub const PAGE_SHIFT: usize = 12;
/// Size of a large page (a PDE with PS set).
pub const LARGE_PAGE_SIZE: usize = 2 * 1024 * 1024;
/// Size of a huge page (a PDPTE with PS set).
pub const HUGE_PAGE_SIZE: usize = 1024 * 1024 * 1024;

/// Bring the CPU's architectural state up: descriptor tables, per-CPU block,
/// interrupt vectors and the interrupt controller.
///
/// # Safety
/// Must run exactly once on the boot CPU, with interrupts disabled, before any
/// subsystem that can fault or take an interrupt.
pub unsafe fn init_boot_cpu() {
    // Order matters. `segmentation::init` reloads the segment registers, and
    // loading a null selector into GS zeroes `GS_BASE`, so per-CPU state must
    // be installed *after* it.
    // SAFETY: first call on the boot CPU, interrupts disabled.
    unsafe {
        segmentation::init(0);
        percpu::init(0, 0);
        trap::init_idt();
        interrupt::init();
    }
    // Re-record the local APIC id now that the APIC is up.
    percpu::current().lapic_id.store(apic::id(), core::sync::atomic::Ordering::Relaxed);
}
