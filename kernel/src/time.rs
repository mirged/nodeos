//! Monotonic time.
//!
//! NodeOS exposes exactly one clock to userspace: a monotonic nanosecond
//! counter since boot, readable through the `clock` syscall. Wall-clock time,
//! time zones and calendars are a userspace service's problem -- the kernel has
//! no business knowing what a leap second is.
//!
//! The counter is the TSC, calibrated against the PIT at boot. On every CPU
//! NodeOS targets the TSC is invariant (constant rate, unaffected by power
//! states); when CPUID says otherwise the timer-tick counter is used instead,
//! at millisecond resolution.

use core::sync::atomic::{AtomicBool, AtomicU64, Ordering};

use crate::arch::x86_64::{cpu, interrupt, percpu, pit};

/// Nanoseconds in a second.
pub const NANOS_PER_SEC: u64 = 1_000_000_000;
/// Nanoseconds in a millisecond.
pub const NANOS_PER_MILLI: u64 = 1_000_000;
/// Nanoseconds in a microsecond.
pub const NANOS_PER_MICRO: u64 = 1_000;

static TSC_HZ: AtomicU64 = AtomicU64::new(0);
static TSC_AT_BOOT: AtomicU64 = AtomicU64::new(0);
static TSC_INVARIANT: AtomicBool = AtomicBool::new(false);

/// Whether the CPU advertises an invariant TSC.
pub fn tsc_is_invariant() -> bool {
    if cpu::cpuid_max_extended_leaf() < 0x8000_0007 {
        return false;
    }
    let (_, _, _, edx) = cpu::cpuid(0x8000_0007, 0);
    edx & (1 << 8) != 0
}

/// Calibrate the TSC against the PIT.
///
/// # Safety
/// Must run with interrupts disabled, on the boot CPU, while the kernel still
/// owns the PIT.
pub unsafe fn init() {
    let invariant = tsc_is_invariant();
    TSC_INVARIANT.store(invariant, Ordering::Relaxed);

    // Measure over 10 ms of PIT input clock.
    let window = (pit::PIT_FREQUENCY / 100) as u16;
    // SAFETY: the caller guarantees exclusive use of the PIT with interrupts
    // disabled, so the measured window is not stretched by a handler.
    let (start, end) = unsafe {
        let start = cpu::rdtsc();
        pit::calibrate_window(window, || {});
        (start, cpu::rdtsc())
    };
    let hz = (end - start) * 100;
    TSC_HZ.store(hz, Ordering::Release);
    TSC_AT_BOOT.store(end, Ordering::Release);

    crate::info!(
        "clock: TSC {}.{:03} GHz{}",
        hz / 1_000_000_000,
        (hz % 1_000_000_000) / 1_000_000,
        if invariant { " (invariant)" } else { " (variable -- falling back to timer ticks)" }
    );
}

/// Nanoseconds since [`init`].
pub fn now_ns() -> u64 {
    let hz = TSC_HZ.load(Ordering::Acquire);
    if hz == 0 {
        return 0;
    }
    if !TSC_INVARIANT.load(Ordering::Relaxed) {
        // Fall back to the scheduler tick, which is driven by the APIC timer.
        if percpu::is_ready() {
            return interrupt::ticks() * (NANOS_PER_SEC / interrupt::TICK_HZ as u64);
        }
        return 0;
    }
    let now = cpu::rdtsc();
    let base = TSC_AT_BOOT.load(Ordering::Acquire);
    let elapsed = now.saturating_sub(base);
    // Split the multiply to avoid overflowing at ~5 seconds on a 4 GHz part.
    let seconds = elapsed / hz;
    let remainder = elapsed % hz;
    seconds * NANOS_PER_SEC + (remainder * NANOS_PER_SEC) / hz
}

/// Microseconds since [`init`].
pub fn now_us() -> u64 {
    now_ns() / NANOS_PER_MICRO
}

/// Milliseconds since [`init`].
pub fn now_ms() -> u64 {
    now_ns() / NANOS_PER_MILLI
}

/// Measured TSC frequency in hertz, or zero before calibration.
pub fn tsc_hz() -> u64 {
    TSC_HZ.load(Ordering::Relaxed)
}

/// Busy-wait for `ns` nanoseconds. Only for bring-up paths that cannot block.
pub fn spin_ns(ns: u64) {
    let deadline = now_ns() + ns;
    while now_ns() < deadline {
        cpu::pause();
    }
}
