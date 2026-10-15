//! `doom` -- Doom, as a NodeOS process.
//!
//! The game itself is doomgeneric's C tree, compiled freestanding and linked
//! in by `build.rs`. This file is the other side of that boundary: it collects
//! the capabilities the shell hands it -- a filesystem channel, a channel to
//! `compositor`, and optionally the mixer -- turns the first into the IWAD and
//! the second into a window, and exposes them to C through the handful of
//! `ndos_*` functions declared in `include/nodeos_platform.h`.
//!
//! Doom is an ordinary disk program now: launched from the shell with
//! `run /bin/doom.nxf`, reading `/DOOM.WAD` from the filesystem like any other
//! client. It used to be a boot module handed the IWAD by the launcher, which
//! meant it could not be started any other way. Loading from the filesystem
//! removes that distinction and is the reason Doom needs no authority the shell
//! does not already hold.
//!
//! Doom performs no syscall of its own beyond what those functions do on its
//! behalf: it cannot, having been handed no other handle. A capability system
//! makes that statement checkable rather than aspirational.
//!
//! See `docs/DOOM.md` for how the pieces fit together.

#![no_std]
#![no_main]

extern crate alloc;

mod platform;

use libnode::abi::message::MessageFlags;
use libnode::abi::proto;
use libnode::abi::types::{MapFlags, MapWindow};
use libnode::mem::Vmo;
use libnode::{println, Channel, Env, Message};

extern "C" {
    /// doomgeneric's entry point, in `csrc/doomgeneric_nodeos.c`. Never
    /// returns: Doom exits through `I_Quit` -> `exit` -> `ndos_exit`.
    fn doom_main(argc: i32, argv: *mut *mut u8) -> i32;
}

libnode::main! {
    fn main(env: Env) -> i32 {
        run(env)
    }
}

/// What Doom asks the compositor for. Doom renders 320x200 and scales it up by
/// whole pixels, so anything at least that large works; asking for more lets
/// the compositor pick the largest whole-pixel scale its layout allows.
const WANTED_WIDTH: u32 = 640;
/// See [`WANTED_WIDTH`].
const WANTED_HEIGHT: u32 = 400;

/// Where the IWAD lives on the NodeOS disk.
const IWAD_PATH: &[u8] = b"/DOOM.WAD";

fn run(mut env: Env) -> i32 {
    println!("doom: starting");

    // The shell's session layout, in order: console (which Doom does not use),
    // filesystem (for the IWAD), compositor, then the mixer. `take_extra(0)`
    // pops from the front, so the order here matches the order the shell
    // pushed them.
    let _console = env.take_extra(0);
    let Some(files_handle) = env.take_extra(0) else {
        println!("doom: no filesystem connection granted; exiting");
        return 1;
    };
    let Some(wm_handle) = env.take_extra(0) else {
        println!("doom: no compositor connection granted; exiting");
        return 1;
    };
    let files = Channel::from_handle(files_handle);
    let wm = Channel::from_handle(wm_handle);

    // --- the window ------------------------------------------------------
    let request = proto::Encoded::new(proto::WmCreateSurface {
        width: WANTED_WIDTH,
        height: WANTED_HEIGHT,
        flags: 0,
        _reserved: 0,
    });
    let mut payload = alloc::vec::Vec::from(request.bytes());
    payload.extend_from_slice(b"doom");

    let Ok(mut reply) = wm.call(Message::new(proto::WM_CREATE_SURFACE).with_payload(&payload)) else {
        println!("doom: the compositor did not answer; exiting");
        return 1;
    };
    if reply.flags.contains(MessageFlags::ERROR) || reply.handles.is_empty() {
        println!("doom: no window granted ({:?}); exiting", proto::decode_error(&reply.payload));
        return 1;
    }
    let Some(surface) = proto::decode::<proto::WmSurfaceInfo>(&reply.payload) else {
        println!("doom: malformed surface reply; exiting");
        return 1;
    };

    let surface_vmo = Vmo::from_handle(reply.handles.remove(0), surface.size);
    // Shared and populated: the compositor holds the other mapping of these
    // pages and reads them on every commit.
    let Ok(surface_mapping) = surface_vmo.map(
        0,
        surface.size,
        MapFlags::READ.or(MapFlags::WRITE).or(MapFlags::SHARED).or(MapFlags::POPULATE),
        MapWindow::Shared,
    ) else {
        println!("doom: failed to map the window surface; exiting");
        return 1;
    };
    println!("doom: {}x{} window from the compositor", surface.width, surface.height);

    // --- the IWAD --------------------------------------------------------
    // Read the whole file into a memory object and map it read-only. The mini-
    // libc's `fopen` resolves `doom.wad` to this range, so the game reads the
    // WAD in place and never copies it onto its heap.
    let Some((wad, wad_size)) = load_iwad(&files) else {
        println!("doom: could not read {IWAD_PATH:?} from the filesystem; exiting");
        return 1;
    };
    let Ok(wad_mapping) = wad.map(0, wad_size, MapFlags::READ, MapWindow::MappedFile) else {
        println!("doom: failed to map the IWAD; exiting");
        return 1;
    };
    println!("doom: IWAD mapped, {} KiB", wad_size / 1024);

    let audio = env.take_extra(0).map(Channel::from_handle);
    println!("doom: sound is {}", if audio.is_some() { "on" } else { "off (no audio service)" });

    platform::install(wm, surface, surface_vmo, surface_mapping, wad_mapping, audio);

    // Doom parses its own command line. `-iwad` names the file the mini-libc
    // resolves to the mapping installed above; the name is what tells Doom
    // which game this is, so it has to end in ".wad" and match the IWAD's
    // identity, not merely exist.
    //
    // `-mb 16` raises Doom's zone from doomgeneric's 6 MiB default. The zone
    // is where every cached lump lives, and this machine has memory to spare.
    // Doom mutates argv in place when it expands a response file, so these
    // are owned buffers rather than pointers into read-only string data.
    let mut arg_program = *b"doom\0";
    let mut arg_iwad = *b"-iwad\0";
    let mut arg_iwad_name = *b"doom.wad\0";
    let mut arg_mb = *b"-mb\0";
    let mut arg_mb_value = *b"16\0";
    let mut argv = [
        arg_program.as_mut_ptr(),
        arg_iwad.as_mut_ptr(),
        arg_iwad_name.as_mut_ptr(),
        arg_mb.as_mut_ptr(),
        arg_mb_value.as_mut_ptr(),
        core::ptr::null_mut(),
    ];

    // SAFETY: `argv` is a NULL-terminated array of NUL-terminated strings
    // that outlive the call, which is the C `main` contract. Doom stores the
    // pointers in `myargv` and reads through them for the rest of the run.
    unsafe { doom_main(argv.len() as i32 - 1, argv.as_mut_ptr()) }
}

/// Read `/DOOM.WAD` into a memory object, returning it and its exact size.
///
/// The reply buffer for each read is one 32 KiB chunk, the filesystem
/// service's own maximum, and sits inside the runtime allocator's largest size
/// class -- so the hundreds of chunks a WAD takes reuse one buffer instead of
/// leaking a fresh one each time.
fn load_iwad(files: &Channel) -> Option<(Vmo, u64)> {
    let reply = call(files, Message::new(proto::FS_OPEN).with_payload(IWAD_PATH))?;
    let open = proto::decode::<proto::FsOpenReply>(&reply.payload)?;
    if open.kind != proto::fs_kind::FILE || open.size == 0 {
        return None;
    }
    let size = open.size;
    let vmo = Vmo::create(size, false).ok()?;

    const CHUNK: u32 = 32 * 1024;
    let mut offset = 0u64;
    while offset < size {
        let request = proto::Encoded::new(proto::FsReadRequest {
            node: open.node,
            offset,
            length: CHUNK,
            _reserved: 0,
        });
        let Ok(reply) = files.call_with(
            Message::new(proto::FS_READ).with_payload(request.bytes()),
            CHUNK as usize + 64,
            0,
        ) else {
            break;
        };
        if reply.flags.contains(MessageFlags::ERROR) || reply.payload.is_empty() {
            break;
        }
        if vmo.write(offset, &reply.payload).is_err() {
            break;
        }
        offset += reply.payload.len() as u64;
    }

    let close = proto::Encoded::new(proto::FsCloseRequest { node: open.node });
    let _ = files.call_with(Message::new(proto::FS_CLOSE).with_payload(close.bytes()), 64, 0);

    (offset == size).then_some((vmo, size))
}

/// A filesystem call whose reply is small, turning an error reply into `None`.
fn call(files: &Channel, request: Message) -> Option<Message> {
    let reply = files.call_with(request, 4096, 0).ok()?;
    if reply.flags.contains(MessageFlags::ERROR) {
        return None;
    }
    Some(reply)
}
