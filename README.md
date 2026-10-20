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

The prompt shows the working directory. Programs resolve in `/bin`, so type
`sysinfo`, `tree`, `bench`, `edit /NOTES.TXT`, `cc /tests/hello.c` or `doom`.
`run /bin/<name>.nxf [args...]` still accepts explicit paths.

```text
/> help syntax
/> write /NOTES.TXT "first line\nsecond line\n"
/> cp /NOTES.TXT /BACKUP.TXT; grep "second line" /BACKUP.TXT
/> cd /docs; more NODEOS.MD
/docs> cd -
/> source /TESTS/SETUP.NSH
```

Single and double quotes preserve spaces and empty arguments. Backslash escapes
a character; double quotes additionally accept `\n` and `\t`. Newlines and
semicolons separate commands; `#` starts a comment at a word boundary. Input is
fully parsed before execution, so an unclosed quote cannot execute the commands
before it. `source` runs UTF-8 command files, capped at 64 KiB and eight nested
files. Operators `|`, `&`, `<` and `>` are explicitly rejected outside quotes.

`help` groups commands by task; `help cp` (or another command name) shows its usage.
Directory listings put folders first and show readable file sizes.

`cp` streams file data and refuses to replace an existing destination unless
`-f` is given. It refuses copying onto the same normalized, case-insensitive
path. `grep` searches literal text and includes line numbers. `more` pages text
with Space, Enter or q. The normal filesystem commands remain: `ls`, `cat`,
`write`, `append`, `touch`, `mkdir`, `rm`, `stat`, `df` and `fstest`. Writes use
exact text; include `\n` in a quoted argument when you want a trailing newline.
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
* `edit` -- a full-screen text editor on its own full-screen surface: type, move with the
  arrow keys, save with Ctrl+S, quit with Ctrl+Q. `make test-edit` types into it
  and reads the saved file back with mtools.
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

## Licence

Unlicensed research code. Take what is useful.
