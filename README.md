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
   | display    | console |   fs    | audio   |
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
| Full-screen display/input broker; no desktop, windows or mouse UI | working |
| Text console, cursor, 512-line scrollback and command shell | working |
| Doom as an explicitly launched full-screen program (`docs/DOOM.md`) | working |
| Linux programs: musl over an in-process Linux personality (`docs/MUSL.md`) | working: Lua 5.4 |
| Maylang programs, and the Maylang compiler running here (`docs/MAYLANG.md`) | working: self-hosting |
| Userspace services (ramfs, devmgr) | in progress |

## Quick start

```sh
# Prerequisites: Rust (stable), QEMU, GRUB + xorriso for the ISO.
sudo apt-get install -y qemu-system-x86 grub-pc-bin grub-efi-amd64-bin grub-common xorriso mtools

make      # build everything: kernel, userspace, ISO and disk
make run  # boot the full-screen shell in QEMU
```

At the shell, `doom` plays Doom; `/bin` also holds `sysinfo`,
`tree` and `bench`. Every other target is a debug or CI check; `make help`
lists them all.

```sh
make test         # headless kernel self-tests
make test-fs      # filesystem I/O through the shell
make test-apps    # run the console apps from the shell
make test-cc      # compile and run a C program
make test-edit    # edit and save a file
make test-lua     # build Lua 5.4 against musl and run it
make test-maylang # run Maylang programs and the Maylang compiler
make test-audio   # check sound using QEMU's wav backend
make test-shell   # scripts, editing, paging, foreground apps, Doom return
make test-shell-parser # quotes, escapes and command-file syntax
make test-fat-read # fragmented files and batched disk reads
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

## The shell

NodeOS boots directly to a full-screen text shell. There is no graphical
desktop, taskbar, pointer, tiling or window chrome. The display service only
brokers the screen and keyboard: starting a full-screen application temporarily
hands it the display; exiting restores the console's exact previous contents.
The shell waits for foreground programs rather than interleaving another prompt
with their output.

The console uses a smooth 16-pixel monospace font, comfortable line spacing
and inset margins. Prompts, headings and errors use restrained, high-contrast
colours. Use `font large` for larger text, `font small` for a denser view, or
`font normal` to restore the default; earlier output remains in scrollback.

The prompt shows the working directory. Programs resolve in `/bin`, then in
the working directory, so type `sysinfo`, `tree`, `bench`, `edit /NOTES.TXT`,
`cc /tests/hello.c` or `doom`. `run /bin/<name>.nxf [args...]` still accepts
explicit paths. Programs start in the shell's working directory.

```text
/> help syntax
/> write /NOTES.TXT "first line\nsecond line\n"
/> cp /NOTES.TXT /BACKUP.TXT; grep "second line" /BACKUP.TXT
/> cd /docs; more NODEOS.MD
/docs> cd -
/> source /TESTS/SETUP.NSH
/> NAME=world; echo "hello $NAME" > /HELLO.TXT
/> ls -l /bin > /LIST.TXT && wc /LIST.TXT || echo "no listing"
/> export GREETING=hi; mayupper < /HELLO.TXT >> /HELLO.TXT
```

Single and double quotes preserve spaces and empty arguments. Backslash escapes
a character; double quotes additionally accept `\n` and `\t`. Newlines and
semicolons separate commands; `#` starts a comment at a word boundary. Input is
fully parsed before execution, so an unclosed quote cannot execute the commands
before it. `source` runs UTF-8 command files, capped at 64 KiB and eight nested
files.

`NAME=value` sets a shell variable and `$NAME`, `${NAME}` or `$?` (the last
exit status) use it; a value never splits into several arguments. `export`
passes variables to every program started, `NAME=value program` to one; `env`
and `set` list them, `unset` removes them. `a && b` runs `b` only if `a`
succeeded and `a || b` only if it failed, using the exit status programs
report. `>` replaces a file with a command's output, `>>` appends, and
`program < file` feeds a program its input lines. A program's output and input
are redirected by handing it a console channel the shell serves itself, not a
file descriptor. Pipes (`|`) and background jobs (`&`) are rejected.

`help` groups commands by task; `help cp` (or another command name) shows its usage.
Directory listings put folders first and show readable file sizes.

`cp` streams file data and refuses to replace an existing destination unless
`-f` is given. It refuses copying onto the same normalized, case-insensitive
path. `grep` searches literal text and includes line numbers. `more` pages text
with Space, Enter or q. The normal filesystem commands remain: `ls`, `cat`,
`write`, `append`, `touch`, `mkdir`, `rm`, `stat`, `df` and `fstest`, and
`mv`, `head`/`tail [-n N]`, `wc`, `find [dir] [text]`, `rmdir`, `rm -r` and
`ls -l` (exact sizes, one entry per line). `mv` copies and deletes, because the
filesystem has no rename, so it moves files only. Writes use exact text;
include `\n` in a quoted argument when you want a trailing newline.
`make test-shell-features` checks all of this by reading the disk back.
FAT creation still uses 8.3 names.

| Keys | Action |
| --- | --- |
| Left/Right, Home/End, Delete | Edit the current line |
| Up/Down | Recall the last 128 distinct adjacent history entries |
| Ctrl+A / Ctrl+E | Move to the start / end |
| Ctrl+U / Ctrl+K / Ctrl+W | Erase left / right / previous word |
| Ctrl+C / Ctrl+L | Cancel the line / clear the screen |
| Tab / Tab twice | Complete commands and paths / show matching choices |
| Ctrl+Left / Right | Move by word |
| Ctrl+D | Delete the next character |
| Shift+Page Up / Page Down | Browse 512 lines of terminal history |
| Meta+Q | Ask a foreground full-screen app to close |
| Meta or Scroll Lock | Cycle full-screen sessions |

Typing or new output returns scrollback to the live terminal. See
[`docs/CONSOLE.md`](docs/CONSOLE.md) for the display and console design.

## Doom

Put a Doom IWAD at `DOOM.WAD` in the repository root before `make` (or pass
`DOOM_WAD=/path/to/doom1.wad`); `make` packages it as `/DOOM.WAD` on the disk.
Shareware `doom1.wad` works, so does a full `doom.wad`. Then:

```sh
make run
/> doom
```

Doom is an ordinary disk program now: launched from the shell, it reads its IWAD
from the filesystem like any other client. It holds a channel to the display service,
a channel to the filesystem and, when there is a mixer, a channel to `audio` --
it cannot create a process or reach any device, including the screen it is
drawing on, because it was handed nothing that would let it. It runs full-screen and returns to the shell when it exits.

The interesting part is not that Doom runs -- it runs on everything -- but
which design choices made the port easy and which made it hard. That is
[`docs/DOOM.md`](docs/DOOM.md).

## The shell and the programs it runs

The shell is an ordinary console client, but it reads raw keys now, so it has
history (Up/Down), cursor movement, Tab completion of commands and file names,
and Ctrl+C to abandon a line. It launches programs with `run /bin/<name>.nxf
[args...]`, handing each child the same session -- console, filesystem,
display, audio channels -- and its command line.

* `sysinfo` -- a process's view of the system: its capability set, the clock,
  the console's geometry, a memory object it allocates, and filesystem
  statistics and directory listing. `make test-apps` runs it.
* `tree` -- a recursive filesystem listing. `make test-apps`.
* `bench` -- a memory fill/verify and scheduler-yield benchmark. `make test-apps`.
* `edit` -- a full-screen code editor: syntax colouring for Maylang, C, Lua
  and shell scripts, line numbers, several files in tabs (`edit a.may b.may`,
  Ctrl+O, Ctrl+N), undo and redo, Shift-selection with cut, copy and paste,
  find and replace, go to line, comment toggling, auto-indent and closing
  brackets. F1 lists the keys. `make test-edit` types into it and reads the
  saved file back with mtools; `make test-editor` tests its buffer and
  highlighting on the host.
* `doom` -- see above.
* `cc` -- a small C compiler. It reads a `.c` file from the disk, compiles it to
  a stack machine, and runs it, with a standard library of builtins (`printf`,
  `strlen`, `malloc`, `rand`, and friends). `make test-cc` compiles and runs
  `user/cc/tests/hello.c` and checks the output.

```
/> run /bin/edit.nxf /notes.txt
/> run /bin/cc.nxf /tests/hello.c
hello, world
sum 1..10 = 55
...
```

## Linux programs

Unmodified C programs written for Linux build against musl and run on
NodeOS. `make lx-apps` fetches musl 1.2.6 and Lua 5.4.7 (from
github.com/lua/lua), builds both, and the next `make` puts them on the disk:

```
nodeos> lua /tests/hello.lua
hello from Lua 5.4 on NodeOS
fib(20) = 6765
...
nodeos> lua
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

## Maylang

[Maylang](https://github.com/mirged/maylang) targets NodeOS directly
(`--target x86_64-nodeos`), and its compiler runs here. `make maylang` builds
it from a sibling checkout (`MAYLANG=../maylang`), and the next `make` puts it
on the disk with its runtime, the standard library and its own source:

```
/> mayc /tests/hello.may -o /hello.nxf
/> run /hello.nxf
Hello, Maylang!
/> mayc /src/mayc/main.may -o /bin/mayc2.nxf
```

The last line is the compiler rebuilding itself; the result is byte-identical
to the compiler cross-built on Linux (`make test-maylang-selfhost`; use
`QEMU_MEM=1G` to do it interactively). Maylang programs need no libc: `mayc`
writes NXF itself, and a personality compiled into each program answers the
runtime's Linux-numbered syscalls with NodeOS syscalls and the console and
filesystem protocols. [`docs/MAYLANG.md`](docs/MAYLANG.md) has the details.

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
* [`docs/CONSOLE.md`](docs/CONSOLE.md) -- the full-screen console, display broker
  and shell workflows.
* [`docs/DOOM.md`](docs/DOOM.md) -- what porting Doom exercised, and the one
  gap it found.
* [`docs/MUSL.md`](docs/MUSL.md) -- the plan for running ordinary Linux
  programs: musl over an in-process Linux personality, and what the kernel
  needs for it.
* [`docs/MAYLANG.md`](docs/MAYLANG.md) -- Maylang programs and the Maylang
  compiler on NodeOS.

## Licence

Unlicensed research code. Take what is useful.
