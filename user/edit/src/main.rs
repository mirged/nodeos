//! `edit` -- a small full-screen text editor.
//!
//! It is a compositor client like `console` and `doom`: it holds a channel to
//! `compositor`, a shared memory object for its window, and the filesystem
//! channel the launcher granted it. It owns no device and no framebuffer; what
//! appears on screen is a rectangle it draws into and then commits, and every
//! keystroke arrives as a `WM_EVENT` on the same channel.
//!
//! The whole program is deliberately one screenful of state: a list of lines,
//! a cursor, a scroll offset, and a dirty bit. There is no undo, no selection
//! and no horizontal scrolling, so nothing here needs a data structure more
//! elaborate than a `Vec<String>` -- and the failure mode of indexing one of
//! those out of bounds is a panic, which is why every cursor operation clamps
//! before it reads.

#![no_std]
#![no_main]

extern crate alloc;

use alloc::string::String;
use alloc::vec::Vec;

use libdraw::{Canvas, Rgb, GLYPH_SIZE};
use libnode::abi::message::MessageFlags;
use libnode::abi::proto::{self, wm_event};
use libnode::abi::types::{MapFlags, MapWindow};
use libnode::mem::{Mapping, Vmo};
use libnode::{println, Channel, Env, Error, Message};

libnode::main! {
    fn main(env: Env) -> i32 {
        run(env)
    }
}

/// A glyph cell is `GLYPH_SIZE` pixels square; every coordinate below is a
/// multiple of this, and spelling it once keeps the casts in one place.
const CELL: u32 = GLYPH_SIZE as u32;

/// Content size the editor asks for. Like the console's request, this is what
/// would be comfortable rather than a demand -- the compositor decides.
const WANTED_WIDTH: u32 = 640;
/// See [`WANTED_WIDTH`].
const WANTED_HEIGHT: u32 = 480;

/// The console's colours, so the editor looks like the rest of the system.
const BACKGROUND: Rgb = 0x0010_1010;
/// See [`BACKGROUND`].
const FOREGROUND: Rgb = 0x00e0_e0e0;
/// A muted bar behind the status line, to separate it from the text.
const STATUS_BACKGROUND: Rgb = 0x0020_2040;

/// Spaces a tab inserts. The buffer stores the spaces themselves rather than a
/// tab character, because the font has no tab glyph and a column count that
/// depended on the byte before it would complicate every cursor calculation.
const TAB_WIDTH: usize = 4;

/// Bytes per filesystem read. `/fs` caps a request at 32 KiB and answers with
/// what fits, so there is no point asking for more.
const READ_CHUNK: usize = 32 * 1024;
/// Bytes per filesystem write, for the same reason.
const WRITE_CHUNK: usize = 32 * 1024;

/// Where a fileless edit session starts. The name is conventional; nothing
/// creates the file until the first save.
const DEFAULT_PATH: &str = "/UNTITLED.TXT";

fn run(mut env: Env) -> i32 {
    // The shell hands every child its session capabilities in a fixed order:
    // console, filesystem, compositor, audio. `take_extra(0)` pops from the
    // front, so each call yields the next one. The editor has no use for the
    // console or the mixer; it discards the first and never reaches the last.
    let _console = env.take_extra(0);
    let Some(files_handle) = env.take_extra(0) else {
        println!("edit: no filesystem connection granted; exiting");
        return 1;
    };
    let Some(wm_handle) = env.take_extra(0) else {
        println!("edit: no compositor connection granted; exiting");
        return 1;
    };
    let files = Channel::from_handle(files_handle);
    let wm = Channel::from_handle(wm_handle);

    // `argv[0]` is the resolved program path; `argv[1]`, if any, is the file.
    let path = match env.args().get(1) {
        Some(path) if !path.is_empty() => path.clone(),
        _ => String::from(DEFAULT_PATH),
    };

    let mut editor = Editor::new(path.clone());
    editor.lines = load(&files, &path);

    let mut screen = match create_surface(&wm, &path) {
        Some(screen) => screen,
        None => return 1,
    };

    println!("edit: editing {} ({} lines)", path, editor.lines.len());
    render(&mut screen, &mut editor, &wm);

    loop {
        // Replies to our own `WM_COMMIT` calls are correlated and consumed by
        // `call_with`, so anything that reaches here is an unsolicited event.
        let Ok(message) = wm.receive() else { continue };
        if message.kind != proto::WM_EVENT {
            continue;
        }
        let Some(event) = proto::decode::<proto::WmEvent>(&message.payload) else { continue };

        match event.kind {
            wm_event::KEY if event.pressed != 0 => {
                on_key(&mut editor, &event, &files);
                render(&mut screen, &mut editor, &wm);
            }
            // The compositor replaced the surface: the window was maximized or
            // restored. Its new memory object travels in the message, so the
            // canvas is rebuilt around the new geometry and repainted. The text
            // lives in `editor`, so unlike a terminal with no scrollback this
            // resize loses nothing.
            wm_event::CONFIGURE => {
                let Some(handle) = message.handles.into_iter().next() else { continue };
                let (width, height) = (event.width, event.height);
                let pitch = width * 4;
                let size = pitch as u64 * height as u64;
                let vmo = Vmo::from_handle(handle, size);
                let mapping = vmo.map(
                    0,
                    size,
                    MapFlags::READ.or(MapFlags::WRITE).or(MapFlags::SHARED).or(MapFlags::POPULATE),
                    MapWindow::Shared,
                );
                let Ok(mapping) = mapping else {
                    println!("edit: failed to map the resized surface");
                    continue;
                };
                // SAFETY: the mapping covers the whole object, whose size is
                // exactly `pitch * height`, and nothing else here maps it.
                unsafe { screen.replace(vmo, mapping, width, height, pitch) };
                render(&mut screen, &mut editor, &wm);
            }
            wm_event::CLOSE => libnode::exit(0),
            // Focus changes need no action, and the editor has no selection or
            // scrollbar for the pointer to act on.
            _ => {}
        }
    }
}

// ---------------------------------------------------------------------------
// The window
// ---------------------------------------------------------------------------

/// The editor's surface: the memory object, its mapping, and a canvas over it.
///
/// The memory object and the mapping are held only so they outlive the canvas.
/// `Canvas` is a borrowed view with no `Drop`, so dropping either here would
/// leave it pointing at address space that is no longer mapped.
struct Screen {
    _vmo: Vmo,
    _mapping: Mapping,
    canvas: Canvas,
    id: u32,
}

impl Screen {
    /// Wrap a mapped surface memory object.
    ///
    /// # Safety
    /// `mapping` must cover at least `info.pitch * info.height` bytes of
    /// `vmo`, and nothing else in this process may write it.
    unsafe fn new(vmo: Vmo, mapping: Mapping, info: &proto::WmSurfaceInfo) -> Self {
        // SAFETY: the caller guarantees the mapping covers the geometry, and
        // the fields below keep it alive for as long as the canvas.
        let canvas = unsafe {
            Canvas::from_raw(mapping.base() as *mut u8, info.width, info.height, info.pitch)
        };
        Self { _vmo: vmo, _mapping: mapping, canvas, id: info.surface }
    }

    /// Adopt a new surface memory object, as a `CONFIGURE` event asks.
    ///
    /// The old mapping is unmapped before its object is released, so the frames
    /// are never freed while still translated.
    ///
    /// # Safety
    /// `mapping` must cover at least `pitch * height` bytes of `vmo`, and no
    /// other mapping in this process may write it.
    unsafe fn replace(&mut self, vmo: Vmo, mapping: Mapping, width: u32, height: u32, pitch: u32) {
        // SAFETY: the caller guarantees the mapping covers the geometry, and
        // `_mapping` below keeps it alive for as long as the canvas.
        let canvas = unsafe { Canvas::from_raw(mapping.base() as *mut u8, width, height, pitch) };
        self._mapping = mapping;
        self._vmo = vmo;
        self.canvas = canvas;
    }
}

/// Ask the compositor for a window and map the memory object it replies with.
fn create_surface(wm: &Channel, title: &str) -> Option<Screen> {
    let request = proto::Encoded::new(proto::WmCreateSurface {
        width: WANTED_WIDTH,
        height: WANTED_HEIGHT,
        flags: 0,
        _reserved: 0,
    });
    // The title follows the struct as UTF-8; the compositor reads whatever is
    // left of the payload.
    let mut payload = Vec::from(request.bytes());
    payload.extend_from_slice(title.as_bytes());

    let Ok(mut reply) = wm.call(Message::new(proto::WM_CREATE_SURFACE).with_payload(&payload)) else {
        println!("edit: the compositor refused a surface; exiting");
        return None;
    };
    if reply.flags.contains(MessageFlags::ERROR) || reply.handles.is_empty() {
        println!("edit: no surface granted; exiting");
        return None;
    }
    let info = proto::decode::<proto::WmSurfaceInfo>(&reply.payload)?;

    let vmo = Vmo::from_handle(reply.handles.remove(0), info.size);
    // Eagerly and shared: the compositor holds the other mapping of these
    // pages and reads them whenever this process commits.
    let Ok(mapping) = vmo.map(
        0,
        info.size,
        MapFlags::READ.or(MapFlags::WRITE).or(MapFlags::SHARED).or(MapFlags::POPULATE),
        MapWindow::Shared,
    ) else {
        println!("edit: failed to map the surface; exiting");
        return None;
    };

    // SAFETY: the mapping covers the whole object, whose size the compositor
    // reported, and nothing else here maps it.
    Some(unsafe { Screen::new(vmo, mapping, &info) })
}

/// Publish the whole window and wait for the compositor to acknowledge it.
///
/// The editor redraws everything on every change, so the damaged rectangle is
/// always the whole surface. That would be wasteful for a frame loop, but a
/// keystroke is rare and the round trip is what paces the client; tracking a
/// smaller rectangle would buy nothing.
fn commit(screen: &Screen, wm: &Channel) {
    let damage = proto::Encoded::new(proto::WmDamage {
        surface: screen.id,
        x: 0,
        y: 0,
        w: screen.canvas.width(),
        h: screen.canvas.height(),
        _reserved: 0,
    });
    // A small reply buffer, not the 4 KiB default: this runs once per redraw,
    // and the runtime allocator never reclaims a block that large.
    let _ = wm.call_with(Message::new(proto::WM_COMMIT).with_payload(damage.bytes()), 64, 1);
}

// ---------------------------------------------------------------------------
// Rendering
// ---------------------------------------------------------------------------

/// Repaint the whole window from `editor`.
fn render(screen: &mut Screen, editor: &mut Editor, wm: &Channel) {
    editor.clamp();

    let canvas = &mut screen.canvas;
    let width = canvas.width();
    let rows = canvas.rows();
    let cols = canvas.cols();

    canvas.clear(BACKGROUND);

    // A surface too small to hold even one glyph has nothing sensible to draw;
    // the overflow checks are on, so subtracting first would panic rather than
    // wrap.
    if rows == 0 || cols == 0 {
        commit(screen, wm);
        return;
    }

    // The last row is the status bar; every row above it shows a line of text.
    let visible = rows as usize - 1;

    // Scroll the view so the cursor's row is on screen. The two comparisons are
    // deliberately separate: the first scrolls up when the cursor moved above
    // the view, the second scrolls down when it moved below.
    if visible > 0 {
        if editor.row < editor.scroll {
            editor.scroll = editor.row;
        } else if editor.row >= editor.scroll + visible {
            editor.scroll = editor.row + 1 - visible;
        }
    } else {
        editor.scroll = editor.row;
    }

    // Draw the visible lines. `draw_text` handles the row's pixels and stops
    // at the right edge.
    let mut y = 0u32;
    for line_index in editor.scroll..editor.lines.len() {
        if y / CELL >= visible as u32 {
            break;
        }
        draw_text(canvas, y, &editor.lines[line_index], FOREGROUND, BACKGROUND);
        y += CELL;
    }

    // The cursor: the character under it drawn in the swapped colours, which
    // is legible on both a blank cell and a filled one.
    if visible > 0 && editor.row >= editor.scroll {
        let cursor_row = editor.row - editor.scroll;
        let x = editor.col as u32 * CELL;
        if cursor_row < visible && x + CELL <= width {
            let cell = editor.lines[editor.row].chars().nth(editor.col);
            match cell {
                Some(ch) if (ch as u32) < 0x80 => {
                    canvas.draw_glyph_at(x, cursor_row as u32 * CELL, ch as u8, BACKGROUND, Some(FOREGROUND));
                }
                // Past a wide or non-ASCII character, or on the end of the
                // line, a solid block is the honest cursor.
                _ => canvas.fill_rect(x, cursor_row as u32 * CELL, CELL, CELL, FOREGROUND),
            }
        }
    }

    // The status line, on its own bar, reports the file and cursor and reminds
    // the user of the two shortcuts that are not obvious from the text.
    let status_y = (rows - 1) * CELL;
    canvas.fill_rect(0, status_y, width, CELL, STATUS_BACKGROUND);
    let status = alloc::format!(
        "{}{}  {}:{}  Ctrl+S save  Ctrl+Q quit",
        editor.path,
        if editor.dirty { "*" } else { "" },
        editor.row + 1,
        editor.col + 1,
    );
    draw_text(canvas, status_y, &status, FOREGROUND, STATUS_BACKGROUND);

    commit(screen, wm);
}

/// Draw a string one character per cell, stopping at the right edge.
///
/// `Canvas::draw_text_at` walks bytes, but the cursor column counts characters:
/// for the ASCII this editor writes the two agree, and for anything else this
/// keeps them in step.
fn draw_text(canvas: &mut Canvas, y: u32, text: &str, fg: Rgb, bg: Rgb) {
    let width = canvas.width();
    let mut x = 0u32;
    for ch in text.chars() {
        if x + CELL > width {
            break;
        }
        let byte = if (ch as u32) < 0x80 { ch as u8 } else { b'?' };
        canvas.draw_glyph_at(x, y, byte, fg, Some(bg));
        x += CELL;
    }
}

// ---------------------------------------------------------------------------
// The buffer
// ---------------------------------------------------------------------------

/// The document and where the caret is in it.
struct Editor {
    /// Always at least one line, so `lines[row]` is a valid expression.
    lines: Vec<String>,
    /// Cursor row, an index into `lines`.
    row: usize,
    /// Cursor column, counted in characters rather than bytes.
    col: usize,
    /// First visible line; the view scrolls by whole lines.
    scroll: usize,
    dirty: bool,
    path: String,
}

impl Editor {
    fn new(path: String) -> Self {
        Self { lines: alloc::vec![String::new()], row: 0, col: 0, scroll: 0, dirty: false, path }
    }

    /// Restore every invariant a cursor operation relies on.
    fn clamp(&mut self) {
        if self.lines.is_empty() {
            self.lines.push(String::new());
        }
        if self.row >= self.lines.len() {
            self.row = self.lines.len() - 1;
        }
        let len = self.lines[self.row].chars().count();
        if self.col > len {
            self.col = len;
        }
    }

    /// Characters in the cursor's line.
    fn line_len(&self) -> usize {
        self.lines.get(self.row).map(|line| line.chars().count()).unwrap_or(0)
    }

    fn move_left(&mut self) {
        if self.col > 0 {
            self.col -= 1;
        } else if self.row > 0 {
            // Moving off the start of a line lands on the end of the one above.
            self.row -= 1;
            self.col = self.line_len();
        }
    }

    fn move_right(&mut self) {
        if self.col < self.line_len() {
            self.col += 1;
        } else if self.row + 1 < self.lines.len() {
            self.row += 1;
            self.col = 0;
        }
    }

    fn move_up(&mut self) {
        if self.row > 0 {
            self.row -= 1;
            self.col = self.col.min(self.line_len());
        }
    }

    fn move_down(&mut self) {
        if self.row + 1 < self.lines.len() {
            self.row += 1;
            self.col = self.col.min(self.line_len());
        }
    }

    fn home(&mut self) {
        self.col = 0;
    }

    fn end(&mut self) {
        self.col = self.line_len();
    }

    /// Split the cursor's line at the cursor.
    fn newline(&mut self) {
        let at = char_byte_index(&self.lines[self.row], self.col);
        let tail = self.lines[self.row].split_off(at);
        self.lines.insert(self.row + 1, tail);
        self.row += 1;
        self.col = 0;
        self.dirty = true;
    }

    /// Erase the character before the cursor, joining lines at a line start.
    fn backspace(&mut self) {
        if self.col > 0 {
            let at = char_byte_index(&self.lines[self.row], self.col - 1);
            self.lines[self.row].remove(at);
            self.col -= 1;
            self.dirty = true;
        } else if self.row > 0 {
            let current = self.lines.remove(self.row);
            self.row -= 1;
            self.col = self.line_len();
            self.lines[self.row].push_str(&current);
            self.dirty = true;
        }
    }

    /// Erase the character under the cursor, joining lines at a line end.
    fn delete_forward(&mut self) {
        if self.col < self.line_len() {
            let at = char_byte_index(&self.lines[self.row], self.col);
            self.lines[self.row].remove(at);
            self.dirty = true;
        } else if self.row + 1 < self.lines.len() {
            let next = self.lines.remove(self.row + 1);
            self.lines[self.row].push_str(&next);
            self.dirty = true;
        }
    }

    fn insert_char(&mut self, ch: char) {
        let at = char_byte_index(&self.lines[self.row], self.col);
        self.lines[self.row].insert(at, ch);
        self.col += 1;
        self.dirty = true;
    }

    fn insert_tab(&mut self) {
        for _ in 0..TAB_WIDTH {
            self.insert_char(' ');
        }
    }
}

/// Byte offset of character `col` in `line`, or the line's end if it is
/// shorter. The returned offset always lies on a character boundary, so it is
/// safe to hand to `String::insert`, `remove` and `split_off`.
fn char_byte_index(line: &str, col: usize) -> usize {
    line.char_indices().nth(col).map(|(index, _)| index).unwrap_or(line.len())
}

// ---------------------------------------------------------------------------
// Input
// ---------------------------------------------------------------------------

/// Act on one key press. The cursor is clamped by the operations themselves and
/// again before the next render.
fn on_key(editor: &mut Editor, event: &proto::WmEvent, files: &Channel) {
    // The arrow cluster and its neighbours arrive `0xE0`-prefixed and carry no
    // decoded character, so their scancodes are the only signal there is.
    if event.extended != 0 {
        match event.scancode {
            0x48 => editor.move_up(),
            0x50 => editor.move_down(),
            0x4B => editor.move_left(),
            0x4D => editor.move_right(),
            0x47 => editor.home(),
            0x4F => editor.end(),
            0x53 => editor.delete_forward(),
            _ => {}
        }
        return;
    }

    // The compositor does not fold Ctrl into a control character: a Ctrl+S
    // arrives as `'s'` with the CTRL modifier set. Accept both spellings so the
    // editor works whether the key arrived from this compositor or a terminal
    // that did fold it.
    if event.modifiers & proto::wm_mod::CTRL != 0 {
        match event.ascii {
            b's' | 0x13 => {
                save(editor, files);
                return;
            }
            b'q' | 0x11 => libnode::exit(0),
            _ => return,
        }
    }

    // Escape, Enter and Backspace can arrive with no decoded character if the
    // keymap did not produce one; their make codes are the reliable fallback.
    if event.scancode == 0x01 {
        libnode::exit(0);
    }
    if event.scancode == 0x1C {
        editor.newline();
        return;
    }
    if event.scancode == 0x0E {
        editor.backspace();
        return;
    }

    match event.ascii {
        b'\n' => editor.newline(),
        b'\x08' => editor.backspace(),
        b'\t' => editor.insert_tab(),
        0x20..=0x7e => editor.insert_char(event.ascii as char),
        _ => {}
    }
}

// ---------------------------------------------------------------------------
// Filesystem, wrapped
// ---------------------------------------------------------------------------

/// A `channel_call` that turns an error reply into an `Err`.
///
/// The service reports failure in the message flags with a status in the
/// payload; this is the one place that translation happens.
fn fs_call(files: &Channel, request: Message, reply_capacity: usize) -> Result<Message, Error> {
    let reply = files.call_with(request, reply_capacity, 0)?;
    if reply.flags.contains(MessageFlags::ERROR) {
        return Err(proto::decode_error(&reply.payload).unwrap_or(Error::BadState));
    }
    Ok(reply)
}

fn fs_open(files: &Channel, path: &str) -> Result<proto::FsOpenReply, Error> {
    let reply = fs_call(files, Message::new(proto::FS_OPEN).with_payload(path.as_bytes()), 1024)?;
    proto::decode::<proto::FsOpenReply>(&reply.payload).ok_or(Error::BadState)
}

fn fs_create(files: &Channel, path: &str) -> Result<proto::FsOpenReply, Error> {
    let reply = fs_call(files, Message::new(proto::FS_CREATE).with_payload(path.as_bytes()), 1024)?;
    proto::decode::<proto::FsOpenReply>(&reply.payload).ok_or(Error::BadState)
}

fn fs_close(files: &Channel, node: u64) {
    let request = proto::Encoded::new(proto::FsCloseRequest { node });
    let _ = fs_call(files, Message::new(proto::FS_CLOSE).with_payload(request.bytes()), 256);
}

/// Write `data` at `offset`, returning how many bytes the service took (which
/// is capped at 32 KiB per request).
fn fs_write(files: &Channel, node: u64, offset: u64, data: &[u8]) -> Result<usize, Error> {
    let header = proto::Encoded::new(proto::FsWriteHeader {
        node,
        offset,
        length: data.len() as u32,
        _reserved: 0,
    });
    let mut payload = Vec::with_capacity(core::mem::size_of::<proto::FsWriteHeader>() + data.len());
    payload.extend_from_slice(header.bytes());
    payload.extend_from_slice(data);
    let reply = fs_call(files, Message::new(proto::FS_WRITE).with_payload(&payload), 256)?;
    Ok(proto::decode::<proto::FsWriteHeader>(&reply.payload).map(|h| h.length as usize).unwrap_or(0))
}

/// Read the whole file, one service-capped chunk at a time. A file that cannot
/// be opened (or read) yields the empty document rather than an error: the
/// editor is a place to type, and a missing file is a normal way to start.
fn load(files: &Channel, path: &str) -> Vec<String> {
    let node = match fs_open(files, path) {
        Ok(node) => node,
        // Not there yet: start empty, and saving will create it.
        Err(_) => return alloc::vec![String::new()],
    };

    let mut bytes = Vec::new();
    let mut offset = 0u64;
    loop {
        let request = proto::Encoded::new(proto::FsReadRequest {
            node: node.node,
            offset,
            length: READ_CHUNK as u32,
            _reserved: 0,
        });
        // The reply carries the bytes inline, so its buffer is sized for a
        // whole chunk plus its (absent) header.
        let reply = match fs_call(files, Message::new(proto::FS_READ).with_payload(request.bytes()), READ_CHUNK + 64)
        {
            Ok(reply) => reply,
            Err(_) => break,
        };
        if reply.payload.is_empty() {
            break;
        }
        offset += reply.payload.len() as u64;
        bytes.extend_from_slice(&reply.payload);
    }
    fs_close(files, node.node);

    split_lines(&bytes)
}

/// Split file bytes into lines, tolerating both endings.
///
/// A trailing newline leaves a final empty line, which is the line the cursor
/// rests on at the end of a file -- and joining the lines with `\n` on save
/// turns that empty element back into the trailing newline, so a round trip is
/// byte-for-byte.
fn split_lines(bytes: &[u8]) -> Vec<String> {
    let mut lines: Vec<String> = Vec::new();
    for raw in bytes.split(|byte| *byte == b'\n') {
        // Files edited on another system may carry CRLF; the carriage return is
        // not part of a NodeOS line.
        let raw = if raw.last() == Some(&b'\r') { &raw[..raw.len() - 1] } else { raw };
        lines.push(String::from_utf8_lossy(raw).into_owned());
    }
    if lines.is_empty() {
        lines.push(String::new());
    }
    lines
}

/// Write the document back to its path, truncating whatever was there.
///
/// `FS_CREATE` truncates an existing file and creates a missing one, so a save
/// never needs to know which case it is in.
fn save(editor: &mut Editor, files: &Channel) {
    let node = match fs_create(files, &editor.path) {
        Ok(node) => node,
        Err(error) => {
            println!("edit: could not save {} ({error:?})", editor.path);
            return;
        }
    };

    let content = join_lines(&editor.lines);
    let mut offset = 0u64;
    let mut total = 0usize;
    while (offset as usize) < content.len() {
        let end = (offset as usize + WRITE_CHUNK).min(content.len());
        match fs_write(files, node.node, offset, &content[offset as usize..end]) {
            // A short write is not an error; the loop continues from where the
            // service stopped. A zero-length one is, and would spin forever.
            Ok(0) => break,
            Ok(written) => {
                offset += written as u64;
                total += written;
            }
            Err(error) => {
                println!("edit: write to {} failed ({error:?})", editor.path);
                break;
            }
        }
    }
    fs_close(files, node.node);

    editor.dirty = false;
    println!("edit: saved {} bytes to {}", total, editor.path);
}

/// The document as it should be written: lines joined by `\n`.
fn join_lines(lines: &[String]) -> Vec<u8> {
    let mut content = Vec::new();
    for (index, line) in lines.iter().enumerate() {
        if index != 0 {
            content.push(b'\n');
        }
        content.extend_from_slice(line.as_bytes());
    }
    content
}
