//! Raw syscall wrappers.
//!
//! One `syscall` instruction per function, no error state, no thread-local
//! anything. The `Result` these return is derived from `RAX` alone.

use core::arch::asm;

use nodeos_abi::{decode_status, Error, Status, Syscall};

/// Issue a syscall with six arguments.
///
/// # Safety
/// The arguments must satisfy whatever contract the specific syscall imposes;
/// most take user pointers that the kernel will validate but that must at least
/// be meaningful.
#[inline(always)]
pub unsafe fn syscall6(number: Syscall, a0: u64, a1: u64, a2: u64, a3: u64, a4: u64, a5: u64) -> i64 {
    let result: i64;
    // SAFETY: `syscall` clobbers RCX and R11 (return address and flags), which
    // are declared. The kernel preserves everything else.
    unsafe {
        asm!(
            "syscall",
            inlateout("rax") number as u64 => result,
            in("rdi") a0,
            in("rsi") a1,
            in("rdx") a2,
            in("r10") a3,
            in("r8") a4,
            in("r9") a5,
            lateout("rcx") _,
            lateout("r11") _,
            options(nostack)
        );
    }
    result
}

/// Issue a syscall with no arguments.
///
/// # Safety
/// See [`syscall6`].
#[inline(always)]
pub unsafe fn syscall0(number: Syscall) -> i64 {
    // SAFETY: forwarded contract.
    unsafe { syscall6(number, 0, 0, 0, 0, 0, 0) }
}

/// Issue a syscall with one argument.
///
/// # Safety
/// See [`syscall6`].
#[inline(always)]
pub unsafe fn syscall1(number: Syscall, a0: u64) -> i64 {
    // SAFETY: forwarded contract.
    unsafe { syscall6(number, a0, 0, 0, 0, 0, 0) }
}

/// Issue a syscall with two arguments.
///
/// # Safety
/// See [`syscall6`].
#[inline(always)]
pub unsafe fn syscall2(number: Syscall, a0: u64, a1: u64) -> i64 {
    // SAFETY: forwarded contract.
    unsafe { syscall6(number, a0, a1, 0, 0, 0, 0) }
}

/// Issue a syscall with three arguments.
///
/// # Safety
/// See [`syscall6`].
#[inline(always)]
pub unsafe fn syscall3(number: Syscall, a0: u64, a1: u64, a2: u64) -> i64 {
    // SAFETY: forwarded contract.
    unsafe { syscall6(number, a0, a1, a2, 0, 0, 0) }
}

/// Issue a syscall with four arguments.
///
/// # Safety
/// See [`syscall6`].
#[inline(always)]
pub unsafe fn syscall4(number: Syscall, a0: u64, a1: u64, a2: u64, a3: u64) -> i64 {
    // SAFETY: forwarded contract.
    unsafe { syscall6(number, a0, a1, a2, a3, 0, 0) }
}

/// Issue a syscall with five arguments.
///
/// # Safety
/// See [`syscall6`].
#[inline(always)]
pub unsafe fn syscall5(number: Syscall, a0: u64, a1: u64, a2: u64, a3: u64, a4: u64) -> i64 {
    // SAFETY: forwarded contract.
    unsafe { syscall6(number, a0, a1, a2, a3, a4, 0) }
}

/// Convert a raw return value into a [`Status`].
#[inline]
pub fn status(raw: i64) -> Status {
    decode_status(raw)
}

/// Convert a raw return value into `Result<(), Error>`.
#[inline]
pub fn ok(raw: i64) -> Result<(), Error> {
    status(raw).map(|_| ())
}
