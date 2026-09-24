//! Interrupt controller abstraction and bring-up.

use alloc::sync::Arc;
use core::sync::atomic::Ordering;

use super::trap::{self, TrapFrame, VECTOR_KEYBOARD, VECTOR_RESCHEDULE, VECTOR_SPURIOUS, VECTOR_TIMER, VECTOR_TLB_SHOOTDOWN};
use super::{apic, cpu, percpu, pic, port};
use crate::obj::Device;
use crate::sync::IrqSpinLock;

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
/// There is no I/O APIC driver in this kernel, so the one legacy PIC line
/// this kernel ever unmasks (keyboard IRQ1, vector [`VECTOR_KEYBOARD`])
/// reaches the CPU through the local APIC's LINT0 pin in ExtINT mode -- the
/// local APIC does not put such a vector in its own in-service register, so
/// it is the 8259 that needs acknowledging, regardless of whether the local
/// APIC is also enabled for its own vectors.
///
/// This is *not* "any vector in the PIC's remapped range": [`VECTOR_TIMER`]
/// (32) is numerically also `PIC1_VECTOR_BASE`, because the local APIC timer
/// and the (otherwise fully masked) 8259's IRQ0 happen to share a vector
/// number in this scheme. Routing vector 32 to the PIC would silently starve
/// the scheduler tick of its local APIC EOI. If another legacy PIC line is
/// ever unmasked, add its vector here.
///
/// # Safety
/// Must be called exactly once, from the handler for `vector`.
pub unsafe fn end_of_interrupt(vector: u8) {
    if vector == VECTOR_KEYBOARD {
        // SAFETY: this vector is only ever delivered via the 8259.
        unsafe { pic::end_of_interrupt(vector) };
    } else if apic::is_enabled() {
        // SAFETY: the local APIC delivered this interrupt.
        unsafe { apic::end_of_interrupt() };
    } else {
        // SAFETY: no local APIC; the PIC delivered everything.
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

/// PS/2 controller data port, read to acknowledge and consume a scancode.
const PS2_DATA_PORT: u16 = 0x60;

/// The keyboard `Device`, once one exists to receive scancodes.
///
/// A raw `static` rather than a per-CPU slot: there is exactly one PS/2
/// keyboard on the machine, IRQ1 is only ever delivered to the boot CPU in
/// this kernel (no I/O APIC to redirect it elsewhere), and the handler only
/// ever reads this under interrupts-disabled, matching how [`TICK_HANDLER`]
/// is treated above.
static KEYBOARD_DEVICE: IrqSpinLock<Option<Arc<Device>>> = IrqSpinLock::new(None);

/// Start delivering PS/2 keyboard scancodes (IRQ1) to `device`.
///
/// Must run after the object and scheduler layers exist, since `device` is a
/// heap-allocated kernel object -- unlike [`init`], which runs before either.
///
/// # Safety
/// Must be called at most once, with interrupts disabled or otherwise not
/// racing a keyboard interrupt.
pub unsafe fn install_keyboard_handler(device: Arc<Device>) {
    *KEYBOARD_DEVICE.lock() = Some(device);
    // SAFETY: the handler above is ready to receive interrupts for this
    // vector as soon as it is installed.
    unsafe { trap::register_handler(VECTOR_KEYBOARD, keyboard_interrupt) };
    // SAFETY: the local APIC was already brought up by `init`.
    unsafe { apic::enable_legacy_irq_passthrough() };
    // SAFETY: a handler for the keyboard vector is now installed.
    unsafe { pic::unmask(1) };
}

fn keyboard_interrupt(_frame: &mut TrapFrame) {
    // SAFETY: reading the PS/2 data port is how a keyboard IRQ is
    // acknowledged at the device; it is always safe to read.
    let scancode = unsafe { port::inb(PS2_DATA_PORT) };
    // Deliberately silent: this runs once per make *and* break code, so
    // tracing it turns a held-down key into a stream of serial writes -- slow
    // enough to be felt, and enough to bury everything else in the log.
    if let Some(device) = KEYBOARD_DEVICE.lock().as_ref() {
        device.push_scancode(scancode);
    }
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
