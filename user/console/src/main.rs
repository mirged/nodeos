//! `console` -- the text and graphics service.
//!
//! Once the owner of the framebuffer and the keyboard; now a client of
//! `compositor` like any other. What it draws goes into a memory object the
//! compositor allocated for its window, and the keystrokes it reads arrive as
//! `WM_EVENT` messages on the same channel it draws over. It holds no device
//! capability at all, which is why it and `doom` can now be on screen at the
//! same time.
//!
//! Its own protocol -- `abi::proto`'s `CONSOLE_*` kinds, which `shell` speaks
//! -- did not change. A client of the console never knew whether it was
//! talking to something that owned a screen or something that owned a window,
//! and that is the property that made this substitution cost nothing above.

#![no_std]
#![no_main]

extern crate alloc;

mod surface;

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

/// Content size the console asks the compositor for. The compositor decides
/// what it actually gets -- the request is a statement of what would be
/// comfortable, not a demand.
const WANTED_WIDTH: u32 = 512;
/// See [`WANTED_WIDTH`].
const WANTED_HEIGHT: u32 = 768;

fn run(mut env: Env) -> i32 {
    let Some(wm_handle) = env.take_extra(0) else {
        println!("console: no compositor connection granted; exiting");
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
    screen.write_text("NodeOS console online.\n");
    screen.commit(&wm);

    serve(&mut screen, &wm, &bootstrap);
    0
}

/// Ask the compositor for a window, and map the memory object it replies with.
fn create_surface(wm: &Channel) -> Option<Surface> {
    let request = proto::Encoded::new(proto::WmCreateSurface {
        width: WANTED_WIDTH,
        height: WANTED_HEIGHT,
        flags: 0,
        _reserved: 0,
    });
    let mut payload = alloc::vec::Vec::from(request.bytes());
    payload.extend_from_slice(b"console");

    let Ok(mut reply) = wm.call(Message::new(proto::WM_CREATE_SURFACE).with_payload(&payload)) else {
        println!("console: the compositor refused a surface; exiting");
        return None;
    };
    if reply.flags.contains(MessageFlags::ERROR) || reply.handles.is_empty() {
        println!("console: no surface granted ({:?}); exiting", proto::decode_error(&reply.payload));
        return None;
    }
    let info = proto::decode::<proto::WmSurfaceInfo>(&reply.payload)?;

    let vmo = Vmo::from_handle(reply.handles.remove(0), info.size);
    // Mapped eagerly and shared: the compositor holds the other mapping of
    // these pages and reads them whenever this process commits.
    let Ok(mapping) = vmo.map(
        0,
        info.size,
        MapFlags::READ.or(MapFlags::WRITE).or(MapFlags::SHARED).or(MapFlags::POPULATE),
        MapWindow::Shared,
    ) else {
        println!("console: failed to map the surface; exiting");
        return None;
    };

    println!("console: {}x{} window, {} cols", info.width, info.height, info.width / 8);
    // SAFETY: the mapping covers the whole object, whose size the compositor
    // reported as `pitch * height`, and nothing else here maps it.
    Some(unsafe { Surface::new(vmo, mapping, &info) })
}

/// A `CONSOLE_READ_LINE` request the client is waiting on, plus what has been
/// typed so far.
struct PendingReadLine {
    request: Message,
    line: alloc::string::String,
}

fn serve(screen: &mut Surface, wm: &Channel, bootstrap: &Channel) {
    let mut pending: Option<PendingReadLine> = None;
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

        let Ok((index, _)) = wait_any(&items, 0) else { continue };

        match index {
            0 => {
                if let Ok(message) = bootstrap.try_receive() {
                    println!("console: DEBUG bootstrap kind {:#x} handles {}", message.kind, message.handles.len());
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
                    handle_event(screen, wm, message, client.as_ref(), &mut pending);
                }
            }
            _ => {
                let Some(channel) = client.as_ref() else { continue };
                let Ok(message) = channel.try_receive() else { continue };
                handle_request(screen, wm, channel, message, &mut pending);
            }
        }
    }
}

// ---------------------------------------------------------------------------
// Input, from the compositor
// ---------------------------------------------------------------------------

fn handle_event(
    screen: &mut Surface,
    wm: &Channel,
    message: Message,
    client: Option<&Channel>,
    pending: &mut Option<PendingReadLine>,
) {
    if message.kind != proto::WM_EVENT {
        return;
    }
    let Some(event) = proto::decode::<proto::WmEvent>(&message.payload) else { return };
    match event.kind {
        wm_event::KEY if event.pressed != 0 => {
            // Only a pending `CONSOLE_READ_LINE` gives a keystroke anywhere to
            // go: the console has no scrollback and nothing else to type at.
            let Some(state) = pending.as_mut() else { return };
            match event.ascii {
                0 => {}
                b'\n' => {
                    let Some(client) = client else { return };
                    let Some(state) = pending.take() else { return };
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
        wm_event::CLOSE => libnode::exit(0),
        other => println!("console: unknown compositor event kind {other}"),
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
    pending: &mut Option<PendingReadLine>,
) {
    println!("console: DEBUG request {:#x}", message.kind);
    match message.kind {
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
            }
            let _ = client.reply(&message, Message::new(proto::reply_kind(message.kind)));
        }
        proto::CONSOLE_FILL_RECT => {
            if let Some(rect) = proto::decode::<proto::ConsoleFillRect>(&message.payload) {
                screen.fill_rect(rect.x, rect.y, rect.w, rect.h, rect.color);
                screen.commit(wm);
            }
            let _ = client.reply(&message, Message::new(proto::reply_kind(message.kind)));
        }
        proto::CONSOLE_DRAW_LINE => {
            if let Some(line) = proto::decode::<proto::ConsoleDrawLine>(&message.payload) {
                screen.draw_line(line.x0, line.y0, line.x1, line.y1, line.color);
                screen.commit(wm);
            }
            let _ = client.reply(&message, Message::new(proto::reply_kind(message.kind)));
        }
        proto::CONSOLE_READ_LINE => {
            // Held open, not replied to, until Enter arrives from the
            // compositor side of `serve`'s loop.
            *pending = Some(PendingReadLine { request: message, line: alloc::string::String::new() });
        }
        other => {
            println!("console: unknown request kind {other:#x}");
            let mut reply = Message::new(proto::reply_kind(other));
            reply.flags = MessageFlags::ERROR;
            reply.payload = alloc::vec::Vec::from(proto::encode_error(libnode::Error::InvalidArgument));
            let _ = client.reply(&message, reply);
        }
    }
}
