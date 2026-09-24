//! Voices, resampling and the mix itself.
//!
//! The mixing is here rather than in the client for the same reason the
//! compositor owns the blit: there is one output device, several programs want
//! it at once, and the thing that owns it is the only party that can decide
//! what the sum sounds like. A client says "this sound, on this voice, this
//! loud, this far to the left" and never learns what else is playing.
//!
//! # Fixed point, and why there is no floating point here
//!
//! Resampling is a rate ratio and a position, and both are kept in 16.16 fixed
//! point. That is not nostalgia: NodeOS saves SSE state across a context
//! switch now (see `docs/COMPOSITOR.md`), but integers are exact, and a mixer
//! that accumulates position error over a long sound drifts audibly against
//! everything else in the mix.

use alloc::vec::Vec;

use libnode::abi::proto::{audio_flags, audio_format};

/// Voices that can sound at once.
///
/// Doom asks for eight and uses them as a hard channel limit, so sixteen is
/// room for the game plus whatever else has a channel to the service.
pub const VOICES: usize = 16;

/// Fractional bits in a sample position.
const FRACTION_BITS: u32 = 16;
const ONE: u64 = 1 << FRACTION_BITS;

/// One sound in flight.
struct Voice {
    /// Mono samples, already converted to signed 16-bit.
    samples: Vec<i16>,
    /// Playback position in the sample array, 16.16 fixed point.
    position: u64,
    /// How far to advance per output frame, 16.16 fixed point.
    step: u64,
    /// Left and right gains, 0..=256 where 256 is unity.
    left: u32,
    right: u32,
    /// Whether to wrap to the start instead of stopping.
    repeating: bool,
    /// False once the sound has finished; the samples are kept until the slot
    /// is reused, which costs a little memory and saves a free per gunshot.
    active: bool,
}

impl Voice {
    const fn silent() -> Self {
        Self {
            samples: Vec::new(),
            position: 0,
            step: ONE,
            left: 0,
            right: 0,
            repeating: false,
            active: false,
        }
    }
}

/// Every voice, plus the master volume.
pub struct Mixer {
    voices: Vec<Voice>,
    /// Applied to the sum, 0..=256.
    master: u32,
    /// Output rate the hardware is running at.
    output_rate: u32,
    /// Frames mixed since boot. Not reported anywhere yet; kept because the
    /// mixer's own idea of how much audio it has produced is the first thing
    /// to compare against the card's when they disagree.
    frames: u64,
}

impl Mixer {
    /// A silent mixer feeding a device running at `output_rate`.
    pub fn new(output_rate: u32) -> Self {
        let mut voices = Vec::new();
        for _ in 0..VOICES {
            voices.push(Voice::silent());
        }
        Self { voices, master: 256, output_rate, frames: 0 }
    }

    /// Start `samples` on `voice`, replacing whatever it was playing.
    ///
    /// The conversion to the mixer's own representation happens once, here,
    /// rather than per output frame: a sound is played tens of thousands of
    /// times more often than it is started.
    pub fn play(
        &mut self,
        voice: usize,
        samples: &[u8],
        format: u32,
        rate: u32,
        volume: u32,
        separation: u32,
        flags: u32,
    ) -> Result<usize, ()> {
        let slot = self.voices.get_mut(voice).ok_or(())?;
        if rate == 0 {
            return Err(());
        }

        let converted: Vec<i16> = match format {
            // DMX sound lumps are unsigned 8-bit centred on 128. Shifting into
            // the top eight bits is the whole conversion; there is nothing to
            // dither, because no information is being discarded.
            audio_format::U8 => samples.iter().map(|byte| ((*byte as i16) - 128) << 8).collect(),
            audio_format::S16 => samples
                .chunks_exact(2)
                .map(|pair| i16::from_le_bytes([pair[0], pair[1]]))
                .collect(),
            _ => return Err(()),
        };
        if converted.is_empty() {
            return Err(());
        }

        let count = converted.len();
        slot.samples = converted;
        slot.position = 0;
        // Advance by (input rate / output rate) input samples per output
        // frame: a 11 025 Hz effect played at 48 000 Hz moves 0.2297 samples
        // per frame, so most output frames repeat the sample before them.
        slot.step = (rate as u64 * ONE) / self.output_rate as u64;
        slot.repeating = flags & audio_flags::LOOP != 0;
        slot.active = true;
        let (left, right) = gains(volume, separation);
        slot.left = left;
        slot.right = right;
        Ok(count)
    }

    /// Silence one voice.
    pub fn stop(&mut self, voice: usize) {
        if let Some(slot) = self.voices.get_mut(voice) {
            slot.active = false;
            // Dropping the samples here rather than at the next `play` is what
            // keeps a service that has been running for an hour from holding
            // every sound it has ever played.
            slot.samples = Vec::new();
        }
    }

    /// Adjust a sounding voice without restarting it -- what a game does when
    /// the thing making the noise moves.
    pub fn set_volume(&mut self, voice: usize, volume: u32, separation: u32) {
        if let Some(slot) = self.voices.get_mut(voice) {
            let (left, right) = gains(volume, separation);
            slot.left = left;
            slot.right = right;
        }
    }

    /// Set the volume applied to the whole mix.
    pub fn set_master(&mut self, volume: u32) {
        self.master = (volume.min(255) * 256) / 255;
    }

    /// Whether a voice still has samples to play, and how many.
    pub fn voice_state(&self, voice: usize) -> (bool, u32) {
        match self.voices.get(voice) {
            Some(slot) if slot.active => {
                let played = (slot.position >> FRACTION_BITS) as usize;
                (true, slot.samples.len().saturating_sub(played) as u32)
            }
            _ => (false, 0),
        }
    }

    /// Whether anything at all is sounding.
    pub fn idle(&self) -> bool {
        !self.voices.iter().any(|voice| voice.active)
    }

    /// Fill `out` with interleaved stereo frames.
    ///
    /// The accumulator is 32-bit and the clamp is at the end: summing sixteen
    /// voices in 16-bit would wrap, and a wrap is not a loud sound, it is a
    /// crack at full scale in the opposite direction.
    pub fn render(&mut self, out: &mut [i16]) {
        let frames = out.len() / 2;
        for slot in out.iter_mut() {
            *slot = 0;
        }

        for voice in self.voices.iter_mut() {
            if !voice.active || voice.samples.is_empty() {
                continue;
            }
            let length = (voice.samples.len() as u64) << FRACTION_BITS;
            for frame in 0..frames {
                if voice.position >= length {
                    if voice.repeating {
                        // Wrap by subtraction rather than reset, so a loop
                        // whose length is not a whole number of output frames
                        // does not accumulate a gap at every seam.
                        voice.position %= length;
                    } else {
                        voice.active = false;
                        break;
                    }
                }
                let sample = voice.samples[(voice.position >> FRACTION_BITS) as usize] as i32;
                let left = &mut out[frame * 2];
                *left = saturate(*left as i32 + ((sample * voice.left as i32) >> 8));
                let right = &mut out[frame * 2 + 1];
                *right = saturate(*right as i32 + ((sample * voice.right as i32) >> 8));
                voice.position += voice.step;
            }
        }

        if self.master != 256 {
            for slot in out.iter_mut() {
                *slot = saturate((*slot as i32 * self.master as i32) >> 8);
            }
        }
        self.frames += frames as u64;
    }
}

/// Split a volume and a stereo position into per-channel gains.
///
/// Linear panning rather than constant power: it is one multiply, it is what
/// Doom's own mixer did, and the difference is inaudible for effects that are
/// mostly transients.
fn gains(volume: u32, separation: u32) -> (u32, u32) {
    let volume = volume.min(255);
    let separation = separation.min(255);
    // Doubling either side means the centre is full volume on both channels
    // rather than half on each, which is what a listener expects from "in
    // front of me".
    let left = (255 - separation).saturating_mul(2).min(255);
    let right = separation.saturating_mul(2).min(255);
    ((left * volume) / 255, (right * volume) / 255)
}

/// Clamp to the 16-bit sample range.
fn saturate(value: i32) -> i16 {
    value.clamp(i16::MIN as i32, i16::MAX as i32) as i16
}
