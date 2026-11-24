//! `stress` -- hostile-userspace regression cases for the NodeOS kernel.
//!
//! Each case is one thing a buggy or malicious ring-3 program can do to the
//! kernel. Run one per boot, because a case that finds its bug usually takes
//! the machine down with it:
//!
//! ```text
//! run /bin/stress.nxf <case> [parameter]
//! run /bin/stress.nxf list
//! ```
//!
//! The host harness (`tests/stress/run.sh`) reads the serial log for these
//! markers:
//!
//! * `STRESS-BEGIN <case>`: the case started.
//! * `STRESS-EXPECT-FAULT <case>`: from here on, the kernel killing this
//!   process for a user-mode fault is a correct outcome.
//! * `STRESS-RESULT <case> PASS|FAIL|ERROR <detail>`: the verdict, when the
//!   process lives long enough to give one. A kernel panic, a hang, or QEMU
//!   exiting is a FAIL that the harness detects on its own.
//!
//! PASS means the kernel refused the operation or survived it. FAIL means the
//! weakness reproduced. ERROR means the case could not set itself up.
//!
//! Nothing here touches the disk or the network. The CPU starvation cases
//! stop after two seconds, and the quota cases stop once they cross a
//! threshold rather than draining the machine. The one case that drains it on
//! purpose is `heap-exhaust-crash`.

#![no_std]
#![no_main]

extern crate alloc;

use alloc::format;
use alloc::string::String;
use core::sync::atomic::{AtomicBool, AtomicU64, Ordering};

use libnode::abi::message::MessageDescriptor;
use libnode::abi::types::{HandleInfo, MapFlags, MapWindow, ThreadParams};
use libnode::abi::{Error, Rights, Syscall, WaitItem};
use libnode::{println, sys, Env};

libnode::main! {
    fn main(env: Env) -> i32 {
        run(env)
    }
}

// ---------------------------------------------------------------------------
// Cases
// ---------------------------------------------------------------------------

enum Outcome {
    Pass(String),
    Fail(String),
    Error(String),
}

type CaseFn = fn(Option<u64>) -> Outcome;

/// Name, weakness area, description, entry point.
const CASES: &[(&str, &str, &str, CaseFn)] = &[
    ("alive", "-", "liveness probe the harness runs after every case", case_alive),
    // 1. Arithmetic overflow on user-controlled values (kernel builds with overflow-checks).
    ("overflow-transfer", "1-overflow", "memory_transfer with an offset that wraps u64", case_overflow_transfer),
    ("overflow-map-len", "1-overflow", "memory_map with a length that wraps when page-aligned", case_overflow_map_len),
    ("overflow-map-hint", "1-overflow", "memory_map with a fixed address whose end wraps", case_overflow_map_hint),
    ("overflow-fault-offset", "1-overflow", "map at a VMO offset near u64::MAX, then fault the second page", case_overflow_fault_offset),
    // 2. Channel object graphs.
    ("chan-nest-drop", "2-channels", "close the outer end of a deep chain of endpoints nested in queues", case_chan_nest_drop),
    ("chan-self-cycle", "2-channels", "send an endpoint into its own queue, repeatedly (leaked cycles)", case_chan_self_cycle),
    // 3. Unvalidated thread entry state.
    ("thread-noncanon-rip", "3-entry", "thread_create with a non-canonical entry point", case_thread_noncanon_rip),
    ("thread-noncanon-rsp", "3-entry", "thread_create with a non-canonical stack pointer", case_thread_noncanon_rsp),
    // 4. No resource accounting.
    ("mem-commit-quota", "4-resources", "commit memory past the declared --max-memory", case_mem_commit_quota),
    ("vmo-metadata-quota", "4-resources", "pin kernel heap with empty, uncommitted VMOs", case_vmo_metadata_quota),
    ("heap-exhaust-crash", "4-resources", "drain the kernel heap completely, then make ordinary syscalls", case_heap_exhaust_crash),
    ("cpu-interactive-starve", "4-resources", "an Interactive/31 spinner, which needs no privilege", case_cpu_interactive),
    ("cpu-rt-unbounded", "4-resources", "a RealTime thread with budget 0, meaning no budget", case_cpu_rt),
    // 5. Authority leaks.
    ("caps-undeclared", "5-authority", "use a capability class the NXF header never declared", case_caps_undeclared),
    ("rights-map-noread-protect", "5-authority", "MAP-only VMO handle, then memory_protect(READ)", case_rights_protect),
    ("rights-map-noread-direct", "5-authority", "MAP-only VMO handle mapped with no permissions, then read", case_rights_direct),
];

fn run(env: Env) -> i32 {
    let args = env.args();
    // args[0] is the program path.
    let Some(name) = args.get(1) else {
        println!("usage: run /bin/stress.nxf <case> [parameter]   (or: list)");
        return 2;
    };
    if name == "list" {
        for (case, area, description, _) in CASES {
            println!("STRESS-CASE {case} {area} {description}");
        }
        return 0;
    }
    let parameter = args.get(2).and_then(|text| text.parse::<u64>().ok());
    let Some((case, _, description, entry)) = CASES.iter().find(|(case, ..)| case == name) else {
        println!("STRESS-RESULT {name} ERROR unknown case");
        return 2;
    };

    println!("STRESS-BEGIN {case} -- {description}");
    let (verdict, detail) = match entry(parameter) {
        Outcome::Pass(detail) => ("PASS", detail),
        Outcome::Fail(detail) => ("FAIL", detail),
        Outcome::Error(detail) => ("ERROR", detail),
    };
    println!("STRESS-RESULT {case} {verdict} {detail}");
    0
}

fn note(case: &str, text: &str) {
    println!("STRESS-NOTE {case}: {text}");
}

fn expect_fault(case: &str) {
    println!("STRESS-EXPECT-FAULT {case}");
}

// ---------------------------------------------------------------------------
// Raw syscall helpers. Handles here are plain integers that we close by hand,
// so that nothing outside a case's control closes them.
// ---------------------------------------------------------------------------

const PAGE: u64 = 4096;
const MIB: u64 = 1024 * 1024;

fn status(raw: i64) -> Result<u64, Error> {
    sys::status(raw).map(|value| value as u64)
}

fn close(handle: u64) {
    // SAFETY: scalar argument.
    unsafe { sys::syscall1(Syscall::CapabilityClose, handle) };
}

fn vmo_alloc(size: u64) -> Result<u64, Error> {
    // SAFETY: scalar arguments.
    status(unsafe { sys::syscall2(Syscall::MemoryAllocate, size, 0) })
}

fn map(vmo: u64, offset: u64, len: u64, flags: MapFlags, window: MapWindow, hint: u64) -> Result<u64, Error> {
    let encoded = flags.0 as u64 | ((window as u64) << 16);
    // SAFETY: scalar arguments; the kernel validates the range.
    status(unsafe { sys::syscall6(Syscall::MemoryMap, 0, vmo, offset, len, encoded, hint) })
}

fn unmap(base: u64) {
    // SAFETY: scalar arguments.
    unsafe { sys::syscall2(Syscall::MemoryUnmap, 0, base) };
}

fn protect(base: u64, flags: MapFlags) -> Result<u64, Error> {
    // SAFETY: scalar arguments.
    status(unsafe { sys::syscall3(Syscall::MemoryProtect, 0, base, flags.0 as u64) })
}

/// `memory_transfer`. `to_vmo` copies user memory at `ptr` into the object,
/// which means the kernel reads `ptr`.
fn transfer(vmo: u64, offset: u64, ptr: u64, len: u64, to_vmo: bool) -> Result<u64, Error> {
    // SAFETY: the kernel validates `ptr`; the callers pass their own buffers
    // or addresses they mean to test.
    status(unsafe { sys::syscall5(Syscall::MemoryTransfer, vmo, offset, ptr, len, to_vmo as u64) })
}

fn duplicate(handle: u64, rights: Rights) -> Result<u64, Error> {
    // SAFETY: scalar arguments.
    status(unsafe { sys::syscall2(Syscall::CapabilityDuplicate, handle, rights.0 as u64) })
}

fn channel_pair() -> Result<(u64, u64), Error> {
    let mut out = [0u32; 2];
    // SAFETY: `out` is writable and holds two handle values.
    status(unsafe { sys::syscall1(Syscall::ChannelCreate, out.as_mut_ptr() as u64) })?;
    Ok((out[0] as u64, out[1] as u64))
}

/// Send an empty message carrying `handles`, without blocking.
fn send_handles(channel: u64, handles: &[u32]) -> Result<u64, Error> {
    let descriptor = MessageDescriptor {
        kind: 0x5354_5253, // "STRS"
        flags: libnode::abi::MessageFlags::NON_BLOCKING.0,
        txid: 0,
        payload: 0,
        payload_len: 0,
        handle_count: handles.len() as u32,
        handles: handles.as_ptr() as u64,
    };
    // SAFETY: `descriptor` and `handles` are live for the call.
    status(unsafe { sys::syscall2(Syscall::ChannelSend, channel, &descriptor as *const _ as u64) })
}

fn query(handle: u64) -> Result<HandleInfo, Error> {
    let mut info = HandleInfo::default();
    // SAFETY: `info` is writable and correctly sized.
    status(unsafe { sys::syscall2(Syscall::CapabilityQuery, handle, &mut info as *mut _ as u64) })?;
    Ok(info)
}

/// Wait for `wanted` on one handle, until the absolute `deadline_ns`.
fn wait_one(handle: u64, wanted: u32, deadline_ns: u64) -> Result<u32, Error> {
    let mut item = WaitItem { handle: libnode::HandleValue(handle as u32), wanted, observed: 0, _reserved: 0 };
    // SAFETY: `item` is one writable WaitItem.
    let raw = unsafe { sys::syscall3(Syscall::Wait, &mut item as *mut _ as u64, 1, deadline_ns) };
    status(raw).map(|_| item.observed)
}

/// Sleep by waiting on a fresh channel that nothing will ever write to.
fn sleep_ms(ms: u64) {
    if let Ok((a, b)) = channel_pair() {
        let _ = wait_one(a, libnode::abi::Signals::READABLE.0, libnode::clock_ns() + ms * 1_000_000);
        close(a);
        close(b);
    }
}

/// Find this process's own process handle.
///
/// The shell sends `process: INVALID` in the bootstrap message, but
/// `process_start` installs a self-handle in the child's table right after
/// the bootstrap channel, so it is among the first few slots. Handle values
/// are `generation << 20 | slot`.
fn self_process() -> Option<u64> {
    for generation in 1..=4u64 {
        for slot in 0..64u64 {
            let value = (generation << 20) | slot;
            if query(value).is_ok_and(|info| info.object_type == 1) {
                return Some(value);
            }
        }
    }
    None
}

fn thread_create(process: u64, entry: u64, stack: u64, params: Option<&ThreadParams>) -> Result<u64, Error> {
    let params = params.map_or(0, |p| p as *const _ as u64);
    // SAFETY: `params` is null or a live ThreadParams.
    status(unsafe { sys::syscall4(Syscall::ThreadCreate, process, entry, stack, params) })
}

/// A 64 KiB stack in the stack window. Returns (vmo, base, initial rsp). The
/// returned rsp is `top - 8`, the alignment a SysV function expects on entry.
fn make_stack() -> Result<(u64, u64, u64), Error> {
    let size = 64 * 1024;
    let vmo = vmo_alloc(size)?;
    let base = map(vmo, 0, size, MapFlags::READ.or(MapFlags::WRITE), MapWindow::Stack, 0)?;
    Ok((vmo, base, base + size - 8))
}

const THREAD_TERMINATED: u32 = libnode::abi::Signals::THREAD_TERMINATED.0;

// A thread entry that never touches its stack: exits straight away. This lets
// the non-canonical-RSP case create a thread that, if the kernel gets it to
// ring 3, does nothing that needs RSP to be valid.
core::arch::global_asm!(
    ".global stress_exit_stub",
    "stress_exit_stub:",
    "    mov eax, 5", // Syscall::ThreadExit
    "    xor edi, edi",
    "    syscall",
    "    ud2",
);

extern "C" {
    fn stress_exit_stub();
}

// ---------------------------------------------------------------------------
// 0. Liveness
// ---------------------------------------------------------------------------

fn case_alive(_: Option<u64>) -> Outcome {
    // Touch the three subsystems the other cases stress: memory, channels,
    // and the scheduler.
    let Ok(vmo) = vmo_alloc(PAGE) else { return Outcome::Fail("memory_allocate failed".into()) };
    let data = *b"alive";
    let wrote = transfer(vmo, 0, data.as_ptr() as u64, 5, true);
    close(vmo);
    let Ok((a, b)) = channel_pair() else { return Outcome::Fail("channel_create failed".into()) };
    close(a);
    close(b);
    sleep_ms(10);
    match wrote {
        Ok(_) => Outcome::Pass("kernel and shell responsive".into()),
        Err(e) => Outcome::Fail(format!("memory_transfer failed ({e:?})")),
    }
}

// ---------------------------------------------------------------------------
// 1. Arithmetic overflow on user-controlled values
//
// The kernel profile sets `overflow-checks = true`, so any `a + b` on values
// userspace chose is a kernel panic waiting to happen. `Vmo::write`/`read`
// compute `offset + len`, `space::align_up` computes `len + 4095`, `map`
// computes `hint + len`, and `handle_fault` computes `page - base + offset`.
// ---------------------------------------------------------------------------

fn case_overflow_transfer(_: Option<u64>) -> Outcome {
    let Ok(vmo) = vmo_alloc(PAGE) else { return Outcome::Error("memory_allocate failed".into()) };
    let mut buffer = [0xA5u8; 32];
    let offset = u64::MAX - 15;

    note("overflow-transfer", "memory_transfer(write, offset=u64::MAX-15, len=32)");
    let write = transfer(vmo, offset, buffer.as_ptr() as u64, 32, true);
    if write.is_ok() {
        close(vmo);
        return Outcome::Fail("kernel accepted a write at a wrapping offset".into());
    }
    note("overflow-transfer", "memory_transfer(read, offset=u64::MAX-15, len=32)");
    let read = transfer(vmo, offset, buffer.as_mut_ptr() as u64, 32, false);
    close(vmo);
    match (write, read) {
        (Err(w), Err(r)) => Outcome::Pass(format!("both directions rejected ({w:?}, {r:?})")),
        _ => Outcome::Fail("kernel accepted a read at a wrapping offset".into()),
    }
}

fn case_overflow_map_len(_: Option<u64>) -> Outcome {
    let Ok(vmo) = vmo_alloc(PAGE) else { return Outcome::Error("memory_allocate failed".into()) };
    note("overflow-map-len", "memory_map(len=u64::MAX-100)");
    let result = map(vmo, 0, u64::MAX - 100, MapFlags::READ, MapWindow::Shared, 0);
    close(vmo);
    match result {
        Err(e) => Outcome::Pass(format!("rejected ({e:?})")),
        Ok(base) => Outcome::Fail(format!("kernel created a near-2^64-byte mapping at {base:#x}")),
    }
}

fn case_overflow_map_hint(_: Option<u64>) -> Outcome {
    let Ok(vmo) = vmo_alloc(2 * PAGE) else { return Outcome::Error("memory_allocate failed".into()) };
    note("overflow-map-hint", "memory_map(len=8192, hint=0xffff_ffff_ffff_f000)");
    let result = map(vmo, 0, 2 * PAGE, MapFlags::READ, MapWindow::Shared, 0xFFFF_FFFF_FFFF_F000);
    close(vmo);
    match result {
        Err(e) => Outcome::Pass(format!("rejected ({e:?})")),
        Ok(base) => Outcome::Fail(format!("kernel mapped at a wrapping fixed address {base:#x}")),
    }
}

fn case_overflow_fault_offset(_: Option<u64>) -> Outcome {
    let Ok(vmo) = vmo_alloc(PAGE) else { return Outcome::Error("memory_allocate failed".into()) };
    let Ok(scratch) = vmo_alloc(PAGE) else { return Outcome::Error("memory_allocate failed".into()) };
    note("overflow-fault-offset", "memory_map(offset=0xffff_ffff_ffff_f000, len=8192) of a 4 KiB VMO");
    let base = match map(vmo, 0xFFFF_FFFF_FFFF_F000, 2 * PAGE, MapFlags::READ.or(MapFlags::WRITE), MapWindow::Shared, 0) {
        Err(e) => {
            close(vmo);
            close(scratch);
            return Outcome::Pass(format!("map rejected an offset past the end of the object ({e:?})"));
        }
        Ok(base) => base,
    };
    note("overflow-fault-offset", "map accepted the offset; making the kernel fault the second page");
    // memory_transfer(to_vmo) makes the kernel read user memory, which goes
    // through uaccess::validate and then AddressSpace::handle_fault.
    let result = transfer(scratch, 0, base + PAGE, 16, true);
    unmap(base);
    close(vmo);
    close(scratch);
    match result {
        Err(e) => Outcome::Pass(format!(
            "map accepted an out-of-range offset, but the fault was refused cleanly ({e:?}); rejecting it at map time would be better"
        )),
        Ok(_) => Outcome::Fail("kernel serviced a fault ~2^64 bytes past the end of a 4 KiB object".into()),
    }
}

// ---------------------------------------------------------------------------
// 2. Channel object graphs
//
// A queued message owns the handles it carries, and a channel endpoint owns
// its queue. So an endpoint can be kept alive only by a message sitting in
// another endpoint's queue, and those links can be chained, or closed into a
// loop. Teardown is recursive `Drop`: one level of nesting costs one level of
// kernel stack, and kernel stacks are 16 KiB (task/stack.rs). A loop is never
// torn down at all.
// ---------------------------------------------------------------------------

fn case_chan_nest_drop(parameter: Option<u64>) -> Outcome {
    let depth = parameter.unwrap_or(20_000);
    let case = "chan-nest-drop";

    // `held` is the one endpoint we own. Everything else hangs off its queue.
    let Ok((first_writer, mut held)) = channel_pair() else { return Outcome::Error("channel_create failed".into()) };
    close(first_writer);

    let mut built = 1;
    let mut stopped = None;
    while built < depth {
        let (writer, reader) = match channel_pair() {
            Ok(pair) => pair,
            Err(e) => {
                stopped = Some(format!("channel_create failed at depth {built} ({e:?})"));
                break;
            }
        };
        // Put the endpoint we hold into `reader`'s queue, then drop the
        // writer. `reader` is now the only way to reach the whole chain.
        if let Err(e) = send_handles(writer, &[held as u32]) {
            close(writer);
            close(reader);
            close(held); // may already be gone, if the kernel consumed it
            return Outcome::Pass(format!("kernel refused to nest endpoints at depth {built} ({e:?})"));
        }
        close(writer);
        held = reader;
        built += 1;
        if built % 5000 == 0 {
            note(case, &format!("chain depth {built}"));
        }
    }

    note(case, &format!("built a chain {built} endpoints deep; closing the outermost one"));
    close(held);
    // If teardown recursed per level, the machine is already dead.
    sleep_ms(50);
    match stopped {
        Some(why) => Outcome::Pass(format!("tore down a {built}-deep chain; growth stopped: {why}")),
        None => Outcome::Pass(format!("tore down a {built}-deep chain of nested endpoints")),
    }
}

fn case_chan_self_cycle(parameter: Option<u64>) -> Outcome {
    // Each cycle strands roughly 1 KiB of kernel heap: two channel objects
    // (the closed one is kept alive by its peer's Weak), a VecDeque buffer,
    // the message's handle vector and a grant node. Two million of them is
    // several times a 512 MiB guest's memory, so a kernel that leaks them
    // runs out long before the loop ends.
    let iterations = parameter.unwrap_or(2_000_000);
    let case = "chan-self-cycle";
    for i in 0..iterations {
        let (a, b) = match channel_pair() {
            Ok(pair) => pair,
            Err(e) => {
                return Outcome::Fail(format!(
                    "channel_create failed after {i} self-cycles ({e:?}): unreachable channels are never reclaimed"
                ))
            }
        };
        // `b` goes into the queue of... `b`. Then the only handle that could
        // drain it, `a`, is closed.
        if let Err(e) = send_handles(a, &[b as u32]) {
            close(a);
            close(b);
            return Outcome::Pass(format!("kernel refused to queue an endpoint on itself ({e:?})"));
        }
        close(a);
        if i > 0 && i % 250_000 == 0 {
            note(case, &format!("{i} cycles stranded"));
        }
    }
    Outcome::Pass(format!(
        "survived {iterations} self-referential cycles (the kernel reclaims them, or it leaked less than RAM)"
    ))
}

// ---------------------------------------------------------------------------
// 3. Unvalidated thread entry state
//
// thread_create passes `entry` and `stack` straight into the trap frame that
// `enter_user` hands to `iretq`. A non-canonical RIP makes iretq raise #GP
// while the CPU is still in ring 0, after `swapgs`, which the kernel can only
// treat as a kernel bug.
// ---------------------------------------------------------------------------

fn bad_entry_case(case: &str, entry: u64, rsp: Option<u64>) -> Outcome {
    let Some(process) = self_process() else { return Outcome::Error("could not find own process handle".into()) };
    let Ok((stack_vmo, stack_base, good_rsp)) = make_stack() else {
        return Outcome::Error("could not map a thread stack".into());
    };
    let rsp = rsp.unwrap_or(good_rsp);

    // From here on, the kernel killing this process with a user-mode fault is
    // a correct outcome too.
    expect_fault(case);
    note(case, &format!("thread_create(entry={entry:#x}, rsp={rsp:#x})"));
    let thread = match thread_create(process, entry, rsp, None) {
        Err(e) => {
            unmap(stack_base);
            close(stack_vmo);
            return Outcome::Pass(format!("thread_create rejected the bad state ({e:?})"));
        }
        Ok(thread) => thread,
    };
    note(case, "thread created; waiting for it to be scheduled");
    let observed = wait_one(thread, THREAD_TERMINATED, libnode::clock_ns() + 3_000_000_000);
    sleep_ms(200);
    close(thread);
    unmap(stack_base);
    close(stack_vmo);
    Outcome::Pass(format!(
        "kernel survived the thread being scheduled (wait: {observed:?}); validating at thread_create would still be better"
    ))
}

fn case_thread_noncanon_rip(_: Option<u64>) -> Outcome {
    bad_entry_case("thread-noncanon-rip", 0x0000_8000_0000_0000, None)
}

fn case_thread_noncanon_rsp(_: Option<u64>) -> Outcome {
    bad_entry_case("thread-noncanon-rsp", stress_exit_stub as *const () as usize as u64, Some(0x0000_8000_0000_0000))
}

// ---------------------------------------------------------------------------
// 4. No resource accounting
//
// The NXF header's `max_memory_bytes` is parsed and never enforced, and
// `memory_allocate` does not check the MEMORY class. Kernel heap and user
// pages come from the same frame pool, and most kernel allocations are
// infallible (`Arc::new`, `Vec::push`), which ends in
// `__rust_alloc_error_handler`'s panic. The scheduler orders classes strictly,
// and only RealTime is gated.
// ---------------------------------------------------------------------------

/// Declared in the Makefile's stress.nxf packaging (`--max-memory`).
const DECLARED_MAX_MEMORY: u64 = 16 * MIB;
/// How far past the declaration a case goes before calling it unenforced.
const QUOTA_THRESHOLD: u64 = 64 * MIB;

fn case_mem_commit_quota(_: Option<u64>) -> Outcome {
    let case = "mem-commit-quota";
    let size = 256 * MIB;
    let Ok(vmo) = vmo_alloc(size) else { return Outcome::Error("memory_allocate(256 MiB) failed".into()) };
    let byte = [1u8];
    let pages = QUOTA_THRESHOLD / PAGE + 1;
    note(case, "committing one page at a time with memory_transfer");
    for page in 0..pages {
        if let Err(e) = transfer(vmo, page * PAGE, byte.as_ptr() as u64, 1, true) {
            close(vmo);
            let committed = page * PAGE / MIB;
            return if page * PAGE <= DECLARED_MAX_MEMORY {
                Outcome::Pass(format!("commit refused at {committed} MiB ({e:?})"))
            } else {
                Outcome::Fail(format!(
                    "commit refused only at {committed} MiB ({e:?}), past the declared max of {} MiB",
                    DECLARED_MAX_MEMORY / MIB
                ))
            };
        }
    }
    close(vmo); // frees everything committed
    Outcome::Fail(format!(
        "committed {} MiB with no complaint, but stress.nxf declares --max-memory {}M; max_memory_bytes is never enforced",
        QUOTA_THRESHOLD / MIB,
        DECLARED_MAX_MEMORY / MIB
    ))
}

/// Kernel heap a VMO's page vector costs: one `Option<Pfn>` (8 bytes) per page
/// of *virtual* size, allocated and zeroed at `memory_allocate` time.
fn metadata_bytes(size: u64) -> u64 {
    size / PAGE * 8
}

static mut HELD: [u32; 65536] = [0; 65536];

/// Allocate empty VMOs, largest first, until `limit` bytes of kernel metadata
/// are pinned or nothing more fits. Returns (count, metadata bytes, last error).
fn pin_metadata(limit: u64, smallest: u64) -> (usize, u64, Option<Error>) {
    let mut size = 1u64 << 40;
    let mut count = 0usize;
    let mut pinned = 0u64;
    let mut last = None;
    // SAFETY: single-threaded; only this function and `release_held` touch it.
    let held = unsafe { &mut *core::ptr::addr_of_mut!(HELD) };
    while pinned < limit && count < held.len() {
        match vmo_alloc(size) {
            Ok(handle) => {
                held[count] = handle as u32;
                count += 1;
                pinned += metadata_bytes(size);
            }
            Err(e) => {
                last = Some(e);
                if size <= smallest {
                    break;
                }
                size >>= 1;
            }
        }
    }
    (count, pinned, last)
}

fn release_held(count: usize) {
    // SAFETY: as in `pin_metadata`.
    let held = unsafe { &*core::ptr::addr_of!(HELD) };
    for &handle in &held[..count] {
        close(handle as u64);
    }
}

fn case_vmo_metadata_quota(_: Option<u64>) -> Outcome {
    let case = "vmo-metadata-quota";
    note(case, "allocating empty VMOs; no page is ever touched");
    let (count, pinned, last) = pin_metadata(QUOTA_THRESHOLD, 1 << 24);
    release_held(count);
    if pinned >= QUOTA_THRESHOLD {
        Outcome::Fail(format!(
            "{count} empty VMOs (0 bytes committed) pinned {} MiB of kernel heap; nothing charges object metadata to the caller",
            pinned / MIB
        ))
    } else {
        Outcome::Pass(format!("stopped after {} MiB of metadata ({last:?})", pinned / MIB))
    }
}

fn case_heap_exhaust_crash(_: Option<u64>) -> Outcome {
    let case = "heap-exhaust-crash";
    note(case, "draining the kernel heap with empty VMOs (this one is meant to hurt)");
    let (count, pinned, last) = pin_metadata(u64::MAX, PAGE);
    note(case, &format!("holding {count} VMOs, ~{} MiB of kernel heap, last error {last:?}; probing", pinned / MIB));

    let mut ok = 0;
    let probes = 64;
    for _ in 0..probes {
        if let Ok((a, b)) = channel_pair() {
            close(a);
            close(b);
            ok += 1;
        }
        if let Ok(vmo) = vmo_alloc(PAGE) {
            close(vmo);
        }
        sleep_ms(1);
    }
    release_held(count);
    sleep_ms(100);
    Outcome::Pass(format!(
        "kernel survived a drained heap ({} MiB pinned); {ok}/{probes} channel_create probes succeeded",
        pinned / MIB
    ))
}

// CPU starvation. A Normal-class witness thread counts; a spinner in a higher
// class busy-waits for two seconds and samples the count before and after. A
// witness that made no progress means every Normal-class thread on the
// machine, including the shell, console and drivers, was frozen.

static WITNESS: AtomicU64 = AtomicU64::new(0);
static WITNESS_STOP: AtomicBool = AtomicBool::new(false);
static SPIN_BEFORE: AtomicU64 = AtomicU64::new(0);
static SPIN_AFTER: AtomicU64 = AtomicU64::new(0);
const SPIN_NS: u64 = 2_000_000_000;

extern "C" fn witness_entry(_: u64) -> ! {
    while !WITNESS_STOP.load(Ordering::Relaxed) {
        WITNESS.fetch_add(1, Ordering::Relaxed);
        // Yield every iteration. With a pure spin loop here, the main thread
        // never came back from its 200 ms sleep in testing (cause not yet
        // understood; a lone Normal spinner does not reproduce it), which
        // hid what this case measures.
        // SAFETY: no arguments.
        unsafe { sys::syscall0(Syscall::Yield) };
    }
    libnode::thread::exit(0)
}

extern "C" fn spinner_entry(_: u64) -> ! {
    SPIN_BEFORE.store(WITNESS.load(Ordering::Relaxed), Ordering::Relaxed);
    let start = libnode::clock_ns();
    while libnode::clock_ns() - start < SPIN_NS {
        core::hint::spin_loop();
    }
    SPIN_AFTER.store(WITNESS.load(Ordering::Relaxed), Ordering::Relaxed);
    libnode::thread::exit(0)
}

fn starvation_case(case: &str, params: ThreadParams) -> Outcome {
    let Some(process) = self_process() else { return Outcome::Error("could not find own process handle".into()) };
    let Ok((wvmo, wbase, wrsp)) = make_stack() else { return Outcome::Error("stack mapping failed".into()) };
    let Ok((svmo, sbase, srsp)) = make_stack() else { return Outcome::Error("stack mapping failed".into()) };
    let cleanup = || {
        unmap(wbase);
        close(wvmo);
        unmap(sbase);
        close(svmo);
    };

    let normal = ThreadParams { class: 2, priority: 16, period_ns: 0, budget_ns: 0, affinity: 0 };
    let witness = match thread_create(process, witness_entry as *const () as usize as u64, wrsp, Some(&normal)) {
        Ok(t) => t,
        Err(e) => {
            cleanup();
            return Outcome::Error(format!("witness thread_create failed ({e:?})"));
        }
    };
    sleep_ms(200);
    note(case, &format!("starting spinner: class {} priority {} budget {} ns", params.class, params.priority, params.budget_ns));

    let spinner = match thread_create(process, spinner_entry as *const () as usize as u64, srsp, Some(&params)) {
        Ok(t) => t,
        Err(e) => {
            WITNESS_STOP.store(true, Ordering::Relaxed);
            let _ = wait_one(witness, THREAD_TERMINATED, libnode::clock_ns() + 1_000_000_000);
            close(witness);
            cleanup();
            return Outcome::Pass(format!("kernel refused the privileged class ({e:?})"));
        }
    };
    let _ = wait_one(spinner, THREAD_TERMINATED, libnode::clock_ns() + 4 * SPIN_NS);
    WITNESS_STOP.store(true, Ordering::Relaxed);
    let _ = wait_one(witness, THREAD_TERMINATED, libnode::clock_ns() + 1_000_000_000);
    close(spinner);
    close(witness);
    cleanup();

    let progress = SPIN_AFTER.load(Ordering::Relaxed).wrapping_sub(SPIN_BEFORE.load(Ordering::Relaxed));
    if progress == 0 {
        Outcome::Fail(format!(
            "Normal-class threads got no CPU for {} s; an unprivileged spinner freezes the shell, console and drivers",
            SPIN_NS / 1_000_000_000
        ))
    } else {
        Outcome::Pass(format!("Normal-class witness still advanced {progress} times while the spinner ran"))
    }
}

fn case_cpu_interactive(_: Option<u64>) -> Outcome {
    starvation_case(
        "cpu-interactive-starve",
        ThreadParams { class: 1, priority: 31, period_ns: 0, budget_ns: 0, affinity: 0 },
    )
}

fn case_cpu_rt(_: Option<u64>) -> Outcome {
    starvation_case("cpu-rt-unbounded", ThreadParams { class: 0, priority: 0, period_ns: 0, budget_ns: 0, affinity: 0 })
}

// ---------------------------------------------------------------------------
// 5. Authority leaks
// ---------------------------------------------------------------------------

fn case_caps_undeclared(_: Option<u64>) -> Outcome {
    // stress.nxf is packaged with only `--cap CHANNEL --cap MEMORY`. The
    // kernel's granted set should therefore not contain PROCESS.
    let name = b"stress-probe";
    // SAFETY: `name` is live for the call.
    let result = status(unsafe { sys::syscall3(Syscall::ProcessCreate, name.as_ptr() as u64, name.len() as u64, 0) });
    match result {
        Ok(handle) => {
            close(handle); // never started, so this frees it
            Outcome::Fail(
                "process_create succeeded, but stress.nxf declares only CHANNEL+MEMORY; the launcher asks for u32::MAX \
                 (libnode session::launch) and nothing narrows the grant to the NXF declaration"
                    .into(),
            )
        }
        Err(Error::CapabilityDenied) => Outcome::Pass("PROCESS class was not granted".into()),
        Err(e) => Outcome::Error(format!("process_create failed for an unrelated reason ({e:?})")),
    }
}

const SECRET: [u8; 16] = *b"NODEOS-SECRET-42";

/// A VMO holding SECRET, plus a MAP-only duplicate of it. The original handle
/// is closed, so the duplicate is the only authority the process holds.
fn map_only_handle() -> Result<u64, String> {
    let vmo = vmo_alloc(PAGE).map_err(|e| format!("memory_allocate failed ({e:?})"))?;
    transfer(vmo, 0, SECRET.as_ptr() as u64, 16, true).map_err(|e| format!("seeding failed ({e:?})"))?;
    let weak = duplicate(vmo, Rights::MAP).map_err(|e| format!("duplicate failed ({e:?})"))?;
    close(vmo);
    let info = query(weak).map_err(|e| format!("query failed ({e:?})"))?;
    if info.rights & Rights::READ.0 != 0 {
        return Err(format!("duplicate kept READ (rights {:#x})", info.rights));
    }
    Ok(weak)
}

fn case_rights_protect(_: Option<u64>) -> Outcome {
    let case = "rights-map-noread-protect";
    let weak = match map_only_handle() {
        Ok(h) => h,
        Err(why) => return Outcome::Error(why),
    };
    let base = match map(weak, 0, PAGE, MapFlags(0), MapWindow::Shared, 0) {
        Ok(base) => base,
        Err(e) => {
            close(weak);
            return Outcome::Pass(format!("a no-permission mapping of a MAP-only handle was refused ({e:?})"));
        }
    };
    note(case, &format!("MAP-only handle mapped with no permissions at {base:#x}; asking memory_protect for READ"));
    if let Err(e) = protect(base, MapFlags::READ) {
        unmap(base);
        close(weak);
        return Outcome::Pass(format!("memory_protect refused to add READ ({e:?})"));
    }
    // The region now claims READ. Have the kernel copy it into a fresh VMO.
    let Ok(sink) = vmo_alloc(PAGE) else { return Outcome::Error("memory_allocate failed".into()) };
    let copied = transfer(sink, 0, base, 16, true);
    let mut out = [0u8; 16];
    let _ = transfer(sink, 0, out.as_mut_ptr() as u64, 16, false);
    close(sink);
    unmap(base);
    close(weak);
    match copied {
        Ok(_) if out == SECRET => Outcome::Fail(
            "memory_protect added READ to a mapping of a handle without READ, and the kernel copied out its contents".into(),
        ),
        Ok(_) => Outcome::Fail("memory_protect added READ to a mapping of a handle without READ".into()),
        Err(e) => Outcome::Fail(format!("memory_protect added READ to a READ-less handle's mapping (copy-out then failed: {e:?})")),
    }
}

fn case_rights_direct(_: Option<u64>) -> Outcome {
    let case = "rights-map-noread-direct";
    let weak = match map_only_handle() {
        Ok(h) => h,
        Err(why) => return Outcome::Error(why),
    };
    let base = match map(weak, 0, PAGE, MapFlags(0), MapWindow::Shared, 0) {
        Ok(base) => base,
        Err(e) => {
            close(weak);
            return Outcome::Pass(format!("a no-permission mapping of a MAP-only handle was refused ({e:?})"));
        }
    };
    note(case, &format!("MAP-only handle mapped with no permissions at {base:#x}; reading it from ring 3"));
    // A correct kernel kills us here: a region without READ must not be
    // demand-paged in on a read fault.
    expect_fault(case);
    // SAFETY: deliberately reading a mapping we were not granted READ on.
    // Either the kernel faults us (correct) or the read returns the page.
    let seen = unsafe { core::ptr::read_volatile(base as *const [u8; 16]) };
    unmap(base);
    close(weak);
    if seen == SECRET {
        Outcome::Fail("read the contents of a VMO through a handle that has MAP but not READ".into())
    } else {
        Outcome::Fail("a mapping with no permissions was readable from ring 3 (contents differed)".into())
    }
}
