//! `edit` -- a full-screen code editor.
//!
//! It is a display client like `console` and `doom`: it holds a channel to the
//! display service, a shared memory object for its surface, and the
//! filesystem channel the launcher granted it. It owns no device; what appears
//! on screen is a surface it draws into and commits, and every keystroke
//! arrives as a `WM_EVENT` on the same channel.
//!
//! The text logic lives in two modules with no NodeOS dependencies, tested on
//! the host (`make test-editor`): `buffer` (a document, its cursor, selection
//! and undo history) and `highlight` (per-line syntax classes for Maylang, C,
//! Lua and shell scripts). This file is the window, the keys and the files.

#![no_std]
#![no_main]

extern crate alloc;

mod buffer;
mod highlight;

use alloc::format;
use alloc::string::String;
use alloc::vec::Vec;

use buffer::{Buffer, Pos};
use highlight::{Class, State};
use libdraw::terminal_font::{TerminalFont, MARGIN};
use libdraw::{Canvas, Rgb};
use libnode::abi::message::MessageFlags;
use libnode::abi::proto::{self, wm_event, wm_mod};
use libnode::abi::types::{MapFlags, MapWindow};
use libnode::mem::{Mapping, Vmo};
use libnode::{println, Channel, Env, Error, Message};

libnode::main! {
    fn main(env: Env) -> i32 {
        run(env)
    }
}

// The console's palette, extended with syntax colours.
const BACKGROUND: Rgb = 0x0010_1216;
const FOREGROUND: Rgb = 0x00E3_E8EE;
const CURRENT_LINE: Rgb = 0x0018_1C22;
const GUTTER: Rgb = 0x005C_6370;
const GUTTER_CURRENT: Rgb = 0x00C8_CCD4;
const SELECTION: Rgb = 0x002D_4F7C;
const MATCH: Rgb = 0x0054_4520;
const BRACKET: Rgb = 0x0040_4859;
const BAR: Rgb = 0x000B_0D10;
const TAB_INACTIVE: Rgb = 0x008A_919C;
const STATUS: Rgb = 0x001E_2A3A;
const NOTICE: Rgb = 0x00E5_C07B;
const ERROR: Rgb = 0x00E0_6C75;

fn class_color(class: Class) -> Rgb {
    match class {
        Class::Normal => FOREGROUND,
        Class::Keyword => 0x00C6_78DD,
        Class::Type => 0x00E5_C07B,
        Class::Constant | Class::Number => 0x00D1_9A66,
        Class::Function => 0x0061_AFEF,
        Class::String => 0x0098_C379,
        Class::Escape => 0x0056_B6C2,
        Class::Comment => 0x007F_848E,
    }
}

/// Bytes per filesystem request; the service caps one at 32 KiB.
const CHUNK: usize = 32 * 1024;
/// Largest file the editor opens.
const MAX_FILE: usize = 4 * 1024 * 1024;

fn run(mut env: Env) -> i32 {
    // The session handles arrive in a fixed order: console, filesystem,
    // display, audio. The editor uses the filesystem and the display.
    let _console = env.take_extra(0);
    let Some(files) = env.take_extra(0).map(Channel::from_handle) else {
        println!("edit: no filesystem connection granted; exiting");
        return 1;
    };
    let Some(wm) = env.take_extra(0).map(Channel::from_handle) else {
        println!("edit: no display connection granted; exiting");
        return 1;
    };
    let cwd = env.var("PWD").filter(|p| p.starts_with('/')).map(String::from).unwrap_or_else(|| String::from("/"));
    let mut app = App::new(files, cwd);
    let paths: Vec<String> = env.args().iter().skip(1).filter(|p| !p.is_empty()).cloned().collect();
    if paths.is_empty() {
        app.tabs.push(Buffer::new(String::new(), ""));
    }
    for path in paths {
        app.open(&path);
    }
    app.current = 0;

    let title = app.tabs[0].path.clone();
    let Some(mut screen) = create_surface(&wm, if title.is_empty() { "edit" } else { &title }) else {
        return 1;
    };
    println!("edit: editing {} ({} lines)", app.tabs[0].path, app.tabs[0].lines.len());
    app.render(&mut screen, &wm);

    loop {
        let Ok(message) = wm.receive() else { continue };
        if message.kind != proto::WM_EVENT {
            continue;
        }
        let Some(event) = proto::decode::<proto::WmEvent>(&message.payload) else { continue };
        match event.kind {
            wm_event::KEY if event.pressed != 0 => {
                if !app.key(&event) {
                    return 0;
                }
                app.render(&mut screen, &wm);
            }
            // The display replaced the surface; repaint into the new one.
            wm_event::CONFIGURE => {
                let Some(handle) = message.handles.into_iter().next() else { continue };
                let (width, height) = (event.width, event.height);
                let pitch = width * 4;
                let size = pitch as u64 * height as u64;
                let vmo = Vmo::from_handle(handle, size);
                let Ok(mapping) = vmo.map(
                    0,
                    size,
                    MapFlags::READ.or(MapFlags::WRITE).or(MapFlags::SHARED).or(MapFlags::POPULATE),
                    MapWindow::Shared,
                ) else {
                    println!("edit: failed to map the resized surface");
                    continue;
                };
                // SAFETY: the mapping covers the whole object, whose size is
                // exactly `pitch * height`, and nothing else here maps it.
                unsafe { screen.replace(vmo, mapping, width, height, pitch) };
                app.render(&mut screen, &wm);
            }
            wm_event::CLOSE => return 0,
            _ => {}
        }
    }
}

// ---------------------------------------------------------------------------
// The window
// ---------------------------------------------------------------------------

/// The surface: the memory object, its mapping, and a canvas over it. The
/// object and mapping are held so they outlive the canvas, which borrows them.
struct Screen {
    _vmo: Vmo,
    _mapping: Mapping,
    canvas: Canvas,
    id: u32,
}

impl Screen {
    /// # Safety
    /// `mapping` must cover at least `pitch * height` bytes of `vmo`, and
    /// nothing else in this process may write it.
    unsafe fn replace(&mut self, vmo: Vmo, mapping: Mapping, width: u32, height: u32, pitch: u32) {
        // SAFETY: as the caller guarantees; `_mapping` keeps it alive.
        let canvas = unsafe { Canvas::from_raw(mapping.base() as *mut u8, width, height, pitch) };
        self._mapping = mapping;
        self._vmo = vmo;
        self.canvas = canvas;
    }
}

fn create_surface(wm: &Channel, title: &str) -> Option<Screen> {
    // The display shows one full-screen surface at a time and grants the
    // whole screen; the size asked for is only a preference.
    let request = proto::Encoded::new(proto::WmCreateSurface { width: 1024, height: 768, flags: 0, _reserved: 0 });
    let mut payload = Vec::from(request.bytes());
    payload.extend_from_slice(title.as_bytes());
    let Ok(mut reply) = wm.call(Message::new(proto::WM_CREATE_SURFACE).with_payload(&payload)) else {
        println!("edit: the display refused a surface; exiting");
        return None;
    };
    if reply.flags.contains(MessageFlags::ERROR) || reply.handles.is_empty() {
        println!("edit: no surface granted; exiting");
        return None;
    }
    let info = proto::decode::<proto::WmSurfaceInfo>(&reply.payload)?;
    let vmo = Vmo::from_handle(reply.handles.remove(0), info.size);
    let Ok(mapping) = vmo.map(
        0,
        info.size,
        MapFlags::READ.or(MapFlags::WRITE).or(MapFlags::SHARED).or(MapFlags::POPULATE),
        MapWindow::Shared,
    ) else {
        println!("edit: failed to map the surface; exiting");
        return None;
    };
    // SAFETY: the mapping covers the object the display sized for this
    // geometry, and nothing else here maps it.
    let canvas = unsafe { Canvas::from_raw(mapping.base() as *mut u8, info.width, info.height, info.pitch) };
    Some(Screen { _vmo: vmo, _mapping: mapping, canvas, id: info.surface })
}

/// Publish the whole surface and wait for the display to take it.
fn commit(screen: &Screen, wm: &Channel) {
    let damage = proto::Encoded::new(proto::WmDamage {
        surface: screen.id,
        x: 0,
        y: 0,
        w: screen.canvas.width(),
        h: screen.canvas.height(),
        _reserved: 0,
    });
    let _ = wm.call_with(Message::new(proto::WM_COMMIT).with_payload(damage.bytes()), 64, 1);
}

// ---------------------------------------------------------------------------
// The editor
// ---------------------------------------------------------------------------

#[derive(Clone, Copy, PartialEq, Eq)]
enum PromptKind {
    Find,
    ReplaceFind,
    ReplaceWith,
    Goto,
    Open,
    SaveAs,
}

struct Prompt {
    kind: PromptKind,
    text: String,
}

struct App {
    files: Channel,
    cwd: String,
    tabs: Vec<Buffer>,
    current: usize,
    clipboard: String,
    prompt: Option<Prompt>,
    /// The last search, for F3 and for highlighting matches.
    search: String,
    /// What a replace prompt is replacing, between its two questions.
    replacing: String,
    message: Option<(String, bool)>,
    font: TerminalFont,
    help: bool,
    /// A destructive key pressed once with unsaved changes, waiting for a
    /// second press to confirm: Ctrl+Q or Ctrl+W.
    armed: Option<u8>,
    /// Text rows and columns in the last drawn layout, for paging and scrolling.
    view_rows: usize,
    view_cols: usize,
}

impl App {
    fn new(files: Channel, cwd: String) -> Self {
        Self {
            files,
            cwd,
            tabs: Vec::new(),
            current: 0,
            clipboard: String::new(),
            prompt: None,
            search: String::new(),
            replacing: String::new(),
            message: None,
            font: TerminalFont::Normal,
            help: false,
            armed: None,
            view_rows: 30,
            view_cols: 80,
        }
    }

    fn tab(&mut self) -> &mut Buffer {
        &mut self.tabs[self.current]
    }

    fn notice(&mut self, text: String) {
        self.message = Some((text, false));
    }

    fn error(&mut self, text: String) {
        self.message = Some((text, true));
    }

    /// Open `path` in a tab, or switch to the tab already showing it.
    fn open(&mut self, path: &str) {
        let path = resolve(&self.cwd, path);
        if let Some(index) = self.tabs.iter().position(|t| t.path.eq_ignore_ascii_case(&path)) {
            self.current = index;
            return;
        }
        match load(&self.files, &path) {
            Ok(Some(bytes)) => {
                let text = String::from_utf8_lossy(&bytes);
                self.tabs.push(Buffer::new(path.clone(), &text));
                self.current = self.tabs.len() - 1;
                self.notice(format!("Opened {path}"));
            }
            Ok(None) => {
                self.tabs.push(Buffer::new(path.clone(), ""));
                self.current = self.tabs.len() - 1;
                self.notice(format!("New file {path}; Ctrl+S creates it"));
            }
            Err(error) => self.error(format!("Cannot open {path}: {}", error.message())),
        }
    }

    fn save(&mut self) {
        if self.tab().path.is_empty() {
            self.prompt = Some(Prompt { kind: PromptKind::SaveAs, text: self.cwd_prefix() });
            return;
        }
        let path = self.tab().path.clone();
        let text = self.tab().text();
        match save(&self.files, &path, text.as_bytes()) {
            Ok(()) => {
                self.tab().dirty = false;
                println!("edit: saved {} bytes to {}", text.len(), path);
                self.notice(format!("Saved {} bytes to {path}", text.len()));
            }
            Err(error) => {
                println!("edit: could not save {path} ({error:?})");
                let hint = if error == Error::InvalidArgument { " (new names must fit 8.3)" } else { "" };
                self.error(format!("Could not save {path}: {}{hint}", error.message()));
            }
        }
    }

    fn cwd_prefix(&self) -> String {
        if self.cwd == "/" { String::from("/") } else { format!("{}/", self.cwd) }
    }

    fn find_next(&mut self, backwards: bool) {
        let needle = self.search.clone();
        let tab = &mut self.tabs[self.current];
        let from = if backwards {
            tab.selection().map(|(s, _)| s).unwrap_or(tab.cursor)
        } else {
            tab.selection().map(|(_, e)| e).unwrap_or(tab.cursor)
        };
        match tab.find(&needle, from, backwards) {
            Some((start, end)) => {
                tab.jump(start, false);
                tab.jump(end, true);
            }
            None if !needle.is_empty() => self.error(format!("No match for \"{needle}\"")),
            None => {}
        }
    }

    /// Handle one key press. Returns false to quit.
    fn key(&mut self, event: &proto::WmEvent) -> bool {
        let ctrl = event.modifiers & wm_mod::CTRL != 0;
        let shift = event.modifiers & wm_mod::SHIFT != 0;
        let alt = event.modifiers & wm_mod::ALT != 0;
        let code = event.scancode;
        let ext = event.extended != 0;
        let ascii = event.ascii;
        self.message = None;
        if self.help {
            self.help = false;
            return true;
        }
        let armed = self.armed.take();

        if self.prompt.is_some() {
            self.prompt_key(event);
            return true;
        }

        if ext {
            let page = self.view_rows.max(1) as isize;
            let tab = self.tab();
            match code {
                0x48 => tab.vertical(-1, shift),
                0x50 => tab.vertical(1, shift),
                0x4B => tab.left(shift, ctrl),
                0x4D => tab.right(shift, ctrl),
                0x47 if ctrl => tab.jump(Pos::new(0, 0), shift),
                0x4F if ctrl => {
                    let last = tab.lines.len() - 1;
                    let len = tab.line_len(last);
                    tab.jump(Pos::new(last, len), shift)
                }
                0x47 => tab.home(shift),
                0x4F => tab.end(shift),
                0x49 if ctrl => self.switch(-1),
                0x51 if ctrl => self.switch(1),
                0x49 => {
                    tab.vertical(-page, shift);
                    tab.top = tab.top.saturating_sub(page as usize);
                }
                0x51 => {
                    tab.vertical(page, shift);
                    tab.top += page as usize;
                    let max = tab.lines.len().saturating_sub(1);
                    tab.top = tab.top.min(max);
                }
                0x53 => tab.delete(),
                _ => {}
            }
            return true;
        }

        match code {
            0x3B => {
                self.help = true;
                return true;
            }
            0x3D => {
                self.find_next(shift);
                return true;
            }
            0x01 => {
                self.tab().anchor = None;
                return true;
            }
            0x0F if ctrl => {
                self.switch(if shift { -1 } else { 1 });
                return true;
            }
            _ => {}
        }

        if alt && (b'1'..=b'9').contains(&ascii) {
            let index = (ascii - b'1') as usize;
            if index < self.tabs.len() {
                self.current = index;
            }
            return true;
        }

        if ctrl {
            let letter = ascii.to_ascii_lowercase();
            match letter {
                b's' if shift || ascii == b'S' => {
                    let path = if self.tab().path.is_empty() { self.cwd_prefix() } else { self.tab().path.clone() };
                    self.prompt = Some(Prompt { kind: PromptKind::SaveAs, text: path });
                }
                b's' | 0x13 => self.save(),
                b'q' | 0x11 => {
                    if self.tabs.iter().any(|t| t.dirty) && armed != Some(b'q') {
                        self.armed = Some(b'q');
                        self.error(String::from("Unsaved changes. Press Ctrl+Q again to quit without saving."));
                    } else {
                        return false;
                    }
                }
                b'w' => {
                    if self.tab().dirty && armed != Some(b'w') {
                        self.armed = Some(b'w');
                        self.error(String::from("Unsaved changes. Press Ctrl+W again to close without saving."));
                    } else {
                        self.tabs.remove(self.current);
                        if self.tabs.is_empty() {
                            return false;
                        }
                        self.current = self.current.min(self.tabs.len() - 1);
                    }
                }
                b'n' => {
                    self.tabs.push(Buffer::new(String::new(), ""));
                    self.current = self.tabs.len() - 1;
                }
                b'o' => self.prompt = Some(Prompt { kind: PromptKind::Open, text: self.cwd_prefix() }),
                b'f' => {
                    let seed = self.tab().selected_text().filter(|t| !t.contains('\n')).unwrap_or_default();
                    self.prompt = Some(Prompt { kind: PromptKind::Find, text: seed });
                }
                b'h' | b'r' => {
                    let seed = self.tab().selected_text().filter(|t| !t.contains('\n')).unwrap_or_default();
                    self.prompt = Some(Prompt { kind: PromptKind::ReplaceFind, text: seed });
                }
                b'g' => self.prompt = Some(Prompt { kind: PromptKind::Goto, text: String::new() }),
                b'z' if shift || ascii == b'Z' => {
                    self.tab().redo();
                }
                b'z' => {
                    if !self.tab().undo() {
                        self.notice(String::from("Nothing to undo"));
                    }
                }
                b'y' => {
                    if !self.tab().redo() {
                        self.notice(String::from("Nothing to redo"));
                    }
                }
                b'a' => self.tab().select_all(),
                b'c' => {
                    let text = match self.tab().selected_text() {
                        Some(text) => text,
                        None => {
                            let line = self.tab().cursor.line;
                            format!("{}\n", self.tab().lines[line])
                        }
                    };
                    self.clipboard = text;
                }
                b'x' => {
                    self.clipboard = match self.tab().cut() {
                        Some(text) => text,
                        None => self.tab().cut_line(),
                    };
                }
                b'v' => {
                    let text = self.clipboard.clone();
                    self.tab().insert(&text);
                }
                b'd' => self.tab().duplicate_line(),
                b'/' => self.tab().toggle_comment(),
                b'=' | b'+' => self.resize(1),
                b'-' => self.resize(-1),
                _ => {}
            }
            return true;
        }

        let tab = self.tab();
        match code {
            0x1C => tab.newline(),
            0x0E => tab.backspace(),
            0x0F => tab.tab(shift),
            _ => match ascii {
                b'\n' => tab.newline(),
                0x08 => tab.backspace(),
                b'\t' => tab.tab(shift),
                0x20..=0x7e => tab.type_char(ascii as char),
                _ => {}
            },
        }
        true
    }

    fn switch(&mut self, by: isize) {
        let count = self.tabs.len() as isize;
        self.current = ((self.current as isize + by).rem_euclid(count)) as usize;
    }

    fn resize(&mut self, by: i32) {
        self.font = match (self.font, by) {
            (TerminalFont::Small, 1) => TerminalFont::Normal,
            (TerminalFont::Normal, 1) | (TerminalFont::Large, 1) => TerminalFont::Large,
            (TerminalFont::Large, -1) => TerminalFont::Normal,
            _ => TerminalFont::Small,
        };
    }

    fn prompt_key(&mut self, event: &proto::WmEvent) {
        let Some(mut prompt) = self.prompt.take() else { return };
        let shift = event.modifiers & wm_mod::SHIFT != 0;
        let ext = event.extended != 0;
        let enter = !ext && (event.scancode == 0x1C || event.ascii == b'\n');
        if !ext && event.scancode == 0x01 {
            return; // Esc: the prompt is gone
        }
        if ext && prompt.kind == PromptKind::Find && matches!(event.scancode, 0x48 | 0x50) {
            self.find_next(event.scancode == 0x48);
            self.prompt = Some(prompt);
            return;
        }
        if !ext && event.scancode == 0x3D && prompt.kind == PromptKind::Find {
            self.find_next(shift);
            self.prompt = Some(prompt);
            return;
        }
        if !ext && (event.scancode == 0x0E || event.ascii == 0x08) {
            prompt.text.pop();
        } else if enter {
            self.accept(prompt, shift);
            return;
        } else if !ext && event.modifiers & wm_mod::CTRL == 0 && (0x20..=0x7e).contains(&event.ascii) {
            prompt.text.push(event.ascii as char);
        }
        if prompt.kind == PromptKind::Find {
            // Search as you type, from where the search began.
            self.search = prompt.text.clone();
            let tab = &mut self.tabs[self.current];
            let from = tab.selection().map(|(s, _)| s).unwrap_or(tab.cursor);
            if let Some((start, end)) = tab.find(&prompt.text, from, false) {
                tab.jump(start, false);
                tab.jump(end, true);
            }
        }
        self.prompt = Some(prompt);
    }

    fn accept(&mut self, prompt: Prompt, shift: bool) {
        let text = prompt.text.trim().into();
        match prompt.kind {
            PromptKind::Find => {
                self.search = text;
                self.find_next(shift);
                // Enter repeats the search with the prompt still open.
                self.prompt = Some(prompt);
            }
            PromptKind::ReplaceFind if !text.is_empty() => {
                self.replacing = text;
                self.prompt = Some(Prompt { kind: PromptKind::ReplaceWith, text: String::new() });
            }
            PromptKind::ReplaceFind => {}
            PromptKind::ReplaceWith => {
                let needle = self.replacing.clone();
                let count = self.tab().replace_all(&needle, &prompt.text);
                self.notice(format!("Replaced {count} occurrence(s) of \"{needle}\""));
            }
            PromptKind::Goto => match text.parse::<usize>() {
                Ok(line) if line > 0 => {
                    let half = self.view_rows / 2;
                    let tab = self.tab();
                    let line = (line - 1).min(tab.lines.len() - 1);
                    tab.jump(Pos::new(line, 0), false);
                    tab.top = line.saturating_sub(half);
                }
                _ => self.error(format!("\"{text}\" is not a line number")),
            },
            PromptKind::Open if !text.is_empty() => self.open(&text),
            PromptKind::Open => {}
            PromptKind::SaveAs if !text.is_empty() => {
                let path = resolve(&self.cwd, &text);
                let tab = self.tab();
                tab.path = path;
                tab.lang = highlight::Lang::for_path(&tab.path);
                self.save();
            }
            PromptKind::SaveAs => {}
        }
    }

    // -----------------------------------------------------------------------
    // Drawing

    fn render(&mut self, screen: &mut Screen, wm: &Channel) {
        let font = self.font;
        let canvas = &mut screen.canvas;
        canvas.clear(BACKGROUND);
        let (cols, rows) = font.geometry(canvas);
        let (cols, rows) = (cols as usize, rows as usize);
        if cols < 20 || rows < 4 {
            commit(screen, wm);
            return;
        }
        let mut painter = Painter { canvas, font };
        let text_rows = rows - 2;
        let tab_index = self.current;
        let line_count = self.tabs[tab_index].lines.len();
        let gutter = digits(line_count).max(3) + 2;
        let text_cols = cols - gutter;
        self.view_rows = text_rows;
        self.view_cols = text_cols;

        // Tab bar.
        painter.fill(0, 0, cols, BAR);
        let mut x = 0;
        for (index, tab) in self.tabs.iter().enumerate() {
            let name = if tab.path.is_empty() { "untitled" } else { tab.path.rsplit('/').next().unwrap_or("") };
            let label = format!(" {}{} ", name, if tab.dirty { "*" } else { "" });
            let (fg, bg) = if index == tab_index { (FOREGROUND, BACKGROUND) } else { (TAB_INACTIVE, BAR) };
            if x + label.len() > cols {
                break;
            }
            painter.text(x, 0, &label, fg, bg);
            x += label.len() + 1;
        }

        // Text.
        let search = self.search.to_lowercase();
        let show_matches = !search.is_empty();
        let tab = &mut self.tabs[tab_index];
        tab.reveal(text_rows, text_cols);
        let selection = tab.selection();
        let brackets = tab.matching_bracket();
        let mut state = State::default();
        for line in 0..tab.top.min(tab.lines.len()) {
            highlight::classify(tab.lang, &tab.lines[line], &mut state);
        }
        for row in 0..text_rows {
            let line = tab.top + row;
            let y = row + 1;
            let current = line == tab.cursor.line;
            let base = if current && selection.is_none() { CURRENT_LINE } else { BACKGROUND };
            if line >= tab.lines.len() {
                painter.text(0, y, &format!("{:>w$}", "~", w = gutter - 2), GUTTER, BACKGROUND);
                continue;
            }
            let number = format!("{:>w$}  ", line + 1, w = gutter - 2);
            painter.text(0, y, &number, if current { GUTTER_CURRENT } else { GUTTER }, base);
            painter.fill(gutter, y, text_cols, base);
            let text = &tab.lines[line];
            let classes = highlight::classify(tab.lang, text, &mut state);
            let chars: Vec<char> = text.chars().collect();
            let lower: Vec<char> = if show_matches { text.to_lowercase().chars().collect() } else { Vec::new() };
            let needle: Vec<char> = search.chars().collect();
            for col in tab.left..chars.len().min(tab.left + text_cols) {
                let here = Pos::new(line, col);
                let mut bg = base;
                if show_matches && lower.len() == chars.len() {
                    let start = col.saturating_sub(needle.len() - 1);
                    let hit = (start..=col).any(|s| s + needle.len() <= lower.len() && lower[s..s + needle.len()] == needle[..]);
                    if hit {
                        bg = MATCH;
                    }
                }
                if brackets.is_some_and(|(a, b)| a == here || b == here) {
                    bg = BRACKET;
                }
                if selection.is_some_and(|(s, e)| s <= here && here < e) {
                    bg = SELECTION;
                }
                painter.cell(gutter + col - tab.left, y, chars[col], class_color(classes[col]), bg);
            }
            // A selection that continues past the end of the line.
            if selection.is_some_and(|(s, e)| s <= Pos::new(line, chars.len()) && Pos::new(line, chars.len()) < e) {
                let col = chars.len();
                if col >= tab.left && col < tab.left + text_cols {
                    painter.cell(gutter + col - tab.left, y, ' ', FOREGROUND, SELECTION);
                }
            }
        }
        // The cursor: the character under it in swapped colours.
        if tab.cursor.line >= tab.top && tab.cursor.line < tab.top + text_rows && tab.cursor.col >= tab.left {
            let x = gutter + tab.cursor.col - tab.left;
            if x < cols {
                let under = tab.char_at(tab.cursor).unwrap_or(' ');
                painter.cell(x, tab.cursor.line - tab.top + 1, under, BACKGROUND, FOREGROUND);
            }
        }

        // Status line, or the prompt that replaces it.
        let y = rows - 1;
        painter.fill(0, y, cols, STATUS);
        if let Some(prompt) = &self.prompt {
            let label = match prompt.kind {
                PromptKind::Find => "Find (Enter/Down next, Up previous, Esc done): ",
                PromptKind::ReplaceFind => "Replace: ",
                PromptKind::ReplaceWith => "Replace with: ",
                PromptKind::Goto => "Go to line: ",
                PromptKind::Open => "Open file: ",
                PromptKind::SaveAs => "Save as: ",
            };
            let shown = format!("{label}{}", prompt.text);
            painter.text(0, y, &shown, NOTICE, STATUS);
            painter.cell(shown.chars().count().min(cols - 1), y, ' ', STATUS, FOREGROUND);
        } else {
            let tab = &self.tabs[tab_index];
            let right = format!(
                "Ln {}, Col {}  {}  {} lines  F1 help ",
                tab.cursor.line + 1,
                tab.cursor.col + 1,
                tab.lang.name(),
                tab.lines.len()
            );
            let (left, color) = match &self.message {
                Some((text, error)) => (text.clone(), if *error { ERROR } else { NOTICE }),
                None => (
                    format!(
                        " {}{}",
                        if tab.path.is_empty() { "untitled" } else { &tab.path },
                        if tab.dirty { " (modified)" } else { "" }
                    ),
                    FOREGROUND,
                ),
            };
            painter.text(0, y, &left, color, STATUS);
            if right.len() + left.chars().count() < cols {
                painter.text(cols - right.len(), y, &right, TAB_INACTIVE, STATUS);
            }
        }

        if self.help {
            draw_help(&mut painter, cols, rows);
        }
        commit(screen, wm);
    }
}

/// Cell drawing in the terminal font's grid.
struct Painter<'a> {
    canvas: &'a mut Canvas,
    font: TerminalFont,
}

impl Painter<'_> {
    fn cell(&mut self, col: usize, row: usize, c: char, fg: Rgb, bg: Rgb) {
        if c == ' ' {
            let (w, h) = self.font.metrics();
            self.canvas.fill_rect(MARGIN + col as u32 * w, MARGIN + row as u32 * h, w, h, bg);
            return;
        }
        let byte = if (c as u32) < 0x80 { c as u8 } else { b'?' };
        self.font.draw(self.canvas, col as u32, row as u32, byte, fg, bg);
    }

    fn fill(&mut self, col: usize, row: usize, cells: usize, bg: Rgb) {
        let (w, h) = self.font.metrics();
        self.canvas.fill_rect(MARGIN + col as u32 * w, MARGIN + row as u32 * h, cells as u32 * w, h, bg);
    }

    fn text(&mut self, col: usize, row: usize, text: &str, fg: Rgb, bg: Rgb) {
        for (index, c) in text.chars().enumerate() {
            self.cell(col + index, row, c, fg, bg);
        }
    }
}

const HELP: &[&str] = &[
    "edit -- keys",
    "",
    "Ctrl+S  save           Ctrl+Shift+S  save as     Ctrl+O  open file",
    "Ctrl+N  new tab        Ctrl+W  close tab         Ctrl+Q  quit",
    "Ctrl+Tab / Ctrl+PgDn   next tab    Alt+1..9  go to tab",
    "",
    "Ctrl+Z  undo           Ctrl+Y  redo              Ctrl+A  select all",
    "Ctrl+X  cut (line)     Ctrl+C  copy (line)       Ctrl+V  paste",
    "Ctrl+D  duplicate line Ctrl+/  toggle comment    Tab / Shift+Tab  indent",
    "Shift+arrows  select   Ctrl+Left/Right  by word  Ctrl+Home/End  file ends",
    "",
    "Ctrl+F  find (F3 next, Shift+F3 previous)        Ctrl+H  replace all",
    "Ctrl+G  go to line     Ctrl+= / Ctrl+-  text size",
    "",
    "Maylang, C, Lua and shell files are coloured by their extension.",
    "Brackets and quotes close themselves; Enter keeps the indentation.",
    "",
    "Press any key to return.",
];

fn draw_help(painter: &mut Painter<'_>, cols: usize, rows: usize) {
    let width = HELP.iter().map(|l| l.len()).max().unwrap_or(0) + 4;
    let height = HELP.len() + 2;
    if width > cols || height > rows {
        return;
    }
    let left = (cols - width) / 2;
    let top = (rows - height) / 2;
    for row in 0..height {
        painter.fill(left, top + row, width, STATUS);
    }
    for (index, line) in HELP.iter().enumerate() {
        let color = if index == 0 { NOTICE } else { FOREGROUND };
        painter.text(left + 2, top + 1 + index, line, color, STATUS);
    }
}

fn digits(mut n: usize) -> usize {
    let mut count = 1;
    while n >= 10 {
        n /= 10;
        count += 1;
    }
    count
}

/// An absolute path for `input`, relative to `cwd`, with `.` and `..` resolved.
fn resolve(cwd: &str, input: &str) -> String {
    let mut parts: Vec<&str> = Vec::new();
    let base = if input.starts_with('/') { "" } else { cwd };
    for part in base.split('/').chain(input.split('/')) {
        match part {
            "" | "." => {}
            ".." => {
                parts.pop();
            }
            part => parts.push(part),
        }
    }
    format!("/{}", parts.join("/"))
}

// ---------------------------------------------------------------------------
// Files
// ---------------------------------------------------------------------------

fn fs_call(files: &Channel, request: Message, capacity: usize) -> Result<Message, Error> {
    let reply = files.call_with(request, capacity, 0)?;
    if reply.flags.contains(MessageFlags::ERROR) {
        return Err(proto::decode_error(&reply.payload).unwrap_or(Error::BadState));
    }
    Ok(reply)
}

fn fs_node(files: &Channel, kind: u32, path: &str) -> Result<proto::FsOpenReply, Error> {
    let reply = fs_call(files, Message::new(kind).with_payload(path.as_bytes()), 256)?;
    proto::decode::<proto::FsOpenReply>(&reply.payload).ok_or(Error::BadState)
}

fn fs_close(files: &Channel, node: u64) {
    let request = proto::Encoded::new(proto::FsCloseRequest { node });
    let _ = fs_call(files, Message::new(proto::FS_CLOSE).with_payload(request.bytes()), 64);
}

/// The file's bytes, or `None` if it does not exist yet.
fn load(files: &Channel, path: &str) -> Result<Option<Vec<u8>>, Error> {
    let node = match fs_node(files, proto::FS_OPEN, path) {
        Ok(node) => node,
        Err(Error::NotFound) => return Ok(None),
        Err(error) => return Err(error),
    };
    let result = (|| {
        if node.kind != proto::fs_kind::FILE {
            return Err(Error::InvalidArgument);
        }
        if node.size as usize > MAX_FILE {
            return Err(Error::LimitExceeded);
        }
        let mut bytes = Vec::with_capacity(node.size as usize);
        while (bytes.len() as u64) < node.size {
            let request = proto::Encoded::new(proto::FsReadRequest {
                node: node.node,
                offset: bytes.len() as u64,
                length: CHUNK as u32,
                _reserved: 0,
            });
            let reply = fs_call(files, Message::new(proto::FS_READ).with_payload(request.bytes()), CHUNK + 64)?;
            if reply.payload.is_empty() {
                break;
            }
            bytes.extend_from_slice(&reply.payload);
        }
        Ok(Some(bytes))
    })();
    fs_close(files, node.node);
    result
}

/// Replace the file with `data`. `FS_CREATE` truncates an existing file.
fn save(files: &Channel, path: &str, data: &[u8]) -> Result<(), Error> {
    let node = fs_node(files, proto::FS_CREATE, path)?;
    let result = (|| {
        let mut offset = 0usize;
        while offset < data.len() {
            let chunk = &data[offset..(offset + CHUNK).min(data.len())];
            let header = proto::Encoded::new(proto::FsWriteHeader {
                node: node.node,
                offset: offset as u64,
                length: chunk.len() as u32,
                _reserved: 0,
            });
            let mut payload = Vec::from(header.bytes());
            payload.extend_from_slice(chunk);
            let reply = fs_call(files, Message::new(proto::FS_WRITE).with_payload(&payload), 256)?;
            let written = proto::decode::<proto::FsWriteHeader>(&reply.payload).map(|h| h.length as usize).unwrap_or(0);
            if written == 0 {
                return Err(Error::NoSpace);
            }
            offset += written;
        }
        Ok(())
    })();
    fs_close(files, node.node);
    result
}
