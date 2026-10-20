# Console and shell

The boot path is `display -> console -> shell`. `display` owns the framebuffer
and PS/2 keyboard. It grants full-screen shared-memory surfaces to console and
explicitly launched applications. It contains no layout, window decoration,
mouse input, desktop, taskbar or launcher UI. The `WM_*` names in the wire ABI
are retained for existing applications; they now describe full-screen surfaces.

Only the active surface's damage is copied to the framebuffer. Commits from
inactive clients are acknowledged without touching the display. When a client
exits, its surfaces are dropped and the previous full-screen session is restored.
Input is polled before frame messages, preventing a fast rendering client from
starving the keyboard. Meta+Q asks an application to close; the base console is
protected from that shortcut. Meta/Scroll Lock cycles sessions.

`libnode::session::launch` reads NXF data directly into a VMO in 32 KiB chunks
and introduces a private display endpoint for each child. The shell waits on
`PROCESS_TERMINATED`, giving foreground apps exclusive use of the console input
path and preventing overlapping prompts. Process exit status is not currently
exposed by the ABI; this is completion waiting, not POSIX job control.

The text grid stores visible cells and at most 512 history rows, including their
colours. An underline cursor is restored before each edit and repainted after it.
Shift+Page Up/Down changes only the viewport, preserving the live insertion
point. New text returns to live output. Clear resets text, history and cursor.

The line editor tracks its origin as text wraps and the terminal scrolls.
History recalls erase any old suffix. Ctrl+A/E/U/K/W/C/L, Delete and the usual
navigation keys are interpreted in the shell, with Control checked as a modifier
rather than assuming control bytes arrive from the keyboard. Completion includes
packaged NXF names from `/bin`, in addition to builtins and filesystem paths.
Input is capped at 2048 characters and history at 128 adjacent-distinct entries.

The parser first validates the whole input. Single quotes are literal; double
quotes accept escaped characters plus \n and \t; unquoted backslash escapes one
character. Newlines and semicolons separate commands, and comments start only
at word boundaries. Unfinished quotes/escapes and unsupported operators fail
before any commands execute. Command files are UTF-8, at most 64 KiB and eight
levels deep. This deliberately does not claim pipes, redirection, environment
expansion, conditional execution, background jobs or a Unix process model.

`cp` validates source and destination before truncation, refuses self-copies,
and requires `-f` for replacement. It copies in 32 KiB chunks and checks short
writes. Copy failures retain the source; a destination created during a failed
copy can contain a partial result. `grep` scans chunks and keeps line fragments
across reads; one line is bounded to 32 KiB. `more` also streams chunks and
counts both wraps and newlines before pausing for input.

Checks: `make test-shell`, `make test-shell-parser`, `make test-fat-read` and
`make test-terminal`. The QEMU workflow test uses an isolated disk and validates
saved bytes with mtools after the guest shuts down.

## Readability and text size

The terminal uses checked-in grayscale glyph atlases derived from DejaVu Sans
Mono, with its redistribution license in `user/libdraw/src/terminal-font/`.
The normal size uses 16-pixel glyphs in 10x22 cells, small uses 13-pixel glyphs
in 8x18 cells, and large uses 20-pixel glyphs in 13x26 cells. A 16-pixel inset
keeps text off the framebuffer edges. The original 8x8 font remains available
for existing apps; the shell console uses the new font independently.

`font small|normal|large` sends `CONSOLE_SET_FONT` and receives updated console
geometry. Existing coloured cells are retained and wrapped to the new column
count, with the oldest history trimmed at the existing bound. The line editor
reads geometry again before each command. Text size changes last for the current
console session. Atlas regeneration uses `scripts/build-terminal-font.py` and
system FreeType; ordinary builds need neither host fonts nor FreeType.

Only SGR text styling is interpreted: normal/reset, emphasis, muted text and
standard foreground/background colours. Incomplete SGR sequences are retained
across IPC writes and occupy no text cells. Unknown CSI sequences are ignored.
Scrolling moves only the text viewport, preserving the inset margins. Cursor
and damage rectangles use the active font's cell metrics throughout.

Help groups related commands and accepts individual command names. File argument
counts are checked before execution, and common errors use plain-language text.
Directory listings put folders first and show sizes in B/KiB/MiB. A second Tab
lists ambiguous completions without discarding the line being edited. Directory
completion adds `/`, and Ctrl+Left/Right and Ctrl+D support word movement and
forward deletion.
