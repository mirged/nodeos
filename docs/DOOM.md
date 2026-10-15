# Doom on NodeOS

Doom runs on NodeOS as an ordinary disk program, launched from the shell with
`run /bin/doom.nxf`. This document is about what that took, because the
interesting part of the exercise is not that Doom runs -- it runs on everything
-- but which of NodeOS's design choices made the port easy and which made it
hard.

```
   make run
     |  GRUB boots compositor, console, shell
     v
   nodeos> run /bin/doom.nxf
     v
  shell (ring 3)
     |  duplicates its session channels into the child
     |  console, filesystem, compositor, audio
     v
  doom (ring 3)
     |  opens /DOOM.WAD through the filesystem channel
     |  asks the compositor for a window
     +-- src/main.rs, src/platform.rs   the ndos_* boundary
     +-- csrc/                          doomgeneric
     +-- libc/                          just enough libc
```

## What Doom is given, and nothing else

The shell hands every program it launches the same four session handles, in a
fixed order; Doom uses three of them and ignores the first:

| # | Handle | Rights | What Doom does with it |
| --- | --- | --- | --- |
| 0 | `Channel` | channel default | nothing -- the console |
| 1 | `Channel` | channel default | opens `/DOOM.WAD` on the filesystem |
| 2 | `Channel` | channel default | asks `compositor` for a window |
| 3 | `Channel` | channel default | the mixer, when there is one |

That is the complete list. Doom cannot create a process or reach any device --
including the screen it is drawing on -- because it holds nothing that would let
it, and there is no ambient authority to fall back on, which is the whole point
of §1 of [`ARCHITECTURE.md`](ARCHITECTURE.md). "What can Doom do?" is answered
by reading one message.

It used to be a boot module the launcher handed an IWAD and a window. Loading
from the filesystem removed that special case: Doom is a program on the disk
now, and the shell can start it the way it starts any other. It once held two
*more* handles -- the framebuffer and the keyboard, *instead of* `console` --
because there was one screen and one keyboard and the boot configuration had to
decide who got them. `compositor` holds those now and hands out windows instead,
so Doom and the console run side by side; [`COMPOSITOR.md`](COMPOSITOR.md) is
what that took. Doom's own authority went *down* in the process: a window is
strictly less than a framebuffer.

## The IWAD comes from the filesystem

`/DOOM.WAD` sits on the same FAT disk as `/bin/doom.nxf`. Doom opens it over the
filesystem channel, reads it in 32 KiB chunks into a fresh memory object, and
maps that read-only. The mini-libc's `fopen("doom.wad")` then resolves to a
stream over the mapping, so the game reads the WAD in place: `fread` is a
`memcpy` out of the mapping and the remaining ~11 MiB never travels through an
IPC message.

It used to be a boot module instead: the boot loader's copy of the WAD was
turned into a memory object over the physical pages it already occupied
(`Vmo::new_physical`) and granted read-only. That worked, and it made Doom
unlaunchable any other way. Reading it through the filesystem costs one startup
pass -- a few hundred 32 KiB reads, into an object that is then mapped -- and
buys the ordinary-program status. Bulk data still moves by shared memory object;
the filesystem is just the thing that fills it.

Reading a memory object you did not create needs to know how big it is, which
the ABI could not previously answer -- so `HandleInfo` gained a `size` field
(`capability_query`). The filesystem path uses `FS_OPEN`'s reply for the same
information.

## No floating point, enforced by the compiler

> **This section used to be wrong, and the way it was wrong is worth keeping.**
> It said the kernel saved no FPU state and that this only mattered for a
> program doing floating-point arithmetic. The first half was true; the second
> was not. The compiler emits SSE for ordinary 16-byte data movement, so a
> `movups` store that faults on a fresh page came back with a clobbered `xmm0`
> and wrote the kernel's leftovers instead of the program's data -- silently,
> in any program, floating point or not. Building the compositor tripped over
> it; [`COMPOSITOR.md`](COMPOSITOR.md) has the diagnosis. The kernel now saves
> and restores this state on every entry from ring 3.

Doom itself still needs none of it. It is fixed-point throughout -- that is why it ran on a 386 -- so the port
compiles the whole C tree with `-mgeneral-regs-only`, which forbids SSE and
x87 outright. The compiler now *enforces* the restriction: a float that crept
in fails the build rather than failing at 3am in a way nobody can reproduce.

Four places in the original tree had to give way, each marked `__NODEOS__` in
the sources:

* `g_game.c` -- the `-timingdemo` frame rate, now accumulated in tenths.
* `m_config.c` -- reading and writing float-valued settings.
* `v_video.c` -- the `-testcontrols` mouse speedometer, which is the one place
  in Doom that does real floating-point arithmetic. There is no mouse.
* `i_system.c` -- the zenity error dialog, which wants a shell.

Everything else in `csrc/` is doomgeneric as it ships.

## Just enough libc

`user/doom/libc/` is about 1,800 lines: strings, ctype, a printf engine,
a first-fit heap, and streams. Three details are worth naming.

**The heap.** Doom's zone allocator asks for one 16 MiB block at startup and
manages it itself, so `malloc` sees very little traffic. It grows by asking
`ndos_sbrk` for a fresh memory object, mapped into the heap window; nothing is
ever given back, because Doom never asks.

**Streams.** `stdout` is the kernel debug log, line-buffered. The IWAD is the
read-only mapping above. Everything Doom *writes* -- savegames, the config it
rewrites on exit -- goes to a small table of in-memory files. A savegame
therefore survives until reboot and no longer, which is what a system with no
persistent storage can honestly offer. Nothing above the stream layer knows the
difference.

**What is deliberately missing.** `memcpy`, `memmove`, `memset`, `memcmp` and
`strlen` are *not* here. Every NodeOS program already links libnode's versions
(`libnode/src/rt.rs` exists because the compiler emits calls to those names and
there is no libc), and a stable toolchain has no weak linkage, so defining them
again would be a link error rather than an override.

## Video and input

Doom renders at 320x200 into a 32-bit `0x00RRGGBB` buffer, which is
byte-for-byte the layout a NodeOS window surface uses -- so the blit in
`DG_DrawFrame` is a scale and a copy with no pixel conversion. The scale is the
largest whole number that fits its window; integer scaling keeps every Doom
pixel the same size, where a fractional scale would shimmer as the view moves.

Moving to the compositor cost the C tree exactly one line: an `ndos_fb_commit`
at the end of `DG_DrawFrame`, because pixels written to a surface are not on
screen until the compositor is told to copy them. Everything else that changed
is in `platform.rs`.

Input is still raw PS/2 set 1 scancodes, drained non-blocking once per frame
and decoded to Doom key codes with make/break and `0xE0` prefix handling in
`doomgeneric_nodeos.c` -- that decoder was not touched. The compositor now
delivers *events* rather than a byte stream, so `platform.rs` re-encodes each
one back into the bytes the C side already knew how to read. Arrows move, Ctrl
fires, Space opens, Shift runs, Alt strafes, Tab is the automap, 1-7 select
weapons; Meta hands the keyboard to the window next door.

Sound goes through the `audio` service: Doom's `i_nodeossound.c` turns each of
Doom's channel decisions into one `AUDIO_PLAY` on the mixer, which holds the
sound card. Doom holds a channel to it, not the device.

## Building it

The C is compiled by `user/doom/build.rs`, which shells out to the host gcc --
no cross-compiler is needed, since the host is already x86-64 and only has to
be told to stop assuming an operating system. The flags that matter are in that
file's module comment.

The IWAD is not in this repository. Put one at `DOOM.WAD` in the repository
root, or point `DOOM_WAD` at it, and `make` packages it onto the disk as
`/DOOM.WAD`:

```sh
make DOOM_WAD=/path/to/doom1.wad
make run
nodeos> run /bin/doom.nxf
```

Shareware `doom1.wad` works; so does a full `doom.wad`. A build without one
still compiles and boots -- `run /bin/doom.nxf` just reports that it cannot read
the IWAD.

## What this exercised

The port touched five subsystems and found one gap.

* **Capabilities** did what they claim: the handles the shell forwards are the
  whole of Doom's authority, and they are the same session every program gets.
* **Memory objects** make the IWAD a read-through-then-map operation rather
  than a copy held on the heap: the filesystem fills one, the game reads it in
  place.
* **Device capabilities** were already the right shape -- Doom used the
  framebuffer exactly as `console` did, with no new kernel code. It holds
  neither any more; see [`COMPOSITOR.md`](COMPOSITOR.md).
* **The NXF loader and the resource envelope** loaded a 704 KiB image with a
  1 MiB stack without complaint.
* **The gap**: `capability_query` could not report an object's size, so a
  process handed a memory object had no way to learn how much of it to map.
  Fixed.

The one thing the port worked *around* rather than fixed was floating point,
on the reasoning that Doom did not need it and saving FPU state costs
something. That reasoning was sound and the conclusion was wrong: the state
had to be saved anyway, for programs that never do arithmetic on it. The
kernel does now.
