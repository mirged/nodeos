//! The `.BAP` format: a header, then frames that each carry their own sound
//! and a complete 1-bit picture.
//!
//! ```text
//!   header   "BAP2"  width:u16 height:u16 fps:u16 frames:u32 rate:u32 channels:u16
//!   frame    audio_len:u32  rle_len:u32  audio[audio_len]  rle[rle_len]   (x frames)
//! ```
//!
//! Everything is little-endian. `audio` is signed 16-bit mono PCM at `rate`.
//! `rle` is alternating black and white run lengths in unsigned LEB128,
//! starting with black. No frame depends on another, so skipping one costs
//! nothing and seeking is an index lookup. `tools/pack_bap.c` writes it.

use alloc::vec::Vec;

/// Bytes in the header.
pub const HEADER_LEN: usize = 20;

/// What the header says, which is all the browser needs to list a file.
#[derive(Clone, Copy, Debug)]
pub struct Header {
    pub width: usize,
    pub height: usize,
    pub fps: u32,
    pub frames: usize,
    pub rate: u32,
}

impl Header {
    pub fn parse(data: &[u8]) -> Result<Self, &'static str> {
        if data.len() < HEADER_LEN || &data[..4] != b"BAP2" {
            return Err("not a .BAP file");
        }
        let u16_at = |at: usize| u16::from_le_bytes([data[at], data[at + 1]]) as u32;
        let header = Self {
            width: u16_at(4) as usize,
            height: u16_at(6) as usize,
            fps: u16_at(8),
            frames: u32_at(data, 10) as usize,
            rate: u32_at(data, 14),
        };
        if header.width == 0 || header.height == 0 || header.fps == 0 || header.fps > 240 {
            return Err("bad picture size or frame rate");
        }
        if u16_at(18) != 1 || header.rate == 0 || header.rate > 192_000 {
            return Err("only mono 16-bit sound is supported");
        }
        Ok(header)
    }

    /// Length in whole seconds.
    pub fn seconds(&self) -> u64 {
        self.frames as u64 / self.fps as u64
    }
}

fn u32_at(data: &[u8], at: usize) -> u32 {
    u32::from_le_bytes([data[at], data[at + 1], data[at + 2], data[at + 3]])
}

/// Where one frame's pieces are in the file.
#[derive(Clone, Copy)]
struct Frame {
    audio: usize,
    audio_len: usize,
    rle: usize,
    rle_len: usize,
}

/// A whole file in memory, indexed.
pub struct Video<'a> {
    data: &'a [u8],
    pub header: Header,
    frames: Vec<Frame>,
}

impl<'a> Video<'a> {
    pub fn parse(data: &'a [u8]) -> Result<Self, &'static str> {
        let mut header = Header::parse(data)?;

        // Index every frame up front, checking each one fits. After this,
        // playback never has to ask whether an offset is in bounds.
        let mut frames = Vec::with_capacity(header.frames);
        let mut at = HEADER_LEN;
        while frames.len() < header.frames && at + 8 <= data.len() {
            let audio_len = u32_at(data, at) as usize;
            let rle_len = u32_at(data, at + 4) as usize;
            let audio = at + 8;
            let Some(end) = audio.checked_add(audio_len).and_then(|rle| rle.checked_add(rle_len)) else { break };
            if end > data.len() {
                break;
            }
            frames.push(Frame { audio, audio_len: audio_len & !1, rle: audio + audio_len, rle_len });
            at = end;
        }
        if frames.is_empty() {
            return Err("the file holds no frames");
        }
        if frames.len() < header.frames {
            libnode::println!("video: file is truncated; playing {} of {} frames", frames.len(), header.frames);
            header.frames = frames.len();
        }
        Ok(Self { data, header, frames })
    }

    pub fn len(&self) -> usize {
        self.frames.len()
    }

    /// Unpack frame `index` into one byte per pixel, 0 black and 1 white.
    pub fn decode(&self, index: usize, out: &mut [u8]) {
        let frame = self.frames[index];
        let rle = &self.data[frame.rle..frame.rle + frame.rle_len];
        let mut colour = 0u8;
        let mut at = 0usize;
        let mut pos = 0usize;
        while at < rle.len() && pos < out.len() {
            let mut run = 0usize;
            let mut shift = 0;
            while at < rle.len() {
                let byte = rle[at];
                at += 1;
                if shift < usize::BITS {
                    run |= ((byte & 0x7f) as usize) << shift;
                }
                shift += 7;
                if byte & 0x80 == 0 {
                    break;
                }
            }
            let end = pos.saturating_add(run).min(out.len());
            out[pos..end].fill(colour);
            pos = end;
            colour ^= 1;
        }
        // A short frame is finished in whatever colour it ran out on, rather
        // than leaving the previous frame's pixels behind.
        out[pos..].fill(colour);
    }

    /// The sound that plays during frame `index`.
    pub fn audio(&self, index: usize) -> &[u8] {
        let frame = self.frames[index];
        &self.data[frame.audio..frame.audio + frame.audio_len]
    }

    /// The soundtrack sample at which frame `index` starts.
    pub fn sample_of(&self, index: usize) -> u64 {
        index as u64 * self.header.rate as u64 / self.header.fps as u64
    }

    /// The frame on screen at soundtrack sample `sample`.
    pub fn frame_at(&self, sample: u64) -> usize {
        (sample * self.header.fps as u64 / self.header.rate as u64) as usize
    }
}
