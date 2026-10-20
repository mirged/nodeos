//! Fullscreen display/input broker. No desktop, windows, chrome or pointer.
//! The shell console owns the base surface; an explicitly launched program
//! temporarily takes the screen. Its exit restores the console's pixels.
#![no_std]
#![no_main]
extern crate alloc;
mod keyboard;
use alloc::{string::String, vec::Vec};
use libdraw::Canvas;
use libnode::abi::{
    message::MessageFlags,
    proto::{self, wm_event, wm_mod},
    types::{MapFlags, MapWindow},
    Rights, Signals,
};
use libnode::device::Device;
use libnode::mem::{Mapping, Vmo};
use libnode::wait::wait_any;
use libnode::{println, Channel, Env, Error, Message};
libnode::main! { fn main(env: Env) -> i32 { run(env) } }
struct Client {
    id: u32,
    channel: Channel,
    next: u32,
}
struct Surface {
    client: u32,
    id: u32,
    title: String,
    canvas: Canvas,
    _mapping: Mapping,
    _vmo: Vmo,
}
struct Display {
    screen: Canvas,
    clients: Vec<Client>,
    surfaces: Vec<Surface>,
    focus: Option<usize>,
    next_client: u32,
    meta_used: bool,
}
fn run(mut env: Env) -> i32 {
    let Some(fb) = env.take_extra(0).map(Device::from_handle) else {
        return 1;
    };
    let Some(keyboard) = env.take_extra(0).map(Device::from_handle) else {
        return 1;
    };
    let Some(control) = env.take_bootstrap() else {
        return 1;
    };
    let Ok(info) = fb.info() else { return 1 };
    if info.bpp != 32 {
        return 1;
    }
    let Ok(mapping) = fb.map(
        info.pitch as u64 * info.height as u64,
        MapFlags::WRITE.or(MapFlags::DEVICE),
    ) else {
        return 1;
    };
    // SAFETY: this broker alone owns the framebuffer mapping for its lifetime.
    let screen = unsafe {
        Canvas::from_raw(
            mapping.base() as *mut u8,
            info.width,
            info.height,
            info.pitch,
        )
    };
    let mut display = Display {
        screen,
        clients: Vec::new(),
        surfaces: Vec::new(),
        focus: None,
        next_client: 1,
        meta_used: false,
    };
    display.screen.clear(0x101010);
    println!(
        "display: fullscreen {}x{}, keyboard only",
        info.width, info.height
    );
    let mut decoder = keyboard::Decoder::new();
    loop {
        // Give input priority over a fast client's frame stream.
        let mut items = Vec::new();
        items.push((control.handle().raw(), Signals::READABLE));
        items.push((keyboard.handle().raw(), Signals::READABLE));
        for client in &display.clients {
            items.push((
                client.channel.handle().raw(),
                Signals::READABLE.or(Signals::PEER_CLOSED),
            ));
        }
        let Ok((index, signals)) = wait_any(&items, 0) else {
            continue;
        };
        match index {
            0 => {
                if let Ok(mut m) = control.try_receive() {
                    if m.kind == proto::CONNECT && m.handles.len() == 1 {
                        add_client(&mut display, Channel::from_handle(m.handles.remove(0)));
                    }
                }
            }
            1 => {
                let mut bytes = [0; 64];
                if let Ok(count) = keyboard.read(&mut bytes) {
                    for byte in &bytes[..count] {
                        if let Some(key) = decoder.feed(*byte) {
                            key_event(&mut display, key);
                        }
                    }
                }
            }
            _ => {
                let client = index - 2;
                if signals.contains(Signals::READABLE) {
                    if let Ok(message) = display.clients[client].channel.try_receive() {
                        request(&mut display, client, message);
                    }
                } else if signals.contains(Signals::PEER_CLOSED) {
                    let owner = display.clients[client].id;
                    let active = display
                        .focus
                        .map(|i| (display.surfaces[i].client, display.surfaces[i].id));
                    display.clients.remove(client);
                    display.surfaces.retain(|s| s.client != owner);
                    display.focus = active
                        .and_then(|key| {
                            display
                                .surfaces
                                .iter()
                                .position(|s| (s.client, s.id) == key)
                        })
                        .or_else(|| display.surfaces.len().checked_sub(1));
                    repaint(&mut display);
                    println!("display: client {owner} disconnected; console restored");
                }
            }
        }
    }
}
fn add_client(d: &mut Display, channel: Channel) {
    let id = d.next_client;
    d.next_client += 1;
    d.clients.push(Client {
        id,
        channel,
        next: 1,
    });
}
fn fail(d: &Display, c: usize, m: &Message, error: Error) {
    let mut reply =
        Message::new(proto::reply_kind(m.kind)).with_payload(&proto::encode_error(error));
    reply.flags = MessageFlags::ERROR;
    let _ = d.clients[c].channel.reply(m, reply);
}
fn request(d: &mut Display, c: usize, mut m: Message) {
    match m.kind {
        proto::CONNECT => {
            if m.handles.len() != 1 {
                return fail(d, c, &m, Error::InvalidArgument);
            }
            add_client(d, Channel::from_handle(m.handles.remove(0)));
            let _ = d.clients[c]
                .channel
                .reply(&m, Message::new(proto::reply_kind(m.kind)));
        }
        proto::WM_CREATE_SURFACE => {
            if proto::decode::<proto::WmCreateSurface>(&m.payload).is_none() {
                return fail(d, c, &m, Error::InvalidArgument);
            }
            if d.surfaces.len() >= 8 {
                return fail(d, c, &m, Error::LimitExceeded);
            }
            let (w, h) = (d.screen.width(), d.screen.height());
            let pitch = w * 4;
            let size = pitch as u64 * h as u64;
            let Ok(vmo) = Vmo::create(size, false) else {
                return fail(d, c, &m, Error::NoMemory);
            };
            let Ok(mapping) = vmo.map(
                0,
                size,
                MapFlags::READ
                    .or(MapFlags::WRITE)
                    .or(MapFlags::SHARED)
                    .or(MapFlags::POPULATE),
                MapWindow::Shared,
            ) else {
                return fail(d, c, &m, Error::NoMemory);
            };
            let Ok(handle) = vmo.handle().duplicate(
                Rights::DUPLICATE
                    .or(Rights::TRANSFER)
                    .or(Rights::READ)
                    .or(Rights::WRITE)
                    .or(Rights::MAP)
                    .or(Rights::GET_PROPERTY)
                    .or(Rights::WAIT),
            ) else {
                return fail(d, c, &m, Error::AccessDenied);
            };
            let title = m
                .payload
                .get(core::mem::size_of::<proto::WmCreateSurface>()..)
                .and_then(|b| core::str::from_utf8(b).ok())
                .unwrap_or("program");
            // SAFETY: mapping is the full surface and is kept alive below.
            let canvas = unsafe { Canvas::from_raw(mapping.base() as *mut u8, w, h, pitch) };
            let id = d.clients[c].next;
            d.clients[c].next += 1;
            d.surfaces.push(Surface {
                client: d.clients[c].id,
                id,
                title: String::from(title),
                canvas,
                _mapping: mapping,
                _vmo: vmo,
            });
            let previous = d.focus;
            d.focus = Some(d.surfaces.len() - 1);
            let info = proto::Encoded::new(proto::WmSurfaceInfo {
                surface: id,
                width: w,
                height: h,
                pitch,
                size,
            });
            let _ = d.clients[c].channel.reply(
                &m,
                Message::new(proto::reply_kind(m.kind))
                    .with_payload(info.bytes())
                    .with_handle(handle),
            );
            if let Some(previous) = previous {
                event(
                    d,
                    previous,
                    proto::WmEvent {
                        kind: wm_event::FOCUS_OUT,
                        ..Default::default()
                    },
                );
            }
            event(
                d,
                d.focus.unwrap(),
                proto::WmEvent {
                    kind: wm_event::FOCUS_IN,
                    ..Default::default()
                },
            );
            repaint(d);
            println!("display: foreground {} surface {id} ({}x{})", title, w, h);
        }
        proto::WM_COMMIT => {
            let Some(damage) = proto::decode::<proto::WmDamage>(&m.payload) else {
                return fail(d, c, &m, Error::InvalidArgument);
            };
            let Some(index) = find(d, d.clients[c].id, damage.surface) else {
                return fail(d, c, &m, Error::NotFound);
            };
            if d.focus == Some(index) {
                let s = &d.surfaces[index].canvas;
                let x = damage.x.min(s.width());
                let y = damage.y.min(s.height());
                d.screen.blit(
                    x,
                    y,
                    s,
                    x,
                    y,
                    damage.w.min(s.width() - x),
                    damage.h.min(s.height() - y),
                );
            }
            let _ = d.clients[c]
                .channel
                .reply(&m, Message::new(proto::reply_kind(m.kind)));
        }
        proto::WM_SET_TITLE | proto::WM_DESTROY_SURFACE => {
            let Some(target) = proto::decode::<proto::WmSurfaceRef>(&m.payload) else {
                return fail(d, c, &m, Error::InvalidArgument);
            };
            let Some(index) = find(d, d.clients[c].id, target.surface) else {
                return fail(d, c, &m, Error::NotFound);
            };
            if m.kind == proto::WM_DESTROY_SURFACE {
                d.surfaces.remove(index);
                d.focus = match d.focus {
                    Some(f) if f > index => Some(f - 1),
                    Some(f) if f == index => d.surfaces.len().checked_sub(1),
                    other => other,
                };
                repaint(d);
            }
            let _ = d.clients[c]
                .channel
                .reply(&m, Message::new(proto::reply_kind(m.kind)));
        }
        _ => fail(d, c, &m, Error::NotSupported),
    }
}
fn find(d: &Display, client: u32, id: u32) -> Option<usize> {
    d.surfaces
        .iter()
        .position(|s| s.client == client && s.id == id)
}
fn event(d: &Display, index: usize, mut value: proto::WmEvent) {
    let s = &d.surfaces[index];
    value.surface = s.id;
    if let Some(c) = d.clients.iter().find(|c| c.id == s.client) {
        let mut m = Message::new(proto::WM_EVENT).with_payload(proto::Encoded::new(value).bytes());
        m.flags = MessageFlags::NON_BLOCKING;
        let _ = c.channel.send(m);
    }
}
fn repaint(d: &mut Display) {
    if let Some(index) = d.focus {
        let s = &d.surfaces[index];
        d.screen
            .blit(0, 0, &s.canvas, 0, 0, s.canvas.width(), s.canvas.height());
        println!("display: active {}", s.title);
    } else {
        d.screen.clear(0x101010);
    }
}
fn key_event(d: &mut Display, key: keyboard::KeyEvent) {
    if key.is_meta_key() {
        if key.pressed {
            d.meta_used = false;
        } else if !d.meta_used {
            cycle(d);
        }
        return;
    }
    if key.pressed && key.modifiers & wm_mod::META != 0 {
        d.meta_used = true;
        match key.scancode {
            0x0f => cycle(d),
            0x10 => {
                if let Some(i) = d.focus.filter(|i| *i != 0) {
                    event(
                        d,
                        i,
                        proto::WmEvent {
                            kind: wm_event::CLOSE,
                            ..Default::default()
                        },
                    );
                }
            }
            _ => {}
        }
        return;
    }
    if let Some(index) = d.focus {
        event(
            d,
            index,
            proto::WmEvent {
                kind: wm_event::KEY,
                scancode: key.scancode,
                extended: key.extended as u8,
                pressed: key.pressed as u8,
                ascii: key.ascii,
                modifiers: key.modifiers,
                ..Default::default()
            },
        );
    }
}
fn cycle(d: &mut Display) {
    if d.surfaces.is_empty() {
        return;
    }
    let next = d.focus.map(|i| (i + 1) % d.surfaces.len()).unwrap_or(0);
    if let Some(old) = d.focus {
        event(
            d,
            old,
            proto::WmEvent {
                kind: wm_event::FOCUS_OUT,
                ..Default::default()
            },
        );
    }
    d.focus = Some(next);
    event(
        d,
        next,
        proto::WmEvent {
            kind: wm_event::FOCUS_IN,
            ..Default::default()
        },
    );
    repaint(d);
}
