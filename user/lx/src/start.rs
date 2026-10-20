//! From a NodeOS bootstrap message to a Linux process entry.
//!
//! The kernel starts every process at libnode's `_start` with one handle in
//! `RDI`; libnode receives the bootstrap message and calls `__libnode_main`,
//! which liblx defines here. musl expects what Linux provides instead: `argc`,
//! then `argv[]`, `envp[]` and the auxiliary vector laid out back to back.
//! This module builds that block and hands it to `__libc_start_main`, which
//! sets up TLS, runs constructors and calls the program's `main`.

use alloc::vec::Vec;

use libnode::{Env, Handle};

extern "C" {
    /// The C program's entry point.
    fn main(argc: i32, argv: *mut *mut u8, envp: *mut *mut u8) -> i32;

    /// musl's process startup. Never returns: it calls `exit(main(...))`.
    fn __libc_start_main(
        main: unsafe extern "C" fn(i32, *mut *mut u8, *mut *mut u8) -> i32,
        argc: i32,
        argv: *mut *mut u8,
        init: usize,
        fini: usize,
        ldso: usize,
    ) -> i32;

    /// The ELF header, mapped at the start of the text segment by `lx.ld`.
    static __ehdr_start: [u8; 64];
}

/// The environment every program starts with. NodeOS has no ambient one to
/// inherit, and the shell does not pass one yet.
const ENVIRONMENT: &[&str] = &["PATH=/bin", "HOME=/", "TERM=dumb", "LANG=C"];

// Auxiliary vector keys musl reads.
const AT_NULL: usize = 0;
const AT_PHDR: usize = 3;
const AT_PHENT: usize = 4;
const AT_PHNUM: usize = 5;
const AT_PAGESZ: usize = 6;
const AT_ENTRY: usize = 9;
const AT_UID: usize = 11;
const AT_EUID: usize = 12;
const AT_GID: usize = 13;
const AT_EGID: usize = 14;
const AT_HWCAP: usize = 16;
const AT_CLKTCK: usize = 17;
const AT_SECURE: usize = 23;
const AT_RANDOM: usize = 25;
const AT_EXECFN: usize = 31;

libnode::main! {
    fn lx_main(env: Env) -> i32 {
        start(env)
    }
}

fn start(mut env: Env) -> i32 {
    // The shell's session order: console (0), filesystem (1), then whatever
    // else it had. Keep every handle for the life of the process.
    let mut session: Vec<Handle> = Vec::new();
    while let Some(handle) = env.take_extra(0) {
        session.push(handle);
    }
    crate::fs::init(session.first().map(|h| h.raw().0), session.get(1).map(|h| h.raw().0), env.var("PWD"));
    core::mem::forget(session);

    // NUL-terminated copies of the arguments and environment, which musl and
    // the program will hold pointers into forever.
    let mut strings: Vec<*mut u8> = Vec::new();
    for arg in env.args() {
        strings.push(leak_c_string(arg.as_bytes()));
    }
    if strings.is_empty() {
        strings.push(leak_c_string(b"a.out"));
    }
    let argc = strings.len();
    // The launcher's environment when it sent one (the shell does), and a
    // minimal default otherwise.
    let mut environment: Vec<*mut u8> = if env.vars().is_empty() {
        ENVIRONMENT.iter().map(|var| leak_c_string(var.as_bytes())).collect()
    } else {
        env.vars().iter().map(|var| leak_c_string(var.as_bytes())).collect()
    };

    // AT_RANDOM: sixteen bytes musl takes its stack-protector canary from.
    let random = Vec::leak(alloc::vec![0u8; 16]);
    for byte in random.iter_mut() {
        *byte = crate::random_u64() as u8;
    }

    // SAFETY: `lx.ld` maps the ELF header at `__ehdr_start`; the program
    // header table follows it in the same segment.
    let (phdr, phent, phnum, entry) = unsafe {
        let header = core::ptr::addr_of!(__ehdr_start) as *const u8;
        let entry = (header.add(24) as *const u64).read_unaligned();
        let phoff = (header.add(32) as *const u64).read_unaligned();
        let phent = (header.add(54) as *const u16).read_unaligned();
        let phnum = (header.add(56) as *const u16).read_unaligned();
        (header as usize + phoff as usize, phent as usize, phnum as usize, entry as usize)
    };

    let auxv: [(usize, usize); 14] = [
        (AT_PHDR, phdr),
        (AT_PHENT, phent),
        (AT_PHNUM, phnum),
        (AT_PAGESZ, 4096),
        (AT_ENTRY, entry),
        (AT_HWCAP, 0),
        (AT_CLKTCK, 100),
        // Present and equal, so musl skips the "are fds 0-2 open?" check it
        // makes for setuid programs.
        (AT_UID, 0),
        (AT_EUID, 0),
        (AT_GID, 0),
        (AT_EGID, 0),
        (AT_SECURE, 0),
        (AT_RANDOM, random.as_ptr() as usize),
        (AT_EXECFN, strings[0] as usize),
    ];

    // argv[], NULL, envp[], NULL, auxv pairs, AT_NULL: the block musl walks.
    let mut block: Vec<usize> = Vec::new();
    block.extend(strings.iter().map(|p| *p as usize));
    block.push(0);
    block.extend(environment.drain(..).map(|p| p as usize));
    block.push(0);
    for (key, value) in auxv {
        block.push(key);
        block.push(value);
    }
    block.push(AT_NULL);
    block.push(0);
    let argv = Vec::leak(block).as_mut_ptr() as *mut *mut u8;

    // SAFETY: `argv` heads a well-formed Linux startup block that lives
    // forever, and `main` is the C program's entry point.
    unsafe { __libc_start_main(main, argc as i32, argv, 0, 0, 0) }
}

fn leak_c_string(bytes: &[u8]) -> *mut u8 {
    let mut owned = Vec::with_capacity(bytes.len() + 1);
    owned.extend_from_slice(bytes);
    owned.push(0);
    Vec::leak(owned).as_mut_ptr()
}
