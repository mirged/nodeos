//! Keys and window events from the display, decoded once for every screen.

use alloc::vec::Vec;

use libnode::abi::proto::{self, wm_event};
use libnode::abi::Signals;

use crate::screen::Screen;

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Key {
    Escape,
    Enter,
    Space,
    Tab,
    Up,
    Down,
    Left,
    Right,
    PageUp,
    PageDown,
    Home,
    End,
    /// A printable key, lowercased.
    Char(u8),
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Event {
    Key(Key),
    /// The display asked the program to exit (Meta+Q).
    Close,
    /// The surface was replaced; layouts need recomputing.
    Resized,
}

/// Take every event the display has queued, without blocking.
pub fn poll(screen: &mut Screen) -> Vec<Event> {
    let mut events = Vec::new();
    while let Ok(message) = screen.wm.receive_with(64, 1, true) {
        if message.kind != proto::WM_EVENT {
            continue;
        }
        let Some(event) = proto::decode::<proto::WmEvent>(&message.payload) else { continue };
        match event.kind {
            wm_event::CLOSE => events.push(Event::Close),
            wm_event::CONFIGURE => {
                if let Some(handle) = message.handles.into_iter().next() {
                    screen.reconfigure(handle, event.width, event.height);
                    events.push(Event::Resized);
                }
            }
            wm_event::KEY if event.pressed != 0 => {
                if let Some(key) = decode(event.extended != 0, event.scancode, event.ascii) {
                    events.push(Event::Key(key));
                }
            }
            _ => {}
        }
    }
    events
}

/// Block until the display has something to say, or `ns` passes.
pub fn wait(screen: &Screen, ns: u64) {
    let _ = screen.wm.handle().wait(Signals::READABLE, libnode::clock_ns() + ns);
}

/// PS/2 set 1. The cursor block arrives `0xE0`-prefixed; the keypad's
/// unprefixed codes for the same keys count too when they carry no character
/// (Num Lock off).
fn decode(extended: bool, scancode: u8, ascii: u8) -> Option<Key> {
    let cursor = extended || ascii == 0;
    Some(match scancode {
        0x01 => Key::Escape,
        0x1c => Key::Enter,
        0x39 => Key::Space,
        0x0f => Key::Tab,
        0x48 if cursor => Key::Up,
        0x50 if cursor => Key::Down,
        0x4b if cursor => Key::Left,
        0x4d if cursor => Key::Right,
        0x49 if cursor => Key::PageUp,
        0x51 if cursor => Key::PageDown,
        0x47 if cursor => Key::Home,
        0x4f if cursor => Key::End,
        _ if ascii.is_ascii_graphic() => Key::Char(ascii.to_ascii_lowercase()),
        _ => return None,
    })
}
