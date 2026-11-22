//! `net` -- the network service.
//!
//! It owns the Ethernet controller, runs the TCP/IP stack over it, and answers
//! socket requests from other processes -- `netprobe`, and liblx on behalf of
//! every Linux program that opens a socket.
//!
//! The shape is a single-threaded event loop:
//!
//! ```text
//!   poll the stack (ARP, TCP timers, DHCP, retransmits)
//!   complete any requests that were waiting for a socket to become ready
//!   wait on the launcher channel and every client channel, until either a
//!     message arrives or the stack has a timer to fire
//! ```
//!
//! Requests that cannot be answered yet -- a connect waiting for its handshake,
//! a receive waiting for bytes, a poll waiting for readiness -- are parked with
//! their reply channel and a deadline, and answered from the loop. That is what
//! makes `recv` block and `poll` cost nothing while a download is idle.

#![no_std]
#![no_main]

extern crate alloc;

use alloc::vec;
use alloc::vec::Vec;

use libnode::abi::message::MessageFlags;
use libnode::abi::proto;
use libnode::abi::Signals;
use libnode::device::Device;
use libnode::wait::wait_any;
use libnode::{println, Channel, Env, Error, Message};
use net::stack::{Connecting, Kind, Recv, Received, Sent, Stack};
use net::{ready, socket_kind, socket_state, status};

/// How often the loop returns to the stack even with nothing to do.
const TICK_NS: u64 = 1_000_000;

/// Largest receive the service will assemble in one reply.
const MAX_RECV: usize = 32 * 1024;

/// Most requests parked at once, across all clients.
const MAX_PENDING: usize = 32;

libnode::main! {
    fn main(env: Env) -> i32 {
        run(env)
    }
}

fn run(mut env: Env) -> i32 {
    let Some(device_handle) = env.take_extra(0) else {
        println!("net: no Ethernet device was granted; exiting");
        return 1;
    };
    let device = Device::from_handle(device_handle);
    let info = match device.info() {
        Ok(info) => info,
        Err(error) => {
            println!("net: device_info failed ({error:?}); exiting");
            return 1;
        }
    };
    let driver = match net::E1000Driver::new(device) {
        Ok(driver) => driver,
        Err(error) => {
            println!("net: e1000 initialization failed ({error:?}); exiting");
            return 1;
        }
    };
    let mac = driver.mac_address();
    println!(
        "net: e1000 {:04x}:{:04x}, MAC {:02x}:{:02x}:{:02x}:{:02x}:{:02x}:{:02x}, link {}",
        info.vendor,
        info.device,
        mac[0],
        mac[1],
        mac[2],
        mac[3],
        mac[4],
        mac[5],
        if driver.link_up() { "up" } else { "down" }
    );
    let Some(bootstrap) = env.take_bootstrap() else {
        println!("net: no bootstrap channel; exiting");
        return 1;
    };
    let stack = Stack::new(driver, mac, libnode::clock_ns());
    serve(stack, bootstrap)
}

/// One connected client process.
struct Client {
    id: u64,
    channel: Channel,
}

/// A request that is waiting for the stack.
struct Pending {
    client: u64,
    request: Message,
    /// Absolute deadline in nanoseconds; `0` waits indefinitely.
    deadline_ns: u64,
    op: Op,
}

/// What a parked request is waiting for.
enum Op {
    Connect(net::Id),
    Receive { socket: net::Id, max: usize },
    Poll(Vec<(net::Id, u8)>),
}

fn serve(mut stack: Stack, bootstrap: Channel) -> i32 {
    let mut clients: Vec<Client> = Vec::new();
    let mut pending: Vec<Pending> = Vec::new();
    let mut next_client = 1u64;
    let mut buffer = vec![0u8; MAX_RECV];
    let mut previous_link = stack.link_up();

    loop {
        let now = libnode::clock_ns();
        stack.poll(now);
        let link = stack.link_up();
        if link != previous_link {
            println!("net: link {}", if link { "up" } else { "down" });
            previous_link = link;
        }
        complete_pending(&mut stack, &mut clients, &mut pending, &mut buffer, now);

        // Sleep until a client sends something, the stack needs a timer, or a
        // parked request runs out of time.
        let mut deadline = now.saturating_add(TICK_NS);
        let stack_delay = stack.poll_delay_ms(now).min(1000) * 1_000_000;
        deadline = deadline.min(now.saturating_add(stack_delay));
        for request in &pending {
            if request.deadline_ns != 0 {
                deadline = deadline.min(request.deadline_ns);
            }
        }

        let mut watched = Vec::with_capacity(clients.len() + 1);
        watched.push((bootstrap.handle().raw(), Signals::READABLE.or(Signals::PEER_CLOSED)));
        watched.extend(clients.iter().map(|client| {
            (client.channel.handle().raw(), Signals::READABLE.or(Signals::PEER_CLOSED))
        }));

        let (index, observed) = match wait_any(&watched, deadline) {
            Ok(ready) => ready,
            Err(Error::TimedOut) => continue,
            Err(error) => {
                println!("net: wait failed ({error:?}); stopping service");
                return 1;
            }
        };

        if index == 0 {
            if observed.contains(Signals::READABLE) {
                accept(&bootstrap, &mut clients, &mut next_client);
            }
            continue;
        }

        let client = index - 1;
        let id = clients[client].id;
        let message = match clients[client].channel.try_receive() {
            Ok(message) => message,
            Err(Error::ShouldWait) => continue,
            Err(Error::PeerClosed) => {
                disconnect(&mut clients, &mut pending, id, client);
                continue;
            }
            Err(error) => {
                println!("net: client receive failed ({error:?})");
                disconnect(&mut clients, &mut pending, id, client);
                continue;
            }
        };
        if !message.flags.contains(MessageFlags::REQUEST) {
            println!("net: dropping a message that was not a request");
            continue;
        }
        match answer(&mut stack, &mut buffer, &message) {
            Answer::Reply(reply) => {
                if let Err(Error::PeerClosed) = clients[client].channel.reply(&message, reply) {
                    disconnect(&mut clients, &mut pending, id, client);
                }
            }
            Answer::Later(op, timeout_ms) => {
                if pending.len() == MAX_PENDING {
                    let reply = failure(message.kind, Error::NoSpace);
                    let _ = clients[client].channel.reply(&message, reply);
                    continue;
                }
                // `u32::MAX` waits indefinitely, `0` does not wait at all,
                // and anything else is a deadline. A caller that must not
                // block a program (a non-blocking connect or receive) uses
                // zero and gets its answer on the next turn of the loop.
                let now = libnode::clock_ns();
                let deadline = match timeout_ms {
                    u32::MAX => 0,
                    0 => now,
                    ms => now.saturating_add(ms as u64 * 1_000_000),
                };
                pending.push(Pending { client: id, request: message, deadline_ns: deadline, op });
            }
            Answer::Reject(error) => {
                let reply = failure(message.kind, error);
                if let Err(Error::PeerClosed) = clients[client].channel.reply(&message, reply) {
                    disconnect(&mut clients, &mut pending, id, client);
                }
            }
        }
    }
}

/// A client's channel closed: forget it and everything it was waiting for.
fn disconnect(clients: &mut Vec<Client>, pending: &mut Vec<Pending>, id: u64, index: usize) {
    pending.retain(|request| request.client != id);
    clients.remove(index);
}

fn accept(bootstrap: &Channel, clients: &mut Vec<Client>, next_client: &mut u64) {
    loop {
        let message = match bootstrap.try_receive() {
            Ok(message) => message,
            Err(Error::ShouldWait) => return,
            Err(Error::PeerClosed) => {
                println!("net: launcher connection closed");
                return;
            }
            Err(error) => {
                println!("net: bootstrap receive failed ({error:?})");
                return;
            }
        };
        if message.kind != proto::CONNECT || message.handles.len() != 1 {
            println!("net: ignored malformed connection request");
            continue;
        }
        if let Some(handle) = message.handles.into_iter().next() {
            clients.push(Client { id: *next_client, channel: Channel::from_handle(handle) });
            *next_client += 1;
        }
    }
}

/// The service's answer to one request.
enum Answer {
    /// Send this reply now.
    Reply(Message),
    /// Nobody can answer yet; park it with a timeout in milliseconds
    /// (`u32::MAX` waits indefinitely, `0` does not wait).
    Later(Op, u32),
    /// The request itself is wrong.
    Reject(Error),
}

fn answer(stack: &mut Stack, buffer: &mut [u8], request: &Message) -> Answer {
    let payload = &request.payload;
    match request.kind {
        net::NET_INFO => {
            let info = net::NetInfo {
                mac_address: stack.mac_address(),
                link_up: stack.link_up(),
                configured: stack.configured,
                config: stack.config,
            };
            Answer::Reply(Message::new(net::NET_INFO).with_payload(&net::encode_info(&info)))
        }
        net::NET_SOCKET_OPEN => {
            if payload.len() != 3 {
                return Answer::Reject(Error::InvalidArgument);
            }
            let kind = match payload[0] {
                socket_kind::TCP => Kind::Tcp,
                socket_kind::UDP => Kind::Udp,
                _ => return Answer::Reject(Error::InvalidArgument),
            };
            let port = u16::from_le_bytes([payload[1], payload[2]]);
            match stack.open(kind, port) {
                Some(id) => Answer::Reply(
                    Message::new(net::NET_SOCKET_OPEN).with_payload(&id.to_le_bytes()),
                ),
                None => Answer::Reject(Error::NoSpace),
            }
        }
        net::NET_SOCKET_CONNECT => {
            let Some((id, address, port, timeout_ms)) = decode_connect(payload) else {
                return Answer::Reject(Error::InvalidArgument);
            };
            match stack.connect(id, address, port) {
                Connecting::Established => Answer::Reply(status_message(net::NET_SOCKET_CONNECT, status::OK)),
                Connecting::Refused => Answer::Reply(status_message(
                    net::NET_SOCKET_CONNECT,
                    status::REFUSED,
                )),
                Connecting::Pending => Answer::Later(Op::Connect(id), timeout_ms),
            }
        }
        net::NET_SOCKET_SEND => {
            let Some(id) = decode_id(payload) else {
                return Answer::Reject(Error::InvalidArgument);
            };
            if payload.len() < 11 {
                return Answer::Reject(Error::InvalidArgument);
            }
            let mut address = [0u8; 4];
            address.copy_from_slice(&payload[4..8]);
            let port = u16::from_le_bytes([payload[8], payload[9]]);
            let data = &payload[10..];
            let mut reply = Message::new(net::NET_SOCKET_SEND);
            match stack.send(id, address, port, data) {
                Sent::Bytes(written) => {
                    reply.payload.push(status::OK);
                    reply.payload.extend_from_slice(&(written as u32).to_le_bytes());
                }
                Sent::Again => {
                    reply.payload.push(status::AGAIN);
                    reply.payload.extend_from_slice(&0u32.to_le_bytes());
                }
                Sent::Reset => reply.payload.push(status::RESET),
                Sent::NotConnected => reply.payload.push(status::NOT_CONNECTED),
            }
            Answer::Reply(reply)
        }
        net::NET_SOCKET_RECV => {
            let Some(id) = decode_id(payload) else {
                return Answer::Reject(Error::InvalidArgument);
            };
            if payload.len() < 12 {
                return Answer::Reject(Error::InvalidArgument);
            }
            let max = u32::from_le_bytes([payload[4], payload[5], payload[6], payload[7]]) as usize;
            let timeout_ms = u32::from_le_bytes([payload[8], payload[9], payload[10], payload[11]]);
            let max = max.min(buffer.len());
            let received = stack.recv(id, &mut buffer[..max]);
            match received.outcome {
                Recv::Bytes(got) => {
                    Answer::Reply(data_message(&buffer[..got], received.from))
                }
                Recv::Eof => Answer::Reply(status_message(net::NET_SOCKET_RECV, status::EOF)),
                Recv::Again => Answer::Later(Op::Receive { socket: id, max }, timeout_ms),
            }
        }
        net::NET_SOCKET_POLL => {
            if payload.len() < 5 {
                return Answer::Reject(Error::InvalidArgument);
            }
            let timeout_ms = u32::from_le_bytes([payload[0], payload[1], payload[2], payload[3]]);
            let count = payload[4] as usize;
            if payload.len() != 5 + count * 5 {
                return Answer::Reject(Error::InvalidArgument);
            }
            let mut entries = Vec::with_capacity(count);
            for index in 0..count {
                let at = 5 + index * 5;
                let id = u32::from_le_bytes([payload[at], payload[at + 1], payload[at + 2], payload[at + 3]]);
                entries.push((id, payload[at + 4]));
            }
            match poll_ready(stack, &entries) {
                Some(bits) => Answer::Reply(Message::new(net::NET_SOCKET_POLL).with_payload(&bits)),
                None => Answer::Later(Op::Poll(entries), timeout_ms),
            }
        }
        net::NET_SOCKET_CLOSE => {
            let Some(id) = decode_id(payload) else {
                return Answer::Reject(Error::InvalidArgument);
            };
            stack.close(id);
            Answer::Reply(Message::new(net::NET_SOCKET_CLOSE))
        }
        net::NET_SOCKET_SHUTDOWN => {
            let Some(id) = decode_id(payload) else {
                return Answer::Reject(Error::InvalidArgument);
            };
            if payload.len() < 5 {
                return Answer::Reject(Error::InvalidArgument);
            }
            stack.shutdown(id, payload[4]);
            Answer::Reply(Message::new(net::NET_SOCKET_SHUTDOWN))
        }
        net::NET_SOCKET_INFO => {
            let Some(id) = decode_id(payload) else {
                return Answer::Reject(Error::InvalidArgument);
            };
            match stack.info(id) {
                // Only what the requested socket actually knows: an id that is
                // not open is reported as closed rather than as an error, which
                // is what getsockname on a stale descriptor should say.
                Some((state, local_port, peer, peer_port)) => {
                    let mut reply = Message::new(net::NET_SOCKET_INFO);
                    reply.payload.push(state);
                    reply.payload.extend_from_slice(&local_port.to_le_bytes());
                    reply.payload.extend_from_slice(&peer);
                    reply.payload.extend_from_slice(&peer_port.to_le_bytes());
                    Answer::Reply(reply)
                }
                None => {
                    let mut reply = Message::new(net::NET_SOCKET_INFO);
                    reply.payload.push(socket_state::CLOSED);
                    reply.payload.extend_from_slice(&0u16.to_le_bytes());
                    reply.payload.extend_from_slice(&[0; 4]);
                    reply.payload.extend_from_slice(&0u16.to_le_bytes());
                    Answer::Reply(reply)
                }
            }
        }
        _ => Answer::Reject(Error::NotSupported),
    }
}

/// Answer every parked request that the stack can now satisfy, and the ones
/// whose deadlines have passed.
fn complete_pending(
    stack: &mut Stack,
    clients: &[Client],
    pending: &mut Vec<Pending>,
    buffer: &mut [u8],
    now: u64,
) {
    let mut index = 0;
    while index < pending.len() {
        let satisfied = {
            let request = &pending[index];
            match try_complete(stack, &request.op, buffer) {
                Some(reply) => Some(reply),
                None if request.deadline_ns != 0 && now >= request.deadline_ns => {
                    Some(timeout_reply(&request.op))
                }
                None => None,
            }
        };
        let Some(reply) = satisfied else {
            index += 1;
            continue;
        };
        let client = pending[index].client;
        if let Some(target) = clients.iter().find(|entry| entry.id == client) {
            // A reply that cannot be delivered means the client closed its
            // channel; the main loop drops it when it next reads from it.
            let _ = target.channel.reply(&pending[index].request, reply);
        }
        pending.remove(index);
    }
}

fn try_complete(stack: &mut Stack, op: &Op, buffer: &mut [u8]) -> Option<Message> {
    match op {
        Op::Connect(id) => match stack.connecting(*id) {
            Connecting::Pending => None,
            Connecting::Established => Some(status_message(net::NET_SOCKET_CONNECT, status::OK)),
            Connecting::Refused => {
                Some(status_message(net::NET_SOCKET_CONNECT, status::REFUSED))
            }
        },
        Op::Receive { socket, max } => {
            let max = (*max).min(buffer.len());
            let received: Received = stack.recv(*socket, &mut buffer[..max]);
            match received.outcome {
                Recv::Bytes(got) => Some(data_message(&buffer[..got], received.from)),
                Recv::Eof => Some(status_message(net::NET_SOCKET_RECV, status::EOF)),
                Recv::Again => None,
            }
        }
        Op::Poll(entries) => poll_ready(stack, entries)
            .map(|bits| Message::new(net::NET_SOCKET_POLL).with_payload(&bits)),
    }
}

fn timeout_reply(op: &Op) -> Message {
    match op {
        Op::Connect(_) => status_message(net::NET_SOCKET_CONNECT, status::TIMEOUT),
        Op::Receive { .. } => status_message(net::NET_SOCKET_RECV, status::AGAIN),
        Op::Poll(entries) => Message::new(net::NET_SOCKET_POLL).with_payload(&vec![0u8; entries.len()]),
    }
}

/// Readiness for every entry, or `None` if none of them is ready yet.
fn poll_ready(stack: &mut Stack, entries: &[(net::Id, u8)]) -> Option<Vec<u8>> {
    let mut bits = Vec::with_capacity(entries.len());
    let mut any = false;
    for (id, interest) in entries {
        let ready = stack.readiness(*id) & (interest | ready::CLOSED);
        any |= ready != 0;
        bits.push(ready);
    }
    any.then_some(bits)
}

/// A receive reply: status, the sender's address when the stack knows it, then
/// the bytes.
fn data_message(data: &[u8], from: Option<([u8; 4], u16)>) -> Message {
    let mut reply = Message::new(net::NET_SOCKET_RECV);
    reply.payload.push(status::OK);
    let (address, port) = from.unwrap_or(([0; 4], 0));
    reply.payload.extend_from_slice(&address);
    reply.payload.extend_from_slice(&port.to_le_bytes());
    reply.payload.extend_from_slice(data);
    reply
}

fn status_message(kind: u32, status: u8) -> Message {
    let mut reply = Message::new(kind);
    reply.payload.push(status);
    reply
}

fn failure(kind: u32, error: Error) -> Message {
    let mut reply = Message::new(kind);
    reply.flags = MessageFlags::ERROR;
    reply.payload.extend_from_slice(&error.to_raw().to_le_bytes());
    reply
}

fn decode_id(payload: &[u8]) -> Option<net::Id> {
    payload
        .get(..4)
        .and_then(|bytes| bytes.try_into().ok())
        .map(u32::from_le_bytes)
}

fn decode_connect(payload: &[u8]) -> Option<(net::Id, [u8; 4], u16, u32)> {
    if payload.len() != 14 {
        return None;
    }
    let id = u32::from_le_bytes(payload[..4].try_into().ok()?);
    let mut address = [0u8; 4];
    address.copy_from_slice(&payload[4..8]);
    let port = u16::from_le_bytes([payload[8], payload[9]]);
    let timeout_ms = u32::from_le_bytes([payload[10], payload[11], payload[12], payload[13]]);
    Some((id, address, port, timeout_ms))
}
