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

use libnode::abi::message::MessageFlags;
use libnode::abi::proto::{self, fs_kind};
use libnode::{println, Channel, Error, Message};

use crate::write_text;

/// Change directory only after the filesystem confirms it is a directory.
pub fn change_dir(console: &Channel, files: &Channel, cwd: &mut String, rest: &str) {
    let path = resolve(
        cwd,
        if rest.trim().is_empty() {
            "/"
        } else {
            rest.trim()
        },
    );
    match call(
        files,
        Message::new(proto::FS_STAT).with_payload(path.as_bytes()),
    ) {
        Ok(reply) => match proto::decode::<proto::FsStat>(&reply.payload) {
            Some(stat) if stat.kind == fs_kind::DIRECTORY => *cwd = path,
            Some(_) => write_text(console, "cd: not a directory\n"),
            None => write_text(console, "cd: malformed reply\n"),
        },
        Err(error) => report(console, &path, error),
    }
}

pub fn resolve(cwd: &str, input: &str) -> String {
    let mut components: Vec<String> = Vec::new();
    let source = if input.starts_with('/') { input } else { cwd };
    for component in source
        .split('/')
        .chain(if input.starts_with('/') { "" } else { input }.split('/'))
    {
        match component {
            "" | "." => {}
            ".." => {
                components.pop();
            }
            name => components.push(name.into()),
        }
    }
    let mut path = String::from("/");
    for (index, component) in components.iter().enumerate() {
        if index != 0 {
            path.push('/');
        }
        path.push_str(component);
    }
    path
}

// ---------------------------------------------------------------------------
// Commands
// ---------------------------------------------------------------------------

fn list(console: &Channel, files: &Channel, path: &str) {
    let reply = match call(
        files,
        Message::new(proto::FS_LIST).with_payload(path.as_bytes()),
    ) {
        Ok(reply) => reply,
        Err(error) => return report(console, path, error),
    };
    write_text(
        console,
        &alloc::format!(
            "\x1b[1m{}\x1b[0m\n\n\x1b[2m{:<32} {:>12}\x1b[0m\n",
            path,
            "Name",
            "Size"
        ),
    );
    let mut entries: Vec<(&str, bool, u64)> = reply
        .payload
        .split(|b| *b == 0)
        .filter_map(|record| {
            let text = core::str::from_utf8(record).ok()?;
            let mut fields = text.split('\t');
            let name = fields.next()?;
            if name.is_empty() {
                return None;
            }
            Some((
                name,
                fields.next() == Some("d"),
                fields.next().unwrap_or("0").parse().unwrap_or(0),
            ))
        })
        .collect();
    entries.sort_by(|a, b| b.1.cmp(&a.1).then(a.0.cmp(b.0)));
    for (name, dir, size) in &entries {
        let label = alloc::format!("{}{}", name, if *dir { "/" } else { "" });
        let size = if *dir {
            String::from("folder")
        } else if *size >= 1024 * 1024 {
            alloc::format!(
                "{}.{:01} MiB",
                size / (1024 * 1024),
                (size % (1024 * 1024)) * 10 / (1024 * 1024)
            )
        } else if *size >= 1024 {
            alloc::format!("{}.{:01} KiB", size / 1024, (size % 1024) * 10 / 1024)
        } else {
            alloc::format!("{} B", size)
        };
        let color = if *dir {
            "\x1b[36m"
        } else if name.to_ascii_lowercase().ends_with(".nxf") {
            "\x1b[32m"
        } else {
            "\x1b[0m"
        };
        write_text(
            console,
            &alloc::format!("{}{:<32}\x1b[0m {:>12}\n", color, label, size),
        );
    }
    write_text(
        console,
        &alloc::format!("\n\x1b[2m{} entries\x1b[0m\n", entries.len()),
    );
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
        let Ok(reply) = call(
            files,
            Message::new(proto::FS_READ).with_payload(request.bytes()),
        ) else {
            break;
        };
        if reply.payload.is_empty() {
            break;
        }
        offset += reply.payload.len() as u64;
        // Written a chunk at a time rather than assembled: a large file would
        // otherwise be held twice, once here and once in the console's own
        // buffer, for no benefit.
        write_text(
            console,
            core::str::from_utf8(&reply.payload).unwrap_or("<binary>"),
        );
    }
    close(files, node.node);
    write_text(console, "\n");
}

/// `write <path> <text>` and `append <path> <text>`.

fn stat(console: &Channel, files: &Channel, path: &str) {
    match call(
        files,
        Message::new(proto::FS_STAT).with_payload(path.as_bytes()),
    ) {
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
                    if stat.kind == fs_kind::DIRECTORY {
                        "directory"
                    } else {
                        "file"
                    },
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
            let name = core::str::from_utf8(&tag)
                .unwrap_or("FAT")
                .trim_end_matches('\0');
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
    step(
        reopened
            .as_ref()
            .is_ok_and(|n| n.size == FIRST.len() as u64),
        "reopen, size matches",
    );
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
    step(
        read.as_deref() == Some(expected.as_slice()),
        "appended contents match",
    );
    close(files, reopened.node);

    // It should be in the root listing.
    let listed = call(files, Message::new(proto::FS_LIST).with_payload(b"/"))
        .map(|reply| find_name(&reply.payload, "SHTEST.TXT"))
        .unwrap_or(false);
    step(listed, "appears in the root listing");

    // And stat should agree about its length.
    let sized = call(
        files,
        Message::new(proto::FS_STAT).with_payload(PATH.as_bytes()),
    )
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
    let refused = call(
        files,
        Message::new(proto::FS_UNLINK).with_payload(b"/SHDIR"),
    )
    .is_err();
    step(refused, "rm refuses a non-empty directory");

    // Clean up everything except the file the host will check for.
    let removed = call(
        files,
        Message::new(proto::FS_UNLINK).with_payload(b"/SHDIR/INNER.TXT"),
    )
    .is_ok()
        && call(
            files,
            Message::new(proto::FS_UNLINK).with_payload(b"/SHDIR"),
        )
        .is_ok();
    step(removed, "remove the subdirectory");
    let gone = call(
        files,
        Message::new(proto::FS_OPEN).with_payload(b"/SHDIR/INNER.TXT"),
    )
    .is_err();
    step(gone, "the removed file is gone");

    finish(console, failures);
}

fn finish(console: &Channel, failures: u32) {
    if failures == 0 {
        write_text(console, "fstest: PASS\n");
        println!("fstest: PASS");
    } else {
        let mut line = String::new();
        let _ = core::fmt::write(
            &mut line,
            format_args!("fstest: FAIL ({failures} step(s))\n"),
        );
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
    let reply = call(
        files,
        Message::new(proto::FS_OPEN).with_payload(path.as_bytes()),
    )?;
    proto::decode::<proto::FsOpenReply>(&reply.payload).ok_or(Error::BadState)
}

fn create(files: &Channel, path: &str) -> Result<proto::FsOpenReply, Error> {
    let reply = call(
        files,
        Message::new(proto::FS_CREATE).with_payload(path.as_bytes()),
    )?;
    proto::decode::<proto::FsOpenReply>(&reply.payload).ok_or(Error::BadState)
}

fn close(files: &Channel, node: u64) {
    let request = proto::Encoded::new(proto::FsCloseRequest { node });
    let _ = call(
        files,
        Message::new(proto::FS_CLOSE).with_payload(request.bytes()),
    );
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
    Ok(proto::decode::<proto::FsWriteHeader>(&reply.payload)
        .map(|h| h.length as usize)
        .unwrap_or(0))
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
        let reply = call(
            files,
            Message::new(proto::FS_READ).with_payload(request.bytes()),
        )
        .ok()?;
        if reply.payload.is_empty() {
            return Some(out);
        }
        out.extend_from_slice(&reply.payload);
    }
}

fn report(console: &Channel, what: &str, error: Error) {
    crate::show_error(console, what, error);
}

/// Execute file commands with parsed arguments, preserving quoted paths/text.
pub fn command(
    console: &Channel,
    files: &Channel,
    cwd: &str,
    name: &str,
    args: &[String],
) -> Result<(), Error> {
    let path = |at: usize| {
        args.get(at)
            .map(|s| resolve(cwd, s))
            .ok_or(Error::InvalidArgument)
    };
    match name {
        "ls" => list(
            console,
            files,
            &resolve(cwd, args.first().map(String::as_str).unwrap_or(".")),
        ),
        "cat" => cat(console, files, &path(0)?),
        "stat" => stat(console, files, &path(0)?),
        "df" => df(console, files),
        "fstest" => self_test(console, files),
        "mkdir" | "rm" => {
            let p = path(0)?;
            call(
                files,
                Message::new(if name == "mkdir" {
                    proto::FS_MKDIR
                } else {
                    proto::FS_UNLINK
                })
                .with_payload(p.as_bytes()),
            )?;
        }
        "touch" => {
            let p = path(0)?;
            match open(files, &p) {
                Ok(node) => close(files, node.node),
                Err(Error::NotFound) => {
                    let node = create(files, &p)?;
                    close(files, node.node)
                }
                Err(error) => return Err(error),
            }
        }
        "write" | "append" => {
            let p = path(0)?;
            let text = args.get(1..).unwrap_or(&[]).join(" ");
            let node = if name == "append" {
                match open(files, &p) {
                    Ok(node) => node,
                    Err(Error::NotFound) => create(files, &p)?,
                    Err(e) => return Err(e),
                }
            } else {
                create(files, &p)?
            };
            let result = (|| {
                let mut offset = if name == "append" { node.size } else { 0 };
                for chunk in text.as_bytes().chunks(32768) {
                    let written = write_at(files, node.node, offset, chunk)?;
                    if written != chunk.len() {
                        return Err(Error::BadState);
                    }
                    offset += written as u64;
                }
                Ok(())
            })();
            close(files, node.node);
            result?;
        }
        "cp" => {
            let force = args.first().map(|s| s == "-f").unwrap_or(false);
            let at = force as usize;
            copy_file(files, &path(at)?, &path(at + 1)?, force)?;
        }
        "grep" => {
            let pattern = args.first().ok_or(Error::InvalidArgument)?;
            let p = path(1)?;
            let mut line = Vec::new();
            let mut number = 1;
            let mut matched = 0;
            scan_file(files, &p, |bytes| {
                for byte in bytes {
                    if *byte == b'\n' {
                        if String::from_utf8_lossy(&line).contains(pattern.as_str()) {
                            matched += 1;
                            write_text(
                                console,
                                &alloc::format!("{number}:{}\n", String::from_utf8_lossy(&line)),
                            );
                        }
                        line.clear();
                        number += 1;
                    } else {
                        if line.len() >= 32768 {
                            return Err(Error::LimitExceeded);
                        }
                        line.push(*byte);
                    }
                }
                Ok(true)
            })?;
            if !line.is_empty() && String::from_utf8_lossy(&line).contains(pattern.as_str()) {
                matched += 1;
                write_text(
                    console,
                    &alloc::format!("{number}:{}\n", String::from_utf8_lossy(&line)),
                );
            }
            println!("shell: grep matched {matched} line(s)");
        }
        "more" => {
            let p = path(0)?;
            let reply = console.call_with(Message::new(proto::CONSOLE_GET_INFO), 128, 0)?;
            let info =
                proto::decode::<proto::ConsoleInfo>(&reply.payload).ok_or(Error::BadState)?;
            let rows = info.rows.saturating_sub(2).max(1) as usize;
            let cols = info.cols.max(1) as usize;
            let mut used = 0;
            let mut column = 0;
            let mut pending = Vec::new();
            let mut allowance = rows;
            scan_file(files, &p, |bytes| {
                for byte in bytes {
                    if *byte == 0 {
                        return Err(Error::NotSupported);
                    }
                    pending.push(*byte);
                    if *byte == b'\n' {
                        used += 1;
                        column = 0;
                    } else {
                        column += 1;
                        if column >= cols {
                            used += 1;
                            column = 0;
                        }
                    }
                    if used >= allowance {
                        write_text(console, &String::from_utf8_lossy(&pending));
                        pending.clear();
                        write_text(console, "-- more --  Space: page  Enter: line  q: quit");
                        println!("shell: pager waiting");
                        let key = loop {
                            let reply =
                                console.call_with(Message::new(proto::CONSOLE_READ_KEY), 128, 0)?;
                            let Some(key) = proto::decode::<proto::ConsoleKey>(&reply.payload)
                            else {
                                return Err(Error::BadState);
                            };
                            if key.pressed != 0
                                && key.modifiers & proto::wm_mod::CTRL != 0
                                && key.ascii == b'c'
                            {
                                break 3;
                            }
                            if key.pressed != 0
                                && matches!(key.ascii, b' ' | b'\n' | b'q' | b'Q' | 3)
                            {
                                break key.ascii;
                            }
                        };
                        write_text(console, "\n");
                        if matches!(key, b'q' | b'Q' | 3) {
                            return Ok(false);
                        }
                        used = 0;
                        allowance = if key == b'\n' { 1 } else { rows };
                    }
                }
                Ok(true)
            })?;
            if !pending.is_empty() {
                write_text(console, &String::from_utf8_lossy(&pending));
            }
            write_text(console, "\n");
        }
        _ => return Err(Error::NotSupported),
    }
    Ok(())
}
/// Read bounded command-file contents and close the handle on every path.
pub fn read_file(files: &Channel, path: &str, limit: usize) -> Result<Vec<u8>, Error> {
    let node = open(files, path)?;
    let result = (|| {
        if node.kind != fs_kind::FILE || node.size > limit as u64 {
            return Err(Error::LimitExceeded);
        }
        let mut out = Vec::with_capacity(node.size as usize);
        let mut offset = 0;
        while offset < node.size {
            let bytes = read_chunk(files, node.node, offset)?;
            if bytes.is_empty() {
                return Err(Error::BadState);
            }
            offset += bytes.len() as u64;
            out.extend_from_slice(&bytes);
        }
        Ok(out)
    })();
    close(files, node.node);
    result
}
fn read_chunk(files: &Channel, node: u64, offset: u64) -> Result<Vec<u8>, Error> {
    let header = proto::Encoded::new(proto::FsReadRequest {
        node,
        offset,
        length: 32768,
        _reserved: 0,
    });
    let reply = files.call_with(
        Message::new(proto::FS_READ).with_payload(header.bytes()),
        32768,
        0,
    )?;
    if reply.flags.contains(MessageFlags::ERROR) {
        return Err(proto::decode_error(&reply.payload).unwrap_or(Error::BadState));
    }
    Ok(reply.payload)
}
fn scan_file(
    files: &Channel,
    path: &str,
    mut consume: impl FnMut(&[u8]) -> Result<bool, Error>,
) -> Result<(), Error> {
    let node = open(files, path)?;
    let result = (|| {
        if node.kind != fs_kind::FILE {
            return Err(Error::InvalidArgument);
        }
        let mut offset = 0;
        while offset < node.size {
            let bytes = read_chunk(files, node.node, offset)?;
            if bytes.is_empty() {
                return Err(Error::BadState);
            }
            offset += bytes.len() as u64;
            if !consume(&bytes)? {
                break;
            }
        }
        Ok(())
    })();
    close(files, node.node);
    result
}
fn copy_file(files: &Channel, source: &str, destination: &str, force: bool) -> Result<(), Error> {
    if source.eq_ignore_ascii_case(destination) {
        return Err(Error::InvalidArgument);
    }
    // Validate the source before creating/truncating the destination.
    let source_node = open(files, source)?;
    let result = (|| {
        if source_node.kind != fs_kind::FILE {
            return Err(Error::InvalidArgument);
        }
        match open(files, destination) {
            Ok(existing) => {
                close(files, existing.node);
                if existing.kind != fs_kind::FILE {
                    return Err(Error::InvalidArgument);
                }
                if !force {
                    return Err(Error::AlreadyExists);
                }
            }
            Err(Error::NotFound) => {}
            Err(e) => return Err(e),
        }
        let dest = create(files, destination)?;
        let copied = (|| {
            let mut offset = 0;
            while offset < source_node.size {
                let bytes = read_chunk(files, source_node.node, offset)?;
                if bytes.is_empty() {
                    return Err(Error::BadState);
                }
                if write_at(files, dest.node, offset, &bytes)? != bytes.len() {
                    return Err(Error::BadState);
                }
                offset += bytes.len() as u64;
            }
            Ok(())
        })();
        close(files, dest.node);
        copied
    })();
    close(files, source_node.node);
    result
}
