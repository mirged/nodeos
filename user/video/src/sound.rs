//! The soundtrack's channel to the mixer, which is also the player's clock.

use alloc::vec::Vec;

use libnode::abi::message::MessageFlags;
use libnode::abi::proto;
use libnode::{println, Channel, Message};

/// The mixer voice the soundtrack plays on. Doom uses the bottom eight; the
/// top one is out of its way.
const VOICE: u32 = 15;

pub struct Sound {
    channel: Channel,
    /// Milliseconds between the mixer taking a sample and the card playing it.
    pub latency_ms: u32,
}

impl Sound {
    pub fn open(channel: Channel) -> Option<Self> {
        let reply = channel.call_with(Message::new(proto::AUDIO_GET_INFO), 256, 0).ok()?;
        if reply.flags.contains(MessageFlags::ERROR) {
            return None;
        }
        let info = proto::decode::<proto::AudioInfo>(&reply.payload)?;
        if VOICE >= info.voices {
            return None;
        }
        println!("video: mixer at {} Hz, {} ms latency", info.sample_rate, info.latency_ms);
        Some(Self { channel, latency_ms: info.latency_ms })
    }

    fn call(&self, request: Message) -> Option<Message> {
        let reply = self.channel.call_with(request, 64, 0).ok()?;
        (!reply.flags.contains(MessageFlags::ERROR)).then_some(reply)
    }

    /// Append signed 16-bit mono samples at `rate`, at `volume` (0..=255).
    pub fn queue(&self, samples: &[u8], rate: u32, volume: u32) -> bool {
        let header = proto::Encoded::new(proto::AudioPlay {
            voice: VOICE,
            rate,
            format: proto::audio_format::S16,
            flags: 0,
            volume,
            // A mono soundtrack, straight down the middle.
            separation: 128,
        });
        let mut payload = Vec::with_capacity(core::mem::size_of::<proto::AudioPlay>() + samples.len());
        payload.extend_from_slice(header.bytes());
        payload.extend_from_slice(samples);
        self.call(Message::new(proto::AUDIO_QUEUE).with_payload(&payload)).is_some()
    }

    /// Samples queued but not yet mixed.
    pub fn remaining(&self) -> u64 {
        let request = proto::Encoded::new(proto::AudioVoice { voice: VOICE, _reserved: 0 });
        self.call(Message::new(proto::AUDIO_QUERY_VOICE).with_payload(request.bytes()))
            .and_then(|reply| proto::decode::<proto::AudioVoiceState>(&reply.payload))
            .filter(|state| state.playing != 0)
            .map_or(0, |state| state.remaining as u64)
    }

    /// Change the loudness of what is already queued.
    pub fn set_volume(&self, volume: u32) {
        let request = proto::Encoded::new(proto::AudioVolume { voice: VOICE, volume, separation: 128, _reserved: 0 });
        let _ = self.call(Message::new(proto::AUDIO_SET_VOLUME).with_payload(request.bytes()));
    }

    /// Drop everything queued.
    pub fn stop(&self) {
        let request = proto::Encoded::new(proto::AudioVoice { voice: VOICE, _reserved: 0 });
        let _ = self.call(Message::new(proto::AUDIO_STOP).with_payload(request.bytes()));
    }
}
