//! The TCP/IP stack, and the sockets clients see.
//!
//! smoltcp owns the e1000 controller now. It is a `no_std` stack with no
//! operating system behind it: it wants an Ethernet device that hands it one
//! frame at a time, and a monotonic millisecond clock, and supplies IPv4, ARP,
//! TCP, UDP and DHCP from that. This module is the join -- an smoltcp `Device`
//! over [`E1000Driver`], an [`Interface`] configured by DHCP with a static
//! fallback, and the socket table the service's protocol addresses by number.
//!
//! ```text
//!   socket table (TCP/UDP)  <-  the service's message protocol
//!         |
//!   Interface (smoltcp)  -- ARP, IPv4, TCP state machine, DHCP client
//!         |  one Ethernet frame at a time
//!   E1000Phy  -- the descriptor rings in user/net/src/lib.rs
//!         |
//!   e1000 / QEMU's user-mode network / a real card
//! ```

use alloc::collections::VecDeque;
use alloc::vec;
use alloc::vec::Vec;
use core::cell::RefCell;

use smoltcp::iface::{Config, Interface, SocketHandle, SocketSet};
use smoltcp::phy::{Device as PhyDevice, DeviceCapabilities, Medium, RxToken, TxToken};
use smoltcp::socket::{dhcpv4, tcp, udp};
use smoltcp::time::Instant;
use smoltcp::wire::{
    EthernetAddress, HardwareAddress, IpAddress, IpCidr, IpEndpoint, IpListenEndpoint, Ipv4Address,
    Ipv4Cidr,
};

use crate::E1000Driver;

/// How long DHCP gets before the stack settles on [`FALLBACK`].
pub const DHCP_TIMEOUT_NS: u64 = 4_000_000_000;

/// Most sockets the table holds at once. Each TCP socket costs two buffers, so
/// this is a memory bound, not a policy.
pub const MAX_SOCKETS: usize = 16;

/// Bytes in one direction of a TCP connection's ring.
pub const TCP_BUFFER: usize = 8 * 1024;

/// The address QEMU's user-mode network hands out, used when no DHCP server
/// answers: it keeps `make run` and the headless tests working with no host
/// configuration at all.
pub const FALLBACK: ClientConfig = ClientConfig {
    address: [10, 0, 2, 15],
    prefix_length: 24,
    gateway: [10, 0, 2, 2],
    dns: [10, 0, 2, 3],
};

/// The addresses clients need in order to reach the network.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ClientConfig {
    /// The interface's own IPv4 address.
    pub address: [u8; 4],
    /// Longest-prefix length, e.g. 24 for a `/24`.
    pub prefix_length: u8,
    /// Default gateway.
    pub gateway: [u8; 4],
    /// Name server, as learned from DHCP or the fallback.
    pub dns: [u8; 4],
}

/// Largest frame the e1000 driver accepts, FCS excluded.
const MTU: usize = 1514;

/// smoltcp's clock: whole microseconds since an arbitrary origin, which is the
/// boot here.
fn instant(now_ns: u64) -> Instant {
    Instant::from_micros((now_ns / 1000) as i64)
}

// ---------------------------------------------------------------------------
// smoltcp device over the driver
// ---------------------------------------------------------------------------

/// An smoltcp device over the polled e1000 driver.
///
/// smoltcp drives a device by asking for a receive token and a transmit token
/// and then letting the tokens' closures touch the bytes. Both are interior
/// mutable here because the tokens borrow the device immutably: the service is
/// single-threaded, so a `RefCell` costs a counter and nothing else.
pub struct E1000Phy {
    driver: RefCell<E1000Driver>,
    received: RefCell<VecDeque<Vec<u8>>>,
    frame: RefCell<Vec<u8>>,
}

impl E1000Phy {
    /// Wrap an initialized driver.
    pub fn new(driver: E1000Driver) -> Self {
        Self {
            driver: RefCell::new(driver),
            received: RefCell::new(VecDeque::new()),
            frame: RefCell::new(Vec::new()),
        }
    }

    /// The station MAC address.
    pub fn mac_address(&self) -> [u8; 6] {
        self.driver.borrow().mac_address()
    }

    /// Whether the controller reports carrier.
    pub fn link_up(&self) -> bool {
        self.driver.borrow().link_up()
    }

    /// Move whatever the card has queued into the receive queue.
    fn drain_ring(&self) {
        let mut queue = self.received.borrow_mut();
        let mut driver = self.driver.borrow_mut();
        let mut packet = [0u8; crate::RX_BUFFER_SIZE];
        // Bounded: a burst on a busy link must not starve the socket timers.
        for _ in 0..64 {
            match driver.receive_into(&mut packet) {
                Ok(Some(length)) => queue.push_back(Vec::from(&packet[..length])),
                Ok(None) => break,
                Err(crate::ReceiveError::BufferTooSmall { .. }) => {}
            }
            if queue.len() >= 64 {
                break;
            }
        }
    }
}

/// One received frame, owned until consumed.
pub struct RxFrame {
    frame: Vec<u8>,
}

impl RxToken for RxFrame {
    fn consume<R, F>(mut self, f: F) -> R
    where
        F: FnOnce(&mut [u8]) -> R,
    {
        f(&mut self.frame)
    }
}

/// The right to transmit one frame.
pub struct TxFrame<'a> {
    device: &'a E1000Phy,
}

impl TxToken for TxFrame<'_> {
    fn consume<R, F>(self, len: usize, f: F) -> R
    where
        F: FnOnce(&mut [u8]) -> R,
    {
        let mut frame = self.device.frame.borrow_mut();
        frame.resize(len, 0);
        let result = f(&mut frame);
        let mut driver = self.device.driver.borrow_mut();
        // The descriptor ring holds 64 frames; a full ring means the card has
        // not cleaned any yet, so retry a moment rather than dropping a packet
        // the stack believes it sent.
        for attempt in 0..64 {
            if driver.transmit(&frame).is_ok() {
                break;
            }
            if attempt == 63 {
                libnode::println!("net: transmit ring is stuck; dropping a frame");
            }
            libnode::yield_now();
        }
        result
    }
}

impl PhyDevice for E1000Phy {
    type RxToken<'a> = RxFrame;
    type TxToken<'a> = TxFrame<'a>;

    fn receive(&mut self, _timestamp: Instant) -> Option<(RxFrame, TxFrame<'_>)> {
        self.drain_ring();
        let frame = self.received.borrow_mut().pop_front()?;
        Some((RxFrame { frame }, TxFrame { device: &*self }))
    }

    fn transmit(&mut self, _timestamp: Instant) -> Option<TxFrame<'_>> {
        Some(TxFrame { device: &*self })
    }

    fn capabilities(&self) -> DeviceCapabilities {
        let mut caps = DeviceCapabilities::default();
        caps.medium = Medium::Ethernet;
        caps.max_transmission_unit = MTU;
        caps
    }
}

// ---------------------------------------------------------------------------
// Socket outcomes, as the protocol reports them
// ---------------------------------------------------------------------------

/// What a send attempt did.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Sent {
    /// This many bytes were queued.
    Bytes(usize),
    /// The send buffer is full; try again after a writable poll.
    Again,
    /// The peer is gone.
    Reset,
    /// Nothing is connected.
    NotConnected,
}

/// What a receive attempt found, writing any data into the caller's buffer.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Recv {
    /// This many bytes were copied out.
    Bytes(usize),
    /// The peer will send nothing more.
    Eof,
    /// Nothing has arrived yet.
    Again,
}

/// The outcome of a receive, and where a datagram came from.
///
/// `recvfrom` and `recvmsg` report the source address, which for a bound
/// datagram socket is not the peer it was connected to -- musl's resolver
/// checks it against the name servers it sent to, so getting it right matters.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Received {
    /// What happened.
    pub outcome: Recv,
    /// The sender's address, for a datagram.
    pub from: Option<([u8; 4], u16)>,
}

/// How a connection attempt stands.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Connecting {
    /// The handshake is still in progress.
    Pending,
    /// Data may flow.
    Established,
    /// The peer refused it.
    Refused,
}

/// A socket's protocol kind.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Kind {
    /// A reliable byte stream.
    Tcp,
    /// A datagram socket.
    Udp,
}

/// One open socket, and whether it is on its way out.
struct Slot {
    kind: Kind,
    handle: SocketHandle,
    /// Set once `close` was asked for: the FIN still has to finish.
    closing: bool,
}

/// A socket as the protocol names it: one past its slot index.
pub type Id = u32;

// ---------------------------------------------------------------------------
// The stack
// ---------------------------------------------------------------------------

/// The stack: one interface, one socket set, and the DHCP client driving it.
pub struct Stack {
    phy: E1000Phy,
    iface: Interface,
    sockets: SocketSet<'static>,
    dhcp: SocketHandle,
    /// Cleared once the lease has been taken and the socket removed.
    dhcp_active: bool,
    dhcp_until_ns: u64,
    slots: Vec<Option<Slot>>,
    /// The interface's addresses, as clients are told them.
    pub config: ClientConfig,
    /// Whether the interface has an address and a default route.
    pub configured: bool,
    use_fallback: bool,
    next_ephemeral: u16,
}

impl Stack {
    /// Build the interface over a driver, with DHCP as the configuration
    /// source.
    pub fn new(driver: E1000Driver, mac: [u8; 6], now_ns: u64) -> Self {
        let mut phy = E1000Phy::new(driver);
        let hardware_addr = HardwareAddress::Ethernet(EthernetAddress(mac));
        let mut config = Config::new(hardware_addr);
        config.random_seed = now_ns ^ 0x5EED_1234_9AB6_CDEF;
        let iface = Interface::new(config, &mut phy, instant(now_ns));

        let mut sockets = SocketSet::new(Vec::new());
        let dhcp = sockets.add(dhcpv4::Socket::new());

        Self {
            phy,
            iface,
            sockets,
            dhcp,
            dhcp_active: true,
            dhcp_until_ns: now_ns + DHCP_TIMEOUT_NS,
            slots: (0..MAX_SOCKETS).map(|_| None).collect(),
            config: FALLBACK,
            configured: false,
            use_fallback: false,
            next_ephemeral: 49152,
        }
    }

    /// The station MAC address.
    pub fn mac_address(&self) -> [u8; 6] {
        self.phy.mac_address()
    }

    /// Whether the link currently has carrier.
    pub fn link_up(&self) -> bool {
        self.phy.link_up()
    }

    /// Run the stack once, then apply any DHCP result and the static fallback.
    ///
    /// Returns whether anything happened, i.e. whether socket readiness may
    /// have changed.
    pub fn poll(&mut self, now_ns: u64) -> bool {
        let instant = instant(now_ns);
        let changed = self.iface.poll(instant, &mut self.phy, &mut self.sockets);
        self.apply_dhcp(now_ns);
        self.reap();
        changed
    }

    fn apply_dhcp(&mut self, now_ns: u64) {
        let event = if self.dhcp_active {
            self.sockets.get_mut::<dhcpv4::Socket>(self.dhcp).poll()
        } else {
            None
        };
        if let Some(dhcpv4::Event::Configured(config)) = event {
            let address = config.address.address().0;
            let prefix_length = config.address.prefix_len();
            let gateway = config.router.map(|router| router.0).unwrap_or(FALLBACK.gateway);
            let dns = config
                .dns_servers
                .first()
                .map(|server| server.0)
                .unwrap_or(FALLBACK.dns);
            self.set_addresses(address, prefix_length, gateway, dns);
            self.configured = true;
            libnode::println!(
                "net: DHCP {}.{}.{}.{}/{} via {}.{}.{}.{}, dns {}.{}.{}.{}",
                address[0], address[1], address[2], address[3], prefix_length,
                gateway[0], gateway[1], gateway[2], gateway[3],
                dns[0], dns[1], dns[2], dns[3]
            );
            // The lease is in: DHCP has nothing more to do, and leaving the
            // socket in the set would let a renewal race the fallback. The
            // handle is dead from here on, so nothing may look it up again.
            self.sockets.remove(self.dhcp);
            self.dhcp_active = false;
        } else if let Some(dhcpv4::Event::Deconfigured) = event {
            self.configured = false;
        }

        if !self.configured && !self.use_fallback && now_ns >= self.dhcp_until_ns {
            self.use_fallback = true;
            let address = FALLBACK.address;
            let prefix_length = FALLBACK.prefix_length;
            let gateway = FALLBACK.gateway;
            let dns = FALLBACK.dns;
            self.set_addresses(address, prefix_length, gateway, dns);
            self.configured = true;
            libnode::println!(
                "net: no DHCP answer; using {}.{}.{}.{}/{} via {}.{}.{}.{}, dns {}.{}.{}.{}",
                address[0], address[1], address[2], address[3], prefix_length,
                gateway[0], gateway[1], gateway[2], gateway[3],
                dns[0], dns[1], dns[2], dns[3]
            );
        }
    }

    fn set_addresses(
        &mut self,
        address: [u8; 4],
        prefix_length: u8,
        gateway: [u8; 4],
        dns: [u8; 4],
    ) {
        self.iface.update_ip_addrs(|addresses| {
            addresses.clear();
            let _ = addresses.push(IpCidr::Ipv4(Ipv4Cidr::new(Ipv4Address(address), prefix_length)));
        });
        let _ = self.iface.routes_mut().add_default_ipv4_route(Ipv4Address(gateway));
        self.config = ClientConfig { address, prefix_length, gateway, dns };
    }

    /// How long the stack can sleep before it wants to be polled again.
    pub fn poll_delay_ms(&mut self, now_ns: u64) -> u64 {
        let instant = instant(now_ns);
        match self.iface.poll_delay(instant, &self.sockets) {
            Some(delay) => delay.total_millis().max(1),
            None => 1000,
        }
    }

    // -- the socket table ---------------------------------------------------

    fn slot(&mut self, id: Id) -> Option<&mut Slot> {
        self.slots.get_mut(id.checked_sub(1)? as usize)?.as_mut()
    }

    fn kind_of(&self, id: Id) -> Option<Kind> {
        self.slots.get(id.checked_sub(1)? as usize)?.as_ref().map(|slot| slot.kind)
    }

    /// A free ephemeral port, on the low side of the dynamic range.
    fn ephemeral_port(&mut self) -> u16 {
        let port = self.next_ephemeral;
        self.next_ephemeral = if port == 65535 { 49152 } else { port + 1 };
        port
    }

    /// Open a socket. `port` of zero asks for an ephemeral one.
    pub fn open(&mut self, kind: Kind, port: u16) -> Option<Id> {
        let free = self.slots.iter().position(Option::is_none)?;
        let port = if port == 0 { self.ephemeral_port() } else { port };
        let handle = match kind {
            Kind::Tcp => self.sockets.add(tcp::Socket::new(
                tcp::SocketBuffer::new(vec![0; TCP_BUFFER]),
                tcp::SocketBuffer::new(vec![0; TCP_BUFFER]),
            )),
            Kind::Udp => {
                let mut socket = udp::Socket::new(
                    udp::PacketBuffer::new(vec![udp::PacketMetadata::EMPTY; 8], vec![0; 8 * 1024]),
                    udp::PacketBuffer::new(vec![udp::PacketMetadata::EMPTY; 8], vec![0; 8 * 1024]),
                );
                socket.bind(IpListenEndpoint { addr: None, port }).ok()?;
                self.sockets.add(socket)
            }
        };
        self.slots[free] = Some(Slot { kind, handle, closing: false });
        Some(free as Id + 1)
    }

    /// Start a TCP connection, or accept a UDP peer (which needs no handshake).
    pub fn connect(&mut self, id: Id, address: [u8; 4], port: u16) -> Connecting {
        if !self.configured {
            return Connecting::Refused;
        }
        match self.kind_of(id) {
            Some(Kind::Udp) => Connecting::Established,
            Some(Kind::Tcp) => {
                let Some(slot) = self.slot(id) else { return Connecting::Refused };
                let handle = slot.handle;
                let local = IpListenEndpoint { addr: None, port: self.ephemeral_port() };
                let remote = IpEndpoint::new(IpAddress::Ipv4(Ipv4Address(address)), port);
                let socket = self.sockets.get_mut::<tcp::Socket>(handle);
                match socket.connect(self.iface.context(), remote, local) {
                    // The handshake has only been started; the caller waits
                    // for `connecting` to stop saying `Pending`.
                    Ok(()) => Connecting::Pending,
                    Err(_) => Connecting::Refused,
                }
            }
            None => Connecting::Refused,
        }
    }

    /// How a connection attempt stands.
    pub fn connecting(&mut self, id: Id) -> Connecting {
        match self.kind_of(id) {
            Some(Kind::Udp) => Connecting::Established,
            Some(Kind::Tcp) => {
                let Some(handle) = self.slot(id).map(|slot| slot.handle) else {
                    return Connecting::Refused;
                };
                match self.sockets.get_mut::<tcp::Socket>(handle).state() {
                    tcp::State::Established => Connecting::Established,
                    tcp::State::Closed => Connecting::Refused,
                    _ => Connecting::Pending,
                }
            }
            None => Connecting::Refused,
        }
    }

    /// Queue bytes for transmission. For UDP, `address`/`port` name the
    /// destination; TCP ignores them.
    pub fn send(&mut self, id: Id, address: [u8; 4], port: u16, data: &[u8]) -> Sent {
        let Some(kind) = self.kind_of(id) else { return Sent::NotConnected };
        let Some(slot) = self.slot(id) else { return Sent::NotConnected };
        let handle = slot.handle;
        match kind {
            Kind::Tcp => {
                let socket = self.sockets.get_mut::<tcp::Socket>(handle);
                if !socket.is_open() || !socket.may_send() {
                    return Sent::Reset;
                }
                match socket.send_slice(data) {
                    Ok(written) => Sent::Bytes(written),
                    Err(tcp::SendError::InvalidState) => Sent::Reset,
                }
            }
            Kind::Udp => {
                let socket = self.sockets.get_mut::<udp::Socket>(handle);
                let remote = IpEndpoint::new(IpAddress::Ipv4(Ipv4Address(address)), port);
                match socket.send_slice(data, remote) {
                    Ok(()) => Sent::Bytes(data.len()),
                    Err(udp::SendError::BufferFull) => Sent::Again,
                    Err(udp::SendError::Unaddressable) => Sent::NotConnected,
                }
            }
        }
    }

    /// Copy whatever has arrived into `out`.
    pub fn recv(&mut self, id: Id, out: &mut [u8]) -> Received {
        let Some(kind) = self.kind_of(id) else {
            return Received { outcome: Recv::Eof, from: None };
        };
        let Some(slot) = self.slot(id) else {
            return Received { outcome: Recv::Eof, from: None };
        };
        let handle = slot.handle;
        match kind {
            Kind::Tcp => {
                let socket = self.sockets.get_mut::<tcp::Socket>(handle);
                let outcome = match socket.recv_slice(out) {
                    // `Ok(0)` means the buffer is empty but the connection is
                    // still open; both error variants mean nothing more will
                    // arrive, which a reader sees as end of file.
                    Ok(0) => Recv::Again,
                    Ok(got) => Recv::Bytes(got),
                    Err(_) => Recv::Eof,
                };
                Received { outcome, from: None }
            }
            Kind::Udp => {
                let socket = self.sockets.get_mut::<udp::Socket>(handle);
                match socket.recv() {
                    Ok((payload, meta)) => {
                        let got = payload.len().min(out.len());
                        out[..got].copy_from_slice(&payload[..got]);
                        let from = match meta.endpoint.addr {
                            IpAddress::Ipv4(address) => Some((address.0, meta.endpoint.port)),
                            #[allow(unreachable_patterns)]
                            _ => None,
                        };
                        Received { outcome: Recv::Bytes(got), from }
                    }
                    // `Truncated` means the datagram was larger than the
                    // caller's buffer and has been dropped; there is nothing
                    // to hand out, so it reads as "nothing yet".
                    Err(_) => Received { outcome: Recv::Again, from: None },
                }
            }
        }
    }

    /// Readiness bits: `1` readable, `2` writable, `4` closed.
    pub fn readiness(&mut self, id: Id) -> u8 {
        let Some(kind) = self.kind_of(id) else { return crate::ready::CLOSED };
        let Some(slot) = self.slot(id) else { return crate::ready::CLOSED };
        let handle = slot.handle;
        match kind {
            Kind::Tcp => {
                let socket = self.sockets.get_mut::<tcp::Socket>(handle);
                let mut ready = 0;
                if socket.can_recv() || !socket.may_recv() {
                    ready |= crate::ready::READ;
                }
                if socket.can_send() && socket.may_send() {
                    ready |= crate::ready::WRITE;
                }
                if !socket.is_open() {
                    ready |= crate::ready::CLOSED;
                }
                ready
            }
            Kind::Udp => {
                let socket = self.sockets.get_mut::<udp::Socket>(handle);
                let mut ready = 0;
                if socket.can_recv() {
                    ready |= crate::ready::READ;
                }
                if socket.can_send() {
                    ready |= crate::ready::WRITE;
                }
                ready
            }
        }
    }

    /// Close a socket. A TCP connection's FIN still has to finish, so its slot
    /// stays reserved until [`Stack::reap`] sees the stack let go of it.
    pub fn close(&mut self, id: Id) {
        let Some(kind) = self.kind_of(id) else { return };
        let Some(slot) = self.slot(id) else { return };
        let handle = slot.handle;
        slot.closing = true;
        match kind {
            Kind::Tcp => self.sockets.get_mut::<tcp::Socket>(handle).close(),
            Kind::Udp => {
                self.sockets.get_mut::<udp::Socket>(handle).close();
                self.sockets.remove(handle);
                self.slots[(id - 1) as usize] = None;
            }
        }
    }

    /// Shut one direction down: 0 read, 1 write, 2 both. The stack has a single
    /// knob (a FIN), so a write shutdown is the whole of it; a read shutdown
    /// only stops this side from looking at what arrives.
    pub fn shutdown(&mut self, id: Id, how: u8) {
        let Some(Kind::Tcp) = self.kind_of(id) else { return };
        if how & 1 == 0 {
            return;
        }
        let Some(handle) = self.slot(id).map(|slot| slot.handle) else { return };
        self.sockets.get_mut::<tcp::Socket>(handle).close();
    }

    /// Connection state, local port and peer.
    pub fn info(&mut self, id: Id) -> Option<(u8, u16, [u8; 4], u16)> {
        let kind = self.kind_of(id)?;
        let slot = self.slot(id)?;
        let handle = slot.handle;
        use crate::socket_state;
        match kind {
            Kind::Tcp => {
                let socket = self.sockets.get_mut::<tcp::Socket>(handle);
                let state = match socket.state() {
                    tcp::State::Closed | tcp::State::TimeWait => socket_state::CLOSED,
                    tcp::State::SynSent | tcp::State::SynReceived => socket_state::CONNECTING,
                    tcp::State::Established => socket_state::ESTABLISHED,
                    _ => socket_state::CLOSING,
                };
                let local = socket.local_endpoint().map(|endpoint| endpoint.port).unwrap_or(0);
                let (peer, peer_port) = match socket.remote_endpoint() {
                    Some(IpEndpoint { addr: IpAddress::Ipv4(address), port }) => (address.0, port),
                    _ => ([0; 4], 0),
                };
                Some((state, local, peer, peer_port))
            }
            Kind::Udp => {
                let socket = self.sockets.get_mut::<udp::Socket>(handle);
                Some((socket_state::ESTABLISHED, socket.endpoint().port, [0; 4], 0))
            }
        }
    }

    /// Drop sockets whose FIN has finished, freeing their ids.
    fn reap(&mut self) {
        for index in 0..self.slots.len() {
            let Some(slot) = self.slots[index].as_mut() else { continue };
            if !slot.closing || slot.kind != Kind::Tcp {
                continue;
            }
            let state = self.sockets.get_mut::<tcp::Socket>(slot.handle).state();
            if matches!(state, tcp::State::Closed | tcp::State::TimeWait) {
                let slot = self.slots[index].take().expect("checked above");
                self.sockets.remove(slot.handle);
            }
        }
    }
}
