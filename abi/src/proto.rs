//! Wire protocols for the standard NodeOS services.
//!
//! A NodeOS "system call" for anything above the nine kernel primitives is a
//! message on a channel. That means the protocols *are* the operating system's
//! interface, and they belong in the ABI crate alongside the syscall numbers
//! rather than in whichever service happens to implement them today.
//!
//! Every protocol follows the same shape:
//!
//! * request kinds are `0xNNNN`, replies are the request kind with `0x8000`
//!   set, so a reply is always recognisable without tracking state;
//! * a failed reply sets [`crate::message::MessageFlags::ERROR`] and carries a
//!   little-endian `i32` status in the payload;
//! * capabilities travel in the message's handle array, never encoded in the
//!   payload -- there is no such thing as a "file descriptor number" that means
//!   something to another process.

/// Turn a request kind into its reply kind.
pub const fn reply_kind(request: u32) -> u32 {
    request | 0x8000
}

/// Encode an error status as a reply payload.
pub fn encode_error(status: crate::Error) -> [u8; 4] {
    (status.to_raw() as i32).to_le_bytes()
}

/// A protocol struct together with its wire bytes.
///
/// Every payload here is a `repr(C)` struct, and the obvious way to get its
/// bytes -- `slice::from_raw_parts` over a local -- is a trap. The slice
/// borrows a value the borrow checker cannot see, so nothing stops the
/// compiler reusing that local's stack slot the moment its last *tracked* use
/// goes by, and the caller sends whatever landed there instead. It is a trap
/// that springs quietly: the message is the right length and the right kind,
/// and only the numbers inside it are wrong.
///
/// Wrapping the value fixes it for the usual reason -- the byte slice borrows
/// `self`, whose lifetime the compiler does track:
///
/// ```ignore
/// let request = Encoded::new(WmCreateSurface { width, height, flags: 0, _reserved: 0 });
/// channel.call(Message::new(WM_CREATE_SURFACE).with_payload(request.bytes()))
/// ```
#[repr(transparent)]
pub struct Encoded<T: Copy> {
    value: T,
}

impl<T: Copy> Encoded<T> {
    /// Take ownership of a payload struct.
    pub const fn new(value: T) -> Self {
        Self { value }
    }

    /// The struct's bytes, borrowed from it.
    pub fn bytes(&self) -> &[u8] {
        // SAFETY: `self.value` is owned by `self` and so outlives the returned
        // borrow, and `T` is one of this module's `repr(C)` plain-old-data
        // payload structs, every byte of which is initialised.
        unsafe { core::slice::from_raw_parts(&self.value as *const T as *const u8, core::mem::size_of::<T>()) }
    }
}

/// Read a `repr(C)` payload struct out of a message, if the payload is long
/// enough to hold one.
///
/// The inverse of [`Encoded`], and the reason every service used to carry its
/// own copy of this function.
pub fn decode<T: Copy>(payload: &[u8]) -> Option<T> {
    if payload.len() < core::mem::size_of::<T>() {
        return None;
    }
    // SAFETY: the length is checked above, and every payload struct in this
    // module is `repr(C)` plain-old-data with no invalid bit patterns. Read
    // unaligned because a payload is a byte buffer with no alignment promise.
    Some(unsafe { (payload.as_ptr() as *const T).read_unaligned() })
}

/// Decode an error status from a reply payload.
pub fn decode_error(payload: &[u8]) -> Option<crate::Error> {
    if payload.len() < 4 {
        return None;
    }
    let raw = i32::from_le_bytes([payload[0], payload[1], payload[2], payload[3]]) as i64;
    crate::Error::from_raw(raw)
}

// ---------------------------------------------------------------------------
// Launcher -> service
// ---------------------------------------------------------------------------

/// A launcher introducing a new client to a service. The client's server-side
/// channel endpoint travels in the handle array.
pub const CONNECT: u32 = 0x0101;

// ---------------------------------------------------------------------------
// Log service
// ---------------------------------------------------------------------------

/// Log a line. Payload: one severity byte followed by UTF-8 text.
pub const LOG_WRITE: u32 = 0x1001;
/// Ask the log service for its statistics.
pub const LOG_STATS: u32 = 0x1002;

/// Severity levels understood by the log service.
pub mod log_level {
    /// Something failed.
    pub const ERROR: u8 = 0;
    /// Something unexpected but handled.
    pub const WARN: u8 = 1;
    /// Normal progress.
    pub const INFO: u8 = 2;
    /// Detail.
    pub const DEBUG: u8 = 3;
}

// ---------------------------------------------------------------------------
// Service registry
// ---------------------------------------------------------------------------

/// Publish a service under a name. Handle array: the service's listening
/// endpoint.
pub const REGISTRY_PUBLISH: u32 = 0x2001;
/// Look up a service by name. Reply handle array: a fresh connection to it.
pub const REGISTRY_LOOKUP: u32 = 0x2002;
/// List the names visible to this client.
pub const REGISTRY_LIST: u32 = 0x2003;

// ---------------------------------------------------------------------------
// Filesystem
// ---------------------------------------------------------------------------

/// Create a file. Payload: path.
pub const FS_CREATE: u32 = 0x3001;
/// Open an existing node. Payload: path. Reply payload: [`FsOpenReply`].
pub const FS_OPEN: u32 = 0x3002;
/// Read from a node. Payload: [`FsReadRequest`]. Reply payload: the bytes.
pub const FS_READ: u32 = 0x3003;
/// Write to a node. Payload: [`FsWriteHeader`] followed by the bytes.
pub const FS_WRITE: u32 = 0x3004;
/// Stat a node. Payload: path. Reply payload: [`FsStat`].
pub const FS_STAT: u32 = 0x3005;
/// List a directory. Payload: path. Reply payload: NUL-separated names.
pub const FS_LIST: u32 = 0x3006;
/// Remove a node. Payload: path.
pub const FS_UNLINK: u32 = 0x3007;
/// Obtain a memory object for a file's contents, for zero-copy access.
/// Payload: path. Reply handle array: the memory object.
pub const FS_GET_VMO: u32 = 0x3008;
/// Create a directory. Payload: path.
pub const FS_MKDIR: u32 = 0x3009;
/// Report filesystem statistics.
pub const FS_STATFS: u32 = 0x300A;
/// Release a node identifier returned by [`FS_OPEN`]. Payload:
/// [`FsCloseRequest`].
///
/// A node identifier is not a capability -- it is an index into the service's
/// own table, meaningful only on the channel it was issued on -- so it needs
/// an explicit release. The alternative, a channel per open file, would make
/// closing automatic at the cost of one kernel object per file; see
/// `docs/FILESYSTEM.md`.
pub const FS_CLOSE: u32 = 0x300B;
/// Set a file's length, truncating or extending it. Payload:
/// [`FsTruncateRequest`].
pub const FS_TRUNCATE: u32 = 0x300C;

/// Node kinds.
pub mod fs_kind {
    /// A regular file.
    pub const FILE: u32 = 1;
    /// A directory.
    pub const DIRECTORY: u32 = 2;
}

/// Reply to [`FS_OPEN`].
#[repr(C)]
#[derive(Clone, Copy, Debug, Default)]
pub struct FsOpenReply {
    /// Stable object identifier within the filesystem.
    pub node: u64,
    /// Size in bytes.
    pub size: u64,
    /// See [`fs_kind`].
    pub kind: u32,
    /// Padding.
    pub _reserved: u32,
}

/// Payload of [`FS_READ`].
#[repr(C)]
#[derive(Clone, Copy, Debug, Default)]
pub struct FsReadRequest {
    /// Node identifier from [`FS_OPEN`].
    pub node: u64,
    /// Byte offset.
    pub offset: u64,
    /// Bytes requested.
    pub length: u32,
    /// Padding.
    pub _reserved: u32,
}

/// Header of a [`FS_WRITE`] payload; the bytes to write follow it.
#[repr(C)]
#[derive(Clone, Copy, Debug, Default)]
pub struct FsWriteHeader {
    /// Node identifier from [`FS_OPEN`].
    pub node: u64,
    /// Byte offset.
    pub offset: u64,
    /// Bytes to write.
    pub length: u32,
    /// Padding.
    pub _reserved: u32,
}

/// Reply to [`FS_STAT`].
#[repr(C)]
#[derive(Clone, Copy, Debug, Default)]
pub struct FsStat {
    /// Stable object identifier.
    pub node: u64,
    /// Size in bytes.
    pub size: u64,
    /// Number of entries, for a directory.
    pub entries: u32,
    /// See [`fs_kind`].
    pub kind: u32,
    /// Creation time, nanoseconds since boot.
    pub created_ns: u64,
    /// Last modification time, nanoseconds since boot.
    pub modified_ns: u64,
}

/// Payload of [`FS_CLOSE`].
#[repr(C)]
#[derive(Clone, Copy, Debug, Default)]
pub struct FsCloseRequest {
    /// Node identifier from [`FS_OPEN`].
    pub node: u64,
}

/// Payload of [`FS_TRUNCATE`].
#[repr(C)]
#[derive(Clone, Copy, Debug, Default)]
pub struct FsTruncateRequest {
    /// Node identifier from [`FS_OPEN`].
    pub node: u64,
    /// The length the file should have afterwards.
    pub length: u64,
}

/// Reply to [`FS_STATFS`].
#[repr(C)]
#[derive(Clone, Copy, Debug, Default)]
pub struct FsStatFs {
    /// Nodes currently open on this connection.
    pub nodes: u64,
    /// Bytes the volume holds in total.
    pub bytes: u64,
    /// Bytes not yet allocated to any file.
    pub free: u64,
    /// Block-device requests served since boot.
    pub requests: u64,
    /// Bytes in one allocation unit.
    pub block_size: u64,
    /// Filesystem type, as a four-character tag ("FAT6", "FAT3").
    pub kind: u64,
}

// ---------------------------------------------------------------------------
// Console
// ---------------------------------------------------------------------------

/// Report the console's geometry. No payload. Reply payload: [`ConsoleInfo`].
pub const CONSOLE_GET_INFO: u32 = 0x5001;
/// Clear the screen and reset the cursor to the origin. No payload.
pub const CONSOLE_CLEAR: u32 = 0x5002;
/// Write text at the cursor, wrapping and scrolling like a TTY. Payload:
/// UTF-8 bytes.
pub const CONSOLE_WRITE_TEXT: u32 = 0x5003;
/// Move the text cursor. Payload: [`ConsoleCursor`].
pub const CONSOLE_SET_CURSOR: u32 = 0x5004;
/// Fill a rectangle of pixels. Payload: [`ConsoleFillRect`].
pub const CONSOLE_FILL_RECT: u32 = 0x5005;
/// Draw a one-pixel-wide line. Payload: [`ConsoleDrawLine`].
pub const CONSOLE_DRAW_LINE: u32 = 0x5006;
/// Read one line of input, sent with `channel_call`. The console holds the
/// reply until Enter is pressed, echoing keystrokes to the display itself as
/// they arrive. No request payload. Reply payload: the UTF-8 line, without
/// the trailing newline.
pub const CONSOLE_READ_LINE: u32 = 0x5007;

/// Reply to [`CONSOLE_GET_INFO`].
#[repr(C)]
#[derive(Clone, Copy, Debug, Default)]
pub struct ConsoleInfo {
    /// Framebuffer width, in pixels.
    pub width_px: u32,
    /// Framebuffer height, in pixels.
    pub height_px: u32,
    /// Text columns (width_px / glyph width).
    pub cols: u32,
    /// Text rows (height_px / glyph height).
    pub rows: u32,
}

/// Payload of [`CONSOLE_SET_CURSOR`].
#[repr(C)]
#[derive(Clone, Copy, Debug, Default)]
pub struct ConsoleCursor {
    /// Column, in glyph cells.
    pub col: u32,
    /// Row, in glyph cells.
    pub row: u32,
}

/// A packed 0xRRGGBB colour, as used by [`ConsoleFillRect`] and
/// [`ConsoleDrawLine`]. The console's framebuffer may use a different pixel
/// layout; converting is the console service's job, not the protocol's.
pub type Rgb = u32;

/// Payload of [`CONSOLE_FILL_RECT`].
#[repr(C)]
#[derive(Clone, Copy, Debug, Default)]
pub struct ConsoleFillRect {
    /// Left edge, in pixels.
    pub x: u32,
    /// Top edge, in pixels.
    pub y: u32,
    /// Width, in pixels.
    pub w: u32,
    /// Height, in pixels.
    pub h: u32,
    /// Fill colour.
    pub color: Rgb,
}

/// Payload of [`CONSOLE_DRAW_LINE`].
#[repr(C)]
#[derive(Clone, Copy, Debug, Default)]
pub struct ConsoleDrawLine {
    /// Start X, in pixels.
    pub x0: u32,
    /// Start Y, in pixels.
    pub y0: u32,
    /// End X, in pixels.
    pub x1: u32,
    /// End Y, in pixels.
    pub y1: u32,
    /// Line colour.
    pub color: Rgb,
}

// ---------------------------------------------------------------------------
// Window system
// ---------------------------------------------------------------------------
//
// The compositor owns the one framebuffer and the one keyboard. A client owns
// a *surface*: a memory object of its own pixels, which it draws into at its
// own pace and then commits. The compositor decides where -- and whether --
// those pixels appear on the screen, and which client the next keystroke
// belongs to.
//
// Nothing here mentions the framebuffer's geometry, its address, or the
// keyboard. That is the point: a client holding a compositor channel holds
// authority over its own window and nothing else, which is strictly less than
// the `Display`/`Input` device capabilities it would otherwise need.

/// Create a surface. Payload: [`WmCreateSurface`], optionally followed by the
/// window title as UTF-8. Reply payload: [`WmSurfaceInfo`]; reply handle
/// array: the surface's memory object, which the client maps shared.
pub const WM_CREATE_SURFACE: u32 = 0x6001;
/// Publish a damaged region of a surface. Payload: [`WmDamage`]. The reply is
/// the compositor's acknowledgement that it has finished reading those pixels,
/// which is also what paces a client that draws faster than the screen.
pub const WM_COMMIT: u32 = 0x6002;
/// Rename a window. Payload: [`WmSurfaceRef`] followed by the title as UTF-8.
pub const WM_SET_TITLE: u32 = 0x6003;
/// Destroy a surface and its window. Payload: [`WmSurfaceRef`].
pub const WM_DESTROY_SURFACE: u32 = 0x6004;

/// An input or lifecycle event, sent by the compositor to a client without
/// being asked. Payload: [`WmEvent`].
///
/// Sent non-blocking and dropped if the client's queue is full: a client that
/// has stopped reading its events must not be able to stall the compositor,
/// and therefore everyone else's window with it.
pub const WM_EVENT: u32 = 0x6010;

/// Event kinds carried in [`WmEvent::kind`].
pub mod wm_event {
    /// A key was pressed or released. See [`super::WmEvent`]'s key fields.
    pub const KEY: u32 = 1;
    /// This surface now has the keyboard focus.
    pub const FOCUS_IN: u32 = 2;
    /// This surface no longer has the keyboard focus.
    pub const FOCUS_OUT: u32 = 3;
    /// The compositor is asking the client to exit.
    pub const CLOSE: u32 = 4;
    /// The window's content area changed size; `width`/`height` are the new
    /// dimensions and the surface memory object has been replaced.
    pub const CONFIGURE: u32 = 5;
    /// The pointer moved while it was over this surface, or while this
    /// surface held the implicit grab of a button press. `x`/`y` are the
    /// pointer's position relative to the surface's content origin, and
    /// `buttons` is the mask of buttons held down -- a client that drags
    /// wants both in one event, not a motion event and a separate query.
    pub const POINTER_MOTION: u32 = 6;
    /// A pointer button changed state over this surface. `button` names the
    /// button, `pressed` says which way, and `x`/`y`/`buttons` carry the same
    /// meaning as for [`POINTER_MOTION`].
    pub const POINTER_BUTTON: u32 = 7;
}

/// Pointer buttons, as the bits of [`super::WmEvent::buttons`].
///
/// Bit values rather than indices: the state of every button at once is what
/// a client that is dragging something needs, and one field carrying all of
/// them costs no more than one carrying a single button. The button named by
/// [`super::WmEvent::button`] on a [`wm_event::POINTER_BUTTON`] event is
/// exactly one of these bits.
pub mod wm_button {
    /// The primary button.
    pub const LEFT: u32 = 1 << 0;
    /// The secondary button.
    pub const RIGHT: u32 = 1 << 1;
    /// The middle button.
    pub const MIDDLE: u32 = 1 << 2;
}

/// Modifier bits carried in [`WmEvent::modifiers`].
pub mod wm_mod {
    /// Either shift key is down.
    pub const SHIFT: u32 = 1 << 0;
    /// Either control key is down.
    pub const CTRL: u32 = 1 << 1;
    /// Either alt key is down.
    pub const ALT: u32 = 1 << 2;
    /// The Meta/Windows key used for compositor shortcuts.
    pub const META: u32 = 1 << 3;
}

/// Payload of [`WM_CREATE_SURFACE`]. The title, if any, follows these bytes.
#[repr(C)]
#[derive(Clone, Copy, Debug, Default)]
pub struct WmCreateSurface {
    /// Desired content width in pixels. The compositor may grant less; the
    /// reply says what was actually allocated.
    pub width: u32,
    /// Desired content height in pixels.
    pub height: u32,
    /// Reserved for surface options; must be zero.
    pub flags: u32,
    /// Padding.
    pub _reserved: u32,
}

/// Reply to [`WM_CREATE_SURFACE`]. The surface's memory object travels in the
/// reply's handle array.
///
/// The buffer is `pitch * height` bytes of 32bpp `0x00RRGGBB` pixels -- the
/// same format as the framebuffer, so the compositor's blit is a copy.
#[repr(C)]
#[derive(Clone, Copy, Debug, Default)]
pub struct WmSurfaceInfo {
    /// Identifier for this surface, unique within the connection. Used by
    /// every later request and by every event about this window.
    pub surface: u32,
    /// Content width in pixels.
    pub width: u32,
    /// Content height in pixels.
    pub height: u32,
    /// Bytes per scanline.
    pub pitch: u32,
    /// Size of the memory object, in bytes.
    pub size: u64,
}

/// Payload of [`WM_COMMIT`].
#[repr(C)]
#[derive(Clone, Copy, Debug, Default)]
pub struct WmDamage {
    /// Surface identifier from [`WmSurfaceInfo`].
    pub surface: u32,
    /// Left edge of the damaged region, in surface pixels.
    pub x: u32,
    /// Top edge of the damaged region, in surface pixels.
    pub y: u32,
    /// Width of the damaged region.
    pub w: u32,
    /// Height of the damaged region.
    pub h: u32,
    /// Padding.
    pub _reserved: u32,
}

/// Payload of the requests that only name a surface.
#[repr(C)]
#[derive(Clone, Copy, Debug, Default)]
pub struct WmSurfaceRef {
    /// Surface identifier from [`WmSurfaceInfo`].
    pub surface: u32,
    /// Padding.
    pub _reserved: u32,
}

/// Payload of [`WM_EVENT`].
///
/// A key event carries both the decoded character and the raw PS/2 set 1
/// scancode that produced it. Decoding once, in the compositor, is what stops
/// every client from needing a keymap; keeping the raw code is what lets a
/// client with its own keymap -- Doom, whose key handling predates this
/// protocol by thirty years -- keep using it.
#[repr(C)]
#[derive(Clone, Copy, Debug, Default)]
pub struct WmEvent {
    /// Surface the event is about.
    pub surface: u32,
    /// See [`wm_event`].
    pub kind: u32,
    /// PS/2 set 1 make code, without the release bit.
    pub scancode: u8,
    /// 1 when the code arrived `0xE0`-prefixed.
    pub extended: u8,
    /// 1 for a press, 0 for a release.
    pub pressed: u8,
    /// Decoded character, or 0 for a key with no printable form.
    pub ascii: u8,
    /// See [`wm_mod`].
    pub modifiers: u32,
    /// New content width, for [`wm_event::CONFIGURE`].
    pub width: u32,
    /// New content height, for [`wm_event::CONFIGURE`].
    pub height: u32,
    /// Pointer X relative to the surface's content origin, for
    /// [`wm_event::POINTER_MOTION`] and [`wm_event::POINTER_BUTTON`].
    ///
    /// Signed, and deliberately not clamped to the surface: a client holding
    /// an implicit grab keeps receiving motion after the pointer has left its
    /// window, and a value that wrapped at the edge would turn a drag off the
    /// top-left corner into a jump across the window.
    pub x: i32,
    /// Pointer Y. See [`WmEvent::x`].
    pub y: i32,
    /// Buttons held down, as [`wm_button`] bits, for the pointer events.
    pub buttons: u32,
    /// The button whose state changed, for [`wm_event::POINTER_BUTTON`]: one
    /// [`wm_button`] bit. Zero for every other event.
    pub button: u32,
}

// ---------------------------------------------------------------------------
// Audio
// ---------------------------------------------------------------------------

/// Ask the audio service what it can play. Reply payload: [`AudioInfo`].
pub const AUDIO_GET_INFO: u32 = 0x7001;
/// Start a sound on one voice. Payload: [`AudioPlay`] followed by the samples.
///
/// A voice that is already playing is *replaced*, not layered: that is what a
/// game's sound channel means, and doing it here rather than making the client
/// stop and start is one round trip instead of two.
pub const AUDIO_PLAY: u32 = 0x7002;
/// Silence one voice. Payload: [`AudioVoice`].
pub const AUDIO_STOP: u32 = 0x7003;
/// Change a voice's volume and stereo position mid-sound, or the master
/// volume. Payload: [`AudioVolume`].
pub const AUDIO_SET_VOLUME: u32 = 0x7004;
/// Ask whether a voice is still sounding. Payload: [`AudioVoice`].
/// Reply payload: [`AudioVoiceState`].
pub const AUDIO_QUERY_VOICE: u32 = 0x7005;

/// Sample formats [`AUDIO_PLAY`] accepts.
///
/// Both are mono, because both callers are: a mixer that is given stereo
/// material has nothing left to do with a stereo position.
pub mod audio_format {
    /// Unsigned 8-bit mono -- the format Doom's DMX sound lumps are in, and
    /// the reason it is here rather than a conversion in every client.
    pub const U8: u32 = 1;
    /// Signed 16-bit little-endian mono.
    pub const S16: u32 = 2;
}

/// Flags for [`AudioPlay`].
pub mod audio_flags {
    /// Repeat until stopped.
    pub const LOOP: u32 = 1 << 0;
}

/// The voice number meaning "the mix as a whole" in [`AudioVolume`].
pub const AUDIO_MASTER_VOICE: u32 = u32::MAX;

/// Reply to [`AUDIO_GET_INFO`].
#[repr(C)]
#[derive(Clone, Copy, Debug, Default)]
pub struct AudioInfo {
    /// Sample rate the hardware is running at, in hertz. A client does not
    /// have to match it -- [`AudioPlay::rate`] says what its samples are and
    /// the service resamples -- but a client that can choose should choose
    /// this.
    pub sample_rate: u32,
    /// Channels the hardware is playing (2 for the AC'97 driver).
    pub channels: u32,
    /// Bits per hardware sample.
    pub bits: u32,
    /// How many voices can sound at once.
    pub voices: u32,
    /// Milliseconds of audio buffered ahead of the play position. This is the
    /// latency between [`AUDIO_PLAY`] and the first sample leaving the card,
    /// and a client that wants to feel responsive should know it.
    pub latency_ms: u32,
    /// Padding.
    pub _reserved: u32,
}

/// Header of an [`AUDIO_PLAY`] payload; the samples follow it.
#[repr(C)]
#[derive(Clone, Copy, Debug, Default)]
pub struct AudioPlay {
    /// Which voice to sound this on, `0..AudioInfo::voices`.
    pub voice: u32,
    /// Sample rate of the attached samples, in hertz.
    pub rate: u32,
    /// See [`audio_format`].
    pub format: u32,
    /// See [`audio_flags`].
    pub flags: u32,
    /// Loudness, 0 (silent) to 255 (full).
    pub volume: u32,
    /// Stereo position: 0 hard left, 128 centre, 255 hard right.
    pub separation: u32,
}

/// Names one voice, for [`AUDIO_STOP`] and [`AUDIO_QUERY_VOICE`].
#[repr(C)]
#[derive(Clone, Copy, Debug, Default)]
pub struct AudioVoice {
    /// Voice number.
    pub voice: u32,
    /// Padding.
    pub _reserved: u32,
}

/// Payload of [`AUDIO_SET_VOLUME`].
#[repr(C)]
#[derive(Clone, Copy, Debug, Default)]
pub struct AudioVolume {
    /// Voice number, or [`AUDIO_MASTER_VOICE`].
    pub voice: u32,
    /// Loudness, 0 to 255.
    pub volume: u32,
    /// Stereo position, 0 to 255. Ignored for the master.
    pub separation: u32,
    /// Padding.
    pub _reserved: u32,
}

/// Reply to [`AUDIO_QUERY_VOICE`].
#[repr(C)]
#[derive(Clone, Copy, Debug, Default)]
pub struct AudioVoiceState {
    /// 1 while the voice still has samples to play.
    pub playing: u32,
    /// Samples remaining, in the client's own rate.
    pub remaining: u32,
}
