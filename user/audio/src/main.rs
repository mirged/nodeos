//! `audio` -- the sound service.
//!
//! Holds the AC'97 controller the boot launcher grants under
//! `CapClass::Audio`, and is the only process in NodeOS that does. Everything
//! that wants to make a noise gets a channel and speaks `abi::proto`'s
//! `AUDIO_*` kinds; nothing else can reach a port, a DMA buffer, or another
//! program's voices.
//!
//! This is the same shape as `compositor`, and for the same reason. There is
//! one sound card and several programs that want it, so something has to own
//! it and decide what the sum is. A client that could open the card itself
//! would silence everyone else the moment it started the DMA engine, exactly
//! as a client holding the framebuffer used to own the whole screen.
//!
//! ```text
//!   doom ──AUDIO_PLAY(voice, samples)──┐
//!                                      ├─► audio ──► mixer ──► AC'97 ring
//!   anything else ───────────────────  ┘             16 voices   16 buffers
//! ```
//!
//! # The loop, and why it is shaped like this
//!
//! Mixing has a deadline: the card is playing a 10.7 ms buffer and the next
//! one has to be ready before it finishes. NodeOS has no timer object and no
//! sleep, so the service polls -- but only while something is sounding. When
//! every voice has finished it stops the engine and blocks on its channels
//! like any other service, which costs nothing until the next sound arrives.
//! A machine sitting at a shell prompt does no audio work at all.

#![no_std]
#![no_main]

extern crate alloc;

mod ac97;
mod mixer;

use alloc::vec::Vec;

use libnode::abi::message::MessageFlags;
use libnode::abi::proto::{self, AUDIO_MASTER_VOICE};
use libnode::abi::types::device_kind;
use libnode::abi::Signals;
use libnode::device::Device;
use libnode::wait::wait_any;
use libnode::{println, Channel, Env, Error, Message};

use ac97::Ac97;
use mixer::Mixer;

libnode::main! {
    fn main(env: Env) -> i32 {
        run(env)
    }
}

/// Largest `AUDIO_PLAY` payload accepted, header included.
///
/// A sound bigger than this is a stream, and streaming wants a shared memory
/// object rather than a message -- see the honest-limitations section of
/// `docs/AUDIO.md`. Doom's largest effect is a few kilobytes.
const MAX_REQUEST: usize = 64 * 1024;

/// How long to wait for a client message before going back to the mixer.
///
/// Half a buffer: short enough that a refill is never late because a client
/// went quiet, long enough that an idle-but-sounding service is not spinning
/// on the scheduler.
const POLL_NS: u64 = 5_000_000;

struct Service {
    card: Ac97,
    mixer: Mixer,
    clients: Vec<Channel>,
    /// Whether the DMA engine is currently running.
    running: bool,
    /// When the last statistics line was logged.
    last_report_ns: u64,
}

fn run(mut env: Env) -> i32 {
    let Some(device_handle) = env.take_extra(0) else {
        println!("audio: no sound card granted; exiting");
        return 1;
    };
    let Some(bootstrap) = env.take_bootstrap() else {
        println!("audio: no bootstrap channel; exiting");
        return 1;
    };

    let device = Device::from_handle(device_handle);
    match device.info() {
        Ok(info) if info.kind == device_kind::AC97 => {
            println!(
                "audio: AC'97 {:04x}:{:04x}, mixer {:#x}+{}, engine {:#x}+{}",
                info.vendor, info.device, info.port_base[0], info.port_len[0], info.port_base[1], info.port_len[1]
            );
        }
        Ok(info) => {
            println!("audio: device is kind {}, not an AC'97; exiting", info.kind);
            return 1;
        }
        Err(error) => {
            println!("audio: device_info failed ({error:?}); exiting");
            return 1;
        }
    }

    let card = match Ac97::new(device) {
        Ok(card) => card,
        Err(error) => {
            println!("audio: could not initialise the card ({error:?}); exiting");
            return 1;
        }
    };

    println!(
        "audio: {} Hz stereo, {} voices, {} ms latency, {} ms ring",
        ac97::SAMPLE_RATE,
        mixer::VOICES,
        latency_ms(),
        ac97::Ac97::ring_ms()
    );

    let mut service = Service {
        card,
        mixer: Mixer::new(ac97::SAMPLE_RATE),
        clients: Vec::new(),
        running: false,
        last_report_ns: 0,
    };
    serve(&mut service, &bootstrap);
    0
}

/// Milliseconds of audio between a `AUDIO_PLAY` arriving and the first sample
/// of it leaving the card, worst case.
fn latency_ms() -> u32 {
    (ac97::LOOKAHEAD as u32 * ac97::FRAMES_PER_BUFFER as u32 * 1000) / ac97::SAMPLE_RATE
}

fn serve(service: &mut Service, bootstrap: &Channel) {
    loop {
        // Silence is the cheap case: with nothing sounding there is no
        // deadline to meet, so stop the engine and block. This is what keeps
        // the service off the run queue for the 99% of a boot in which nobody
        // is making a noise.
        let idle = service.mixer.idle();
        if idle && service.running {
            service.stop_engine();
            // One line per quiet moment, at most: buffers mixed and underruns
            // suffered are the two numbers that say whether the sound was
            // clean, and they are worth having in the log without a debug
            // build. Rate-limited because a game makes a noise every second.
            let (filled, underruns) = service.card.stats();
            let now = libnode::clock_ns();
            if now.saturating_sub(service.last_report_ns) > 10 * 1_000_000_000 {
                service.last_report_ns = now;
                println!("audio: {filled} buffers mixed, {underruns} underrun(s)");
            }
        }

        let mut items = Vec::with_capacity(service.clients.len() + 1);
        items.push((bootstrap.handle().raw(), Signals::READABLE));
        for client in &service.clients {
            items.push((client.handle().raw(), Signals::READABLE.or(Signals::PEER_CLOSED)));
        }
        let deadline = if idle { 0 } else { libnode::clock_ns() + POLL_NS };

        match wait_any(&items, deadline) {
            Ok((0, _)) => accept_client(service, bootstrap),
            Ok((index, observed)) => {
                let client = index - 1;
                if observed.contains(Signals::READABLE) {
                    // One message per pass rather than draining: mixing is on
                    // a deadline, and a client that queues a hundred sounds
                    // must not be able to push the refill past it.
                    if let Ok(message) = service.clients[client].receive_with(MAX_REQUEST, 0, true) {
                        handle_request(service, client, message);
                    }
                } else if observed.contains(Signals::PEER_CLOSED) {
                    disconnect(service, client);
                }
            }
            // A timeout is the normal case while sound is playing: it means
            // nobody asked for anything and it is time to mix.
            Err(_) => {}
        }

        if !service.mixer.idle() {
            service.pump();
        }
    }
}

impl Service {
    /// Mix into every buffer the card has passed, and keep the engine running.
    fn pump(&mut self) {
        let mixer = &mut self.mixer;
        let written = self.card.refill(|buffer| mixer.render(buffer)).unwrap_or(0);
        if !self.running || self.card.halted() {
            // Starting after the first refill rather than before it is what
            // stops the card playing the uninitialised ring once at boot.
            if written > 0 || self.running {
                let _ = self.card.start();
                self.running = true;
            }
        }
    }

    fn stop_engine(&mut self) {
        // Nothing to play: let the engine run off the end of its valid
        // descriptors, which is what it does when `refill` stops moving LVI.
        // Explicitly clearing `running` means the next sound restarts it
        // rather than assuming it kept going.
        self.running = false;
    }
}

fn accept_client(service: &mut Service, bootstrap: &Channel) {
    let Ok(message) = bootstrap.try_receive() else { return };
    if message.kind != proto::CONNECT || message.handles.is_empty() {
        println!("audio: ignoring bootstrap message of kind {:#x}", message.kind);
        return;
    }
    let mut handles = message.handles;
    service.clients.push(Channel::from_handle(handles.remove(0)));
    println!("audio: client {} connected", service.clients.len());
}

fn disconnect(service: &mut Service, index: usize) {
    service.clients.remove(index);
    println!("audio: client disconnected");
    // A client's voices are not stopped when it goes: several clients share
    // the voice space today and there is no ownership recorded per voice.
    // `docs/AUDIO.md` says why that is a deliberate limit rather than a bug.
}

fn handle_request(service: &mut Service, client: usize, message: Message) {
    match message.kind {
        proto::AUDIO_GET_INFO => {
            let info = proto::Encoded::new(proto::AudioInfo {
                sample_rate: ac97::SAMPLE_RATE,
                channels: 2,
                bits: 16,
                voices: mixer::VOICES as u32,
                latency_ms: latency_ms(),
                _reserved: 0,
            });
            reply(service, client, &message, Message::new(proto::reply_kind(message.kind)).with_payload(info.bytes()));
        }
        proto::AUDIO_PLAY => play(service, client, message),
        proto::AUDIO_STOP => {
            if let Some(target) = proto::decode::<proto::AudioVoice>(&message.payload) {
                service.mixer.stop(target.voice as usize);
            }
            reply(service, client, &message, Message::new(proto::reply_kind(message.kind)));
        }
        proto::AUDIO_SET_VOLUME => {
            if let Some(request) = proto::decode::<proto::AudioVolume>(&message.payload) {
                if request.voice == AUDIO_MASTER_VOICE {
                    service.mixer.set_master(request.volume);
                } else {
                    service.mixer.set_volume(request.voice as usize, request.volume, request.separation);
                }
            }
            reply(service, client, &message, Message::new(proto::reply_kind(message.kind)));
        }
        proto::AUDIO_QUERY_VOICE => {
            let voice = proto::decode::<proto::AudioVoice>(&message.payload).map(|v| v.voice).unwrap_or(0);
            let (playing, remaining) = service.mixer.voice_state(voice as usize);
            let state = proto::Encoded::new(proto::AudioVoiceState {
                playing: playing as u32,
                remaining,
            });
            reply(
                service,
                client,
                &message,
                Message::new(proto::reply_kind(message.kind)).with_payload(state.bytes()),
            );
        }
        other => {
            println!("audio: unknown request kind {other:#x}");
            reply_error(service, client, &message, Error::InvalidArgument);
        }
    }
}

fn play(service: &mut Service, client: usize, message: Message) {
    let Some(request) = proto::decode::<proto::AudioPlay>(&message.payload) else {
        reply_error(service, client, &message, Error::InvalidArgument);
        return;
    };
    let samples = message.payload.get(core::mem::size_of::<proto::AudioPlay>()..).unwrap_or(&[]);
    if request.voice as usize >= mixer::VOICES {
        reply_error(service, client, &message, Error::InvalidArgument);
        return;
    }

    let started = service.mixer.play(
        request.voice as usize,
        samples,
        request.format,
        request.rate,
        request.volume,
        request.separation,
        request.flags,
    );
    match started {
        Ok(_) => {
            // Mix immediately rather than waiting for the next pass: the reply
            // is what unblocks the client, and by the time it runs again the
            // first buffer of this sound is already on the ring.
            service.pump();
            reply(service, client, &message, Message::new(proto::reply_kind(proto::AUDIO_PLAY)));
        }
        Err(()) => reply_error(service, client, &message, Error::InvalidArgument),
    }
}

fn reply(service: &Service, client: usize, request: &Message, message: Message) {
    let _ = service.clients[client].reply(request, message);
}

fn reply_error(service: &Service, client: usize, request: &Message, status: Error) {
    let mut message = Message::new(proto::reply_kind(request.kind));
    message.flags = MessageFlags::ERROR;
    message.payload = Vec::from(proto::encode_error(status));
    let _ = service.clients[client].reply(request, message);
}
