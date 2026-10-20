//! # liblx -- a Linux personality for C programs on NodeOS
//!
//! musl, built with the overlay in `user/lx/musl/`, never executes a
//! `syscall` instruction. Every Linux syscall it would have made arrives here
//! instead, as an ordinary call to [`__lx_syscall`] with the Linux number and
//! the Linux arguments, and is answered in-process with the handles this
//! program was given: the console channel becomes descriptors 0-2, the
//! filesystem channel becomes `open`, memory objects become `mmap`.
//!
//! ```text
//!   C program  --printf-->  musl  --__lx_syscall(SYS_writev)-->  liblx
//!                                                                  |
//!                               CONSOLE_WRITE_TEXT on channel 0 <--+
//! ```
//!
//! liblx holds no authority of its own. It can do exactly what the handles in
//! the bootstrap message allow, which is what keeps "what can this program
//! do?" answerable on NodeOS even for code written for Linux.
//!
//! The program is single-threaded for now: `clone` reports `ENOSYS`, so the
//! state below is guarded by spin locks that are never contended. See
//! docs/MUSL.md for the phases that lift each limit.

#![no_std]
#![deny(unsafe_op_in_unsafe_fn)]

extern crate alloc;

mod errno;
mod fs;
mod mm;
mod start;

use core::sync::atomic::{AtomicU64, Ordering};

use nodeos_abi::Syscall;

use errno::*;

/// Linux x86-64 syscall numbers that liblx answers.
#[allow(missing_docs)]
mod nr {
    pub const READ: i64 = 0;
    pub const WRITE: i64 = 1;
    pub const OPEN: i64 = 2;
    pub const CLOSE: i64 = 3;
    pub const STAT: i64 = 4;
    pub const FSTAT: i64 = 5;
    pub const LSTAT: i64 = 6;
    pub const POLL: i64 = 7;
    pub const LSEEK: i64 = 8;
    pub const MMAP: i64 = 9;
    pub const MPROTECT: i64 = 10;
    pub const MUNMAP: i64 = 11;
    pub const BRK: i64 = 12;
    pub const RT_SIGACTION: i64 = 13;
    pub const RT_SIGPROCMASK: i64 = 14;
    pub const IOCTL: i64 = 16;
    pub const PREAD64: i64 = 17;
    pub const PWRITE64: i64 = 18;
    pub const READV: i64 = 19;
    pub const WRITEV: i64 = 20;
    pub const ACCESS: i64 = 21;
    pub const SCHED_YIELD: i64 = 24;
    pub const MREMAP: i64 = 25;
    pub const MADVISE: i64 = 28;
    pub const DUP: i64 = 32;
    pub const DUP2: i64 = 33;
    pub const NANOSLEEP: i64 = 35;
    pub const GETPID: i64 = 39;
    pub const CLONE: i64 = 56;
    pub const FORK: i64 = 57;
    pub const VFORK: i64 = 58;
    pub const EXECVE: i64 = 59;
    pub const EXIT: i64 = 60;
    pub const WAIT4: i64 = 61;
    pub const KILL: i64 = 62;
    pub const UNAME: i64 = 63;
    pub const FCNTL: i64 = 72;
    pub const FSYNC: i64 = 74;
    pub const FTRUNCATE: i64 = 77;
    pub const GETCWD: i64 = 79;
    pub const CHDIR: i64 = 80;
    pub const RENAME: i64 = 82;
    pub const MKDIR: i64 = 83;
    pub const RMDIR: i64 = 84;
    pub const UNLINK: i64 = 87;
    pub const READLINK: i64 = 89;
    pub const UMASK: i64 = 95;
    pub const GETTIMEOFDAY: i64 = 96;
    pub const GETRLIMIT: i64 = 97;
    pub const GETUID: i64 = 102;
    pub const GETGID: i64 = 104;
    pub const SETUID: i64 = 105;
    pub const SETGID: i64 = 106;
    pub const GETEUID: i64 = 107;
    pub const GETEGID: i64 = 108;
    pub const GETPPID: i64 = 110;
    pub const SIGALTSTACK: i64 = 131;
    pub const ARCH_PRCTL: i64 = 158;
    pub const GETTID: i64 = 186;
    pub const TIME: i64 = 201;
    pub const FUTEX: i64 = 202;
    pub const GETDENTS64: i64 = 217;
    pub const SET_TID_ADDRESS: i64 = 218;
    pub const CLOCK_GETTIME: i64 = 228;
    pub const CLOCK_GETRES: i64 = 229;
    pub const CLOCK_NANOSLEEP: i64 = 230;
    pub const EXIT_GROUP: i64 = 231;
    pub const TGKILL: i64 = 234;
    pub const OPENAT: i64 = 257;
    pub const MKDIRAT: i64 = 258;
    pub const NEWFSTATAT: i64 = 262;
    pub const UNLINKAT: i64 = 263;
    pub const RENAMEAT: i64 = 264;
    pub const READLINKAT: i64 = 267;
    pub const FACCESSAT: i64 = 269;
    pub const PPOLL: i64 = 271;
    pub const SET_ROBUST_LIST: i64 = 273;
    pub const DUP3: i64 = 292;
    pub const PIPE2: i64 = 293;
    pub const PRLIMIT64: i64 = 302;
    pub const GETRANDOM: i64 = 318;
    pub const STATX: i64 = 332;
    pub const RSEQ: i64 = 334;
    pub const FACCESSAT2: i64 = 439;
}

/// The one entry point musl calls instead of executing `syscall`.
///
/// Returns what the Linux kernel would: a non-negative result, or `-errno`.
/// musl's `__syscall_ret` turns the latter into `errno` and `-1`.
///
/// # Safety
/// Called by musl with the arguments of the Linux syscall `n`. Pointer
/// arguments are trusted as far as the Linux contract makes them valid; a
/// wrong one faults this process, as it would on Linux.
#[no_mangle]
pub unsafe extern "C" fn __lx_syscall(n: i64, a: i64, b: i64, c: i64, d: i64, e: i64, f: i64) -> i64 {
    // SAFETY: forwarded contract.
    unsafe { dispatch(n, a, b, c, d, e, f) }
}

unsafe fn dispatch(n: i64, a: i64, b: i64, c: i64, d: i64, e: i64, f: i64) -> i64 {
    // SAFETY (for every arm): the pointer arguments come from musl with the
    // meaning the Linux ABI gives them; see `__lx_syscall`.
    unsafe {
        match n {
            nr::READ => fs::read(a as i32, b as *mut u8, c as usize),
            nr::WRITE => fs::write(a as i32, b as *const u8, c as usize),
            nr::PREAD64 => fs::pread(a as i32, b as *mut u8, c as usize, d as u64),
            nr::PWRITE64 => fs::pwrite(a as i32, b as *const u8, c as usize, d as u64),
            nr::READV => fs::readv(a as i32, b as *const fs::IoVec, c as usize),
            nr::WRITEV => fs::writev(a as i32, b as *const fs::IoVec, c as usize),
            nr::OPEN => fs::openat(AT_FDCWD, a as *const u8, b as i32),
            nr::OPENAT => fs::openat(a as i32, b as *const u8, c as i32),
            nr::CLOSE => fs::close(a as i32),
            nr::LSEEK => fs::lseek(a as i32, b, c as i32),
            nr::STAT | nr::LSTAT => fs::fstatat(AT_FDCWD, a as *const u8, b as *mut u8, 0),
            nr::FSTAT => fs::fstat(a as i32, b as *mut u8),
            nr::NEWFSTATAT => fs::fstatat(a as i32, b as *const u8, c as *mut u8, d as i32),
            nr::STATX => -ENOSYS, // musl falls back to newfstatat
            nr::ACCESS => fs::faccessat(AT_FDCWD, a as *const u8),
            nr::FACCESSAT | nr::FACCESSAT2 => fs::faccessat(a as i32, b as *const u8),
            nr::GETDENTS64 => fs::getdents64(a as i32, b as *mut u8, c as usize),
            nr::IOCTL => fs::ioctl(a as i32, b as u64, c as *mut u8),
            nr::FCNTL => fs::fcntl(a as i32, b as i32, c),
            nr::DUP => fs::dup(a as i32, 0),
            nr::DUP2 => fs::dup2(a as i32, b as i32),
            nr::DUP3 => fs::dup2(a as i32, b as i32),
            nr::FTRUNCATE => fs::ftruncate(a as i32, b as u64),
            nr::FSYNC => 0,
            nr::GETCWD => fs::getcwd(a as *mut u8, b as usize),
            nr::CHDIR => fs::chdir(a as *const u8),
            nr::MKDIR => fs::mkdirat(AT_FDCWD, a as *const u8),
            nr::MKDIRAT => fs::mkdirat(a as i32, b as *const u8),
            nr::UNLINK | nr::RMDIR => fs::unlinkat(AT_FDCWD, a as *const u8),
            nr::UNLINKAT => fs::unlinkat(a as i32, b as *const u8),
            nr::RENAME => fs::renameat(AT_FDCWD, a as *const u8, AT_FDCWD, b as *const u8),
            nr::RENAMEAT => fs::renameat(a as i32, b as *const u8, c as i32, d as *const u8),
            // FAT has no symbolic links, so nothing is one.
            nr::READLINK | nr::READLINKAT => -EINVAL,
            nr::POLL => fs::poll(a as *mut fs::PollFd, b as usize),
            nr::PPOLL => fs::poll(a as *mut fs::PollFd, b as usize),
            nr::UMASK => 0o022,
            nr::PIPE2 => -ENOSYS,

            nr::MMAP => mm::mmap(a as u64, b as u64, c as i32, d as i32, e as i32, f as u64),
            nr::MUNMAP => mm::munmap(a as u64, b as u64),
            nr::MREMAP => -ENOMEM, // mallocng falls back to malloc + copy
            nr::BRK => 0,          // and to mmap, permanently, on the first refusal
            nr::MPROTECT | nr::MADVISE => 0,

            nr::ARCH_PRCTL => arch_prctl(a as i32, b as u64),
            nr::SET_TID_ADDRESS => TID,
            nr::SET_ROBUST_LIST | nr::RSEQ => -ENOSYS,
            nr::FUTEX => -ENOSYS,
            nr::GETTID => TID,
            nr::GETPID => PID,
            nr::GETPPID => 1,
            // There are no users: authority is the handles a process holds.
            // Reporting root satisfies programs that check for it.
            nr::GETUID | nr::GETEUID | nr::GETGID | nr::GETEGID => 0,
            nr::SETUID | nr::SETGID => 0,
            nr::UNAME => uname(a as *mut u8),
            nr::GETRLIMIT | nr::PRLIMIT64 => -ENOSYS,
            nr::GETRANDOM => getrandom(a as *mut u8, b as usize),

            nr::CLOCK_GETTIME => clock_gettime(a as i32, b as *mut i64),
            nr::CLOCK_GETRES => clock_getres(b as *mut i64),
            nr::GETTIMEOFDAY => gettimeofday(a as *mut i64),
            nr::TIME => time(a as *mut i64),
            nr::NANOSLEEP => nanosleep(a as *const i64),
            nr::CLOCK_NANOSLEEP => clock_nanosleep(b as i32, c as *const i64),
            nr::SCHED_YIELD => {
                libnode::yield_now();
                0
            }

            // Signals are not delivered (docs/MUSL.md, phase 5); recording
            // the disposition and the mask is all a single-threaded program
            // that never receives one can observe.
            nr::RT_SIGACTION => rt_sigaction(b as *const u8, c as *mut u8),
            nr::RT_SIGPROCMASK => rt_sigprocmask(c as *mut u64),
            nr::SIGALTSTACK => 0,
            nr::KILL | nr::TGKILL => kill(b as i32),

            nr::CLONE | nr::FORK | nr::VFORK | nr::EXECVE | nr::WAIT4 => -ENOSYS,

            nr::EXIT | nr::EXIT_GROUP => exit(a as i32),

            _ => unimplemented(n),
        }
    }
}

/// Value of `AT_FDCWD`.
pub(crate) const AT_FDCWD: i32 = -100;

/// What `getpid` and `gettid` report. A process does not know its own koid
/// unless the launcher sent its process handle, and the shell does not; any
/// constant other than 0 and 1 serves a single-threaded program.
const PID: i64 = 2;
const TID: i64 = 2;

fn unimplemented(n: i64) -> i64 {
    // Report each missing syscall once: the kernel log then reads as the
    // to-do list for the next program to port.
    static SEEN: [AtomicU64; 8] = [const { AtomicU64::new(0) }; 8];
    if (0..512).contains(&n) {
        let word = &SEEN[n as usize / 64];
        let bit = 1u64 << (n % 64);
        if word.fetch_or(bit, Ordering::Relaxed) & bit == 0 {
            libnode::println!("lx: unimplemented Linux syscall {n}");
        }
    }
    -ENOSYS
}

// ---------------------------------------------------------------------------
// Threads and process
// ---------------------------------------------------------------------------

const ARCH_SET_FS: i32 = 0x1002;
const ARCH_GET_FS: i32 = 0x1003;

static FS_BASE: AtomicU64 = AtomicU64::new(0);

unsafe fn arch_prctl(code: i32, value: u64) -> i64 {
    match code {
        ARCH_SET_FS => {
            // SAFETY: both arguments are scalars; the kernel validates the
            // value. The invalid handle (0) means the calling thread.
            let raw = unsafe { libnode::sys::syscall2(Syscall::ThreadSetFsBase, 0, value) };
            match libnode::sys::ok(raw) {
                Ok(()) => {
                    FS_BASE.store(value, Ordering::Relaxed);
                    0
                }
                Err(error) => -errno::from_error(error),
            }
        }
        ARCH_GET_FS => {
            // SAFETY: the Linux contract: `value` points at a u64.
            unsafe { (value as *mut u64).write(FS_BASE.load(Ordering::Relaxed)) };
            0
        }
        _ => -EINVAL,
    }
}

fn exit(code: i32) -> ! {
    fs::flush_mirrors();
    libnode::println!("lx: exited with {code}");
    libnode::exit(code)
}

fn kill(signal: i32) -> i64 {
    // The only process a program can name is itself, and the only signals
    // whose effect can be honoured without delivery are the fatal defaults.
    match signal {
        0 => 0,
        _ => exit(128 + signal),
    }
}

unsafe fn uname(out: *mut u8) -> i64 {
    // `struct utsname`: six fields of 65 bytes.
    const FIELD: usize = 65;
    let fields: [&[u8]; 6] = [b"NodeOS", b"nodeos", b"0.1-lx", b"musl", b"x86_64", b""];
    for (index, value) in fields.iter().enumerate() {
        // SAFETY: the Linux contract: `out` points at a 390-byte utsname.
        unsafe {
            let field = out.add(index * FIELD);
            core::ptr::write_bytes(field, 0, FIELD);
            core::ptr::copy_nonoverlapping(value.as_ptr(), field, value.len());
        }
    }
    0
}

/// A xorshift generator seeded from the clock. NodeOS has no entropy source
/// yet (docs/MUSL.md, K8), so this is honest only about being unpredictable to
/// a casual observer -- enough for hash seeds and `AT_RANDOM`, not for keys.
pub(crate) fn random_u64() -> u64 {
    static STATE: AtomicU64 = AtomicU64::new(0);
    let mut x = STATE.load(Ordering::Relaxed);
    if x == 0 {
        x = libnode::clock_ns() ^ 0x9E37_79B9_7F4A_7C15;
    }
    x ^= x << 13;
    x ^= x >> 7;
    x ^= x << 17;
    STATE.store(x, Ordering::Relaxed);
    x
}

unsafe fn getrandom(out: *mut u8, len: usize) -> i64 {
    for index in 0..len {
        // SAFETY: the Linux contract: `out` has `len` writable bytes.
        unsafe { out.add(index).write(random_u64() as u8) };
    }
    len as i64
}

// ---------------------------------------------------------------------------
// Signals (recorded, never delivered)
// ---------------------------------------------------------------------------

unsafe fn rt_sigaction(_new: *const u8, old: *mut u8) -> i64 {
    if !old.is_null() {
        // `struct k_sigaction`: handler, flags, restorer, mask. All zero is
        // SIG_DFL with nothing blocked, which is the truth.
        // SAFETY: the Linux contract: `old` has room for one k_sigaction.
        unsafe { core::ptr::write_bytes(old, 0, 32) };
    }
    0
}

unsafe fn rt_sigprocmask(old: *mut u64) -> i64 {
    if !old.is_null() {
        // SAFETY: the Linux contract: `old` points at a sigset.
        unsafe { old.write(0) };
    }
    0
}

// ---------------------------------------------------------------------------
// Time
// ---------------------------------------------------------------------------

const CLOCK_REALTIME: i32 = 0;

/// Nanoseconds on `clock`. NodeOS's only clock is monotonic since boot, and
/// there is no wall-clock source yet (docs/MUSL.md, K8), so every Linux clock
/// reads it: the realtime clock starts at the epoch at boot.
fn now_ns(_clock: i32) -> u64 {
    libnode::clock_ns()
}

unsafe fn clock_gettime(clock: i32, out: *mut i64) -> i64 {
    let ns = now_ns(clock);
    // SAFETY: the Linux contract: `out` points at a timespec.
    unsafe {
        out.write((ns / 1_000_000_000) as i64);
        out.add(1).write((ns % 1_000_000_000) as i64);
    }
    0
}

unsafe fn clock_getres(out: *mut i64) -> i64 {
    if !out.is_null() {
        // SAFETY: as above.
        unsafe {
            out.write(0);
            out.add(1).write(1);
        }
    }
    0
}

unsafe fn gettimeofday(out: *mut i64) -> i64 {
    if !out.is_null() {
        let ns = now_ns(CLOCK_REALTIME);
        // SAFETY: the Linux contract: `out` points at a timeval.
        unsafe {
            out.write((ns / 1_000_000_000) as i64);
            out.add(1).write(((ns % 1_000_000_000) / 1000) as i64);
        }
    }
    0
}

unsafe fn time(out: *mut i64) -> i64 {
    let seconds = (now_ns(CLOCK_REALTIME) / 1_000_000_000) as i64;
    if !out.is_null() {
        // SAFETY: the Linux contract: `out` points at a time_t.
        unsafe { out.write(seconds) };
    }
    seconds
}

unsafe fn nanosleep(request: *const i64) -> i64 {
    // SAFETY: the Linux contract: `request` points at a timespec.
    let ns = unsafe { request.read() as u64 * 1_000_000_000 + request.add(1).read() as u64 };
    sleep_until(libnode::clock_ns().saturating_add(ns));
    0
}

unsafe fn clock_nanosleep(flags: i32, request: *const i64) -> i64 {
    const TIMER_ABSTIME: i32 = 1;
    // SAFETY: as above.
    let ns = unsafe { request.read() as u64 * 1_000_000_000 + request.add(1).read() as u64 };
    let deadline = if flags & TIMER_ABSTIME != 0 { ns } else { libnode::clock_ns().saturating_add(ns) };
    sleep_until(deadline);
    0
}

fn sleep_until(deadline: u64) {
    // `wait` needs an object; there are no timer objects yet, so yield until
    // the clock passes the deadline. Correct, if not free.
    while libnode::clock_ns() < deadline {
        libnode::yield_now();
    }
}
