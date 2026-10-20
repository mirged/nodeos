# Doom on NodeOS

Doom is an ordinary disk program launched by `doom` or `run /bin/doom.nxf`.
The shell stages its NXF into a memory object, creates a process and supplies
console, filesystem, private display and audio channels. It waits for process
termination before printing the next prompt.

The game holds no device handles. It loads `/DOOM.WAD` through the filesystem,
maps it read-only, draws into a full-screen surface and commits frame damage to
the display broker. Keyboard events carry ASCII and raw scancodes; the platform
shim reconstructs the make/break stream Doom expects. Audio goes to the mixer.

Doom renders 320x200 and scales by whole pixels to fit the full-screen surface.
The C sources are built with host GCC and a small libc. WAD reads use the mapped
file; configuration and savegame streams remain in process-local RAM and are
lost on exit. The port does not yet persist those streams to the FAT disk.
The kernel saves x87/SSE state on every transition from userspace, including
programs whose own sources contain no explicit floating-point arithmetic.

## Build and run

Put a compatible IWAD in the repository root as `DOOM.WAD`, or pass
`make DOOM_WAD=/path/to/doom1.wad`. Shareware and registered Doom IWADs work.
Then `make run` and type `doom`. A build without an IWAD still boots the shell;
Doom logs that the file is missing and exits. Movement, Ctrl to fire, Space to
open, Shift to run and the game's normal menus use keyboard input. Meta+Q
closes the foreground game; Scroll Lock is a Meta alias for virtual machines.

## Full-screen shell and startup

The desktop and tiled window manager have been removed. The shell launches Doom
as a foreground full-screen program; Meta+Q closes it and restores the console.
The IWAD is loaded before Doom takes the screen, so the terminal remains visible
while the filesystem is read.

The FAT reader indexes the last file's cluster chain lazily and coalesces
physically adjacent clusters into larger disk transfers. It no longer walks
from the beginning for every 512-byte sector. Table mutations invalidate the
index; fragmented files and unaligned reads still follow the real chain.

A local QEMU run of the 10.6 MiB registered IWAD improved from about 12 seconds
from Enter to WAD mapping to under half a second. The guest logs its WAD read
in milliseconds. `make test-fat-read` checks batching, fragmentation, EOF and
invalidation; `make test-shell` checks full-screen Doom and return to the shell.
