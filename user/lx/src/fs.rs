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
fn call(channel: u32, kind: u32, parts: &[&[u8]], reply: &mut [u8]) -> Result<usize, i64> {
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

enum Object {
    Tty(Tty),
    File(File),
    Dir(Dir),
}

impl Drop for Object {
    fn drop(&mut self) {
        match self {
            Object::File(file) => fs_close(file.node),
            Object::Dir(dir) => fs_close(dir.node),
            Object::Tty(_) => {}
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

pub unsafe fn openat(dirfd: i32, path: *const u8, flags: i32) -> i64 {
    // SAFETY: forwarded contract.
    match unsafe { c_str(path) }.and_then(|path| open_path(dirfd, path, flags)) {
        Ok(fd) => fd,
        Err(errno) => -errno,
    }
}

fn open_path(dirfd: i32, path: &str, flags: i32) -> Result<i64, i64> {
    let path = resolve(dirfd, path)?;
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
        // Canonical mode, as the console provides it: it edits and echoes the
        // line itself and replies when Enter is pressed.
        let mut line = alloc::vec![0u8; 1024];
        let got = match call(CONSOLE.load(Ordering::Relaxed), proto::CONSOLE_READ_LINE, &[], &mut line) {
            Ok(got) => got,
            Err(_) => return 0, // a console that went away is end of file
        };
        line.truncate(got);
        line.push(b'\n');
        *pending = (line, 0);
    }
    let available = &pending.0[pending.1..];
    let n = available.len().min(out.len());
    out[..n].copy_from_slice(&available[..n]);
    pending.1 += n;
    n as i64
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
// ioctl, fcntl, poll
// ---------------------------------------------------------------------------

const TCGETS: u64 = 0x5401;
const TCSETS: u64 = 0x5402;
const TCSETSW: u64 = 0x5403;
const TCSETSF: u64 = 0x5404;
const TIOCGWINSZ: u64 = 0x5413;
const TIOCGPGRP: u64 = 0x540F;

pub unsafe fn ioctl(fd: i32, request: u64, arg: *mut u8) -> i64 {
    let entry = match get(fd) {
        Ok(entry) => entry,
        Err(errno) => return -errno,
    };
    let is_tty = matches!(&*entry.lock(), Object::Tty(_));
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
            // what the console's line input actually does.
            // SAFETY: the Linux contract: `arg` points at a termios.
            unsafe {
                core::ptr::write_bytes(arg, 0, 60);
                let words = arg as *mut u32;
                words.write(0o2400); // c_iflag: ICRNL | IXON
                words.add(1).write(0o5); // c_oflag: OPOST | ONLCR
                words.add(2).write(0o277); // c_cflag: B38400 | CS8 | CREAD
                words.add(3).write(0o105073); // c_lflag: ISIG ICANON ECHO ECHOE ECHOK IEXTEN ECHOCTL ECHOKE
            }
            0
        }
        TCSETS | TCSETSW | TCSETSF => 0,
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
            Object::Tty(Tty::Input) | Object::Dir(_) => 0,
            Object::Tty(_) => 1,
        },
        F_SETFL => 0,
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

pub unsafe fn poll(fds: *mut PollFd, count: usize) -> i64 {
    // Files are always ready, and console input is a blocking call rather
    // than a signal anything can wait on, so report every valid descriptor as
    // ready for what it asked and let the read itself block.
    const POLLNVAL: i16 = 0x20;
    let mut ready = 0;
    for index in 0..count {
        // SAFETY: the Linux contract: `count` pollfds.
        let item = unsafe { &mut *fds.add(index) };
        item.revents = if item.fd < 0 {
            0
        } else if get(item.fd).is_ok() {
            item.events & 0x5
        } else {
            POLLNVAL
        };
        if item.revents != 0 {
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
