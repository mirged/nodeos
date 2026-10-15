//! `sysinfo` -- a system report, written to the console.
//!
//! A console client like the shell: its only authority is the console channel
//! and the filesystem channel the shell handed it in its bootstrap message, and
//! everything it prints came over one of those two. It exists to make the shape
//! of a NodeOS process visible -- its capability set, its clock, a memory
//! object it is allowed to allocate, and the filesystem it can see -- without
//! reaching for any device or kernel interface of its own.
//!
//! Launch it from the shell with `run /bin/sysinfo.nxf`.

#![no_std]
#![no_main]

extern crate alloc;

use alloc::string::String;
use core::fmt::Write;

use libnode::abi::message::MessageFlags;
use libnode::abi::proto;
use libnode::abi::CapClass;
use libnode::{println, Channel, Env, Error, Message};

libnode::main! {
    fn main(env: Env) -> i32 {
        run(env)
    }
}

fn run(mut env: Env) -> i32 {
    // Positional, exactly as the shell's `run` wired them: the console first,
    // the filesystem second. A boot without a filesystem gives one handle and
    // the report says so rather than the program refusing to start.
    let Some(console_handle) = env.take_extra(0) else {
        println!("sysinfo: no console connection granted; exiting");
        return 1;
    };
    let console = Channel::from_handle(console_handle);
    let files = env.take_extra(0).map(Channel::from_handle);

    let mut report = String::new();
    report.push_str("sysinfo: NodeOS system report\n");

    let uptime_s = libnode::clock_ns() / 1_000_000_000;
    let _ = writeln!(report, "  uptime:        {uptime_s} s");
    let _ = writeln!(report, "  capabilities:  {}", describe_caps(env.granted_mask()));

    // The console answers with its own geometry. A client that never asked
    // could not know how wide its window is, which is the point of the call.
    if let Ok(reply) = console.call(Message::new(proto::CONSOLE_GET_INFO)) {
        if let Some(info) = proto::decode::<proto::ConsoleInfo>(&reply.payload) {
            let _ = writeln!(
                report,
                "  console:       {}x{} px, {}x{} text cells",
                info.width_px, info.height_px, info.cols, info.rows
            );
        }
    }

    // Prove the `MEMORY` capability is real by using it, and report what the
    // kernel returned. The object is dropped at the end of this scope.
    match libnode::mem::Vmo::create(64 * 1024, false) {
        Ok(vmo) => {
            let _ = writeln!(report, "  memory object: {} KiB allocated", vmo.size() / 1024);
        }
        Err(error) => {
            let _ = writeln!(report, "  memory object: allocation failed ({error:?})");
        }
    }

    write(&console, &report);

    if let Some(files) = files.as_ref() {
        report_filesystem(&console, files);
    } else {
        write(&console, "  filesystem:    not connected\n");
    }

    write(&console, "sysinfo: done\n");
    // Also on the kernel debug log, so a boot harness can check the report
    // without reading pixels off a screen.
    println!("sysinfo: done");
    0
}

/// Write text through the console protocol.
fn write(console: &Channel, text: &str) {
    let _ = console.call(Message::new(proto::CONSOLE_WRITE_TEXT).with_payload(text.as_bytes()));
}

/// The names of the capability classes set in `mask`.
fn describe_caps(mask: u32) -> String {
    const ALL: [(CapClass, &str); 16] = [
        (CapClass::FileRead, "FILE_READ"),
        (CapClass::FileWrite, "FILE_WRITE"),
        (CapClass::FileCreate, "FILE_CREATE"),
        (CapClass::Network, "NETWORK"),
        (CapClass::NetworkBind, "NETWORK_BIND"),
        (CapClass::Gpu, "GPU"),
        (CapClass::Audio, "AUDIO"),
        (CapClass::Memory, "MEMORY"),
        (CapClass::Process, "PROCESS"),
        (CapClass::Channel, "CHANNEL"),
        (CapClass::Device, "DEVICE"),
        (CapClass::Clock, "CLOCK"),
        (CapClass::Debug, "DEBUG"),
        (CapClass::Load, "LOAD"),
        (CapClass::Input, "INPUT"),
        (CapClass::Display, "DISPLAY"),
    ];
    let mut line = String::new();
    for (class, name) in ALL {
        if mask & (1 << class as u32) != 0 {
            if !line.is_empty() {
                line.push(' ');
            }
            line.push_str(name);
        }
    }
    if line.is_empty() {
        line.push_str("(none)");
    }
    line
}

/// Ask the filesystem for its statistics and the root directory.
fn report_filesystem(console: &Channel, files: &Channel) {
    match call(files, Message::new(proto::FS_STATFS)) {
        Ok(reply) => {
            if let Some(stat) = proto::decode::<proto::FsStatFs>(&reply.payload) {
                let tag = stat.kind.to_le_bytes();
                let name = core::str::from_utf8(&tag).unwrap_or("FAT").trim_end_matches('\0');
                let mut line = String::new();
                let _ = writeln!(
                    line,
                    "  filesystem:    {name}, {} KiB total, {} KiB free, {} requests",
                    stat.bytes / 1024,
                    stat.free / 1024,
                    stat.requests
                );
                write(console, &line);
            }
        }
        Err(error) => {
            let mut line = String::new();
            let _ = writeln!(line, "  filesystem:    statfs failed ({error:?})");
            write(console, &line);
        }
    }

    match call(files, Message::new(proto::FS_LIST).with_payload(b"/")) {
        Ok(reply) => {
            let mut line = String::new();
            line.push_str("  /:\n");
            let mut shown = 0;
            for record in reply.payload.split(|byte| *byte == 0) {
                if record.is_empty() {
                    continue;
                }
                let Ok(text) = core::str::from_utf8(record) else { continue };
                let mut columns = text.split('\t');
                let name = columns.next().unwrap_or("");
                let kind = columns.next().unwrap_or("f");
                let size = columns.next().unwrap_or("0");
                let _ = writeln!(
                    line,
                    "    {name:<20} {:>10} {}",
                    if kind == "d" { "<dir>" } else { size },
                    kind
                );
                shown += 1;
                if shown == 32 {
                    line.push_str("    ...\n");
                    break;
                }
            }
            write(console, &line);
        }
        Err(error) => {
            let mut line = String::new();
            let _ = writeln!(line, "  /:            list failed ({error:?})");
            write(console, &line);
        }
    }
    println!("sysinfo: filesystem probed");
}

/// A `channel_call` that turns an error reply into an `Err`.
fn call(files: &Channel, request: Message) -> Result<Message, Error> {
    let reply = files.call_with(request, 8192, 0)?;
    if reply.flags.contains(MessageFlags::ERROR) {
        return Err(proto::decode_error(&reply.payload).unwrap_or(Error::BadState));
    }
    Ok(reply)
}
