//! `tree` -- recursively list the filesystem, one indented line per entry.
//!
//! The shell's `ls` lists one directory; `tree` shows the whole shape of the
//! volume, which makes it a useful smoke test for the filesystem service: every
//! nested directory is another `FS_LIST` round trip, and a wrong cluster chain
//! shows up as an entry in the wrong place or a missing branch rather than as a
//! test assertion nobody reads.
//!
//! Launch it from the shell with `run /bin/tree.nxf`.

#![no_std]
#![no_main]

extern crate alloc;

use alloc::string::String;
use core::fmt::Write;

use libnode::abi::message::MessageFlags;
use libnode::abi::proto;
use libnode::{println, Channel, Env, Error, Message};

libnode::main! {
    fn main(env: Env) -> i32 {
        run(env)
    }
}

/// How deep to descend. A volume deeper than this is a curiosity rather than a
/// directory tree, and the bound keeps a malformed chain from recursing until
/// the stack runs out.
const MAX_DEPTH: usize = 6;
/// Total entries printed before the walk stops.
const MAX_ENTRIES: usize = 256;

fn run(mut env: Env) -> i32 {
    let Some(console_handle) = env.take_extra(0) else {
        println!("tree: no console connection granted; exiting");
        return 1;
    };
    let console = Channel::from_handle(console_handle);
    let Some(files) = env.take_extra(0).map(Channel::from_handle) else {
        println!("tree: no filesystem connection granted; exiting");
        let _ = console.call(Message::new(proto::CONSOLE_WRITE_TEXT).with_payload(b"tree: no filesystem connected\n"));
        return 1;
    };

    let mut out = String::new();
    out.push_str("tree: /\n");
    let mut walked = 0usize;
    let mut files_seen = 0usize;
    let mut dirs_seen = 0usize;
    walk(&files, "/", 0, &mut out, &mut walked, &mut files_seen, &mut dirs_seen);

    let _ = writeln!(out, "tree: {dirs_seen} directories, {files_seen} files");
    let _ = console.call(Message::new(proto::CONSOLE_WRITE_TEXT).with_payload(out.as_bytes()));
    println!("tree: {dirs_seen} directories, {files_seen} files");
    0
}

/// List `path`, then recurse into each directory it names.
#[allow(clippy::too_many_arguments)]
fn walk(
    files: &Channel,
    path: &str,
    depth: usize,
    out: &mut String,
    walked: &mut usize,
    files_seen: &mut usize,
    dirs_seen: &mut usize,
) {
    if depth >= MAX_DEPTH || *walked >= MAX_ENTRIES {
        return;
    }

    let Ok(reply) = call(files, Message::new(proto::FS_LIST).with_payload(path.as_bytes())) else {
        let _ = writeln!(out, "{}<cannot list {path}>", indent(depth + 1));
        return;
    };

    // The reply is NUL-separated `name\tkind\tsize` records. Collect the
    // directories first so the output reads like a tree rather than a listing.
    let mut directories = alloc::vec::Vec::new();
    for record in reply.payload.split(|byte| *byte == 0) {
        if record.is_empty() || *walked >= MAX_ENTRIES {
            continue;
        }
        let Ok(text) = core::str::from_utf8(record) else { continue };
        let mut columns = text.split('\t');
        let name = columns.next().unwrap_or("");
        let kind = columns.next().unwrap_or("f");
        let size = columns.next().unwrap_or("0");
        if name.is_empty() {
            continue;
        }
        if kind == "d" {
            let _ = writeln!(out, "{}{name}/", indent(depth + 1));
            *dirs_seen += 1;
            directories.push(String::from(name));
        } else {
            let _ = writeln!(out, "{}{name}  ({size} bytes)", indent(depth + 1));
            *files_seen += 1;
        }
        *walked += 1;
    }

    for name in directories {
        let mut child = String::from(path);
        if !child.ends_with('/') {
            child.push('/');
        }
        child.push_str(&name);
        walk(files, &child, depth + 1, out, walked, files_seen, dirs_seen);
    }
}

/// Spaces for nesting level `depth`.
fn indent(depth: usize) -> &'static str {
    const SPACES: &str = "                                                                ";
    let width = (depth * 2).min(SPACES.len());
    &SPACES[..width]
}

fn call(files: &Channel, request: Message) -> Result<Message, Error> {
    let reply = files.call_with(request, 8192, 0)?;
    if reply.flags.contains(MessageFlags::ERROR) {
        return Err(proto::decode_error(&reply.payload).unwrap_or(Error::BadState));
    }
    Ok(reply)
}
