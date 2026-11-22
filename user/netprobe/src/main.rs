//! `netprobe` -- the network service's smoke test, and its user manual.
//!
//! ```text
//!   netprobe                       GET / from QEMU's host at 10.0.2.2:80
//!   netprobe 93.184.215.14 80 /    GET / from that address
//!   netprobe dns example.com       one A query, over UDP, to the name server
//! ```
//!
//! It talks to the network service directly, without musl or liblx, so a
//! failure here is the stack or the service and never the Linux personality.

#![no_std]
#![no_main]

extern crate alloc;

use alloc::string::String;
use alloc::vec::Vec;

use libnode::{println, Channel, Env};
use net::stack::Kind;
use net::{socket_kind, status, Received, SocketClient};

const DEFAULT_TARGET: [u8; 4] = [10, 0, 2, 2];
const DEFAULT_PORT: u16 = 80;
const DEFAULT_PATH: &str = "/";
const TIMEOUT_MS: u32 = 10_000;

libnode::main! {
    fn main(env: Env) -> i32 {
        run(env)
    }
}

fn run(mut env: Env) -> i32 {
    // The shell hands the session out in order; the network channel is last.
    let Some(index) = env.extras().len().checked_sub(1) else {
        println!("netprobe: no network service connection");
        return 1;
    };
    let Some(handle) = env.take_extra(index) else {
        println!("netprobe: no network service connection");
        return 1;
    };
    let client = SocketClient::from_channel(Channel::from_handle(handle));

    let args: Vec<String> = env.args().iter().skip(1).cloned().collect();
    let info = match client.info() {
        Ok(info) => info,
        Err(error) => {
            println!("netprobe: the network service did not answer ({error:?})");
            return 1;
        }
    };
    println!(
        "netprobe: mac {:02x}:{:02x}:{:02x}:{:02x}:{:02x}:{:02x}, link {}, \
         {}.{}.{}.{}/{} via {}.{}.{}.{}, dns {}.{}.{}.{}",
        info.mac_address[0],
        info.mac_address[1],
        info.mac_address[2],
        info.mac_address[3],
        info.mac_address[4],
        info.mac_address[5],
        if info.link_up { "up" } else { "down" },
        info.config.address[0],
        info.config.address[1],
        info.config.address[2],
        info.config.address[3],
        info.config.prefix_length,
        info.config.gateway[0],
        info.config.gateway[1],
        info.config.gateway[2],
        info.config.gateway[3],
        info.config.dns[0],
        info.config.dns[1],
        info.config.dns[2],
        info.config.dns[3]
    );
    if !info.configured {
        println!("netprobe: the interface has no address yet");
        return 1;
    }

    if args.first().map(String::as_str) == Some("dns") {
        let Some(name) = args.get(1) else {
            println!("usage: netprobe dns <name>");
            return 2;
        };
        return probe_dns(&client, info.config.dns, name);
    }

    let target = match args.first() {
        Some(text) => match parse_ipv4(text) {
            Some(address) => address,
            None => {
                println!("netprobe: {text} is not an IPv4 address (try: netprobe dns <name>)");
                return 2;
            }
        },
        None => DEFAULT_TARGET,
    };
    let port = match args.get(1) {
        Some(text) => match text.parse::<u16>() {
            Ok(port) => port,
            Err(_) => {
                println!("netprobe: {text} is not a port");
                return 2;
            }
        },
        None => DEFAULT_PORT,
    };
    let path = args.get(2).map(String::as_str).unwrap_or(DEFAULT_PATH);
    probe_http(&client, target, port, path)
}

fn probe_http(client: &SocketClient, target: [u8; 4], port: u16, path: &str) -> i32 {
    let Some(id) = open(client, Kind::Tcp) else { return 1 };
    println!("netprobe: connecting to {}.{}.{}.{}:{port}", target[0], target[1], target[2], target[3]);
    match client.connect(id, target, port, TIMEOUT_MS) {
        Ok(status::OK) => {}
        Ok(other) => {
            println!("netprobe: connect failed (status {other})");
            let _ = client.close(id);
            return 1;
        }
        Err(error) => {
            println!("netprobe: connect failed ({error:?})");
            let _ = client.close(id);
            return 1;
        }
    }
    let request = alloc::format!(
        "GET {path} HTTP/1.0\r\nHost: {}.{}.{}.{}\r\nUser-Agent: netprobe\r\n\r\n",
        target[0], target[1], target[2], target[3]
    );
    let mut sent = 0;
    while sent < request.len() {
        match client.send(id, &request.as_bytes()[sent..]) {
            Ok((status::OK, written)) if written > 0 => sent += written,
            Ok((status::AGAIN, _)) => {
                let _ = client.poll(&[(id, net::ready::WRITE)], 1000);
            }
            Ok((other, _)) => {
                println!("netprobe: send failed (status {other})");
                let _ = client.close(id);
                return 1;
            }
            Err(error) => {
                println!("netprobe: send failed ({error:?})");
                let _ = client.close(id);
                return 1;
            }
        }
    }
    println!("netprobe: request sent, {} bytes", request.len());

    let mut total = 0;
    let mut bytes = Vec::new();
    loop {
        match client.receive(id, 4096, TIMEOUT_MS) {
            Ok(Received::Data { bytes: data, .. }) => {
                total += data.len();
                if bytes.len() < 2048 {
                    bytes.extend_from_slice(&data);
                }
            }
            Ok(Received::Eof) => {
                println!("netprobe: response complete, {total} bytes");
                break;
            }
            Ok(Received::Again) => {
                println!("netprobe: timed out after {total} bytes");
                break;
            }
            Ok(Received::Closed) => {
                println!("netprobe: connection closed after {total} bytes");
                break;
            }
            Err(error) => {
                println!("netprobe: receive failed ({error:?})");
                break;
            }
        }
    }
    let _ = client.close(id);

    for line in bytes.split_inclusive(|byte| *byte == b'\n').take(32) {
        print_line(line);
    }
    if total > 0 { 0 } else { 1 }
}

fn probe_dns(client: &SocketClient, server: [u8; 4], name: &str) -> i32 {
    let Some(query) = build_query(name) else {
        println!("netprobe: {name} is not a hostname");
        return 2;
    };
    let Some(id) = open(client, Kind::Udp) else { return 1 };
    println!(
        "netprobe: asking {}.{}.{}.{} for {name}",
        server[0], server[1], server[2], server[3]
    );
    match client.send_to(id, server, 53, &query) {
        Ok((status::OK, _)) => {}
        Ok((other, _)) => {
            println!("netprobe: dns send failed (status {other})");
            let _ = client.close(id);
            return 1;
        }
        Err(error) => {
            println!("netprobe: dns send failed ({error:?})");
            let _ = client.close(id);
            return 1;
        }
    }
    let reply = match client.receive(id, 512, TIMEOUT_MS) {
        Ok(Received::Data { bytes, .. }) => bytes,
        Ok(other) => {
            println!("netprobe: no answer ({other:?})");
            let _ = client.close(id);
            return 1;
        }
        Err(error) => {
            println!("netprobe: dns receive failed ({error:?})");
            let _ = client.close(id);
            return 1;
        }
    };
    let _ = client.close(id);
    report_answer(name, &reply)
}

fn report_answer(name: &str, reply: &[u8]) -> i32 {
    if reply.len() < 12 {
        println!("netprobe: short DNS reply");
        return 1;
    }
    let answers = u16::from_be_bytes([reply[6], reply[7]]);
    let rcode = reply[3] & 0x0F;
    println!("netprobe: DNS reply: {answers} answers, rcode {rcode}, {} bytes", reply.len());
    if answers == 0 || rcode != 0 {
        return 1;
    }
    // Walk past the question, then read every answer, keeping the A records.
    let Some(mut at) = skip_name(reply, 12) else {
        println!("netprobe: malformed DNS question");
        return 1;
    };
    at += 4; // qtype, qclass
    let mut found = 0;
    for _ in 0..answers {
        let Some(next) = skip_name(reply, at) else { break };
        at = next;
        if at + 10 > reply.len() {
            break;
        }
        let kind = u16::from_be_bytes([reply[at], reply[at + 1]]);
        let class = u16::from_be_bytes([reply[at + 2], reply[at + 3]]);
        let length = u16::from_be_bytes([reply[at + 8], reply[at + 9]]) as usize;
        at += 10;
        if at + length > reply.len() {
            break;
        }
        if kind == 1 && class == 1 && length == 4 {
            println!(
                "netprobe: {name} is {}.{}.{}.{}",
                reply[at], reply[at + 1], reply[at + 2], reply[at + 3]
            );
            found += 1;
        }
        at += length;
    }
    if found == 0 {
        println!("netprobe: no A record for {name}");
        return 1;
    }
    0
}

/// Step over a DNS name, which is either a run of labels or a compression
/// pointer to one.
fn skip_name(reply: &[u8], start: usize) -> Option<usize> {
    let mut at = start;
    loop {
        let length = *reply.get(at)? as usize;
        if length == 0 {
            return Some(at + 1);
        }
        if length & 0xC0 == 0xC0 {
            return Some(at + 2);
        }
        at += 1 + length;
    }
}

fn open(client: &SocketClient, kind: Kind) -> Option<u32> {
    let code = match kind {
        Kind::Tcp => socket_kind::TCP,
        Kind::Udp => socket_kind::UDP,
    };
    match client.open(code, 0) {
        Ok(id) => Some(id),
        Err(error) => {
            println!("netprobe: opening a socket failed ({error:?})");
            None
        }
    }
}

fn print_line(line: &[u8]) {
    let text = String::from_utf8_lossy(line);
    let text = text.trim_end();
    if !text.is_empty() {
        println!("netprobe: | {text}");
    }
}

fn parse_ipv4(text: &str) -> Option<[u8; 4]> {
    let mut parts = text.split('.');
    let mut address = [0; 4];
    for part in &mut address {
        *part = parts.next()?.parse::<u8>().ok()?;
    }
    if parts.next().is_some() {
        return None;
    }
    Some(address)
}

/// Build a DNS query for an A record: header, one label-encoded name, then the
/// question's type and class.
fn build_query(name: &str) -> Option<Vec<u8>> {
    let mut query = Vec::with_capacity(64);
    query.extend_from_slice(&[
        0x42, 0x43, // id
        0x01, 0x00, // recursion desired
        0x00, 0x01, // one question
        0x00, 0x00, // no answers
        0x00, 0x00, 0x00, 0x00,
    ]);
    let mut labels = 0;
    for label in name.trim_end_matches('.').split('.') {
        if label.is_empty() || label.len() > 63 {
            return None;
        }
        query.push(label.len() as u8);
        query.extend_from_slice(label.as_bytes());
        labels += 1;
    }
    if labels == 0 {
        return None;
    }
    query.push(0);
    query.extend_from_slice(&[0x00, 0x01, 0x00, 0x01]); // A, IN
    Some(query)
}
