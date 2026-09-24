//! `fs` -- the filesystem service.
//!
//! Holds the virtio block device the boot launcher grants under
//! `CapClass::Device`, and serves `abi::proto`'s `FS_*` kinds over a channel
//! to anyone the launcher introduces. Nothing else in NodeOS can reach a
//! sector, and nothing else needs to: a client asks for a path and gets bytes.
//!
//! ```text
//!   shell ──FS_OPEN "/docs/notes.txt"──► fs ──► FAT16 ──► virtio-blk ──► disk
//!         ◄──node id, size──────────────
//!         ──FS_READ(node, offset, len)─►
//! ```
//!
//! # One process, two layers
//!
//! The block driver (`virtio`) and the filesystem (`fat`) are modules in this
//! process rather than two processes with a protocol between them. That is a
//! departure from how `compositor` and `console` are split, and it is
//! deliberate: the only client a block driver would ever have here is this
//! filesystem, so the split would buy isolation between two halves that fail
//! together anyway, at the cost of a second address space and a protocol on
//! the path of every sector. The layering is still real -- `fat` names no
//! port and `virtio` knows nothing about files -- it is just not a process
//! boundary. `docs/FILESYSTEM.md` argues this at greater length.
//!
//! # Node identifiers are not capabilities
//!
//! `FS_OPEN` returns a `u64` that indexes a table in this service, scoped to
//! the channel it was issued on. It is not a handle: a client cannot pass it
//! to another process and have it mean anything, and this service will not
//! honour one from a channel that did not open it. That is weaker than the
//! rest of NodeOS, where authority travels as handles, and it is the reason
//! `FS_CLOSE` has to exist -- see the note on it in `abi::proto`.

#![no_std]
#![no_main]

extern crate alloc;

mod fat;
mod virtio;

use alloc::string::String;
use alloc::vec::Vec;

use libnode::abi::message::MessageFlags;
use libnode::abi::proto::{self, fs_kind};
use libnode::abi::types::device_kind;
use libnode::abi::Signals;
use libnode::device::Device;
use libnode::wait::wait_any;
use libnode::{println, Channel, Env, Error, Message};

use fat::{Entry, Fat, Flavour, ATTR_DIRECTORY};
use virtio::VirtioBlk;

libnode::main! {
    fn main(env: Env) -> i32 {
        run(env)
    }
}

/// Largest read or write in one request.
///
/// The kernel's message payload limit is 64 KiB, and a request that asks for
/// more gets what fits rather than an error: a client reading a file loops
/// until it has what it wanted, and telling it "too much" would only mean it
/// had to know this constant.
const MAX_IO: usize = 32 * 1024;

/// An open file or directory, and who has it open.
struct Open {
    /// Identifier handed to the client.
    id: u64,
    /// Which client's table this belongs to.
    client: usize,
    entry: Entry,
}

struct Service {
    fat: Fat,
    clients: Vec<Channel>,
    open: Vec<Open>,
    next_id: u64,
}

fn run(mut env: Env) -> i32 {
    let Some(device_handle) = env.take_extra(0) else {
        println!("fs: no disk granted; exiting");
        return 1;
    };
    let Some(bootstrap) = env.take_bootstrap() else {
        println!("fs: no bootstrap channel; exiting");
        return 1;
    };

    let device = Device::from_handle(device_handle);
    match device.info() {
        Ok(info) if info.kind == device_kind::VIRTIO_BLK => {}
        Ok(info) => {
            println!("fs: device is kind {}, not a virtio disk; exiting", info.kind);
            return 1;
        }
        Err(error) => {
            println!("fs: device_info failed ({error:?}); exiting");
            return 1;
        }
    }

    let disk = match VirtioBlk::new(device) {
        Ok(disk) => disk,
        Err(error) => {
            println!("fs: could not initialise the disk ({error:?}); exiting");
            return 1;
        }
    };
    println!("fs: virtio disk with {} sectors ({} MiB)", disk.capacity(), disk.capacity() / 2048);

    let fat = match Fat::mount(disk) {
        Ok(fat) => fat,
        Err(error) => {
            println!("fs: no filesystem on the disk ({error:?}); exiting");
            return 1;
        }
    };
    println!(
        "fs: {} mounted, {} byte clusters",
        match fat.flavour() {
            Flavour::Fat16 => "FAT16",
            Flavour::Fat32 => "FAT32",
        },
        fat.cluster_bytes()
    );

    let mut service = Service { fat, clients: Vec::new(), open: Vec::new(), next_id: 1 };
    serve(&mut service, &bootstrap);
    0
}

fn serve(service: &mut Service, bootstrap: &Channel) {
    loop {
        let mut items = Vec::with_capacity(service.clients.len() + 1);
        items.push((bootstrap.handle().raw(), Signals::READABLE));
        for client in &service.clients {
            items.push((client.handle().raw(), Signals::READABLE.or(Signals::PEER_CLOSED)));
        }

        let Ok((index, observed)) = wait_any(&items, 0) else { continue };
        if index == 0 {
            accept_client(service, bootstrap);
            continue;
        }

        let client = index - 1;
        if observed.contains(Signals::READABLE) {
            // Sized for a whole write request rather than the 4 KiB default:
            // an `FS_WRITE` carries its bytes inline.
            if let Ok(message) = service.clients[client].receive_with(64 * 1024, 0, true) {
                handle_request(service, client, message);
            }
        } else if observed.contains(Signals::PEER_CLOSED) {
            disconnect(service, client);
        }
    }
}

fn accept_client(service: &mut Service, bootstrap: &Channel) {
    let Ok(message) = bootstrap.try_receive() else { return };
    if message.kind != proto::CONNECT || message.handles.is_empty() {
        println!("fs: ignoring bootstrap message of kind {:#x}", message.kind);
        return;
    }
    let mut handles = message.handles;
    service.clients.push(Channel::from_handle(handles.remove(0)));
    println!("fs: client {} connected", service.clients.len());
}

fn disconnect(service: &mut Service, client: usize) {
    // Everything the departing client had open goes with it, and every
    // *later* client's index shifts down by one -- the open table stores an
    // index into `clients`, so it has to be renumbered here or the next
    // client's reads would be answered from a dead client's files.
    service.open.retain(|open| open.client != client);
    for open in service.open.iter_mut() {
        if open.client > client {
            open.client -= 1;
        }
    }
    service.clients.remove(client);
    println!("fs: client disconnected");
}

// ---------------------------------------------------------------------------
// Requests
// ---------------------------------------------------------------------------

fn handle_request(service: &mut Service, client: usize, message: Message) {
    match message.kind {
        proto::FS_OPEN => open(service, client, message),
        proto::FS_CREATE => create(service, client, message, false),
        proto::FS_MKDIR => create(service, client, message, true),
        proto::FS_READ => read(service, client, message),
        proto::FS_WRITE => write(service, client, message),
        proto::FS_TRUNCATE => truncate(service, client, message),
        proto::FS_CLOSE => close(service, client, message),
        proto::FS_STAT => stat(service, client, message),
        proto::FS_LIST => list(service, client, message),
        proto::FS_UNLINK => unlink(service, client, message),
        proto::FS_STATFS => statfs(service, client, message),
        other => {
            println!("fs: unknown request kind {other:#x}");
            fail(service, client, &message, Error::InvalidArgument);
        }
    }
}

fn open(service: &mut Service, client: usize, message: Message) {
    let Some(path) = path_of(&message) else {
        fail(service, client, &message, Error::InvalidArgument);
        return;
    };
    let entry = match service.fat.lookup(&path) {
        Ok(entry) => entry,
        Err(error) => return fail(service, client, &message, error),
    };

    let id = service.next_id;
    service.next_id += 1;
    let reply = proto::Encoded::new(proto::FsOpenReply {
        node: id,
        size: entry.size as u64,
        kind: if entry.is_dir() { fs_kind::DIRECTORY } else { fs_kind::FILE },
        _reserved: 0,
    });
    service.open.push(Open { id, client, entry });
    succeed(service, client, &message, Message::new(proto::reply_kind(proto::FS_OPEN)).with_payload(reply.bytes()));
}

fn create(service: &mut Service, client: usize, message: Message, directory: bool) {
    let Some(path) = path_of(&message) else {
        fail(service, client, &message, Error::InvalidArgument);
        return;
    };
    // Creating over an existing file truncates it, which is what every caller
    // of "create" means and saves them an FS_TRUNCATE round trip.
    let result = match service.fat.lookup(&path) {
        Ok(mut existing) if !directory && !existing.is_dir() => {
            service.fat.truncate(&mut existing, 0).map(|()| existing)
        }
        Ok(_) => Err(Error::AlreadyExists),
        Err(_) => service.fat.create(&path, directory),
    };
    match result {
        Ok(entry) => {
            let id = service.next_id;
            service.next_id += 1;
            let reply = proto::Encoded::new(proto::FsOpenReply {
                node: id,
                size: entry.size as u64,
                kind: if directory { fs_kind::DIRECTORY } else { fs_kind::FILE },
                _reserved: 0,
            });
            service.open.push(Open { id, client, entry });
            succeed(
                service,
                client,
                &message,
                Message::new(proto::reply_kind(message.kind)).with_payload(reply.bytes()),
            );
        }
        Err(error) => fail(service, client, &message, error),
    }
}

fn read(service: &mut Service, client: usize, message: Message) {
    let Some(request) = proto::decode::<proto::FsReadRequest>(&message.payload) else {
        fail(service, client, &message, Error::InvalidArgument);
        return;
    };
    let Some(index) = find_open(service, client, request.node) else {
        fail(service, client, &message, Error::BadHandle);
        return;
    };
    let entry = service.open[index].entry.clone();
    let length = (request.length as usize).min(MAX_IO);
    match service.fat.read(&entry, request.offset, length) {
        Ok(bytes) => succeed(
            service,
            client,
            &message,
            Message::new(proto::reply_kind(proto::FS_READ)).with_payload(&bytes),
        ),
        Err(error) => fail(service, client, &message, error),
    }
}

fn write(service: &mut Service, client: usize, message: Message) {
    let Some(header) = proto::decode::<proto::FsWriteHeader>(&message.payload) else {
        fail(service, client, &message, Error::InvalidArgument);
        return;
    };
    let Some(index) = find_open(service, client, header.node) else {
        fail(service, client, &message, Error::BadHandle);
        return;
    };
    let start = core::mem::size_of::<proto::FsWriteHeader>();
    let available = message.payload.len().saturating_sub(start);
    let length = (header.length as usize).min(available).min(MAX_IO);
    let data: Vec<u8> = message.payload[start..start + length].to_vec();

    let mut entry = service.open[index].entry.clone();
    match service.fat.write(&mut entry, header.offset, &data) {
        Ok(written) => {
            // The entry's size and first cluster may both have changed; the
            // open table has to learn about it or the next read through this
            // node would stop at the old length.
            service.open[index].entry = entry;
            let reply = proto::Encoded::new(proto::FsWriteHeader {
                node: header.node,
                offset: header.offset,
                length: written as u32,
                _reserved: 0,
            });
            succeed(
                service,
                client,
                &message,
                Message::new(proto::reply_kind(proto::FS_WRITE)).with_payload(reply.bytes()),
            );
        }
        Err(error) => fail(service, client, &message, error),
    }
}

fn truncate(service: &mut Service, client: usize, message: Message) {
    let Some(request) = proto::decode::<proto::FsTruncateRequest>(&message.payload) else {
        fail(service, client, &message, Error::InvalidArgument);
        return;
    };
    let Some(index) = find_open(service, client, request.node) else {
        fail(service, client, &message, Error::BadHandle);
        return;
    };
    let mut entry = service.open[index].entry.clone();
    match service.fat.truncate(&mut entry, request.length) {
        Ok(()) => {
            service.open[index].entry = entry;
            succeed(service, client, &message, Message::new(proto::reply_kind(proto::FS_TRUNCATE)));
        }
        Err(error) => fail(service, client, &message, error),
    }
}

fn close(service: &mut Service, client: usize, message: Message) {
    if let Some(request) = proto::decode::<proto::FsCloseRequest>(&message.payload) {
        if let Some(index) = find_open(service, client, request.node) {
            service.open.remove(index);
        }
    }
    succeed(service, client, &message, Message::new(proto::reply_kind(proto::FS_CLOSE)));
}

fn stat(service: &mut Service, client: usize, message: Message) {
    let Some(path) = path_of(&message) else {
        fail(service, client, &message, Error::InvalidArgument);
        return;
    };
    let entry = match service.fat.lookup(&path) {
        Ok(entry) => entry,
        Err(error) => return fail(service, client, &message, error),
    };
    let entries = if entry.is_dir() {
        service.fat.list(entry.as_dir(service.fat.flavour())).map(|list| list.len()).unwrap_or(0)
    } else {
        0
    };
    let stat = proto::Encoded::new(proto::FsStat {
        node: entry.cluster as u64,
        size: entry.size as u64,
        entries: entries as u32,
        kind: if entry.is_dir() { fs_kind::DIRECTORY } else { fs_kind::FILE },
        // FAT records wall-clock timestamps and NodeOS has no wall clock to
        // compare them against, so these stay zero rather than reporting an
        // uptime as a creation date.
        created_ns: 0,
        modified_ns: 0,
    });
    succeed(service, client, &message, Message::new(proto::reply_kind(proto::FS_STAT)).with_payload(stat.bytes()));
}

fn list(service: &mut Service, client: usize, message: Message) {
    let Some(path) = path_of(&message) else {
        fail(service, client, &message, Error::InvalidArgument);
        return;
    };
    let entry = match service.fat.lookup(&path) {
        Ok(entry) => entry,
        Err(error) => return fail(service, client, &message, error),
    };
    if !entry.is_dir() {
        return fail(service, client, &message, Error::InvalidArgument);
    }
    let listing = match service.fat.list(entry.as_dir(service.fat.flavour())) {
        Ok(listing) => listing,
        Err(error) => return fail(service, client, &message, error),
    };

    // One NUL-separated record per entry: name, then a tab, then the type and
    // size. The protocol says "NUL-separated names"; the extra columns ride
    // along after a tab so a client that only wants names can cut at it, and
    // `ls` does not have to stat every entry it just listed.
    let mut payload = Vec::new();
    for item in listing {
        if item.name == "." || item.name == ".." {
            continue;
        }
        payload.extend_from_slice(item.name.as_bytes());
        payload.push(b'\t');
        payload.push(if item.attr & ATTR_DIRECTORY != 0 { b'd' } else { b'f' });
        payload.push(b'\t');
        let mut number = alloc::string::String::new();
        write_number(&mut number, item.size as u64);
        payload.extend_from_slice(number.as_bytes());
        payload.push(0);
    }
    succeed(service, client, &message, Message::new(proto::reply_kind(proto::FS_LIST)).with_payload(&payload));
}

fn unlink(service: &mut Service, client: usize, message: Message) {
    let Some(path) = path_of(&message) else {
        fail(service, client, &message, Error::InvalidArgument);
        return;
    };
    match service.fat.unlink(&path) {
        Ok(()) => succeed(service, client, &message, Message::new(proto::reply_kind(proto::FS_UNLINK))),
        Err(error) => fail(service, client, &message, error),
    }
}

fn statfs(service: &mut Service, client: usize, message: Message) {
    let (total, free, requests) = service.fat.usage().unwrap_or((0, 0, 0));
    let kind = match service.fat.flavour() {
        Flavour::Fat16 => u64::from_le_bytes(*b"FAT16\0\0\0"),
        Flavour::Fat32 => u64::from_le_bytes(*b"FAT32\0\0\0"),
    };
    let stat = proto::Encoded::new(proto::FsStatFs {
        nodes: service.open.iter().filter(|open| open.client == client).count() as u64,
        bytes: total,
        free,
        requests,
        block_size: service.fat.cluster_bytes() as u64,
        kind,
    });
    succeed(service, client, &message, Message::new(proto::reply_kind(proto::FS_STATFS)).with_payload(stat.bytes()));
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

/// The path a request names, as text.
fn path_of(message: &Message) -> Option<String> {
    let text = core::str::from_utf8(&message.payload).ok()?;
    Some(String::from(text.trim()))
}

/// Find an open node, refusing one that belongs to another client.
fn find_open(service: &Service, client: usize, node: u64) -> Option<usize> {
    service.open.iter().position(|open| open.id == node && open.client == client)
}

fn succeed(service: &Service, client: usize, request: &Message, reply: Message) {
    let _ = service.clients[client].reply(request, reply);
}

fn fail(service: &Service, client: usize, request: &Message, status: Error) {
    let mut reply = Message::new(proto::reply_kind(request.kind));
    reply.flags = MessageFlags::ERROR;
    reply.payload = Vec::from(proto::encode_error(status));
    let _ = service.clients[client].reply(request, reply);
}

/// Append a decimal number. `core::fmt` would do this, and pulling in its
/// machinery for one field in a listing is most of a kilobyte of code.
fn write_number(out: &mut String, mut value: u64) {
    if value == 0 {
        out.push('0');
        return;
    }
    let mut digits = [0u8; 20];
    let mut length = 0;
    while value > 0 {
        digits[length] = b'0' + (value % 10) as u8;
        value /= 10;
        length += 1;
    }
    for index in (0..length).rev() {
        out.push(digits[index] as char);
    }
}
