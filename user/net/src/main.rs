#![no_std]
#![no_main]

extern crate alloc;

use alloc::collections::VecDeque;
use alloc::vec::Vec;
use libnode::abi::message::MessageFlags;
use libnode::abi::proto;
use libnode::abi::Signals;
use libnode::device::Device;
use libnode::wait::wait_any;
use libnode::{println, Channel, Env, Error, Message};
use net::{
    E1000Driver, LinkInfo, MAX_RX_PACKET_SIZE, NET_INFO, NET_RECEIVE, NET_TRANSMIT,
    RX_BUFFER_SIZE,
};

const MAX_QUEUED_FRAMES: usize = 32;
const POLL_INTERVAL_NS: u64 = 1_000_000;

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
    let driver = match E1000Driver::new(device) {
        Ok(driver) => driver,
        Err(error) => {
            println!("net: e1000 initialization failed ({error:?}); exiting");
            return 1;
        }
    };
    let mac = driver.mac_address();
    println!(
        "net: e1000 {:04x}:{:04x}, MAC {:02x}:{:02x}:{:02x}:{:02x}:{:02x}:{:02x}",
        info.vendor, info.device, mac[0], mac[1], mac[2], mac[3], mac[4], mac[5]
    );
    let Some(bootstrap) = env.take_bootstrap() else {
        println!("net: no bootstrap channel; exiting");
        return 1;
    };
    serve(driver, bootstrap)
}

fn serve(mut driver: E1000Driver, bootstrap: Channel) -> i32 {
    let mut clients = Vec::new();
    let mut frames = VecDeque::new();
    let mut packet = [0u8; RX_BUFFER_SIZE];
    let mut previous_link = !driver.link_up();
    loop {
        loop {
            match driver.receive_into(&mut packet) {
                Ok(Some(length)) => {
                    if length > MAX_RX_PACKET_SIZE {
                        continue;
                    }
                    if frames.len() == MAX_QUEUED_FRAMES {
                        frames.pop_front();
                    }
                    frames.push_back(Vec::from(&packet[..length]));
                }
                Ok(None) => break,
                Err(net::ReceiveError::BufferTooSmall { needed }) => {
                    println!("net: receive buffer too small for {needed}-byte frame");
                    break;
                }
            }
        }
        let link = driver.link_up();
        if link != previous_link {
            println!("net: link {}", if link { "up" } else { "down" });
            previous_link = link;
        }

        let mut watched = Vec::with_capacity(clients.len() + 1);
        watched.push((
            bootstrap.handle().raw(),
            Signals::READABLE.or(Signals::PEER_CLOSED),
        ));
        watched.extend(clients.iter().map(|channel: &Channel| {
            (
                channel.handle().raw(),
                Signals::READABLE.or(Signals::PEER_CLOSED),
            )
        }));
        let deadline = libnode::clock_ns().saturating_add(POLL_INTERVAL_NS);
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
                accept_connections(&bootstrap, &mut clients);
            }
        } else {
            let client_index = index - 1;
            if observed.contains(Signals::READABLE)
                && !handle_client(
                    &clients[client_index],
                    &mut driver,
                    &mut frames,
                )
            {
                clients.remove(client_index);
            } else if observed.contains(Signals::PEER_CLOSED) {
                clients.remove(client_index);
            }
        }
    }
}

fn accept_connections(bootstrap: &Channel, clients: &mut Vec<Channel>) {
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
            clients.push(Channel::from_handle(handle));
        }
    }
}

fn handle_client(
    client: &Channel,
    driver: &mut E1000Driver,
    frames: &mut VecDeque<Vec<u8>>,
) -> bool {
    let request = match client.try_receive() {
        Ok(request) => request,
        Err(Error::ShouldWait) => return true,
        Err(Error::PeerClosed) => return false,
        Err(error) => {
            println!("net: client receive failed ({error:?})");
            return false;
        }
    };
    if !request.flags.contains(MessageFlags::REQUEST) {
        return reply_error(client, &request, Error::InvalidArgument);
    }
    let response = match request.kind {
        NET_INFO if request.payload.is_empty() => {
            let info = LinkInfo {
                mac_address: driver.mac_address(),
                link_up: driver.link_up(),
            };
            let mut reply = Message::new(NET_INFO);
            reply.payload.extend_from_slice(&info.mac_address);
            reply.payload.push(info.link_up as u8);
            reply
        }
        NET_TRANSMIT => match driver.transmit(&request.payload) {
            Ok(()) => Message::new(NET_TRANSMIT),
            Err(error) => return reply_error(client, &request, error),
        },
        NET_RECEIVE if request.payload.is_empty() => {
            let mut reply = Message::new(NET_RECEIVE);
            if let Some(frame) = frames.pop_front() {
                reply.payload = frame;
            }
            reply
        }
        NET_INFO | NET_RECEIVE => {
            return reply_error(client, &request, Error::InvalidArgument);
        }
        _ => return reply_error(client, &request, Error::NotSupported),
    };
    match client.reply(&request, response) {
        Ok(()) => true,
        Err(Error::PeerClosed) => false,
        Err(error) => {
            println!("net: client reply failed ({error:?})");
            false
        }
    }
}

fn reply_error(client: &Channel, request: &Message, error: Error) -> bool {
    let mut reply = Message::new(request.kind);
    reply.flags = MessageFlags::ERROR;
    reply.payload.extend_from_slice(&error.to_raw().to_le_bytes());
    match client.reply(request, reply) {
        Ok(()) => true,
        Err(Error::PeerClosed) => false,
        Err(reply_error) => {
            println!("net: error reply failed ({reply_error:?})");
            false
        }
    }
}
