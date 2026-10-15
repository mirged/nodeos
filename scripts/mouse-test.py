#!/usr/bin/env python3
"""Boot the graphic ISO, drive the mouse through QEMU, and assert what happened.

The kernel's self-tests and `scripts/run-tests.sh` stop at ring 3: they can say
that the compositor was launched, never that it drew the right pixels or that a
button press focused the window under it. This is the test that goes the rest of
the way. It boots the same ISO with a display and a PS/2 mouse, injects real
device traffic through QEMU's monitor, and then asks the two things that can
answer back:

  * the framebuffer, read back with `screendump` and searched for the arrow,
    which is the only place a pointer is visible at all; and
  * the compositor's own log, which names the windows it focuses, moves and
    maximizes.

Neither alone is enough. Pixels alone cannot tell a cursor that moved for the
right reason from one that was drawn in the wrong place, and the log alone is a
program reporting on itself. Together they close the loop: the log says what
should have happened to which window, and the pixels say it happened on screen.

What is checked, in order:

  1. the pointer appears, at the position the compositor logged for it, drawn
     in exactly the shape and colours `pointer.rs` says to draw;
  2. motion is linear, signed and reversible: four packets in one direction
     move it, four back return it to the same pixel;
  3. a click focuses the window under it, and a click on the desktop drops the
     focus again, with the title bars changing colour both ways;
  4. a double click maximizes, and again restores;
  5. a drag on a title bar moves the window to the other column, and back;
  6. a keystroke still reaches the focused window, so the pointer work did not
     break the keyboard that was already there.

The graphic boot runs one window (the console, with the shell inside it, in the
first column) and an empty column beside it, so the drag moves a window into a
column nothing else holds rather than trading places with a second window. The
swap in `move_to_slot` is what a second window would exercise; the boot that has
one is the Doom boot, which needs an IWAD this test does not carry.

Run it with `make test-mouse`. Needs python3 on the host, and QEMU's monitor:
the guest is driven entirely from outside, so nothing in NodeOS is built for the
test.
"""

import json
import os
import re
import shutil
import socket
import subprocess
import sys
import time

ROOT = os.path.dirname(os.path.dirname(os.path.abspath(__file__)))
BUILD = os.path.join(ROOT, "build")
ISO = os.path.join(BUILD, "nodeos.iso")
DISK = os.path.join(BUILD, "disk.img")
LOG = os.path.join(BUILD, "mouse-serial.log")
# Named for this process, so that a QEMU left behind by an interrupted run
# cannot make this one fail to bind -- a collision that looks exactly like the
# guest never booting.
SOCK = os.path.join(BUILD, f"mouse-qmp.{os.getpid()}.sock")
QEMU_LOG = os.path.join(BUILD, "mouse-qemu.log")
FRAMES = os.path.join(BUILD, "mouse-frames")

# Colours from `user/compositor/src/window.rs`, by value rather than by import:
# this is the outside of the system looking in, and a test that shared a
# constant with the code under test would not notice the constant changing.
DESKTOP = 0x181E26
FRAME_ACTIVE = 0x3366A0
FRAME_INACTIVE = 0x3A4048
CURSOR_EDGE = 0x000000
CURSOR_FILL = 0xFFFFFF
# The console's own background, from `user/console/src/surface.rs`. A
# maximized window whose surface really grew is painted this; one that did not
# still shows the desktop colour where the frame outran the old surface.
CONSOLE_BG = 0x101010

# How long a whole boot may take before the test gives up, in seconds.
BOOT_TIMEOUT = 90.0
# How long the compositor takes to react to a packet, before a screendump is
# expected to show it. Generous: it is a poll on a serial log, not a race.
SETTLE = 0.5


# ---------------------------------------------------------------------------
# The monitor
# ---------------------------------------------------------------------------

class Monitor:
    """A QMP connection, with the handful of commands this test needs."""

    def __init__(self, path, timeout=15.0):
        self.sock = socket.socket(socket.AF_UNIX, socket.SOCK_STREAM)
        deadline = time.time() + timeout
        while True:
            try:
                self.sock.connect(path)
                break
            except (FileNotFoundError, ConnectionRefusedError):
                if time.time() > deadline:
                    raise
                time.sleep(0.1)
        self.buf = b""

    def send(self, obj):
        self.sock.sendall((json.dumps(obj) + "\n").encode())

    def read_line(self, timeout=20.0):
        self.sock.settimeout(timeout)
        while b"\n" not in self.buf:
            chunk = self.sock.recv(65536)
            if not chunk:
                raise EOFError("the monitor closed")
            self.buf += chunk
        line, self.buf = self.buf.split(b"\n", 1)
        return line.decode(errors="replace").strip()

    def read_obj(self):
        while True:
            line = self.read_line()
            if not line:
                continue
            try:
                return json.loads(line)
            except json.JSONDecodeError:
                continue

    def greet(self):
        self.read_obj()
        self.cmd("qmp_capabilities")

    def cmd(self, name, **args):
        self.send({"execute": name, "arguments": args} if args else {"execute": name})
        return self.read_obj()

    def hmp(self, command):
        result = self.cmd("human-monitor-command", **{"command-line": command})
        return result.get("return", result)

    def move(self, dx, dy):
        return self.hmp(f"mouse_move {int(dx)} {int(dy)}")

    def button(self, mask):
        return self.hmp(f"mouse_button {int(mask)}")

    def screendump(self, path):
        self.hmp(f"screendump {path}")

    def key(self, name):
        return self.hmp(f"sendkey {name}")

    def close(self):
        try:
            self.cmd("quit")
        except Exception:
            pass
        self.sock.close()


# ---------------------------------------------------------------------------
# PPM reading
# ---------------------------------------------------------------------------

class Frame:
    """A screen, as pixels, with the two readers a test needs."""

    def __init__(self, path):
        with open(path, "rb") as handle:
            data = handle.read()
        if not data.startswith(b"P6"):
            raise ValueError(f"{path}: not a binary PPM")
        fields, offset = [], 2
        while len(fields) < 3:
            while offset < len(data) and data[offset : offset + 1].isspace():
                offset += 1
            if data[offset : offset + 1] == b"#":
                while data[offset : offset + 1] not in (b"\n", b""):
                    offset += 1
                continue
            start = offset
            while offset < len(data) and not data[offset : offset + 1].isspace():
                offset += 1
            fields.append(int(data[start:offset]))
        offset += 1  # the single whitespace that ends the header
        self.width, self.height, maxval = fields
        if maxval != 255:
            raise ValueError(f"{path}: maxval {maxval} is not 8-bit")
        self.pixels = data[offset:]
        self.path = path

    def at(self, x, y):
        """The 24-bit colour at (x, y), or None off the screen."""
        if x < 0 or y < 0 or x >= self.width or y >= self.height:
            return None
        index = (y * self.width + x) * 3
        r, g, b = self.pixels[index], self.pixels[index + 1], self.pixels[index + 2]
        return (r << 16) | (g << 8) | b

    def difference(self, other, rect):
        """How many pixels in `rect` differ between two frames.

        Exact, and fast enough to use on a whole window: rows are compared as
        byte slices, and only a row that differs is walked pixel by pixel. A
        sparse sample would be cheaper still and would miss a single glyph,
        which is exactly what the keystroke check is looking for.
        """
        x, y, w, h = rect
        changed = 0
        for row in range(y, min(y + h, self.height)):
            start = (row * self.width + x) * 3
            end = start + w * 3
            mine, theirs = self.pixels[start:end], other.pixels[start:end]
            if mine == theirs:
                continue
            for index in range(0, len(mine), 3):
                if mine[index : index + 3] != theirs[index : index + 3]:
                    changed += 1
        return changed

    def positions_of(self, colour):
        """Every pixel with a given colour."""
        r, g, b = (colour >> 16) & 0xFF, (colour >> 8) & 0xFF, colour & 0xFF
        wanted = bytes((r, g, b))
        found = []
        for y in range(self.height):
            row = self.pixels[y * self.width * 3 : (y + 1) * self.width * 3]
            start = 0
            while True:
                index = row.find(wanted, start)
                if index < 0:
                    break
                if index % 3 == 0:
                    found.append((index // 3, y))
                start = index + 1
        return found


def hexstr(colour):
    return "none" if colour is None else f"#{colour:06x}"


# ---------------------------------------------------------------------------
# The cursor's shape, read back out of the compositor's source
# ---------------------------------------------------------------------------

def cursor_pattern():
    """The arrow, cell by cell, as `pointer.rs` draws it.

    Parsed out of the source rather than copied into the test: the body table is
    the one place the shape is written down, and a test carrying its own copy
    would keep passing after the shape changed. The edge rule is repeated here
    -- white unless a pixel of the body touches the outside -- which is the one
    thing this test cannot read from the source, so it asserts the rule's
    *output*: a change to the rule that changed the drawn arrow would fail here.
    """
    source = open(os.path.join(ROOT, "user", "compositor", "src", "pointer.rs"), encoding="utf-8").read()
    match = re.search(r"const CURSOR_BODY: \[&str; [A-Za-z_0-9 ()]+ as usize\] = \[(.*?)\];", source, re.S)
    if not match:
        raise SystemExit("mouse-test: could not find CURSOR_BODY in pointer.rs")
    rows = re.findall(r'"([^"]*)"', match.group(1))
    if not rows:
        raise SystemExit("mouse-test: CURSOR_BODY is empty")

    def inside(x, y):
        return 0 <= y < len(rows) and 0 <= x < len(rows[y]) and rows[y][x] == "#"

    pattern = []
    for y, row in enumerate(rows):
        for x, cell in enumerate(row):
            if cell != "#":
                continue
            edge = not (inside(x - 1, y) and inside(x + 1, y) and inside(x, y - 1) and inside(x, y + 1))
            pattern.append((x, y, CURSOR_EDGE if edge else CURSOR_FILL))
    return pattern


def find_arrows(frame, pattern):
    """Every place on the screen where the arrow is drawn, tip position first.

    A list rather than one position, because an arrow that was drawn without
    taking the previous one off is a bug this test is here to catch: the screen
    would grow a copy of the cursor wherever the pointer had been, and a test
    that only looked for *an* arrow would happily report the oldest one as the
    current position.
    """
    whites = [cell for cell in pattern if cell[2] == CURSOR_FILL]
    anchor = whites[0] if whites else pattern[0]
    anchor_x, anchor_y, anchor_colour = anchor
    found = []
    for x, y in frame.positions_of(anchor_colour):
        tip_x, tip_y = x - anchor_x, y - anchor_y
        if tip_x < 0 or tip_y < 0:
            continue
        if all(frame.at(tip_x + cell_x, tip_y + cell_y) == cell_colour for cell_x, cell_y, cell_colour in pattern):
            found.append((tip_x, tip_y))
    return found


def find_cursor(frame, pattern):
    """Where the arrow's tip is, or None. The first match if there are more
    than one -- [`find_arrows`] is what a caller counts."""
    found = find_arrows(frame, pattern)
    return found[0] if found else None


# ---------------------------------------------------------------------------
# Reporting
# ---------------------------------------------------------------------------

class Report:
    def __init__(self):
        self.passed = 0
        self.failed = 0

    def check(self, ok, description, detail=""):
        suffix = f" -- {detail}" if detail else ""
        print(f"mouse-test: {'OK  ' if ok else 'FAIL'} {description}{suffix}", flush=True)
        if ok:
            self.passed += 1
        else:
            self.failed += 1
        return ok

    def note(self, text):
        print(f"mouse-test:      {text}", flush=True)


# ---------------------------------------------------------------------------
# The guest
# ---------------------------------------------------------------------------

def tail(text, lines=1):
    """The last few lines of log output, for a check that failed."""
    stripped = text.strip()
    if not stripped:
        return "no log output at all"
    return " | ".join(stripped.splitlines()[-lines:])


def plain_log():
    if not os.path.exists(LOG):
        return ""
    with open(LOG, "r", errors="replace") as handle:
        text = handle.read()
    return re.sub(r"\x1b\[[0-9;]*m", "", text)


def wait_for_log(patterns, timeout=BOOT_TIMEOUT):
    """Wait until every pattern has appeared in the serial log."""
    deadline = time.time() + timeout
    while time.time() < deadline:
        text = plain_log()
        if all(re.search(pattern, text) for pattern in patterns):
            return text
        time.sleep(0.2)
    return plain_log()


class Guest:
    """A booted NodeOS, and the geometry the compositor reported for it."""

    def __init__(self, report):
        for path in (LOG, SOCK):
            if os.path.exists(path):
                os.unlink(path)
        shutil.rmtree(FRAMES, ignore_errors=True)
        os.makedirs(FRAMES, exist_ok=True)

        argv = [
            "qemu-system-x86_64",
            "-machine", "q35", "-cpu", "qemu64,+pdpe1gb", "-smp", "1", "-m", "512M",
            "-no-reboot", "-no-shutdown", "-boot", "d",
            "-audiodev", "none,id=snd0", "-device", "AC97,audiodev=snd0",
            "-drive", f"file={DISK},if=virtio,format=raw",
            "-cdrom", ISO,
            "-serial", f"file:{LOG}",
            "-display", "none",
            "-qmp", f"unix:{SOCK},server,nowait",
        ]
        # QEMU's own output goes to a file rather than /dev/null: when the guest
        # does not come up, the reason is almost always in there, and a test that
        # throws it away fails with "the compositor never reported a geometry"
        # and no way to find out why.
        self.qemu_log = open(QEMU_LOG, "wb")
        self.proc = subprocess.Popen(argv, stdout=self.qemu_log, stderr=subprocess.STDOUT)
        self.monitor = None
        self.frames = 0
        self.report = report

        try:
            self.monitor = Monitor(SOCK)
            self.monitor.greet()

            # The compositor logs the screen it found; the test derives every
            # coordinate from that rather than repeating the numbers, so a change
            # of resolution or column count does not silently move every probe
            # point.
            text = wait_for_log(
                [r"pointer ready at \d+,\d+", r"compositor: client \d+ surface \d+ in column \d+"]
            )
            geometry = re.search(r"compositor: (\d+)x(\d+) screen, (\d+) columns of (\d+)x(\d+)", text)
            if not geometry:
                raise SystemExit("mouse-test: the compositor never reported a screen geometry")
            width, height, columns, content_w, content_h = (int(value) for value in geometry.groups())
            self.screen = (width, height)
            self.columns = columns
            # The title bar height is the one number the layout does not print;
            # it falls out of the two it does, since the content area is the
            # frame minus the title bar and one border pixel.
            self.title_h = height - content_h - 1
            self.column_w = width // columns
            self.content_w = content_w

            self.clients = self._clients(text)
        except BaseException:
            self.close()
            raise

    def _clients(self, text):
        """`(client, surface, column)` for every window the compositor placed."""
        return [
            (int(client), int(surface), int(column))
            for client, surface, column in re.findall(
                r"compositor: client (\d+) surface (\d+) in column (\d+)", text
            )
        ]

    def refresh_clients(self):
        self.clients = self._clients(plain_log()) or self.clients
        return self.clients

    def column_of_client(self, client):
        for owner, _, column in self.clients:
            if owner == client:
                return column
        return None

    def title_point(self, column):
        """A point in the middle of a column's title bar."""
        return (column * self.column_w + self.column_w // 2, self.title_h // 2)

    def content_point(self, column):
        return (column * self.column_w + self.column_w // 2, self.title_h + 100)

    def snap(self, name):
        self.frames += 1
        path = os.path.join(FRAMES, f"{self.frames:02d}-{name}.ppm")
        self.monitor.screendump(path)
        return Frame(path)

    def cursor(self, frame, pattern):
        return find_cursor(frame, pattern)

    def only_arrow(self, frame, pattern, report, what):
        """Where the arrow is, checking that it is the only one there."""
        arrows = find_arrows(frame, pattern)
        report.check(len(arrows) == 1, f"the screen shows exactly one arrow ({what})", f"{len(arrows)} at {arrows}")
        return arrows[0] if arrows else None

    def log_since(self, marker):
        """Everything the log gained after `marker` (a byte offset)."""
        with open(LOG, "r", errors="replace") as handle:
            handle.seek(marker)
            return re.sub(r"\x1b\[[0-9;]*m", "", handle.read())

    def log_size(self):
        return os.path.getsize(LOG) if os.path.exists(LOG) else 0

    def close(self):
        """Stop the guest. Idempotent, and safe to call before it started."""
        if self.monitor is not None:
            try:
                self.monitor.close()
            except Exception:
                pass
            self.monitor = None
        if self.proc.poll() is None:
            self.proc.kill()
        try:
            self.proc.wait(timeout=15)
        except Exception:
            pass
        if not self.qemu_log.closed:
            self.qemu_log.close()


# ---------------------------------------------------------------------------
# Gestures
# ---------------------------------------------------------------------------

def move_to(guest, pattern, target, report, tolerance=1, rounds=8):
    """Move the pointer to a screen position, a screendump at a time.

    The scale between a monitor `mouse_move` and a screen pixel is QEMU's
    business, not this test's, so rather than assume it the test measures: it
    asks the framebuffer where the arrow is, converts the remainder with the
    scale it has seen so far, and repeats. That also makes the move an
    end-to-end one -- every step of it travels through the emulated controller,
    the kernel's driver, and the compositor's decoder.
    """
    scale = 1.0
    for _ in range(rounds):
        frame = guest.snap("move")
        here = guest.cursor(frame, pattern)
        if here is None:
            return None
        dx, dy = target[0] - here[0], target[1] - here[1]
        if abs(dx) <= tolerance and abs(dy) <= tolerance:
            return here
        step_x, step_y = int(round(dx / scale)), int(round(dy / scale))
        # A sign is all that is needed once the scale is right; a zero here
        # would mean "arrived" to QEMU and the loop would never finish.
        step_x = step_x or (1 if dx > 0 else -1)
        step_y = step_y or (1 if dy > 0 else -1)
        guest.monitor.move(step_x, step_y)
        time.sleep(SETTLE)
        after = guest.cursor(guest.snap("scaled"), pattern)
        if after is not None and (after[0] != here[0] or after[1] != here[1]):
            moved_x, moved_y = after[0] - here[0], after[1] - here[1]
            measured = []
            if step_x and moved_x:
                measured.append(moved_x / step_x)
            if step_y and moved_y:
                measured.append(moved_y / step_y)
            if measured:
                scale = sum(measured) / len(measured)
                report.note(f"the device reports one count as {scale:.3f} px")
    return guest.cursor(guest.snap("move-final"), pattern)


def click(guest, hold=0.15, gap=SETTLE):
    """Press and release once. `gap` is the pause before whatever comes next,
    which has to be short for the second click of a double click."""
    guest.monitor.button(1)
    time.sleep(hold)
    guest.monitor.button(0)
    time.sleep(gap)


def main():
    report = Report()
    if not os.path.exists(ISO):
        raise SystemExit(f"mouse-test: {ISO} not found; run 'make' first")
    if not shutil.which("qemu-system-x86_64"):
        raise SystemExit("mouse-test: qemu-system-x86_64 not found")

    pattern = cursor_pattern()
    guest = Guest(report)
    try:
        text = plain_log()

        # 1. The decoder's own vectors, then the arrow.
        vectors = re.search(r"pointer decoder: (\d+) vectors passed, (\d+) failed", text)
        report.check(
            vectors is not None and vectors.group(2) == "0",
            "the pointer decoder passed its own test vectors",
            vectors.group(0) if vectors else "no decoder line in the log",
        )
        report.check("no PS/2 mouse" not in text, "a PS/2 mouse answered at bring-up")

        start = re.search(r"pointer ready at (\d+),(\d+)", text)
        start = (int(start.group(1)), int(start.group(2)))
        frame = guest.snap("start")
        found = guest.only_arrow(frame, pattern, report, "at rest")
        report.check(
            found == start,
            "the arrow is on the screen where the compositor says it is",
            f"logged {start}, found {found}",
        )

        # 2. Motion: linear, signed, reversible.
        before = found
        for _ in range(4):
            guest.monitor.move(37, 23)
            time.sleep(0.05)
        time.sleep(SETTLE)
        moved = guest.only_arrow(guest.snap("moved"), pattern, report, "after moving")
        delta = (moved[0] - before[0], moved[1] - before[1]) if moved else None
        report.check(
            delta is not None and delta[0] > 0 and delta[1] > 0,
            "four packets right and down moved the arrow right and down",
            f"{delta}",
        )
        # The two axes are proportional to what was sent: this is what a
        # sign-extension bug breaks, and it breaks it asymmetrically -- the
        # up/left half of the plane reads as an enormous jump, not a small one.
        ratio_ok = delta is not None and delta[0] > 0 and abs(delta[0] / max(delta[1], 1) - 37 / 23) < 0.05
        report.check(ratio_ok, "the two axes moved in the ratio they were sent", f"{delta} for (148, 92) sent")

        for _ in range(4):
            guest.monitor.move(-37, -23)
            time.sleep(0.05)
        time.sleep(SETTLE)
        back = guest.only_arrow(guest.snap("returned"), pattern, report, "after returning")
        report.check(
            back == before,
            "four packets back returned the arrow to the same pixel",
            f"left {before}, returned to {back}",
        )

        # 3. Click to focus -- and click away to drop it.
        #
        # Every gesture from here on is read two ways: the compositor's log says
        # which window it decided to focus, move or maximize, and the title bars
        # say whether the screen agrees. Neither is sufficient on its own.
        clients = guest.refresh_clients()
        window_column = min(entry[2] for entry in clients)
        window = next(entry for entry in clients if entry[2] == window_column)
        other = 1 - window_column if guest.columns > 1 else window_column
        occupied = any(entry[2] == other for entry in clients)

        def probe(column):
            """A point in a column's title bar, away from its text."""
            return (column * guest.column_w + 2, 1)

        report.note(f"column {window_column} holds client {window[0]} surface {window[1]}")

        if not occupied:
            # A column with no window in it is the desktop, and clicking it is
            # how the focus is given up: there is nothing under the pointer to
            # give it to.
            marker = guest.log_size()
            point = (other * guest.column_w + 150, guest.screen[1] // 2)
            report.check(move_to(guest, pattern, point, report) is not None, "the pointer reached the empty column")
            click(guest)
            gained = guest.log_since(marker)
            report.check("focus dropped" in gained, "a click on the desktop dropped the focus", tail(gained))
            frame = guest.snap("unfocused")
            report.check(
                frame.at(*probe(window_column)) == FRAME_INACTIVE,
                "the window's title bar went unfocused",
                hexstr(frame.at(*probe(window_column))),
            )
        else:
            report.note("every column holds a window; the desktop has nowhere to be clicked")

        title = guest.title_point(window_column)
        marker = guest.log_size()
        report.check(move_to(guest, pattern, title, report) is not None, "the pointer reached the title bar")
        click(guest)
        gained = guest.log_since(marker)
        report.check(
            f"focus -> client {window[0]} surface {window[1]}" in gained,
            "a click focused the window under the pointer",
            tail(gained),
        )
        frame = guest.snap("focused")
        report.check(
            frame.at(*probe(window_column)) == FRAME_ACTIVE,
            "the focused window's title bar is the focused colour",
            hexstr(frame.at(*probe(window_column))),
        )

        # 4. Double click maximizes, a second double click restores.
        marker = guest.log_size()
        time.sleep(0.5)  # long enough that these are not read as one triple
        click(guest, hold=0.05, gap=0.05)
        click(guest, hold=0.05, gap=0.05)
        time.sleep(SETTLE)
        gained = guest.log_since(marker)
        report.check("maximized" in gained, "a double click maximized the window", tail(gained))

        frame = guest.snap("maximized")
        # The maximized frame is the whole screen, so the other column's title
        # band is now the maximized window's -- even though that column holds no
        # window, or holds one that is no longer showing its own frame.
        report.check(
            frame.at(*probe(other)) == FRAME_ACTIVE,
            "the maximized frame covers the whole screen",
            hexstr(frame.at(*probe(other))),
        )

        # Maximizing has to grow the window *inside* the frame, not just the
        # frame: the compositor replaces the client's memory object with a
        # screen-sized one and sends CONFIGURE, and the console repaints. Sample
        # near the bottom of the area that belonged to the empty column -- far
        # from where any text lands -- and look for the console's background
        # rather than the desktop showing through.
        content_probe = (other * guest.column_w + guest.column_w // 2, guest.screen[1] - 50)
        report.check(
            frame.at(*content_probe) == CONSOLE_BG,
            "the maximized window's content grew to fill the frame",
            hexstr(frame.at(*content_probe)),
        )

        marker = guest.log_size()
        time.sleep(0.5)
        click(guest, hold=0.05, gap=0.05)
        click(guest, hold=0.05, gap=0.05)
        time.sleep(SETTLE)
        gained = guest.log_since(marker)
        report.check("restored" in gained, "a second double click restored it", tail(gained))
        frame = guest.snap("restored")
        report.check(
            frame.at(*probe(other)) != FRAME_ACTIVE,
            "the other column is its own again",
            hexstr(frame.at(*probe(other))),
        )

        # 5. Drag the title bar into the other column, and back.
        #
        # The drag is the one gesture that has to work across the pointer
        # leaving the window it started on: the compositor keeps sending motion
        # to the window that took the press, which is what lets the window
        # follow the pointer out of its own column.
        marker = guest.log_size()
        time.sleep(0.5)
        report.check(move_to(guest, pattern, guest.title_point(window_column), report) is not None, "the pointer is back on the title bar")
        guest.monitor.button(1)
        time.sleep(0.1)
        report.check(move_to(guest, pattern, guest.content_point(other), report) is not None, "the pointer dragged into the other column")
        guest.monitor.button(0)
        time.sleep(SETTLE)
        gained = guest.log_since(marker)
        report.check(
            re.search(rf"client {window[0]} surface {window[1]} moved to column {other}", gained) is not None,
            f"dragging the title bar moved the window to column {other}",
            tail(gained),
        )
        frame = guest.snap("dragged")
        report.check(
            frame.at(*probe(other)) == FRAME_ACTIVE,
            f"the dragged window is in column {other} and still focused",
            hexstr(frame.at(*probe(other))),
        )
        report.check(
            frame.at(*probe(window_column)) == (FRAME_INACTIVE if occupied else DESKTOP),
            "the column it left shows what is there now",
            hexstr(frame.at(*probe(window_column))),
        )

        marker = guest.log_size()
        time.sleep(0.5)
        report.check(move_to(guest, pattern, guest.title_point(other), report) is not None, "the pointer is on the moved window's title bar")
        guest.monitor.button(1)
        time.sleep(0.1)
        report.check(move_to(guest, pattern, guest.content_point(window_column), report) is not None, "the pointer dragged back")
        guest.monitor.button(0)
        time.sleep(SETTLE)
        gained = guest.log_since(marker)
        report.check(
            re.search(rf"client {window[0]} surface {window[1]} moved to column {window_column}", gained) is not None,
            "dragging it back returned it to the column it started in",
            tail(gained),
        )
        frame = guest.snap("dragged-back")
        report.check(
            frame.at(*probe(window_column)) == FRAME_ACTIVE,
            "and it is focused there",
            hexstr(frame.at(*probe(window_column))),
        )
        guest.only_arrow(frame, pattern, report, "after two drags")

        # 6. The keyboard still reaches a window with a pointer in the system.
        #
        # The console echoes a character only while a client is waiting to read
        # a line, which is the shell's prompt: so this is a check on the whole
        # path -- emulated keyboard, IRQ1, the kernel's queue, the compositor's
        # scancode decoder, the focus, and the console's line editor -- read off
        # the pixels of the window that has the focus.
        content = (window_column * guest.column_w + 1, guest.title_h, guest.content_w, guest.screen[1] - guest.title_h - 1)
        before_typing = guest.snap("typing-before")
        guest.monitor.key("a")
        time.sleep(SETTLE)
        after_typing = guest.snap("typing-after")
        changed = after_typing.difference(before_typing, content)
        report.check(changed > 0, "a keystroke still changed what the focused window is showing", f"{changed} pixels")

        text = plain_log()
        report.check("panic" not in text.lower(), "nothing panicked along the way")
        report.check(
            "compositor: client" not in text.split("in column")[-1] or "disconnect" not in text.lower(),
            "no client disconnected during the test",
        )
    finally:
        guest.close()

    print(f"mouse-test: {'PASS' if report.failed == 0 else 'FAIL'} -- {report.passed} checks passed, {report.failed} failed")
    if report.failed:
        print("--- last 40 lines of the serial log ---")
        print("\n".join(plain_log().splitlines()[-40:]))
        if os.path.exists(QEMU_LOG):
            with open(QEMU_LOG, "r", errors="replace") as handle:
                qemu_tail = handle.read()[-1500:]
            if qemu_tail.strip():
                print("--- QEMU ---")
                print(qemu_tail)
        return 1
    print(f"mouse-test: frames left in {FRAMES}")
    return 0


if __name__ == "__main__":
    sys.exit(main())
