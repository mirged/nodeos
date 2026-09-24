//! The pointer: PS/2 packets in, an arrow on the screen, and the arithmetic
//! that connects the two.
//!
//! The compositor is the only process in NodeOS that holds the mouse, for the
//! same reason it is the only one that holds the keyboard: there is one
//! pointer, and something has to decide which window it is over. Clients learn
//! about it through `WM_EVENT`, exactly as they learn about keys, and the rest
//! of this module is the part they never see.
//!
//! Three things live here and nowhere else:
//!
//! * **Packet reassembly.** A PS/2 mouse reports movement in three-byte
//!   packets that are only meaningful together, arriving one byte per
//!   interrupt. The device gives no framing, so the decoder finds its own: the
//!   first byte of a packet always has bit 3 set, and nothing else in a packet
//!   is guaranteed to. That is the resynchronisation marker, and using it is
//!   the difference between a pointer that recovers from a dropped byte and
//!   one that stays scrambled until the next boot.
//!
//! * **The cursor itself.** There is no hardware cursor and no second plane to
//!   put one on, so the arrow is drawn into the framebuffer like everything
//!   else -- which means the pixels underneath it have to be saved and put
//!   back, or each frame's draw leaves a trail of arrows across the screen.
//!   `uncover`/`paint` are that pair, and every function in `main` that writes
//!   the framebuffer is bracketed by them.
//!
//! * **A decoder self-test.** The packet rules above are the sort of thing that
//!   fails quietly -- a sign extension that is wrong turns up as a pointer
//!   that jumps, not as an error -- so the decoder is checked against
//!   hand-written byte streams on every boot, before the first real packet
//!   arrives. See [`selftest`].
//!
//! The pointer's position is kept in *screen* pixels, and clients are told
//! where it is only in their own surface's coordinates. A window that moves
//! does not have to be told the pointer is somewhere new relative to it,
//! because the compositor recomputes that at delivery time.

use libdraw::{Canvas, Rgb};
use libnode::println;
use nodeos_abi::proto::wm_button;

/// Bytes in a packet from a mouse that reports movement three bytes at a time.
///
/// The device can be asked for four (the fourth carries a scroll wheel), and
/// deliberately is not: nothing in this tree scrolls, and a mode that is
/// negotiated but unused is one more thing that can be out of step with the
/// device.
const PACKET_LEN: usize = 3;

/// Bit 3 of the first byte of a packet is always set.
///
/// Nothing else in a packet guarantees anything about that bit, which is what
/// makes it usable as the resynchronisation marker.
const FIRST_BYTE_MARK: u8 = 1 << 3;
/// X movement overflowed the device's counter.
const OVERFLOW_X: u8 = 1 << 6;
/// Y movement overflowed the device's counter.
const OVERFLOW_Y: u8 = 1 << 7;
/// X delta is negative.
const SIGN_X: u8 = 1 << 4;
/// Y delta is negative.
const SIGN_Y: u8 = 1 << 5;
/// Button bits, as the device reports them.
const BUTTON_LEFT: u8 = 1 << 0;
/// See [`BUTTON_LEFT`].
const BUTTON_RIGHT: u8 = 1 << 1;
/// See [`BUTTON_LEFT`].
const BUTTON_MIDDLE: u8 = 1 << 2;

/// One decoded movement packet.
///
/// `dx`/`dy` are in device counts, which this compositor treats as pixels --
/// see [`Pointer::move_by`] for what that costs. `buttons` is the complete
/// button state after the packet, which is what makes a drag expressible in
/// one event.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Default)]
pub struct Motion {
    /// Horizontal movement since the previous packet.
    pub dx: i32,
    /// Vertical movement since the previous packet.
    pub dy: i32,
    /// Buttons held, as [`wm_button`] bits.
    pub buttons: u32,
}

/// PS/2 packet reassembly.
#[derive(Clone, Copy, Debug, Default)]
pub struct Decoder {
    packet: [u8; PACKET_LEN],
    have: usize,
}

impl Decoder {
    /// A decoder with no packet in progress.
    pub const fn new() -> Self {
        Self { packet: [0; PACKET_LEN], have: 0 }
    }

    /// Forget any packet in progress.
    ///
    /// The caller owns the only thing that can find a packet boundary after a
    /// byte has been lost -- see [`Decoder::feed`] -- so it owns the decision
    /// to resynchronise, and this is how it acts on that decision.
    pub const fn reset(&mut self) {
        self.have = 0;
    }

    /// Feed one byte from the device. `Some` once a whole packet has arrived.
    ///
    /// A byte where the *start* of a packet is expected is checked, because
    /// byte zero is the only byte in a packet that is marked: bit 3 is always
    /// set there, and a delta byte that happens to have bit 3 set is not a
    /// packet start only because the decoder is not looking for one. So a
    /// stray byte -- a command's acknowledgement read from the wrong port, say
    /// -- costs the stream nothing; it is discarded and the next marked byte
    /// starts a packet.
    ///
    /// What that cannot fix is a byte lost *inside* a packet. Deltas carry no
    /// framing, so the rest of the packet is indistinguishable from the start
    /// of the next one, and every packet after it is read one byte out of
    /// step. There is no way to notice from the bytes alone; the only signal is
    /// that the device went quiet, which is why the caller resets this on an
    /// idle gap longer than any the device produces when it is working.
    pub fn feed(&mut self, byte: u8) -> Option<Motion> {
        if self.have == 0 && byte & FIRST_BYTE_MARK == 0 {
            return None;
        }
        self.packet[self.have] = byte;
        self.have += 1;
        if self.have < PACKET_LEN {
            return None;
        }
        self.have = 0;

        let flags = self.packet[0];
        // Sign extend: the deltas are eight-bit values whose ninth bit lives
        // in the flags byte, so a negative one arrives as a small positive
        // number and has to be pushed down by 256.
        let dx = self.packet[1] as i32 - if flags & SIGN_X != 0 { 0x100 } else { 0 };
        let dy = self.packet[2] as i32 - if flags & SIGN_Y != 0 { 0x100 } else { 0 };
        // The device counts Y upwards and the screen counts it downwards, so
        // the sign is flipped here, at the one place that knows which
        // convention the bytes are in. Everything above this -- the compositor,
        // the protocol, a client -- is in screen coordinates, where a larger Y
        // is further down the screen.
        let dy = -dy;

        let mut buttons = 0;
        if flags & BUTTON_LEFT != 0 {
            buttons |= wm_button::LEFT;
        }
        if flags & BUTTON_RIGHT != 0 {
            buttons |= wm_button::RIGHT;
        }
        if flags & BUTTON_MIDDLE != 0 {
            buttons |= wm_button::MIDDLE;
        }

        // An overflowed axis carries a movement that is not the movement that
        // happened, so it is dropped rather than clamped: a clamped value tells
        // the compositor the pointer went somewhere it never went, and the
        // pointer would jump. The other axis is unaffected -- the flags are per
        // axis -- and the buttons are unaffected too, so a packet that says
        // "the button went down while the mouse was flung across the desk" is
        // still worth delivering.
        Some(Motion {
            dx: if flags & OVERFLOW_X != 0 { 0 } else { dx },
            dy: if flags & OVERFLOW_Y != 0 { 0 } else { dy },
            buttons,
        })
    }
}

// ---------------------------------------------------------------------------
// The cursor
// ---------------------------------------------------------------------------

/// Width of the arrow, in pixels.
pub const CURSOR_W: u32 = 11;
/// Height of the arrow, in pixels.
pub const CURSOR_H: u32 = 16;

/// The arrow's body, one string per row, `#` for inside and anything else for
/// outside.
///
/// Only the *body* is written down: the black edge is derived from it in
/// [`Pointer::paint`], so the shape is stated once rather than twice, and a
/// slip while editing it cannot produce a border that disagrees with the thing
/// it is drawn around. The hot spot is the top-left pixel, which is where the
/// tip is.
const CURSOR_BODY: [&str; CURSOR_H as usize] = [
    "##.........",
    "###........",
    "####.......",
    "#####......",
    "######.....",
    "#######....",
    "########...",
    "#########..",
    "##########.",
    "###########",
    "##########.",
    "#######....",
    "####.##....",
    "###...##...",
    "##.....##..",
    "#......##..",
];

/// The arrow's edge.
const CURSOR_EDGE: Rgb = 0x0000_0000;
/// The arrow's fill.
const CURSOR_FILL: Rgb = 0x00FF_FFFF;

/// Whether the arrow's body covers pixel `(x, y)` of the sprite.
fn body_pixel(x: i32, y: i32) -> bool {
    if x < 0 || y < 0 || x >= CURSOR_W as i32 || y >= CURSOR_H as i32 {
        return false;
    }
    CURSOR_BODY[y as usize].as_bytes()[x as usize] == b'#'
}

/// The colour the arrow puts at sprite pixel `(x, y)`, or `None` to leave the
/// screen alone.
///
/// A pixel of the body is white unless it touches the outside of the body,
/// which is the whole of the edge computation: one pass over the body, no
/// second bitmap to keep in step with the first.
fn cursor_pixel(x: i32, y: i32) -> Option<Rgb> {
    if !body_pixel(x, y) {
        return None;
    }
    let edge = !body_pixel(x - 1, y) || !body_pixel(x + 1, y) || !body_pixel(x, y - 1) || !body_pixel(x, y + 1);
    Some(if edge { CURSOR_EDGE } else { CURSOR_FILL })
}

/// A rectangle of the framebuffer the arrow currently covers.
#[derive(Clone, Copy, Debug, Default)]
struct Saved {
    x: u32,
    y: u32,
    w: u32,
    h: u32,
    /// Whether there is a saved region at all.
    valid: bool,
}

/// The pointer: where it is, what is held, and the pixels under the arrow.
pub struct Pointer {
    /// Screen position of the arrow's tip, clamped to the screen.
    x: u32,
    /// See [`Pointer::x`].
    y: u32,
    /// Buttons currently held, as [`wm_button`] bits.
    pub buttons: u32,
    /// Whether a mouse answered at bring-up.
    ///
    /// A machine with no mouse still has an auxiliary port, so this is a real
    /// question with a real answer: a cursor that can never move is worse than
    /// no cursor, so an absent pointer is not drawn at all.
    visible: bool,
    /// The pixels the arrow is currently covering.
    saved: [Rgb; (CURSOR_W * CURSOR_H) as usize],
    /// Where on the screen those pixels came from.
    region: Saved,
}

impl Pointer {
    /// A pointer at the centre of a `width x height` screen.
    ///
    /// The centre rather than a corner because the device reports *relative*
    /// movement and nothing in the system knows where the pointer really is
    /// when NodeOS starts; the middle of the screen is the only starting
    /// position that is equally far from everything.
    pub fn new(width: u32, height: u32, visible: bool) -> Self {
        Self {
            x: width / 2,
            y: height / 2,
            buttons: 0,
            visible,
            saved: [0; (CURSOR_W * CURSOR_H) as usize],
            region: Saved::default(),
        }
    }

    /// Where the arrow's tip is, in screen pixels.
    pub fn position(&self) -> (u32, u32) {
        (self.x, self.y)
    }

    /// Move by a packet's delta, clamped to a `width x height` screen.
    ///
    /// One count is one pixel. A real mouse reports around 400 counts per inch
    /// and a real system would multiply that by a speed the user chose and by
    /// an acceleration curve on top; neither exists here, and the honest
    /// description of what this does is "the pointer goes where the ball went",
    /// which is enough to reach any pixel of a 1024x768 screen with a hand
    /// movement of about seven centimetres.
    pub fn move_by(&mut self, dx: i32, dy: i32, width: u32, height: u32) {
        if !self.visible {
            return;
        }
        self.x = (self.x as i32 + dx).clamp(0, width.saturating_sub(1) as i32) as u32;
        self.y = (self.y as i32 + dy).clamp(0, height.saturating_sub(1) as i32) as u32;
    }

    /// Put back the pixels the arrow is covering, if it is on the screen.
    ///
    /// Must run before *anything* writes the framebuffer: the arrow is not a
    /// separate layer, so a blit that lands under it would be painted over and
    /// lost, and a save-under buffer that outlived its position would restore
    /// pixels belonging to an older frame.
    pub fn uncover(&mut self, screen: &mut Canvas) {
        if !self.region.valid {
            return;
        }
        for row in 0..self.region.h {
            for column in 0..self.region.w {
                let index = (row * CURSOR_W + column) as usize;
                screen.put_pixel(self.region.x + column, self.region.y + row, self.saved[index]);
            }
        }
        self.region.valid = false;
    }

    /// Move the arrow to where the pointer is now: take the old one off, save
    /// what is under the new one, draw it.
    ///
    /// Starting with [`Pointer::uncover`] is what makes the arrow move rather
    /// than multiply. Every caller may therefore bracket its own writes without
    /// tracking whether something above it already uncovered -- and a caller
    /// that only moved the pointer, like the packet drain, does not have to
    /// remember to uncover at all: the old arrow is restored, then a new one is
    /// drawn, and the screen shows one arrow at the new position.
    ///
    /// A caller that wrote the framebuffer *under* the arrow still has to
    /// uncover before it does, because the pixels saved here belong to the old
    /// position and would be put back on top of the new ones.
    pub fn paint(&mut self, screen: &mut Canvas) {
        if !self.visible {
            return;
        }
        self.uncover(screen);
        // Clipped to the screen: the tip can sit in the last column or row,
        // and the region saved has to be exactly the region drawn.
        let w = CURSOR_W.min(screen.width().saturating_sub(self.x));
        let h = CURSOR_H.min(screen.height().saturating_sub(self.y));
        if w == 0 || h == 0 {
            return;
        }

        for row in 0..h {
            for column in 0..w {
                let index = (row * CURSOR_W + column) as usize;
                let Some(under) = screen.pixel(self.x + column, self.y + row) else { continue };
                self.saved[index] = under;
                if let Some(color) = cursor_pixel(column as i32, row as i32) {
                    screen.put_pixel(self.x + column, self.y + row, color);
                }
            }
        }
        self.region = Saved { x: self.x, y: self.y, w, h, valid: true };
    }
}

// ---------------------------------------------------------------------------
// Self-test
// ---------------------------------------------------------------------------

/// One check: what a byte stream decoded to, against what it should decode to.
struct Check {
    name: &'static str,
    bytes: &'static [u8],
    /// Index of the byte before which the decoder is reset, standing in for the
    /// idle gap the caller uses to resynchronise. [`NO_RESET`] is a stream with
    /// no gap in it.
    reset_before: usize,
    /// Every packet the stream should produce, in order.
    expected: &'static [Motion],
}

/// No gap in the stream: feed every byte to one decoder.
const NO_RESET: usize = usize::MAX;

/// The decoder's test vectors.
///
/// Hand-written rather than generated, because the thing being checked is a
/// hand-written reading of somebody else's specification, and a generator
/// would encode the same misreading twice.
const CHECKS: &[Check] = &[
    Check {
        name: "right and down, from device bytes that say right and down",
        // The device's Y counts upwards, so "down the screen" arrives as a
        // negative delta: -20 as the device reports it, which is 0xEC.
        bytes: &[0x08 | SIGN_Y, 10, 0xEC],
        reset_before: NO_RESET,
        expected: &[Motion { dx: 10, dy: 20, buttons: 0 }],
    },
    Check {
        name: "the device's up is the screen's up",
        bytes: &[0x08, 0, 1],
        reset_before: NO_RESET,
        expected: &[Motion { dx: 0, dy: -1, buttons: 0 }],
    },
    Check {
        name: "right and up, with both signs extended",
        bytes: &[0x08 | SIGN_X | SIGN_Y, 0x0A, 0x14],
        reset_before: NO_RESET,
        expected: &[Motion { dx: -246, dy: 236, buttons: 0 }],
    },
    Check {
        name: "a negative delta is not a small positive one",
        bytes: &[0x08 | SIGN_X, 0x01, 0x00],
        reset_before: NO_RESET,
        expected: &[Motion { dx: -255, dy: 0, buttons: 0 }],
    },
    Check {
        name: "one axis overflowing leaves the other alone",
        bytes: &[0x08 | SIGN_X | OVERFLOW_Y | BUTTON_LEFT, 0xFE, 0x05],
        reset_before: NO_RESET,
        expected: &[Motion { dx: -2, dy: 0, buttons: wm_button::LEFT }],
    },
    Check {
        name: "both axes overflowing still reports the buttons",
        bytes: &[0x08 | OVERFLOW_X | OVERFLOW_Y | BUTTON_RIGHT, 0x7F, 0x7F],
        reset_before: NO_RESET,
        expected: &[Motion { dx: 0, dy: 0, buttons: wm_button::RIGHT }],
    },
    Check {
        name: "buttons report held and released",
        bytes: &[0x08 | BUTTON_LEFT, 0, 0, 0x08, 0, 0],
        reset_before: NO_RESET,
        expected: &[
            Motion { dx: 0, dy: 0, buttons: wm_button::LEFT },
            Motion { dx: 0, dy: 0, buttons: 0 },
        ],
    },
    Check {
        name: "every button is named",
        bytes: &[0x08 | BUTTON_LEFT | BUTTON_RIGHT | BUTTON_MIDDLE, 0, 0],
        reset_before: NO_RESET,
        expected: &[Motion { dx: 0, dy: 0, buttons: wm_button::LEFT | wm_button::RIGHT | wm_button::MIDDLE }],
    },
    Check {
        name: "a stray byte before a packet is discarded",
        bytes: &[0x00, 0x00, 0x08, 1, 2],
        reset_before: NO_RESET,
        expected: &[Motion { dx: 1, dy: -2, buttons: 0 }],
    },
    Check {
        name: "a byte lost inside a packet is recovered from at the next gap",
        bytes: &[0x08, 1, 0x08, 3, 4],
        reset_before: 2,
        expected: &[Motion { dx: 3, dy: -4, buttons: 0 }],
    },
    Check {
        name: "without a gap, a lost byte is still read one out of step",
        // The same stream with no reset: the honest statement of what the
        // decoder cannot do on its own, kept as a vector so that the reason the
        // caller resets on an idle gap is written down next to the behaviour.
        bytes: &[0x08, 1, 0x08, 3, 4],
        reset_before: NO_RESET,
        expected: &[Motion { dx: 1, dy: -8, buttons: 0 }],
    },
    Check {
        name: "a packet is delivered once, after its last byte",
        bytes: &[0x08, 7, 9, 0x08, 1, 1],
        reset_before: NO_RESET,
        expected: &[Motion { dx: 7, dy: -9, buttons: 0 }, Motion { dx: 1, dy: -1, buttons: 0 }],
    },
];

/// Run every check against a fresh decoder.
///
/// Returns `(passed, failed)` so the caller can decide what a failure means --
/// which, for a decoder this one has no way to work around, is to refuse to
/// run: a pointer placed by a decoder that cannot read its own test vectors
/// would be wrong in ways nobody would think to look for.
pub fn selftest() -> (u32, u32) {
    let mut passed = 0;
    let mut failed = 0;

    for check in CHECKS {
        let mut decoder = Decoder::new();
        let mut decoded: [Option<Motion>; 4] = [None; 4];
        let mut count = 0;
        for (index, byte) in check.bytes.iter().enumerate() {
            if index == check.reset_before {
                decoder.reset();
            }
            if let Some(motion) = decoder.feed(*byte) {
                if count < decoded.len() {
                    decoded[count] = Some(motion);
                }
                count += 1;
            }
        }

        let mut ok = count == check.expected.len();
        for (index, expected) in check.expected.iter().enumerate() {
            ok &= decoded.get(index).copied().flatten().is_some_and(|motion| motion == *expected);
        }
        if ok {
            passed += 1;
        } else {
            failed += 1;
            println!("pointer: decoder check failed: {}", check.name);
        }
    }

    (passed, failed)
}
