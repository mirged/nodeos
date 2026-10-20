# Building NodeOS

## The unusual part

NodeOS builds a freestanding kernel and freestanding userspace programs using a
**stable** Rust toolchain and the **host** target triple
(`x86_64-unknown-linux-gnu`), with `#![no_std]` and a custom linker script.

That is not the conventional recipe. The conventional recipe is a custom target
JSON plus `-Z build-std`, both of which require a nightly toolchain. This tree
was developed in an environment where the nightly channel could not be fetched,
so it takes the other road -- and the road turns out to be perfectly good:

* **`core`, `alloc` and `compiler_builtins` ship precompiled** for the host
  triple, so there is nothing to build from source.
* The **linker script** decides the memory layout, not the target spec.
* `-C relocation-model=static -C code-model=kernel` gets the kernel's
  sign-extended high-half addressing; userspace uses `code-model=small`.
* `-C no-redzone=yes` and `-C force-frame-pointers=yes` apply to our code.

### What we have to supply ourselves

Because the host target assumes a libc, `compiler_builtins` is built *without*
its `mem` feature. `kernel/src/rt.rs` and `user/libnode/src/rt.rs` therefore
provide `memcpy`, `memmove`, `memset`, `memcmp`, `bcmp` and `strlen`, plus
`rust_eh_personality` and `_Unwind_Resume` stubs that the precompiled `core`
references but that `panic = "abort"` makes unreachable.

### The one hazard worth knowing about

The precompiled `core` was built **with SSE enabled and with the red zone
enabled**. Two consequences:

1. The boot trampoline must enable SSE (`CR0.EM=0`, `CR0.MP=1`, `CR4.OSFXSR`,
   `CR4.OSXMMEXCPT`) *before the first Rust instruction*, or the first
   compiler-emitted `movaps` raises `#UD`.
2. The kernel runs with **interrupts disabled** (see the interrupt discipline in
   `docs/ARCHITECTURE.md`), so an interrupt can never land inside a
   compiler-generated leaf function that is using the red zone.

Also: SysV requires `RSP + 8` to be 16-byte aligned *at a function's first
instruction*. Every hand-written entry path in this tree -- the boot
trampoline, the interrupt stubs, the syscall stub, the thread trampoline, and
`_start` -- states that alignment explicitly rather than deriving it, because
getting it wrong by eight bytes produces a `#GP` inside the first SSE stack
spill, a very long way from the cause.

## Prerequisites

```sh
sudo apt-get install -y qemu-system-x86 grub-pc-bin grub-efi-amd64-bin grub-common xorriso mtools
rustup toolchain install stable
```

No nightly, no `rust-src`, no extra targets, no crates.io dependencies at all --
the whole tree builds from `core` and `alloc`.

## Targets

```sh
make            # kernel + userspace + tools + ISO + disk (the whole build)
make run        # boot the full-screen shell in QEMU
make kernel     # just the kernel ELF
make user       # just the userspace programs
make modules    # pack userspace programs into NXF images
make iso        # build the interactive ISO, build/nodeos.iso
make disk       # build the FAT disk, build/disk.img
make debug      # boot and wait for gdb on :1234
make test       # headless self-tests
make clean
```

The ISO build includes GRUB's BIOS and UEFI boot images when both GRUB module
sets are installed. Do not restrict `grub-mkrescue` to `i386-pc`: that produces
a BIOS-only ISO, which does not show up in the boot menu of UEFI-only machines.
The generated `build/nodeos.iso` can be written directly to a USB drive with
Rufus in DD mode or another raw image writer. On Linux, use `lsblk` to identify
the whole USB device, then write with `sudo dd if=build/nodeos.iso
of=/dev/sdX bs=4M status=progress oflag=sync` (replace `/dev/sdX` with the
device, not a partition). This erases the selected drive.

`make help` lists the rest; everything past `make`/`make run` is a debug or CI
target.

## Three cargo workspaces, on purpose

| Workspace | Target | Flags come from |
| --- | --- | --- |
| `/` (kernel, abi) | bare metal, ring 0 | `kernel/.cargo/config.toml` |
| `/user` | bare metal, ring 3 | `user/.cargo/config.toml` |
| `/tools` | the host | nothing special |

They are separate because `RUSTFLAGS` in a cargo config applies per target
triple, and all three use the same triple with wildly different flags. Cargo
reads `.cargo/config.toml` from the invocation directory upward, so each
workspace gets its own flags by being built from its own directory -- which is
what the Makefile does. Each also has its own `target-dir` so switching between
them does not force a rebuild.

The linker-script paths in those configs are relative to the **workspace root**,
because that is cargo's working directory when it invokes `rustc`.

## Debugging

```sh
make debug            # in one terminal
gdb target/kernel/release/nodeos-kernel -ex 'target remote :1234'
```

The kernel image carries full DWARF, and every address printed in a panic is a
link-time address, so `addr2line` works directly:

```sh
addr2line -e target/kernel/release/nodeos-kernel -f -C 0xffffffff8021...
```

QEMU's own tracing is often faster for entry-path bugs:

```sh
qemu-system-x86_64 ... -d int,cpu_reset,guest_errors -D build/qemu.log
```
