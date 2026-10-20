# musl on NodeOS

The goal is that most Linux programs build and run on NodeOS without changes to
their source: `./configure && make` against a NodeOS toolchain, then `run` the
result from the shell. This document is the plan for getting there. It covers
what the work involves, the order to do it in, and which parts of NodeOS have to
change, and why each change is still a mechanism rather than a return to Unix.

## Status

Phase 1, plus the parts of phase 2 that a program without `fork` needs, is
implemented. **Lua 5.4.7, cloned from github.com/lua/lua and compiled without
changes, runs on NodeOS**, both from a script on the disk and as an interactive
REPL at the console. `make test-lua` builds it and checks it in QEMU.

| Piece | Where | State |
| --- | --- | --- |
| K1: per-thread `FS_BASE`, `thread_set_fs_base` (syscall 32) | `kernel/src/task/thread.rs`, `sched/mod.rs`, `syscall/mod.rs` | done |
| musl 1.2.6 with the seven-file overlay, pinned by hash | `user/lx/musl/`, `scripts/build-musl.sh` | done: 0 raw `syscall`s in `libc.a` (upstream has 512) |
| liblx: startup, ~80 Linux syscalls | `user/lx/src/` | done for single-threaded programs |
| Compiler driver and link layout | `scripts/lx-cc`, `user/lx/lx.ld` | done |
| Raw-syscall check on every linked program | `scripts/build-lx-apps.sh` | done |
| Shell: a program runs in the foreground, so it owns the console until it exits | `user/shell/src/main.rs` | done (in the shell itself) |
| K2 futex, threads | | not started: `clone` returns `ENOSYS` |
| K3-K7: process lifecycle, `fork`/`exec`, signals | | not started |
| K8: wall clock and entropy | | not started: realtime starts at the epoch at boot, `getrandom` is xorshift |

What liblx answers today: `read`, `write`, `readv`, `writev`, `pread64` and
`pwrite64`; `open`, `openat`, `close`, `lseek` and `ftruncate`; `stat`, `fstat`,
`lstat` and `newfstatat`; `access` and `faccessat`; `getdents64`, `getcwd`,
`chdir`, `mkdir`, `unlink`, `rmdir` and `rename` (done by copying);
`dup`, `dup2`, `dup3`, `fcntl`, `ioctl` (`TCGETS`, `TIOCGWINSZ`) and `poll`;
`mmap` and `munmap`; `arch_prctl`, `set_tid_address`, the clocks,
`nanosleep`, `uname`, `getrandom`, `exit` and `exit_group`; and stubs
that tell the truth for identity, signal masks and `brk`. Anything else
returns `ENOSYS` and is logged once as `lx: unimplemented Linux syscall N`,
so the kernel log of a program that fails is the to-do list for porting it.

---

## 1. The one decision everything else follows from

**Keep musl as it is, Linux syscall numbers and structure layouts included,
and replace the single instruction it uses to reach a kernel.**

musl is written so that this works. Its ~1,800 C and assembly sources reach
the kernel through the `__syscallN(n, a, b, ...)` inlines in
`arch/x86_64/syscall_arch.h`. Measured against musl 1.2.6, exactly **seven**
files contain a raw `syscall` instruction:

| File | What it does |
| --- | --- |
| `arch/x86_64/syscall_arch.h` | `__syscall0` to `__syscall6`, used by everything else |
| `src/thread/x86_64/__set_thread_area.s` | `arch_prctl(ARCH_SET_FS)`, which sets the TLS pointer |
| `src/thread/x86_64/clone.s` | thread creation: the child returns on a new stack |
| `src/thread/x86_64/__unmapself.s` | a detached thread frees its own stack and exits |
| `src/thread/x86_64/syscall_cp.s` | cancellable syscalls (pthread cancellation points) |
| `src/process/x86_64/vfork.s` | `vfork` |
| `src/signal/x86_64/restore.s` | `__restore_rt`, the signal-return trampoline |

The dynamic linker (`ldso/dynlink.c`) goes through the same `__syscall` macros,
so it needs nothing extra when §6 comes around.

Change those seven and route every call to one function:

```c
/* arch/x86_64/syscall_arch.h, NodeOS edition */
long __lx_syscall(long n, long a, long b, long c, long d, long e, long f);

static __inline long __syscall3(long n, long a1, long a2, long a3)
{
	return __lx_syscall(n, a1, a2, a3, 0, 0, 0);
}
```

`__lx_syscall` is **liblx**: a Linux personality, linked into every program
and running in that program's own address space, that answers Linux syscalls
using NodeOS handles. musl keeps calling `SYS_openat` with a `struct stat`
laid out the way Linux lays it out. liblx turns that into an `FS_OPEN` message
on the filesystem channel.

```
     ordinary C program        unchanged source
            |
          musl                 upstream, with 7 files patched
            |  __lx_syscall(SYS_openat, ...)
          liblx                Linux personality, in-process, Rust, on libnode
            |  channel_call(fs, FS_OPEN, ...)
     NodeOS syscalls           31 of them, unchanged numbering
            |
   kernel  /  fs, console, compositor, audio services
```

### Why this shape and not another

* **Using Linux syscall numbers inside the process keeps the patch to musl
  small.** The alternative is to give musl a NodeOS backend function by
  function: `open` calls the filesystem, `pthread_create` calls
  `thread_create`, and so on. Fuchsia started that way with a musl fork and
  ended up maintaining a different libc. Keeping Linux's syscall vocabulary as
  an *internal* interface between two layers of the same process costs nothing
  at runtime (it is one function call either way) and makes upgrading musl a
  rebase of seven files.
* **Keeping Linux structure layouts in the headers keeps programs unmodified.**
  `struct stat`, `struct termios`, `sigset_t` and `ioctl` numbers are what
  autoconf probes and what programs hard-code. If they differ from Linux,
  something somewhere breaks.
* **Running the translation in-process fits NodeOS.** liblx holds only the
  handles the process was given. It is a library, not a server, so it has no
  more authority than the program it is linked into. "What can this Linux
  program do?" still has the same answer: the bootstrap message.
* **There is precedent.** seL4's musl port works the same way: `muslcsys`
  installs a table that the patched `__syscall` jumps through, and in-process
  code implements the Linux calls. Genode runs a POSIX libc on a capability
  kernel with all process state kept in the library, including a userspace
  `fork`. Managarm keeps that state in a separate POSIX server instead, which
  is the other option and is discussed in §4.

### Why musl and not mlibc or relibc

mlibc is designed to be ported (its `sysdeps` layer is the abstraction this
plan builds by hand) and relibc is Rust. Both are less compatible with existing
programs. musl is what Alpine Linux is built on, so it is the libc that the
largest body of real software already builds against unchanged. §6 explains
why that matters beyond source compatibility.

---

## 2. What "most Linux programs" actually needs

Guessing which syscalls matter is easy to get wrong, so the list below is
measured. `tools/syscall-census/census.sh` builds musl 1.2.6 and a static
busybox 1.37 on the host, runs a shell workload under a small ptrace tracer
(pipes, redirection, `sort | uniq | wc`, `find`, `sed`, `awk`, `mkdir`/`mv`/`rm`,
`$(...)`, `&`/`wait`, `trap` plus `kill -INT $$`) and a 4-thread mutex test, and
counts the calls:

```
busybox sh workload: 43 distinct syscalls
  rt_sigprocmask(183) mmap(79) close(60) munmap(57) stat(30) set_tid_address(30)
  wait4(28) read(22) open(21) rt_sigaction(21) fcntl(19) brk(18) dup2(17)
  rt_sigreturn(16) exit_group(16) fork(15) getuid(15) getgid(15) setuid(15)
  setgid(15) arch_prctl(15) execve(14) writev(12) getdents64(12) lstat(11)
  ioctl(11) write(7) pipe(4) fstat(2) lseek(2) getpid(2) sendfile(2) mkdir(2)
  rmdir(2) umask(2) nanosleep(1) kill(1) uname(1) getcwd(1) rename(1) unlink(1)
  getppid(1) rt_sigsuspend(1)

pthread test: 12 distinct syscalls
  futex(17333) rt_sigprocmask(17) mmap(4) mprotect(4) munmap(4) clone(4) exit(4)
  ioctl(1) writev(1) arch_prctl(1) set_tid_address(1) exit_group(1)
```

So **about 50 syscalls run a complete Unix userland**, not the 341 distinct
`SYS_` names musl mentions. They fall into five groups, and each group asks
NodeOS for something different:

| Group | Calls | Can liblx do it alone? |
| --- | --- | --- |
| Memory | `mmap munmap mprotect brk` | mostly; see K5 |
| Files and stdio | `open read write writev close stat lstat fstat lseek getdents64 fcntl dup2 pipe mkdir rmdir rename unlink sendfile ioctl umask getcwd` | yes, over channels; filesystem protocol gaps below |
| Threads | `clone futex set_tid_address arch_prctl exit` | **no**: needs TLS and futex in the kernel (K1, K2) |
| Processes | `fork execve wait4 exit_group getpid getppid kill` | partly; `fork` needs K6 |
| Signals | `rt_sigaction rt_sigprocmask rt_sigreturn rt_sigsuspend` | cooperative delivery yes; asynchronous delivery needs K7 |
| Identity | `getuid getgid setuid setgid uname` | yes, as constants |

The rest of this document works through those groups in that order. It is also
the build order.

Re-run the census with any program you want to support. Its output is the
to-do list for liblx.

---

## 3. What the kernel has to provide

NodeOS today has no way to set a per-thread TLS pointer and no futex, and a
process cannot read a child's exit code. Without the first two, musl's
pthreads cannot work at all. The list below is everything the kernel needs to
gain. Each item is a mechanism with no policy in it, and it is listed with the
phase that first needs it.

**K1. Per-thread `FS_BASE` (phase 1).** musl reads `%fs:0` on every
`pthread_self`, `errno` access and stack-protector check. `kernel/src/arch/
x86_64/cpu.rs` defines `MSR_FS_BASE` but nothing writes it for user threads,
and the context switch does not save it. The change has three parts: a
`fs_base` field in the thread, saved and restored on every switch; a
`thread_set_fs_base(thread, value)` syscall, where the invalid handle means
the calling thread (the same convention `resolve_space` uses); and optionally
`CR4.FSGSBASE`, so that `wrfsbase` works from ring 3 without a syscall. The
FSGSBASE route is faster but exposes `wrgsbase` as well, and the entry stub's
`swapgs` must then never trust a user-controlled `GS_BASE`. The current stub
already loads its own value, but this needs an audit before turning it on.

**K2. Futex (phase 1).** `futex_wait(addr, expected, deadline)` and
`futex_wake(addr, count)`. The census pthread test makes 17,000 futex calls, so
emulating it with `yield` is not an option. Key the wait queue by
`(VMO koid, offset)` rather than by virtual address, so that a futex in a
shared memory object works across processes; Zircon keys it the same way.
`wait` with a deadline currently polls (ARCHITECTURE.md §9). A futex that
sleeps properly is a good excuse to make `Timer` a real object.

**K3. Process lifecycle (phase 3).** The kernel records `exit_code` in
`task/process.rs`, but only `launch.rs` reads it, so a parent cannot learn how
its child exited. Add `process_info(handle) -> {exit_code, state}` (or an
`exit_code` field in `HandleInfo`). Also add `process_kill(handle)`, which
needs `MANAGE_PROCESS` and is what `SIGKILL` becomes. There is currently no way
to end another process at all.

**K4. A child's address space (phase 3).** Only the boot launcher can install
an `AddressSpace` handle (`launch.rs:401`). A userspace ELF loader and a
userspace `fork` both need to map memory into a child they just created. Have
`process_create` also return the child's space handle, or add
`process_get_space(handle)`. This adds no authority: anyone holding
`MANAGE_PROCESS` on a child can already load arbitrary code into it.

**K5. Range unmap and protect, widening up to the handle's rights (phase 1
workaround, phase 2 fix).** `AddressSpace::unmap(base)` removes a whole region
by its base address, and `protect` only narrows. Linux programs `munmap` the
middle of a mapping, and they `mprotect` a `PROT_NONE` reservation up to
`PROT_READ|PROT_WRITE` (thread stacks with guard pages, allocators that
reserve address space first). Widening is not an escalation if the backing
handle already carries the right. The rule that matters is "never above what
the handle allows", which `MapFlags::required_rights` already enforces at map
time. liblx can work around both limits: it keeps the VMO handle for every
mapping it makes, so it can unmap the region and remap the pieces it wants to
keep. That is good enough to start with, but slow for allocators, so the
kernel should take over later.

**K6. Snapshot child of a memory object (phase 4).** `vmo_create_child(vmo,
SNAPSHOT)` returns a copy-on-write clone in which later writes from either side
stay private. The frame database already reference-counts frames and the fault
handler already does copy-on-write, so this is mostly bookkeeping. This one
primitive is what lets `fork` be implemented in userspace (§4.3) and keeps
"processes are built, not cloned" true at the kernel level.

**K7. Interrupting a thread (phase 5).** `thread_interrupt(thread, upcall_pc)`
saves the target's user register state where its own runtime can find it and
resumes it at `upcall_pc`. If the thread is blocked, its syscall returns
`Interrupted` first. This is the only way to deliver a signal to a thread that
is in a compute loop. The same path can deliver synchronous faults (`SIGSEGV`,
`SIGFPE`) by reflecting them from the existing user fault hook in
`exception.rs` to the faulting thread instead of to a supervisor.

**K8. Wall-clock time and entropy (phase 1).** `FsStat` timestamps are
"nanoseconds since boot", `clock` is monotonic only, and there is no RNG. musl
needs `CLOCK_REALTIME` and fills `AT_RANDOM` (the stack protector canary and
malloc's secrets come from it). Read the CMOS RTC once at boot, publish the
offset, and add `RDRAND`/`RDSEED` with a TSC-jitter fallback for older CPUs.
Both could be services instead, but a kernel-provided boot-time offset is
simpler and contains no policy.

**K9. Syscall user dispatch (phase 7, optional).** See §6.

---

## 4. liblx, layer by layer

liblx is a `no_std` Rust static library built on `libnode` and linked after
`libc.a`. It is Rust because `libnode` is, because the protocols live in
`nodeos-abi` as Rust structs, and because the C side of the tree (Doom) already
links against Rust the same way.

### 4.0 Startup: from a bootstrap channel to `argc, argv, envp, auxv`

A NodeOS process starts with one handle in `RDI`. musl expects a Linux
initial stack. liblx bridges the two without a startup file of its own, by
reusing libnode's (`user/lx/src/start.rs`):

1. libnode's `_start` receives the bootstrap message, as it does for every
   NodeOS program, and calls `__libnode_main`, which liblx defines. The message
   carries the session channels (console, filesystem, then compositor and
   audio when present) and `argv`. Later it will also carry `envp`, the working
   directory and the inherited descriptor table (§4.2).
2. liblx builds the Linux block: `argv[]`, `envp[]` and an auxv with
   `AT_PHDR`/`AT_PHNUM`/`AT_PHENT`, `AT_PAGESZ`, `AT_ENTRY`, `AT_RANDOM` and
   `AT_UID`/`AT_EUID`/`AT_GID`/`AT_EGID`. The uid and gid entries matter: if
   they are missing, musl treats the program as possibly setuid and
   `poll`s descriptors 0-2 before `main`.
3. It calls musl's `__libc_start_main(main, argc, argv, ...)`, which sets up
   TLS (through `arch_prctl`, which becomes K1's syscall), runs constructors
   and calls `main`.

musl's `static_init_tls` finds the TLS template by walking the program headers
from `AT_PHDR`, but `nxfpack` keeps only `PT_LOAD`. The link script
(`user/lx/lx.ld`) solves this by mapping the ELF header and program headers
at the start of the text segment (`FILEHDR PHDRS`) and emitting a `PT_TLS`
header. liblx reads `__ehdr_start` and points `AT_PHDR` at
`__ehdr_start + e_phoff`. The loader does not need to know any of this.

### 4.1 Memory

* `mmap(MAP_ANONYMOUS)` becomes `memory_allocate` plus `memory_map` into the
  heap window. liblx records `(base, len, vmo handle, prot)` in a region table.
* `mmap` of a file becomes `FS_GET_VMO`, mapped copy-on-write for
  `MAP_PRIVATE` and directly for `MAP_SHARED`.
* `munmap` and `mprotect` use the region table, splitting by unmap and remap
  until K5 lands.
* `brk` returns the current break and refuses to grow it. musl's malloc then
  falls back to `mmap`, which it was designed to do.
* `MAP_FIXED` becomes a `memory_map` with a hint. The address-space windows
  are a feature here: a non-PIE program linked at `0x400000` lands in the
  program window exactly where `user.ld` already puts NodeOS programs.

**The red zone is fine.** NodeOS programs are built with `-mno-red-zone`, and
`user/doom/build.rs` explains why. That restriction protects *kernel* code.
An interrupt taken in ring 3 switches to `TSS.RSP0` before pushing anything, so
the user stack below `rsp` is never touched. musl and the programs built
against it should use the normal SysV ABI, red zone and SSE included (the
kernel saves FPU state on every entry from ring 3 now). The red zone matters
again only in phase 5, where a signal frame must be built more than 128 bytes
below the interrupted `rsp`, which is what Linux does.

### 4.2 Descriptors

liblx owns the descriptor table. Each slot holds an object:

| Kind | Backed by | Notes |
| --- | --- | --- |
| `Tty` | console channel | line discipline and termios live in liblx (below) |
| `File` | fs channel + node id + offset | the offset is client-side, as `FS_READ` takes one |
| `Dir` | fs channel + path + cursor | `getdents64` from `FS_LIST`, cached |
| `Pipe` | a channel pair carrying byte messages | readable and writable signals come from the kernel |
| `Null` | none | `/dev/null`, `/dev/zero` |

`poll`, `select` and `ppoll` become a single `wait` over the channel handles
behind the descriptors. File and null descriptors are always ready. When a
process is spawned, liblx serializes the descriptor table into the bootstrap
message and duplicates the handles behind it into the child, which is how
`dup2(fd, 1); execve(...)` reaches the child. Two processes sharing one
filesystem channel is safe, because `Channel::call` matches each reply to its
caller by `txid` (`ipc/channel.rs:286`).

Gaps in the filesystem protocol that the census hits:

* **No rename.** The workload's `mv` calls `rename`. Add `FS_RENAME`.
  Emulating it with copy and unlink is wrong for directories and not atomic.
* **No mode, owner or wall-clock times.** Synthesize `0755` for directories and
  `0644` for files, owner 0, and times rebased by K8's boot offset. FAT has
  none of this anyway.
* **No symlinks.** `lstat` equals `stat`, and `readlink` returns `EINVAL`.
  That is honest on FAT.
* **Paths only.** `openat(dirfd, rel)` resolves in liblx against the
  directory's recorded path, and the working directory is a string in liblx.
  Every protocol call already takes a path, so this costs nothing.

**The terminal is the biggest user-visible gap.** The console speaks
`CONSOLE_WRITE_TEXT` and `CONSOLE_READ_KEY`. It does not interpret any escape
sequences (it has no ESC handling at all), and Linux TUI programs (`vi`,
`less`, `top`, anything that uses curses) assume a VT100/xterm. There are two
routes:

1. **Recommended:** move escape-sequence parsing into the console. The
   console is the terminal emulator, so it should parse CSI cursor movement,
   SGR colours and erase sequences, which is a few hundred lines. Then a byte
   stream written to a `Tty` descriptor is passed through without changes, and
   every program that already speaks VT100 works.
2. Translate escapes in liblx into `CONSOLE_SET_CURSOR` and similar calls.
   This puts terminal emulation in every process and is the wrong layer.

`termios` stays in liblx. In canonical mode liblx builds lines from
`CONSOLE_READ_KEY` events, with echo and erase. In raw mode it passes keys
through as bytes, with the arrow keys encoded as `ESC [ A` and so on. `ioctl`
handles `TCGETS`, `TCSETS*` and `TIOCGWINSZ` (from `CONSOLE_GET_INFO`) and
returns `ENOTTY` for anything else.

### 4.3 Processes

**Spawn first, fork later.** Most programs that start other programs do it
through `posix_spawn`, `system` or `popen`. musl implements all three with
`clone(CLONE_VM|CLONE_VFORK)` followed by `execve`, so they need no copy of
memory. liblx implements that pattern as **spawn-on-exec**:

1. `vfork` or `clone(CLONE_VFORK)` returns 0 in the current thread, on the
   caller's stack (or on the stack `clone` was given), and switches liblx into
   *child mode*.
2. In child mode, `dup2`, `close`, `chdir`, `setpgid` and `sigprocmask` change
   a *pending child*'s descriptor table and attributes, not the parent's.
3. `execve` loads the image, creates the process with `process_create`,
   `process_load` and `process_start`, hands over the pending state, leaves
   child mode, and resumes the vfork call site in the parent with the child's
   pid. `_exit` in child mode makes the "child" exit immediately with that
   status.

This is valid `vfork` semantics: the child borrows the parent's memory until
`execve`. It is also the whole design of busybox's NOMMU build, which uses
`vfork` exclusively. So the first shell to target is **busybox with
`CONFIG_NOMMU`**. It gives `sh` and about 300 applets before `fork` exists.

**Real `fork` (phase 4)** builds on K4 and K6, entirely in userspace:

1. Create the child process and obtain its address space (K4).
2. For every region in liblx's table, plus the image segments and the main
   stack (known from `LoadedImageInfo`), make a snapshot child of the backing
   VMO (K6) and map it into the child at the same address with the same
   protection.
3. Duplicate the session channels and every descriptor's handles.
4. Start the child's one thread at a `fork_return` trampoline with a copy of
   the caller's registers and `rax = 0`.

POSIX copies only the calling thread, which is all this does. The region table
has to be complete, so every mapping must go through liblx, including the
pthread stacks musl creates with `mmap`. That is the reason liblx, not musl,
must own `mmap`. This also gives a cheap test: after a fork, compare liblx's
table with the kernel's view of the address space, using a debug-only region
enumeration syscall.

**Process identity.** A pid is the process's koid. `getpid` comes from
`capability_query` on the process's own handle. `getppid` arrives in the
bootstrap message. `wait4` becomes `wait` on `PROCESS_TERMINATED` plus
`process_info` (K3), with children tracked in liblx. `kill(child, SIGKILL)`
becomes `process_kill`. A signal to anything other than one's own children, a
process group or the terminal's foreground job needs someone who knows the
whole process tree. That is the one place a **`proc` service** is needed: it
holds the pid table, process groups and sessions, gives each process a signal
channel, and is told by the console which group is in the foreground, so that
Ctrl+C becomes `SIGINT` to that group. Keep it small. The descriptor table,
memory and everything else stay in-process.

### 4.4 Threads

With K1 and K2:

* `clone(CLONE_VM|CLONE_THREAD|CLONE_SETTLS|...)` becomes `thread_create`,
  with an entry trampoline that sets `FS_BASE`, writes the tid to
  `CLONE_PARENT_SETTID`, and calls the child function on the new stack.
  liblx's version of `clone.s` is a small rewrite of musl's.
* `set_tid_address` and `CLONE_CHILD_CLEARTID` are recorded in liblx. Linux
  clears the tid word and wakes it from the kernel when a thread exits.
  liblx's `exit` does the same in userspace before calling `thread_exit`.
  `pthread_join` depends on this.
* `__unmapself` (a detached thread freeing its own stack) stays the two-instruction
  routine musl already has, using NodeOS syscalls: `memory_unmap` and then
  `thread_exit`, with the arguments in registers so that no stack is touched
  once it is gone. liblx drops the region from its table first.
* `futex` maps directly onto K2. Only `FUTEX_WAIT`, `FUTEX_WAKE` and their
  `_PRIVATE` variants are needed. `FUTEX_REQUEUE` can be emulated as a wake of
  all waiters, which is correct but has a thundering-herd cost.
* `sched_yield` becomes `yield`, and `sched_getaffinity` reports one CPU until
  SMP is on.

### 4.5 Signals

Delivery comes in two strengths.

**Cooperative (phase 3).** Each process has a signal channel (from `proc`, or
from the parent before `proc` exists). Every blocking call in liblx (`read`
on a tty or pipe, `wait4`, `poll`, `nanosleep`, `futex`) waits on its own
handle *and* the signal channel. When a signal arrives, liblx runs the handler
on the calling thread before the syscall returns, then returns `EINTR` or
restarts under `SA_RESTART`. Non-blocking calls check the channel without
waiting. Handlers get a real `siginfo_t` and a `ucontext_t` that describes the
syscall return point. That is enough for Ctrl+C at a prompt, `SIGCHLD`,
`SIGPIPE` (raised by liblx itself on `PEER_CLOSED`), `alarm` and `sleep`, and
busybox's `trap`, which the census workload exercises. `SIGKILL` and
`SIGSTOP` never reach liblx: `SIGKILL` is `process_kill`.

**Asynchronous (phase 5)** needs K7: a CPU-bound thread is interrupted, liblx
builds a Linux `rt_sigframe` below the red zone and runs the handler, and
`__restore_rt`'s `rt_sigreturn` restores the full register state.
Synchronous faults (`SIGSEGV`, `SIGBUS`, `SIGFPE`, `SIGILL`) use the same
path from the exception hook. Programs that catch `SIGSEGV` on purpose
(garbage collectors, some JITs) need this.

---

## 5. Toolchain

```
build/sysroot/      musl headers and libc.a       scripts/build-musl.sh
build/lx/liblx.a    liblx, libnode's mem* weak     make lx
scripts/lx-cc       host gcc, -nostdinc against the sysroot, -fno-pie;
                    links with lld and user/lx/lx.ld:
                    <objs> --start-group libc.a liblx.a --end-group libgcc.a
```

* musl is configured with `--disable-shared` and the host gcc. The seven
  replacement files live in `user/lx/musl/` and are copied over an unmodified
  upstream tarball pinned by hash, so moving to a new musl release means
  updating the hash and checking those files against upstream. The build fails
  if `libc.a` still contains a `syscall` instruction.
* libnode defines `memcpy`, `memset` and the rest of its `rt.rs` because Rust
  programs have no libc. A C program has one, so the copy of liblx it links
  has those symbols weakened (`objcopy --weaken-symbol`), and musl's
  optimised versions win.
* No `crt1.o`: liblx's startup (§4.0) is the entry point.
* There is no `-mno-red-zone` and no `-mgeneral-regs-only`. Lua's floating
  point, which uses SSE, works.
* Programs link at `0x400000` (gcc's default for non-PIE static links), and
  `nxfpack` translates them as it does today. Its `--cap` list for a POSIX
  program is the session set, and the launcher decides what to grant.
* **A build-time check that matters:** disassemble the final image and fail if
  any `syscall` instruction (`0f 05`) appears outside libnode and liblx
  (`scripts/build-lx-apps.sh` does this for every program it builds).
  A Linux syscall number reaching the NodeOS kernel directly means the wrong
  call with the wrong arguments (Linux 1 is `write`, NodeOS 1 is
  `process_create`), and nothing stops it at runtime until K9 exists.
* **Rust is included.** Rust's `x86_64-unknown-linux-musl` target links
  `std` against a musl `libc.a`. Point it at this sysroot (`-C linker=nodeos-cc
  -C link-self-contained=no`) and ordinary Rust programs that use
  `std`, such as ripgrep or fd, build for NodeOS with no NodeOS-specific code.

`make test-lua` follows the existing `test-cc` pattern. It boots, runs a C
test program and Lua (a script, then an interactive session typed through
QEMU's monitor), and compares their output on the serial log, where liblx
mirrors everything written to stdout and stderr as `lx: | <line>`. It also
saves a screenshot to `build/lua-test.ppm`.

---

## 6. Beyond recompiling: running existing Linux binaries

Two further steps turn "recompile and run" into "download and run". Both build
on the work above rather than replacing it.

**Dynamic musl (phase 6): the Alpine ecosystem.** Alpine's packages are
dynamically linked against musl's `libc.so`, which also serves as the dynamic
linker (`/lib/ld-musl-x86_64.so.1`). If NodeOS ships a `libc.so` that is this
musl with liblx linked in, then an unmodified Alpine binary calls *our* libc,
which uses the same functions, the same structure layouts and the same
versioning, and runs. Its only contact with the kernel goes through our
`__lx_syscall`. This needs a userspace ELF loader (K4), file-backed private
mappings, NXF's missing symbolic relocations (or none at all: `ld-musl`
relocates itself and everything it loads), and a launcher rule for a Linux ELF
with no NXF capability declarations. A good policy is to grant the session set
and nothing more. Most of `apk add` would then simply work.

**Raw syscalls (phase 7): static binaries and Go.** Statically linked Linux
binaries and Go programs carry their own `syscall` instructions. For those,
add **syscall user dispatch** (K9), which is modelled on Linux's own
`PR_SET_SYSCALL_USER_DISPATCH`. A process marked as a Linux personality may
execute `syscall` only from inside one allowed text range, liblx's. A
`syscall` from anywhere else is not dispatched as a NodeOS syscall. Instead the
kernel saves the frame and resumes the thread at liblx's registered handler,
which calls `__lx_syscall` and returns with `rt_sigreturn`-style restoration.
The loader maps liblx as a standalone position-independent blob into the
runtime window (`0x7000_0000_0000`) of a process it did not link against.
This is the same idea as FreeBSD's Linuxulator and WSL1, but done in the
process instead of in the kernel. It is also the only mechanism that settles
the syscall-number collision safely: dispatch is decided by *where* the
instruction is, never by the number in `rax`.

---

## 7. Phases

Each phase ends with a test in the style of the existing `make test-*`
targets.

| Phase | Kernel | liblx / userspace | Proof |
| --- | --- | --- | --- |
| 1. hello | K1, K2, K8 | patched musl; sysroot; startup; `write`/`writev`/`exit_group`/`mmap`/`ioctl(TIOCGWINSZ)`/`clock_gettime`; region table | `printf("%f", 1.5)` and a 4-thread mutex counter; **done except threads (K2): Lua runs, `make test-lua`** |
| 2. files | K5 (optional) | descriptor table; fs, tty, null, pipe; termios; `poll`; `FS_RENAME`; console escape parsing | `cat`, `sed`, `sort`, `vi` from busybox, built NOMMU |
| 3. processes | K3, K4 | spawn-on-exec `vfork`/`execve`; `wait4`; cooperative signals; `proc` service | busybox `sh` (NOMMU) running the census workload |
| 4. fork | K6 | userspace `fork` | busybox MMU build; `make` and `lua` from source |
| 5. async signals | K7 | signal frames; fault reflection | a `SIGALRM` interrupting a busy loop; a program that catches `SIGSEGV` |
| 6. dynamic | K4 | ELF loader; `libc.so`; private file mappings | an unmodified Alpine `busybox` or `lua` binary |
| 7. raw syscalls | K9 | liblx as an injectable blob | a static Go "hello" |

Phases 1 to 3 are the bulk of the value. With them, a NOMMU busybox, every
C program that does not fork, and most Rust programs run.

### Out of scope for now

* **Networking.** Sockets need a network stack service first. `AF_UNIX`
  could be built on channels earlier, and should carry handles in
  `SCM_RIGHTS`, which maps directly onto channel capability transfer.
* **Users and permissions.** There are none, which is the point of NodeOS.
  `getuid` returns 0 so that programs which insist on root are satisfied,
  `setuid` succeeds as a no-op, and file modes are synthesized. Authority is
  still decided only by the handles the process holds.
* **`/proc` and `/sys`.** Programs that read `/proc/self/maps`,
  `/proc/cpuinfo` or `/proc/meminfo` can be served by liblx synthesizing those
  few files. A real `procfs` is unnecessary.
* **`io_uring`, `ptrace`, `epoll`.** `epoll` is easy on top of `wait` (an
  interest list kept in liblx) and is worth doing once programs need it. The
  other two are not.

---

## 8. Risks

* **The region table must be authoritative.** If any code maps memory without
  going through liblx, `fork` and `munmap` splitting silently go wrong. The
  debug-only region-enumeration check in §4.3 exists for this.
* **Spawn-on-exec breaks on programs that misuse `vfork`.** A child that writes
  through globals, returns from the function that called `vfork`, or calls
  non-async-signal-safe functions before `execve` is already undefined
  behaviour on Linux, but it often works there. Before phase 4, such programs
  will fail on NodeOS.
* **Cooperative signals do not preempt.** A program in a tight loop does not
  see Ctrl+C until phase 5. `SIGKILL` still works, because it is a kernel
  operation.
* **Syscall numbers collide.** A single stray `syscall` instruction from musl
  makes an arbitrary NodeOS call. The build check in §5 is not optional.
* **Every request is a message round trip.** A `read` costs an IPC round trip
  where Linux makes a function call, which is the trade ARCHITECTURE.md §2
  already accepts. Programs that read a byte at a time through `stdio` are
  covered by musl's buffering. Programs that `read(fd, buf, 1)` in a loop
  (shells reading scripts) are not, and liblx should read ahead on `File`
  descriptors.
