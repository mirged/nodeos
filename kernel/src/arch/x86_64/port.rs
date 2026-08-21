//! Raw x86 I/O port access.
//!
//! # Safety model
//!
//! Every function here is `unsafe`: an I/O port write is a side effect on a
//! device whose semantics the compiler knows nothing about. Callers must own
//! the port (in NodeOS terms: hold a `Device` capability for it, or be kernel
//! bootstrap code running before userspace exists).

use core::arch::asm;

/// Read one byte from an I/O port.
///
/// # Safety
/// The caller must own `port` and must tolerate the device-specific side
/// effects of a read (many device registers are read-to-clear).
#[inline]
pub unsafe fn inb(port: u16) -> u8 {
    let value: u8;
    // SAFETY: `in` with a 16-bit port selector; the only effect on the CPU
    // state is writing AL, which is captured by the output operand.
    unsafe { asm!("in al, dx", out("al") value, in("dx") port, options(nomem, nostack, preserves_flags)) };
    value
}

/// Write one byte to an I/O port.
///
/// # Safety
/// The caller must own `port`; the write is an arbitrary device side effect.
#[inline]
pub unsafe fn outb(port: u16, value: u8) {
    // SAFETY: `out` touches no memory the compiler tracks.
    unsafe { asm!("out dx, al", in("dx") port, in("al") value, options(nomem, nostack, preserves_flags)) };
}

/// Read one 32-bit word from an I/O port.
///
/// # Safety
/// See [`inb`].
#[inline]
pub unsafe fn inl(port: u16) -> u32 {
    let value: u32;
    // SAFETY: see `inb`.
    unsafe { asm!("in eax, dx", out("eax") value, in("dx") port, options(nomem, nostack, preserves_flags)) };
    value
}

/// Write one 32-bit word to an I/O port.
///
/// # Safety
/// See [`outb`].
#[inline]
pub unsafe fn outl(port: u16, value: u32) {
    // SAFETY: see `outb`.
    unsafe { asm!("out dx, eax", in("dx") port, in("eax") value, options(nomem, nostack, preserves_flags)) };
}

/// Waste roughly one ISA bus cycle, used to let slow legacy devices settle.
///
/// # Safety
/// Port `0x80` is the POST diagnostic port; writing it is harmless on every
/// machine NodeOS supports, but it is still a device write.
#[inline]
pub unsafe fn io_wait() {
    // SAFETY: 0x80 is unclaimed on all supported platforms.
    unsafe { outb(0x80, 0) };
}
