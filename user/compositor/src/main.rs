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
//! See `abi::proto`'s `WM_*` message kinds for the protocol.

#![no_std]
#![no_main]

extern crate alloc;

mod keyboard;
mod window;

use alloc::string::{String, ToString};
use alloc::vec::Vec;

use libdraw::Canvas;
use libnode::abi::message::MessageFlags;
use libnode::abi::proto::{self, wm_event};
use libnode::abi::types::{MapFlags, MapWindow};
use libnode::abi::{Rights, Signals};
use libnode::device::Device;
use libnode::mem::Vmo;
use libnode::wait::wait_any;
use libnode::{println, Channel, Env, Error, Message};

use keyboard::{Decoder, KeyEvent};
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
    };

    for slot in 0..MAX_WINDOWS {
        window::draw_empty(&mut compositor.screen, &compositor.layout, slot);
    }
    println!(
        "compositor: {}x{} screen, {} columns of {}x{}",
        info.width,
        info.height,
        MAX_WINDOWS,
        compositor.layout.content(0).w,
        compositor.layout.content(0).h
    );

    serve(&mut compositor, &bootstrap, &kbd_device);
    0
}

fn serve(compositor: &mut Compositor, bootstrap: &Channel, keyboard: &Device) {
    let mut decoder = Decoder::new();

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

        let Ok((index, observed)) = wait_any(&items, 0) else { continue };

        if index == 0 {
            accept_client(compositor, bootstrap);
        } else if index == keyboard_index {
            drain_keyboard(compositor, keyboard, &mut decoder);
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
    let content = compositor.layout.maximized_content();
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
    let source = &compositor.windows[index].surface.canvas;
    // `blit` clips to both canvases, and the surface is exactly the size of
    // the content area, so a damage rectangle that overruns the surface is
    // trimmed to the window rather than into the neighbouring column.
    compositor.screen.blit(
        content.x + damage.x.min(content.w),
        content.y + damage.y.min(content.h),
        source,
        damage.x,
        damage.y,
        damage.w,
        damage.h,
    );

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
    window::draw_frame(
        &mut compositor.screen,
        &compositor.layout,
        &compositor.windows[index],
        focused,
        compositor.maximized == Some(index),
    );

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
            width: 0,
            height: 0,
        },
    );
}

fn cycle_focus(compositor: &mut Compositor) {
    if compositor.windows.is_empty() {
        compositor.focus = None;
        return;
    }
    let next = match compositor.focus {
        Some(current) => (current + 1) % compositor.windows.len(),
        None => 0,
    };
    if compositor.focus == Some(next) {
        return;
    }

    compositor.maximized = None;

    if let Some(previous) = compositor.focus {
        notify_focus(compositor, previous, wm_event::FOCUS_OUT);
    }
    compositor.focus = Some(next);
    notify_focus(compositor, next, wm_event::FOCUS_IN);

    repaint(compositor);
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
// Painting
// ---------------------------------------------------------------------------

/// Redraw every frame and every window's last committed pixels.
///
/// Called when the set of windows or the focus changes -- not per commit,
/// which only touches the damaged rectangle. The compositor keeps no copy of a
/// client's pixels beyond the surface itself, so "repaint" really is a re-blit
/// of what each client last put there.
fn repaint(compositor: &mut Compositor) {
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
        compositor.screen.blit(content.x, content.y, source, 0, 0, content.w, content.h);
    }
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

