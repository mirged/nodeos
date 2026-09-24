//! `compositor` -- the display server.
//!
//! Holds the two device capabilities the boot launcher grants under
//! `CapClass::Display`/`CapClass::Input`: the framebuffer and the PS/2
//! keyboard. It is now the only process in NodeOS that does, which is the
//! whole point of it existing.
//!
//! Before this service, a program that wanted to draw was handed the
//! framebuffer device -- and with it the entire screen, every other program's
//! output included. `console` had it, and so `console` and `doom` could not
//! run at the same time; the boot configuration had to choose. A client of
//! this service is handed a memory object the size of its own window and a
//! channel, and that is the whole of its display authority. Two of them now
//! run side by side because neither can reach the other's pixels, not because
//! they agreed not to.
//!
//! It also holds the mouse, and with it the pointer: the arrow on the screen
//! and the window management a pointing device implies -- click to focus, drag
//! a title bar to move a window to the other column, double-click one to
//! maximize. See `pointer.rs` for the packets and the cursor itself.
//!
//! See `abi::proto`'s `WM_*` message kinds for the protocol.

#![no_std]
#![no_main]

extern crate alloc;

mod keyboard;
mod pointer;
mod window;

use alloc::string::{String, ToString};
use alloc::vec::Vec;

use libdraw::Canvas;
use libnode::abi::message::MessageFlags;
use libnode::abi::proto::{self, wm_button, wm_event};
use libnode::abi::types::{MapFlags, MapWindow};
use libnode::abi::{Rights, Signals};
use libnode::device::Device;
use libnode::mem::Vmo;
use libnode::wait::wait_any;
use libnode::{println, Channel, Env, Error, Message};

use keyboard::{Decoder, KeyEvent};
use pointer::{Motion, Pointer};
use window::{Layout, Surface, Window, MAX_WINDOWS};

libnode::main! {
    fn main(env: Env) -> i32 {
        run(env)
    }
}

/// One connected client, and the surfaces it has created.
struct Client {
    /// Stable identifier, so a window can name its owner without holding an
    /// index into a vector that shifts when a client disconnects.
    id: u32,
    channel: Channel,
    next_surface: u32,
}

/// How long two presses count as one double click, in nanoseconds.
///
/// Long enough for a hand that is not trying to be fast, short enough that two
/// deliberate clicks are not one gesture. X11's default is 400 ms and this is
/// the same number because the value is about human hands, not about this
/// system.
const DOUBLE_CLICK_NS: u64 = 400 * 1_000_000;

/// A window being dragged by its title bar.
#[derive(Clone, Copy)]
struct Drag {
    /// The window, named by identity rather than by index: `windows` is a
    /// vector, and indices move under it whenever a client disconnects.
    window: (u32, u32),
    /// Where in the title bar the pointer grabbed it, so that the window does
    /// not jump to put its own left edge under the cursor on the first motion.
    grab_x: i32,
}

/// Everything the compositor owns.
struct Compositor {
    screen: Canvas,
    layout: Layout,
    clients: Vec<Client>,
    windows: Vec<Window>,
    /// Index into `windows` of the window holding the keyboard focus.
    focus: Option<usize>,
    maximized: Option<usize>,
    meta_used: bool,
    next_client_id: u32,
    /// The pointer and the arrow it draws, which is painted over everything
    /// else on the screen.
    pointer: Pointer,
    /// A title-bar drag in progress, if any.
    drag: Option<Drag>,
    /// The window that received the last button press, by identity.
    ///
    /// Motion and the matching release keep going to it after the pointer has
    /// left it. That is the implicit grab, and it is a compositor decision
    /// rather than a protocol one: a client is never told the pointer left,
    /// only where it went, which is what lets a client follow a drag out of
    /// its own window without any grab handshake.
    grab: Option<(u32, u32)>,
    /// When and where the last press landed, for double-click. One press
    /// remembered is enough to recognise a pair; a queue of clicks would only
    /// be needed by a client that wanted triples.
    last_press: Option<(u64, (u32, u32))>,
}

fn run(mut env: Env) -> i32 {
    let Some(fb_handle) = env.take_extra(0) else {
        println!("compositor: no framebuffer device granted; exiting");
        return 1;
    };
    let Some(kbd_handle) = env.take_extra(0) else {
        println!("compositor: no keyboard device granted; exiting");
        return 1;
    };
    // The mouse is optional in the way the filesystem is optional to the
    // shell: a machine with no mouse still gets a window system, and the log
    // says why there is no cursor on it rather than the service refusing to
    // start over a device it can live without.
    let pointer_device = env.take_extra(0).map(Device::from_handle);
    let mouse_present = pointer_device
        .as_ref()
        .and_then(|device| device.info().ok())
        .is_some_and(|info| info.present != 0);
    if pointer_device.is_none() {
        println!("compositor: no pointer device granted; continuing without a cursor");
    }

    // Check the packet decoder before it is trusted with a single real packet.
    // Nothing downstream can tell a misread packet from a hand that moved: the
    // pointer simply goes somewhere else, silently, and the mistake is
    // invisible in the only place it shows. A display served by a decoder that
    // cannot read its own test vectors is worse than no display.
    let (vectors_passed, vectors_failed) = pointer::selftest();
    println!("compositor: pointer decoder: {vectors_passed} vectors passed, {vectors_failed} failed");
    if vectors_failed > 0 {
        println!("compositor: refusing to serve a display with a broken packet decoder");
        return 1;
    }
    let Some(bootstrap) = env.take_bootstrap() else {
        println!("compositor: no bootstrap channel; exiting");
        return 1;
    };

    let fb_device = Device::from_handle(fb_handle);
    let kbd_device = Device::from_handle(kbd_handle);

    let Ok(info) = fb_device.info() else {
        println!("compositor: device_info failed for the framebuffer; exiting");
        return 1;
    };
    if info.bpp != 32 {
        println!("compositor: framebuffer is {} bits per pixel, not 32; exiting", info.bpp);
        return 1;
    }
    let len = info.pitch as u64 * info.height as u64;
    let Ok(mapping) = fb_device.map(len, MapFlags::WRITE.or(MapFlags::DEVICE)) else {
        println!("compositor: failed to map the framebuffer; exiting");
        return 1;
    };

    // SAFETY: the mapping covers `pitch * height` bytes of the framebuffer,
    // and this process is the only holder of the display device, so nothing
    // else writes it. `mapping` is leaked below so it outlives the canvas.
    let screen = unsafe { Canvas::from_raw(mapping.base() as *mut u8, info.width, info.height, info.pitch) };
    // The framebuffer mapping lives as long as the process; forgetting it
    // rather than threading it through `Compositor` keeps the canvas's
    // lifetime story simple, and there is nothing to unmap it for.
    core::mem::forget(mapping);

    let mut compositor = Compositor {
        screen,
        layout: Layout::new(info.width, info.height),
        clients: Vec::new(),
        windows: Vec::new(),
        focus: None,
        maximized: None,
        meta_used: false,
        next_client_id: 1,
        pointer: Pointer::new(info.width, info.height, mouse_present),
        drag: None,
        grab: None,
        last_press: None,
    };

    // Through `repaint` rather than a loop of `draw_empty` calls, so that the
    // desktop is painted the same way it will be repainted from here on --
    // including the bracket that puts the arrow on the screen, which is why
    // the cursor is visible from the first frame rather than from the first
    // mouse movement.
    repaint(&mut compositor);
    println!(
        "compositor: {}x{} screen, {} columns of {}x{}",
        info.width,
        info.height,
        MAX_WINDOWS,
        compositor.layout.content(0).w,
        compositor.layout.content(0).h
    );

    let (pointer_x, pointer_y) = compositor.pointer.position();
    if mouse_present {
        println!("compositor: pointer ready at {pointer_x},{pointer_y}");
    } else {
        println!("compositor: no PS/2 mouse; the pointer is disabled");
    }

    serve(&mut compositor, &bootstrap, &kbd_device, pointer_device.as_ref());
    0
}

fn serve(compositor: &mut Compositor, bootstrap: &Channel, keyboard: &Device, mouse: Option<&Device>) {
    let mut decoder = Decoder::new();
    let mut pointer_decoder = pointer::Decoder::new();
    // The compositor starts serving long after boot, so the first packet has a
    // gap in front of it by definition; starting the clock at zero would make
    // that first read look idle, which is harmless, and starting it at now is
    // exact. Either way the reset is a no-op on a decoder with no packet in
    // progress.
    let mut pointer_last_packet = libnode::clock_ns();

    loop {
        // Rebuilt every iteration because the client list changes underneath
        // it. The three groups are laid out so the woken index decodes
        // without a lookup table: bootstrap, then one slot per client, then
        // the keyboard.
        let mut items = Vec::with_capacity(compositor.clients.len() + 2);
        items.push((bootstrap.handle().raw(), Signals::READABLE));
        for client in &compositor.clients {
            items.push((client.channel.handle().raw(), Signals::READABLE.or(Signals::PEER_CLOSED)));
        }
        let keyboard_index = items.len();
        items.push((keyboard.handle().raw(), Signals::READABLE));
        let pointer_index = if let Some(device) = mouse {
            items.push((device.handle().raw(), Signals::READABLE));
            Some(items.len() - 1)
        } else {
            None
        };

        let Ok((index, observed)) = wait_any(&items, 0) else { continue };

        if index == 0 {
            accept_client(compositor, bootstrap);
        } else if index == keyboard_index {
            drain_keyboard(compositor, keyboard, &mut decoder);
        } else if pointer_index == Some(index) {
            if let Some(device) = mouse {
                drain_pointer(compositor, device, &mut pointer_decoder, &mut pointer_last_packet);
            }
        } else {
            let client_index = index - 1;
            if observed.contains(Signals::READABLE) {
                if let Ok(message) = compositor.clients[client_index].channel.try_receive() {
                    handle_request(compositor, client_index, message);
                }
            } else if observed.contains(Signals::PEER_CLOSED) {
                disconnect(compositor, client_index);
            }
        }
    }
}

// ---------------------------------------------------------------------------
// Connections
// ---------------------------------------------------------------------------

fn accept_client(compositor: &mut Compositor, bootstrap: &Channel) {
    let Ok(message) = bootstrap.try_receive() else { return };
    if message.kind != proto::CONNECT || message.handles.is_empty() {
        println!("compositor: ignoring bootstrap message of kind {:#x}", message.kind);
        return;
    }
    let mut handles = message.handles;
    let id = compositor.next_client_id;
    compositor.next_client_id += 1;
    compositor.clients.push(Client { id, channel: Channel::from_handle(handles.remove(0)), next_surface: 1 });
    println!("compositor: client {id} connected");
}

/// Drop a client and every window it owned.
///
/// This is the whole of the compositor's cleanup path, and it is short because
/// the kernel did the hard part: the client's handles were closed when its
/// process died, which is what asserted `PEER_CLOSED` and woke us here. There
/// is no reaping, no timeout and no way for a dead client to leave a window
/// behind, because the window's pixels were this process's memory object all
/// along.
fn disconnect(compositor: &mut Compositor, client_index: usize) {
    let id = compositor.clients[client_index].id;
    println!("compositor: client {id} disconnected");
    compositor.clients.remove(client_index);

    let focused_surface = compositor.focus.map(|i| (compositor.windows[i].client, compositor.windows[i].surface_id));
    let maximized_surface = compositor.maximized.map(|i| (compositor.windows[i].client, compositor.windows[i].surface_id));
    compositor.windows.retain(|w| w.client != id);

    compositor.maximized = maximized_surface
        .and_then(|key| compositor.windows.iter().position(|w| (w.client, w.surface_id) == key));

    // Indices into `windows` shifted; recover the focus by identity rather
    // than by position, and fall back to the first remaining window.
    compositor.focus = focused_surface
        .and_then(|key| compositor.windows.iter().position(|w| (w.client, w.surface_id) == key))
        .or(if compositor.windows.is_empty() { None } else { Some(0) });

    repaint(compositor);
}

// ---------------------------------------------------------------------------
// Requests
// ---------------------------------------------------------------------------

fn handle_request(compositor: &mut Compositor, client_index: usize, message: Message) {
    match message.kind {
        proto::WM_CREATE_SURFACE => create_surface(compositor, client_index, message),
        proto::WM_COMMIT => commit(compositor, client_index, message),
        proto::WM_SET_TITLE => set_title(compositor, client_index, message),
        proto::WM_DESTROY_SURFACE => destroy_surface(compositor, client_index, message),
        other => {
            println!("compositor: unknown request kind {other:#x}");
            reply_error(&compositor.clients[client_index].channel, &message, Error::InvalidArgument);
        }
    }
}

fn create_surface(compositor: &mut Compositor, client_index: usize, message: Message) {
    let Some(request) = proto::decode::<proto::WmCreateSurface>(&message.payload) else {
        reply_error(&compositor.clients[client_index].channel, &message, Error::InvalidArgument);
        return;
    };
    let title = message
        .payload
        .get(core::mem::size_of::<proto::WmCreateSurface>()..)
        .and_then(|bytes| core::str::from_utf8(bytes).ok())
        .unwrap_or("")
        .to_string();

    let Some(slot) = free_slot(compositor) else {
        println!("compositor: no free column for a new surface");
        reply_error(&compositor.clients[client_index].channel, &message, Error::LimitExceeded);
        return;
    };

    // The requested size is advisory: a column is the only size this layout
    // has to offer, and a surface that is not the size of its column would
    // leave a strip of the window showing whatever was underneath. What the
    // client asked for is still worth checking, because a client that cannot
    // work at this size should hear about it now rather than draw garbage.
    //
    // The column's own size, not the maximized one: a surface allocated at the
    // width of the whole screen is wider than the window it belongs to, and
    // every commit from that client would then paint across the neighbouring
    // column. Maximizing a window does not resize it -- a surface is mapped
    // once, and the protocol has no way to replace one under a running client
    // -- so the extra width would only ever be spent on its neighbour.
    let content = compositor.layout.content(slot);
    if request.width > content.w || request.height > content.h {
        println!(
            "compositor: client asked for {}x{}, larger than the {}x{} column",
            request.width, request.height, content.w, content.h
        );
    }

    let pitch = content.w * 4;
    let size = pitch as u64 * content.h as u64;

    let Ok(vmo) = Vmo::create(size, false) else {
        reply_error(&compositor.clients[client_index].channel, &message, Error::NoMemory);
        return;
    };
    // Mapped eagerly: the alternative is that the first fault on each page
    // happens in whichever of the two processes touches it first, and a
    // shared buffer whose backing depends on the order two processes run is
    // a race waiting for a slower machine.
    let Ok(mapping) = vmo.map(
        0,
        size,
        MapFlags::READ.or(MapFlags::WRITE).or(MapFlags::SHARED).or(MapFlags::POPULATE),
        MapWindow::Shared,
    ) else {
        reply_error(&compositor.clients[client_index].channel, &message, Error::NoSpace);
        return;
    };

    // The client gets a handle that can map and write the buffer, and nothing
    // else -- notably not `DESTROY`, so it cannot pull its own window's pixels
    // out from under the compositor mid-blit.
    let client_rights = Rights::DUPLICATE
        .or(Rights::TRANSFER)
        .or(Rights::READ)
        .or(Rights::WRITE)
        .or(Rights::MAP)
        .or(Rights::GET_PROPERTY)
        .or(Rights::WAIT);
    let Ok(client_handle) = vmo.handle().duplicate(client_rights) else {
        reply_error(&compositor.clients[client_index].channel, &message, Error::AccessDenied);
        return;
    };

    // SAFETY: the mapping was just made over the whole object, which is
    // `pitch * content.h` bytes, and no other mapping of it exists here.
    let mut surface = unsafe { Surface::new(vmo, mapping, content.w, content.h, pitch) };
    // A freshly created memory object is zero-filled, which is black; being
    // explicit costs one pass and means the window is never showing a
    // half-drawn client's first frame over an assumption.
    surface.canvas.clear(0x0000_0000);

    let client = &mut compositor.clients[client_index];
    let surface_id = client.next_surface;
    client.next_surface += 1;
    let client_id = client.id;

    compositor.windows.push(Window {
        client: client_id,
        surface_id,
        slot,
        title: if title.is_empty() { String::from("window") } else { title },
        surface,
    });
    if compositor.focus.is_none() {
        compositor.focus = Some(compositor.windows.len() - 1);
    }
    repaint(compositor);

    let info = proto::Encoded::new(proto::WmSurfaceInfo {
        surface: surface_id,
        width: content.w,
        height: content.h,
        pitch,
        size,
    });
    let reply = Message::new(proto::reply_kind(proto::WM_CREATE_SURFACE))
        .with_payload(info.bytes())
        .with_handle(client_handle);
    let _ = compositor.clients[client_index].channel.reply(&message, reply);

    // Sent after the reply so the client learns it has the focus only once it
    // has a surface identifier to attach the event to.
    if compositor.focus == Some(compositor.windows.len() - 1) {
        send_event(
            &compositor.clients[client_index].channel,
            proto::WmEvent { surface: surface_id, kind: wm_event::FOCUS_IN, ..Default::default() },
        );
    }
    println!("compositor: client {client_id} surface {surface_id} in column {slot}, {}x{}", content.w, content.h);
}

fn commit(compositor: &mut Compositor, client_index: usize, message: Message) {
    let Some(damage) = proto::decode::<proto::WmDamage>(&message.payload) else {
        reply_error(&compositor.clients[client_index].channel, &message, Error::InvalidArgument);
        return;
    };
    let client_id = compositor.clients[client_index].id;
    let Some(index) = find_window(compositor, client_id, damage.surface) else {
        reply_error(&compositor.clients[client_index].channel, &message, Error::NotFound);
        return;
    };

    //println!("compositor: DEBUG commit c{} s{} slot {} rect {},{} {}x{}", client_id, damage.surface, compositor.windows[index].slot, damage.x, damage.y, damage.w, damage.h);
    let content = compositor.layout.content_for(
        compositor.windows[index].slot,
        compositor.maximized == Some(index),
    );
    // The arrow comes off before the blit and goes back on after it: the
    // damaged rectangle can be anywhere, including under the cursor.
    compositor.uncover_pointer();
    let source = &compositor.windows[index].surface.canvas;
    // `blit` clips to the canvas, and the canvas is the whole screen: a damage
    // rectangle that overruns the window would be trimmed by the edge of the
    // display rather than by the edge of the window, and would paint over the
    // column next door. So the rectangle is trimmed here, where the window's
    // geometry is known, on both axes and against both the content area and
    // the surface it is being read from.
    let x = damage.x.min(content.w);
    let y = damage.y.min(content.h);
    let w = damage.w.min(content.w - x).min(source.width().saturating_sub(x));
    let h = damage.h.min(content.h - y).min(source.height().saturating_sub(y));
    compositor.screen.blit(content.x + x, content.y + y, source, x, y, w, h);
    compositor.paint_pointer();

    let _ = compositor.clients[client_index]
        .channel
        .reply(&message, Message::new(proto::reply_kind(proto::WM_COMMIT)));
}

fn set_title(compositor: &mut Compositor, client_index: usize, message: Message) {
    let Some(target) = proto::decode::<proto::WmSurfaceRef>(&message.payload) else {
        reply_error(&compositor.clients[client_index].channel, &message, Error::InvalidArgument);
        return;
    };
    let client_id = compositor.clients[client_index].id;
    let Some(index) = find_window(compositor, client_id, target.surface) else {
        reply_error(&compositor.clients[client_index].channel, &message, Error::NotFound);
        return;
    };

    let title = message
        .payload
        .get(core::mem::size_of::<proto::WmSurfaceRef>()..)
        .and_then(|bytes| core::str::from_utf8(bytes).ok())
        .unwrap_or("");
    compositor.windows[index].title = title.to_string();

    let focused = compositor.focus == Some(index);
    compositor.uncover_pointer();
    window::draw_frame(
        &mut compositor.screen,
        &compositor.layout,
        &compositor.windows[index],
        focused,
        compositor.maximized == Some(index),
    );
    compositor.paint_pointer();

    let _ = compositor.clients[client_index]
        .channel
        .reply(&message, Message::new(proto::reply_kind(proto::WM_SET_TITLE)));
}

fn destroy_surface(compositor: &mut Compositor, client_index: usize, message: Message) {
    let Some(target) = proto::decode::<proto::WmSurfaceRef>(&message.payload) else {
        reply_error(&compositor.clients[client_index].channel, &message, Error::InvalidArgument);
        return;
    };
    let client_id = compositor.clients[client_index].id;
    let Some(index) = find_window(compositor, client_id, target.surface) else {
        reply_error(&compositor.clients[client_index].channel, &message, Error::NotFound);
        return;
    };

    compositor.windows.remove(index);
    compositor.maximized = match compositor.maximized {
        Some(maximized) if maximized == index => None,
        Some(maximized) if maximized > index => Some(maximized - 1),
        other => other,
    };
    compositor.focus = match compositor.focus {
        _ if compositor.windows.is_empty() => None,
        Some(focused) if focused > index => Some(focused - 1),
        Some(focused) if focused == index => Some(index.min(compositor.windows.len() - 1)),
        other => other,
    };
    repaint(compositor);

    let _ = compositor.clients[client_index]
        .channel
        .reply(&message, Message::new(proto::reply_kind(proto::WM_DESTROY_SURFACE)));
}

// ---------------------------------------------------------------------------
// Input
// ---------------------------------------------------------------------------

fn drain_keyboard(compositor: &mut Compositor, keyboard: &Device, decoder: &mut Decoder) {
    let mut bytes = [0u8; 32];
    let Ok(count) = keyboard.read(&mut bytes) else { return };
    for byte in &bytes[..count] {
        let Some(event) = decoder.feed(*byte) else { continue };
        if event.is_focus_hotkey() {
            cycle_focus(compositor);
            continue;
        }
        if event.is_meta_key() {
            if event.pressed {
                compositor.meta_used = false;
            } else if !compositor.meta_used {
                cycle_focus(compositor);
            }
            continue;
        }
        if event.pressed && event.modifiers & proto::wm_mod::META != 0 {
            compositor.meta_used = true;
            match event.scancode {
                0x1C => toggle_maximize(compositor),
                0x0F => cycle_focus(compositor),
                0x10 => close_focus(compositor),
                _ => {}
            }
            continue;
        }
        route_key(compositor, event);
    }
}

/// Deliver a key to the focused window's client, if there is one.
fn route_key(compositor: &Compositor, event: KeyEvent) {
    let Some(index) = compositor.focus else { return };
    let window = &compositor.windows[index];
    let Some(client) = compositor.clients.iter().find(|c| c.id == window.client) else { return };

    send_event(
        &client.channel,
        proto::WmEvent {
            surface: window.surface_id,
            kind: wm_event::KEY,
            scancode: event.scancode,
            extended: event.extended as u8,
            pressed: event.pressed as u8,
            ascii: event.ascii,
            modifiers: event.modifiers,
            // A key event names no position and no button; the pointer's own
            // events are the only ones that fill these in.
            ..Default::default()
        },
    );
}

fn cycle_focus(compositor: &mut Compositor) {
    if compositor.windows.is_empty() {
        set_focus(compositor, None);
        return;
    }
    let next = match compositor.focus {
        Some(current) => (current + 1) % compositor.windows.len(),
        None => 0,
    };
    if compositor.focus == Some(next) {
        return;
    }

    // Cycling away from a maximized window tiling the screen again is
    // deliberate: this shortcut is how a maximized window is escaped on a
    // machine with no pointer, so it must not leave the screen covered by a
    // window that no longer has the focus.
    compositor.maximized = None;
    set_focus(compositor, Some(next));
}

fn toggle_maximize(compositor: &mut Compositor) {
    let Some(index) = compositor.focus else { return };
    compositor.maximized = if compositor.maximized == Some(index) { None } else { Some(index) };
    repaint(compositor);
}

fn close_focus(compositor: &mut Compositor) {
    let Some(index) = compositor.focus else { return };
    let window = &compositor.windows[index];
    let Some(client) = compositor.clients.iter().find(|client| client.id == window.client) else { return };
    send_event(&client.channel, proto::WmEvent { surface: window.surface_id, kind: wm_event::CLOSE, ..Default::default() });
}

/// Move the keyboard focus, telling the window that lost it and the one that
/// gained it.
///
/// One function for both paths that move the focus -- the compositor's own
/// keyboard shortcut and a click -- because the two must not be able to
/// disagree about what a focus change involves: the events, the repaint and
/// the line this prints are the same whichever way it happened.
fn set_focus(compositor: &mut Compositor, focus: Option<usize>) {
    if compositor.focus == focus {
        return;
    }
    if let Some(previous) = compositor.focus {
        notify_focus(compositor, previous, wm_event::FOCUS_OUT);
    }
    compositor.focus = focus;
    match focus {
        Some(index) => {
            notify_focus(compositor, index, wm_event::FOCUS_IN);
            let window = &compositor.windows[index];
            println!("compositor: focus -> client {} surface {}", window.client, window.surface_id);
        }
        None => println!("compositor: focus dropped (nothing is under the pointer)"),
    }
    repaint(compositor);
}

fn notify_focus(compositor: &Compositor, index: usize, kind: u32) {
    let window = &compositor.windows[index];
    let Some(client) = compositor.clients.iter().find(|c| c.id == window.client) else { return };
    send_event(&client.channel, proto::WmEvent { surface: window.surface_id, kind, ..Default::default() });
}

/// Post an event to a client without blocking.
///
/// A client that has stopped reading its events must not be able to stall the
/// compositor -- and with it every other window -- so a full queue drops the
/// event rather than waiting for room. Losing a keystroke is a nuisance; a
/// display server that stops repainting because one program is wedged is the
/// failure this whole design exists to avoid.
fn send_event(channel: &Channel, event: proto::WmEvent) {
    let event = proto::Encoded::new(event);
    let mut message = Message::new(proto::WM_EVENT).with_payload(event.bytes());
    message.flags = MessageFlags::NON_BLOCKING;
    let _ = channel.send(message);
}


// ---------------------------------------------------------------------------
// The pointer
// ---------------------------------------------------------------------------

/// How long a silence from the mouse means a byte was lost, in nanoseconds.
///
/// A PS/2 mouse in the mode this driver leaves it in sends a packet every ten
/// milliseconds, so a tenth of a second of quiet is not a slow hand: it is the
/// kernel's input queue having dropped its oldest byte under a full queue,
/// which leaves the decoder reading deltas as flags from then on. The kernel
/// drops the oldest byte on purpose -- an interrupt handler may never block --
/// so this is the compositor's half of that bargain.
const RESYNC_NS: u64 = 100 * 1_000_000;

/// Take every packet the mouse has queued, and act on each in order.
fn drain_pointer(
    compositor: &mut Compositor,
    device: &Device,
    decoder: &mut pointer::Decoder,
    last_packet: &mut u64,
) {
    let now = libnode::clock_ns();
    if now.saturating_sub(*last_packet) > RESYNC_NS {
        decoder.reset();
    }
    *last_packet = now;

    let mut bytes = [0u8; 64];
    let Ok(count) = device.read(&mut bytes) else { return };
    for byte in &bytes[..count] {
        let Some(motion) = decoder.feed(*byte) else { continue };
        apply_motion(compositor, motion);
    }
    // The arrow goes back on last, because everything above may have written
    // the framebuffer; one painted earlier would be painted over by them.
    compositor.paint_pointer();
}

/// One decoded packet: move, notice any button that changed, and tell whoever
/// is interested.
fn apply_motion(compositor: &mut Compositor, motion: Motion) {
    let moved = motion.dx != 0 || motion.dy != 0;
    let (width, height) = (compositor.screen.width(), compositor.screen.height());
    compositor.pointer.move_by(motion.dx, motion.dy, width, height);
    follow_drag(compositor);

    // Button transitions are handled before this packet's movement is
    // delivered, so that a press which focuses a window has done so by the time
    // the motion that came with it reaches anybody.
    let changed = compositor.pointer.buttons ^ motion.buttons;
    compositor.pointer.buttons = motion.buttons;
    for button in [wm_button::LEFT, wm_button::RIGHT, wm_button::MIDDLE] {
        if changed & button != 0 {
            let pressed = motion.buttons & button != 0;
            apply_button(compositor, button, pressed);
        }
    }

    if moved {
        deliver_pointer_event(compositor, wm_event::POINTER_MOTION, 0, false);
    }
}

/// One button transition, and everything the window manager does about it.
fn apply_button(compositor: &mut Compositor, button: u32, pressed: bool) {
    if !pressed {
        // A release goes to the window that took the press, which is why the
        // grab is cleared after the event rather than before it.
        compositor.drag = None;
        deliver_pointer_event(compositor, wm_event::POINTER_BUTTON, button, false);
        compositor.grab = None;
        return;
    }

    let (x, y) = compositor.pointer.position();
    let Some(index) = window_at(compositor, x, y) else {
        // The desktop. The focus goes nowhere rather than staying where it was:
        // "which window is getting the keyboard" should have an answer a person
        // can see, and clicking the empty screen is how a window gives it up.
        // Nothing becomes unreachable this way -- the compositor's own shortcut
        // still cycles the remaining windows.
        set_focus(compositor, None);
        compositor.last_press = None;
        return;
    };

    set_focus(compositor, Some(index));
    let key = window_key(compositor, index);

    if button != wm_button::LEFT || !on_title_bar(compositor, index, x, y) {
        // A press anywhere but a title bar ends any pending double click: two
        // presses are one gesture only if they landed on the same bar. Note
        // where the clearing is *not*: a press that landed on the bar leaves
        // the previous press alone, because this press may be the second half
        // of a gesture that started with it.
        //
        // The window still takes the pointer, so a drag that starts inside a
        // window and leaves it keeps being delivered to that window.
        compositor.last_press = None;
        compositor.grab = Some(key);
        deliver_pointer_event(compositor, wm_event::POINTER_BUTTON, button, true);
        return;
    }

    if double_clicked(compositor, key) {
        // A double click toggles between tiled and maximized, and the drag the
        // first press armed is dropped with it: a double click is not a move
        // that happened to stall.
        let maximized = compositor.maximized != Some(index);
        set_maximized(compositor, index, maximized);
        compositor.drag = None;
        compositor.grab = None;
        deliver_pointer_event(compositor, wm_event::POINTER_BUTTON, button, true);
        return;
    }

    let frame = compositor.layout.frame_for(compositor.windows[index].slot, false);
    compositor.drag = Some(Drag { window: key, grab_x: x as i32 - frame.x as i32 });
    compositor.grab = Some(key);
    deliver_pointer_event(compositor, wm_event::POINTER_BUTTON, button, true);
}

/// Send one pointer event to whichever window it belongs to.
///
/// Motion goes to the window under the pointer unless a button is held: then it
/// goes to the window that took the press, which is the implicit grab and the
/// reason a client can follow a drag that has left its own window.
fn deliver_pointer_event(compositor: &Compositor, kind: u32, button: u32, pressed: bool) {
    let (x, y) = compositor.pointer.position();
    let target = compositor
        .grab
        .and_then(|(client, surface)| find_window(compositor, client, surface))
        .or_else(|| window_at(compositor, x, y));
    let Some(index) = target else { return };

    let (local_x, local_y) = content_point(compositor, index, x, y);
    let window = &compositor.windows[index];
    let Some(client) = compositor.clients.iter().find(|client| client.id == window.client) else { return };

    send_event(
        &client.channel,
        proto::WmEvent {
            surface: window.surface_id,
            kind,
            x: local_x,
            y: local_y,
            buttons: compositor.pointer.buttons,
            button,
            pressed: pressed as u8,
            ..Default::default()
        },
    );
}

/// Keep a dragged window under the pointer, in whichever column the pointer is.
fn follow_drag(compositor: &mut Compositor) {
    let Some(drag) = compositor.drag else { return };
    let Some(index) = find_window(compositor, drag.window.0, drag.window.1) else {
        // The window went away mid-drag: its client exited, and `disconnect`
        // has already taken the window with it.
        compositor.drag = None;
        return;
    };
    if compositor.maximized == Some(index) {
        // A maximized window is the whole screen, so there is no other column
        // to drag it to. Dropping the drag is better than leaving one armed
        // that would move the window the moment it was put back.
        compositor.drag = None;
        return;
    }

    let (x, _) = compositor.pointer.position();
    let slot = compositor.layout.slot_at(x);
    if slot != compositor.windows[index].slot {
        move_to_slot(compositor, index, slot);
    }
}

/// Give a window a different column, swapping with whatever holds that one.
///
/// A fixed set of columns has no third place for a window to go, so a drag that
/// takes an occupied column has to hand its own over. That is also what makes
/// the gesture reversible: drag the window back and the two trade places again,
/// which a scheme that only ever moved the dragged window could not do -- it
/// would need somewhere to park the other one.
fn move_to_slot(compositor: &mut Compositor, index: usize, slot: usize) {
    let current = compositor.windows[index].slot;
    if current == slot {
        return;
    }
    if let Some(displaced) = compositor.windows.iter().position(|window| window.slot == slot) {
        compositor.windows[displaced].slot = current;
    }
    compositor.windows[index].slot = slot;

    let window = &compositor.windows[index];
    println!(
        "compositor: pointer: client {} surface {} moved to column {slot}",
        window.client, window.surface_id
    );
    repaint(compositor);
}

/// Set a window's maximized state.
fn set_maximized(compositor: &mut Compositor, index: usize, maximized: bool) {
    let wanted = if maximized { Some(index) } else { None };
    if compositor.maximized == wanted {
        return;
    }
    compositor.maximized = wanted;

    let window = &compositor.windows[index];
    println!(
        "compositor: pointer: client {} surface {} {}",
        window.client,
        window.surface_id,
        if maximized { "maximized" } else { "restored" }
    );
    repaint(compositor);
}

/// Whether this press is the second of a double click on `key`.
///
/// Records the press either way, so the next one is measured against it: three
/// presses in a row are two double clicks and nothing else, which is what every
/// window system does with them.
fn double_clicked(compositor: &mut Compositor, key: (u32, u32)) -> bool {
    let now = libnode::clock_ns();
    let double = compositor
        .last_press
        .is_some_and(|(when, at)| at == key && now.saturating_sub(when) < DOUBLE_CLICK_NS);
    compositor.last_press = Some((now, key));
    double
}

/// The window under a screen point, if any.
///
/// A maximized window's frame is the whole screen, so it is under every point --
/// which is what makes a click on the "desktop" focus it rather than dropping
/// the focus, and why the desktop is clickable only when nothing is maximized.
fn window_at(compositor: &Compositor, x: u32, y: u32) -> Option<usize> {
    for (index, window) in compositor.windows.iter().enumerate() {
        let frame = compositor.layout.frame_for(window.slot, compositor.maximized == Some(index));
        if frame.contains(x, y) {
            return Some(index);
        }
    }
    None
}

/// Whether a screen point is in a window's title bar -- the only part of a
/// window that moves it.
fn on_title_bar(compositor: &Compositor, index: usize, x: u32, y: u32) -> bool {
    let frame = compositor.layout.frame_for(compositor.windows[index].slot, compositor.maximized == Some(index));
    y >= frame.y && y < frame.y + window::TITLE_BAR_H && x >= frame.x && x < frame.x + frame.w
}

/// A window's identity: whose client it is, and which surface of theirs.
fn window_key(compositor: &Compositor, index: usize) -> (u32, u32) {
    (compositor.windows[index].client, compositor.windows[index].surface_id)
}

/// Where a screen point is, in a window's content coordinates.
///
/// Deliberately not clamped to the content: a client holding the grab keeps
/// receiving motion after the pointer has left its window, and its coordinates
/// go negative on the way out rather than saturating at the edge. See
/// [`proto::WmEvent::x`].
fn content_point(compositor: &Compositor, index: usize, x: u32, y: u32) -> (i32, i32) {
    let content = compositor.layout.content_for(compositor.windows[index].slot, compositor.maximized == Some(index));
    (x as i32 - content.x as i32, y as i32 - content.y as i32)
}

// ---------------------------------------------------------------------------
// Painting
// ---------------------------------------------------------------------------

impl Compositor {
    /// Take the arrow off the screen so that what follows can write underneath
    /// it.
    ///
    /// Every function in this file that touches the framebuffer starts with this
    /// and ends with [`Compositor::paint_pointer`]. The pair is what an arrow
    /// drawn straight into the framebuffer costs: without it, a blit that landed
    /// under the cursor would be undone by the cursor's own saved pixels on the
    /// next move, and the screen would grow a trail of arrows wherever the
    /// pointer had been.
    fn uncover_pointer(&mut self) {
        self.pointer.uncover(&mut self.screen);
    }

    /// Put the arrow back on, saving whatever it covers.
    ///
    /// Idempotent in the useful direction: a second call with no `uncover` in
    /// between re-saves the arrow's own pixels and draws over them again, so a
    /// caller may bracket its own writes without knowing whether something
    /// above it already drew the cursor.
    fn paint_pointer(&mut self) {
        self.pointer.paint(&mut self.screen);
    }
}

/// Redraw every frame and every window's last committed pixels.
///
/// Called when the set of windows or the focus changes -- not per commit,
/// which only touches the damaged rectangle. The compositor keeps no copy of a
/// client's pixels beyond the surface itself, so "repaint" really is a re-blit
/// of what each client last put there.
fn repaint(compositor: &mut Compositor) {
    compositor.uncover_pointer();
    for slot in 0..MAX_WINDOWS {
        if !compositor.windows.iter().any(|w| w.slot == slot) {
            window::draw_empty(&mut compositor.screen, &compositor.layout, slot);
        }
    }
    for index in 0..compositor.windows.len() {
        let focused = compositor.focus == Some(index);
        window::draw_frame(
            &mut compositor.screen,
            &compositor.layout,
            &compositor.windows[index],
            focused,
            compositor.maximized == Some(index),
        );

        let content = compositor.layout.content_for(
            compositor.windows[index].slot,
            compositor.maximized == Some(index),
        );
        let source = &compositor.windows[index].surface.canvas;
        // A maximized window's frame is the whole screen, and its surface is
        // still the size of the column it was allocated in: a surface is mapped
        // once, and the protocol has no way to hand a running client a bigger
        // one (see `window`). So the content covers what it can and the rest is
        // painted as desktop, which is a limitation stated on the screen rather
        // than the previous frame's pixels left lying in the gap -- the
        // difference between "the window is not that big" and "the window looks
        // corrupted".
        let w = content.w.min(source.width());
        let h = content.h.min(source.height());
        compositor.screen.blit(content.x, content.y, source, 0, 0, w, h);
        if w < content.w {
            compositor.screen.fill_rect(content.x + w, content.y, content.w - w, content.h, window::DESKTOP);
        }
        if h < content.h {
            compositor.screen.fill_rect(content.x, content.y + h, content.w, content.h - h, window::DESKTOP);
        }
    }
    compositor.paint_pointer();
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

fn free_slot(compositor: &Compositor) -> Option<usize> {
    (0..MAX_WINDOWS).find(|slot| !compositor.windows.iter().any(|w| w.slot == *slot))
}

fn find_window(compositor: &Compositor, client: u32, surface: u32) -> Option<usize> {
    compositor.windows.iter().position(|w| w.client == client && w.surface_id == surface)
}

fn reply_error(channel: &Channel, request: &Message, status: Error) {
    let mut reply = Message::new(proto::reply_kind(request.kind));
    reply.flags = MessageFlags::ERROR;
    reply.payload = Vec::from(proto::encode_error(status));
    let _ = channel.reply(request, reply);
}

