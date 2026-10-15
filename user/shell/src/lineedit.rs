//! Line editing for the shell.
//!
//! The console service will happily edit a line for a client that asks for one
//! with `CONSOLE_READ_LINE`, but it does so blind: no history, no cursor
//! movement, no completion. This module reads raw keys with
//! `CONSOLE_READ_KEY` and does that editing here instead, where the shell's
//! history and its filesystem connection live.
//!
//! The console's cursor is not drawn (its `TextGrid` keeps no character buffer,
//! so it cannot repaint the cell under a block), so the insertion point is
//! invisible. Everything that changes the text is visible, which is enough for
//! a command line; the windowed editor draws its own cursor.

use alloc::string::String;
use alloc::vec::Vec;

use libnode::abi::proto;
use libnode::{Channel, Message};

/// Command words, for completion at the start of a line.
const COMMANDS: &[&str] = &[
    "help", "pwd", "cd", "ls", "cat", "touch", "write", "append", "mkdir", "rm", "stat", "df", "fstest",
    "run", "echo", "clear", "about", "draw",
];

/// Read and edit one line, return it without the newline. Pushes non-empty
/// lines onto `history`.
pub fn read_line(console: &Channel, files: Option<&Channel>, cwd: &str, history: &mut Vec<String>) -> String {
    write(console, crate::PROMPT);
    let Some((cols, _rows)) = info(console) else { return String::new() };
    let Some((col0, row0)) = cursor(console) else { return String::new() };

    let mut line: Vec<char> = Vec::new();
    let mut cursor = 0usize;
    let mut history_index = history.len();
    let mut saved: Vec<char> = Vec::new();

    loop {
        let Some(key) = read_key(console) else { return String::new() };

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
                0x48 => {
                    if history_index > 0 {
                        if history_index == history.len() {
                            saved = line.clone();
                        }
                        history_index -= 1;
                        line = history[history_index].chars().collect();
                        cursor = line.len();
                        render(console, col0, row0, cols, &line, cursor, line.len());
                    }
                }
                0x50 => {
                    if history_index < history.len() {
                        history_index += 1;
                        line = if history_index == history.len() {
                            saved.clone()
                        } else {
                            history[history_index].chars().collect()
                        };
                        cursor = line.len();
                        render(console, col0, row0, cols, &line, cursor, line.len());
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
                if !result.trim().is_empty() && history.last().map(|last| last != &result).unwrap_or(true) {
                    history.push(result.clone());
                }
                return result;
            }
            0x08 => {
                if cursor > 0 {
                    let previous = line.len();
                    line.remove(cursor - 1);
                    cursor -= 1;
                    render(console, col0, row0, cols, &line, cursor, previous);
                }
            }
            0x09 => {
                complete(console, files, cwd, col0, row0, cols, &mut line, &mut cursor);
                history_index = history.len();
            }
            0x03 => {
                write(console, "^C\n");
                return String::new();
            }
            0x20..=0x7e => {
                let previous = line.len();
                line.insert(cursor, key.ascii as char);
                cursor += 1;
                render(console, col0, row0, cols, &line, cursor, previous);
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
/// `previous` is the length the line had before the edit, so a line that shrank
/// has its tail erased. The whole line is rewritten in one `WRITE_TEXT`, so a
/// long line costs one round trip, not one per character. Wrapping is not
/// handled: a line longer than the window's row wraps on screen and the
/// absolute positions this computes drift. Commands that long are rare enough
/// that the alternative -- a full terminal emulator here -- does not earn its
/// keep.
fn render(console: &Channel, col0: u32, row0: u32, cols: u32, line: &[char], cursor: usize, previous: usize) {
    place(console, col0, row0, cols, 0);
    let text: String = line.iter().collect();
    write(console, &text);
    if previous > line.len() {
        let mut spaces = String::new();
        for _ in line.len()..previous {
            spaces.push(' ');
        }
        write(console, &spaces);
    }
    place(console, col0, row0, cols, cursor);
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
/// ambiguous set extends the word to its longest common prefix. Candidates are
/// never listed -- that would need the screen's cursor bookkeeping that this
/// editor does not keep.
fn complete(
    console: &Channel,
    files: Option<&Channel>,
    cwd: &str,
    col0: u32,
    row0: u32,
    cols: u32,
    line: &mut Vec<char>,
    cursor: &mut usize,
) {
    let mut start = *cursor;
    while start > 0 && !line[start - 1].is_whitespace() {
        start -= 1;
    }
    let token: String = line[start..*cursor].iter().collect();

    let candidates: Vec<String> = if start == 0 {
        COMMANDS
            .iter()
            .filter(|command| command.starts_with(&token))
            .map(|command| String::from(*command))
            .collect()
    } else if let Some(files) = files {
        let (directory, prefix) = split_directory(&token);
        list_names(files, cwd, &directory)
            .into_iter()
            .filter(|name| name.starts_with(&prefix))
            .collect()
    } else {
        Vec::new()
    };

    if candidates.is_empty() {
        return;
    }
    let completion = if candidates.len() == 1 {
        let mut only = candidates[0].clone();
        if start == 0 {
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
    render(console, col0, row0, cols, line, *cursor, previous);
}

/// Split a partially typed path into `(directory, last component)`. The
/// directory keeps its trailing slash, or is empty for a bare name.
fn split_directory(token: &str) -> (String, String) {
    match token.rfind('/') {
        Some(index) => (String::from(&token[..=index]), String::from(&token[index + 1..])),
        None => (String::new(), String::from(token)),
    }
}

/// The names in the directory a completion token names.
fn list_names(files: &Channel, cwd: &str, directory: &str) -> Vec<String> {
    let path = crate::files::resolve(cwd, if directory.is_empty() { "." } else { directory });
    let Ok(reply) = files.call_with(Message::new(proto::FS_LIST).with_payload(path.as_bytes()), 8192, 0) else {
        return Vec::new();
    };
    let mut names = Vec::new();
    for record in reply.payload.split(|byte| *byte == 0) {
        let Ok(text) = core::str::from_utf8(record) else { continue };
        if let Some(name) = text.split('\t').next() {
            if !name.is_empty() {
                names.push(String::from(name));
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
        let common = prefix.iter().zip(chars.iter()).take_while(|(a, b)| a == b).count();
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

fn write(console: &Channel, text: &str) {
    let _ = console.call(Message::new(proto::CONSOLE_WRITE_TEXT).with_payload(text.as_bytes()));
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
