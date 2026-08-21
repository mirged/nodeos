//! Intel 8253/8254 programmable interval timer.
//!
//! The PIT is used for exactly one thing in NodeOS: calibrating the local APIC
//! timer and the TSC at boot. Channel 2 is used because it is gated by a
//! software-controlled bit and raises no interrupt, so calibration works before
//! the interrupt controller is configured.

use super::port;

const CHANNEL2_DATA: u16 = 0x42;
const COMMAND: u16 = 0x43;
/// Port 0x61: bit 0 gates PIT channel 2, bit 5 mirrors its output.
const SPEAKER_CONTROL: u16 = 0x61;

/// The PIT's fixed input frequency, 1.193182 MHz.
pub const PIT_FREQUENCY: u32 = 1_193_182;

/// Run `f` while PIT channel 2 counts down `ticks` input cycles, and return
/// what `f` returned.
///
/// # Safety
/// Requires ownership of ports 0x42, 0x43 and 0x61, and must run with
/// interrupts disabled so the measured window is not stretched by a handler.
pub unsafe fn calibrate_window<T>(ticks: u16, f: impl FnOnce() -> T) -> T {
    assert!(ticks > 0, "calibration window must be non-zero");
    // SAFETY: the caller owns the PIT and speaker-gate ports.
    unsafe {
        // Disconnect the speaker (bit 1) but enable the channel-2 gate (bit 0).
        let control = (port::inb(SPEAKER_CONTROL) & 0xFC) | 0x01;
        port::outb(SPEAKER_CONTROL, control);

        // Channel 2, access lo/hi, mode 0 (interrupt on terminal count), binary.
        port::outb(COMMAND, 0b1011_0000);
        port::outb(CHANNEL2_DATA, (ticks & 0xFF) as u8);
        port::outb(CHANNEL2_DATA, (ticks >> 8) as u8);

        // Re-arm: toggling the gate low then high restarts the count so the
        // window starts exactly here.
        port::outb(SPEAKER_CONTROL, control & !0x01);
        port::outb(SPEAKER_CONTROL, control);

        let result = f();

        // Wait for the output pin (bit 5 of port 0x61) to go high.
        let mut guard = 0u64;
        while port::inb(SPEAKER_CONTROL) & 0x20 == 0 {
            guard += 1;
            if guard > 100_000_000 {
                break;
            }
            core::hint::spin_loop();
        }

        // Close the gate again.
        port::outb(SPEAKER_CONTROL, control & !0x01);

        result
    }
}

/// Busy-wait for approximately `micros` microseconds using channel 2.
///
/// # Safety
/// See [`calibrate_window`].
pub unsafe fn delay_us(micros: u32) {
    let ticks = ((PIT_FREQUENCY as u64 * micros as u64) / 1_000_000).min(0xFFFF) as u16;
    if ticks == 0 {
        return;
    }
    // SAFETY: forwarded contract.
    unsafe { calibrate_window(ticks, || {}) };
}
