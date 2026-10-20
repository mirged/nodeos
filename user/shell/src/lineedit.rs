//! Line editing for the shell.
//!
//! The console service will happily edit a line for a client that asks for one
//! with `CONSOLE_READ_LINE`, but it does so blind: no history, no cursor
//! movement, no completion. This module reads raw keys with
//! `CONSOLE_READ_KEY` and does that editing here instead, where the shell's
//! history and its filesystem connection live.
//!
//! The console paints its cursor and retains scrollback. This editor tracks
//! the command's origin as wraps scroll the screen, and keeps bounded history.

use alloc::string::String;
use alloc::vec::Vec;

use libnode::abi::proto;
use libnode::{Channel, Message};

/// Command words, for completion at the start of a line.
const COMMANDS: &[&str] = &[
    "help", "pwd", "cd", "ls", "cat", "touch", "write", "append", "mkdir", "rm", "stat", "df",
    "fstest", "run", "echo", "clear", "about", "history", "source", "cp", "grep", "more", "uptime",
    "font", "mv", "head", "tail", "wc", "find", "rmdir", "export", "unset", "set", "env", "true", "false",
];

/// Read and edit one line, return it without the newline. Pushes non-empty
/// lines onto `history`.
pub fn read_line(
    console: &Channel,
    files: Option<&Channel>,
    cwd: &str,
    history: &mut Vec<String>,
) -> String {
    prompt(console, cwd);
    let Some((cols, rows)) = info(console) else {
        return String::new();
    };
    let Some((col0, mut row0)) = cursor(console) else {
        return String::new();
    };

    let mut line: Vec<char> = Vec::new();
    let mut cursor = 0usize;
    let mut history_index = history.len();
    let mut saved: Vec<char> = Vec::new();
    let mut last_was_tab = false;

    loop {
        let Some(key) = read_key(console) else {
            return String::new();
        };

        let show_matches = key.ascii == b'\t' && last_was_tab;
        last_was_tab = key.ascii == b'\t';
        if key.modifiers & proto::wm_mod::CTRL != 0 {
            if key.extended != 0 && matches!(key.scancode, 0x4b | 0x4d) {
                if key.scancode == 0x4b {
                    while cursor > 0 && line[cursor - 1].is_whitespace() {
                        cursor -= 1;
                    }
                    while cursor > 0 && !line[cursor - 1].is_whitespace() {
                        cursor -= 1;
                    }
                } else {
                    while cursor < line.len() && !line[cursor].is_whitespace() {
                        cursor += 1;
                    }
                    while cursor < line.len() && line[cursor].is_whitespace() {
                        cursor += 1;
                    }
                }
                place(console, col0, row0, cols, cursor);
                continue;
            }
            let previous = line.len();
            match key.ascii.to_ascii_lowercase() {
                b'd' => {
                    if cursor < line.len() {
                        line.remove(cursor);
                    }
                }
                b'a' => cursor = 0,
                b'e' => cursor = line.len(),
                b'u' => {
                    line.drain(..cursor);
                    cursor = 0;
                }
                b'k' => line.truncate(cursor),
                b'w' => {
                    let end = cursor;
                    while cursor > 0 && line[cursor - 1].is_whitespace() {
                        cursor -= 1;
                    }
                    while cursor > 0 && !line[cursor - 1].is_whitespace() {
                        cursor -= 1;
                    }
                    line.drain(cursor..end);
                }
                b'l' => {
                    let _ = console.call_with(Message::new(proto::CONSOLE_CLEAR), 64, 0);
                    prompt(console, cwd);
                    row0 = 0;
                }
                b'c' => {
                    write(console, "^C\n");
                    return String::new();
                }
                _ => continue,
            }
            render(
                console, col0, &mut row0, cols, rows, &line, cursor, previous,
            );
            continue;
        }

        // Extended keys carry no character, so they are matched by scancode.
        if key.extended != 0 {
            match key.scancode {
                0x4B => {
                    if cursor > 0 {
                        cursor -= 1;
                        place(console, col0, row0, cols, cursor);
                    }
                }
                0x4D => {
                    if cursor < line.len() {
                        cursor += 1;
                        place(console, col0, row0, cols, cursor);
                    }
                }
                0x47 => {
                    cursor = 0;
                    place(console, col0, row0, cols, cursor);
                }
                0x4F => {
                    cursor = line.len();
                    place(console, col0, row0, cols, cursor);
                }
                0x53 => {
                    if cursor < line.len() {
                        let previous = line.len();
                        line.remove(cursor);
                        render(
                            console, col0, &mut row0, cols, rows, &line, cursor, previous,
                        );
                    }
                }
                0x48 => {
                    if history_index > 0 {
                        if history_index == history.len() {
                            saved = line.clone();
                        }
                        history_index -= 1;
                        let previous = line.len();
                        line = history[history_index].chars().collect();
                        cursor = line.len();
                        render(
                            console, col0, &mut row0, cols, rows, &line, cursor, previous,
                        );
                    }
                }
                0x50 => {
                    if history_index < history.len() {
                        history_index += 1;
                        let previous = line.len();
                        line = if history_index == history.len() {
                            saved.clone()
                        } else {
                            history[history_index].chars().collect()
                        };
                        cursor = line.len();
                        render(
                            console, col0, &mut row0, cols, rows, &line, cursor, previous,
                        );
                    }
                }
                _ => {}
            }
            continue;
        }

        match key.ascii {
            b'\n' => {
                write(console, "\n");
                let result: String = line.iter().collect();
                if !result.trim().is_empty()
                    && history.last().map(|last| last != &result).unwrap_or(true)
                {
                    if history.len() >= 128 {
                        history.remove(0);
                    }
                    history.push(result.clone());
                }
                return result;
            }
            0x08 => {
                if cursor > 0 {
                    let previous = line.len();
                    line.remove(cursor - 1);
                    cursor -= 1;
                    render(
                        console, col0, &mut row0, cols, rows, &line, cursor, previous,
                    );
                }
            }
            0x09 => {
                complete(
                    console,
                    files,
                    cwd,
                    col0,
                    &mut row0,
                    cols,
                    rows,
                    &mut line,
                    &mut cursor,
                    show_matches,
                );
                history_index = history.len();
            }
            0x03 => {
                write(console, "^C\n");
                return String::new();
            }
            0x20..=0x7e => {
                if line.len() >= 2048 {
                    continue;
                }
                let previous = line.len();
                line.insert(cursor, key.ascii as char);
                cursor += 1;
                render(
                    console, col0, &mut row0, cols, rows, &line, cursor, previous,
                );
                history_index = history.len();
            }
            _ => {}
        }
    }
}

// ---------------------------------------------------------------------------
// Rendering
// ---------------------------------------------------------------------------

/// Redraw the input line and put the cursor at character `cursor`.
///
/// `previous` is the old length, allowing shorter recalls to erase the suffix.
/// Scroll before painting a wrapped line so its origin stays on screen.
fn render(
    console: &Channel,
    col0: u32,
    row0: &mut u32,
    cols: u32,
    rows: u32,
    line: &[char],
    cursor: usize,
    previous: usize,
) {
    let last = *row0 + (col0 as usize + line.len().max(previous)) as u32 / cols.max(1);
    let scroll = last.saturating_sub(rows.saturating_sub(1));
    for _ in 0..scroll {
        set_cursor(console, 0, rows.saturating_sub(1));
        write(console, "\n");
    }
    *row0 = row0.saturating_sub(scroll);
    place(console, col0, *row0, cols, 0);
    let text: String = line.iter().collect();
    write(console, &text);
    if previous > line.len() {
        let mut spaces = String::new();
        for _ in line.len()..previous {
            spaces.push(' ');
        }
        write(console, &spaces);
    }
    place(console, col0, *row0, cols, cursor);
}

/// Move the console cursor to character `index` of the input line.
fn place(console: &Channel, col0: u32, row0: u32, cols: u32, index: usize) {
    let total = col0 as usize + index;
    let cols = cols.max(1) as usize;
    set_cursor(console, (total % cols) as u32, row0 + (total / cols) as u32);
}

// ---------------------------------------------------------------------------
// Completion
// ---------------------------------------------------------------------------

/// Complete the word before the cursor: a command at the start of the line, a
/// file name anywhere else.
///
/// A single match is inserted whole (with a trailing space after a command); an
/// ambiguous set extends the common prefix. A repeated Tab lists candidates,
/// then restores the prompt, editable line and insertion point.
fn complete(
    console: &Channel,
    files: Option<&Channel>,
    cwd: &str,
    col0: u32,
    row0: &mut u32,
    cols: u32,
    rows: u32,
    line: &mut Vec<char>,
    cursor: &mut usize,
    show_matches: bool,
) {
    let mut start = *cursor;
    while start > 0 && !line[start - 1].is_whitespace() {
        start -= 1;
    }
    let token: String = line[start..*cursor].iter().collect();

    let candidates: Vec<String> = if start == 0 {
        let mut names: Vec<String> = COMMANDS.iter().map(|name| String::from(*name)).collect();
        if let Some(files) = files {
            names.extend(
                list_names(files, "/", "/bin/")
                    .into_iter()
                    .filter_map(|name| {
                        let name = name.to_ascii_lowercase();
                        name.strip_suffix(".nxf")
                            .filter(|name| {
                                !matches!(
                                    *name,
                                    "init" | "display" | "console" | "fs" | "audio" | "shell"
                                )
                            })
                            .map(String::from)
                    }),
            );
        }
        names.sort();
        names.dedup();
        names
            .into_iter()
            .filter(|name| name.starts_with(&token.to_ascii_lowercase()))
            .collect()
    } else if let Some(files) = files {
        let (directory, prefix) = split_directory(&token);
        list_names(files, cwd, &directory)
            .into_iter()
            .filter(|name| {
                name.to_ascii_lowercase()
                    .starts_with(&prefix.to_ascii_lowercase())
            })
            .collect()
    } else {
        Vec::new()
    };

    if candidates.is_empty() {
        return;
    }
    if candidates.len() > 1 && show_matches {
        write(console, "\n\x1b[2m");
        crate::write_text(console, &candidates.join("   "));
        write(console, "\x1b[0m\n");
        prompt(console, cwd);
        if let Some((_, row)) = crate::lineedit::cursor(console) {
            *row0 = row;
        }
        render(console, col0, row0, cols, rows, line, *cursor, line.len());
        return;
    }
    let completion = if candidates.len() == 1 {
        let mut only = candidates[0].clone();
        if start == 0 || !only.ends_with('/') {
            only.push(' ');
        }
        only
    } else {
        common_prefix(&candidates)
    };

    let new_token: Vec<char> = completion.chars().collect();
    if new_token.len() <= *cursor - start {
        return;
    }
    let previous = line.len();
    line.splice(start..*cursor, new_token.iter().copied());
    *cursor = start + new_token.len();
    render(console, col0, row0, cols, rows, line, *cursor, previous);
}

/// Split a partially typed path into `(directory, last component)`. The
/// directory keeps its trailing slash, or is empty for a bare name.
fn split_directory(token: &str) -> (String, String) {
    match token.rfind('/') {
        Some(index) => (
            String::from(&token[..=index]),
            String::from(&token[index + 1..]),
        ),
        None => (String::new(), String::from(token)),
    }
}

/// The names in the directory a completion token names.
fn list_names(files: &Channel, cwd: &str, directory: &str) -> Vec<String> {
    let path = crate::files::resolve(cwd, if directory.is_empty() { "." } else { directory });
    let Ok(reply) = files.call_with(
        Message::new(proto::FS_LIST).with_payload(path.as_bytes()),
        8192,
        0,
    ) else {
        return Vec::new();
    };
    let mut names = Vec::new();
    for record in reply.payload.split(|byte| *byte == 0) {
        let Ok(text) = core::str::from_utf8(record) else {
            continue;
        };
        let mut fields = text.split('\t');
        if let Some(name) = fields.next() {
            if !name.is_empty() {
                let mut name = String::from(name);
                if fields.next() == Some("d") {
                    name.push('/');
                }
                names.push(name);
            }
        }
    }
    names
}

/// The longest string every candidate starts with.
fn common_prefix(candidates: &[String]) -> String {
    let mut prefix: Vec<char> = candidates[0].chars().collect();
    for candidate in &candidates[1..] {
        let chars: Vec<char> = candidate.chars().collect();
        let common = prefix
            .iter()
            .zip(chars.iter())
            .take_while(|(a, b)| a == b)
            .count();
        prefix.truncate(common);
        if prefix.is_empty() {
            break;
        }
    }
    prefix.into_iter().collect()
}

// ---------------------------------------------------------------------------
// The console protocol, wrapped
// ---------------------------------------------------------------------------

fn prompt(console: &Channel, cwd: &str) {
    write(
        console,
        &alloc::format!("\x1b[36m{}\x1b[0m \x1b[32m>\x1b[0m ", cwd),
    );
}

fn write(console: &Channel, text: &str) {
    crate::write_text(console, text);
}

fn info(console: &Channel) -> Option<(u32, u32)> {
    let reply = console.call(Message::new(proto::CONSOLE_GET_INFO)).ok()?;
    let info = proto::decode::<proto::ConsoleInfo>(&reply.payload)?;
    Some((info.cols, info.rows))
}

fn cursor(console: &Channel) -> Option<(u32, u32)> {
    let reply = console.call(Message::new(proto::CONSOLE_GET_CURSOR)).ok()?;
    let cursor = proto::decode::<proto::ConsoleCursor>(&reply.payload)?;
    Some((cursor.col, cursor.row))
}

fn set_cursor(console: &Channel, col: u32, row: u32) {
    let cursor = proto::Encoded::new(proto::ConsoleCursor { col, row });
    let _ = console.call(Message::new(proto::CONSOLE_SET_CURSOR).with_payload(cursor.bytes()));
}

fn read_key(console: &Channel) -> Option<proto::ConsoleKey> {
    let reply = console.call(Message::new(proto::CONSOLE_READ_KEY)).ok()?;
    proto::decode::<proto::ConsoleKey>(&reply.payload)
}
