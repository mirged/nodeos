//! The network service: an e1000 driver, a TCP/IP stack and the message
//! protocol clients use to reach it.
//!
//! There are two halves and they meet in the middle:
//!
//! * [`E1000Driver`] programs the Intel 82540EM: descriptor rings, DMA, one
//!   Ethernet frame in and one out.
//! * [`stack::Stack`] is smoltcp over that driver: ARP, IPv4, TCP, UDP and
//!   DHCP, with no operating system underneath it.
//!
//! On top of the stack sits a socket table and a small request/reply protocol.
//! A client does not get a network "stack" object; it gets a channel to this
//! service and asks for an IPv4 socket, connects it, sends and receives on it,
//! and polls it for readiness. liblx turns those messages into the Linux socket
//! system calls, which is how `curl` reaches the internet (docs/MUSL.md).
//!
//! ```text
//!   C program --send--> musl --__lx_syscall--> liblx --NET_SOCKET_SEND--> net
//!                                                                        |
//!                                                      smoltcp -- e1000 --+
//! ```

#![no_std]

extern crate alloc;

pub mod stack;

use alloc::vec::Vec;
use core::ptr::{copy_nonoverlapping, read_volatile, write_volatile};
use core::sync::atomic::{fence, Ordering};

use libnode::abi::message::MessageFlags;
use libnode::abi::types::{device_kind, MapFlags};
use libnode::device::{Device, Dma};
use libnode::mem::Mapping;
use libnode::{Channel, Error, Message};

pub use stack::{ClientConfig, Id};

// ---------------------------------------------------------------------------
// The service protocol
// ---------------------------------------------------------------------------

/// Link state, addresses and station MAC for this interface.
pub const NET_INFO: u32 = 0x4E45_0001;
/// Open a socket. `kind: u8`, `port: u16`; replies with an id `u32`.
pub const NET_SOCKET_OPEN: u32 = 0x4E45_0011;
/// Connect a TCP socket or set a UDP peer. Blocks until established.
pub const NET_SOCKET_CONNECT: u32 = 0x4E45_0012;
/// Queue bytes for transmission. Replies with the number accepted (`u32`).
pub const NET_SOCKET_SEND: u32 = 0x4E45_0013;
/// Receive bytes. Blocks until data, EOF or the timeout.
pub const NET_SOCKET_RECV: u32 = 0x4E45_0014;
/// Wait until any of a set of sockets is ready, or the timeout passes.
pub const NET_SOCKET_POLL: u32 = 0x4E45_0015;
/// Close a socket.
pub const NET_SOCKET_CLOSE: u32 = 0x4E45_0016;
/// Shut down one direction of a socket. `how: u8` as `SHUT_*`.
pub const NET_SOCKET_SHUTDOWN: u32 = 0x4E45_0017;
/// Ask about a socket's state, local port and peer.
pub const NET_SOCKET_INFO: u32 = 0x4E45_0018;

/// Socket kinds a client may open.
pub mod socket_kind {
    /// A reliable byte stream.
    pub const TCP: u8 = 1;
    /// A datagram socket.
    pub const UDP: u8 = 2;
}

/// Readiness bits, both in [`NET_SOCKET_POLL`] requests and its reply, and in
/// [`SocketInfo::state`]'s neighbourhood: they mirror `poll(2)`.
pub mod ready {
    /// Data is available, or end of file has been reached.
    pub const READ: u8 = 1;
    /// Space is available in the send buffer.
    pub const WRITE: u8 = 2;
    /// The connection is gone; reads return EOF and writes fail.
    pub const CLOSED: u8 = 4;
}

/// What an operation did, as the first byte of most socket replies.
///
/// Errors in the protocol itself -- an id that was never opened, a malformed
/// request -- come back as an `ERROR` message instead; these are outcomes of a
/// well-formed operation.
pub mod status {
    /// The operation did what was asked.
    pub const OK: u8 = 0;
    /// Nothing was available before the timeout.
    pub const AGAIN: u8 = 1;
    /// The peer will send nothing more, or is gone.
    pub const EOF: u8 = 2;
    /// The peer refused the connection.
    pub const REFUSED: u8 = 3;
    /// The connection attempt ran out of time.
    pub const TIMEOUT: u8 = 4;
    /// There is no route, or the interface has no address yet.
    pub const UNREACHABLE: u8 = 5;
    /// The connection was established and then lost.
    pub const RESET: u8 = 6;
    /// Nothing is connected on this socket.
    pub const NOT_CONNECTED: u8 = 7;
}

/// A socket's connection state, as [`NET_SOCKET_INFO`] reports it.
pub mod socket_state {
    /// Nothing is connected.
    pub const CLOSED: u8 = 0;
    /// A TCP connection is being established.
    pub const CONNECTING: u8 = 1;
    /// Data may flow.
    pub const ESTABLISHED: u8 = 2;
    /// One side has started to close.
    pub const CLOSING: u8 = 3;
}

/// Link properties returned by [`SocketClient::info`].
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct NetInfo {
    /// Station MAC address.
    pub mac_address: [u8; 6],
    /// Whether carrier is currently reported.
    pub link_up: bool,
    /// The interface has an address and a default route.
    pub configured: bool,
    /// Addresses clients should use.
    pub config: ClientConfig,
}

/// What [`SocketClient::state`] reports about one socket.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct SocketInfo {
    /// One of [`socket_state`].
    pub state: u8,
    /// The port this side is bound to.
    pub local_port: u16,
    /// The peer, once connected.
    pub peer: [u8; 4],
    /// The peer's port.
    pub peer_port: u16,
}

/// What [`SocketClient::receive`] returned.
#[derive(Debug)]
pub enum Received {
    /// Bytes from the peer, and the address they came from.
    Data {
        /// The bytes themselves.
        bytes: Vec<u8>,
        /// The sender, which for a datagram socket is not necessarily the peer
        /// it was connected to.
        from: ([u8; 4], u16),
    },
    /// The peer shut the connection down.
    Eof,
    /// Nothing arrived before the timeout.
    Again,
    /// The connection is gone.
    Closed,
}

/// Client for the network service's TCP/IP channel protocol.
pub struct SocketClient {
    channel: Channel,
}

impl SocketClient {
    /// Use a network channel capability received from the launcher.
    pub const fn from_channel(channel: Channel) -> Self {
        Self { channel }
    }

    /// The underlying channel.
    pub fn channel(&self) -> &Channel {
        &self.channel
    }

    /// Link state, addresses and the station MAC.
    pub fn info(&self) -> Result<NetInfo, Error> {
        let reply = self.channel.call_with(Message::new(NET_INFO), 32, 0)?;
        check_reply(&reply)?;
        decode_info(&reply.payload)
    }

    /// Open a socket. `port` of zero asks for an ephemeral one.
    pub fn open(&self, kind: u8, port: u16) -> Result<u32, Error> {
        let mut request = Message::new(NET_SOCKET_OPEN);
        request.payload.push(kind);
        request.payload.extend_from_slice(&port.to_le_bytes());
        let reply = self.channel.call_with(request, 8, 0)?;
        check_reply(&reply)?;
        decode_u32(&reply.payload)
    }

    /// Connect a socket to `address:port`, waiting up to `timeout_ms` (`0`
    /// waits as long as the stack takes). Returns the status byte:
    /// [`status::OK`], [`status::REFUSED`], [`status::TIMEOUT`] or
    /// [`status::UNREACHABLE`].
    pub fn connect(
        &self,
        id: u32,
        address: [u8; 4],
        port: u16,
        timeout_ms: u32,
    ) -> Result<u8, Error> {
        let mut request = Message::new(NET_SOCKET_CONNECT);
        request.payload.extend_from_slice(&id.to_le_bytes());
        request.payload.extend_from_slice(&address);
        request.payload.extend_from_slice(&port.to_le_bytes());
        request.payload.extend_from_slice(&timeout_ms.to_le_bytes());
        let reply = self.channel.call_with(request, 16, 0)?;
        check_reply(&reply)?;
        reply.payload.first().copied().ok_or(Error::BadState)
    }

    /// Queue bytes for transmission on a connected socket. Returns the status
    /// byte and how many bytes the stack accepted; [`status::AGAIN`] means the
    /// send buffer is full.
    pub fn send(&self, id: u32, data: &[u8]) -> Result<(u8, usize), Error> {
        self.send_to(id, [0; 4], 0, data)
    }

    /// Queue a datagram for `address:port`, or bytes on a TCP connection
    /// (where the address is ignored).
    pub fn send_to(
        &self,
        id: u32,
        address: [u8; 4],
        port: u16,
        data: &[u8],
    ) -> Result<(u8, usize), Error> {
        let mut request = Message::new(NET_SOCKET_SEND);
        request.payload.extend_from_slice(&id.to_le_bytes());
        request.payload.extend_from_slice(&address);
        request.payload.extend_from_slice(&port.to_le_bytes());
        request.payload.extend_from_slice(data);
        let reply = self.channel.call_with(request, 16, 0)?;
        check_reply(&reply)?;
        let status = reply.payload.first().copied().ok_or(Error::BadState)?;
        let written = reply
            .payload
            .get(1..5)
            .and_then(|bytes| bytes.try_into().ok())
            .map(u32::from_le_bytes)
            .unwrap_or(0);
        Ok((status, written as usize))
    }

    /// Receive up to `max` bytes, waiting up to `timeout_ms`.
    pub fn receive(&self, id: u32, max: usize, timeout_ms: u32) -> Result<Received, Error> {
        let mut request = Message::new(NET_SOCKET_RECV);
        request.payload.extend_from_slice(&id.to_le_bytes());
        request.payload.extend_from_slice(&(max as u32).to_le_bytes());
        request.payload.extend_from_slice(&timeout_ms.to_le_bytes());
        let reply = self.channel.call_with(request, max.max(16) + 8, 0)?;
        check_reply(&reply)?;
        let payload = &reply.payload;
        let status = payload.first().copied().ok_or(Error::BadState)?;
        if status != status::OK {
            return match status {
                status::EOF => Ok(Received::Eof),
                status::AGAIN => Ok(Received::Again),
                status::RESET | status::NOT_CONNECTED => Ok(Received::Closed),
                _ => Err(Error::BadState),
            };
        }
        // status, the sender's address, its port, then the bytes.
        if payload.len() < 7 {
            return Err(Error::BadState);
        }
        let mut address = [0; 4];
        address.copy_from_slice(&payload[1..5]);
        let port = u16::from_le_bytes([payload[5], payload[6]]);
        Ok(Received::Data { bytes: payload[7..].to_vec(), from: (address, port) })
    }

    /// Wait until any of `entries` (`(socket, ready bits)`) is ready.
    pub fn poll(&self, entries: &[(u32, u8)], timeout_ms: u32) -> Result<Vec<u8>, Error> {
        let mut request = Message::new(NET_SOCKET_POLL);
        request.payload.extend_from_slice(&timeout_ms.to_le_bytes());
        request.payload.push(entries.len() as u8);
        for (id, interest) in entries {
            request.payload.extend_from_slice(&id.to_le_bytes());
            request.payload.push(*interest);
        }
        let reply = self.channel.call_with(request, 64, 0)?;
        check_reply(&reply)?;
        Ok(reply.payload)
    }

    /// Close a socket.
    pub fn close(&self, id: u32) -> Result<(), Error> {
        let mut request = Message::new(NET_SOCKET_CLOSE);
        request.payload.extend_from_slice(&id.to_le_bytes());
        let reply = self.channel.call_with(request, 16, 0)?;
        check_reply(&reply)
    }

    /// Shut down one direction. `how`: 0 read, 1 write, 2 both.
    pub fn shutdown(&self, id: u32, how: u8) -> Result<(), Error> {
        let mut request = Message::new(NET_SOCKET_SHUTDOWN);
        request.payload.extend_from_slice(&id.to_le_bytes());
        request.payload.push(how);
        let reply = self.channel.call_with(request, 16, 0)?;
        check_reply(&reply)
    }

    /// Connection state, local port and peer for a socket.
    pub fn state(&self, id: u32) -> Result<SocketInfo, Error> {
        let mut request = Message::new(NET_SOCKET_INFO);
        request.payload.extend_from_slice(&id.to_le_bytes());
        let reply = self.channel.call_with(request, 32, 0)?;
        check_reply(&reply)?;
        decode_socket_info(&reply.payload)
    }
}

fn check_reply(reply: &Message) -> Result<(), Error> {
    if !reply.flags.contains(MessageFlags::ERROR) {
        return Ok(());
    }
    let raw = reply
        .payload
        .get(..8)
        .and_then(|bytes| bytes.try_into().ok())
        .map(i64::from_le_bytes)
        .ok_or(Error::BadState)?;
    Err(Error::from_raw(raw).unwrap_or(Error::BadState))
}

fn decode_u32(bytes: &[u8]) -> Result<u32, Error> {
    bytes
        .get(..4)
        .and_then(|bytes| bytes.try_into().ok())
        .map(u32::from_le_bytes)
        .ok_or(Error::BadState)
}

/// Decode the 21-byte [`NET_INFO`] reply.
pub fn decode_info(payload: &[u8]) -> Result<NetInfo, Error> {
    if payload.len() != 21 {
        return Err(Error::BadState);
    }
    let mut mac_address = [0; 6];
    mac_address.copy_from_slice(&payload[..6]);
    let mut address = [0; 4];
    address.copy_from_slice(&payload[8..12]);
    let mut gateway = [0; 4];
    gateway.copy_from_slice(&payload[13..17]);
    let mut dns = [0; 4];
    dns.copy_from_slice(&payload[17..21]);
    Ok(NetInfo {
        mac_address,
        link_up: payload[6] != 0,
        configured: payload[7] != 0,
        config: ClientConfig { address, prefix_length: payload[12], gateway, dns },
    })
}

/// Encode the 21-byte [`NET_INFO`] reply.
pub fn encode_info(info: &NetInfo) -> [u8; 21] {
    let mut out = [0u8; 21];
    out[..6].copy_from_slice(&info.mac_address);
    out[6] = info.link_up as u8;
    out[7] = info.configured as u8;
    out[8..12].copy_from_slice(&info.config.address);
    out[12] = info.config.prefix_length;
    out[13..17].copy_from_slice(&info.config.gateway);
    out[17..21].copy_from_slice(&info.config.dns);
    out
}

/// Decode the 9-byte [`NET_SOCKET_INFO`] reply.
pub fn decode_socket_info(payload: &[u8]) -> Result<SocketInfo, Error> {
    if payload.len() != 9 {
        return Err(Error::BadState);
    }
    let mut peer = [0; 4];
    peer.copy_from_slice(&payload[3..7]);
    Ok(SocketInfo {
        state: payload[0],
        local_port: u16::from_le_bytes([payload[1], payload[2]]),
        peer,
        peer_port: u16::from_le_bytes([payload[7], payload[8]]),
    })
}

// ---------------------------------------------------------------------------
// The e1000 driver
// ---------------------------------------------------------------------------

const E1000_CTRL: usize = 0x0000;
const E1000_STATUS: usize = 0x0008;
const E1000_CTRL_RST: u32 = 1 << 26;
const E1000_CTRL_SLU: u32 = 1 << 6;
const E1000_STATUS_LINK_UP: u32 = 1 << 1;
const E1000_IMC: usize = 0x00D8;
const E1000_RCTL: usize = 0x0100;
const E1000_TCTL: usize = 0x0400;
const E1000_TIPG: usize = 0x0410;
const E1000_RAL0: usize = 0x5400;
const E1000_RAH0: usize = 0x5404;
const E1000_TDBAL: usize = 0x3800;
const E1000_TDBAH: usize = 0x3804;
const E1000_TDLEN: usize = 0x3808;
const E1000_TDH: usize = 0x3810;
const E1000_TDT: usize = 0x3818;
const E1000_RDBAL: usize = 0x2800;
const E1000_RDBAH: usize = 0x2804;
const E1000_RDLEN: usize = 0x2808;
const E1000_RDH: usize = 0x2810;
const E1000_RDT: usize = 0x2818;

/// Minimum e1000 BAR0 span required by this driver.
pub const E1000_BAR0_LEN: u64 = 0x6000;
const NUM_TX_DESCRIPTORS: usize = 64;
const NUM_RX_DESCRIPTORS: usize = 64;
/// Receive buffer capacity, in bytes.
pub const RX_BUFFER_SIZE: usize = 2048;
/// Maximum untagged Ethernet frame size accepted for transmission.
pub const MAX_TX_PACKET_SIZE: usize = 1518;
const TX_BUFFER_SIZE: usize = 2048;
const DESCRIPTOR_SIZE: usize = 16;
const TX_RING_OFFSET: usize = 0;
const RX_RING_OFFSET: usize = 4096;
const RX_BUFFER_OFFSET: usize = 8192;
const TX_BUFFER_OFFSET: usize = RX_BUFFER_OFFSET + NUM_RX_DESCRIPTORS * RX_BUFFER_SIZE;
const DMA_ARENA_LEN: u64 = (TX_BUFFER_OFFSET + NUM_TX_DESCRIPTORS * TX_BUFFER_SIZE) as u64;
const DESC_DD: u8 = 1;
const RX_STATUS_EOP: u8 = 1 << 1;
const TX_CMD_EOP: u8 = 1;
const TX_CMD_IFCS: u8 = 1 << 1;
const TX_CMD_RS: u8 = 1 << 3;
const RESET_POLL_LIMIT: usize = 1_000_000;

#[repr(C)]
#[derive(Clone, Copy, Default)]
struct TxDescriptor {
    addr: u64,
    length: u16,
    checksum_offset: u8,
    command: u8,
    status: u8,
    checksum_start: u8,
    special: u16,
}

#[repr(C)]
#[derive(Clone, Copy, Default)]
struct RxDescriptor {
    addr: u64,
    length: u16,
    checksum: u16,
    status: u8,
    errors: u8,
    special: u16,
}

/// Why a received Ethernet frame could not be copied to a caller's buffer.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ReceiveError {
    /// The frame remains queued; retry with a buffer at least this large.
    BufferTooSmall { needed: usize },
}

/// A polled e1000 driver backed by a device capability and contiguous DMA VMO.
pub struct E1000Driver {
    _device: Device,
    _mmio: Mapping,
    dma: Dma,
    dma_base: *mut u8,
    mmio_base: usize,
    mac: [u8; 6],
    tx_next_to_use: usize,
    tx_next_to_clean: usize,
    rx_next_to_clean: usize,
}

impl E1000Driver {
    /// Validate and initialize the claimed Intel 82540EM-compatible device.
    pub fn new(device: Device) -> Result<Self, Error> {
        let info = device.info()?;
        if info.kind != device_kind::E1000 || info.vendor != 0x8086 || info.device != 0x100E {
            return Err(Error::WrongType);
        }

        let mmio = device.map(
            E1000_BAR0_LEN,
            MapFlags::READ
                .or(MapFlags::WRITE)
                .or(MapFlags::DEVICE)
                .or(MapFlags::POPULATE),
        )?;
        let mut dma = device.dma_alloc(DMA_ARENA_LEN)?;
        let dma_base = dma.bytes().as_mut_ptr();
        let mmio_base = mmio.base() as usize;

        // SAFETY: DeviceDmaAlloc returns an exclusively owned mapping that is
        // physically contiguous, and both descriptor rings fit in this arena.
        let tx_ring = unsafe { dma_base.add(TX_RING_OFFSET).cast::<TxDescriptor>() };
        let rx_ring = unsafe { dma_base.add(RX_RING_OFFSET).cast::<RxDescriptor>() };
        for index in 0..NUM_TX_DESCRIPTORS {
            unsafe { write_volatile(tx_ring.add(index), TxDescriptor::default()) };
        }
        for index in 0..NUM_RX_DESCRIPTORS {
            let descriptor = RxDescriptor {
                addr: dma.phys() + (RX_BUFFER_OFFSET + index * RX_BUFFER_SIZE) as u64,
                ..RxDescriptor::default()
            };
            unsafe { write_volatile(rx_ring.add(index), descriptor) };
        }

        let mut driver = Self {
            _device: device,
            _mmio: mmio,
            dma,
            dma_base,
            mmio_base,
            mac: [0; 6],
            tx_next_to_use: 0,
            tx_next_to_clean: 0,
            rx_next_to_clean: NUM_RX_DESCRIPTORS - 1,
        };

        driver.write_reg(E1000_IMC, u32::MAX);
        driver.write_reg(E1000_CTRL, driver.read_reg(E1000_CTRL) | E1000_CTRL_RST);
        let mut reset = false;
        for _ in 0..RESET_POLL_LIMIT {
            if driver.read_reg(E1000_CTRL) & E1000_CTRL_RST == 0 {
                reset = true;
                break;
            }
            core::hint::spin_loop();
        }
        if !reset {
            return Err(Error::TimedOut);
        }

        driver.write_reg(E1000_IMC, u32::MAX);
        driver.write_reg(E1000_CTRL, driver.read_reg(E1000_CTRL) | E1000_CTRL_SLU);
        driver.mac = driver.read_mac()?;

        let tx_phys = driver.dma.phys() + TX_RING_OFFSET as u64;
        driver.write_reg(E1000_TDBAL, tx_phys as u32);
        driver.write_reg(E1000_TDBAH, (tx_phys >> 32) as u32);
        driver.write_reg(E1000_TDLEN, (NUM_TX_DESCRIPTORS * DESCRIPTOR_SIZE) as u32);
        driver.write_reg(E1000_TDH, 0);
        driver.write_reg(E1000_TDT, 0);
        driver.write_reg(E1000_TIPG, 10 | (8 << 10) | (6 << 20));
        driver.write_reg(E1000_TCTL, (1 << 1) | (1 << 3) | (0x0F << 4) | (0x40 << 12));

        let rx_phys = driver.dma.phys() + RX_RING_OFFSET as u64;
        driver.write_reg(E1000_RDBAL, rx_phys as u32);
        driver.write_reg(E1000_RDBAH, (rx_phys >> 32) as u32);
        driver.write_reg(E1000_RDLEN, (NUM_RX_DESCRIPTORS * DESCRIPTOR_SIZE) as u32);
        driver.write_reg(E1000_RDH, 0);
        driver.write_reg(E1000_RDT, (NUM_RX_DESCRIPTORS - 1) as u32);
        // Enable receive, accept broadcast, and strip the Ethernet CRC.
        driver.write_reg(E1000_RCTL, (1 << 1) | (1 << 15) | (1 << 26));
        fence(Ordering::SeqCst);
        Ok(driver)
    }

    /// The hardware's station MAC address.
    pub fn mac_address(&self) -> [u8; 6] {
        self.mac
    }

    /// Whether the controller currently reports carrier/link.
    pub fn link_up(&self) -> bool {
        self.read_reg(E1000_STATUS) & E1000_STATUS_LINK_UP != 0
    }

    /// Queue one raw Ethernet frame for transmission.
    ///
    /// The frame is copied into a DMA-owned bounce buffer, so callers may pass
    /// any ordinary slice without needing physical-address translation.
    pub fn transmit(&mut self, frame: &[u8]) -> Result<(), Error> {
        if frame.is_empty() || frame.len() > MAX_TX_PACKET_SIZE {
            return Err(Error::InvalidArgument);
        }
        self.clean_tx();

        let index = self.tx_next_to_use;
        if (index + 1) % NUM_TX_DESCRIPTORS == self.tx_next_to_clean {
            return Err(Error::ShouldRetry);
        }
        let buffer_offset = TX_BUFFER_OFFSET + index * TX_BUFFER_SIZE;
        unsafe {
            copy_nonoverlapping(
                frame.as_ptr(),
                self.dma_base.add(buffer_offset),
                frame.len(),
            );
            let descriptor = self.tx_descriptor(index);
            write_volatile(
                core::ptr::addr_of_mut!((*descriptor).addr),
                self.dma.phys() + buffer_offset as u64,
            );
            write_volatile(
                core::ptr::addr_of_mut!((*descriptor).length),
                frame.len() as u16,
            );
            write_volatile(
                core::ptr::addr_of_mut!((*descriptor).command),
                TX_CMD_EOP | TX_CMD_IFCS | TX_CMD_RS,
            );
            write_volatile(core::ptr::addr_of_mut!((*descriptor).status), 0);
        }
        fence(Ordering::Release);
        self.tx_next_to_use = (index + 1) % NUM_TX_DESCRIPTORS;
        self.write_reg(E1000_TDT, self.tx_next_to_use as u32);
        Ok(())
    }

    /// Poll for one frame and copy it into caller-owned storage.
    ///
    /// The descriptor is returned to hardware only after the copy completes.
    /// An undersized output buffer leaves the frame pending for a retry.
    pub fn receive_into(&mut self, out: &mut [u8]) -> Result<Option<usize>, ReceiveError> {
        for _ in 0..NUM_RX_DESCRIPTORS {
            let index = (self.rx_next_to_clean + 1) % NUM_RX_DESCRIPTORS;
            let descriptor = self.rx_descriptor(index);
            let status = unsafe { read_volatile(core::ptr::addr_of!((*descriptor).status)) };
            if status & DESC_DD == 0 {
                return Ok(None);
            }
            fence(Ordering::Acquire);
            let length =
                unsafe { read_volatile(core::ptr::addr_of!((*descriptor).length)) as usize };
            let errors = unsafe { read_volatile(core::ptr::addr_of!((*descriptor).errors)) };
            if errors == 0 && status & RX_STATUS_EOP != 0 && length != 0 && length <= RX_BUFFER_SIZE
            {
                if out.len() < length {
                    return Err(ReceiveError::BufferTooSmall { needed: length });
                }
                let buffer_offset = RX_BUFFER_OFFSET + index * RX_BUFFER_SIZE;
                unsafe {
                    copy_nonoverlapping(self.dma_base.add(buffer_offset), out.as_mut_ptr(), length)
                };
                self.rearm_rx(index, descriptor);
                return Ok(Some(length));
            }
            self.rearm_rx(index, descriptor);
        }
        Ok(None)
    }

    fn read_mac(&self) -> Result<[u8; 6], Error> {
        let low = self.read_reg(E1000_RAL0);
        let high = self.read_reg(E1000_RAH0);
        if high & (1 << 31) == 0 {
            return Err(Error::BadState);
        }
        Ok([
            low as u8,
            (low >> 8) as u8,
            (low >> 16) as u8,
            (low >> 24) as u8,
            high as u8,
            (high >> 8) as u8,
        ])
    }

    fn clean_tx(&mut self) {
        while self.tx_next_to_clean != self.tx_next_to_use {
            let descriptor = self.tx_descriptor(self.tx_next_to_clean);
            let status = unsafe { read_volatile(core::ptr::addr_of!((*descriptor).status)) };
            if status & DESC_DD == 0 {
                break;
            }
            fence(Ordering::Acquire);
            self.tx_next_to_clean = (self.tx_next_to_clean + 1) % NUM_TX_DESCRIPTORS;
        }
    }

    fn rearm_rx(&mut self, index: usize, descriptor: *mut RxDescriptor) {
        unsafe {
            write_volatile(core::ptr::addr_of_mut!((*descriptor).status), 0);
            write_volatile(core::ptr::addr_of_mut!((*descriptor).errors), 0);
            write_volatile(core::ptr::addr_of_mut!((*descriptor).length), 0);
        }
        fence(Ordering::Release);
        self.write_reg(E1000_RDT, index as u32);
        self.rx_next_to_clean = index;
    }

    fn tx_descriptor(&self, index: usize) -> *mut TxDescriptor {
        unsafe {
            self.dma_base
                .add(TX_RING_OFFSET)
                .cast::<TxDescriptor>()
                .add(index)
        }
    }

    fn rx_descriptor(&self, index: usize) -> *mut RxDescriptor {
        unsafe {
            self.dma_base
                .add(RX_RING_OFFSET)
                .cast::<RxDescriptor>()
                .add(index)
        }
    }

    fn write_reg(&self, offset: usize, value: u32) {
        unsafe { write_volatile((self.mmio_base + offset) as *mut u32, value) }
    }

    fn read_reg(&self, offset: usize) -> u32 {
        unsafe { read_volatile((self.mmio_base + offset) as *const u32) }
    }
}

impl Drop for E1000Driver {
    fn drop(&mut self) {
        self.write_reg(E1000_IMC, u32::MAX);
        self.write_reg(E1000_RCTL, 0);
        self.write_reg(E1000_TCTL, 0);
        fence(Ordering::SeqCst);
    }
}
