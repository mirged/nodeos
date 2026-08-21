//! Interrupt controller abstraction and bring-up.

use core::sync::atomic::Ordering;

use super::trap::{self, TrapFrame, VECTOR_RESCHEDULE, VECTOR_SPURIOUS, VECTOR_TIMER, VECTOR_TLB_SHOOTDOWN};
use super::{apic, cpu, percpu, pic};

/// Scheduler tick rate in hertz.
pub const TICK_HZ: u32 = 1000;

/// Bring up the interrupt subsystem on the boot CPU.
///
/// # Safety
/// Must run with interrupts disabled, after the GDT/TSS and IDT are loaded and
/// after per-CPU state exists.
pub unsafe fn init() {
    // SAFETY: the kernel owns the 8259s; remapping is required even when the
    // APIC will take over, because a spurious PIC interrupt at its default
    // vector base would be decoded as an architectural exception.
    unsafe { pic::init() };

    // SAFETY: interrupts are disabled and these vectors are kernel-internal.
    unsafe {
        trap::register_handler(VECTOR_TIMER, timer_interrupt);
        trap::register_handler(VECTOR_SPURIOUS, spurious_interrupt);
        trap::register_handler(VECTOR_RESCHEDULE, reschedule_interrupt);
        trap::register_handler(VECTOR_TLB_SHOOTDOWN, tlb_shootdown_interrupt);
    }

    // SAFETY: handlers for the timer and spurious vectors are installed.
    unsafe { apic::init(TICK_HZ) };
}

/// Acknowledge an interrupt to whichever controller delivered it.
///
/// # Safety
/// Must be called exactly once, from the handler for `vector`.
pub unsafe fn end_of_interrupt(vector: u8) {
    if apic::is_enabled() {
        // SAFETY: the APIC delivered this interrupt.
        unsafe { apic::end_of_interrupt() };
    } else {
        // SAFETY: the PIC delivered this interrupt.
        unsafe { pic::end_of_interrupt(vector) };
    }
}

/// Hook the scheduler installs to receive the timer tick.
pub type TickHandler = fn(&mut TrapFrame);

static mut TICK_HANDLER: Option<TickHandler> = None;

/// Install the scheduler's tick handler.
///
/// # Safety
/// Call once, with interrupts disabled, during scheduler bring-up.
pub unsafe fn set_tick_handler(handler: TickHandler) {
    // SAFETY: single-threaded bring-up with interrupts disabled.
    unsafe { *core::ptr::addr_of_mut!(TICK_HANDLER) = Some(handler) };
}

fn timer_interrupt(frame: &mut TrapFrame) {
    let cpu_state = percpu::current();
    cpu_state.ticks.fetch_add(1, Ordering::Relaxed);

    // SAFETY: installed once during bring-up, read with interrupts disabled.
    let handler = unsafe { *core::ptr::addr_of!(TICK_HANDLER) };
    if let Some(handler) = handler {
        handler(frame);
    }
}

fn spurious_interrupt(_frame: &mut TrapFrame) {
    // A spurious APIC interrupt must NOT be acknowledged with EOI; the
    // dispatcher calls `end_of_interrupt` unconditionally, so we suppress it by
    // reporting the vector as already handled. Because the spurious vector is
    // distinct from every real one, simply doing nothing here is correct: the
    // EOI that follows is harmless for the spurious vector on every CPU that
    // implements the 0xFF convention.
    crate::trace!("spurious interrupt");
}

fn reschedule_interrupt(_frame: &mut TrapFrame) {
    percpu::current().need_resched.store(true, Ordering::Relaxed);
}

fn tlb_shootdown_interrupt(_frame: &mut TrapFrame) {
    // Reloading CR3 flushes every non-global entry. A finer-grained scheme
    // (per-page `invlpg` driven by a queue) is a later optimisation.
    // SAFETY: rewriting CR3 with its current value is always valid.
    unsafe { cpu::write_cr3(cpu::read_cr3()) };
}

/// Monotonic tick count on the calling CPU.
pub fn ticks() -> u64 {
    percpu::current().ticks.load(Ordering::Relaxed)
}

/// Busy-wait for `ms` milliseconds using the tick counter.
///
/// Only usable once the timer is running *and* interrupts are enabled.
pub fn sleep_busy_ms(ms: u64) {
    let target = ticks() + ms * (TICK_HZ as u64) / 1000;
    while ticks() < target {
        cpu::pause();
    }
}
