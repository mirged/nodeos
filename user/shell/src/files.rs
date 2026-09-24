//! The shell's file commands.
//!
//! Every one of them is a `channel_call` to `fs` and nothing else. The shell
//! holds no device, knows no sector, and has never seen a FAT: what it has is
//! a channel the launcher handed it, and the whole of its filesystem authority
//! is what the service on the other end is willing to do.
//!
//! `fstest` is the odd one out and earns its place: it drives a file through
//! its whole life -- create, write, read back, append, list, stat, delete --
//! and reports PASS or FAIL to the serial log as well as the screen, so
//! `make test-fs` can boot this, type one word, and check the answer without a
//! human looking at the display. The host then reads the same disk image with
//! mtools, which is what makes it a test of the *filesystem* rather than a
//! test of the service agreeing with itself.

use alloc::string::String;
use alloc::vec::Vec;

use libnode::abi::message::{BootstrapMessage, KIND_BOOTSTRAP};
use libnode::abi::message::MessageFlags;
use libnode::abi::proto::{self, fs_kind};
use libnode::abi::{CapClass, Rights};
use libnode::mem::Vmo;
use libnode::process::Process;
use libnode::{println, Channel, Error, Message};

use crate::write_text;

/// Run one file command.
pub fn run(console: &Channel, files: &Channel, cwd: &str, command: &str, rest: &str) {
    let rest = rest.trim();
    match command {
        "ls" => list(console, files, &resolve(cwd, if rest.is_empty() { "." } else { rest })),
        "cat" => cat(console, files, &resolve(cwd, rest)),
        "write" => put(console, files, cwd, rest, false),
        "append" => put(console, files, cwd, rest, true),
        "mkdir" => simple(console, files, proto::FS_MKDIR, &resolve(cwd, rest), "created"),
        "rm" => simple(console, files, proto::FS_UNLINK, &resolve(cwd, rest), "removed"),
        "stat" => stat(console, files, &resolve(cwd, rest)),
        "df" => df(console, files),
        "fstest" => self_test(console, files),
        "touch" => touch(console, files, cwd, rest),
        "run" => run_program(console, files, cwd, rest),
        _ => {}
    }
}

pub fn change_dir(console: &Channel, files: &Channel, cwd: &mut String, rest: &str) {
    let path = resolve(cwd, if rest.trim().is_empty() { "/" } else { rest.trim() });
    match call(files, Message::new(proto::FS_STAT).with_payload(path.as_bytes())) {
        Ok(reply) => match proto::decode::<proto::FsStat>(&reply.payload) {
            Some(stat) if stat.kind == fs_kind::DIRECTORY => *cwd = path,
            Some(_) => write_text(console, "cd: not a directory\n"),
            None => write_text(console, "cd: malformed reply\n"),
        },
        Err(error) => report(console, &path, error),
    }
}

fn resolve(cwd: &str, input: &str) -> String {
    let mut components: Vec<String> = Vec::new();
    let source = if input.starts_with('/') { input } else { cwd };
    for component in source.split('/').chain(if input.starts_with('/') { "" } else { input }.split('/')) {
        match component {
            "" | "." => {}
            ".." => { components.pop(); }
            name => components.push(name.into()),
        }
    }
    let mut path = String::from("/");
    for (index, component) in components.iter().enumerate() {
        if index != 0 { path.push('/'); }
        path.push_str(component);
    }
    path
}

// ---------------------------------------------------------------------------
// Commands
// ---------------------------------------------------------------------------

fn list(console: &Channel, files: &Channel, path: &str) {
    let reply = match call(files, Message::new(proto::FS_LIST).with_payload(path.as_bytes())) {
        Ok(reply) => reply,
        Err(error) => return report(console, path, error),
    };
    let mut count = 0;
    for record in reply.payload.split(|byte| *byte == 0) {
        if record.is_empty() {
            continue;
        }
        let Ok(text) = core::str::from_utf8(record) else { continue };
        let mut columns = text.split('\t');
        let name = columns.next().unwrap_or("");
        let kind = columns.next().unwrap_or("f");
        let size = columns.next().unwrap_or("0");
        let mut line = String::new();
        let _ = core::fmt::write(
            &mut line,
            format_args!("{:<20} {:>10} {}\n", name, if kind == "d" { "<dir>" } else { size }, kind),
        );
        write_text(console, &line);
        count += 1;
    }
    let mut summary = String::new();
    let _ = core::fmt::write(&mut summary, format_args!("{count} entries in {path}\n"));
    write_text(console, &summary);
}

fn cat(console: &Channel, files: &Channel, path: &str) {
    let Ok(node) = open(files, path) else {
        return report(console, path, Error::NotFound);
    };
    let mut offset = 0u64;
    loop {
        let request = proto::Encoded::new(proto::FsReadRequest {
            node: node.node,
            offset,
            length: 4096,
            _reserved: 0,
        });
        let Ok(reply) = call(files, Message::new(proto::FS_READ).with_payload(request.bytes())) else {
            break;
        };
        if reply.payload.is_empty() {
            break;
        }
        offset += reply.payload.len() as u64;
        // Written a chunk at a time rather than assembled: a large file would
        // otherwise be held twice, once here and once in the console's own
        // buffer, for no benefit.
        write_text(console, core::str::from_utf8(&reply.payload).unwrap_or("<binary>"));
    }
    close(files, node.node);
    write_text(console, "\n");
}

/// `write <path> <text>` and `append <path> <text>`.
fn put(console: &Channel, files: &Channel, cwd: &str, rest: &str, append: bool) {
    let mut parts = rest.splitn(2, ' ');
    let path = parts.next().unwrap_or("").trim();
    let text = parts.next().unwrap_or("");
    if path.is_empty() {
        return write_text(console, "usage: write <path> <text>\n");
    }
    let path = resolve(cwd, path);

    let node = if append {
        match open(files, &path) {
            Ok(node) => node,
            // Appending to a file that is not there creates it, which is what
            // every shell's `>>` does.
            Err(_) => match create(files, &path) {
                Ok(node) => node,
                Err(error) => return report(console, &path, error),
            },
        }
    } else {
        match create(files, &path) {
            Ok(node) => node,
            Err(error) => return report(console, &path, error),
        }
    };

    let mut payload = Vec::new();
    let header = proto::Encoded::new(proto::FsWriteHeader {
        node: node.node,
        offset: if append { node.size } else { 0 },
        length: (text.len() + 1) as u32,
        _reserved: 0,
    });
    payload.extend_from_slice(header.bytes());
    payload.extend_from_slice(text.as_bytes());
    payload.push(b'\n');

    match call(files, Message::new(proto::FS_WRITE).with_payload(&payload)) {
        Ok(reply) => {
            let written = proto::decode::<proto::FsWriteHeader>(&reply.payload).map(|h| h.length).unwrap_or(0);
            let mut line = String::new();
            let _ = core::fmt::write(&mut line, format_args!("wrote {written} bytes to {path}\n"));
            write_text(console, &line);
        }
        Err(error) => report(console, &path, error),
    }
    close(files, node.node);
}

fn touch(console: &Channel, files: &Channel, cwd: &str, rest: &str) {
    let input = rest.trim();
    if input.is_empty() {
        return write_text(console, "usage: touch <path>\n");
    }
    let path = resolve(cwd, input);
    match create(files, &path) {
        Ok(node) => {
            close(files, node.node);
            let mut line = String::new();
            let _ = core::fmt::write(&mut line, format_args!("created {path}\n"));
            write_text(console, &line);
        }
        Err(_) => match open(files, &path) {
            Ok(node) => {
                close(files, node.node);
                write_text(console, "touched\n");
            }
            Err(error) => report(console, &path, error),
        },
    }
}

fn run_program(console: &Channel, files: &Channel, cwd: &str, rest: &str) {
    let input = rest.trim();
    if input.is_empty() {
        return write_text(console, "usage: run <program>\n");
    }
    let path = resolve(cwd, input);
    let Ok(node) = open(files, &path) else {
        return report(console, &path, Error::NotFound);
    };
    if node.kind != fs_kind::FILE {
        close(files, node.node);
        return write_text(console, "run: not a regular file\n");
    }

    let image = read_all(files, node.node);
    close(files, node.node);
    let Some(image) = image else {
        return write_text(console, "run: could not read executable\n");
    };
    if image.len() < 4 || image[..4] != *b"NXF\0" {
        return write_text(console, "run: file is not an NXF executable\n");
    }

    let Ok(vmo) = Vmo::create(image.len() as u64, false) else {
        return write_text(console, "run: could not allocate image memory\n");
    };
    if vmo.write(0, &image).is_err() {
        return write_text(console, "run: could not stage executable\n");
    }

    let Ok(process) = Process::create(&path, u32::MAX) else {
        return write_text(console, "run: process creation denied\n");
    };
    let Ok(loaded) = process.load(&vmo, image.len() as u64) else {
        return write_text(console, "run: executable load failed\n");
    };
    let Ok((parent_bootstrap, child_bootstrap)) = Channel::create() else {
        return write_text(console, "run: could not create bootstrap channel\n");
    };
    if process.start(loaded.entry, loaded.stack, child_bootstrap.into_handle()).is_err() {
        return write_text(console, "run: could not start process\n");
    }

    let granted_caps = [
        CapClass::Process,
        CapClass::Load,
        CapClass::Channel,
        CapClass::Memory,
        CapClass::Clock,
        CapClass::FileRead,
        CapClass::FileWrite,
        CapClass::FileCreate,
    ]
    .into_iter()
    .fold(0u32, |mask, class| mask | (1 << class as u32));
    let info = proto::Encoded::new(BootstrapMessage {
        version: 1,
        granted_caps,
        process: nodeos_abi::HandleValue::INVALID,
        space: nodeos_abi::HandleValue::INVALID,
        thread: nodeos_abi::HandleValue::INVALID,
        namespace: nodeos_abi::HandleValue::INVALID,
        image_base: 0,
        heap_size: 4 * 1024 * 1024,
        extra_count: 2,
        _reserved: 0,
    });
    let Ok(console_handle) = console.handle().duplicate(Rights::DEFAULT) else {
        return write_text(console, "run: could not duplicate console\n");
    };
    let Ok(files_handle) = files.handle().duplicate(Rights::DEFAULT) else {
        return write_text(console, "run: could not duplicate filesystem\n");
    };
    let message = Message::new(KIND_BOOTSTRAP)
        .with_payload(info.bytes())
        .with_handle(console_handle)
        .with_handle(files_handle);
    if parent_bootstrap.send(message).is_err() {
        return write_text(console, "run: could not deliver bootstrap\n");
    }
    write_text(console, "process started\n");
}

fn stat(console: &Channel, files: &Channel, path: &str) {
    match call(files, Message::new(proto::FS_STAT).with_payload(path.as_bytes())) {
        Ok(reply) => {
            let Some(stat) = proto::decode::<proto::FsStat>(&reply.payload) else {
                return write_text(console, "malformed reply\n");
            };
            let mut line = String::new();
            let _ = core::fmt::write(
                &mut line,
                format_args!(
                    "{path}: {} bytes, {}, first cluster {}, {} entries\n",
                    stat.size,
                    if stat.kind == fs_kind::DIRECTORY { "directory" } else { "file" },
                    stat.node,
                    stat.entries
                ),
            );
            write_text(console, &line);
        }
        Err(error) => report(console, path, error),
    }
}

fn df(console: &Channel, files: &Channel) {
    match call(files, Message::new(proto::FS_STATFS)) {
        Ok(reply) => {
            let Some(stat) = proto::decode::<proto::FsStatFs>(&reply.payload) else {
                return write_text(console, "malformed reply\n");
            };
            let tag = stat.kind.to_le_bytes();
            let name = core::str::from_utf8(&tag).unwrap_or("FAT").trim_end_matches('\0');
            let mut line = String::new();
            let _ = core::fmt::write(
                &mut line,
                format_args!(
                    "{name}: {} KiB total, {} KiB free, {} byte clusters, {} disk requests\n",
                    stat.bytes / 1024,
                    stat.free / 1024,
                    stat.block_size,
                    stat.requests
                ),
            );
            write_text(console, &line);
        }
        Err(error) => report(console, "df", error),
    }
}

fn simple(console: &Channel, files: &Channel, kind: u32, path: &str, verb: &str) {
    if path.is_empty() {
        return write_text(console, "usage: <command> <path>\n");
    }
    match call(files, Message::new(kind).with_payload(path.as_bytes())) {
        Ok(_) => {
            let mut line = String::new();
            let _ = core::fmt::write(&mut line, format_args!("{verb} {path}\n"));
            write_text(console, &line);
        }
        Err(error) => report(console, path, error),
    }
}

// ---------------------------------------------------------------------------
// The self-test
// ---------------------------------------------------------------------------

/// Drive one file through create, write, read back, append, list, stat and
/// delete, checking each step.
fn self_test(console: &Channel, files: &Channel) {
    const PATH: &str = "/SHTEST.TXT";
    const FIRST: &str = "nodeos wrote this\n";
    const SECOND: &str = "and this\n";

    let mut failures = 0;
    let mut step = |ok: bool, what: &str| {
        if ok {
            write_text(console, "  ok   ");
        } else {
            write_text(console, "  FAIL ");
            failures += 1;
        }
        write_text(console, what);
        write_text(console, "\n");
        println!("fstest: {} {}", if ok { "ok  " } else { "FAIL" }, what);
    };

    write_text(console, "fstest: driving a file through its whole life\n");

    // Create and write.
    let node = create(files, PATH);
    step(node.is_ok(), "create /SHTEST.TXT");
    let Ok(node) = node else {
        return finish(console, failures);
    };
    let written = write_at(files, node.node, 0, FIRST.as_bytes());
    step(written == Ok(FIRST.len()), "write 18 bytes");

    // Read it back through a fresh open, so the test crosses the on-disk
    // representation rather than a cached entry.
    close(files, node.node);
    let reopened = open(files, PATH);
    step(reopened.as_ref().is_ok_and(|n| n.size == FIRST.len() as u64), "reopen, size matches");
    let Ok(reopened) = reopened else {
        return finish(console, failures);
    };
    let read = read_all(files, reopened.node);
    step(read.as_deref() == Some(FIRST.as_bytes()), "contents match");

    // Append.
    let appended = write_at(files, reopened.node, FIRST.len() as u64, SECOND.as_bytes());
    step(appended == Ok(SECOND.len()), "append 9 bytes");
    let read = read_all(files, reopened.node);
    let expected = {
        let mut all = Vec::from(FIRST.as_bytes());
        all.extend_from_slice(SECOND.as_bytes());
        all
    };
    step(read.as_deref() == Some(expected.as_slice()), "appended contents match");
    close(files, reopened.node);

    // It should be in the root listing.
    let listed = call(files, Message::new(proto::FS_LIST).with_payload(b"/"))
        .map(|reply| find_name(&reply.payload, "SHTEST.TXT"))
        .unwrap_or(false);
    step(listed, "appears in the root listing");

    // And stat should agree about its length.
    let sized = call(files, Message::new(proto::FS_STAT).with_payload(PATH.as_bytes()))
        .ok()
        .and_then(|reply| proto::decode::<proto::FsStat>(&reply.payload))
        .map(|stat| stat.size == expected.len() as u64)
        .unwrap_or(false);
    step(sized, "stat reports 27 bytes");

    // A directory, and a file inside it, to prove the tree is more than one
    // level deep.
    let made = call(files, Message::new(proto::FS_MKDIR).with_payload(b"/SHDIR")).is_ok();
    step(made, "mkdir /SHDIR");
    let nested = create(files, "/SHDIR/INNER.TXT");
    step(nested.is_ok(), "create /SHDIR/INNER.TXT");
    if let Ok(nested) = nested {
        let ok = write_at(files, nested.node, 0, b"nested\n") == Ok(7);
        step(ok, "write into the subdirectory");
        close(files, nested.node);
    }

    // Removing a directory that still has something in it must be refused --
    // this is the one destructive operation here and the check that it is not
    // recursive is worth having.
    let refused = call(files, Message::new(proto::FS_UNLINK).with_payload(b"/SHDIR")).is_err();
    step(refused, "rm refuses a non-empty directory");

    // Clean up everything except the file the host will check for.
    let removed = call(files, Message::new(proto::FS_UNLINK).with_payload(b"/SHDIR/INNER.TXT")).is_ok()
        && call(files, Message::new(proto::FS_UNLINK).with_payload(b"/SHDIR")).is_ok();
    step(removed, "remove the subdirectory");
    let gone = call(files, Message::new(proto::FS_OPEN).with_payload(b"/SHDIR/INNER.TXT")).is_err();
    step(gone, "the removed file is gone");

    finish(console, failures);
}

fn finish(console: &Channel, failures: u32) {
    if failures == 0 {
        write_text(console, "fstest: PASS\n");
        println!("fstest: PASS");
    } else {
        let mut line = String::new();
        let _ = core::fmt::write(&mut line, format_args!("fstest: FAIL ({failures} step(s))\n"));
        write_text(console, &line);
        println!("fstest: FAIL ({failures} steps)");
    }
}

/// Whether a NUL-separated listing contains `name`.
fn find_name(payload: &[u8], name: &str) -> bool {
    payload
        .split(|byte| *byte == 0)
        .filter_map(|record| core::str::from_utf8(record).ok())
        .any(|record| record.split('\t').next() == Some(name))
}

// ---------------------------------------------------------------------------
// The protocol, wrapped
// ---------------------------------------------------------------------------

/// A `channel_call` that turns an error reply into an `Err`.
///
/// The service reports failure in the message flags with a status in the
/// payload, which is the right thing on the wire and the wrong thing to write
/// at nine call sites.
fn call(files: &Channel, request: Message) -> Result<Message, Error> {
    let reply = files.call_with(request, 8192, 0)?;
    if reply.flags.contains(MessageFlags::ERROR) {
        return Err(proto::decode_error(&reply.payload).unwrap_or(Error::BadState));
    }
    Ok(reply)
}

fn open(files: &Channel, path: &str) -> Result<proto::FsOpenReply, Error> {
    let reply = call(files, Message::new(proto::FS_OPEN).with_payload(path.as_bytes()))?;
    proto::decode::<proto::FsOpenReply>(&reply.payload).ok_or(Error::BadState)
}

fn create(files: &Channel, path: &str) -> Result<proto::FsOpenReply, Error> {
    let reply = call(files, Message::new(proto::FS_CREATE).with_payload(path.as_bytes()))?;
    proto::decode::<proto::FsOpenReply>(&reply.payload).ok_or(Error::BadState)
}

fn close(files: &Channel, node: u64) {
    let request = proto::Encoded::new(proto::FsCloseRequest { node });
    let _ = call(files, Message::new(proto::FS_CLOSE).with_payload(request.bytes()));
}

fn write_at(files: &Channel, node: u64, offset: u64, data: &[u8]) -> Result<usize, Error> {
    let header = proto::Encoded::new(proto::FsWriteHeader {
        node,
        offset,
        length: data.len() as u32,
        _reserved: 0,
    });
    let mut payload = Vec::from(header.bytes());
    payload.extend_from_slice(data);
    let reply = call(files, Message::new(proto::FS_WRITE).with_payload(&payload))?;
    Ok(proto::decode::<proto::FsWriteHeader>(&reply.payload).map(|h| h.length as usize).unwrap_or(0))
}

fn read_all(files: &Channel, node: u64) -> Option<Vec<u8>> {
    let mut out = Vec::new();
    loop {
        let request = proto::Encoded::new(proto::FsReadRequest {
            node,
            offset: out.len() as u64,
            length: 4096,
            _reserved: 0,
        });
        let reply = call(files, Message::new(proto::FS_READ).with_payload(request.bytes())).ok()?;
        if reply.payload.is_empty() {
            return Some(out);
        }
        out.extend_from_slice(&reply.payload);
    }
}

fn report(console: &Channel, what: &str, error: Error) {
    let mut line = String::new();
    let _ = core::fmt::write(&mut line, format_args!("{what}: {error:?}\n"));
    write_text(console, &line);
}
