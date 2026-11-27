//! The filesystem calls the player makes: list a directory, peek at a header,
//! and read a whole file into memory.

use alloc::string::String;
use alloc::vec::Vec;

use libnode::abi::message::MessageFlags;
use libnode::abi::proto;
use libnode::abi::types::{MapFlags, MapWindow};
use libnode::mem::{Mapping, Vmo};
use libnode::{Channel, Message};

use crate::bap::{Header, HEADER_LEN};

/// The filesystem service's largest read.
const READ_CHUNK: u32 = 32 * 1024;

/// A file read into a memory object and mapped read-only.
pub struct File {
    // Field order is drop order: unmap before releasing the object.
    mapping: Mapping,
    _vmo: Vmo,
    len: usize,
}

impl File {
    pub fn bytes(&self) -> &[u8] {
        // SAFETY: the mapping is `len` readable bytes and lives as long as
        // `self`; nothing writes to it once it is mapped.
        unsafe { core::slice::from_raw_parts(self.mapping.base() as *const u8, self.len) }
    }
}

/// A `.BAP` file found on disk, for the browser.
pub struct Entry {
    pub path: String,
    pub name: String,
    pub size: u64,
    pub header: Option<Header>,
}

fn call(files: &Channel, request: Message, capacity: usize) -> Option<Message> {
    let reply = files.call_with(request, capacity, 0).ok()?;
    (!reply.flags.contains(MessageFlags::ERROR)).then_some(reply)
}

struct Open<'c> {
    files: &'c Channel,
    node: u64,
    size: u64,
}

impl<'c> Open<'c> {
    fn new(files: &'c Channel, path: &str) -> Result<Self, &'static str> {
        let reply = files
            .call_with(Message::new(proto::FS_OPEN).with_payload(path.as_bytes()), 4096, 0)
            .map_err(|_| "the filesystem did not answer")?;
        if reply.flags.contains(MessageFlags::ERROR) {
            return Err("no such file");
        }
        let open = proto::decode::<proto::FsOpenReply>(&reply.payload).ok_or("bad filesystem reply")?;
        if open.kind != proto::fs_kind::FILE {
            return Err("not a file");
        }
        Ok(Self { files, node: open.node, size: open.size })
    }

    fn read(&self, offset: u64, length: u32) -> Option<Message> {
        let request = proto::Encoded::new(proto::FsReadRequest { node: self.node, offset, length, _reserved: 0 });
        let reply = call(
            self.files,
            Message::new(proto::FS_READ).with_payload(request.bytes()),
            length as usize + 64,
        )?;
        (!reply.payload.is_empty()).then_some(reply)
    }
}

impl Drop for Open<'_> {
    fn drop(&mut self) {
        let close = proto::Encoded::new(proto::FsCloseRequest { node: self.node });
        let _ = self
            .files
            .call_with(Message::new(proto::FS_CLOSE).with_payload(close.bytes()), 64, 0);
    }
}

/// Read `path` into memory, calling `progress(done, total)` as it arrives.
pub fn load(files: &Channel, path: &str, mut progress: impl FnMut(u64, u64)) -> Result<File, &'static str> {
    let open = Open::new(files, path)?;
    let size = open.size;
    if size == 0 {
        return Err("the file is empty");
    }
    let vmo = Vmo::create(size, false).map_err(|_| "not enough memory for this file")?;
    let mut offset = 0u64;
    let mut drawn = 0u64;
    progress(0, size);
    while offset < size {
        let reply = open.read(offset, READ_CHUNK).ok_or("read failed")?;
        vmo.write(offset, &reply.payload).map_err(|_| "read failed")?;
        offset += reply.payload.len() as u64;
        // A redraw per megabyte: often enough to look alive, rarely enough
        // that drawing is not what the load spends its time on.
        if offset - drawn >= 1 << 20 || offset >= size {
            drawn = offset;
            progress(offset.min(size), size);
        }
    }
    let mapping = vmo
        .map(0, size, MapFlags::READ, MapWindow::MappedFile)
        .map_err(|_| "could not map the file")?;
    Ok(File { mapping, _vmo: vmo, len: size as usize })
}

/// Read just the header of `path`.
pub fn header(files: &Channel, path: &str) -> Option<Header> {
    let open = Open::new(files, path).ok()?;
    let reply = open.read(0, HEADER_LEN as u32)?;
    Header::parse(&reply.payload).ok()
}

/// Every `.BAP` file directly inside `dir`, sorted by name.
pub fn list(files: &Channel, dir: &str) -> Vec<Entry> {
    let mut entries = Vec::new();
    let Some(reply) = call(files, Message::new(proto::FS_LIST).with_payload(dir.as_bytes()), 16 * 1024) else {
        return entries;
    };
    // Records are NUL-separated: name, tab, `d` or `f`, tab, size.
    for record in reply.payload.split(|&b| b == 0) {
        let Ok(record) = core::str::from_utf8(record) else { continue };
        let mut fields = record.split('\t');
        let (Some(name), Some("f"), size) = (fields.next(), fields.next(), fields.next()) else { continue };
        if !name.to_ascii_uppercase().ends_with(".BAP") {
            continue;
        }
        let mut path = String::from(dir.trim_end_matches('/'));
        path.push('/');
        path.push_str(name);
        let header = header(files, &path);
        entries.push(Entry {
            path,
            name: String::from(name),
            size: size.and_then(|s| s.parse().ok()).unwrap_or(0),
            header,
        });
    }
    entries.sort_by(|a, b| a.name.cmp(&b.name));
    entries
}
