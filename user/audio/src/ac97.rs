//! The AC'97 driver, in ring 3.
//!
//! Everything here happens through two capabilities and no privilege: a
//! `Device` handle whose port windows are the codec's mixer and the
//! controller's bus-master engine, and a DMA buffer the kernel allocated
//! contiguously and told us the physical address of. There is no `in`
//! instruction in this file and no way to name a port the device does not
//! decode -- see `kernel/src/obj/device.rs` for the check that makes that
//! true.
//!
//! # How an AC'97 plays sound
//!
//! The controller walks a *buffer descriptor list*: 32 entries, each naming a
//! physical address and a length, in a ring. It plays the entry at `CIV` and
//! stops when it reaches `LVI` -- the last index the driver has declared
//! valid. So playing continuously is a treadmill: the driver refills buffers
//! the engine has already passed and keeps moving `LVI` ahead of `CIV`, and
//! the moment it stops doing that the engine runs out of valid descriptors and
//! halts, which the status register reports as `DCH`.
//!
//! ```text
//!   BDL[0] -> buffer 0 ┐            CIV ──► playing now
//!   BDL[1] -> buffer 1 │  filled ahead
//!    ...               │
//!   BDL[k] -> buffer k ┘            LVI ──► engine halts after this one
//! ```
//!
//! # Why it polls
//!
//! The card raises an interrupt on each buffer completion and NodeOS has no
//! bindable interrupt object, so this driver reads `CIV` instead. That is a
//! real limitation and it costs a wakeup every few milliseconds -- but it also
//! means the driver needs no authority over an interrupt line, and the mixing
//! it does per tick has to happen on a timer anyway.

use libnode::device::{Device, Dma};
use libnode::Error;

/// Mixer registers live in port window 0.
const NAM: u32 = 0;
/// Bus-master registers live in port window 1.
const NABM: u32 = 1;

// --- Mixer (NAM) registers -------------------------------------------------
/// Writing anything here resets the codec to its defaults.
const NAM_RESET: u16 = 0x00;
/// Master output attenuation.
const NAM_MASTER_VOLUME: u16 = 0x02;
/// PCM playback attenuation.
const NAM_PCM_VOLUME: u16 = 0x18;

// --- Bus master (NABM) registers -------------------------------------------
/// Base of the PCM-out register box.
const PCM_OUT: u16 = 0x10;
/// Physical address of the buffer descriptor list.
const BOX_BDBAR: u16 = 0x00;
/// Current index value: the descriptor being played.
const BOX_CIV: u16 = 0x04;
/// Last valid index: the descriptor after which the engine halts.
const BOX_LVI: u16 = 0x05;
/// Status.
const BOX_STATUS: u16 = 0x06;
/// Control.
const BOX_CONTROL: u16 = 0x0B;

/// Status bit: the DMA engine has halted.
const STATUS_DMA_HALTED: u16 = 1 << 0;
/// Control bit: run.
const CONTROL_RUN: u16 = 1 << 0;
/// Control bit: reset this box's registers.
const CONTROL_RESET: u16 = 1 << 1;

/// Global control register.
const GLOBAL_CONTROL: u16 = 0x2C;
/// Global status register.
const GLOBAL_STATUS: u16 = 0x30;
/// Global control bit: release the AC-link cold reset.
const GLOBAL_COLD_RESET: u32 = 1 << 1;
/// Global status bit: the primary codec is ready to be programmed.
const GLOBAL_PRIMARY_READY: u32 = 1 << 8;

/// Descriptors in the ring.
///
/// Thirty-two, because that is not a choice: the controller's index registers
/// are five bits and it wraps at 32 whatever the driver fills. A ring built
/// with sixteen entries and sixteen buffers looks right and is not -- the
/// engine walks into descriptors 16..31, which are whatever the DMA buffer was
/// zeroed to, and plays them. The symptom is a `CIV` of 16 with a driver that
/// masks it into range and concludes that nothing has moved.
pub const BUFFERS: usize = 32;
/// Stereo frames per descriptor.
///
/// This number is the whole latency story. A frame is 4 bytes at 48 kHz, so
/// 512 of them is 10.7 ms: the deadline this driver has to be scheduled within
/// to avoid a gap, and the granularity at which a newly started sound can
/// begin.
pub const FRAMES_PER_BUFFER: usize = 512;
/// Bytes in one descriptor's buffer: stereo, 16-bit.
pub const BYTES_PER_BUFFER: usize = FRAMES_PER_BUFFER * 4;

/// How many descriptors ahead of the play position to keep filled.
///
/// The trade this constant makes is worth being explicit about. Filling the
/// whole ring would make an underrun almost impossible and put 170 ms between
/// a client's `AUDIO_PLAY` and the first sample leaving the card, which for a
/// game is the difference between a gunshot and an echo. Filling four keeps
/// that under 45 ms at the cost of needing to be scheduled four times as
/// often. A missed deadline is a click; a full ring would be a delay on every
/// sound, which is worse and permanent.
pub const LOOKAHEAD: usize = 4;

/// The AC-link runs at a fixed 48 kHz unless variable-rate audio is
/// negotiated, which this driver does not: resampling in the mixer is a
/// multiply per sample and needs no codec feature to be present.
pub const SAMPLE_RATE: u32 = 48_000;

/// A descriptor as the hardware reads it: eight bytes, little-endian.
#[repr(C)]
#[derive(Clone, Copy)]
struct Descriptor {
    /// Physical address of the samples.
    address: u32,
    /// Length in *samples* -- 16-bit units, so twice the frame count.
    samples: u16,
    /// Bit 15 raises an interrupt on completion, bit 14 stops on completion.
    flags: u16,
}

/// A claimed AC'97 controller with its ring set up.
pub struct Ac97 {
    device: Device,
    dma: Dma,
    /// Offset of the descriptor list within the DMA buffer.
    bdl_offset: u64,
    /// Offset of buffer `i` within the DMA buffer.
    buffer_offset: [u64; BUFFERS],
    /// Next descriptor to refill.
    fill: usize,
    /// Descriptors filled since the engine started, for diagnostics.
    filled: u64,
    /// Times the engine was found halted and restarted -- an audible click
    /// each, and the number worth watching if the sound stutters.
    underruns: u64,
}

impl Ac97 {
    /// Reset the codec, build the ring, and leave the engine stopped.
    pub fn new(device: Device) -> Result<Self, Error> {
        // The descriptor list first, then the audio buffers, in one allocation:
        // the hardware needs both to be physically addressed and there is no
        // reason to make the kernel find two contiguous blocks.
        let bdl_bytes = (BUFFERS * core::mem::size_of::<Descriptor>()) as u64;
        let total = bdl_bytes + (BUFFERS * BYTES_PER_BUFFER) as u64;
        let dma = device.dma_alloc(total)?;

        let mut buffer_offset = [0u64; BUFFERS];
        for (index, offset) in buffer_offset.iter_mut().enumerate() {
            *offset = bdl_bytes + (index * BYTES_PER_BUFFER) as u64;
        }

        let mut card = Self {
            device,
            dma,
            bdl_offset: 0,
            buffer_offset,
            fill: 0,
            filled: 0,
            underruns: 0,
        };
        card.reset_codec()?;
        card.write_descriptors()?;
        Ok(card)
    }

    /// Bring the AC-link and the codec up, and open the output at full volume.
    fn reset_codec(&mut self) -> Result<(), Error> {
        // Release the cold reset and wait for the codec to report itself
        // ready. QEMU answers immediately; real silicon takes a moment, and a
        // driver that programmed the mixer before this bit came up would be
        // writing to a codec that is not listening.
        let control = self.device.read32(NABM, GLOBAL_CONTROL)?;
        self.device.write32(NABM, GLOBAL_CONTROL, control | GLOBAL_COLD_RESET)?;
        for _ in 0..1000 {
            if self.device.read32(NABM, GLOBAL_STATUS)? & GLOBAL_PRIMARY_READY != 0 {
                break;
            }
            libnode::yield_now();
        }

        self.device.write16(NAM, NAM_RESET, 0)?;
        // Volume registers are *attenuation*: zero is loudest. The mixer does
        // its own scaling, so the hardware is left wide open and every volume
        // decision stays in one place.
        self.device.write16(NAM, NAM_MASTER_VOLUME, 0x0000)?;
        self.device.write16(NAM, NAM_PCM_VOLUME, 0x0000)?;

        // Reset the PCM-out box. The reset bit clears itself when the engine
        // has finished, and doing this before programming BDBAR is what makes
        // the ring's initial state defined rather than whatever the firmware
        // left.
        self.device.write8(NABM, PCM_OUT + BOX_CONTROL, CONTROL_RESET as u8)?;
        for _ in 0..1000 {
            if self.device.read8(NABM, PCM_OUT + BOX_CONTROL)? & CONTROL_RESET as u8 == 0 {
                break;
            }
            libnode::yield_now();
        }
        Ok(())
    }

    /// Publish the descriptor list to the controller.
    fn write_descriptors(&mut self) -> Result<(), Error> {
        for index in 0..BUFFERS {
            let descriptor = Descriptor {
                address: (self.dma.phys() + self.buffer_offset[index]) as u32,
                // Length is in 16-bit samples, and the buffer is stereo, so a
                // frame is two of them. Getting this wrong halves or doubles
                // the playback rate, which is the classic AC'97 first bug.
                samples: (FRAMES_PER_BUFFER * 2) as u16,
                flags: 0,
            };
            // SAFETY: `bdl_offset + index * 8` is inside the DMA buffer, whose
            // first `BUFFERS * 8` bytes are the descriptor list, and the
            // engine is not running yet.
            unsafe {
                let slot: *mut Descriptor =
                    self.dma.at(self.bdl_offset + (index * core::mem::size_of::<Descriptor>()) as u64);
                slot.write_volatile(descriptor);
            }
        }
        self.device.write32(NABM, PCM_OUT + BOX_BDBAR, (self.dma.phys() + self.bdl_offset) as u32)
    }

    /// The samples of descriptor `index`, as frames of `(left, right)`.
    ///
    /// # Safety
    /// The caller must not hand out two of these at once, and must not write a
    /// buffer the engine is currently playing -- [`Ac97::playing_index`] says
    /// which that is.
    unsafe fn buffer(&mut self, index: usize) -> &mut [i16] {
        // SAFETY: forwarded contract; the offset and length are inside the DMA
        // allocation by construction.
        unsafe {
            let base: *mut i16 = self.dma.at(self.buffer_offset[index]);
            core::slice::from_raw_parts_mut(base, FRAMES_PER_BUFFER * 2)
        }
    }

    /// The descriptor the engine is playing right now.
    pub fn playing_index(&self) -> usize {
        self.device.read8(NABM, PCM_OUT + BOX_CIV).unwrap_or(0) as usize % BUFFERS
    }

    /// Milliseconds of audio the whole ring holds, for the record.
    pub const fn ring_ms() -> u32 {
        (BUFFERS * FRAMES_PER_BUFFER * 1000) as u32 / SAMPLE_RATE
    }

    /// Whether the engine has run out of valid descriptors and stopped.
    pub fn halted(&self) -> bool {
        self.device.read16(NABM, PCM_OUT + BOX_STATUS).unwrap_or(0) & STATUS_DMA_HALTED != 0
    }

    /// Fill descriptors that the engine has already passed, up to
    /// [`LOOKAHEAD`] ahead of it, and declare them valid.
    ///
    /// `render` is called with one buffer's worth of interleaved stereo frames
    /// to fill; it is the mixer, and it is called at most `LOOKAHEAD` times
    /// per tick so that one slow tick cannot turn into an unbounded amount of
    /// mixing.
    pub fn refill(&mut self, mut render: impl FnMut(&mut [i16])) -> Result<usize, Error> {
        let playing = self.playing_index();
        // A halted engine has *nothing* queued, whatever the indices suggest:
        // it has run past its last valid descriptor and stopped. Resuming it
        // continues at the descriptor after the one it stopped on, so that is
        // where the next buffer has to go.
        //
        // Getting this wrong is not a glitch, it is silence: the ring was
        // being treated as four-deep because `fill` was four ahead of a `CIV`
        // that had stopped moving, so nothing was ever written, so `LVI` was
        // never updated, so the engine was never told there was more -- a
        // stall that holds itself in place.
        let mut queued = if self.halted() {
            self.fill = (playing + 1) % BUFFERS;
            0
        } else {
            (self.fill + BUFFERS - playing) % BUFFERS
        };

        let mut written = 0;
        while queued < LOOKAHEAD {
            let index = self.fill;
            // SAFETY: `index` is a descriptor the engine is not reading -- it
            // is ahead of `CIV`, or the engine is halted -- and only one
            // borrow is live at a time.
            let buffer = unsafe { self.buffer(index) };
            render(buffer);
            self.fill = (self.fill + 1) % BUFFERS;
            self.filled += 1;
            written += 1;
            queued += 1;
        }

        if written > 0 {
            // The last descriptor written is the last valid one. Writing this
            // register is also what wakes a halted engine: the controller
            // treats it as "there is more", clears its halt, and picks up at
            // the next descriptor.
            let last = (self.fill + BUFFERS - 1) % BUFFERS;
            self.device.write8(NABM, PCM_OUT + BOX_LVI, last as u8)?;
        }
        Ok(written)
    }

    /// Start the engine, or restart it after an underrun.
    pub fn start(&mut self) -> Result<(), Error> {
        if self.halted() && self.filled > 0 {
            self.underruns += 1;
        }
        // Status bits are write-1-to-clear; leaving the halt latched would
        // make every later check report an underrun that already happened.
        self.device.write16(NABM, PCM_OUT + BOX_STATUS, 0x1C)?;
        self.device.write8(NABM, PCM_OUT + BOX_CONTROL, CONTROL_RUN as u8)
    }

    /// Buffers filled since boot, and underruns suffered.
    pub fn stats(&self) -> (u64, u64) {
        (self.filled, self.underruns)
    }
}
