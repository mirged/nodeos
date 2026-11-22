//! Sockets: the Linux socket calls, answered by the network service.
//!
//! There is no socket code here beyond the translation. A NodeOS process that
//! wants the network holds one channel to the `net` service; this module turns
//! `socket`, `connect`, `send`, `recv`, `poll`, `shutdown` and the rest into
//! requests on it, so that musl's `getaddrinfo` and a program like `curl` see
//! the Linux socket API they were written against.
//!
//! ```text
//!   curl --recv--> musl --__lx_syscall(SYS_recvfrom)--> liblx --NET_SOCKET_RECV--> net
//!                                                               smoltcp -- e1000
//! ```
//!
//! Blocking behaviour is the service's: a receive with no timeout is parked
//! until bytes arrive, so this process makes one call and sleeps. A
//! non-blocking one is answered immediately with "again", which is what
//! `O_NONBLOCK` and `EAGAIN` mean here.

use alloc::string::String;

use libnode::sync::SpinLock;

use crate::errno::*;

/// Address families Linux programs ask for.
pub const AF_UNSPEC: i32 = 0;
/// The only family this stack speaks.
pub const AF_INET: i32 = 2;
/// Recognised so that a program asking for it gets a truthful refusal.
pub const AF_INET6: i32 = 10;

const SOCK_STREAM: i32 = 1;
const SOCK_DGRAM: i32 = 2;
/// The type bits of a `socket(2)` argument; the rest are flags.
const SOCK_TYPE_MASK: i32 = 0xF;
const SOCK_NONBLOCK: i32 = 0x800;
const SOCK_CLOEXEC: i32 = 0x80000;

/// `struct sockaddr_in` as Linux lays it out.
pub const SOCKADDR_IN_LEN: u32 = 16;

/// No timeout at all: the service waits until something happens.
const WAIT_FOREVER: u32 = u32::MAX;
/// How long a connect gets before it is reported as timed out.
const CONNECT_TIMEOUT_MS: u32 = 30_000;
/// Longest single send this module makes; the service's receive buffer is
/// larger, but a chunk this size keeps one request inside the scratch buffer.
const SEND_CHUNK: usize = 16 * 1024;

static CHANNEL: SpinLock<u32> = SpinLock::new(0);

/// Record the network service channel. Called once, by startup.
pub fn init(channel: Option<u32>) {
    *CHANNEL.lock() = channel.unwrap_or(0);
}

fn channel() -> u32 {
    *CHANNEL.lock()
}

/// Whether this process holds a network channel at all.
pub fn available() -> bool {
    channel() != 0
}

/// The interface's addresses, as the service last reported them.
#[derive(Clone, Copy, PartialEq, Eq)]
pub struct Config {
    /// The address programs should use as their source.
    pub address: [u8; 4],
    /// Prefix length, for `getifaddrs`-shaped answers.
    pub prefix: u8,
    /// Default gateway.
    pub gateway: [u8; 4],
    /// Name server, which `/etc/resolv.conf` reports.
    pub dns: [u8; 4],
}

static CACHED: SpinLock<Option<Config>> = SpinLock::new(None);

/// The interface's addresses, asking the service until it has them.
///
/// The stack asks for a DHCP lease at boot and falls back to QEMU's addresses
/// after a few seconds; a program started in that window waits here rather
/// than resolving against an address the interface does not have.
pub fn config() -> Option<Config> {
    if let Some(config) = *CACHED.lock() {
        return Some(config);
    }
    if !available() {
        return None;
    }
    let deadline = libnode::clock_ns().saturating_add(10_000_000_000);
    loop {
        if let Some(config) = query() {
            *CACHED.lock() = Some(config);
            return Some(config);
        }
        if libnode::clock_ns() >= deadline {
            return None;
        }
        crate::sleep_until(libnode::clock_ns().saturating_add(2_000_000));
    }
}

fn query() -> Option<Config> {
    let mut reply = [0u8; 32];
    let len = crate::fs::call(channel(), net::NET_INFO, &[], &mut reply).ok()?;
    if len != 21 || reply[7] == 0 {
        return None;
    }
    let mut address = [0; 4];
    address.copy_from_slice(&reply[8..12]);
    let mut gateway = [0; 4];
    gateway.copy_from_slice(&reply[13..17]);
    let mut dns = [0; 4];
    dns.copy_from_slice(&reply[17..21]);
    Some(Config { address, prefix: reply[12], gateway, dns })
}

/// The contents of a Linux configuration file this system answers itself.
///
/// musl's resolver reads `/etc/resolv.conf` through ordinary `open`/`read`, and
/// there is no filesystem holding one, so these two paths are synthesized from
/// what the network service reports. Everything else is the real filesystem.
pub fn synthetic_file(path: &str) -> Option<String> {
    match path {
        "/etc/resolv.conf" => {
            let config = config()?;
            Some(alloc::format!(
                "nameserver {}.{}.{}.{}\n",
                config.dns[0], config.dns[1], config.dns[2], config.dns[3]
            ))
        }
        "/etc/hosts" => {
            let config = config()?;
            Some(alloc::format!(
                "127.0.0.1 localhost\n{}.{}.{}.{} nodeos\n",
                config.address[0],
                config.address[1],
                config.address[2],
                config.address[3]
            ))
        }
        _ => None,
    }
}

// ---------------------------------------------------------------------------
// The descriptor's view of a socket
// ---------------------------------------------------------------------------

/// One socket, as the descriptor table holds it.
pub struct Socket {
    /// The service's name for it.
    pub id: u32,
    /// `PF_INET`.
    pub domain: i32,
    /// `SOCK_STREAM` or `SOCK_DGRAM`.
    pub kind: i32,
    /// `O_NONBLOCK` and friends, as `fcntl` reports them.
    pub flags: i32,
    /// Where a datagram socket sends by default.
    pub peer: Option<([u8; 4], u16)>,
    /// Set once the handshake succeeded, so a reset can be told from a refusal.
    pub connected: bool,
    /// Set by `shutdown(SHUT_WR)`.
    pub write_shut: bool,
}

/// Open a socket and return the descriptor for it, or `-errno`.
pub fn open(domain: i32, kind: i32, protocol: i32, flags: i32) -> i64 {
    let _ = protocol;
    if !available() {
        return -EAFNOSUPPORT;
    }
    if domain != AF_INET {
        return -EAFNOSUPPORT;
    }
    let type_bits = kind & SOCK_TYPE_MASK;
    let service_kind = match type_bits {
        SOCK_STREAM => net::socket_kind::TCP,
        SOCK_DGRAM => net::socket_kind::UDP,
        _ => return -EPROTOTYPE,
    };
    open_socket(service_kind, domain, kind, flags)
}

/// `kind` is the raw `socket(2)` type word: the low nibble is the socket type,
/// the rest are flags such as `SOCK_NONBLOCK`, which the descriptor keeps.
fn open_socket(service_kind: u8, domain: i32, kind: i32, flags: i32) -> i64 {
    let mut request = [0u8; 3];
    request[0] = service_kind;
    let mut reply = [0u8; 8];
    let len = match crate::fs::call(channel(), net::NET_SOCKET_OPEN, &[&request], &mut reply) {
        Ok(len) => len,
        Err(errno) => return -errno,
    };
    if len != 4 {
        return -EIO;
    }
    let id = u32::from_le_bytes([reply[0], reply[1], reply[2], reply[3]]);
    let mut flags = flags & O_NONBLOCK;
    if kind & SOCK_NONBLOCK != 0 {
        flags |= O_NONBLOCK;
    }
    if kind & SOCK_CLOEXEC != 0 {
        // There is no exec, so close-on-exec has nothing to do; keep the flag
        // visible to `fcntl` anyway.
        flags |= O_CLOEXEC;
    }
    let socket = Socket {
        id,
        domain,
        kind: kind & SOCK_TYPE_MASK,
        flags,
        peer: None,
        connected: false,
        write_shut: false,
    };
    crate::fs::install_socket(socket)
}

const O_NONBLOCK: i32 = 0o4000;
const O_CLOEXEC: i32 = 0o2000000;

/// A parsed `sockaddr_in`.
#[derive(Clone, Copy, Debug)]
pub struct Address {
    /// The IPv4 address.
    pub address: [u8; 4],
    /// The port, host order.
    pub port: u16,
}

/// Read a `sockaddr_in` the program passed.
///
/// # Safety
/// `pointer` must point at a `sockaddr_in` with at least `length` readable
/// bytes, as the Linux socket call contract requires.
pub unsafe fn read_address(pointer: *const u8, length: u32) -> Result<Address, i64> {
    if pointer.is_null() {
        return Err(EFAULT);
    }
    if length < 8 {
        return Err(EINVAL);
    }
    // SAFETY: the caller's contract: `length` readable bytes at `pointer`.
    let bytes = unsafe { core::slice::from_raw_parts(pointer, length.min(SOCKADDR_IN_LEN) as usize) };
    let family = u16::from_le_bytes([bytes[0], bytes[1]]);
    if family as i32 != AF_INET {
        return Err(EAFNOSUPPORT);
    }
    if bytes.len() < 8 {
        return Err(EINVAL);
    }
    let port = u16::from_be_bytes([bytes[2], bytes[3]]);
    Ok(Address { address: [bytes[4], bytes[5], bytes[6], bytes[7]], port })
}

/// Write a `sockaddr_in` and its length where the program expects them.
///
/// # Safety
/// `out` must point at a writable `sockaddr_in` and `length` at a writable
/// `socklen_t`, as `accept`, `getpeername` and `getsockname` require.
pub unsafe fn write_address(
    out: *mut u8,
    length: *mut u32,
    address: [u8; 4],
    port: u16,
) -> i64 {
    if out.is_null() || length.is_null() {
        return -EFAULT;
    }
    if unsafe { length.read() } < SOCKADDR_IN_LEN {
        return -EINVAL;
    }
    // SAFETY: the caller's contract: room for a sockaddr_in.
    unsafe {
        core::ptr::write_bytes(out, 0, SOCKADDR_IN_LEN as usize);
        (out as *mut u16).write_unaligned(AF_INET as u16);
        (out.add(2) as *mut u16).write_unaligned(port.to_be());
        core::ptr::copy_nonoverlapping(address.as_ptr(), out.add(4), 4);
        length.write(SOCKADDR_IN_LEN);
    }
    0
}

/// `connect(2)`, or the peer a datagram socket will send to.
pub fn connect(socket: &mut Socket, address: Address) -> i64 {
    let mut request = [0u8; 14];
    request[..4].copy_from_slice(&socket.id.to_le_bytes());
    request[4..8].copy_from_slice(&address.address);
    request[8..10].copy_from_slice(&address.port.to_le_bytes());
    let timeout = if socket.flags & O_NONBLOCK != 0 {
        0
    } else {
        CONNECT_TIMEOUT_MS
    };
    request[10..14].copy_from_slice(&timeout.to_le_bytes());
    let mut reply = [0u8; 8];
    let len = match crate::fs::call(channel(), net::NET_SOCKET_CONNECT, &[&request], &mut reply) {
        Ok(len) => len,
        Err(errno) => return -errno,
    };
    if len != 1 {
        return -EIO;
    }
    match reply[0] {
        net::status::OK => {
            socket.connected = true;
            socket.peer = Some((address.address, address.port));
            0
        }
        net::status::REFUSED => -ECONNREFUSED,
        net::status::UNREACHABLE => -ENETUNREACH,
        // A non-blocking connect that has only started is not an error: the
        // program waits for the socket to become writable.
        net::status::TIMEOUT if socket.flags & O_NONBLOCK != 0 => {
            socket.connected = true;
            socket.peer = Some((address.address, address.port));
            -EINPROGRESS
        }
        net::status::TIMEOUT => -ETIMEDOUT,
        _ => -EIO,
    }
}

/// `send(2)` and `sendto(2)`.
pub fn send(socket: &Socket, data: &[u8], to: Option<Address>) -> i64 {
    if socket.write_shut {
        return -EPIPE;
    }
    let peer = match to.or(socket.peer.map(|(address, port)| Address { address, port })) {
        Some(peer) => peer,
        None => return -EDESTADDRREQ,
    };
    let blocking = socket.flags & O_NONBLOCK == 0;
    let mut sent = 0usize;
    while sent < data.len() {
        let chunk = &data[sent..(sent + SEND_CHUNK).min(data.len())];
        let mut header = [0u8; 10];
        header[..4].copy_from_slice(&socket.id.to_le_bytes());
        header[4..8].copy_from_slice(&peer.address);
        header[8..10].copy_from_slice(&peer.port.to_le_bytes());
        let mut reply = [0u8; 16];
        let len = match crate::fs::call(channel(), net::NET_SOCKET_SEND, &[&header, chunk], &mut reply)
        {
            Ok(len) => len,
            Err(errno) => return if sent > 0 { sent as i64 } else { -errno },
        };
        if len == 0 {
            return if sent > 0 { sent as i64 } else { -EIO };
        }
        let written = if len >= 5 {
            u32::from_le_bytes([reply[1], reply[2], reply[3], reply[4]]) as usize
        } else {
            0
        };
        match reply[0] {
            net::status::OK => {
                sent += written;
                if written == 0 {
                    break;
                }
            }
            net::status::AGAIN => {
                if sent > 0 {
                    return sent as i64;
                }
                if !blocking {
                    return -EAGAIN;
                }
                // Wait for the send buffer to drain, then try again.
                if wait_ready(socket.id, net::ready::WRITE, WAIT_FOREVER) < 0 {
                    return -EIO;
                }
            }
            net::status::RESET => return if sent > 0 { sent as i64 } else { -EPIPE },
            net::status::NOT_CONNECTED => return -ENOTCONN,
            _ => return -EIO,
        }
    }
    sent as i64
}

/// `recv(2)`, `recvfrom(2)`, and `read(2)` on a socket.
///
/// The reply is a status byte, the sender's address and then the data, and the
/// data lands in the program's own buffer, so a receive of a megabyte allocates
/// nothing.
pub fn recv(socket: &Socket, out: &mut [u8], from: Option<&mut Address>) -> i64 {
    let timeout = if socket.flags & O_NONBLOCK != 0 {
        0
    } else {
        WAIT_FOREVER
    };
    // Status, address, port: seven bytes in front of the data.
    const PREFIX: usize = 7;
    let mut header = [0u8; 12];
    header[..4].copy_from_slice(&socket.id.to_le_bytes());
    header[8..12].copy_from_slice(&timeout.to_le_bytes());

    if out.len() > PREFIX + 16 {
        // Large enough to take the prefix in front of the data, which is then
        // shifted down.
        let wanted = (out.len() - PREFIX) as u32;
        header[4..8].copy_from_slice(&wanted.to_le_bytes());
        let len = match crate::fs::call(channel(), net::NET_SOCKET_RECV, &[&header], out) {
            Ok(len) => len,
            Err(errno) => return -errno,
        };
        if len == 0 {
            return -EIO;
        }
        let status = out[0];
        if status != net::status::OK {
            return status_error(status);
        }
        let got = len - PREFIX;
        let source = source_address(out);
        out.copy_within(PREFIX..PREFIX + got, 0);
        return received(got, from, source);
    }

    let mut small = [0u8; 32];
    let wanted = (small.len() - PREFIX) as u32;
    header[4..8].copy_from_slice(&wanted.to_le_bytes());
    let len = match crate::fs::call(channel(), net::NET_SOCKET_RECV, &[&header], &mut small[..]) {
        Ok(len) => len,
        Err(errno) => return -errno,
    };
    if len == 0 {
        return -EIO;
    }
    let status = small[0];
    if status != net::status::OK {
        return status_error(status);
    }
    let got = (len - PREFIX).min(out.len());
    let source = source_address(&small);
    out[..got].copy_from_slice(&small[PREFIX..PREFIX + got]);
    received(got, from, source)
}

/// The sender's address from a receive reply.
fn source_address(reply: &[u8]) -> Option<Address> {
    let address: [u8; 4] = reply.get(1..5)?.try_into().ok()?;
    if address == [0; 4] {
        return None;
    }
    let port = u16::from_le_bytes([*reply.get(5)?, *reply.get(6)?]);
    Some(Address { address, port })
}

fn received(got: usize, from: Option<&mut Address>, source: Option<Address>) -> i64 {
    if let Some(from) = from {
        if let Some(source) = source {
            *from = source;
        }
    }
    got as i64
}

fn status_error(status: u8) -> i64 {
    match status {
        net::status::AGAIN => -EAGAIN,
        net::status::EOF => 0,
        net::status::RESET | net::status::NOT_CONNECTED => -ECONNRESET,
        _ => -EIO,
    }
}

/// `shutdown(2)`.
pub fn shutdown(socket: &mut Socket, how: i32) -> i64 {
    const SHUT_RD: i32 = 0;
    const SHUT_WR: i32 = 1;
    const SHUT_RDWR: i32 = 2;
    if !matches!(how, SHUT_RD | SHUT_WR | SHUT_RDWR) {
        return -EINVAL;
    }
    let bit = if how == SHUT_RD { 0u8 } else { 1u8 };
    let mut request = [0u8; 5];
    request[..4].copy_from_slice(&socket.id.to_le_bytes());
    request[4] = bit;
    match crate::fs::call(channel(), net::NET_SOCKET_SHUTDOWN, &[&request], &mut [0u8; 8]) {
        Ok(_) => {
            if how != SHUT_RD {
                socket.write_shut = true;
            }
            0
        }
        Err(errno) => -errno,
    }
}

/// `close(2)` on a socket: tell the service, which finishes the connection.
pub fn close(socket: &Socket) {
    let mut request = [0u8; 4];
    request.copy_from_slice(&socket.id.to_le_bytes());
    let _ = crate::fs::call(channel(), net::NET_SOCKET_CLOSE, &[&request], &mut [0u8; 8]);
}

/// `getsockname(2)`.
///
/// # Safety
/// `out` and `length` must be writable, as `getsockname` requires.
pub unsafe fn local_address(socket: &Socket, out: *mut u8, length: *mut u32) -> i64 {
    let port = info(socket).map(|info| info.local_port).unwrap_or(0);
    let address = config().map(|config| config.address).unwrap_or([0; 4]);
    // SAFETY: forwarded contract.
    unsafe { write_address(out, length, address, port) }
}

/// `getpeername(2)`.
///
/// # Safety
/// `out` and `length` must be writable, as `getpeername` requires.
pub unsafe fn peer_address(socket: &Socket, out: *mut u8, length: *mut u32) -> i64 {
    let peer = info(socket)
        .filter(|info| info.peer != [0; 4])
        .map(|info| (info.peer, info.peer_port))
        .or(socket.peer);
    let Some((address, port)) = peer else {
        return -ENOTCONN;
    };
    if !socket.connected {
        return -ENOTCONN;
    }
    // SAFETY: forwarded contract.
    unsafe { write_address(out, length, address, port) }
}

/// What the service reports about a socket.
struct Info {
    state: u8,
    local_port: u16,
    peer: [u8; 4],
    peer_port: u16,
}

fn info(socket: &Socket) -> Option<Info> {
    let mut request = [0u8; 4];
    request.copy_from_slice(&socket.id.to_le_bytes());
    let mut reply = [0u8; 16];
    let len = crate::fs::call(channel(), net::NET_SOCKET_INFO, &[&request], &mut reply).ok()?;
    if len != 9 {
        return None;
    }
    let mut peer = [0; 4];
    peer.copy_from_slice(&reply[3..7]);
    Some(Info {
        state: reply[0],
        local_port: u16::from_le_bytes([reply[1], reply[2]]),
        peer,
        peer_port: u16::from_le_bytes([reply[7], reply[8]]),
    })
}

/// `SO_ERROR`: a refused or timed-out connection is visible here, which is how
/// a non-blocking `connect` reports its outcome.
pub fn pending_error(socket: &Socket) -> i64 {
    let Some(info) = info(socket) else { return 0 };
    if info.state == net::socket_state::CLOSED && socket.connected {
        return -ECONNREFUSED;
    }
    0
}

/// `getsockopt(2)` and `setsockopt(2)`.
///
/// The stack has no tunable socket options; a program that asks for one gets
/// the answer that changes nothing rather than a failure it cannot act on.
pub fn option(level: i32, name: i32) -> i64 {
    const SOL_SOCKET: i32 = 1;
    const IPPROTO_TCP: i32 = 6;
    let _ = name;
    match level {
        SOL_SOCKET | IPPROTO_TCP => 0,
        _ => -ENOPROTOOPT,
    }
}

/// Wait until `id` reports one of `interest`, or the timeout passes.
fn wait_ready(id: u32, interest: u8, timeout_ms: u32) -> i64 {
    let mut request = [0u8; 10];
    request[0..4].copy_from_slice(&timeout_ms.to_le_bytes());
    request[4] = 1;
    request[5..9].copy_from_slice(&id.to_le_bytes());
    request[9] = interest;
    let mut reply = [0u8; 16];
    match crate::fs::call(channel(), net::NET_SOCKET_POLL, &[&request], &mut reply) {
        Ok(_) => 0,
        Err(errno) => -errno,
    }
}

/// `poll(2)`: ask about a set of sockets at once.
///
/// `entries` pairs a service socket id with the readiness bits wanted; `bits`
/// receives one entry per socket. Everything that is not a socket is ready
/// already, so a poll with no sockets in it only has to sleep.
pub fn poll(entries: &[(u32, u8)], timeout_ms: u32, bits: &mut [u8]) -> i64 {
    if entries.is_empty() || !available() {
        if timeout_ms != 0 && timeout_ms != WAIT_FOREVER {
            crate::sleep_until(
                libnode::clock_ns().saturating_add(timeout_ms as u64 * 1_000_000),
            );
        }
        return 0;
    }
    let mut request = [0u8; 5 + 5 * 32];
    request[0..4].copy_from_slice(&timeout_ms.to_le_bytes());
    let count = entries.len().min(32);
    request[4] = count as u8;
    for (index, (id, interest)) in entries.iter().take(count).enumerate() {
        let at = 5 + index * 5;
        request[at..at + 4].copy_from_slice(&id.to_le_bytes());
        request[at + 4] = *interest;
    }
    let mut reply = [0u8; 32];
    let len = match crate::fs::call(
        channel(),
        net::NET_SOCKET_POLL,
        &[&request[..5 + count * 5]],
        &mut reply,
    ) {
        Ok(len) => len,
        Err(errno) => return -errno,
    };
    for index in 0..count.min(bits.len()).min(len) {
        bits[index] = reply[index];
    }
    0
}
