//! The full-screen text console service.
//!
//! Holds a shared surface from `display` and serves text/cursor/input requests
//! from the shell. Visible cells and bounded scrollback live in `libdraw`;
//! this process owns no device and contains no desktop or drawing API.

#![no_std]
#![no_main]

extern crate alloc;

mod surface;

use alloc::collections::VecDeque;

use libnode::abi::message::MessageFlags;
use libnode::abi::proto::{self, wm_event};
use libnode::abi::types::{MapFlags, MapWindow};
use libnode::abi::Signals;
use libnode::mem::Vmo;
use libnode::wait::wait_any;
use libnode::{println, Channel, Env, Message};

use surface::Surface;

libnode::main! {
    fn main(env: Env) -> i32 {
        run(env)
    }
}

/// Content size the console asks the display broker for. The display broker decides
/// what it actually gets -- the request is a statement of what would be
/// comfortable, not a demand.
const WANTED_WIDTH: u32 = 512;
/// See [`WANTED_WIDTH`].
const WANTED_HEIGHT: u32 = 768;

fn run(mut env: Env) -> i32 {
    let Some(wm_handle) = env.take_extra(0) else {
        println!("console: no display broker connection granted; exiting");
        return 1;
    };
    let Some(bootstrap) = env.take_bootstrap() else {
        println!("console: no bootstrap channel; exiting");
        return 1;
    };
    let wm = Channel::from_handle(wm_handle);

    let mut screen = match create_surface(&wm) {
        Some(screen) => screen,
        None => return 1,
    };

    screen.clear();

    screen.commit(&wm);

    serve(&mut screen, &wm, &bootstrap);
    0
}

/// Ask the display broker for a full-screen surface, and map the memory object it replies with.
fn create_surface(wm: &Channel) -> Option<Surface> {
    let request = proto::Encoded::new(proto::WmCreateSurface {
        width: WANTED_WIDTH,
        height: WANTED_HEIGHT,
        flags: 0,
        _reserved: 0,
    });
    let mut payload = alloc::vec::Vec::from(request.bytes());
    payload.extend_from_slice(b"console");

    let Ok(mut reply) = wm.call(Message::new(proto::WM_CREATE_SURFACE).with_payload(&payload))
    else {
        println!("console: the display broker refused a surface; exiting");
        return None;
    };
    if reply.flags.contains(MessageFlags::ERROR) || reply.handles.is_empty() {
        println!(
            "console: no surface granted ({:?}); exiting",
            proto::decode_error(&reply.payload)
        );
        return None;
    }
    let info = proto::decode::<proto::WmSurfaceInfo>(&reply.payload)?;

    let vmo = Vmo::from_handle(reply.handles.remove(0), info.size);
    // Mapped eagerly and shared: the display broker holds the other mapping of
    // these pages and reads them whenever this process commits.
    let Ok(mapping) = vmo.map(
        0,
        info.size,
        MapFlags::READ
            .or(MapFlags::WRITE)
            .or(MapFlags::SHARED)
            .or(MapFlags::POPULATE),
        MapWindow::Shared,
    ) else {
        println!("console: failed to map the surface; exiting");
        return None;
    };

    println!(
        "console: {}x{} terminal, readable monospace",
        info.width, info.height
    );
    // SAFETY: the mapping covers the whole object, whose size the display broker
    // reported as `pitch * height`, and nothing else here maps it.
    Some(unsafe { Surface::new(vmo, mapping, &info) })
}

/// A `CONSOLE_READ_LINE` request the client is waiting on, plus what has been
/// typed so far.
struct PendingReadLine {
    request: Message,
    line: alloc::string::String,
}

/// Keys that arrived before a client asked for one, and the two kinds of read
/// that can be outstanding.
///
/// A client uses either `CONSOLE_READ_LINE` (the console edits and echoes) or
/// `CONSOLE_READ_KEY` (the client edits); never both at once. The queue exists
/// because a shell using `READ_KEY` makes one round trip per keystroke, and a
/// fast typist can outrun it.
struct Input {
    line: Option<PendingReadLine>,
    key: Option<Message>,
    queue: VecDeque<proto::ConsoleKey>,
}

/// How many key presses to buffer between reads.
const KEY_QUEUE_LEN: usize = 64;

fn serve(screen: &mut Surface, wm: &Channel, bootstrap: &Channel) {
    let mut input = Input {
        line: None,
        key: None,
        queue: VecDeque::new(),
    };
    // Exactly one client in this system today (`shell`); a service meant to
    // hold several would keep a list here instead of one `Option`.
    let mut client: Option<Channel> = None;

    loop {
        let mut items = alloc::vec![
            (bootstrap.handle().raw(), Signals::READABLE),
            (wm.handle().raw(), Signals::READABLE),
        ];
        if let Some(client) = client.as_ref() {
            items.push((client.handle().raw(), Signals::READABLE));
        }

        let Ok((index, _)) = wait_any(&items, 0) else {
            continue;
        };

        match index {
            0 => {
                if let Ok(message) = bootstrap.try_receive() {
                    println!(
                        "console: DEBUG bootstrap kind {:#x} handles {}",
                        message.kind,
                        message.handles.len()
                    );
                    if message.kind == proto::CONNECT && !message.handles.is_empty() {
                        let mut handles = message.handles;
                        client = Some(Channel::from_handle(handles.remove(0)));
                    }
                }
            }
            1 => {
                // Replies to this process's own `WM_COMMIT` calls are taken by
                // `channel_call` itself, correlated by transaction id, so
                // anything left here is an unsolicited event.
                if let Ok(message) = wm.try_receive() {
                    handle_event(screen, wm, message, client.as_ref(), &mut input);
                }
            }
            _ => {
                let Some(channel) = client.as_ref() else {
                    continue;
                };
                let Ok(message) = channel.try_receive() else {
                    continue;
                };
                handle_request(screen, wm, channel, message, &mut input);
            }
        }
    }
}

// ---------------------------------------------------------------------------
// Input, from the display broker
// ---------------------------------------------------------------------------

fn handle_event(
    screen: &mut Surface,
    wm: &Channel,
    message: Message,
    client: Option<&Channel>,
    input: &mut Input,
) {
    if message.kind != proto::WM_EVENT {
        return;
    }
    let Some(event) = proto::decode::<proto::WmEvent>(&message.payload) else {
        return;
    };
    match event.kind {
        wm_event::KEY if event.pressed != 0 => {
            if event.extended != 0
                && event.modifiers & proto::wm_mod::SHIFT != 0
                && matches!(event.scancode, 0x49 | 0x51)
            {
                let page = screen.rows().saturating_sub(1) as i32;
                screen.scroll_view(if event.scancode == 0x49 { page } else { -page });
                screen.commit(wm);
                return;
            }
            let key = proto::ConsoleKey {
                scancode: event.scancode,
                extended: event.extended,
                pressed: event.pressed,
                ascii: event.ascii,
                modifiers: event.modifiers,
                _reserved: 0,
            };

            // A client waiting for a raw key gets it and does its own editing.
            if let Some(request) = input.key.take() {
                if let Some(client) = client {
                    let reply = Message::new(proto::reply_kind(proto::CONSOLE_READ_KEY))
                        .with_payload(proto::Encoded::new(key).bytes());
                    let _ = client.reply(&request, reply);
                }
                return;
            }

            // Otherwise a `CONSOLE_READ_LINE` client, or a queue for the next
            // reader. Only a line read gives a keystroke anywhere to go on its
            // own: the console has no scrollback.
            let Some(state) = input.line.as_mut() else {
                if input.queue.len() < KEY_QUEUE_LEN {
                    input.queue.push_back(key);
                }
                return;
            };
            match event.ascii {
                0 => {}
                b'\n' => {
                    let Some(client) = client else { return };
                    let Some(state) = input.line.take() else {
                        return;
                    };
                    screen.write_text("\n");
                    screen.commit(wm);
                    let reply = Message::new(proto::reply_kind(proto::CONSOLE_READ_LINE))
                        .with_payload(state.line.as_bytes());
                    let _ = client.reply(&state.request, reply);
                }
                0x08 => {
                    if state.line.pop().is_some() {
                        screen.backspace();
                        screen.commit(wm);
                    }
                }
                byte => {
                    state.line.push(byte as char);
                    screen.write_text(core::str::from_utf8(&[byte]).unwrap_or("?"));
                    screen.commit(wm);
                }
            }
        }
        wm_event::FOCUS_IN | wm_event::FOCUS_OUT | wm_event::KEY => {}
        // The pointer is delivered, and the console has nothing to do with it:
        // there is no scrollback to drag and no selection to make, and a text
        // service that moved its own caret to wherever a click landed would
        // fight the line editor the shell is typing into. A client that wants
        // the pointer reads the same events on the same channel.
        wm_event::POINTER_MOTION | wm_event::POINTER_BUTTON => {}
        // The display broker replaced the surface under us -- the window was
        // maximized or restored. Its new memory object travels in the message;
        // rebuild the canvas and text grid around it, then repaint. The text
        // that was on screen is gone with the old surface, which is the honest
        // outcome of resizing a terminal with no scrollback.
        wm_event::CONFIGURE => {
            let Some(handle) = message.handles.into_iter().next() else {
                return;
            };
            let width = event.width;
            let height = event.height;
            let pitch = width * 4;
            let size = pitch as u64 * height as u64;
            let vmo = Vmo::from_handle(handle, size);
            let Ok(mapping) = vmo.map(
                0,
                size,
                MapFlags::READ
                    .or(MapFlags::WRITE)
                    .or(MapFlags::SHARED)
                    .or(MapFlags::POPULATE),
                MapWindow::Shared,
            ) else {
                println!("console: failed to map the resized surface; exiting");
                libnode::exit(1);
            };
            // SAFETY: the mapping covers the whole object and nothing else here
            // maps it.
            unsafe { screen.replace(vmo, mapping, width, height, pitch) };
            screen.clear();

            screen.commit(wm);
            println!("console: resized to {width}x{height}");
        }
        wm_event::CLOSE => libnode::exit(0),
        other => println!("console: unknown display broker event kind {other}"),
    }
}

// ---------------------------------------------------------------------------
// The console protocol, unchanged
// ---------------------------------------------------------------------------

fn handle_request(
    screen: &mut Surface,
    wm: &Channel,
    client: &Channel,
    message: Message,
    input: &mut Input,
) {
    match message.kind {
        proto::CONSOLE_READ_KEY => {
            if let Some(key) = input.queue.pop_front() {
                let reply = Message::new(proto::reply_kind(message.kind))
                    .with_payload(proto::Encoded::new(key).bytes());
                let _ = client.reply(&message, reply);
            } else {
                // Held open until a key arrives; `handle_event` replies.
                input.key = Some(message);
            }
        }
        proto::CONSOLE_GET_CURSOR => {
            let (col, row) = screen.cursor();
            let cursor = proto::Encoded::new(proto::ConsoleCursor { col, row });
            let reply = Message::new(proto::reply_kind(message.kind)).with_payload(cursor.bytes());
            let _ = client.reply(&message, reply);
        }
        proto::CONSOLE_GET_INFO => {
            let info = proto::Encoded::new(proto::ConsoleInfo {
                width_px: screen.width(),
                height_px: screen.height(),
                cols: screen.cols(),
                rows: screen.rows(),
            });
            let reply = Message::new(proto::reply_kind(message.kind)).with_payload(info.bytes());
            let _ = client.reply(&message, reply);
        }
        proto::CONSOLE_SET_FONT => {
            let id = message
                .payload
                .get(..4)
                .map(|b| u32::from_le_bytes(b.try_into().unwrap()));
            if id.is_some_and(|id| screen.set_font(id)) {
                screen.commit(wm);
                let info = proto::Encoded::new(proto::ConsoleInfo {
                    width_px: screen.width(),
                    height_px: screen.height(),
                    cols: screen.cols(),
                    rows: screen.rows(),
                });
                let _ = client.reply(
                    &message,
                    Message::new(proto::reply_kind(message.kind)).with_payload(info.bytes()),
                );
            } else {
                let mut reply = Message::new(proto::reply_kind(message.kind))
                    .with_payload(&proto::encode_error(libnode::Error::InvalidArgument));
                reply.flags = MessageFlags::ERROR;
                let _ = client.reply(&message, reply);
            }
        }
        proto::CONSOLE_CLEAR => {
            screen.clear();
            screen.commit(wm);
            let _ = client.reply(&message, Message::new(proto::reply_kind(message.kind)));
        }
        proto::CONSOLE_WRITE_TEXT => {
            if let Some(text) = message.text() {
                screen.write_text(text);
                screen.commit(wm);
            }
            let _ = client.reply(&message, Message::new(proto::reply_kind(message.kind)));
        }
        proto::CONSOLE_SET_CURSOR => {
            if let Some(cursor) = proto::decode::<proto::ConsoleCursor>(&message.payload) {
                screen.set_cursor(cursor.col, cursor.row);
                screen.commit(wm);
            }
            let _ = client.reply(&message, Message::new(proto::reply_kind(message.kind)));
        }
        proto::CONSOLE_READ_LINE => {
            // Held open, not replied to, until Enter arrives from the
            // display broker side of `serve`'s loop.
            input.line = Some(PendingReadLine {
                request: message,
                line: alloc::string::String::new(),
            });
        }
        other => {
            println!("console: unknown request kind {other:#x}");
            let mut reply = Message::new(proto::reply_kind(other));
            reply.flags = MessageFlags::ERROR;
            reply.payload =
                alloc::vec::Vec::from(proto::encode_error(libnode::Error::InvalidArgument));
            let _ = client.reply(&message, reply);
        }
    }
}
