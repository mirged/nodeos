# NodeOS

An experimental capability-based, message-oriented microkernel for x86-64,
written in Rust from scratch. It is deliberately **not** a Unix.

```
                Applications
                     |
                Typed IPC
                     |
              Userspace services
   +---------+---------+---------+-----------+
   | console |  ramfs  | devmgr  |  drivers  |
   +---------+---------+---------+-----------+
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
| 24-syscall ABI over `SYSCALL`/`iretq`, validated user pointers | working |
| NXF executable format, loader, and host packer | working |
| Userspace runtime (`libnode`) and `init` running in ring 3 | working |
| Per-process resource namespaces (kernel object + launcher policy) | partial |
| Userspace services (console, ramfs, devmgr) | in progress |

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

## Licence

Unlicensed research code. Take what is useful.
