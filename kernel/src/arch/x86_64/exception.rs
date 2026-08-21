//! CPU exception handling (vectors 0..32).
//!
//! # Policy
//!
//! An exception in *user* mode is a normal event: it is reported to whoever
//! holds the faulting thread's exception channel, and the thread is suspended
//! rather than killed, so a userspace supervisor (a debugger, a service
//! manager) can inspect and decide. Until IPC exists (stage 5) the fallback is
//! to terminate the thread.
//!
//! An exception in *kernel* mode is a bug. NodeOS panics: there is no
//! meaningful recovery from a broken kernel invariant, and pretending
//! otherwise turns one bug into a security hole. The two exceptions to that
//! rule are the fault-tolerant user-memory accessors (see `mm::uaccess`),
//! which register fixup entries.

use super::trap::TrapFrame;
use super::{cpu, percpu};
use crate::{error, kprintln};

/// Human-readable name for an architectural vector.
pub fn vector_name(vector: u8) -> &'static str {
    match vector {
        0 => "#DE divide error",
        1 => "#DB debug",
        2 => "NMI",
        3 => "#BP breakpoint",
        4 => "#OF overflow",
        5 => "#BR bound range exceeded",
        6 => "#UD invalid opcode",
        7 => "#NM device not available",
        8 => "#DF double fault",
        9 => "coprocessor segment overrun",
        10 => "#TS invalid TSS",
        11 => "#NP segment not present",
        12 => "#SS stack-segment fault",
        13 => "#GP general protection fault",
        14 => "#PF page fault",
        16 => "#MF x87 floating-point",
        17 => "#AC alignment check",
        18 => "#MC machine check",
        19 => "#XM SIMD floating-point",
        20 => "#VE virtualisation",
        21 => "#CP control protection",
        28 => "#HV hypervisor injection",
        29 => "#VC VMM communication",
        30 => "#SX security",
        _ => "reserved",
    }
}

/// Decoded `#PF` error code.
#[derive(Clone, Copy)]
pub struct PageFaultError(u64);

impl PageFaultError {
    /// The fault was a protection violation rather than a missing page.
    pub fn protection_violation(&self) -> bool {
        self.0 & 1 != 0
    }
    /// The access was a write.
    pub fn write(&self) -> bool {
        self.0 & 2 != 0
    }
    /// The access came from ring 3.
    pub fn user(&self) -> bool {
        self.0 & 4 != 0
    }
    /// A reserved bit was set in a paging structure.
    pub fn reserved_bit(&self) -> bool {
        self.0 & 8 != 0
    }
    /// The access was an instruction fetch.
    pub fn instruction_fetch(&self) -> bool {
        self.0 & 16 != 0
    }
}

impl core::fmt::Display for PageFaultError {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        write!(
            f,
            "{}{}{}{}{}",
            if self.protection_violation() { "protection " } else { "not-present " },
            if self.write() { "write " } else { "read " },
            if self.user() { "user " } else { "kernel " },
            if self.instruction_fetch() { "exec " } else { "" },
            if self.reserved_bit() { "reserved-bit " } else { "" },
        )
    }
}

/// Hook installed by the memory manager once it can service demand faults.
///
/// Returns `true` if the fault was resolved and the instruction should be
/// retried.
pub type PageFaultHandler = fn(address: u64, error: PageFaultError, frame: &mut TrapFrame) -> bool;

static mut PAGE_FAULT_HANDLER: Option<PageFaultHandler> = None;

/// Install the demand-paging handler.
///
/// # Safety
/// Call once, with interrupts disabled, during memory-manager bring-up.
pub unsafe fn set_page_fault_handler(handler: PageFaultHandler) {
    // SAFETY: single-threaded bring-up with interrupts disabled.
    unsafe { *core::ptr::addr_of_mut!(PAGE_FAULT_HANDLER) = Some(handler) };
}

/// Hook installed by the task layer to report a fault to userspace.
///
/// Returns `true` if the fault was handled (thread suspended or killed) and the
/// kernel should return to the scheduler rather than panic.
pub type UserFaultHandler = fn(frame: &mut TrapFrame) -> bool;

static mut USER_FAULT_HANDLER: Option<UserFaultHandler> = None;

/// Install the userspace exception reporter.
///
/// # Safety
/// Call once, with interrupts disabled, during task-layer bring-up.
pub unsafe fn set_user_fault_handler(handler: UserFaultHandler) {
    // SAFETY: single-threaded bring-up with interrupts disabled.
    unsafe { *core::ptr::addr_of_mut!(USER_FAULT_HANDLER) = Some(handler) };
}

/// Dispatch an architectural exception.
pub fn handle(frame: &mut TrapFrame) {
    let vector = frame.vector as u8;

    match vector {
        // #PF: the only exception with a fast path worth having.
        14 => {
            let address = cpu::read_cr2();
            let error = PageFaultError(frame.error_code);

            // SAFETY: the hook is installed once during bring-up and read here
            // with interrupts disabled.
            let handler = unsafe { *core::ptr::addr_of!(PAGE_FAULT_HANDLER) };
            if let Some(handler) = handler {
                let depth = if percpu::is_ready() {
                    percpu::current().fault_depth.fetch_add(1, core::sync::atomic::Ordering::Relaxed)
                } else {
                    0
                };
                if depth == 0 && handler(address, error, frame) {
                    if percpu::is_ready() {
                        percpu::current()
                            .fault_depth
                            .fetch_sub(1, core::sync::atomic::Ordering::Relaxed);
                    }
                    return;
                }
                if percpu::is_ready() {
                    percpu::current().fault_depth.fetch_sub(1, core::sync::atomic::Ordering::Relaxed);
                }
            }

            if frame.from_user() && report_to_user(frame) {
                return;
            }

            fatal(frame, Some((address, error)));
        }

        // #BP and #DB belong to whoever is debugging the faulting thread.
        1 | 3 if frame.from_user() => {
            if !report_to_user(frame) {
                fatal(frame, None);
            }
        }

        // Everything else.
        _ => {
            if frame.from_user() && report_to_user(frame) {
                return;
            }
            fatal(frame, None);
        }
    }
}

fn report_to_user(frame: &mut TrapFrame) -> bool {
    // SAFETY: installed once during bring-up.
    let handler = unsafe { *core::ptr::addr_of!(USER_FAULT_HANDLER) };
    match handler {
        Some(handler) => handler(frame),
        None => false,
    }
}

fn fatal(frame: &TrapFrame, page_fault: Option<(u64, PageFaultError)>) -> ! {
    let vector = frame.vector as u8;
    kprintln!();
    error!("fatal exception in {} mode", if frame.from_user() { "user" } else { "kernel" });
    kprintln!("  vector {vector} -- {}", vector_name(vector));
    if let Some((address, err)) = page_fault {
        kprintln!("  faulting address {address:#018x} ({err})");
    }
    kprintln!("{frame}");
    panic!("unhandled CPU exception {vector} ({})", vector_name(vector));
}
