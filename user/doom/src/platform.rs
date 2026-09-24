//! The `ndos_*` functions the C side calls, and the state behind them.
//!
//! Everything here is `extern "C"` and matches `include/nodeos_platform.h`.
//! The C code holds no NodeOS handle and issues no syscall; it calls one of
//! these, which is the only reason the port needed no changes to Doom's own
//! sources beyond removing the parts that assume a Unix.
//!
//! # What moving to the compositor changed
//!
//! Doom used to hold the framebuffer and the keyboard devices and draw
//! straight at the screen. It now holds a channel to `compositor` and a memory
//! object the size of its window. Two things fall out of that, and both are
//! confined to this file:
//!
//! * `ndos_fb_info` hands back the *surface*, and the C side must say when it
//!   has finished a frame -- hence `ndos_fb_commit`, the one function this
//!   change added to an interface that was otherwise already complete.
//! * `ndos_kbd_read` still returns raw PS/2 scancodes, because Doom's key
//!   table is thirty years old and works. They arrive as decoded events on the
//!   channel and are re-encoded here into the byte stream the C decoder in
//!   `doomgeneric_nodeos.c` already knew how to read.
//!
//! # Single-threaded by construction
//!
//! Doom runs on one thread -- it has no threading of its own, and this port
//! creates none -- so the process-wide state below is reached without a lock.
//! `install` is called once, from `main`, before any C code runs.

use core::cell::UnsafeCell;

use libnode::abi::proto::{self, wm_event};
use libnode::mem::{Mapping, Vmo};
use libnode::{Channel, Message};

/// Depth of the scancode queue between the compositor and Doom's own decoder.
/// Doom drains it once a frame; this is several frames' worth of a fast
/// typist, and a full queue drops the oldest byte rather than the newest.
const SCANCODE_QUEUE_LEN: usize = 128;

/// Everything the C side can reach, established once at startup.
struct Platform {
    /// Kept alive for the process's lifetime; the window is drawn through
    /// `fb_base` rather than through these.
    _surface_vmo: Vmo,
    /// As above.
    _surface_mapping: Mapping,
    fb_base: *mut u8,
    fb_width: u32,
    fb_height: u32,
    fb_pitch: u32,

    /// The compositor connection: commits go out on it, input comes back.
    wm: Channel,
    surface_id: u32,
    keys: ScancodeQueue,

    /// The mixer, if the launcher granted one. `None` means this boot has no
    /// `audio` service and Doom plays silently -- which is a configuration,
    /// not a failure.
    audio: Option<Channel>,

    /// As above: the IWAD is read through `wad_base`.
    _wad_mapping: Mapping,
    wad_base: *const u8,
    wad_len: usize,
}

/// The re-encoded scancode stream, between the compositor's events and the C
/// decoder that consumes bytes.
struct ScancodeQueue {
    buf: [u8; SCANCODE_QUEUE_LEN],
    head: usize,
    len: usize,
}

impl ScancodeQueue {
    const fn new() -> Self {
        Self { buf: [0; SCANCODE_QUEUE_LEN], head: 0, len: 0 }
    }

    fn push(&mut self, byte: u8) {
        let tail = (self.head + self.len) % SCANCODE_QUEUE_LEN;
        if self.len == SCANCODE_QUEUE_LEN {
            self.head = (self.head + 1) % SCANCODE_QUEUE_LEN;
            self.len -= 1;
        }
        self.buf[tail] = byte;
        self.len += 1;
    }

    fn drain_into(&mut self, out: &mut [u8]) -> usize {
        let n = out.len().min(self.len);
        for slot in out.iter_mut().take(n) {
            *slot = self.buf[self.head];
            self.head = (self.head + 1) % SCANCODE_QUEUE_LEN;
        }
        self.len -= n;
        n
    }
}

struct PlatformCell(UnsafeCell<Option<Platform>>);

// SAFETY: this process has exactly one thread (see the module comment), so
// there is no concurrent access to synchronise.
unsafe impl Sync for PlatformCell {}

static PLATFORM: PlatformCell = PlatformCell(UnsafeCell::new(None));

fn platform() -> Option<&'static Platform> {
    // SAFETY: single-threaded; `install` runs to completion before any caller.
    unsafe { (*PLATFORM.0.get()).as_ref() }
}

fn platform_mut() -> Option<&'static mut Platform> {
    // SAFETY: as `platform`. The C side is single-threaded and calls one
    // `ndos_*` function at a time, so no two `&mut` are ever live at once.
    unsafe { (*PLATFORM.0.get()).as_mut() }
}

/// Publish the capabilities Doom will run on. Called once, from `main`.
pub fn install(
    wm: Channel,
    surface: proto::WmSurfaceInfo,
    surface_vmo: Vmo,
    surface_mapping: Mapping,
    wad_mapping: Mapping,
    audio: Option<Channel>,
) {
    let fb_base = surface_mapping.base() as *mut u8;
    let wad_base = wad_mapping.base() as *const u8;
    let wad_len = wad_length(wad_base, wad_mapping.len() as usize);

    let platform = Platform {
        _surface_vmo: surface_vmo,
        _surface_mapping: surface_mapping,
        fb_base,
        fb_width: surface.width,
        fb_height: surface.height,
        fb_pitch: surface.pitch,
        wm,
        surface_id: surface.surface,
        keys: ScancodeQueue::new(),
        audio,
        _wad_mapping: wad_mapping,
        wad_base,
        wad_len,
    };

    // SAFETY: single-threaded, and this is the only write.
    unsafe { *PLATFORM.0.get() = Some(platform) };
}

/// The IWAD's length in bytes.
///
/// A memory object's size is a whole number of pages, so the mapping is up to
/// a page longer than the file the boot loader put there. The WAD format says
/// how long it really is -- the lump directory is the last thing in the file --
/// so ask it, and fall back to the mapping's length for anything that is not a
/// WAD.
fn wad_length(base: *const u8, mapped: usize) -> usize {
    if mapped < 12 {
        return mapped;
    }
    // SAFETY: `base` is a mapping of at least 12 readable bytes.
    let header = unsafe { core::slice::from_raw_parts(base, 12) };
    if &header[..4] != b"IWAD" && &header[..4] != b"PWAD" {
        return mapped;
    }
    let lumps = u32::from_le_bytes([header[4], header[5], header[6], header[7]]) as usize;
    let directory = u32::from_le_bytes([header[8], header[9], header[10], header[11]]) as usize;

    match directory.checked_add(lumps.checked_mul(16).unwrap_or(usize::MAX)) {
        Some(end) if end <= mapped => end,
        // A directory that runs past the mapping means the header is not
        // describing these bytes; trust the mapping rather than the header.
        _ => mapped,
    }
}

/// Take every event the compositor has queued, turning key events back into
/// the raw PS/2 byte stream Doom's decoder expects.
///
/// Non-blocking: Doom polls once a frame and must not stall when nobody is
/// typing.
fn pump_events(platform: &mut Platform) {
    loop {
        // Sized for one `WmEvent` rather than the 4 KiB default: this runs
        // several times a frame and the allocation is the expensive part.
        let Ok(message) = platform.wm.receive_with(64, 1, true) else { return };
        if message.kind != proto::WM_EVENT {
            continue;
        }
        let Some(event) = proto::decode::<proto::WmEvent>(&message.payload) else { continue };
        match event.kind {
            wm_event::KEY => {
                // The inverse of the compositor's decode: an extended key gets
                // its 0xE0 prefix back and a release gets bit 7 set, so the C
                // table reads exactly what it read when Doom owned the
                // keyboard device itself.
                if event.extended != 0 {
                    platform.keys.push(0xE0);
                }
                platform.keys.push(event.scancode | if event.pressed != 0 { 0x00 } else { 0x80 });
            }
            wm_event::CLOSE => libnode::exit(0),
            // Focus changes need no action: the compositor simply stops
            // sending keys, and Doom carries on rendering into its own window.
            _ => {}
        }
    }
}

// ---------------------------------------------------------------------------
// The C interface
// ---------------------------------------------------------------------------

/// `void ndos_write(const char *buf, size_t len)`
///
/// # Safety
/// `buf` must point at `len` readable bytes.
#[no_mangle]
pub unsafe extern "C" fn ndos_write(buf: *const u8, len: usize) {
    if buf.is_null() || len == 0 {
        return;
    }
    // SAFETY: the caller guarantees the range is readable.
    let bytes = unsafe { core::slice::from_raw_parts(buf, len) };
    // The kernel log takes UTF-8; Doom's output is ASCII, and anything that
    // is not is worth seeing as replacement characters rather than dropping.
    for chunk in bytes.utf8_chunks() {
        libnode::print!("{}", chunk.valid());
    }
}

/// `void *ndos_sbrk(size_t len)`
///
/// Doom's heap comes from here rather than from libnode's allocator: the zone
/// allocator asks for one block of several megabytes at startup, which is
/// larger than the runtime heap a service is given, and Doom manages the
/// result itself. Each call is a fresh memory object mapped into the heap
/// window; the C allocator in `libc/malloc.c` links them together.
#[no_mangle]
pub extern "C" fn ndos_sbrk(len: usize) -> *mut u8 {
    use libnode::abi::types::{MapFlags, MapWindow};

    let Ok(vmo) = Vmo::create(len as u64, false) else {
        return core::ptr::null_mut();
    };
    let Ok(mapping) = vmo.map(0, len as u64, MapFlags::READ.or(MapFlags::WRITE), MapWindow::Heap) else {
        return core::ptr::null_mut();
    };
    let base = mapping.base() as *mut u8;

    // The mapping and the object have to outlive every allocation carved out
    // of them, which is the process's lifetime. Nothing here ever gives
    // memory back, because Doom never asks to.
    core::mem::forget(mapping);
    core::mem::forget(vmo);
    base
}

/// `void ndos_exit(int code)`
#[no_mangle]
pub extern "C" fn ndos_exit(code: i32) -> ! {
    libnode::exit(code)
}

/// `uint64_t ndos_time_ns(void)`
#[no_mangle]
pub extern "C" fn ndos_time_ns() -> u64 {
    libnode::clock_ns()
}

/// `void ndos_yield(void)`
#[no_mangle]
pub extern "C" fn ndos_yield() {
    libnode::yield_now();
}

/// `const unsigned char *ndos_wad_data(size_t *len)`
///
/// # Safety
/// `len` must be null or point at a writable `size_t`.
#[no_mangle]
pub unsafe extern "C" fn ndos_wad_data(len: *mut usize) -> *const u8 {
    let Some(platform) = platform() else {
        return core::ptr::null();
    };
    if !len.is_null() {
        // SAFETY: the caller guarantees `len` is writable.
        unsafe { *len = platform.wad_len };
    }
    platform.wad_base
}

/// `uint8_t *ndos_fb_info(uint32_t *width, uint32_t *height, uint32_t *pitch)`
///
/// # Safety
/// Each out-parameter must be null or point at a writable `u32`.
#[no_mangle]
pub unsafe extern "C" fn ndos_fb_info(width: *mut u32, height: *mut u32, pitch: *mut u32) -> *mut u8 {
    let Some(platform) = platform() else {
        return core::ptr::null_mut();
    };
    // SAFETY: the caller guarantees each non-null pointer is writable.
    unsafe {
        if !width.is_null() {
            *width = platform.fb_width;
        }
        if !height.is_null() {
            *height = platform.fb_height;
        }
        if !pitch.is_null() {
            *pitch = platform.fb_pitch;
        }
    }
    platform.fb_base
}

/// `void ndos_fb_commit(uint32_t x, uint32_t y, uint32_t w, uint32_t h)`
///
/// Publish a finished region of the window. Blocks until the compositor has
/// acknowledged it, which is what stops Doom from redrawing pixels the
/// compositor is still copying -- and, incidentally, paces the game to the
/// rate the display server can actually keep up with.
#[no_mangle]
pub extern "C" fn ndos_fb_commit(x: u32, y: u32, w: u32, h: u32) {
    let Some(platform) = platform() else { return };
    let damage = proto::Encoded::new(proto::WmDamage { surface: platform.surface_id, x, y, w, h, _reserved: 0 });
    // Sized for the empty acknowledgement, not the 4 KiB default: this runs
    // 35 times a second, and the runtime allocator never reclaims a block that
    // large -- taking the default here exhausted Doom's heap in seconds.
    let _ = platform.wm.call_with(Message::new(proto::WM_COMMIT).with_payload(damage.bytes()), 64, 1);
}

/// `size_t ndos_kbd_read(unsigned char *buf, size_t len)`
///
/// # Safety
/// `buf` must point at `len` writable bytes.
#[no_mangle]
pub unsafe extern "C" fn ndos_kbd_read(buf: *mut u8, len: usize) -> usize {
    let Some(platform) = platform_mut() else {
        return 0;
    };
    if buf.is_null() || len == 0 {
        return 0;
    }
    pump_events(platform);
    // SAFETY: the caller guarantees the range is writable.
    let out = unsafe { core::slice::from_raw_parts_mut(buf, len) };
    platform.keys.drain_into(out)
}

// ---------------------------------------------------------------------------
// Sound
// ---------------------------------------------------------------------------
//
// The C side (`csrc/i_nodeossound.c`) hands over samples it has already pulled
// out of a DMX lump; everything below is the translation into `AUDIO_*`
// messages. Doom's own scale for these is preserved as far as possible --
// channel numbers become voice numbers unchanged, and a separation of 128 is
// still the middle -- because the alternative is a mapping that has to be
// remembered in two places.

/// `int ndos_audio_available(void)`
///
/// Whether there is a mixer to talk to at all. The sound module's `Init`
/// returns this, so a boot with no `audio` service leaves Doom's
/// `sound_module` null and every later sound call is a no-op inside Doom
/// itself -- no messages, no error path, no cost.
#[no_mangle]
pub extern "C" fn ndos_audio_available() -> i32 {
    platform().is_some_and(|platform| platform.audio.is_some()) as i32
}

/// `int ndos_audio_play(uint32_t voice, const uint8_t *samples, uint32_t count,
///                      uint32_t rate, uint32_t volume, uint32_t separation)`
///
/// Start `count` unsigned 8-bit samples on `voice`, replacing whatever it was
/// playing. Returns 1 on success.
///
/// # Safety
/// `samples` must point at `count` readable bytes.
#[no_mangle]
pub unsafe extern "C" fn ndos_audio_play(
    voice: u32,
    samples: *const u8,
    count: u32,
    rate: u32,
    volume: u32,
    separation: u32,
) -> i32 {
    let Some(platform) = platform() else { return 0 };
    let Some(audio) = platform.audio.as_ref() else { return 0 };
    if samples.is_null() || count == 0 {
        return 0;
    }
    // A sound longer than the message limit is truncated rather than refused:
    // no Doom effect is anywhere near it, and a silent gunshot would be a
    // stranger failure than a clipped one.
    let count = count.min(MAX_SOUND_BYTES) as usize;
    // SAFETY: the caller guarantees `count` readable bytes at `samples`.
    let samples = unsafe { core::slice::from_raw_parts(samples, count) };

    let header = proto::Encoded::new(proto::AudioPlay {
        voice,
        rate,
        format: proto::audio_format::U8,
        flags: 0,
        volume,
        separation,
    });
    let mut payload = alloc::vec::Vec::with_capacity(core::mem::size_of::<proto::AudioPlay>() + count);
    payload.extend_from_slice(header.bytes());
    payload.extend_from_slice(samples);

    // A call, not a send: the reply is what tells Doom the mixer took it, and
    // it is one round trip per *sound*, not per frame. The reply buffer is
    // sized for an empty acknowledgement for the reason `ndos_fb_commit`
    // explains.
    let request = Message::new(proto::AUDIO_PLAY).with_payload(&payload);
    audio.call_with(request, 64, 0).is_ok() as i32
}

/// `void ndos_audio_stop(uint32_t voice)`
#[no_mangle]
pub extern "C" fn ndos_audio_stop(voice: u32) {
    let Some(platform) = platform() else { return };
    let Some(audio) = platform.audio.as_ref() else { return };
    let request = proto::Encoded::new(proto::AudioVoice { voice, _reserved: 0 });
    let _ = audio.call_with(Message::new(proto::AUDIO_STOP).with_payload(request.bytes()), 64, 0);
}

/// `void ndos_audio_set_volume(uint32_t voice, uint32_t volume, uint32_t separation)`
///
/// Doom calls this as a monster walks around the player, several times a
/// second per sounding channel.
#[no_mangle]
pub extern "C" fn ndos_audio_set_volume(voice: u32, volume: u32, separation: u32) {
    let Some(platform) = platform() else { return };
    let Some(audio) = platform.audio.as_ref() else { return };
    let request = proto::Encoded::new(proto::AudioVolume { voice, volume, separation, _reserved: 0 });
    let _ = audio.call_with(Message::new(proto::AUDIO_SET_VOLUME).with_payload(request.bytes()), 64, 0);
}

/// Largest sound this port will send in one message.
///
/// The kernel's payload limit is 64 KiB and the header shares the message, so
/// this leaves room for it.
const MAX_SOUND_BYTES: u32 = 60 * 1024;
