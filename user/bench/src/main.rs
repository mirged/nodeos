//! `bench` -- a small memory and scheduler benchmark, run entirely from ring 3.
//!
//! It is deliberately unremarkable as a benchmark and useful as a smoke test:
//! it allocates a megabyte of memory objects, maps them, writes every byte with
//! a deterministic generator, reads every byte back, and reports how long that
//! took. On NodeOS that path crosses the `MEMORY` capability, demand paging,
//! the VMO refcount, and the clock -- all from a process that holds nothing but
//! channels and memory.
//!
//! Launch it from the shell with `run /bin/bench.nxf`.

#![no_std]
#![no_main]

extern crate alloc;

use alloc::string::String;
use core::fmt::Write;

use libnode::abi::proto;
use libnode::abi::types::{MapFlags, MapWindow};
use libnode::{println, Channel, Env, Message};

libnode::main! {
    fn main(env: Env) -> i32 {
        run(env)
    }
}

/// Bytes in the object under test.
const SIZE: u64 = 1024 * 1024;

fn run(mut env: Env) -> i32 {
    let Some(console_handle) = env.take_extra(0) else {
        println!("bench: no console connection granted; exiting");
        return 1;
    };
    let console = Channel::from_handle(console_handle);

    let mut report = String::new();
    report.push_str("bench: NodeOS memory and scheduler benchmark\n");

    match run_memory_test(&mut report) {
        Ok(()) => {}
        Err(error) => {
            let _ = writeln!(report, "  memory: failed ({error:?})");
        }
    }

    run_yield_test(&mut report);

    let _ = console.call(Message::new(proto::CONSOLE_WRITE_TEXT).with_payload(report.as_bytes()));
    println!("bench: done");
    0
}

/// Fill and verify a memory object, reporting throughput.
fn run_memory_test(report: &mut String) -> Result<(), libnode::Error> {
    let vmo = libnode::mem::Vmo::create(SIZE, false)?;
    let mut mapping = vmo.map(0, SIZE, MapFlags::READ.or(MapFlags::WRITE), MapWindow::Shared)?;
    // SAFETY: this process owns the mapping and no other mapping of it exists.
    let bytes = unsafe { mapping.as_mut_slice() };

    let start = libnode::clock_ns();

    // A linear congruential generator: cheap, deterministic, and dependent on
    // every byte, so a wrong page in the mapping changes the checksum.
    let mut state = 0x1234_5678u32;
    for byte in bytes.iter_mut() {
        state = state.wrapping_mul(1_664_525).wrapping_add(1_013_904_223);
        *byte = (state >> 24) as u8;
    }

    let mut verify = 0x1234_5678u32;
    let mut ok = true;
    for &byte in bytes.iter() {
        verify = verify.wrapping_mul(1_664_525).wrapping_add(1_013_904_223);
        if byte != (verify >> 24) as u8 {
            ok = false;
            break;
        }
    }

    let elapsed = libnode::clock_ns().saturating_sub(start);
    // Bytes transferred, counted once for the write pass and once for the read
    // pass, in KiB per second. Integer arithmetic: the kernel has no FPU state
    // for userspace to lean on and a percentage point of precision would not
    // earn the code.
    let transferred = SIZE * 2;
    let kib_per_s = if elapsed == 0 { 0 } else { transferred * 1_000_000_000 / elapsed / 1024 };

    let _ = writeln!(
        report,
        "  memory: {} KiB written and read back {} in {} us ({kib_per_s} KiB/s)",
        SIZE / 1024,
        if ok { "ok" } else { "CORRUPT" },
        elapsed / 1000
    );
    Ok(())
}

/// Time a run of voluntary context switches.
fn run_yield_test(report: &mut String) {
    const ROUNDS: u64 = 10_000;
    let start = libnode::clock_ns();
    for _ in 0..ROUNDS {
        libnode::yield_now();
    }
    let elapsed = libnode::clock_ns().saturating_sub(start);
    let per_yield_ns = elapsed / ROUNDS;
    let _ = writeln!(report, "  sched:  {ROUNDS} yields in {} us ({per_yield_ns} ns each)", elapsed / 1000);
}
