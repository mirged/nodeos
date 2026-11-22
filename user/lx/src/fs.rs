//! Descriptors: the console as a terminal, and files over the filesystem
//! channel.
//!
//! A descriptor names an [`Object`] in a table owned by this process. Nothing
//! about it means anything to another process -- the capability behind it is
//! the channel, and the node identifier the filesystem service handed back is
//! scoped to that channel -- which is why `dup` is a table operation here and
//! not a request to anyone.
//!
//! Requests go out with raw `channel_call`s into caller-owned buffers rather
//! than through `libnode::Channel::call`: a `read` replies straight into the
//! program's own buffer, and libnode's allocator never reclaims the large
//! reply buffers `call` would allocate for each one.

use alloc::string::String;
use alloc::sync::Arc;
use alloc::vec::Vec;
use core::cell::UnsafeCell;
use core::sync::atomic::{AtomicU32, Ordering};

use libnode::sync::SpinLock;
use net::ready;
use nodeos_abi::message::{MessageDescriptor, MessageFlags};
use nodeos_abi::{proto, Syscall};

use crate::errno::*;
use crate::AT_FDCWD;

/// Largest read or write the filesystem service performs in one request.
const MAX_IO: usize = 32 * 1024;
const MAX_PATH: usize = 4096;

static CONSOLE: AtomicU32 = AtomicU32::new(0);
static FILES: AtomicU32 = AtomicU32::new(0);
static CWD: SpinLock<String> = SpinLock::new(String::new());

/// Console input mode, as the last `tcsetattr` set it: non-zero when a program
/// turned off `ICANON` and `ECHO` and is reading keys itself.
static CONSOLE_RAW: AtomicU32 = AtomicU32::new(0);

/// Record the session channels. Called once, by startup.
pub fn init(console: Option<u32>, files: Option<u32>, cwd: Option<&str>) {
    CONSOLE.store(console.unwrap_or(0), Ordering::Relaxed);
    FILES.store(files.unwrap_or(0), Ordering::Relaxed);
    // The launcher's working directory (the shell passes PWD), or the root.
    match cwd {
        Some(path) if path.starts_with('/') => CWD.lock().push_str(path),
        _ => CWD.lock().push('/'),
    }
    let mut table = TABLE.lock();
    for kind in [Tty::Input, Tty::Output, Tty::Error] {
        table.push(Some(Arc::new(SpinLock::new(Object::Tty(kind)))));
    }
}

// ---------------------------------------------------------------------------
// Channel calls
// ---------------------------------------------------------------------------

/// A request buffer: a header and up to [`MAX_IO`] bytes of data. One is
/// enough while programs are single-threaded.
struct Scratch(UnsafeCell<[u8; MAX_IO + 256]>);
// SAFETY: used only under `SCRATCH_LOCK`.
unsafe impl Sync for Scratch {}
static SCRATCH: Scratch = Scratch(UnsafeCell::new([0; MAX_IO + 256]));
static SCRATCH_LOCK: SpinLock<()> = SpinLock::new(());

/// Send `kind` with the concatenation of `parts` as its payload, and receive
/// the reply payload into `reply`. Returns the reply's length, or the errno
/// the failure means.
pub(crate) fn call(
    channel: u32,
    kind: u32,
    parts: &[&[u8]],
    reply: &mut [u8],
) -> Result<usize, i64> {
    if channel == 0 {
        return Err(EBADF);
    }
    let _guard = SCRATCH_LOCK.lock();
    // SAFETY: `SCRATCH_LOCK` is held, so this is the only reference.
    let scratch = unsafe { &mut *SCRATCH.0.get() };
    let mut len = 0;
    for part in parts {
        if len + part.len() > scratch.len() {
            return Err(EINVAL);
        }
        scratch[len..len + part.len()].copy_from_slice(part);
        len += part.len();
    }
    // An error reply carries a four-byte status, which needs somewhere to
    // land even when the caller expects no payload.
    let mut status = [0u8; 4];
    let (reply_ptr, reply_cap) = if reply.len() >= 4 {
        (reply.as_mut_ptr(), reply.len())
    } else {
        (status.as_mut_ptr(), status.len())
    };
    let request = MessageDescriptor {
        kind,
        flags: MessageFlags::REQUEST.0,
        txid: 0,
        payload: scratch.as_ptr() as u64,
        payload_len: len as u32,
        handle_count: 0,
        handles: 0,
    };
    let mut response = MessageDescriptor {
        kind: 0,
        flags: 0,
        txid: 0,
        payload: reply_ptr as u64,
        payload_len: reply_cap as u32,
        handle_count: 0,
        handles: 0,
    };
    // SAFETY: both descriptors point at live buffers of the declared sizes.
    let raw = unsafe {
        libnode::sys::syscall3(
            Syscall::ChannelCall,
            channel as u64,
            &request as *const _ as u64,
            &mut response as *mut _ as u64,
        )
    };
    libnode::sys::ok(raw).map_err(from_error)?;
    let got = response.payload_len as usize;
    if MessageFlags(response.flags).contains(MessageFlags::ERROR) {
        // SAFETY: `reply_ptr` has at least four bytes.
        let bytes = unsafe { core::slice::from_raw_parts(reply_ptr, 4) };
        return Err(proto::decode_error(bytes).map(from_error).unwrap_or(EIO));
    }
    // A caller that passed no buffer wanted only success or failure.
    if reply.len() < 4 {
        return Ok(0);
    }
    Ok(got)
}

fn files() -> u32 {
    FILES.load(Ordering::Relaxed)
}

fn decode<T: Copy>(bytes: &[u8]) -> Result<T, i64> {
    proto::decode::<T>(bytes).ok_or(EIO)
}

fn fs_open(path: &str) -> Result<proto::FsOpenReply, i64> {
    let mut reply = [0u8; 64];
    let len = call(files(), proto::FS_OPEN, &[path.as_bytes()], &mut reply)?;
    decode(&reply[..len])
}

fn fs_create(path: &str, directory: bool) -> Result<proto::FsOpenReply, i64> {
    let kind = if directory { proto::FS_MKDIR } else { proto::FS_CREATE };
    let mut reply = [0u8; 64];
    let len = call(files(), kind, &[path.as_bytes()], &mut reply)?;
    decode(&reply[..len])
}

fn fs_close(node: u64) {
    let request = proto::Encoded::new(proto::FsCloseRequest { node });
    let _ = call(files(), proto::FS_CLOSE, &[request.bytes()], &mut []);
}

fn fs_stat(path: &str) -> Result<proto::FsStat, i64> {
    let mut reply = [0u8; 64];
    let len = call(files(), proto::FS_STAT, &[path.as_bytes()], &mut reply)?;
    decode(&reply[..len])
}

fn fs_read(node: u64, offset: u64, out: &mut [u8]) -> Result<usize, i64> {
    let mut done = 0;
    while done < out.len() {
        let want = (out.len() - done).min(MAX_IO);
        let request = proto::Encoded::new(proto::FsReadRequest {
            node,
            offset: offset + done as u64,
            length: want as u32,
            _reserved: 0,
        });
        let got = call(files(), proto::FS_READ, &[request.bytes()], &mut out[done..done + want])?;
        done += got;
        if got < want {
            break;
        }
    }
    Ok(done)
}

fn fs_write(node: u64, offset: u64, data: &[u8]) -> Result<usize, i64> {
    let mut done = 0;
    while done < data.len() {
        let chunk = &data[done..(done + MAX_IO).min(data.len())];
        let header = proto::Encoded::new(proto::FsWriteHeader {
            node,
            offset: offset + done as u64,
            length: chunk.len() as u32,
            _reserved: 0,
        });
        let mut reply = [0u8; 64];
        let len = call(files(), proto::FS_WRITE, &[header.bytes(), chunk], &mut reply)?;
        let written = decode::<proto::FsWriteHeader>(&reply[..len])?.length as usize;
        done += written;
        if written < chunk.len() {
            break;
        }
    }
    Ok(done)
}

fn fs_truncate(node: u64, length: u64) -> Result<(), i64> {
    let request = proto::Encoded::new(proto::FsTruncateRequest { node, length });
    call(files(), proto::FS_TRUNCATE, &[request.bytes()], &mut []).map(|_| ())
}

// ---------------------------------------------------------------------------
// The descriptor table
// ---------------------------------------------------------------------------

#[derive(Clone, Copy, PartialEq, Eq)]
enum Tty {
    Input,
    Output,
    Error,
}

struct File {
    node: u64,
    path: String,
    offset: u64,
    size: u64,
    flags: i32,
}

struct Dir {
    node: u64,
    path: String,
    /// `(name, is_directory)`, fetched on the first `getdents64`.
    entries: Option<Vec<(String, bool)>>,
    position: usize,
}

/// A file this process answers itself, such as `/etc/resolv.conf`: the bytes
/// come from the network service, not from the disk.
struct MemFile {
    content: Vec<u8>,
    offset: usize,
}

enum Object {
    Tty(Tty),
    File(File),
    Dir(Dir),
    Socket(crate::net::Socket),
    Mem(MemFile),
}

impl Drop for Object {
    fn drop(&mut self) {
        match self {
            Object::File(file) => fs_close(file.node),
            Object::Dir(dir) => fs_close(dir.node),
            Object::Socket(socket) => crate::net::close(socket),
            Object::Tty(_) | Object::Mem(_) => {}
        }
    }
}

type Entry = Arc<SpinLock<Object>>;

static TABLE: SpinLock<Vec<Option<Entry>>> = SpinLock::new(Vec::new());

/// Highest descriptor plus one, like `RLIMIT_NOFILE`.
const MAX_FDS: usize = 256;

fn get(fd: i32) -> Result<Entry, i64> {
    let table = TABLE.lock();
    table.get(fd as usize).and_then(|slot| slot.clone()).ok_or(EBADF)
}

fn install_at_least(entry: Entry, minimum: usize) -> i64 {
    let mut table = TABLE.lock();
    let free = (minimum..table.len()).find(|&fd| table[fd].is_none());
    let fd = match free {
        Some(fd) => fd,
        None => {
            let fd = table.len().max(minimum);
            if fd >= MAX_FDS {
                return -EMFILE;
            }
            table.resize(fd + 1, None);
            fd
        }
    };
    table[fd] = Some(entry);
    fd as i64
}

pub fn close(fd: i32) -> i64 {
    let entry = {
        let mut table = TABLE.lock();
        match table.get_mut(fd as usize) {
            Some(slot @ Some(_)) => slot.take(),
            _ => return -EBADF,
        }
    };
    // The object (and its FS_CLOSE) goes when the last descriptor sharing it
    // does, outside the table lock.
    drop(entry);
    0
}

/// Put a freshly opened socket in the descriptor table, and return its number.
pub fn install_socket(socket: crate::net::Socket) -> i64 {
    install_at_least(Arc::new(SpinLock::new(Object::Socket(socket))), 0)
}

pub fn dup(fd: i32, minimum: usize) -> i64 {
    match get(fd) {
        Ok(entry) => install_at_least(entry, minimum),
        Err(errno) => -errno,
    }
}

pub fn dup2(old: i32, new: i32) -> i64 {
    let entry = match get(old) {
        Ok(entry) => entry,
        Err(errno) => return -errno,
    };
    if old == new {
        return new as i64;
    }
    if new < 0 || new as usize >= MAX_FDS {
        return -EBADF;
    }
    let previous = {
        let mut table = TABLE.lock();
        if table.len() <= new as usize {
            table.resize(new as usize + 1, None);
        }
        table[new as usize].replace(entry)
    };
    drop(previous);
    new as i64
}

// ---------------------------------------------------------------------------
// Paths
// ---------------------------------------------------------------------------

unsafe fn c_str<'a>(pointer: *const u8) -> Result<&'a str, i64> {
    if pointer.is_null() {
        return Err(EFAULT);
    }
    let mut len = 0;
    // SAFETY: the Linux contract: a NUL-terminated string.
    while unsafe { pointer.add(len).read() } != 0 {
        len += 1;
        if len >= MAX_PATH {
            return Err(ENAMETOOLONG);
        }
    }
    // SAFETY: `len` bytes were just read.
    let bytes = unsafe { core::slice::from_raw_parts(pointer, len) };
    core::str::from_utf8(bytes).map_err(|_| EINVAL)
}

/// Turn `path`, relative to `dirfd`, into the absolute, normalised path the
/// filesystem service expects.
fn resolve(dirfd: i32, path: &str) -> Result<String, i64> {
    let base = if path.starts_with('/') {
        String::from("/")
    } else if dirfd == AT_FDCWD {
        CWD.lock().clone()
    } else {
        let entry = get(dirfd)?;
        let object = entry.lock();
        match &*object {
            Object::Dir(dir) => dir.path.clone(),
            _ => return Err(ENOTDIR),
        }
    };
    let mut parts: Vec<&str> = Vec::new();
    for part in base.split('/').chain(path.split('/')) {
        match part {
            "" | "." => {}
            ".." => {
                parts.pop();
            }
            name => parts.push(name),
        }
    }
    let mut out = String::new();
    for part in &parts {
        out.push('/');
        out.push_str(part);
    }
    if out.is_empty() {
        out.push('/');
    }
    Ok(out)
}

// ---------------------------------------------------------------------------
// open / read / write
// ---------------------------------------------------------------------------

const O_ACCMODE: i32 = 3;
const O_RDONLY: i32 = 0;
const O_CREAT: i32 = 0x40;
const O_EXCL: i32 = 0x80;
const O_TRUNC: i32 = 0x200;
const O_APPEND: i32 = 0x400;
const O_DIRECTORY: i32 = 0x10000;
const O_RDWR: i32 = 2;
const O_NONBLOCK: i32 = 0o4000;

pub unsafe fn openat(dirfd: i32, path: *const u8, flags: i32) -> i64 {
    // SAFETY: forwarded contract.
    match unsafe { c_str(path) }.and_then(|path| open_path(dirfd, path, flags)) {
        Ok(fd) => fd,
        Err(errno) => -errno,
    }
}

fn open_path(dirfd: i32, path: &str, flags: i32) -> Result<i64, i64> {
    let path = resolve(dirfd, path)?;
    // Files the network service supplies, for musl's resolver: there is no
    // `/etc` on the disk, and these two are the only paths a program reads
    // before it can reach the network at all.
    if let Some(content) = crate::net::synthetic_file(&path) {
        if flags & O_ACCMODE != O_RDONLY {
            return Err(EACCES);
        }
        let object = Object::Mem(MemFile { content: content.into_bytes(), offset: 0 });
        return Ok(install_at_least(Arc::new(SpinLock::new(object)), 0));
    }
    let node = match fs_open(&path) {
        Ok(node) => {
            if flags & O_CREAT != 0 && flags & O_EXCL != 0 {
                fs_close(node.node);
                return Err(EEXIST);
            }
            node
        }
        Err(ENOENT) if flags & O_CREAT != 0 => fs_create(&path, false)?,
        Err(errno) => return Err(errno),
    };
    let object = if node.kind == proto::fs_kind::DIRECTORY {
        if flags & O_ACCMODE != O_RDONLY {
            fs_close(node.node);
            return Err(EISDIR);
        }
        Object::Dir(Dir { node: node.node, path, entries: None, position: 0 })
    } else {
        if flags & O_DIRECTORY != 0 {
            fs_close(node.node);
            return Err(ENOTDIR);
        }
        let mut size = node.size;
        if flags & O_TRUNC != 0 && flags & O_ACCMODE != O_RDONLY && size != 0 {
            if let Err(errno) = fs_truncate(node.node, 0) {
                fs_close(node.node);
                return Err(errno);
            }
            size = 0;
        }
        Object::File(File { node: node.node, path, offset: 0, size, flags })
    };
    Ok(install_at_least(Arc::new(SpinLock::new(object)), 0))
}

pub unsafe fn read(fd: i32, buf: *mut u8, len: usize) -> i64 {
    // SAFETY: the Linux contract: `buf` has `len` writable bytes.
    let out = unsafe { core::slice::from_raw_parts_mut(buf, len) };
    let entry = match get(fd) {
        Ok(entry) => entry,
        Err(errno) => return -errno,
    };
    let mut object = entry.lock();
    match &mut *object {
        Object::Tty(Tty::Input) => read_console(out),
        Object::Tty(_) => -EBADF,
        Object::Dir(_) => -EISDIR,
        Object::Socket(socket) => crate::net::recv(socket, out, None),
        Object::Mem(mem) => {
            let available = mem.content.len().saturating_sub(mem.offset);
            let got = available.min(out.len());
            out[..got].copy_from_slice(&mem.content[mem.offset..mem.offset + got]);
            mem.offset += got;
            got as i64
        }
        Object::File(file) => match fs_read(file.node, file.offset, out) {
            Ok(got) => {
                file.offset += got as u64;
                got as i64
            }
            Err(errno) => -errno,
        },
    }
}

pub unsafe fn pread(fd: i32, buf: *mut u8, len: usize, offset: u64) -> i64 {
    // SAFETY: as `read`.
    let out = unsafe { core::slice::from_raw_parts_mut(buf, len) };
    let entry = match get(fd) {
        Ok(entry) => entry,
        Err(errno) => return -errno,
    };
    let object = entry.lock();
    match &*object {
        Object::File(file) => fs_read(file.node, offset, out).map_or_else(|e| -e, |n| n as i64),
        _ => -ESPIPE,
    }
}

pub unsafe fn write(fd: i32, buf: *const u8, len: usize) -> i64 {
    // SAFETY: the Linux contract: `buf` has `len` readable bytes.
    let data = unsafe { core::slice::from_raw_parts(buf, len) };
    let entry = match get(fd) {
        Ok(entry) => entry,
        Err(errno) => return -errno,
    };
    let mut object = entry.lock();
    match &mut *object {
        Object::Tty(Tty::Input) => -EBADF,
        Object::Tty(kind) => write_console(*kind, data),
        Object::Dir(_) => -EISDIR,
        Object::Socket(socket) => crate::net::send(socket, data, None),
        Object::Mem(_) => -EACCES,
        Object::File(file) => {
            if file.flags & O_ACCMODE == O_RDONLY {
                return -EBADF;
            }
            if file.flags & O_APPEND != 0 {
                file.offset = file.size;
            }
            match fs_write(file.node, file.offset, data) {
                Ok(written) => {
                    file.offset += written as u64;
                    file.size = file.size.max(file.offset);
                    written as i64
                }
                Err(errno) => -errno,
            }
        }
    }
}

pub unsafe fn pwrite(fd: i32, buf: *const u8, len: usize, offset: u64) -> i64 {
    // SAFETY: as `write`.
    let data = unsafe { core::slice::from_raw_parts(buf, len) };
    let entry = match get(fd) {
        Ok(entry) => entry,
        Err(errno) => return -errno,
    };
    let mut object = entry.lock();
    match &mut *object {
        Object::File(file) => match fs_write(file.node, offset, data) {
            Ok(written) => {
                file.size = file.size.max(offset + written as u64);
                written as i64
            }
            Err(errno) => -errno,
        },
        _ => -ESPIPE,
    }
}

/// `struct iovec`.
#[repr(C)]
pub struct IoVec {
    base: *mut u8,
    len: usize,
}

pub unsafe fn readv(fd: i32, iov: *const IoVec, count: usize) -> i64 {
    let mut total = 0i64;
    for index in 0..count {
        // SAFETY: the Linux contract: `count` iovecs.
        let vec = unsafe { &*iov.add(index) };
        // SAFETY: forwarded contract.
        let got = unsafe { read(fd, vec.base, vec.len) };
        if got < 0 {
            return if total > 0 { total } else { got };
        }
        total += got;
        if (got as usize) < vec.len {
            break;
        }
    }
    total
}

pub unsafe fn writev(fd: i32, iov: *const IoVec, count: usize) -> i64 {
    let mut total = 0i64;
    for index in 0..count {
        // SAFETY: the Linux contract: `count` iovecs.
        let vec = unsafe { &*iov.add(index) };
        if vec.len == 0 {
            continue;
        }
        // SAFETY: forwarded contract.
        let wrote = unsafe { write(fd, vec.base, vec.len) };
        if wrote < 0 {
            return if total > 0 { total } else { wrote };
        }
        total += wrote;
        if (wrote as usize) < vec.len {
            break;
        }
    }
    total
}

pub fn lseek(fd: i32, offset: i64, whence: i32) -> i64 {
    let entry = match get(fd) {
        Ok(entry) => entry,
        Err(errno) => return -errno,
    };
    let mut object = entry.lock();
    if let Object::Mem(mem) = &mut *object {
        let base = match whence {
            0 => 0,
            1 => mem.offset as i64,
            2 => mem.content.len() as i64,
            _ => return -EINVAL,
        };
        let target = base + offset;
        if target < 0 {
            return -EINVAL;
        }
        mem.offset = target as usize;
        return target;
    }
    let Object::File(file) = &mut *object else { return -ESPIPE };
    let base = match whence {
        0 => 0,
        1 => file.offset as i64,
        2 => file.size as i64,
        _ => return -EINVAL,
    };
    let target = base + offset;
    if target < 0 {
        return -EINVAL;
    }
    file.offset = target as u64;
    target
}

pub fn ftruncate(fd: i32, length: u64) -> i64 {
    let entry = match get(fd) {
        Ok(entry) => entry,
        Err(errno) => return -errno,
    };
    let mut object = entry.lock();
    let Object::File(file) = &mut *object else { return -EINVAL };
    match fs_truncate(file.node, length) {
        Ok(()) => {
            file.size = length;
            0
        }
        Err(errno) => -errno,
    }
}

/// Copy `len` bytes of file `fd` at `offset` into memory object `vmo`, for a
/// private file mapping. Bytes past the end of the file stay zero.
pub fn fill_vmo(fd: i32, vmo: u64, offset: u64, len: u64) -> Result<(), i64> {
    let entry = get(fd)?;
    let object = entry.lock();
    let Object::File(file) = &*object else { return Err(EACCES) };
    let mut chunk = alloc::vec![0u8; MAX_IO];
    let mut done = 0u64;
    while done < len {
        let want = (len - done).min(MAX_IO as u64) as usize;
        let got = fs_read(file.node, offset + done, &mut chunk[..want])?;
        if got == 0 {
            break;
        }
        // SAFETY: `chunk[..got]` is a valid readable slice.
        let raw = unsafe {
            libnode::sys::syscall6(Syscall::MemoryTransfer, vmo, done, chunk.as_ptr() as u64, got as u64, 1, 0)
        };
        libnode::sys::ok(raw).map_err(from_error)?;
        done += got as u64;
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// The console as a terminal
// ---------------------------------------------------------------------------

/// What remains of the last line the console returned, for reads smaller
/// than a line.
static PENDING_INPUT: SpinLock<(Vec<u8>, usize)> = SpinLock::new((Vec::new(), 0));

fn read_console(out: &mut [u8]) -> i64 {
    let mut pending = PENDING_INPUT.lock();
    if pending.1 >= pending.0.len() {
        if console_raw() {
            // Raw mode, as `tcsetattr` asked for: the console hands over one
            // key press at a time, does no editing and echoes nothing. Control
            // keys, arrows and the delete/home/end block become the bytes a
            // Linux program expects, because the program is doing its own
            // editing and drawing.
            let mut key = [0u8; 32];
            let got = match call(
                CONSOLE.load(Ordering::Relaxed),
                proto::CONSOLE_READ_KEY,
                &[],
                &mut key,
            ) {
                Ok(got) => got,
                Err(_) => return 0, // a console that went away is end of file
            };
            let key = match decode::<proto::ConsoleKey>(&key[..got]) {
                Ok(key) => key,
                Err(errno) => return errno,
            };
            let mut bytes = [0u8; 8];
            let n = key_bytes(key, &mut bytes);
            *pending = (bytes[..n].to_vec(), 0);
        } else {
            // Canonical mode, as the console provides it: it edits and echoes
            // the line itself and replies when Enter is pressed.
            let mut line = alloc::vec![0u8; 1024];
            let got = match call(
                CONSOLE.load(Ordering::Relaxed),
                proto::CONSOLE_READ_LINE,
                &[],
                &mut line,
            ) {
                Ok(got) => got,
                Err(_) => return 0, // a console that went away is end of file
            };
            line.truncate(got);
            line.push(b'\n');
            *pending = (line, 0);
        }
    }
    let available = &pending.0[pending.1..];
    let n = available.len().min(out.len());
    out[..n].copy_from_slice(&available[..n]);
    pending.1 += n;
    n as i64
}

/// The bytes one key press means to a Linux program reading a raw terminal.
///
/// Printable keys are their character, with control held reducing a letter to
/// the control code it has always been. Enter is CR because the program is
/// reading keys, not lines: a canonical read is what turns it into a newline.
/// The keys with no character of their own become the ANSI sequences a
/// full-screen program parses (`\x1b[A` and friends).
fn key_bytes(key: proto::ConsoleKey, out: &mut [u8; 8]) -> usize {
    let sequence: &[u8] = if key.extended != 0 {
        match key.scancode {
            0x48 => b"\x1b[A", // Up
            0x50 => b"\x1b[B", // Down
            0x4D => b"\x1b[C", // Right
            0x4B => b"\x1b[D", // Left
            0x47 => b"\x1b[H", // Home
            0x4F => b"\x1b[F", // End
            0x52 => b"\x1b[2~", // Insert
            0x53 => b"\x1b[3~", // Delete
            0x49 => b"\x1b[5~", // Page Up
            0x51 => b"\x1b[6~", // Page Down
            _ => b"",
        }
    } else {
        match key.scancode {
            0x1C => b"\r",  // Enter
            0x0E => b"\x7f", // Backspace deletes, in a raw Linux terminal
            0x0F => b"\t",  // Tab
            0x01 => b"\x1b", // Escape
            _ if key.ascii != 0 => {
                let byte = if key.modifiers & proto::wm_mod::CTRL != 0 {
                    key.ascii & 0x1f
                } else {
                    key.ascii
                };
                out[0] = byte;
                return 1;
            }
            _ => b"",
        }
    };
    out[..sequence.len()].copy_from_slice(sequence);
    sequence.len()
}

/// Everything written to stdout and stderr is also copied, a line at a time,
/// to the kernel log as `lx: | <line>`. That is what lets a headless test read
/// a program's output, and it is the only record when the console is gone.
static MIRROR: SpinLock<Vec<u8>> = SpinLock::new(Vec::new());

fn write_console(_kind: Tty, data: &[u8]) -> i64 {
    // The console takes UTF-8 and drops anything else; a program writing
    // arbitrary bytes still gets them through, with non-UTF-8 shown as '?'.
    for chunk in data.chunks(MAX_IO) {
        let text: alloc::borrow::Cow<'_, [u8]> = match core::str::from_utf8(chunk) {
            Ok(_) => alloc::borrow::Cow::Borrowed(chunk),
            Err(_) => chunk.iter().map(|&b| if b < 0x80 { b } else { b'?' }).collect::<Vec<u8>>().into(),
        };
        let _ = call(CONSOLE.load(Ordering::Relaxed), proto::CONSOLE_WRITE_TEXT, &[&text], &mut []);
    }
    let mut mirror = MIRROR.lock();
    for &byte in data {
        if byte == b'\n' {
            emit_mirror(&mut mirror);
        } else if mirror.len() < 480 {
            mirror.push(byte);
        }
    }
    data.len() as i64
}

fn emit_mirror(line: &mut Vec<u8>) {
    let text = String::from_utf8_lossy(line);
    libnode::println!("lx: | {text}");
    line.clear();
}

/// Write out a final unterminated line, at exit.
pub fn flush_mirrors() {
    let mut mirror = MIRROR.lock();
    if !mirror.is_empty() {
        emit_mirror(&mut mirror);
    }
}

// ---------------------------------------------------------------------------
// Sockets
// ---------------------------------------------------------------------------

const ENOTSOCK: i64 = 88;
const EDESTADDRREQ: i64 = 89;
const EOPNOTSUPP: i64 = 95;

/// Run `f` on the socket behind `fd`.
fn with_socket<T>(fd: i32, f: impl FnOnce(&mut crate::net::Socket) -> T) -> Result<T, i64> {
    let entry = get(fd)?;
    let mut object = entry.lock();
    let Object::Socket(socket) = &mut *object else { return Err(ENOTSOCK) };
    Ok(f(socket))
}

pub unsafe fn connect_socket(fd: i32, addr: *const u8, len: u32) -> i64 {
    // SAFETY: the Linux contract: `addr` has `len` bytes.
    let address = match unsafe { crate::net::read_address(addr, len) } {
        Ok(address) => address,
        Err(errno) => return -errno,
    };
    match with_socket(fd, |socket| crate::net::connect(socket, address)) {
        Ok(result) => result,
        Err(errno) => -errno,
    }
}

pub unsafe fn sendto(
    fd: i32,
    buf: *const u8,
    len: usize,
    addr: *const u8,
    addrlen: u32,
) -> i64 {
    // SAFETY: the Linux contract: `buf` has `len` readable bytes.
    let data = unsafe { core::slice::from_raw_parts(buf, len) };
    let to = if addr.is_null() {
        None
    } else {
        // SAFETY: the Linux contract: `addr` has `addrlen` bytes.
        match unsafe { crate::net::read_address(addr, addrlen) } {
            Ok(address) => Some(address),
            Err(errno) => return -errno,
        }
    };
    match with_socket(fd, |socket| crate::net::send(socket, data, to)) {
        Ok(result) => result,
        Err(errno) => -errno,
    }
}

pub unsafe fn recvfrom(
    fd: i32,
    buf: *mut u8,
    len: usize,
    addr: *mut u8,
    addrlen: *mut u32,
) -> i64 {
    // SAFETY: the Linux contract: `buf` has `len` writable bytes.
    let out = unsafe { core::slice::from_raw_parts_mut(buf, len) };
    let mut from = crate::net::Address { address: [0; 4], port: 0 };
    let got = match with_socket(fd, |socket| crate::net::recv(socket, out, Some(&mut from))) {
        Ok(result) => result,
        Err(errno) => return -errno,
    };
    if got >= 0 && !addr.is_null() && !addrlen.is_null() {
        if from.address != [0; 4] {
            // SAFETY: the Linux contract: `addr` has room for a sockaddr_in
            // and `addrlen` is a writable socklen_t.
            unsafe { crate::net::write_address(addr, addrlen, from.address, from.port) };
        }
    }
    got
}

pub unsafe fn sendmsg(fd: i32, message: *const u8) -> i64 {
    if message.is_null() {
        return -EFAULT;
    }
    // `struct msghdr`: name, namelen, iov, iovlen, control, controllen, flags.
    // SAFETY: the Linux contract: `message` points at an msghdr.
    let (name, namelen, iov, iovlen) = unsafe {
        (
            (message as *const u64).read_unaligned() as *const u8,
            (message.add(8) as *const u32).read_unaligned(),
            (message.add(16) as *const *const IoVec).read_unaligned(),
            (message.add(24) as *const usize).read_unaligned(),
        )
    };
    let to = if name.is_null() {
        None
    } else {
        // SAFETY: the Linux contract: `name` has `namelen` bytes.
        match unsafe { crate::net::read_address(name, namelen) } {
            Ok(address) => Some(address),
            Err(errno) => return -errno,
        }
    };
    let mut total = 0i64;
    for index in 0..iovlen {
        // SAFETY: the Linux contract: `iovlen` iovecs at `iov`.
        let vec = unsafe { &*iov.add(index) };
        if vec.len == 0 {
            continue;
        }
        // SAFETY: the contract for each iovec.
        let data = unsafe { core::slice::from_raw_parts(vec.base, vec.len) };
        let sent = match with_socket(fd, |socket| crate::net::send(socket, data, to)) {
            Ok(result) => result,
            Err(errno) => return if total > 0 { total } else { -errno },
        };
        if sent < 0 {
            return if total > 0 { total } else { sent };
        }
        total += sent;
        if (sent as usize) < vec.len {
            break;
        }
    }
    total
}

pub unsafe fn recvmsg(fd: i32, message: *mut u8) -> i64 {
    if message.is_null() {
        return -EFAULT;
    }
    // `struct msghdr`: name, namelen, iov, iovlen, control, controllen, flags.
    // SAFETY: the Linux contract: `message` points at an msghdr.
    let (name, iov, iovlen) = unsafe {
        (
            (message as *const u64).read_unaligned() as *mut u8,
            (message.add(16) as *const *mut IoVec).read_unaligned(),
            (message.add(24) as *const usize).read_unaligned(),
        )
    };
    if iovlen == 0 {
        return 0;
    }
    // The receive fills the first iovec, which is the whole of what a DNS
    // reply or an HTTP read ever asks for; extra iovecs would mean receiving
    // into a scratch buffer first.
    // SAFETY: the Linux contract: one iovec at `iov`.
    let vec = unsafe { &mut *iov };
    let out = unsafe { core::slice::from_raw_parts_mut(vec.base, vec.len) };
    // A slot the receive fills in with the sender, whether or not the caller
    // asked for it: `recvmsg` reports it to musl's resolver, which checks it
    // against the name servers it actually sent to.
    let mut from = crate::net::Address { address: [0; 4], port: 0 };
    let got = match with_socket(fd, |socket| crate::net::recv(socket, out, Some(&mut from))) {
        Ok(result) => result,
        Err(errno) => return -errno,
    };
    if got >= 0 && !name.is_null() && from.address != [0; 4] {
        // SAFETY: the Linux contract: `name` has room for a sockaddr and the
        // length field at msghdr+8 is a writable socklen_t.
        unsafe {
            crate::net::write_address(name, message.add(8) as *mut u32, from.address, from.port);
            (message.add(48) as *mut i32).write_unaligned(0);
        }
    }
    got
}

pub fn shutdown_socket(fd: i32, how: i32) -> i64 {
    match with_socket(fd, |socket| crate::net::shutdown(socket, how)) {
        Ok(result) => result,
        Err(errno) => -errno,
    }
}

pub unsafe fn bind_socket(_fd: i32, addr: *const u8, len: u32) -> i64 {
    // SAFETY: the Linux contract: `addr` has `len` bytes.
    let address = match unsafe { crate::net::read_address(addr, len) } {
        Ok(address) => address,
        Err(errno) => return -errno,
    };
    // The service chooses the local port when the socket is opened, so only a
    // request for "any port" can be honoured.
    if address.port == 0 {
        0
    } else {
        -EOPNOTSUPP
    }
}

pub fn listen_socket(fd: i32) -> i64 {
    match with_socket(fd, |_| ()) {
        Ok(()) => -EOPNOTSUPP,
        Err(errno) => -errno,
    }
}

pub unsafe fn getsockname(fd: i32, addr: *mut u8, len: *mut u32) -> i64 {
    let entry = match get(fd) {
        Ok(entry) => entry,
        Err(errno) => return -errno,
    };
    let object = entry.lock();
    let Object::Socket(socket) = &*object else { return -ENOTSOCK };
    // SAFETY: the Linux contract for getsockname.
    unsafe { crate::net::local_address(socket, addr, len) }
}

pub unsafe fn getpeername(fd: i32, addr: *mut u8, len: *mut u32) -> i64 {
    let entry = match get(fd) {
        Ok(entry) => entry,
        Err(errno) => return -errno,
    };
    let object = entry.lock();
    let Object::Socket(socket) = &*object else { return -ENOTSOCK };
    // SAFETY: the Linux contract for getpeername.
    unsafe { crate::net::peer_address(socket, addr, len) }
}

pub fn setsockopt(fd: i32, level: i32, name: i32) -> i64 {
    match with_socket(fd, |_| crate::net::option(level, name)) {
        Ok(result) => result,
        Err(errno) => -errno,
    }
}

pub unsafe fn getsockopt(fd: i32, level: i32, name: i32, value: *mut u8, len: *mut u32) -> i64 {
    const SO_ERROR: i32 = 4;
    if value.is_null() || len.is_null() {
        return -EFAULT;
    }
    let pending = match with_socket(fd, |socket| {
        if level == 1 && name == SO_ERROR {
            crate::net::pending_error(socket)
        } else {
            crate::net::option(level, name)
        }
    }) {
        Ok(result) => result,
        Err(errno) => return -errno,
    };
    if pending < 0 {
        return pending;
    }
    // SAFETY: the Linux contract for getsockopt: `value` is an int.
    unsafe {
        (value as *mut i32).write_unaligned(0);
        len.write(4);
    }
    0
}

// ---------------------------------------------------------------------------
// ioctl, fcntl, poll
// ---------------------------------------------------------------------------

const TCGETS: u64 = 0x5401;
const TCSETS: u64 = 0x5402;
const TCSETSW: u64 = 0x5403;
const TCSETSF: u64 = 0x5404;
const TIOCGWINSZ: u64 = 0x5413;
const TIOCGPGRP: u64 = 0x540F;
/// `c_lflag` bits: line editing with echo, or the program reading keys.
const ICANON: u32 = 0o2;
const ECHO: u32 = 0o10;

/// Whether the console is in the raw mode a program asked for.
fn console_raw() -> bool {
    CONSOLE_RAW.load(Ordering::Relaxed) != 0
}

/// Put the console back in canonical mode, for a program on its way out.
///
/// The console's mode belongs to the terminal, not to the process that set it:
/// a program that exits mid-raw-mode would otherwise leave the next one reading
/// keys instead of lines, exactly as a real terminal would.
pub fn reset_console_mode() {
    CONSOLE_RAW.store(0, Ordering::Relaxed);
}

pub unsafe fn ioctl(fd: i32, request: u64, arg: *mut u8) -> i64 {
    const FIONBIO: u64 = 0x5421;
    let entry = match get(fd) {
        Ok(entry) => entry,
        Err(errno) => return -errno,
    };
    let mut object = entry.lock();
    if let Object::Socket(socket) = &mut *object {
        if request == FIONBIO {
            if arg.is_null() {
                return -EFAULT;
            }
            // SAFETY: the Linux contract: `arg` points at an int.
            let on = unsafe { (arg as *const i32).read() } != 0;
            if on {
                socket.flags |= O_NONBLOCK;
            } else {
                socket.flags &= !O_NONBLOCK;
            }
            return 0;
        }
        return -ENOTTY;
    }
    let is_tty = matches!(&*object, Object::Tty(_));
    if !is_tty {
        return -ENOTTY;
    }
    match request {
        TIOCGWINSZ => {
            let mut reply = [0u8; 32];
            let (rows, cols) = match call(CONSOLE.load(Ordering::Relaxed), proto::CONSOLE_GET_INFO, &[], &mut reply)
                .and_then(|len| decode::<proto::ConsoleInfo>(&reply[..len]))
            {
                Ok(info) => (info.rows as u16, info.cols as u16),
                Err(_) => (25, 80),
            };
            // SAFETY: the Linux contract: `arg` points at a `struct winsize`.
            unsafe {
                let ws = arg as *mut u16;
                ws.write(rows);
                ws.add(1).write(cols);
                ws.add(2).write(0);
                ws.add(3).write(0);
            }
            0
        }
        TCGETS => {
            // `struct termios`, 60 bytes: canonical mode with echo, which is
            // what the console's line input actually does -- unless a program
            // asked for raw mode with `tcsetattr`, in which case the flags it
            // asked for are what it reads back.
            // SAFETY: the Linux contract: `arg` points at a termios.
            unsafe {
                core::ptr::write_bytes(arg, 0, 60);
                let words = arg as *mut u32;
                words.write(0o2400); // c_iflag: ICRNL | IXON
                words.add(1).write(0o5); // c_oflag: OPOST | ONLCR
                words.add(2).write(0o277); // c_cflag: B38400 | CS8 | CREAD
                words.add(3).write(if console_raw() {
                    0 // no ICANON, no ECHO: the program reads keys itself
                } else {
                    0o105073 // c_lflag: ISIG ICANON ECHO ECHOE ECHOK IEXTEN ECHOCTL ECHOKE
                });
            }
            0
        }
        TCSETS | TCSETSW | TCSETSF => {
            // Only one thing about a termios matters here: whether the program
            // wants the console to edit and echo lines, or wants the keys. The
            // console has no other modes to set, so the rest is read and kept
            // only insofar as `TCGETS` reports it back.
            if arg.is_null() {
                return -EFAULT;
            }
            // SAFETY: the Linux contract: `arg` points at a termios, and
            // `c_lflag` is its fourth word.
            let lflag = unsafe { (arg.add(12) as *const u32).read_unaligned() };
            CONSOLE_RAW.store(
                u32::from(lflag & (ICANON | ECHO) == 0),
                Ordering::Relaxed,
            );
            0
        }
        TIOCGPGRP => {
            // SAFETY: the Linux contract: `arg` points at a pid_t.
            unsafe { (arg as *mut i32).write(crate::PID as i32) };
            0
        }
        _ => -EINVAL,
    }
}

const F_DUPFD: i32 = 0;
const F_GETFD: i32 = 1;
const F_SETFD: i32 = 2;
const F_GETFL: i32 = 3;
const F_SETFL: i32 = 4;
const F_DUPFD_CLOEXEC: i32 = 1030;

pub fn fcntl(fd: i32, command: i32, arg: i64) -> i64 {
    let entry = match get(fd) {
        Ok(entry) => entry,
        Err(errno) => return -errno,
    };
    match command {
        F_DUPFD | F_DUPFD_CLOEXEC => install_at_least(entry, arg.max(0) as usize),
        // There is no exec yet, so close-on-exec has nothing to act on.
        F_GETFD | F_SETFD => 0,
        F_GETFL => match &*entry.lock() {
            Object::File(file) => file.flags as i64,
            Object::Socket(socket) => (socket.flags | O_RDWR) as i64,
            Object::Mem(_) => O_RDONLY as i64,
            Object::Tty(Tty::Input) | Object::Dir(_) => 0,
            Object::Tty(_) => 1,
        },
        F_SETFL => {
            let requested = arg as i32;
            if let Object::Socket(socket) = &mut *entry.lock() {
                socket.flags = (socket.flags & !O_NONBLOCK) | (requested & O_NONBLOCK);
            }
            0
        }
        _ => -EINVAL,
    }
}

/// `struct pollfd`.
#[repr(C)]
pub struct PollFd {
    fd: i32,
    events: i16,
    revents: i16,
}

pub unsafe fn poll(fds: *mut PollFd, count: usize, timeout_ms: i32) -> i64 {
    const POLLIN: i16 = 0x1;
    const POLLOUT: i16 = 0x4;
    const POLLERR: i16 = 0x8;
    const POLLHUP: i16 = 0x10;
    const POLLNVAL: i16 = 0x20;
    const MAX_WATCHED: usize = 32;

    // Files and the console are ready the moment they are asked about: a
    // console line input blocks inside `read` rather than signalling anything
    // a program could wait on. Sockets are the descriptors that need waiting
    // for, so they are collected into one request to the network service.
    let mut watched: [(u32, u8); MAX_WATCHED] = [(0, 0); MAX_WATCHED];
    let mut positions: [usize; MAX_WATCHED] = [0; MAX_WATCHED];
    let mut socket_count = 0;
    let mut ready = 0i64;
    let mut immediate = false;

    for index in 0..count {
        // SAFETY: the Linux contract: `count` pollfds.
        let item = unsafe { &mut *fds.add(index) };
        if item.fd < 0 {
            item.revents = 0;
            continue;
        }
        match get(item.fd) {
            Ok(entry) => {
                let object = entry.lock();
                if let Object::Socket(socket) = &*object {
                    item.revents = 0;
                    if socket_count < MAX_WATCHED {
                        let mut interest = 0;
                        if item.events & POLLIN != 0 {
                            interest |= ready::READ;
                        }
                        if item.events & POLLOUT != 0 {
                            interest |= ready::WRITE;
                        }
                        watched[socket_count] = (socket.id, interest);
                        positions[socket_count] = index;
                        socket_count += 1;
                    }
                } else {
                    item.revents = item.events & (POLLIN | POLLOUT);
                    immediate = true;
                    ready += 1;
                }
            }
            Err(_) => {
                item.revents = POLLNVAL;
                immediate = true;
                ready += 1;
            }
        }
    }

    if socket_count == 0 {
        // Nothing but files and the console: there is nothing to wait on. A
        // timeout is still a timeout, so honour it rather than spinning.
        if !immediate && timeout_ms > 0 {
            crate::sleep_until(
                libnode::clock_ns().saturating_add(timeout_ms as u64 * 1_000_000),
            );
        }
        return ready;
    }

    // A file or the console being ready does not mean nothing else is: a
    // program that polls its keyboard and its socket together expects both
    // answers. Ask the network service without waiting when something is
    // already ready, and with the caller's timeout when nothing is.
    let timeout = if immediate {
        0
    } else if timeout_ms < 0 {
        u32::MAX
    } else {
        timeout_ms as u32
    };
    let mut bits = [0u8; MAX_WATCHED];
    if crate::net::poll(&watched[..socket_count], timeout, &mut bits) < 0 {
        return -EIO;
    }
    for index in 0..socket_count {
        let ready_bits = bits[index];
        let mut revents = 0;
        if ready_bits & ready::READ != 0 {
            revents |= POLLIN;
        }
        if ready_bits & ready::WRITE != 0 {
            revents |= POLLOUT;
        }
        if ready_bits & ready::CLOSED != 0 {
            revents |= POLLERR | POLLHUP;
        }
        // SAFETY: `positions` records indices below `count`.
        let item = unsafe { &mut *fds.add(positions[index]) };
        item.revents = revents;
        if revents != 0 {
            ready += 1;
        }
    }
    ready
}

// ---------------------------------------------------------------------------
// stat and the namespace
// ---------------------------------------------------------------------------

const S_IFDIR: u32 = 0o040000;
const S_IFREG: u32 = 0o100000;
const S_IFCHR: u32 = 0o020000;
const S_IFSOCK: u32 = 0o140000;
const AT_EMPTY_PATH: i32 = 0x1000;
const AT_REMOVEDIR: i32 = 0x200;

/// Fill a Linux x86-64 `struct stat` (144 bytes).
unsafe fn fill_stat(out: *mut u8, ino: u64, mode: u32, size: u64, mtime_ns: u64) {
    // SAFETY: the Linux contract: `out` points at a `struct stat`.
    unsafe {
        core::ptr::write_bytes(out, 0, 144);
        let at = |offset: usize| out.add(offset);
        (at(0) as *mut u64).write(1); // st_dev
        (at(8) as *mut u64).write(ino); // st_ino
        (at(16) as *mut u64).write(1); // st_nlink
        (at(24) as *mut u32).write(mode); // st_mode
        (at(48) as *mut i64).write(size as i64); // st_size
        (at(56) as *mut i64).write(4096); // st_blksize
        (at(64) as *mut i64).write(size.div_ceil(512) as i64); // st_blocks
        for time in [72, 88, 104] {
            // st_atim, st_mtim, st_ctim
            (at(time) as *mut i64).write((mtime_ns / 1_000_000_000) as i64);
            (at(time + 8) as *mut i64).write((mtime_ns % 1_000_000_000) as i64);
        }
    }
}

unsafe fn stat_path(path: &str, out: *mut u8) -> i64 {
    match fs_stat(path) {
        Ok(stat) => {
            let mode = if stat.kind == proto::fs_kind::DIRECTORY { S_IFDIR | 0o755 } else { S_IFREG | 0o644 };
            // SAFETY: forwarded contract.
            unsafe { fill_stat(out, stat.node, mode, stat.size, stat.modified_ns) };
            0
        }
        Err(errno) => -errno,
    }
}

pub unsafe fn fstat(fd: i32, out: *mut u8) -> i64 {
    let entry = match get(fd) {
        Ok(entry) => entry,
        Err(errno) => return -errno,
    };
    let path = match &*entry.lock() {
        Object::Tty(_) => {
            // SAFETY: forwarded contract.
            unsafe { fill_stat(out, 0, S_IFCHR | 0o620, 0, 0) };
            return 0;
        }
        Object::Socket(_) => {
            // SAFETY: forwarded contract.
            unsafe { fill_stat(out, 0, S_IFSOCK | 0o777, 0, 0) };
            return 0;
        }
        Object::Mem(mem) => {
            let size = mem.content.len() as u64;
            // SAFETY: forwarded contract.
            unsafe { fill_stat(out, 0, S_IFREG | 0o444, size, 0) };
            return 0;
        }
        Object::File(file) => file.path.clone(),
        Object::Dir(dir) => dir.path.clone(),
    };
    // SAFETY: forwarded contract.
    unsafe { stat_path(&path, out) }
}

pub unsafe fn fstatat(dirfd: i32, path: *const u8, out: *mut u8, flags: i32) -> i64 {
    // SAFETY: forwarded contract.
    let path = match unsafe { c_str(path) } {
        Ok(path) => path,
        Err(errno) => return -errno,
    };
    if path.is_empty() && flags & AT_EMPTY_PATH != 0 {
        // SAFETY: forwarded contract.
        return unsafe { fstat(dirfd, out) };
    }
    match resolve(dirfd, path) {
        // SAFETY: forwarded contract.
        Ok(path) => unsafe { stat_path(&path, out) },
        Err(errno) => -errno,
    }
}

pub unsafe fn faccessat(dirfd: i32, path: *const u8) -> i64 {
    // There are no permissions to check: a path is accessible if it exists.
    // SAFETY: forwarded contract.
    match unsafe { c_str(path) }.and_then(|path| resolve(dirfd, path)).and_then(|path| fs_stat(&path)) {
        Ok(_) => 0,
        Err(errno) => -errno,
    }
}

pub unsafe fn getdents64(fd: i32, buf: *mut u8, len: usize) -> i64 {
    let entry = match get(fd) {
        Ok(entry) => entry,
        Err(errno) => return -errno,
    };
    let mut object = entry.lock();
    let Object::Dir(dir) = &mut *object else { return -ENOTDIR };
    if dir.entries.is_none() {
        let mut listing = alloc::vec![0u8; 60 * 1024];
        let got = match call(files(), proto::FS_LIST, &[dir.path.as_bytes()], &mut listing) {
            Ok(got) => got,
            Err(errno) => return -errno,
        };
        // Each record is "name\t{d|f}\tsize", NUL-terminated.
        let mut entries = alloc::vec![(String::from("."), true), (String::from(".."), true)];
        for record in listing[..got].split(|b| *b == 0).filter(|r| !r.is_empty()) {
            let mut fields = record.split(|b| *b == b'\t');
            let name = String::from_utf8_lossy(fields.next().unwrap_or_default()).into_owned();
            let is_dir = fields.next() == Some(b"d");
            entries.push((name, is_dir));
        }
        dir.entries = Some(entries);
    }
    let entries = dir.entries.as_ref().map_or(&[][..], |e| &e[..]);
    let mut written = 0;
    while dir.position < entries.len() {
        let (name, is_dir) = &entries[dir.position];
        // `struct linux_dirent64`: ino, off, reclen, type, name, NUL, padded to 8.
        let reclen = (19 + name.len() + 1).div_ceil(8) * 8;
        if written + reclen > len {
            break;
        }
        // SAFETY: the Linux contract: `buf` has `len` bytes; `reclen` fits.
        unsafe {
            let at = buf.add(written);
            core::ptr::write_bytes(at, 0, reclen);
            (at as *mut u64).write_unaligned(dir.position as u64 + 1);
            (at.add(8) as *mut i64).write_unaligned(dir.position as i64 + 1);
            (at.add(16) as *mut u16).write_unaligned(reclen as u16);
            at.add(18).write(if *is_dir { 4 } else { 8 }); // DT_DIR, DT_REG
            core::ptr::copy_nonoverlapping(name.as_ptr(), at.add(19), name.len());
        }
        written += reclen;
        dir.position += 1;
    }
    if written == 0 && dir.position < entries.len() {
        return -EINVAL;
    }
    written as i64
}

pub unsafe fn getcwd(buf: *mut u8, len: usize) -> i64 {
    let cwd = CWD.lock();
    if cwd.len() + 1 > len {
        return -ERANGE;
    }
    // SAFETY: the Linux contract: `buf` has `len` bytes; checked above.
    unsafe {
        core::ptr::copy_nonoverlapping(cwd.as_ptr(), buf, cwd.len());
        buf.add(cwd.len()).write(0);
    }
    cwd.len() as i64 + 1
}

pub unsafe fn chdir(path: *const u8) -> i64 {
    // SAFETY: forwarded contract.
    let path = match unsafe { c_str(path) }.and_then(|path| resolve(AT_FDCWD, path)) {
        Ok(path) => path,
        Err(errno) => return -errno,
    };
    match fs_stat(&path) {
        Ok(stat) if stat.kind == proto::fs_kind::DIRECTORY => {
            *CWD.lock() = path;
            0
        }
        Ok(_) => -ENOTDIR,
        Err(errno) => -errno,
    }
}

pub unsafe fn mkdirat(dirfd: i32, path: *const u8) -> i64 {
    // SAFETY: forwarded contract.
    match unsafe { c_str(path) }.and_then(|path| resolve(dirfd, path)).and_then(|path| fs_create(&path, true)) {
        Ok(node) => {
            fs_close(node.node);
            0
        }
        Err(errno) => -errno,
    }
}

pub unsafe fn unlinkat(dirfd: i32, path: *const u8) -> i64 {
    let _ = AT_REMOVEDIR; // the service removes either kind
    // SAFETY: forwarded contract.
    let result = unsafe { c_str(path) }
        .and_then(|path| resolve(dirfd, path))
        .and_then(|path| call(files(), proto::FS_UNLINK, &[path.as_bytes()], &mut []));
    match result {
        Ok(_) => 0,
        Err(errno) => -errno,
    }
}

/// The filesystem protocol has no rename (docs/MUSL.md, section 4.2). A
/// regular file is moved by copying it and removing the original, which is
/// not atomic; a directory cannot be moved at all.
pub unsafe fn renameat(old_dirfd: i32, old: *const u8, new_dirfd: i32, new: *const u8) -> i64 {
    // SAFETY: forwarded contract.
    let paths = unsafe { c_str(old) }
        .and_then(|old| resolve(old_dirfd, old))
        .and_then(|old| Ok((old, unsafe { c_str(new) }.and_then(|new| resolve(new_dirfd, new))?)));
    let (old, new) = match paths {
        Ok(paths) => paths,
        Err(errno) => return -errno,
    };
    match rename_by_copy(&old, &new) {
        Ok(()) => 0,
        Err(errno) => -errno,
    }
}

fn rename_by_copy(old: &str, new: &str) -> Result<(), i64> {
    if old == new {
        return Ok(());
    }
    let source = fs_open(old)?;
    if source.kind == proto::fs_kind::DIRECTORY {
        fs_close(source.node);
        return Err(ENOSYS);
    }
    let target = match fs_create(new, false) {
        Ok(target) => target,
        Err(errno) => {
            fs_close(source.node);
            return Err(errno);
        }
    };
    let mut chunk = alloc::vec![0u8; MAX_IO];
    let mut offset = 0;
    let result = loop {
        match fs_read(source.node, offset, &mut chunk) {
            Ok(0) => break Ok(()),
            Ok(got) => match fs_write(target.node, offset, &chunk[..got]) {
                Ok(_) => offset += got as u64,
                Err(errno) => break Err(errno),
            },
            Err(errno) => break Err(errno),
        }
    };
    fs_close(source.node);
    fs_close(target.node);
    result?;
    call(files(), proto::FS_UNLINK, &[old.as_bytes()], &mut []).map(|_| ())
}
