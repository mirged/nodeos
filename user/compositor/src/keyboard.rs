//! PS/2 Set 1 scancode decoding.
//!
//! This used to live in `console`, back when `console` held the keyboard
//! device. It belongs to the compositor now for the same reason the
//! framebuffer does: there is one keyboard, and something has to decide which
//! window the next keystroke is for.
//!
//! Decoding happens once, here, and the result carries *both* forms -- the
//! decoded character and the raw make code that produced it. A client with no
//! opinion (the console) reads `ascii`; a client that brought its own keymap
//! (Doom) reads `scancode` and `extended`. Neither has to know about the
//! other, and neither needs a device capability.

use nodeos_abi::proto::wm_mod;

/// One decoded key transition.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct KeyEvent {
    /// PS/2 set 1 make code, without the release bit.
    pub scancode: u8,
    /// Whether the code arrived `0xE0`-prefixed.
    pub extended: bool,
    /// True for a press, false for a release.
    pub pressed: bool,
    /// Decoded character, or 0 for a key with no printable form.
    pub ascii: u8,
    /// See [`wm_mod`].
    pub modifiers: u32,
}

/// Make code of the left Meta ("Windows") key, which arrives `0xE0`-prefixed.
///
/// The compositor's own hotkey has to be one that no client will ever want,
/// because the compositor swallows it. Meta is the only key on the keyboard
/// that neither the shell's line editor nor Doom's thirty-year-old key table
/// has any use for -- every function key, every modifier and every letter is
/// spoken for somewhere.
pub const META_MAKE: u8 = 0x5B;
/// Scroll Lock, an alias for [`META_MAKE`] on keyboards (and virtual machines)
/// that swallow Meta before it reaches the guest.
///
/// It is a full alias, not just a second way to cycle focus: the decoder tracks
/// it as the Meta modifier, so Scroll Lock+Tab, Scroll Lock+Enter and
/// Scroll Lock+Q are the same shortcuts Meta+Tab, Meta+Enter and Meta+Q are.
/// A *tap* of either key -- press and release with nothing in between -- cycles
/// the focus; holding either and pressing another key is a modifier.
pub const SCROLL_LOCK_MAKE: u8 = 0x46;

const BACKSPACE_MAKE: u8 = 0x0E;
const TAB_MAKE: u8 = 0x0F;
const ENTER_MAKE: u8 = 0x1C;
const LEFT_CTRL_MAKE: u8 = 0x1D;
const LEFT_SHIFT_MAKE: u8 = 0x2A;
const RIGHT_SHIFT_MAKE: u8 = 0x36;
const LEFT_ALT_MAKE: u8 = 0x38;
const SPACE_MAKE: u8 = 0x39;
const EXTENDED_PREFIX: u8 = 0xE0;
const PAUSE_PREFIX: u8 = 0xE1;

/// US QWERTY: `(scancode, unshifted, shifted)`. Explicit pairs rather than a
/// positional table, so a transcription slip misplaces one key instead of
/// scrambling every key after it.
const KEYS: &[(u8, u8, u8)] = &[
    (0x02, b'1', b'!'),
    (0x03, b'2', b'@'),
    (0x04, b'3', b'#'),
    (0x05, b'4', b'$'),
    (0x06, b'5', b'%'),
    (0x07, b'6', b'^'),
    (0x08, b'7', b'&'),
    (0x09, b'8', b'*'),
    (0x0A, b'9', b'('),
    (0x0B, b'0', b')'),
    (0x0C, b'-', b'_'),
    (0x0D, b'=', b'+'),
    (0x10, b'q', b'Q'),
    (0x11, b'w', b'W'),
    (0x12, b'e', b'E'),
    (0x13, b'r', b'R'),
    (0x14, b't', b'T'),
    (0x15, b'y', b'Y'),
    (0x16, b'u', b'U'),
    (0x17, b'i', b'I'),
    (0x18, b'o', b'O'),
    (0x19, b'p', b'P'),
    (0x1A, b'[', b'{'),
    (0x1B, b']', b'}'),
    (0x1E, b'a', b'A'),
    (0x1F, b's', b'S'),
    (0x20, b'd', b'D'),
    (0x21, b'f', b'F'),
    (0x22, b'g', b'G'),
    (0x23, b'h', b'H'),
    (0x24, b'j', b'J'),
    (0x25, b'k', b'K'),
    (0x26, b'l', b'L'),
    (0x27, b';', b':'),
    (0x28, b'\'', b'"'),
    (0x29, b'`', b'~'),
    (0x2B, b'\\', b'|'),
    (0x2C, b'z', b'Z'),
    (0x2D, b'x', b'X'),
    (0x2E, b'c', b'C'),
    (0x2F, b'v', b'V'),
    (0x30, b'b', b'B'),
    (0x31, b'n', b'N'),
    (0x32, b'm', b'M'),
    (0x33, b',', b'<'),
    (0x34, b'.', b'>'),
    (0x35, b'/', b'?'),
];

/// Turns the raw scancode byte stream into [`KeyEvent`]s, tracking the
/// modifier keys as it goes.
#[derive(Default)]
pub struct Decoder {
    extended: bool,
    shift: bool,
    ctrl: bool,
    alt: bool,
    meta: bool,
}

impl Decoder {
    /// A decoder with no keys held.
    pub const fn new() -> Self {
        Self { extended: false, shift: false, ctrl: false, alt: false, meta: false }
    }

    /// Feed one scancode byte. Returns an event for every make and break of a
    /// real key, including the modifier keys themselves -- a client that uses
    /// shift to run and control to fire needs to see them, not just their
    /// effect on the next character.
    pub fn feed(&mut self, byte: u8) -> Option<KeyEvent> {
        if byte == EXTENDED_PREFIX {
            self.extended = true;
            return None;
        }
        // 0xE1 introduces the six-byte Pause sequence. Dropping the prefix
        // leaves its remaining bytes to decode as harmless make codes, which
        // is what every other consumer of this stream already assumed.
        if byte == PAUSE_PREFIX {
            return None;
        }

        let extended = core::mem::take(&mut self.extended);
        let pressed = byte & 0x80 == 0;
        let scancode = byte & 0x7F;

        // Modifier state has to be updated on both edges, and before the
        // character lookup, so that shift-down / 'a' / shift-up produces 'A'.
        match (scancode, extended) {
            (LEFT_SHIFT_MAKE | RIGHT_SHIFT_MAKE, false) => self.shift = pressed,
            (LEFT_CTRL_MAKE, _) => self.ctrl = pressed,
            (LEFT_ALT_MAKE, _) => self.alt = pressed,
            (META_MAKE, true) => self.meta = pressed,
            // Scroll Lock is the same modifier as Meta. It is not `0xE0`-
            // prefixed, which is why it needs its own arm.
            (SCROLL_LOCK_MAKE, false) => self.meta = pressed,
            _ => {}
        }

        let ascii = if extended || !pressed {
            // Extended keys have no printable form in this keymap, and a
            // release never produces a character.
            0
        } else {
            match scancode {
                ENTER_MAKE => b'\n',
                BACKSPACE_MAKE => 0x08,
                SPACE_MAKE => b' ',
                TAB_MAKE => b'\t',
                _ => KEYS
                    .iter()
                    .find(|(code, _, _)| *code == scancode)
                    .map(|(_, lower, upper)| if self.shift { *upper } else { *lower })
                    .unwrap_or(0),
            }
        };

        Some(KeyEvent { scancode, extended, pressed, ascii, modifiers: self.modifiers() })
    }

    fn modifiers(&self) -> u32 {
        let mut mask = 0;
        if self.shift {
            mask |= wm_mod::SHIFT;
        }
        if self.ctrl {
            mask |= wm_mod::CTRL;
        }
        if self.alt {
            mask |= wm_mod::ALT;
        }
        if self.meta {
            mask |= wm_mod::META;
        }
        mask
    }
}

impl KeyEvent {
    /// Whether this key *is* the Meta modifier: the extended Windows key or its
    /// Scroll Lock alias. Both are swallowed by the compositor and both take
    /// part in the tap-to-cycle-shortcut logic in `main`.
    pub fn is_meta_key(&self) -> bool {
        (self.extended && self.scancode == META_MAKE) || (!self.extended && self.scancode == SCROLL_LOCK_MAKE)
    }
}
