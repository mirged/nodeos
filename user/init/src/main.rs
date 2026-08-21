//! `init` -- the first NodeOS userspace process.
//!
//! It starts with one capability: a bootstrap channel. Everything it can do
//! came out of the first message on that channel, and everything it delegates,
//! it delegates by handing over a handle.

#![no_std]
#![no_main]

extern crate alloc;

use libnode::abi::types::{MapFlags, MapWindow};
use libnode::abi::CapClass;
use libnode::mem::Vmo;
use libnode::{println, Env};

libnode::main! {
    fn main(env: Env) -> i32 {
        println!("");
        println!("init: userspace is alive");
        println!("init: image based at {:#x}, heap {} KiB",
                 env.image_base(), libnode::alloc_impl::heap_size() / 1024);

        report_capabilities(&env);
        exercise_memory();
        exercise_clock();

        println!("init: done");
        0
    }
}

fn report_capabilities(env: &Env) {
    const ALL: [(CapClass, &str); 16] = [
        (CapClass::FileRead, "FILE_READ"),
        (CapClass::FileWrite, "FILE_WRITE"),
        (CapClass::FileCreate, "FILE_CREATE"),
        (CapClass::Network, "NETWORK"),
        (CapClass::NetworkBind, "NETWORK_BIND"),
        (CapClass::Gpu, "GPU"),
        (CapClass::Audio, "AUDIO"),
        (CapClass::Memory, "MEMORY"),
        (CapClass::Process, "PROCESS"),
        (CapClass::Channel, "CHANNEL"),
        (CapClass::Device, "DEVICE"),
        (CapClass::Clock, "CLOCK"),
        (CapClass::Debug, "DEBUG"),
        (CapClass::Load, "LOAD"),
        (CapClass::Input, "INPUT"),
        (CapClass::Display, "DISPLAY"),
    ];
    let mut line = alloc::string::String::new();
    for (class, name) in ALL {
        if env.has_capability(class) {
            if !line.is_empty() {
                line.push(' ');
            }
            line.push_str(name);
        }
    }
    if line.is_empty() {
        line.push_str("(none)");
    }
    println!("init: granted capabilities: {}", line);
}

fn exercise_memory() {
    let size = 64 * 1024u64;
    let Ok(vmo) = Vmo::create(size, false) else {
        println!("init: memory_allocate FAILED");
        return;
    };
    let Ok(mut mapping) = vmo.map(0, size, MapFlags::READ.or(MapFlags::WRITE), MapWindow::Shared) else {
        println!("init: memory_map FAILED");
        return;
    };

    // SAFETY: this process is the only holder of the mapping.
    let bytes = unsafe { mapping.as_mut_slice() };
    for (index, byte) in bytes.iter_mut().enumerate() {
        *byte = (index % 251) as u8;
    }
    let mut ok = true;
    for (index, byte) in bytes.iter().enumerate() {
        if *byte != (index % 251) as u8 {
            ok = false;
            break;
        }
    }
    println!(
        "init: mapped {} KiB at {:#x}, readback {}",
        size / 1024,
        mapping.base(),
        if ok { "ok" } else { "CORRUPT" }
    );

    // Demand paging: the mapping was lazy, so the pages above were faulted in
    // one at a time by the writes.
    let mut probe = [0u8; 8];
    if vmo.read(0, &mut probe).is_ok() {
        println!("init: vmo read-back without mapping: {:?}", probe);
    }
}

fn exercise_clock() {
    let start = libnode::clock_ns();
    for _ in 0..1000 {
        libnode::yield_now();
    }
    let elapsed = libnode::clock_ns() - start;
    println!("init: 1000 yields took {} us", elapsed / 1000);
}
