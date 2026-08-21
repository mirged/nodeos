//! # NodeOS
//!
//! An experimental capability-based, message-oriented microkernel for x86-64.
//!
//! NodeOS is deliberately *not* a Unix. There is no `fork`, no file
//! descriptors, no ambient authority, no `root`. The kernel implements exactly
//! nine concepts -- address spaces, memory objects, processes, threads,
//! channels, capabilities, timers, interrupts, and a scheduler -- and every
//! other facility (filesystems, networking, terminals, device drivers,
//! graphics) is a userspace process reached over typed IPC.
//!
//! See `docs/ARCHITECTURE.md` for the design rationale.

#![no_std]
#![no_main]
#![deny(missing_docs)]
#![deny(unsafe_op_in_unsafe_fn)]
#![allow(clippy::missing_safety_doc)] // we document safety in prose, not lints

extern crate alloc;

pub mod arch;
pub mod boot;
pub mod drivers;
pub mod ipc;
pub mod launch;
pub mod loader;
pub mod log;
pub mod mm;
pub mod obj;
pub mod panic;
pub mod rt;
pub mod sched;
pub mod selftest;
pub mod syscall;
pub mod sync;
pub mod task;
pub mod time;

use arch::x86_64::{boot as arch_boot, cpu};
use boot::BootInfo;

/// Kernel version string, reported at boot and over the `system` IPC service.
pub const VERSION: &str = env!("CARGO_PKG_VERSION");

/// The Rust entry point, called by the long-mode trampoline in
/// [`arch::x86_64::boot`].
///
/// # Safety
/// Called exactly once, by `_boot_entry64`, on the boot CPU, with:
/// * interrupts disabled,
/// * `.bss` zeroed,
/// * a 64 KiB stack in the high half,
/// * the identity map and the kernel window both live,
/// * `magic`/`info` holding the Multiboot2 handoff registers.
#[no_mangle]
pub extern "C" fn kmain(magic: u32, info: u64) -> ! {
    // SAFETY: first call, boot CPU, nothing else can be using COM1 yet.
    unsafe { drivers::serial::init() };

    banner();

    if magic != arch_boot::MULTIBOOT2_BOOTLOADER_MAGIC {
        panic!("not booted by a Multiboot2 loader (magic = {:#010x})", magic);
    }

    // SAFETY: `info` is the pointer the loader left in EBX and the identity
    // map is still installed, so the physical address is directly addressable.
    let boot_info = match unsafe { BootInfo::from_ptr(info as *const u8) } {
        Some(bi) => bi,
        None => panic!("Multiboot2 information structure at {info:#x} is malformed"),
    };

    if let Some(name) = boot_info.bootloader_name() {
        info!("loader: {name}");
    }
    if let Some(cmdline) = boot_info.cmdline() {
        info!("cmdline: {:?}", cmdline);
    }

    let mut usable = 0u64;
    let mut highest = 0u64;
    for region in boot_info.memory_map() {
        debug!(
            "e820: {:#018x}..{:#018x} {:?}",
            region.base,
            region.end(),
            region.kind
        );
        if region.kind == boot::RegionKind::Usable {
            usable += region.length;
            highest = highest.max(region.end());
        }
    }
    info!("physical memory: {} MiB usable, top of RAM {:#x}", usable / (1024 * 1024), highest);

    for module in boot_info.modules() {
        info!("module {:?}: {:#x}..{:#x} ({} KiB)", module.name, module.start, module.end, module.len() / 1024);
    }

    // ---- Clock -------------------------------------------------------------
    // Calibrated before the interrupt controller takes the PIT away, and before
    // anything wants to measure anything.
    // SAFETY: interrupts are disabled and the PIT is unowned.
    unsafe { time::init() };

    // ---- Architectural bring-up -------------------------------------------
    // SAFETY: first and only call, on the boot CPU, with interrupts disabled.
    unsafe { arch::x86_64::init_boot_cpu() };
    info!("descriptor tables, IDT and interrupt controller online");

    // SAFETY: the IDT is loaded, every vector has a stub, and the timer has a
    // handler. From here on the kernel may be interrupted -- but only at points
    // where it has explicitly enabled interrupts (see the interrupt discipline
    // in docs/ARCHITECTURE.md).
    unsafe { cpu::interrupts_enable() };

    // ---- Memory management -------------------------------------------------
    // SAFETY: first call, interrupts disabled, nothing has allocated yet.
    unsafe { mm::init(&boot_info) };
    // The identity map is gone now, so the loader's structure has to be
    // reached through the direct physical map instead.
    // SAFETY: the direct map covers the whole of physical memory.
    let boot_info = unsafe { boot_info.relocated(mm::layout::PHYSMAP_BASE) };
    mm::selftest();

    // ---- Tasking -----------------------------------------------------------
    // SAFETY: the memory manager is up, so address spaces can be created.
    unsafe { sched::init_boot_cpu() };
    // SAFETY: the scheduler exists, so faults can be attributed to a thread.
    unsafe { mm::init_fault_handler() };
    info!("scheduler online: this context is now idle/0");

    // Prove the timer is actually ticking before anything depends on it.
    let before = arch::x86_64::interrupt::ticks();
    for _ in 0..2_000_000u64 {
        core::hint::spin_loop();
    }
    let after = arch::x86_64::interrupt::ticks();
    info!("timer: {} ticks observed ({} Hz configured)", after - before, arch::x86_64::interrupt::TICK_HZ);
    assert!(after > before, "scheduler tick is not advancing");

    let _ = boot_info.cmdline();

    selftest::run();

    // ---- Syscalls and userspace ---------------------------------------------
    // SAFETY: interrupts are disabled and the GDT has the SYSCALL layout.
    unsafe { syscall::init() };

    // SAFETY: memory management and the scheduler are up, and `boot_info` has
    // been relocated to the direct physical map.
    let launched = unsafe { launch::launch_boot_modules(&boot_info) };
    info!("{} boot module(s) launched; entering the idle loop", launched.len());

    sched::idle_loop()
}



fn banner() {
    kprintln!();
    kprintln!("\x1b[1;36m  ##    ##                #########   ######\x1b[0m");
    kprintln!("\x1b[1;36m  ###   ##  ####   ####   ##     ##  ##    ##\x1b[0m");
    kprintln!("\x1b[1;36m  ## ## ## ##  ## ##  ##  ##     ##  ##\x1b[0m");
    kprintln!("\x1b[1;36m  ##  #### ##  ## ##  ##  ##     ##   ######\x1b[0m");
    kprintln!("\x1b[1;36m  ##   ###  ####   ####   #########  ######\x1b[0m");
    kprintln!();
    kprintln!("  \x1b[1mNodeOS {VERSION}\x1b[0m -- capability-based research microkernel (x86-64)");
    kprintln!();
}
