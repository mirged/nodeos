//! Local APIC (xAPIC) support: timer, EOI and inter-processor interrupts.
//!
//! Registers are reached through the direct physical map rather than a
//! dedicated mapping, so this module works before the virtual memory manager
//! exists. All accesses are 32-bit and volatile, as the architecture requires.

use core::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering};

use super::cpu;
use super::trap::{VECTOR_RESCHEDULE, VECTOR_SPURIOUS, VECTOR_TIMER, VECTOR_TLB_SHOOTDOWN};
use super::{pit, PHYSMAP_BASE};

const REG_ID: usize = 0x020;
const REG_VERSION: usize = 0x030;
const REG_TPR: usize = 0x080;
const REG_EOI: usize = 0x0B0;
const REG_SPURIOUS: usize = 0x0F0;
const REG_ICR_LOW: usize = 0x300;
const REG_ICR_HIGH: usize = 0x310;
const REG_LVT_TIMER: usize = 0x320;
const REG_LVT_LINT0: usize = 0x350;
const REG_LVT_LINT1: usize = 0x360;
const REG_LVT_ERROR: usize = 0x370;
const REG_TIMER_INIT: usize = 0x380;
const REG_TIMER_CURRENT: usize = 0x390;
const REG_TIMER_DIVIDE: usize = 0x3E0;

const SPURIOUS_ENABLE: u32 = 1 << 8;
const LVT_MASKED: u32 = 1 << 16;
const LVT_TIMER_PERIODIC: u32 = 1 << 17;

static LAPIC_BASE: AtomicU64 = AtomicU64::new(0);
static ENABLED: AtomicBool = AtomicBool::new(false);
/// Local APIC timer ticks per second, measured against the PIT.
static TIMER_HZ: AtomicU64 = AtomicU64::new(0);
/// Configured scheduler tick rate.
static TICK_HZ: AtomicU32 = AtomicU32::new(0);

/// Whether the local APIC has been brought up on this machine.
pub fn is_enabled() -> bool {
    ENABLED.load(Ordering::Acquire)
}

fn base() -> *mut u32 {
    LAPIC_BASE.load(Ordering::Acquire) as *mut u32
}

/// Read a local APIC register.
///
/// # Safety
/// The APIC must be mapped, and `offset` must be a valid, readable register.
unsafe fn read(offset: usize) -> u32 {
    // SAFETY: the caller guarantees the APIC window is mapped; APIC registers
    // are 32-bit and must be accessed volatilely.
    unsafe { core::ptr::read_volatile(base().byte_add(offset)) }
}

/// Write a local APIC register.
///
/// # Safety
/// The APIC must be mapped, `offset` must be a writable register, and `value`
/// must be architecturally valid for it.
unsafe fn write(offset: usize, value: u32) {
    // SAFETY: as above.
    unsafe { core::ptr::write_volatile(base().byte_add(offset), value) }
}

/// Whether this CPU reports a local APIC in CPUID.
pub fn supported() -> bool {
    let (_, _, _, edx) = cpu::cpuid(1, 0);
    edx & (1 << 9) != 0
}

/// Enable the local APIC on the calling CPU and calibrate its timer.
///
/// # Safety
/// Must run with interrupts disabled, after the IDT is loaded, and after the
/// direct physical map covers the APIC's physical base.
pub unsafe fn init(tick_hz: u32) {
    if !supported() {
        crate::warn!("no local APIC reported by CPUID; falling back to the PIT");
        return;
    }

    // SAFETY: IA32_APIC_BASE exists whenever CPUID reports an APIC.
    let apic_base_msr = unsafe { cpu::rdmsr(cpu::MSR_APIC_BASE) };
    let phys = apic_base_msr & 0xF_FFFF_F000;
    LAPIC_BASE.store(PHYSMAP_BASE + phys, Ordering::Release);

    // Set the global enable bit (bit 11) if firmware left it clear.
    // SAFETY: re-writing IA32_APIC_BASE with the enable bit set is the
    // architecturally defined way to turn the APIC on.
    unsafe { cpu::wrmsr(cpu::MSR_APIC_BASE, apic_base_msr | (1 << 11)) };

    // SAFETY: the APIC window is now mapped through the direct physical map.
    unsafe {
        // Accept every priority class.
        write(REG_TPR, 0);
        // Enable the APIC and route spurious interrupts to a known vector.
        write(REG_SPURIOUS, SPURIOUS_ENABLE | VECTOR_SPURIOUS as u32);
        // Local interrupt pins: LINT0/LINT1 are firmware's business, mask them.
        write(REG_LVT_LINT0, LVT_MASKED);
        write(REG_LVT_LINT1, LVT_MASKED);
        write(REG_LVT_ERROR, LVT_MASKED);
        write(REG_LVT_TIMER, LVT_MASKED);
    }

    ENABLED.store(true, Ordering::Release);
    // SAFETY: the APIC is enabled and the PIT is ours to use.
    unsafe { calibrate_timer() };
    // SAFETY: calibration produced a usable frequency.
    unsafe { start_timer(tick_hz) };
}

/// The local APIC ID of the calling CPU.
pub fn id() -> u32 {
    if !is_enabled() {
        return 0;
    }
    // SAFETY: the APIC is enabled, so its window is mapped.
    unsafe { read(REG_ID) >> 24 }
}

/// The local APIC version register.
pub fn version() -> u32 {
    if !is_enabled() {
        return 0;
    }
    // SAFETY: the APIC is enabled.
    unsafe { read(REG_VERSION) & 0xFF }
}

/// Measured local APIC timer frequency, in hertz.
pub fn timer_hz() -> u64 {
    TIMER_HZ.load(Ordering::Relaxed)
}

/// Configured scheduler tick rate, in hertz.
pub fn tick_hz() -> u32 {
    TICK_HZ.load(Ordering::Relaxed)
}

/// Measure the APIC timer against the PIT.
///
/// # Safety
/// The APIC must be enabled and the PIT must be owned by the kernel.
unsafe fn calibrate_timer() {
    // Divide by 16 keeps the 32-bit counter from wrapping over a 10 ms window
    // even on a very fast bus.
    const DIVIDE_BY_16: u32 = 0b0011;
    // 10 ms of PIT input clock.
    let window_ticks = (pit::PIT_FREQUENCY / 100) as u16;

    // SAFETY: the APIC is enabled and we own the PIT.
    let (start, end) = unsafe {
        write(REG_TIMER_DIVIDE, DIVIDE_BY_16);
        write(REG_LVT_TIMER, LVT_MASKED);
        write(REG_TIMER_INIT, u32::MAX);

        let start = read(REG_TIMER_CURRENT);
        pit::calibrate_window(window_ticks, || {});
        let end = read(REG_TIMER_CURRENT);

        write(REG_TIMER_INIT, 0);
        (start, end)
    };

    let elapsed = start.saturating_sub(end) as u64;
    // Scale back up: divider 16, window 1/100 s.
    let hz = elapsed * 16 * 100;
    TIMER_HZ.store(hz, Ordering::Relaxed);
    crate::info!("local APIC timer: {} MHz bus clock (id {}, version {:#x})", hz / 1_000_000, id(), version());
}

/// Start the periodic scheduler tick.
///
/// # Safety
/// [`calibrate_timer`] must have run, and a handler must be installed for
/// [`VECTOR_TIMER`].
unsafe fn start_timer(tick_hz: u32) {
    const DIVIDE_BY_16: u32 = 0b0011;
    let hz = TIMER_HZ.load(Ordering::Relaxed);
    if hz == 0 || tick_hz == 0 {
        crate::warn!("APIC timer calibration failed; scheduler tick unavailable");
        return;
    }
    TICK_HZ.store(tick_hz, Ordering::Relaxed);
    // The counter runs at hz/16 after the divider.
    let count = ((hz / 16) / tick_hz as u64).max(1) as u32;
    // SAFETY: the APIC is enabled and the vector has a handler.
    unsafe {
        write(REG_TIMER_DIVIDE, DIVIDE_BY_16);
        write(REG_LVT_TIMER, LVT_TIMER_PERIODIC | VECTOR_TIMER as u32);
        write(REG_TIMER_INIT, count);
    }
}

/// Delivery mode bits (10:8) for ExtINT: "deliver as if the 8259 asserted
/// this pin directly", which is how a legacy PIC's IRQs reach the CPU on a
/// machine with no I/O APIC in use.
const LVT_DELIVERY_EXTINT: u32 = 0b111 << 8;

/// Let the legacy 8259 PIC's interrupts reach the CPU through the local
/// APIC's LINT0 pin.
///
/// `init` masks LINT0 unconditionally, on the assumption that local
/// interrupt pins are "firmware's business". That is true for LINT1 (NMI) but
/// not for LINT0 on a machine with no I/O APIC driver: LINT0 is the only path
/// a PIC-sourced IRQ (here, PS/2 IRQ1) has to the CPU at all. Call this only
/// once a handler is installed for the vector the PIC will assert and the
/// corresponding PIC line is about to be unmasked.
///
/// # Safety
/// The APIC must already be enabled ([`init`] must have run).
pub unsafe fn enable_legacy_irq_passthrough() {
    if !is_enabled() {
        return;
    }
    // SAFETY: the APIC window is mapped once `is_enabled()` is true.
    unsafe { write(REG_LVT_LINT0, LVT_DELIVERY_EXTINT) };
}

/// Acknowledge the interrupt currently in service.
///
/// # Safety
/// Must be called exactly once per APIC-delivered interrupt, from its handler.
pub unsafe fn end_of_interrupt() {
    if is_enabled() {
        // SAFETY: writing 0 to the EOI register is the defined acknowledgement.
        unsafe { write(REG_EOI, 0) };
    }
}

/// Destination shorthand for an inter-processor interrupt.
#[derive(Clone, Copy)]
pub enum IpiTarget {
    /// A single CPU, by local APIC ID.
    Cpu(u32),
    /// Every CPU including the sender.
    All,
    /// Every CPU except the sender.
    Others,
}

/// Send an inter-processor interrupt.
///
/// # Safety
/// `vector` must have a handler installed on every target CPU.
pub unsafe fn send_ipi(target: IpiTarget, vector: u8) {
    if !is_enabled() {
        return;
    }
    let (dest_field, shorthand) = match target {
        IpiTarget::Cpu(id) => (id << 24, 0b00u32),
        IpiTarget::All => (0, 0b10),
        IpiTarget::Others => (0, 0b11),
    };
    // Fixed delivery mode, physical destination, edge triggered, assert.
    let low = vector as u32 | (shorthand << 18) | (1 << 14);
    // SAFETY: ICR_HIGH must be written before ICR_LOW, which is what actually
    // fires the IPI.
    unsafe {
        write(REG_ICR_HIGH, dest_field);
        write(REG_ICR_LOW, low);
    }
}

/// Nudge another CPU to re-run the scheduler.
///
/// # Safety
/// The target CPU must have the reschedule vector installed.
pub unsafe fn send_reschedule(lapic_id: u32) {
    // SAFETY: forwarded contract.
    unsafe { send_ipi(IpiTarget::Cpu(lapic_id), VECTOR_RESCHEDULE) };
}

/// Ask every other CPU to flush its TLB.
///
/// # Safety
/// Every CPU must have the shootdown vector installed.
pub unsafe fn broadcast_tlb_shootdown() {
    // SAFETY: forwarded contract.
    unsafe { send_ipi(IpiTarget::Others, VECTOR_TLB_SHOOTDOWN) };
}
