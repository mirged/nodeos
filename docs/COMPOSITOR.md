# The window system

Before this, a NodeOS program that wanted to draw was handed the framebuffer
device -- and with it the whole screen, every other program's output included.
That is why `console` and `doom` could not run at the same time: there was one
screen and one keyboard, and `grub/grub-doom.cfg` had to decide who got them.

`compositor` is the process that holds them now, and it is the only one that
does. Everything else gets a memory object the size of its own window and a
channel to send damage on. Doom and the shell run side by side not because they
agreed to share, but because neither can reach the other's pixels.

```
   GRUB
     |  module2 /boot/compositor.nxf  console.nxf  shell.nxf  doom.nxf
     v
  kernel/src/launch.rs
     |  Display + Input  ->  compositor      (framebuffer, keyboard)
     |  Memory + Channel ->  console         (a channel to the compositor)
     |  + Clock          ->  doom            (a channel, and the IWAD)
     v
  compositor (ring 3)
     +-- window.rs     columns, frames, title bars
     +-- keyboard.rs   PS/2 set 1 -> events, decoded once for everyone
     +-- main.rs       the protocol, and who the next keystroke is for
```

## What a client is given

A `WM_CREATE_SURFACE` request comes back with a `Vmo` handle and a
`WmSurfaceInfo` describing it: width, height, pitch, and a surface identifier.
The buffer is 32bpp `0x00RRGGBB`, the same format as the framebuffer, so the
compositor's blit is a copy with no pixel conversion.

The compositor allocates that memory object rather than the client, for one
reason: it is the only party that knows how big the window is going to be, so
having it create the buffer makes surface creation one round trip instead of
two. The client's handle carries `READ | WRITE | MAP` and not `DESTROY`, so a
client cannot pull its own window's pixels out from under a blit in progress.

That handle plus the channel is the client's entire display authority. There is
no framebuffer address to be found, no keyboard to poll, and no way to name
another window -- not because the protocol refuses, but because nothing was
handed over that could express the request.

## Damage, and why commits block

Drawing into a surface changes nothing on screen. `WM_COMMIT` names a rectangle
and the compositor copies exactly that rectangle out. Clients accumulate damage
and commit a batch: `console` unions the rectangles it drew since its last
commit (see `user/console/src/surface.rs`), because a console that committed
its whole window to echo one character would copy a megabyte and a half per
keystroke.

The commit is a `channel_call`, so the client blocks until the compositor has
finished reading those pixels. That is deliberate on both counts:

* it stops a client redrawing a region the compositor is still copying, which
  is the only synchronisation the shared buffer has, and
* it paces a client that draws faster than the display server can keep up --
  Doom's frame loop ends up running at the rate the compositor can actually
  service, without either side knowing anything about the other's timing.

Events go the other way and are *not* acknowledged: the compositor sends them
non-blocking and drops them if the client's queue is full. A client that has
stopped reading must not be able to stall the display server, and with it
everyone else's window. Losing a keystroke is a nuisance; a compositor wedged
behind one bad program is the failure this design exists to avoid.

## Input

There is one keyboard, so something has to decide whose keystroke this is. The
compositor decodes PS/2 set 1 scancodes once and routes the result to the
focused window, and the event carries *both* forms:

| Field | For |
| --- | --- |
| `ascii` | a client with no opinion -- the console's line editor |
| `scancode`, `extended`, `pressed` | a client with its own keymap -- Doom's, which is thirty years old and works |

Doom's C decoder was not changed at all. `user/doom/src/platform.rs`
re-encodes each event into the raw byte stream it already knew how to read:
`0xE0` prefix restored for an extended key, bit 7 set for a release.

Modifier keys are forwarded, not swallowed. Shift is how Doom runs and control
is how it fires; a compositor that only reported the modifier's *effect* on the
next character would have made the game unplayable.

The compositor shortcuts are:

| Shortcut | Action |
| --- | --- |
| Meta/Windows or Scroll Lock | Cycle keyboard focus |
| Meta+Tab | Cycle keyboard focus |
| Meta+Enter | Toggle the focused window between tiled and maximized mode |
| Meta+Q | Ask the focused client to close |

Meta is aliased to Scroll Lock for virtual machines that eat the Windows key.
The compositor swallows these shortcuts; all other keys go to the focused
client. A maximized surface keeps its full-screen-width buffer, while tiled
mode clips that buffer to the window's column, so the shell can use the whole
screen without changing the surface protocol.

## The layout is tiled by default

The screen is divided into `MAX_WINDOWS` columns once, at startup. Windows are
tiled into those columns by default. Each surface is allocated at the full
content width, which lets the compositor show the focused window maximized;
the surface is clipped to its column while tiled.

The compositor does not yet offer arbitrary drag-resize or mouse controls. The
protocol still reserves `wm_event::CONFIGURE` for future client-driven size
changes; maximize is implemented without replacing a live memory object.

## What building this uncovered

A `WmCreateSurface` was arriving at the compositor with the right length, the
right message kind, and garbage in its fields. The bytes were correct in the
sender's stack and wrong in the receiver's buffer.

The cause was not in the protocol, the IPC path, or the allocator. The compiler
had compiled the 16-byte struct copy into the obvious thing:

```asm
movups -0x180(%rbp),%xmm0     ; load the request from the stack
movups %xmm0,(%rax)           ; store it into a fresh heap page
```

The store was the first touch of a demand-paged page, so it faulted. The kernel
committed the page, clobbered `xmm0` with its own compiled code -- zeroing the
new frame, as it happens -- and returned to retry the instruction. The retry
stored the kernel's leftovers.

`docs/DOOM.md` had already named the underlying gap: the context switch saved
general-purpose registers and nothing else, and a program with live
floating-point state would have it corrupted. What that entry understated is
that *no floating point is required*. The compiler emits SSE for ordinary data
movement, so the bug reaches any program that copies a 16-byte value into a
page it has not touched yet -- silently, with no fault and no log.

The fix is 512 bytes reserved at the top of every kernel stack and an
`fxsave64` / `fxrstor64` pair in each of the three paths that cross the ring
boundary (`arch/x86_64/trap.rs`, `syscall/entry.rs`, `task/context.rs`). Kernel
-mode entries are exempt: the kernel runs with interrupts disabled, so no
kernel SSE operation can be interrupted part-way. See
`task::thread::FPU_AREA_SIZE` for the reasoning in place.
