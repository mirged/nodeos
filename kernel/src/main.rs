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

pub mod arch;
pub mod boot;
pub mod drivers;
pub mod log;
pub mod panic;
pub mod rt;
pub mod sync;

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

    info!("stage 0 complete: long mode, high half, console");
    cpu::halt_forever()
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
