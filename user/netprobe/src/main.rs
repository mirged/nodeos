#![no_std]
#![no_main]

use libnode::{Channel, Env, println};
use net::NetworkClient;

const TIMEOUT_NS: u64 = 3_000_000_000;
const DEFAULT_TARGET: [u8; 4] = [10, 0, 2, 2];
const DEFAULT_SOURCE: [u8; 4] = [10, 0, 2, 15];

libnode::main! {
    fn main(env: Env) -> i32 {
        run(env)
    }
}

fn run(mut env: Env) -> i32 {
    let Some(network_index) = env.extras().len().checked_sub(1) else {
        println!("netprobe: no network service connection");
        return 1;
    };
    let Some(handle) = env.take_extra(network_index) else {
        println!("netprobe: no network service connection");
        return 1;
    };
    let client = NetworkClient::from_channel(Channel::from_handle(handle));

    let args = env.args();
    if args.len() > 3 {
        println!("usage: netprobe [target-ip [source-ip]]");
        return 2;
    }
    let target = match args.get(1).map(|arg| parse_ipv4(arg)).unwrap_or(Some(DEFAULT_TARGET)) {
        Some(ip) => ip,
        None => {
            println!("netprobe: invalid target IPv4 address");
            return 2;
        }
    };
    let source = match args.get(2).map(|arg| parse_ipv4(arg)).unwrap_or(Some(DEFAULT_SOURCE)) {
        Some(ip) => ip,
        None => {
            println!("netprobe: invalid source IPv4 address");
            return 2;
        }
    };

    let info = match client.link_info() {
        Ok(info) => info,
        Err(error) => {
            println!("netprobe: link query failed: {error:?}");
            return 1;
        }
    };
    println!(
        "netprobe: link {}, local MAC {:02x}:{:02x}:{:02x}:{:02x}:{:02x}:{:02x}",
        if info.link_up { "up" } else { "down" },
        info.mac_address[0],
        info.mac_address[1],
        info.mac_address[2],
        info.mac_address[3],
        info.mac_address[4],
        info.mac_address[5]
    );
    if !info.link_up {
        println!("netprobe: refusing to send while the link is down");
        return 1;
    }

    let request = make_arp_request(info.mac_address, source, target);
    if let Err(error) = client.transmit(&request) {
        println!("netprobe: ARP request transmit failed: {error:?}");
        return 1;
    }
    println!(
        "netprobe: asking who has {}.{}.{}.{}",
        target[0], target[1], target[2], target[3]
    );

    let deadline = libnode::clock_ns().saturating_add(TIMEOUT_NS);
    while libnode::clock_ns() < deadline {
        match client.receive() {
            Ok(Some(frame)) => {
                if let Some(mac) = matching_arp_reply(&frame, info.mac_address, source, target) {
                    println!(
                        "netprobe: peer {}.{}.{}.{} is at {:02x}:{:02x}:{:02x}:{:02x}:{:02x}:{:02x}",
                        target[0],
                        target[1],
                        target[2],
                        target[3],
                        mac[0],
                        mac[1],
                        mac[2],
                        mac[3],
                        mac[4],
                        mac[5]
                    );
                    return 0;
                }
            }
            Ok(None) => libnode::yield_now(),
            Err(error) => {
                println!("netprobe: receive failed: {error:?}");
                return 1;
            }
        }
    }
    println!(
        "netprobe: no ARP reply from {}.{}.{}.{} within 3 seconds",
        target[0], target[1], target[2], target[3]
    );
    1
}

fn parse_ipv4(text: &str) -> Option<[u8; 4]> {
    let mut parts = text.split('.');
    let mut address = [0; 4];
    for part in &mut address {
        let value = parts.next()?.parse::<u8>().ok()?;
        *part = value;
    }
    if parts.next().is_some() {
        return None;
    }
    Some(address)
}

fn make_arp_request(mac: [u8; 6], source: [u8; 4], target: [u8; 4]) -> [u8; 42] {
    let mut frame = [0; 42];
    frame[..6].fill(0xff);
    frame[6..12].copy_from_slice(&mac);
    frame[12..14].copy_from_slice(&[0x08, 0x06]);
    frame[14..16].copy_from_slice(&[0x00, 0x01]);
    frame[16..18].copy_from_slice(&[0x08, 0x00]);
    frame[18] = 6;
    frame[19] = 4;
    frame[20..22].copy_from_slice(&[0x00, 0x01]);
    frame[22..28].copy_from_slice(&mac);
    frame[28..32].copy_from_slice(&source);
    frame[38..42].copy_from_slice(&target);
    frame
}

fn matching_arp_reply(
    frame: &[u8],
    local_mac: [u8; 6],
    source: [u8; 4],
    target: [u8; 4],
) -> Option<[u8; 6]> {
    if frame.len() < 42
        || frame[12..14] != [0x08, 0x06]
        || frame[14..16] != [0x00, 0x01]
        || frame[16..18] != [0x08, 0x00]
        || frame[18] != 6
        || frame[19] != 4
        || frame[20..22] != [0x00, 0x02]
        || frame[32..38] != local_mac
        || frame[38..42] != source
    {
        return None;
    }
    let mut peer_mac = [0; 6];
    peer_mac.copy_from_slice(&frame[22..28]);
    (frame[6..12] == peer_mac).then_some(peer_mac)
}
