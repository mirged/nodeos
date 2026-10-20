//! A document: its lines, the cursor and selection, and its undo history.
//!
//! Positions are (line, column) in characters, never bytes, so a cursor can
//! not land inside a multi-byte character. Every edit goes through
//! [`Buffer::replace`], which records an undo step first; that one choke point
//! is what makes undo exact. Like the highlighter, this file has no NodeOS
//! dependencies and is tested on the host.

extern crate alloc;
use alloc::string::String;
use alloc::vec::Vec;

use crate::highlight::Lang;

/// Spaces per indentation level, and what Tab inserts.
pub const INDENT: usize = 4;
/// Undo steps kept per document.
const UNDO_LIMIT: usize = 256;

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, PartialOrd, Ord)]
pub struct Pos {
    pub line: usize,
    pub col: usize,
}

impl Pos {
    pub const fn new(line: usize, col: usize) -> Self {
        Self { line, col }
    }
}

#[derive(Clone)]
struct Snapshot {
    lines: Vec<String>,
    cursor: Pos,
}

/// What the last edit was, so consecutive typing undoes as one step.
#[derive(Clone, Copy, PartialEq, Eq)]
enum EditKind {
    None,
    Typing,
    Deleting,
    Other,
}

pub struct Buffer {
    pub lines: Vec<String>,
    pub cursor: Pos,
    /// The other end of the selection, when there is one.
    pub anchor: Option<Pos>,
    /// First visible line and column.
    pub top: usize,
    pub left: usize,
    pub path: String,
    pub lang: Lang,
    pub dirty: bool,
    /// The column vertical movement aims for, kept across short lines.
    goal: Option<usize>,
    undo: Vec<Snapshot>,
    redo: Vec<Snapshot>,
    last: EditKind,
}

/// Byte offset of character `col` in `line`, or its end.
pub fn byte_at(line: &str, col: usize) -> usize {
    line.char_indices().nth(col).map(|(index, _)| index).unwrap_or(line.len())
}

pub fn char_len(line: &str) -> usize {
    line.chars().count()
}

fn is_word(c: char) -> bool {
    c.is_alphanumeric() || c == '_'
}

fn closing(c: char) -> Option<char> {
    match c {
        '(' => Some(')'),
        '[' => Some(']'),
        '{' => Some('}'),
        '"' => Some('"'),
        _ => None,
    }
}

impl Buffer {
    pub fn new(path: String, text: &str) -> Self {
        let mut lines: Vec<String> = text
            .split('\n')
            .map(|line| {
                let line = line.strip_suffix('\r').unwrap_or(line);
                // Tabs become spaces to the next stop: the font has no tab
                // glyph, and every column below counts one cell per character.
                let mut out = String::new();
                for c in line.chars() {
                    if c == '\t' {
                        let pad = INDENT - char_len(&out) % INDENT;
                        out.extend(core::iter::repeat_n(' ', pad));
                    } else {
                        out.push(c);
                    }
                }
                out
            })
            .collect();
        if lines.is_empty() {
            lines.push(String::new());
        }
        Self {
            lang: Lang::for_path(&path),
            lines,
            cursor: Pos::default(),
            anchor: None,
            top: 0,
            left: 0,
            path,
            dirty: false,
            goal: None,
            undo: Vec::new(),
            redo: Vec::new(),
            last: EditKind::None,
        }
    }

    /// The document as it is saved: lines joined by `\n`.
    pub fn text(&self) -> String {
        self.lines.join("\n")
    }

    pub fn line_len(&self, line: usize) -> usize {
        self.lines.get(line).map(|l| char_len(l)).unwrap_or(0)
    }

    pub fn char_at(&self, pos: Pos) -> Option<char> {
        self.lines.get(pos.line)?.chars().nth(pos.col)
    }

    fn clamp(&self, pos: Pos) -> Pos {
        let line = pos.line.min(self.lines.len() - 1);
        Pos::new(line, pos.col.min(self.line_len(line)))
    }

    // ------------------------------------------------------------------
    // Selection

    /// The selection as an ordered range, if it is not empty.
    pub fn selection(&self) -> Option<(Pos, Pos)> {
        let anchor = self.anchor?;
        if anchor == self.cursor {
            return None;
        }
        Some(if anchor < self.cursor { (anchor, self.cursor) } else { (self.cursor, anchor) })
    }

    pub fn text_between(&self, start: Pos, end: Pos) -> String {
        if start.line == end.line {
            let line = &self.lines[start.line];
            return String::from(&line[byte_at(line, start.col)..byte_at(line, end.col)]);
        }
        let first = &self.lines[start.line];
        let mut out = String::from(&first[byte_at(first, start.col)..]);
        for line in &self.lines[start.line + 1..end.line] {
            out.push('\n');
            out.push_str(line);
        }
        out.push('\n');
        let last = &self.lines[end.line];
        out.push_str(&last[..byte_at(last, end.col)]);
        out
    }

    pub fn selected_text(&self) -> Option<String> {
        self.selection().map(|(start, end)| self.text_between(start, end))
    }

    pub fn select_all(&mut self) {
        self.anchor = Some(Pos::new(0, 0));
        let last = self.lines.len() - 1;
        self.cursor = Pos::new(last, self.line_len(last));
    }

    /// Move the cursor, extending the selection or dropping it.
    fn move_to(&mut self, pos: Pos, select: bool) {
        if select {
            if self.anchor.is_none() {
                self.anchor = Some(self.cursor);
            }
        } else {
            self.anchor = None;
        }
        self.cursor = self.clamp(pos);
        self.last = EditKind::None;
    }

    // ------------------------------------------------------------------
    // Editing: everything goes through `replace`

    fn checkpoint(&mut self, kind: EditKind) {
        let coalesce = kind == self.last && matches!(kind, EditKind::Typing | EditKind::Deleting);
        if !coalesce {
            self.undo.push(Snapshot { lines: self.lines.clone(), cursor: self.cursor });
            if self.undo.len() > UNDO_LIMIT {
                self.undo.remove(0);
            }
        }
        self.redo.clear();
        self.last = kind;
        self.dirty = true;
        self.goal = None;
    }

    /// Replace `start..end` with `text` and put the cursor after it.
    fn replace(&mut self, start: Pos, end: Pos, text: &str, kind: EditKind) {
        self.checkpoint(kind);
        let tail = {
            let line = &self.lines[end.line];
            String::from(&line[byte_at(line, end.col)..])
        };
        let head = {
            let line = &self.lines[start.line];
            String::from(&line[..byte_at(line, start.col)])
        };
        let mut pieces: Vec<&str> = text.split('\n').collect();
        if pieces.is_empty() {
            pieces.push("");
        }
        let count = pieces.len();
        let mut new_lines: Vec<String> = Vec::with_capacity(count);
        for (index, piece) in pieces.iter().enumerate() {
            let mut line = String::new();
            if index == 0 {
                line.push_str(&head);
            }
            line.push_str(piece);
            new_lines.push(line);
        }
        let cursor = Pos::new(start.line + count - 1, char_len(&new_lines[count - 1]));
        new_lines[count - 1].push_str(&tail);
        self.lines.splice(start.line..=end.line, new_lines);
        self.cursor = cursor;
        self.anchor = None;
    }

    fn delete_selection(&mut self) -> bool {
        match self.selection() {
            Some((start, end)) => {
                self.replace(start, end, "", EditKind::Other);
                true
            }
            None => false,
        }
    }

    /// Insert text as if pasted: it replaces the selection.
    pub fn insert(&mut self, text: &str) {
        let (start, end) = self.selection().unwrap_or((self.cursor, self.cursor));
        self.replace(start, end, text, EditKind::Other);
    }

    /// Type one character, with the conveniences a code editor has: closing
    /// brackets and quotes pair, typing a closer steps over an auto-inserted
    /// one, and `}` on an indented blank line dedents it.
    pub fn type_char(&mut self, c: char) {
        if let Some((start, end)) = self.selection() {
            // A bracket or quote around a selection wraps it.
            if let Some(close) = closing(c) {
                let inner = self.text_between(start, end);
                let mut wrapped = String::new();
                wrapped.push(c);
                wrapped.push_str(&inner);
                wrapped.push(close);
                self.replace(start, end, &wrapped, EditKind::Other);
                return;
            }
            self.replace(start, end, "", EditKind::Other);
        }
        let here = self.cursor;
        let next = self.char_at(here);
        let code = self.lang != Lang::Plain;
        if code && matches!(c, ')' | ']' | '}' | '"') && next == Some(c) {
            self.move_to(Pos::new(here.line, here.col + 1), false);
            return;
        }
        if c == '}' {
            let line = &self.lines[here.line];
            let before = &line[..byte_at(line, here.col)];
            if !before.is_empty() && before.chars().all(|c| c == ' ') {
                let keep = before.len().saturating_sub(INDENT);
                self.replace(Pos::new(here.line, keep), here, "}", EditKind::Typing);
                return;
            }
        }
        let pairs = code && next.is_none_or(|n| n.is_whitespace() || matches!(n, ')' | ']' | '}' | ',' | ';'));
        let previous = here.col.checked_sub(1).and_then(|col| self.char_at(Pos::new(here.line, col)));
        let quote_ok = c != '"' || previous.is_none_or(|p| !is_word(p));
        match closing(c) {
            Some(close) if pairs && quote_ok => {
                let mut pair = String::new();
                pair.push(c);
                pair.push(close);
                self.replace(here, here, &pair, EditKind::Other);
                self.cursor.col -= 1;
            }
            _ => {
                let mut one = [0u8; 4];
                self.replace(here, here, c.encode_utf8(&mut one), EditKind::Typing);
                if c == ' ' {
                    self.last = EditKind::Other;
                }
            }
        }
    }

    /// Enter: keep the indentation, indent after an opening bracket, and
    /// put a closing bracket typed right after it on its own line.
    pub fn newline(&mut self) {
        self.delete_selection();
        let here = self.cursor;
        let line = &self.lines[here.line];
        let indent = line.chars().take_while(|c| *c == ' ').count().min(here.col);
        let before = here.col.checked_sub(1).and_then(|col| self.char_at(Pos::new(here.line, col)));
        let after = self.char_at(here);
        let opens = matches!(before, Some('{' | '(' | '['));
        let base: String = core::iter::repeat_n(' ', indent).collect();
        let inner: String = core::iter::repeat_n(' ', indent + if opens { INDENT } else { 0 }).collect();
        if opens && after.is_some_and(|a| closing(before.unwrap()) == Some(a)) {
            let text = alloc::format!("\n{inner}\n{base}");
            self.replace(here, here, &text, EditKind::Other);
            self.cursor = Pos::new(here.line + 1, inner.len());
        } else {
            self.replace(here, here, &alloc::format!("\n{inner}"), EditKind::Other);
        }
    }

    pub fn backspace(&mut self) {
        if self.delete_selection() {
            return;
        }
        let here = self.cursor;
        if here.col > 0 {
            let line = &self.lines[here.line];
            let before = &line[..byte_at(line, here.col)];
            // In leading indentation, remove a whole level.
            let width = if before.chars().all(|c| c == ' ') {
                let extra = here.col % INDENT;
                if extra == 0 { INDENT } else { extra }
            } else {
                1
            };
            let mut start = Pos::new(here.line, here.col - width.min(here.col));
            // Deleting an opening bracket also deletes the closer typed with it.
            let mut end = here;
            let opened = self.char_at(Pos::new(here.line, here.col - 1));
            if width == 1 && opened.and_then(closing).is_some() && self.char_at(here) == opened.and_then(closing) {
                end.col += 1;
            }
            start.col = start.col.min(here.col);
            self.replace(start, end, "", EditKind::Deleting);
        } else if here.line > 0 {
            let start = Pos::new(here.line - 1, self.line_len(here.line - 1));
            self.replace(start, here, "", EditKind::Deleting);
        }
    }

    pub fn delete(&mut self) {
        if self.delete_selection() {
            return;
        }
        let here = self.cursor;
        let end = if here.col < self.line_len(here.line) {
            Pos::new(here.line, here.col + 1)
        } else if here.line + 1 < self.lines.len() {
            Pos::new(here.line + 1, 0)
        } else {
            return;
        };
        self.replace(here, end, "", EditKind::Deleting);
    }

    /// Tab: indent the selected lines, or insert spaces to the next stop.
    pub fn tab(&mut self, outdent: bool) {
        let (first, last) = match self.selection() {
            Some((start, end)) => (start.line, if end.col == 0 && end.line > start.line { end.line - 1 } else { end.line }),
            None if outdent => (self.cursor.line, self.cursor.line),
            None => {
                let pad = INDENT - self.cursor.col % INDENT;
                let spaces: String = core::iter::repeat_n(' ', pad).collect();
                self.replace(self.cursor, self.cursor, &spaces, EditKind::Other);
                return;
            }
        };
        let had_selection = self.anchor.is_some();
        let lines: Vec<String> = self.lines[first..=last]
            .iter()
            .map(|line| {
                if outdent {
                    let strip = line.chars().take(INDENT).take_while(|c| *c == ' ').count();
                    String::from(&line[strip..])
                } else if line.is_empty() {
                    String::new()
                } else {
                    alloc::format!("{:width$}{}", "", line, width = INDENT)
                }
            })
            .collect();
        self.replace_lines(first, last, lines, had_selection);
    }

    /// Replace whole lines, keeping a selection over them.
    fn replace_lines(&mut self, first: usize, last: usize, lines: Vec<String>, select: bool) {
        let end = Pos::new(last, self.line_len(last));
        let text = lines.join("\n");
        let cursor = self.cursor;
        self.replace(Pos::new(first, 0), end, &text, EditKind::Other);
        if select {
            self.anchor = Some(Pos::new(first, 0));
            self.cursor = Pos::new(last, self.line_len(last));
        } else {
            self.cursor = self.clamp(Pos::new(cursor.line, cursor.col.min(self.line_len(cursor.line))));
        }
    }

    /// Comment or uncomment the selected lines (or the cursor's line).
    pub fn toggle_comment(&mut self) {
        let Some(mark) = self.lang.line_comment() else { return };
        let (first, last) = match self.selection() {
            Some((start, end)) => (start.line, if end.col == 0 && end.line > start.line { end.line - 1 } else { end.line }),
            None => (self.cursor.line, self.cursor.line),
        };
        let uncomment = self.lines[first..=last]
            .iter()
            .filter(|line| !line.trim().is_empty())
            .all(|line| line.trim_start().starts_with(mark));
        let indent = self.lines[first..=last]
            .iter()
            .filter(|line| !line.trim().is_empty())
            .map(|line| line.chars().take_while(|c| *c == ' ').count())
            .min()
            .unwrap_or(0);
        let lines: Vec<String> = self.lines[first..=last]
            .iter()
            .map(|line| {
                if line.trim().is_empty() {
                    line.clone()
                } else if uncomment {
                    let at = line.find(mark).unwrap_or(0);
                    let mut rest = &line[at + mark.len()..];
                    if rest.starts_with(' ') {
                        rest = &rest[1..];
                    }
                    alloc::format!("{}{}", &line[..at], rest)
                } else {
                    alloc::format!("{}{} {}", &line[..indent], mark, &line[indent..])
                }
            })
            .collect();
        let select = self.anchor.is_some();
        self.replace_lines(first, last, lines, select);
    }

    pub fn duplicate_line(&mut self) {
        let line = self.cursor.line;
        let text = alloc::format!("\n{}", self.lines[line]);
        let end = Pos::new(line, self.line_len(line));
        let col = self.cursor.col;
        self.replace(end, end, &text, EditKind::Other);
        self.cursor = Pos::new(line + 1, col);
    }

    /// Remove the cursor's line (or the selected lines) and return the text,
    /// for cutting a line with nothing selected.
    pub fn cut_line(&mut self) -> String {
        let line = self.cursor.line;
        let mut text = self.lines[line].clone();
        text.push('\n');
        if self.lines.len() == 1 {
            self.replace(Pos::new(0, 0), Pos::new(0, self.line_len(0)), "", EditKind::Other);
        } else if line + 1 < self.lines.len() {
            self.replace(Pos::new(line, 0), Pos::new(line + 1, 0), "", EditKind::Other);
        } else {
            let start = Pos::new(line - 1, self.line_len(line - 1));
            self.replace(start, Pos::new(line, self.line_len(line)), "", EditKind::Other);
            self.cursor = Pos::new(line - 1, 0);
        }
        text
    }

    /// Delete the selection and return it.
    pub fn cut(&mut self) -> Option<String> {
        let text = self.selected_text()?;
        self.delete_selection();
        Some(text)
    }

    pub fn undo(&mut self) -> bool {
        let Some(snapshot) = self.undo.pop() else { return false };
        self.redo.push(Snapshot { lines: core::mem::replace(&mut self.lines, snapshot.lines), cursor: self.cursor });
        self.cursor = self.clamp(snapshot.cursor);
        self.anchor = None;
        self.last = EditKind::None;
        self.dirty = true;
        true
    }

    pub fn redo(&mut self) -> bool {
        let Some(snapshot) = self.redo.pop() else { return false };
        self.undo.push(Snapshot { lines: core::mem::replace(&mut self.lines, snapshot.lines), cursor: self.cursor });
        self.cursor = self.clamp(snapshot.cursor);
        self.anchor = None;
        self.last = EditKind::None;
        self.dirty = true;
        true
    }

    // ------------------------------------------------------------------
    // Movement

    pub fn left(&mut self, select: bool, word: bool) {
        if !select {
            if let Some((start, _)) = self.selection() {
                return self.move_to(start, false);
            }
        }
        let mut pos = self.cursor;
        if pos.col == 0 {
            if pos.line > 0 {
                pos = Pos::new(pos.line - 1, self.line_len(pos.line - 1));
            }
        } else if word {
            let chars: Vec<char> = self.lines[pos.line].chars().collect();
            let mut col = pos.col;
            while col > 0 && !is_word(chars[col - 1]) {
                col -= 1;
            }
            while col > 0 && is_word(chars[col - 1]) {
                col -= 1;
            }
            pos.col = col;
        } else {
            pos.col -= 1;
        }
        self.goal = None;
        self.move_to(pos, select);
    }

    pub fn right(&mut self, select: bool, word: bool) {
        if !select {
            if let Some((_, end)) = self.selection() {
                return self.move_to(end, false);
            }
        }
        let mut pos = self.cursor;
        let len = self.line_len(pos.line);
        if pos.col >= len {
            if pos.line + 1 < self.lines.len() {
                pos = Pos::new(pos.line + 1, 0);
            }
        } else if word {
            let chars: Vec<char> = self.lines[pos.line].chars().collect();
            let mut col = pos.col;
            while col < len && !is_word(chars[col]) {
                col += 1;
            }
            while col < len && is_word(chars[col]) {
                col += 1;
            }
            pos.col = col;
        } else {
            pos.col += 1;
        }
        self.goal = None;
        self.move_to(pos, select);
    }

    /// Move by `lines` (negative is up), keeping the goal column.
    pub fn vertical(&mut self, lines: isize, select: bool) {
        let goal = *self.goal.get_or_insert(self.cursor.col);
        let target = (self.cursor.line as isize + lines).clamp(0, self.lines.len() as isize - 1) as usize;
        let pos = Pos::new(target, goal.min(self.line_len(target)));
        self.move_to(pos, select);
        self.goal = Some(goal);
    }

    /// Home: the first non-blank character, then the start of the line.
    pub fn home(&mut self, select: bool) {
        let indent = self.lines[self.cursor.line].chars().take_while(|c| *c == ' ').count();
        let col = if self.cursor.col == indent { 0 } else { indent };
        self.goal = None;
        self.move_to(Pos::new(self.cursor.line, col), select);
    }

    pub fn end(&mut self, select: bool) {
        self.goal = None;
        self.move_to(Pos::new(self.cursor.line, self.line_len(self.cursor.line)), select);
    }

    pub fn jump(&mut self, pos: Pos, select: bool) {
        self.goal = None;
        self.move_to(pos, select);
    }

    // ------------------------------------------------------------------
    // Search

    /// The next match of `needle` (case-insensitive) after `from`, wrapping.
    pub fn find(&self, needle: &str, from: Pos, backwards: bool) -> Option<(Pos, Pos)> {
        if needle.is_empty() {
            return None;
        }
        let needle: Vec<char> = needle.to_lowercase().chars().collect();
        let count = self.lines.len();
        let matches_in = |line: usize| -> Vec<usize> {
            let chars: Vec<char> = self.lines[line].to_lowercase().chars().collect();
            let mut found = Vec::new();
            if chars.len() >= needle.len() {
                for col in 0..=chars.len() - needle.len() {
                    if chars[col..col + needle.len()] == needle[..] {
                        found.push(col);
                    }
                }
            }
            found
        };
        for step in 0..=count {
            let line = if backwards { (from.line + count * 2 - step) % count } else { (from.line + step) % count };
            let found = matches_in(line);
            let col = if backwards {
                found.into_iter().rev().find(|col| step > 0 || *col < from.col)
            } else {
                found.into_iter().find(|col| step > 0 || *col >= from.col)
            };
            if let Some(col) = col {
                return Some((Pos::new(line, col), Pos::new(line, col + needle.len())));
            }
        }
        None
    }

    /// Replace every match of `needle` with `with`, as one undo step.
    pub fn replace_all(&mut self, needle: &str, with: &str) -> usize {
        if needle.is_empty() {
            return 0;
        }
        let lower = needle.to_lowercase();
        let mut count = 0;
        let lines: Vec<String> = self
            .lines
            .iter()
            .map(|line| {
                let mut out = String::new();
                let mut rest = line.as_str();
                while let Some(at) = rest.to_lowercase().find(&lower) {
                    // Lower-casing can change byte lengths outside ASCII; only
                    // replace where the offsets still line up.
                    if !rest.is_char_boundary(at) || !rest.is_char_boundary(at + needle.len()) {
                        break;
                    }
                    out.push_str(&rest[..at]);
                    out.push_str(with);
                    rest = &rest[at + needle.len()..];
                    count += 1;
                }
                out.push_str(rest);
                out
            })
            .collect();
        if count > 0 {
            let last = self.lines.len() - 1;
            self.replace_lines(0, last, lines, false);
        }
        count
    }

    /// The bracket matching the one at or just before the cursor.
    pub fn matching_bracket(&self) -> Option<(Pos, Pos)> {
        let here = self.cursor;
        let candidates = [Some(here), here.col.checked_sub(1).map(|c| Pos::new(here.line, c))];
        for pos in candidates.into_iter().flatten() {
            let Some(c) = self.char_at(pos) else { continue };
            let (open, close, forward) = match c {
                '(' => ('(', ')', true),
                '[' => ('[', ']', true),
                '{' => ('{', '}', true),
                ')' => ('(', ')', false),
                ']' => ('[', ']', false),
                '}' => ('{', '}', false),
                _ => continue,
            };
            let mut depth = 0i32;
            let mut line = pos.line;
            let mut budget = 20_000usize;
            loop {
                let chars: Vec<char> = self.lines[line].chars().collect();
                let cols: Vec<usize> = if forward {
                    (if line == pos.line { pos.col } else { 0 }..chars.len()).collect()
                } else {
                    (0..if line == pos.line { pos.col + 1 } else { chars.len() }).rev().collect()
                };
                for col in cols {
                    budget = budget.checked_sub(1)?;
                    if chars[col] == open {
                        depth += if forward { 1 } else { -1 };
                    } else if chars[col] == close {
                        depth += if forward { -1 } else { 1 };
                    }
                    if depth == 0 {
                        return Some((pos, Pos::new(line, col)));
                    }
                }
                if forward {
                    line += 1;
                    if line >= self.lines.len() {
                        return None;
                    }
                } else {
                    line = line.checked_sub(1)?;
                }
            }
        }
        None
    }

    /// Scroll so the cursor is inside a view of `rows` by `cols`.
    pub fn reveal(&mut self, rows: usize, cols: usize) {
        if self.cursor.line < self.top {
            self.top = self.cursor.line;
        } else if rows > 0 && self.cursor.line >= self.top + rows {
            self.top = self.cursor.line + 1 - rows;
        }
        let margin = cols.min(8) / 2;
        if self.cursor.col < self.left + margin {
            self.left = self.cursor.col.saturating_sub(margin);
        } else if cols > 0 && self.cursor.col + margin >= self.left + cols {
            self.left = self.cursor.col + margin + 1 - cols;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn doc(text: &str) -> Buffer {
        Buffer::new(String::from("/t.may"), text)
    }

    #[test]
    fn typing_pairs_and_undo() {
        let mut b = doc("");
        for c in "print(".chars() {
            b.type_char(c);
        }
        assert_eq!(b.text(), "print()");
        b.type_char('"');
        for c in "hi".chars() {
            b.type_char(c);
        }
        b.type_char('"');
        b.type_char(')');
        assert_eq!(b.text(), "print(\"hi\")");
        assert_eq!(b.cursor, Pos::new(0, 11));
        b.undo();
        b.undo();
        assert!(b.text().len() < 11);
        while b.undo() {}
        assert_eq!(b.text(), "");
        b.redo();
        assert_ne!(b.text(), "");
    }

    #[test]
    fn enter_indents_and_splits_brackets() {
        let mut b = doc("fun f() {}");
        b.jump(Pos::new(0, 9), false);
        b.newline();
        assert_eq!(b.text(), "fun f() {\n    \n}");
        assert_eq!(b.cursor, Pos::new(1, 4));
        b.type_char('x');
        b.newline();
        assert_eq!(b.lines[2], "    ");
        b.type_char('}');
        assert_eq!(b.lines[2], "}");
    }

    #[test]
    fn selection_cut_paste_and_tabs() {
        let mut b = doc("one\ntwo\nthree");
        b.jump(Pos::new(0, 1), false);
        b.jump(Pos::new(1, 2), true);
        assert_eq!(b.selected_text().unwrap(), "ne\ntw");
        assert_eq!(b.cut().unwrap(), "ne\ntw");
        assert_eq!(b.text(), "oo\nthree");
        b.insert("A\nB");
        assert_eq!(b.text(), "oA\nBo\nthree");
        b.select_all();
        b.tab(false);
        assert_eq!(b.text(), "    oA\n    Bo\n    three");
        b.tab(true);
        assert_eq!(b.text(), "oA\nBo\nthree");
    }

    #[test]
    fn comments_backspace_and_lines() {
        let mut b = doc("    let x = 1;\n    let y = 2;");
        b.select_all();
        b.toggle_comment();
        assert_eq!(b.text(), "    // let x = 1;\n    // let y = 2;");
        b.toggle_comment();
        assert_eq!(b.text(), "    let x = 1;\n    let y = 2;");
        b.jump(Pos::new(1, 4), false);
        b.backspace();
        assert_eq!(b.lines[1], "let y = 2;");
        assert_eq!(b.cut_line(), "let y = 2;\n");
        assert_eq!(b.text(), "    let x = 1;");
        b.duplicate_line();
        assert_eq!(b.lines.len(), 2);
    }

    #[test]
    fn find_replace_brackets_and_words() {
        let mut b = doc("alpha beta\nAlpha (gamma)");
        let (start, _) = b.find("ALPHA", Pos::new(0, 1), false).unwrap();
        assert_eq!(start, Pos::new(1, 0));
        let (start, _) = b.find("alpha", Pos::new(1, 0), true).unwrap();
        assert_eq!(start, Pos::new(0, 0));
        b.jump(Pos::new(1, 6), false);
        assert_eq!(b.matching_bracket().unwrap().1, Pos::new(1, 12));
        b.jump(Pos::new(0, 0), false);
        b.right(false, true);
        assert_eq!(b.cursor, Pos::new(0, 5));
        assert_eq!(b.replace_all("alpha", "omega"), 2);
        assert_eq!(b.text(), "omega beta\nomega (gamma)");
        b.undo();
        assert_eq!(b.text(), "alpha beta\nAlpha (gamma)");
    }

    #[test]
    fn tabs_and_line_endings_load_as_spaces() {
        let b = doc("a\tb\r\n\tc");
        assert_eq!(b.lines, ["a   b", "    c"]);
    }
}
