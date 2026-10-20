# NodeOS

An experimental capability-based, message-oriented microkernel for x86-64,
written in Rust from scratch. It is deliberately **not** a Unix.

```
             Applications (shell, doom)
                     |
                Typed IPC
                     |
              Userspace services
   +------------+---------+---------+---------+
   | compositor | console |  ramfs  | devmgr  |
   +------------+---------+---------+---------+
                     |
                Microkernel
                     |
                  Hardware
```

## What is actually implemented

Every item below is exercised by the in-kernel self-tests or by `init` at boot,
and the whole thing runs in QEMU today.

| Layer | Status |
| --- | --- |
| Multiboot2 boot, 32→64-bit trampoline, higher-half kernel | working |
| GDT / TSS / IST, IDT with 256 stubs, exception decoding | working |
| Local APIC, calibrated APIC timer at 1 kHz, PIC remap, PIT | working |
| Buddy frame allocator with a reference-counted frame database | working |
| 4-level paging, W^X kernel image, direct physical map, guard pages | working |
| Kernel heap (`GlobalAlloc`, size classes + buddy backend) | working |
| Typed kernel objects, per-process handle tables, generation tags | working |
| Rights that only ever narrow; transitive capability revocation | working |
| Memory objects (VMOs), demand paging, copy-on-write | working |
| Processes, threads, per-thread kernel stacks with guard pages | working |
| Modular scheduler: real-time / interactive / normal / background | working |
| Typed IPC channels, capability transfer, backpressure, call/reply | working |
| 27-syscall ABI over `SYSCALL`/`iretq`, validated user pointers | working |
| NXF executable format, loader, and host packer | working |
| Userspace runtime (`libnode`) and `init` running in ring 3 | working |
| Per-process resource namespaces (kernel object + launcher policy) | partial |
| User x87/SSE state preserved across every entry from ring 3 | working |
| `Device` capability: boot framebuffer + PS/2 keyboard + PS/2 mouse | working |
| `compositor`: the only holder of the screen, keyboard and mouse (`docs/COMPOSITOR.md`) | working |
| Linux programs: musl over an in-process Linux personality (`docs/MUSL.md`) | working: Lua 5.4 |
| `console` (text/graphics service) and `shell`, as compositor clients | working |
| Doom, in a window beside the shell (`docs/DOOM.md`) | working |
| Userspace services (ramfs, devmgr) | in progress |

## Quick start

```sh
# Prerequisites: Rust (stable), QEMU, GRUB + xorriso for the ISO.
sudo apt-get install -y qemu-system-x86 grub-pc-bin grub-efi-amd64-bin grub-common xorriso mtools

make      # build everything: kernel, userspace, ISO and disk
make run  # boot the window system in QEMU
```

At the shell, `run /bin/doom.nxf` plays Doom; `/bin` also holds `sysinfo`,
`tree` and `bench`. Every other target is a debug or CI check; `make help`
lists them all.

```sh
make test         # headless kernel self-tests
make test-fs      # filesystem I/O through the shell
make test-apps    # run the console apps from the shell
make test-cc      # compile and run a C program
make test-edit    # edit and save a file
make test-lua     # build Lua 5.4 against musl and run it
make test-audio   # check sound using QEMU's wav backend
make test-mouse   # drive the mouse and check the pixels
make debug        # boot and wait for GDB on :1234
```

`build/nodeos.iso` is a hybrid BIOS/UEFI boot image. To boot a physical
machine, write that ISO to a USB drive with an image writer (for example,
Rufus in DD mode or `dd`), then select the USB device from the firmware boot
menu. Writing an image erases the selected drive. On Linux, first identify the
whole USB device with `lsblk`, then run `sudo dd if=build/nodeos.iso
of=/dev/sdX bs=4M status=progress oflag=sync` with `/dev/sdX` replaced by the
correct device (not a partition such as `/dev/sdX1`).

You should see the kernel bring itself up, run 121 self-tests, launch `init`
from an NXF module, and `init` report back from ring 3.

## The window system

`make run` boots `compositor`, `console` and `shell`, and typing
`run /bin/doom.nxf` adds Doom as a second window beside the console.

`compositor` holds the framebuffer, the PS/2 keyboard and the mouse, and it is
the only process in the system that does. Everything else -- the console, Doom
-- gets a memory object the size of its own window and a channel, which is the
whole of its display authority.

With a pointer, the window system behaves like one: click a window to focus it,
click the empty desktop to give the focus up, drag a title bar to move a window
between columns, and double-click one to maximize it. The keyboard still does
everything it did -- Meta ("Windows") or Scroll Lock cycles the focus, Meta+Tab
too, Meta+Enter toggles maximize -- which is what makes a machine with no mouse
a usable one. See [`docs/COMPOSITOR.md`](docs/COMPOSITOR.md) for the input path, and for why
checking any of it needs a framebuffer rather than a serial log.

## Doom

Put a Doom IWAD at `DOOM.WAD` in the repository root before `make` (or pass
`DOOM_WAD=/path/to/doom1.wad`); `make` packages it as `/DOOM.WAD` on the disk.
Shareware `doom1.wad` works, so does a full `doom.wad`. Then:

```sh
make run
nodeos> run /bin/doom.nxf
```

Doom is an ordinary disk program now: launched from the shell, it reads its IWAD
from the filesystem like any other client. It holds a channel to the compositor,
a channel to the filesystem and, when there is a mixer, a channel to `audio` --
it cannot create a process or reach any device, including the screen it is
drawing on, because it was handed nothing that would let it. That is also why it
runs *beside* the shell rather than instead of it: it occupies the next column.

The interesting part is not that Doom runs -- it runs on everything -- but
which design choices made the port easy and which made it hard. That is
[`docs/DOOM.md`](docs/DOOM.md).

## The shell and the programs it runs

The shell is an ordinary console client, but it reads raw keys now, so it has
history (Up/Down), cursor movement, Tab completion of commands and file names,
and Ctrl+C to abandon a line. It launches programs with `run /bin/<name>.nxf
[args...]`, handing each child the same session -- console, filesystem,
compositor, audio channels -- and its command line. A program runs in the
foreground: the shell waits for it to exit before prompting again, so the two
never draw over each other or race for the keyboard. End the line with `&` to
run it in the background instead, which suits a program in its own window
(`run /bin/doom.nxf &`).

* `sysinfo` -- a process's view of the system: its capability set, the clock,
  the console's geometry, a memory object it allocates, and filesystem
  statistics and directory listing. `make test-apps` runs it.
* `tree` -- a recursive filesystem listing. `make test-apps`.
* `bench` -- a memory fill/verify and scheduler-yield benchmark. `make test-apps`.
* `edit` -- a full-screen text editor in its own window: type, move with the
  arrow keys, save with Ctrl+S, quit with Ctrl+Q. `make test-edit` types into it
  and reads the saved file back with mtools.
* `doom` -- see above.
* `cc` -- a small C compiler. It reads a `.c` file from the disk, compiles it to
  a stack machine, and runs it, with a standard library of builtins (`printf`,
  `strlen`, `malloc`, `rand`, and friends). `make test-cc` compiles and runs
  `user/cc/tests/hello.c` and checks the output.

```
nodeos> run /bin/edit.nxf /notes.txt
nodeos> run /bin/cc.nxf /tests/hello.c
hello, world
sum 1..10 = 55
...
```

## Linux programs

Unmodified C programs written for Linux build against musl and run on
NodeOS. `make lx-apps` fetches musl 1.2.6 and Lua 5.4.7 (from
github.com/lua/lua), builds both, and the next `make` puts them on the disk:

```
nodeos> run /bin/lua.nxf /tests/hello.lua
hello from Lua 5.4 on NodeOS
fib(20) = 6765
...
nodeos> run /bin/lua.nxf
Lua 5.4.7  Copyright (C) 1994-2024 Lua.org, PUC-Rio
> print(6*7)
42
```

musl is upstream except for the seven files that issue a raw `syscall`
instruction (`user/lx/musl/`). Those now call `__lx_syscall`, and liblx
(`user/lx/`) answers each Linux syscall in-process with the handles the
program was given: descriptors 0-2 are the console, `open` is the filesystem
channel, and `mmap` is memory objects. `scripts/lx-cc` is the compiler driver,
for use as `$(CC)`. [`docs/MUSL.md`](docs/MUSL.md) has the design, what works,
and what is still to do: threads, `fork`/`exec` and signals.

## The C compiler

The compiler is a real one -- tokenizer, recursive-descent parser, type-aware
code generator, virtual machine -- for a deliberate subset of C: one machine
word (`int`), `char`, pointers and arrays, functions with recursion, `for`,
`while`, `if`, and the usual operators. No structs, floating point, or
preprocessor. See `user/cc/src/`; the subset's boundaries are argued in the
module comments, not hidden.

## Layout

```
kernel/     the microkernel
abi/        the kernel/userspace contract: syscalls, errors, NXF, protocols
user/       libnode (the userspace runtime) and userspace programs
tools/      nxfpack, the ELF -> NXF packager
grub/       boot loader configuration
docs/       design documents
```

## Design documents

* [`docs/ARCHITECTURE.md`](docs/ARCHITECTURE.md) -- why the system is shaped
  the way it is, and what each decision costs.
* [`docs/BUILD.md`](docs/BUILD.md) -- how a bare-metal kernel gets built with a
  stable Rust toolchain and no custom target.
* [`docs/NXF.md`](docs/NXF.md) -- the executable format.
* [`docs/COMPOSITOR.md`](docs/COMPOSITOR.md) -- the window system, and the
  kernel bug that building it uncovered.
* [`docs/DOOM.md`](docs/DOOM.md) -- what porting Doom exercised, and the one
  gap it found.
* [`docs/MUSL.md`](docs/MUSL.md) -- the plan for running ordinary Linux
  programs: musl over an in-process Linux personality, and what the kernel
  needs for it.

## Licence

Unlicensed research code. Take what is useful.
