//! `doom` -- Doom, as a NodeOS process.
//!
//! The game itself is doomgeneric's C tree, compiled freestanding and linked
//! in by `build.rs`. This file is the other side of that boundary: it collects
//! the two capabilities the launcher grants -- a channel to `compositor` and
//! the IWAD as a read-only memory object -- turns the first into a window, and
//! exposes both to C through the handful of `ndos_*` functions declared in
//! `include/nodeos_platform.h`.
//!
//! That is the whole port surface. Doom performs no syscall of its own beyond
//! what those functions do on its behalf: it cannot, having been handed no
//! other handle. A capability system makes that statement checkable rather
//! than aspirational -- `kernel/src/launch.rs` grants `doom` exactly `Memory`,
//! `Channel` and `Clock`, and there is no ambient authority behind them to
//! fall back on.
//!
//! Doom used to hold the framebuffer and keyboard devices directly, which is
//! why it and `console` could not be on screen at once. It now holds neither,
//! and they can.
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

fn run(mut env: Env) -> i32 {
    println!("doom: starting");

    // The launcher's grant order, from `kernel/src/launch.rs`: the compositor
    // connection, then the IWAD.
    let Some(wm_handle) = env.take_extra(0) else {
        println!("doom: no compositor connection granted; exiting");
        return 1;
    };
    // Sound is optional and last: a boot with no `audio` module leaves this
    // `None` and Doom plays silently rather than refusing to start. Taken
    // after the WAD because `take_extra(0)` pops, so the order here is the
    // order the launcher pushed them.
    let Some(wad_handle) = env.take_extra(0) else {
        println!("doom: no IWAD memory object granted; exiting");
        return 1;
    };
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
    // The launcher granted a memory object over the bytes the boot loader
    // placed in memory. Mapping it read-only is the whole of "loading" the
    // WAD: `fopen` in the mini-libc hands Doom a stream over this range, so
    // the 14 MiB is read in place and never copied onto the heap.
    let Ok(wad_info) = wad_handle.query() else {
        println!("doom: could not query the IWAD memory object; exiting");
        return 1;
    };
    let wad = Vmo::from_handle(wad_handle, wad_info.size);
    let Ok(wad_mapping) = wad.map(0, wad_info.size, MapFlags::READ, MapWindow::MappedFile) else {
        println!("doom: failed to map the IWAD; exiting");
        return 1;
    };
    println!("doom: IWAD mapped, {} KiB", wad_info.size / 1024);

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

