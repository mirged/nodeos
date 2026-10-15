# The window system

Before this, a NodeOS program that wanted to draw was handed the framebuffer
device -- and with it the whole screen, every other program's output included.
That is why `console` and `doom` could not run at the same time: there was one
screen and one keyboard, and a boot configuration had to decide who got them.

`compositor` is the process that holds them now, and it is the only one that
does. Everything else gets a memory object the size of its own window and a
channel to send damage on. Doom and the shell run side by side not because they
agreed to share, but because neither can reach the other's pixels.

```
   GRUB
     |  module2 /boot/compositor.nxf  console.nxf  shell.nxf
     v
  kernel/src/launch.rs
     |  Display + Input  ->  compositor   (framebuffer, keyboard, mouse)
     |  Memory + Channel ->  console      (a channel to the compositor)
     |  session channels ->  shell        (console, fs, compositor, audio)
     v
  nodeos> run /bin/doom.nxf
     v
  compositor (ring 3)                 doom gets one of shell's compositor
     +-- window.rs     columns, frames, title bars
     +-- keyboard.rs   PS/2 set 1 -> events, decoded once for everyone
     +-- pointer.rs    PS/2 mouse packets -> motion, and the arrow on screen
     +-- main.rs       the protocol, and who the next keystroke or click is for
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
| Meta/Windows or Scroll Lock (tap) | Cycle keyboard focus |
| Meta+Tab | Cycle keyboard focus |
| Meta+Enter | Toggle the focused window between tiled and maximized mode |
| Meta+Q | Ask the focused client to close |

Scroll Lock is a full alias for Meta, not a second hotkey: both are tracked as
the same modifier, so Scroll Lock+Tab, Scroll Lock+Enter and Scroll Lock+Q are
the same shortcuts Meta+Tab, Meta+Enter and Meta+Q are. That alias exists for
virtual machines that eat the Windows key. A *tap* of either key -- press and
release with nothing in between -- cycles the focus; holding either and pressing
another key makes it a modifier. The compositor swallows these shortcuts; all
other keys go to the focused client.

## The pointer

The mouse is the one input device whose protocol is not merely transport. A PS/2
mouse sends three bytes per movement: two signed deltas, and a byte of flags
carrying their sign bits, the button states and two overflow bits. Nothing in
those bytes says where the pointer *is*. They are a velocity, the position is the
sum of everything that came before, and so the compositor owns it and a client
only ever hears the difference.

The decoding lives in `user/compositor/src/pointer.rs` rather than in the kernel,
for the same reason scancodes do: the kernel moves bytes from the controller to
the process that owns the device, and what the bytes *mean* is a question for
whoever is drawing the result.

| Field | For |
| --- | --- |
| `x`, `y` | where the event happened, in the window's own coordinates |
| `buttons` | the whole button state after the event, so a client needs no history |
| `button`, `pressed` | which button changed, and which way |

`x` and `y` are signed and are deliberately *not* clamped to the window. A client
that took a button press keeps receiving motion after the pointer has left it --
which is what lets a drag be followed out of its own edge -- and its coordinates
go negative on the way out rather than piling up at the border.

Two parts of the decoder are policy rather than parsing, and both were found by
its own test vectors rather than by reasoning:

* **A byte lost inside a packet is undetectable from the bytes.** Deltas carry no
  framing, so the rest of a packet is indistinguishable from the start of the
  next one and the stream is read one byte out of step from then on. The only
  signal is time: a PS/2 mouse in the mode this driver leaves it in sends a packet
  every ten milliseconds, so a hundred milliseconds of quiet means a byte went
  missing -- the kernel's input queue drops its oldest byte when it fills, since
  an interrupt handler may never block -- and the decoder is reset on that gap.
* **An overflowed axis is dropped, not clamped.** The device sets a flag when its
  counter ran out, and the value in the packet is then not the movement that
  happened; a clamped value would tell the compositor the pointer went somewhere
  it never went. The other axis and the buttons are unaffected and still
  delivered: "the button went down while the mouse was flung across the desk" is
  worth reporting.

The decoder is checked against a dozen hand-written packet streams before it is
trusted with a real one, and the compositor refuses to serve a display if any of
them fails. Hand-written, not generated: what is being checked is a hand-written
reading of somebody else's specification, and a generator would encode the same
misreading twice. Three of those vectors failed the first time they ran, and two
of the failures were real bugs in the decoder.

### The arrow

There is no cursor layer, no hardware sprite and no transparency, so the arrow is
drawn into the framebuffer like any other pixel and the pixels it covers are
saved first. Every function in the compositor that writes the framebuffer takes
the arrow off before it and paints it after: a blit that landed under the cursor
would otherwise be undone by the cursor's own saved pixels on the next move, and
the screen would grow a copy of the arrow wherever the pointer had been.

That failure is not hypothetical. `paint` now removes the old arrow before
drawing the new one, and what found it was the test that counts the arrows on the
screen rather than looking for *an* arrow: with a trail, a test that looked for
one would report the oldest copy as the current position and pass.

### Focus, drags and clicks

| Gesture | What happens |
| --- | --- |
| Click a window | It takes the keyboard focus |
| Click the desktop | The focus goes to nobody, and no title bar is highlighted |
| Drag a title bar | The window follows the pointer, into whichever column the pointer is in |
| Double click a title bar | Maximize, or put back |

A press gives the window under it the *implicit grab*: motion and the matching
release keep going to that window after the pointer has left it. That is a
compositor decision rather than a protocol one -- a client is never told the
pointer left, only where it went -- and it is what makes dragging a window past
its own edge work at all.

Dragging a window into a column another window holds hands that column over
instead of stacking the two: a fixed set of columns has no third place for a
window to go, and the exchange is what makes the gesture reversible.

## The layout is tiled by default

The screen is divided into `MAX_WINDOWS` columns, and a window created while
tiled is allocated a surface the size of its column. The last column absorbs the
remainder when the screen is not an exact multiple of the column count: a strip
of screen that no column owns is a strip no repaint ever paints.

Maximizing replaces that surface. The compositor allocates a new memory object
the size of the whole content area, swaps its own mapping of the window's pixels,
and sends the client a `wm_event::CONFIGURE` carrying the new geometry and a
handle to the new object; restoring does the same back at column size. A client
that handles `CONFIGURE` -- `console` and `doom` both do -- maps the new object
and redraws, so the picture grows with the frame. A client that ignores it keeps
drawing into an object the compositor no longer reads, which is why a resize and
the `CONFIGURE` that announces it have to arrive together.

The console has no scrollback, so a resize clears it; Doom re-reads the geometry
and recomputes its integer scale on its next frame. Arbitrary drag-resize would
use the same event.

## Testing the pointer

A cursor is not something a serial log can show, so the pointer is checked by
`make test-mouse`: `scripts/mouse-test.py` boots the graphic ISO with a display,
drives the mouse through QEMU's monitor, and then asks the two things that can
answer back.

* The **framebuffer**, read back with `screendump` and searched for the arrow --
  a search pattern parsed out of `pointer.rs` itself, so the test cannot keep
  passing after the shape changed. Counting the arrows it finds is a check of
  its own, and is how the trail bug was found.
* The **compositor's log**, which names the window it focused, moved or
  maximized.

Neither is enough alone. Pixels alone cannot tell an arrow that moved for the
right reason from one drawn in the wrong place, and the log alone is a program
reporting on itself. Together they close the loop: the log says what should have
happened to which window, and the pixels say it happened on screen.

The test also refuses to assume the scale between a monitor `mouse_move` and a
screen pixel, which is QEMU's business rather than NodeOS's: it moves, reads back
where the arrow landed, converts the remainder and repeats, so every step travels
through the emulated controller, the kernel's PS/2 driver and the compositor's
decoder before anything is asserted. It finishes by typing a character and
requiring the focused window's pixels to change -- the only end-to-end test the
keyboard path has.

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
