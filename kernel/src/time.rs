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

use alloc::sync::{Arc, Weak};
use alloc::vec::Vec;
use core::sync::atomic::{AtomicBool, AtomicU64, Ordering};

use crate::arch::x86_64::{cpu, interrupt, percpu, pit};
use crate::sync::IrqSpinLock;
use crate::task::thread::Thread;

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

// ---------------------------------------------------------------------------
// Sleeping until a deadline
// ---------------------------------------------------------------------------

/// One thread waiting for the clock to reach `deadline_ns`.
///
/// The reference is weak for the same reason a wait queue's is: a thread that
/// is destroyed while sleeping must not be kept alive by the fact that it was.
struct Sleeper {
    deadline_ns: u64,
    thread: Weak<Thread>,
}

/// Everyone waiting on the clock.
///
/// A vector scanned once per tick rather than a heap or a timer wheel. At a
/// thousand ticks a second and a handful of sleepers -- which is what a NodeOS
/// boot has, one per service that polls -- the scan is a few comparisons, and
/// the alternative is a data structure whose correctness under an interrupt
/// would need arguing.
static SLEEPERS: IrqSpinLock<Vec<Sleeper>> = IrqSpinLock::new(Vec::new());

/// Register `thread` to be woken once the clock passes `deadline_ns`.
///
/// The caller blocks itself afterwards, exactly as it would after parking on a
/// wait queue -- and it may park on both, which is what a wait with a timeout
/// is: two things that can end it, whichever comes first.
pub fn sleep_until(thread: &Arc<Thread>, deadline_ns: u64) {
    let mut sleepers = SLEEPERS.lock();
    if let Some(existing) = sleepers.iter_mut().find(|s| s.thread.as_ptr() == Arc::as_ptr(thread)) {
        existing.deadline_ns = deadline_ns;
        return;
    }
    sleepers.push(Sleeper { deadline_ns, thread: Arc::downgrade(thread) });
}

/// Cancel `thread`'s registration, whether or not it has expired.
pub fn cancel_sleep(thread: &Arc<Thread>) {
    SLEEPERS.lock().retain(|s| s.thread.as_ptr() != Arc::as_ptr(thread));
}

/// Wake every thread whose deadline has passed. Called from the timer tick.
///
/// This is what makes a timed wait a *sleep*. Before it existed, a wait with a
/// deadline spun at scheduler granularity -- yield, re-check, yield -- and a
/// service polling a device on a five-millisecond period would take the whole
/// machine with it: the spinning thread was runnable at every scheduling
/// decision, so nothing else with work to do ever got picked. A sound card
/// being fed on time is not a reason for the rest of userspace to stop.
pub fn wake_expired() {
    let now = now_ns();
    let expired: Vec<Arc<Thread>> = {
        let mut sleepers = SLEEPERS.lock();
        let mut expired = Vec::new();
        sleepers.retain(|sleeper| {
            if sleeper.deadline_ns > now {
                return true;
            }
            if let Some(thread) = sleeper.thread.upgrade() {
                expired.push(thread);
            }
            false
        });
        expired
    };
    // Woken outside the lock: `wake` takes the run queue's, and holding both
    // in one order here and the other order anywhere else is how a kernel
    // deadlocks under interrupt.
    for thread in expired {
        crate::sched::wake(&thread);
    }
}
