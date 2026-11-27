//! The list of videos shown when `video` is started without a file.

use alloc::format;
use alloc::string::String;

use libdraw::terminal_font::TerminalFont;

use crate::files::Entry;
use crate::input::{self, Event, Key};
use crate::player::{clock_text, ellipsize};
use crate::screen::{Screen, CELL};

const BACKGROUND: u32 = 0x101418;
const TEXT: u32 = 0xe8ecf2;
const MUTED: u32 = 0x8a93a3;
const ACCENT: u32 = 0x6ea8ff;
const SELECTED: u32 = 0x22324f;
const WARNING: u32 = 0xffb454;

const ROW_H: usize = 34;

pub enum Choice {
    Play(usize),
    Rescan,
    Quit,
}

/// Show `entries` and let the user pick one. `selected` is kept across calls
/// so returning from a video lands back on it.
pub fn choose(screen: &mut Screen, entries: &[Entry], selected: &mut usize, message: Option<&str>) -> Choice {
    let mut dirty = true;
    let mut scroll = 0usize;
    loop {
        if dirty {
            *selected = (*selected).min(entries.len().saturating_sub(1));
            draw(screen, entries, *selected, &mut scroll, message);
            dirty = false;
        }
        input::wait(screen, 1_000_000_000);
        for event in input::poll(screen) {
            dirty = true;
            let key = match event {
                Event::Close => return Choice::Quit,
                Event::Resized => continue,
                Event::Key(key) => key,
            };
            let last = entries.len().saturating_sub(1);
            let page = page_rows(screen).max(1);
            match key {
                Key::Escape | Key::Char(b'q') => return Choice::Quit,
                Key::Enter | Key::Space if !entries.is_empty() => return Choice::Play(*selected),
                Key::Char(b'r') => return Choice::Rescan,
                Key::Up => *selected = selected.saturating_sub(1),
                Key::Down => *selected = (*selected + 1).min(last),
                Key::PageUp => *selected = selected.saturating_sub(page),
                Key::PageDown => *selected = (*selected + page).min(last),
                Key::Home => *selected = 0,
                Key::End => *selected = last,
                _ => {}
            }
        }
    }
}

/// Rows of the list that fit between the header and the footer.
fn page_rows(screen: &Screen) -> usize {
    screen.height.saturating_sub(top() + footer_h()) / ROW_H
}

fn top() -> usize {
    40 + TerminalFont::Large.metrics().1 as usize + 40
}

fn footer_h() -> usize {
    CELL.1 as usize * 3 + 24
}

fn draw(screen: &mut Screen, entries: &[Entry], selected: usize, scroll: &mut usize, message: Option<&str>) {
    let (cw, ch) = (CELL.0 as usize, CELL.1 as usize);
    let width = screen.width;
    let margin = 48.min(width / 10);
    screen.fill(BACKGROUND);

    // Header.
    {
        let mut canvas = screen.canvas();
        TerminalFont::Large.draw_text_at(&mut canvas, margin as u32, 40, "Videos", TEXT, None);
    }
    let count = match entries.len() {
        1 => String::from("1 video"),
        n => format!("{n} videos"),
    };
    let count_x = width.saturating_sub(margin + Screen::text_width(&count));
    screen.text(count_x, 44, &count, MUTED);
    screen.rect(margin, top() - 16, width.saturating_sub(2 * margin), 1, 0x2a3344);

    // Columns, right-aligned from the right edge: size, picture, length.
    let size_x = width.saturating_sub(margin + 10 * cw);
    let picture_x = size_x.saturating_sub(12 * cw);
    let length_x = picture_x.saturating_sub(10 * cw);
    let name_room = length_x.saturating_sub(margin + 4 * cw) / cw;

    if entries.is_empty() {
        let lines = [
            ("No videos found.", TEXT),
            ("", TEXT),
            ("Put .BAP files in /VIDEOS on the disk: on the host, drop them in", MUTED),
            ("videos/ and run `make`. Convert any video with", MUTED),
            ("user/video/tools/encode.sh. R rescans.", MUTED),
        ];
        for (i, (line, colour)) in lines.iter().enumerate() {
            screen.text(margin, top() + i * ch, line, *colour);
        }
    } else {
        let rows = page_rows(screen).max(1);
        if selected < *scroll {
            *scroll = selected;
        } else if selected >= *scroll + rows {
            *scroll = selected + 1 - rows;
        }
        for (row, (index, entry)) in entries.iter().enumerate().skip(*scroll).take(rows).enumerate() {
            let y = top() + row * ROW_H;
            let text_y = y + (ROW_H - ch) / 2;
            if index == selected {
                screen.rect(margin - 12, y, width.saturating_sub(2 * margin) + 24, ROW_H, SELECTED);
                screen.rect(margin - 12, y, 4, ROW_H, ACCENT);
            }
            let (colour, detail) = if index == selected { (TEXT, TEXT) } else { (TEXT, MUTED) };
            screen.text(margin + 2 * cw, text_y, &ellipsize(&entry.name, name_room), colour);
            match &entry.header {
                Some(header) => {
                    screen.text(length_x, text_y, &clock_text(header.seconds() as usize), detail);
                    let picture = format!("{}x{}@{}", header.width, header.height, header.fps);
                    screen.text(picture_x, text_y, &picture, detail);
                }
                None => {
                    screen.text(length_x, text_y, "not a .BAP video", WARNING);
                }
            }
            screen.text(size_x, text_y, &size_text(entry.size), detail);
        }
        if entries.len() > rows {
            let shown = format!("{}-{} of {}", *scroll + 1, (*scroll + rows).min(entries.len()), entries.len());
            let y = top() + rows * ROW_H + 4;
            screen.text(width.saturating_sub(margin + Screen::text_width(&shown)), y, &shown, MUTED);
        }
    }

    // Footer: a message, then the keys.
    let footer_y = screen.height.saturating_sub(footer_h());
    if let Some(message) = message {
        screen.text(margin, footer_y, &ellipsize(message, width.saturating_sub(2 * margin) / cw), WARNING);
    }
    let hints = "Enter  play from here     Up/Down  choose     R  rescan     Esc  quit";
    screen.text(margin, footer_y + ch + 12, hints, MUTED);
    screen.text(margin, footer_y + 2 * ch + 12, "While playing, press H for every key.", MUTED);
    screen.commit();
}

fn size_text(bytes: u64) -> String {
    let tenths = bytes * 10 / (1024 * 1024);
    if tenths >= 10 {
        format!("{}.{} MB", tenths / 10, tenths % 10)
    } else {
        format!("{} KB", bytes.div_ceil(1024))
    }
}
