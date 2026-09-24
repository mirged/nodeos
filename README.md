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
| `Device` capability: boot framebuffer + PS/2 keyboard | working |
| `compositor`: the only holder of the screen and keyboard (`docs/COMPOSITOR.md`) | working |
| `console` (text/graphics service) and `shell`, as compositor clients | working |
| Doom, in a window beside the shell (`docs/DOOM.md`) | working |
| Userspace services (ramfs, devmgr) | in progress |

## Quick start

```sh
# Prerequisites: Rust (stable), QEMU, GRUB + xorriso for the ISO.
sudo apt-get install -y qemu-system-x86 grub-pc-bin grub-common xorriso mtools

make            # build the kernel, userspace, tools, and a bootable ISO
make run        # boot it in QEMU with the console on stdout
make test       # boot and assert that every self-test passed
```

You should see the kernel bring itself up, run 112 self-tests, launch `init`
from an NXF module, and `init` report back from ring 3.

## The window system

```sh
make run-graphic                            # compositor + console + shell
```

`compositor` holds the framebuffer and the PS/2 keyboard, and it is the only
process in the system that does. Everything else -- the console, Doom -- gets a
memory object the size of its own window and a channel, which is the whole of
its display authority. Cycle the keyboard focus with the Meta ("Windows") key,
or Scroll Lock where the VM eats Meta. See
[`docs/COMPOSITOR.md`](docs/COMPOSITOR.md).

## Doom

```sh
make run-doom DOOM_WAD=/path/to/doom1.wad
```

Doom runs as an ordinary NodeOS process holding exactly two capabilities: a
channel to the compositor, and the IWAD as a read-only memory object. It cannot
open a file, create a process, or reach any device -- including the screen it
is drawing on -- because it was handed nothing that would let it. That is also
why it now runs *beside* the shell rather than instead of it: the boot that
starts Doom starts the console too, in the next column.

The interesting part is not that Doom runs -- it runs on everything -- but
which design choices made the port easy and which made it hard. That is
[`docs/DOOM.md`](docs/DOOM.md).

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

## Licence

Unlicensed research code. Take what is useful.
