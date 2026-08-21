//! Powering the machine off.
//!
//! Two mechanisms, tried in order:
//!
//! * QEMU's `isa-debug-exit` device, which turns a port write into a process
//!   exit status. This is how `make test` gets an answer out of a boot without
//!   parsing a log for it.
//! * ACPI soft-off via the PM1a control register, for the general case.
//!
//! Neither exists on all hardware, so both are best-effort and the caller must
//! be prepared to halt instead.

use super::port;

/// I/O port of QEMU's `isa-debug-exit` device, as configured in the Makefile.
pub const DEBUG_EXIT_PORT: u16 = 0xF4;

/// Status codes NodeOS reports through the debug-exit device.
///
/// QEMU exits with `(value << 1) | 1`, so these become 33 and 35.
pub mod status {
    /// Everything the boot asserted held.
    pub const SUCCESS: u32 = 0x10;
    /// Something failed; the log says what.
    pub const FAILURE: u32 = 0x11;
}

/// Ask the emulator to exit with `code`. Returns if no such device is present.
///
/// # Safety
/// Writes to a fixed I/O port. On real hardware `0xF4` is unclaimed, so the
/// write is inert rather than harmful, but it is still a device write.
pub unsafe fn debug_exit(code: u32) {
    // SAFETY: the caller accepts responsibility for the port write; on QEMU
    // this terminates the process and never returns.
    unsafe { port::outl(DEBUG_EXIT_PORT, code) };
}

/// Attempt ACPI soft-off through the well-known QEMU/Bochs PM1a port.
///
/// # Safety
/// Writes to an I/O port that is only meaningful when the platform matches.
pub unsafe fn acpi_poweroff() {
    // SLP_TYPa = 5, SLP_EN = 1 << 13: the standard S5 request. 0x604 is where
    // QEMU, Bochs and VirtualBox all put PM1a_CNT.
    // SAFETY: best-effort; an unclaimed port write is inert.
    unsafe {
        port::outb(0x604, 0x00);
        port::outb(0x605, 0x20);
    }
}

/// Shut the machine down, or halt forever if it cannot be shut down.
pub fn power_off(success: bool) -> ! {
    let code = if success { status::SUCCESS } else { status::FAILURE };
    // SAFETY: both are best-effort platform writes on the way to halting.
    unsafe {
        debug_exit(code);
        acpi_poweroff();
    }
    super::cpu::halt_forever()
}
