//! `cc` -- a small C compiler that runs on NodeOS.
//!
//! It reads a C source file through the shell's filesystem channel, compiles
//! the subset described in `parse.rs` to the stack machine in `vm.rs`, and runs
//! it, printing the program's output to the console. There is no object file
//! and no linker: everything -- the program, the standard library, the runtime
//! -- is in this one process.
//!
//! ```
//! nodeos> run /bin/cc.nxf /TESTS/HELLO.C
//! hello, world
//! ```
//!
//! The standard library is a set of builtins the compiler recognises by name
//! (`printf`, `strlen`, `malloc`, `rand`, ...); a program uses them without a
//! declaration and without a header.

#![no_std]
#![no_main]

extern crate alloc;

mod compile;
mod lex;
mod parse;
mod vm;

use alloc::format;
use alloc::string::String;
use alloc::vec::Vec;

use libnode::abi::message::MessageFlags;
use libnode::abi::proto;
use libnode::{println, Channel, Env, Message};

libnode::main! {
    fn main(env: Env) -> i32 {
        run(env)
    }
}

/// Largest read from the filesystem in one request; matches the fs service.
const READ_CHUNK: u32 = 32 * 1024;

fn run(mut env: Env) -> i32 {
    // The shell's session order: console, filesystem, compositor, audio.
    let console = env.take_extra(0).map(Channel::from_handle);
    let files = env.take_extra(0).map(Channel::from_handle);
    let args: Vec<String> = env.args().to_vec();

    let Some(path) = args.get(1).cloned() else {
        out(&console, "usage: run /bin/cc.nxf <source.c>\n");
        return 2;
    };
    let Some(files) = files.as_ref() else {
        out(&console, "cc: no filesystem connection\n");
        return 2;
    };

    let source = match read_file(files, &path) {
        Some(bytes) => bytes,
        None => {
            out(&console, &format!("cc: cannot read {path}\n"));
            return 2;
        }
    };

    let program = match parse::parse(&source) {
        Ok(program) => program,
        Err(error) => {
            out(&console, &format!("cc: {path}: {error}\n"));
            return 1;
        }
    };

    let compiled = match compile::compile(&program) {
        Ok(compiled) => compiled,
        Err(error) => {
            out(&console, &format!("cc: {path}: {error}\n"));
            return 1;
        }
    };

    let (output, status) = vm::run(&compiled);
    out(&console, &output);

    // Mirror the program's output into the kernel log, where a boot harness can
    // read it without a screen, and report how it ended.
    println!("cc: output for {path}:");
    for line in output.lines() {
        println!("cc: | {line}");
    }
    println!("cc: {path} exited with {status}");

    (status & 0xff) as i32
}

/// Write text to the console, if one is connected.
fn out(console: &Option<Channel>, text: &str) {
    if let Some(console) = console {
        let _ = console.call(Message::new(proto::CONSOLE_WRITE_TEXT).with_payload(text.as_bytes()));
    }
}

/// Read a whole file through the filesystem protocol.
fn read_file(files: &Channel, path: &str) -> Option<Vec<u8>> {
    let reply = call(files, Message::new(proto::FS_OPEN).with_payload(path.as_bytes()))?;
    let open = proto::decode::<proto::FsOpenReply>(&reply.payload)?;
    if open.kind != proto::fs_kind::FILE {
        return None;
    }

    let mut out = Vec::new();
    let mut offset = 0u64;
    while offset < open.size {
        let request = proto::Encoded::new(proto::FsReadRequest {
            node: open.node,
            offset,
            length: READ_CHUNK,
            _reserved: 0,
        });
        let Ok(reply) = files.call_with(
            Message::new(proto::FS_READ).with_payload(request.bytes()),
            READ_CHUNK as usize + 64,
            0,
        ) else {
            break;
        };
        if reply.flags.contains(MessageFlags::ERROR) || reply.payload.is_empty() {
            break;
        }
        out.extend_from_slice(&reply.payload);
        offset += reply.payload.len() as u64;
    }

    let close = proto::Encoded::new(proto::FsCloseRequest { node: open.node });
    let _ = files.call_with(Message::new(proto::FS_CLOSE).with_payload(close.bytes()), 64, 0);

    if offset >= open.size {
        Some(out)
    } else {
        None
    }
}

/// A filesystem call whose reply is small.
fn call(files: &Channel, request: Message) -> Option<Message> {
    let reply = files.call_with(request, 4096, 0).ok()?;
    if reply.flags.contains(MessageFlags::ERROR) {
        return None;
    }
    Some(reply)
}
