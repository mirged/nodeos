//! Text terminal state: visible cells, a cursor and bounded scrollback.
use crate::terminal_font::{TerminalFont, MARGIN};
use crate::{Canvas, Rgb};
use alloc::{collections::VecDeque, vec, vec::Vec};
#[derive(Clone, Copy)]
struct Cell {
    byte: u8,
    fg: Rgb,
    bg: Rgb,
}
/// A terminal grid with 512 lines of scrollback and a visible underline cursor.
pub struct TextGrid {
    cols: u32,
    rows: u32,
    col: u32,
    row: u32,
    background: Rgb,
    foreground: Rgb,
    cells: Vec<Cell>,
    history: VecDeque<Vec<Cell>>,
    view: usize,
    font: TerminalFont,
    ink: Rgb,
    paper: Rgb,
    escape: u8,
    params: [u16; 8],
    param_at: usize,
}
impl TextGrid {
    /// Create a grid sized to the canvas.
    pub fn new(canvas: &Canvas, foreground: Rgb, background: Rgb) -> Self {
        let font = TerminalFont::Normal;
        let (cols, rows) = font.geometry(canvas);
        Self {
            cols,
            rows,
            col: 0,
            row: 0,
            background,
            foreground,
            cells: vec![
                Cell {
                    byte: b' ',
                    fg: foreground,
                    bg: background
                };
                (cols * rows) as usize
            ],
            history: VecDeque::new(),
            view: 0,
            font,
            ink: foreground,
            paper: background,
            escape: 0,
            params: [0; 8],
            param_at: 0,
        }
    }
    /// Resize while preserving the overlap and clamping the cursor.
    pub fn resize(&mut self, canvas: &Canvas) {
        let (cols, rows) = self.font.geometry(canvas);
        let mut cells = vec![self.blank(); (cols * rows) as usize];
        for y in 0..rows.min(self.rows) {
            for x in 0..cols.min(self.cols) {
                cells[(y * cols + x) as usize] = self.cells[(y * self.cols + x) as usize];
            }
        }
        self.cols = cols;
        self.rows = rows;
        self.cells = cells;
        self.col = self.col.min(cols.saturating_sub(1));
        self.row = self.row.min(rows.saturating_sub(1));
        self.view = 0;
    }
    /// Pixel bounds of a terminal cell, including line spacing.
    pub fn cell_rect(&self, col: u32, row: u32) -> (u32, u32, u32, u32) {
        let (w, h) = self.font.metrics();
        (MARGIN + col * w, MARGIN + row * h, w, h)
    }
    /// Change text size, retaining and rewrapping earlier output and its colours.
    pub fn set_font(&mut self, canvas: &mut Canvas, font: TerminalFont) -> bool {
        let (cols, rows) = font.geometry(canvas);
        if cols == 0 || rows == 0 {
            return false;
        }
        if font == self.font {
            return true;
        }
        let old_col = self.col;
        let mut old = core::mem::take(&mut self.history);
        for line in self
            .cells
            .chunks(self.cols.max(1) as usize)
            .take(self.row as usize + 1)
        {
            old.push_back(line.to_vec());
        }
        let mut wrapped = VecDeque::new();
        for line in old {
            let end = line
                .iter()
                .rposition(|cell| cell.byte != b' ')
                .map(|i| i + 1)
                .unwrap_or(0);
            if end == 0 {
                wrapped.push_back(vec![self.blank(); cols as usize]);
            } else {
                for part in line[..end].chunks(cols as usize) {
                    let mut row = vec![self.blank(); cols as usize];
                    row[..part.len()].copy_from_slice(part);
                    wrapped.push_back(row);
                }
            }
            while wrapped.len() > 512 + rows as usize {
                wrapped.pop_front();
            }
        }
        let visible = wrapped.len().min(rows as usize);
        let split = wrapped.len() - visible;
        self.cells = vec![self.blank(); (cols * rows) as usize];
        for y in 0..visible {
            let row = wrapped.remove(split).unwrap();
            self.cells[y * cols as usize..(y + 1) * cols as usize].copy_from_slice(&row);
        }
        self.font = font;
        self.cols = cols;
        self.rows = rows;
        self.history = wrapped;
        self.col = old_col.min(cols - 1);
        self.row = visible.saturating_sub(1) as u32;
        self.view = 0;
        canvas.clear(self.background);
        self.redraw(canvas);
        true
    }
    fn sgr(&mut self) {
        const COLORS: [Rgb; 8] = [
            0x101216, 0xFFA4A4, 0xA9D39A, 0xE9CF8D, 0x86BFFF, 0xCCA8F5, 0x8BD9DD, 0xE3E8EE,
        ];
        for index in 0..=self.param_at {
            match self.params[index] {
                0 => {
                    self.ink = self.foreground;
                    self.paper = self.background;
                }
                1 => self.ink = 0xFFFFFF,
                2 => self.ink = 0xA7B2BE,
                22 | 39 => self.ink = self.foreground,
                30..=37 => self.ink = COLORS[(self.params[index] - 30) as usize],
                90..=97 => self.ink = COLORS[(self.params[index] - 90) as usize],
                40..=47 => self.paper = COLORS[(self.params[index] - 40) as usize],
                49 => self.paper = self.background,
                _ => {}
            }
        }
    }
    /// Text columns.
    pub const fn cols(&self) -> u32 {
        self.cols
    }
    /// Text rows.
    pub const fn rows(&self) -> u32 {
        self.rows
    }
    /// Default foreground colour.
    pub const fn foreground(&self) -> Rgb {
        self.foreground
    }
    /// Default background colour.
    pub const fn background(&self) -> Rgb {
        self.background
    }
    /// Move the logical cursor, clamping to the grid.
    pub fn set_cursor(&mut self, col: u32, row: u32) {
        self.col = col.min(self.cols.saturating_sub(1));
        self.row = row.min(self.rows.saturating_sub(1));
    }
    /// Logical cursor cell.
    pub const fn cursor(&self) -> (u32, u32) {
        (self.col, self.row)
    }
    fn blank(&self) -> Cell {
        Cell {
            byte: b' ',
            fg: self.foreground,
            bg: self.background,
        }
    }
    /// Clear visible text and scrollback, resetting the insertion point.
    pub fn clear(&mut self, canvas: &mut Canvas) {
        let blank = self.blank();
        self.cells.fill(blank);
        self.history.clear();
        self.col = 0;
        self.row = 0;
        self.view = 0;
        self.ink = self.foreground;
        self.paper = self.background;
        self.escape = 0;
        canvas.clear(self.background);
        self.paint_cursor(canvas);
    }
    fn cell(&self) -> Option<Cell> {
        self.cells
            .get((self.row * self.cols + self.col) as usize)
            .copied()
    }
    /// Restore the cell beneath the visible cursor before another operation.
    pub fn erase_cursor(&self, canvas: &mut Canvas) {
        if self.view == 0 {
            if let Some(cell) = self.cell() {
                self.font
                    .draw(canvas, self.col, self.row, cell.byte, cell.fg, cell.bg);
            }
        }
    }
    /// Paint an underline at the insertion point when viewing live output.
    pub fn paint_cursor(&self, canvas: &mut Canvas) {
        if self.view == 0 {
            let (x, y, w, h) = self.cell_rect(self.col, self.row);
            canvas.fill_rect(
                x,
                y + h.saturating_sub(3),
                w.saturating_sub(2),
                2,
                self.foreground,
            );
        }
    }
    /// Scroll the viewport in lines; positive values look into history.
    pub fn scroll_view(&mut self, canvas: &mut Canvas, lines: i32) {
        self.view = if lines >= 0 {
            self.view
                .saturating_add(lines as usize)
                .min(self.history.len())
        } else {
            self.view.saturating_sub(lines.unsigned_abs() as usize)
        };
        self.redraw(canvas);
    }
    fn redraw(&self, canvas: &mut Canvas) {
        for y in 0..self.rows as usize {
            let index = self.history.len() + y - self.view;
            for x in 0..self.cols as usize {
                let cell = if index < self.history.len() {
                    self.history[index].get(x).copied().unwrap_or(self.blank())
                } else {
                    self.cells[(index - self.history.len()) * self.cols as usize + x]
                };
                self.font
                    .draw(canvas, x as u32, y as u32, cell.byte, cell.fg, cell.bg);
            }
        }
        self.paint_cursor(canvas);
    }
    fn live(&mut self, canvas: &mut Canvas) -> bool {
        if self.view != 0 {
            self.view = 0;
            self.redraw(canvas);
            true
        } else {
            false
        }
    }
    fn newline(&mut self, canvas: &mut Canvas) -> bool {
        self.col = 0;
        if self.row + 1 >= self.rows {
            if self.cols == 0 {
                return false;
            }
            if self.history.len() >= 512 {
                self.history.pop_front();
            }
            self.history
                .push_back(self.cells[..self.cols as usize].to_vec());
            self.cells.rotate_left(self.cols as usize);
            let start = self.cells.len() - self.cols as usize;
            let blank = self.blank();
            self.cells[start..].fill(blank);
            let (w, h) = self.font.metrics();
            canvas.scroll_rect_up(
                MARGIN,
                MARGIN,
                self.cols * w,
                self.rows * h,
                h,
                self.background,
            );
            true
        } else {
            self.row += 1;
            false
        }
    }
    /// Erase the previous character in the current row.
    pub fn backspace(&mut self, canvas: &mut Canvas) {
        self.live(canvas);
        self.erase_cursor(canvas);
        if self.col > 0 {
            self.col -= 1;
            let index = (self.row * self.cols + self.col) as usize;
            self.cells[index] = self.blank();
            self.font.draw(
                canvas,
                self.col,
                self.row,
                b' ',
                self.foreground,
                self.background,
            );
        }
        self.paint_cursor(canvas);
    }
    /// Write with default terminal colours. Returns whether the whole view changed.
    pub fn write(&mut self, canvas: &mut Canvas, text: &str) -> bool {
        self.write_inner(canvas, text)
    }
    /// Write coloured cells, keeping scrollback and cursor state consistent.
    pub fn write_colored(&mut self, canvas: &mut Canvas, text: &str, fg: Rgb, bg: Rgb) -> bool {
        self.ink = fg;
        self.paper = bg;
        self.write_inner(canvas, text)
    }
    fn write_inner(&mut self, canvas: &mut Canvas, text: &str) -> bool {
        let mut scrolled = self.live(canvas);
        self.erase_cursor(canvas);
        if self.cols == 0 || self.rows == 0 {
            return scrolled;
        }
        for byte in text.bytes() {
            if self.escape != 0 {
                if self.escape == 1 {
                    self.escape = if byte == b'[' { 2 } else { 0 };
                    self.params = [0; 8];
                    self.param_at = 0;
                } else if byte.is_ascii_digit() {
                    self.params[self.param_at] = self.params[self.param_at]
                        .saturating_mul(10)
                        .saturating_add((byte - b'0') as u16);
                } else if byte == b';' {
                    if self.param_at < 7 {
                        self.param_at += 1;
                    }
                } else {
                    if byte == b'm' {
                        self.sgr();
                    }
                    self.escape = 0;
                }
                continue;
            }
            if byte == 27 {
                self.escape = 1;
                continue;
            }
            if byte == b'\t' {
                let spaces = 8 - self.col % 8;
                for _ in 0..spaces {
                    self.cells[(self.row * self.cols + self.col) as usize] = Cell {
                        byte: b' ',
                        fg: self.ink,
                        bg: self.paper,
                    };
                    self.font
                        .draw(canvas, self.col, self.row, b' ', self.ink, self.paper);
                    self.col += 1;
                    if self.col >= self.cols {
                        scrolled |= self.newline(canvas);
                    }
                }
                continue;
            }
            if byte == b'\n' {
                scrolled |= self.newline(canvas);
                continue;
            }
            if byte == b'\r' {
                self.col = 0;
                continue;
            }
            self.cells[(self.row * self.cols + self.col) as usize] = Cell {
                byte,
                fg: self.ink,
                bg: self.paper,
            };
            self.font
                .draw(canvas, self.col, self.row, byte, self.ink, self.paper);
            self.col += 1;
            if self.col >= self.cols {
                scrolled |= self.newline(canvas);
            }
        }
        self.paint_cursor(canvas);
        scrolled
    }
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn history_and_live_cells_are_independent() {
        let mut pixels = vec![0u32; 72 * 98];
        let mut c = unsafe { Canvas::from_raw(pixels.as_mut_ptr() as *mut u8, 72, 98, 288) };
        let mut grid = TextGrid::new(&c, 0xffffff, 0);
        grid.write(&mut c, "one\ntwo\ntri\nfour\n");
        assert!(!grid.history.is_empty());
        let cursor = grid.cursor();
        let live = grid.cells.iter().map(|c| c.byte).collect::<Vec<_>>();
        grid.scroll_view(&mut c, 20);
        assert_eq!(grid.cursor(), cursor);
        assert_eq!(grid.cells.iter().map(|c| c.byte).collect::<Vec<_>>(), live);
        grid.write(&mut c, "x");
        assert_eq!(grid.view, 0);
        assert!(grid.history.len() <= 512);
    }
    #[test]
    fn history_is_bounded_and_clear_resets_state() {
        let mut pixels = vec![0u32; 72 * 98];
        let mut c = unsafe { Canvas::from_raw(pixels.as_mut_ptr() as *mut u8, 72, 98, 288) };
        let mut grid = TextGrid::new(&c, 0xffffff, 0);
        for _ in 0..600 {
            grid.write(&mut c, "x\n");
        }
        assert_eq!(grid.history.len(), 512);
        grid.clear(&mut c);
        assert!(grid.history.is_empty());
        assert_eq!(grid.cursor(), (0, 0));
    }
    #[test]
    fn font_changes_retain_output_and_cursor_is_in_bounds() {
        let mut pixels = vec![0u32; 640 * 480];
        let mut canvas =
            unsafe { Canvas::from_raw(pixels.as_mut_ptr() as *mut u8, 640, 480, 2560) };
        let mut grid = TextGrid::new(&canvas, 0xE3E8EE, 0x101216);
        grid.clear(&mut canvas);
        grid.write(&mut canvas, "first line\nsecond line\n");
        for font in [
            TerminalFont::Large,
            TerminalFont::Small,
            TerminalFont::Normal,
        ] {
            assert!(grid.set_font(&mut canvas, font));
            let text = grid
                .history
                .iter()
                .flatten()
                .chain(grid.cells.iter())
                .filter(|cell| cell.byte != b' ')
                .map(|cell| cell.byte)
                .collect::<Vec<_>>();
            assert!(text.windows(9).any(|s| s == b"firstline"));
            assert!(text.windows(10).any(|s| s == b"secondline"));
            assert!(grid.col < grid.cols && grid.row < grid.rows);
            let (x, y, w, h) = grid.cell_rect(grid.col, grid.row);
            assert!(x + w <= canvas.width() && y + h <= canvas.height());
        }
    }
    #[test]
    fn split_style_sequences_do_not_occupy_cells_or_damage_margins() {
        let mut pixels = vec![0u32; 72 * 98];
        let mut canvas = unsafe { Canvas::from_raw(pixels.as_mut_ptr() as *mut u8, 72, 98, 288) };
        let mut grid = TextGrid::new(&canvas, 0xE3E8EE, 0x101216);
        grid.clear(&mut canvas);
        grid.write(&mut canvas, "\x1b[3");
        grid.write(&mut canvas, "1mRed\x1b[0m\n");
        assert_eq!(grid.cells[0].byte, b'R');
        assert_eq!(grid.cells[0].fg, 0xFFA4A4);
        for _ in 0..10 {
            grid.write(&mut canvas, "ok\n");
        }
        assert_eq!(canvas.pixel(0, 0), Some(0x101216));
        assert_eq!(canvas.pixel(15, 40), Some(0x101216));
        assert_eq!(canvas.pixel(71, 97), Some(0x101216));
    }
    #[test]
    fn tabs_align_columns_without_rendering_placeholder_glyphs() {
        let mut pixels = vec![0u32; 640 * 480];
        let mut canvas =
            unsafe { Canvas::from_raw(pixels.as_mut_ptr() as *mut u8, 640, 480, 2560) };
        let mut grid = TextGrid::new(&canvas, 0xFFFFFF, 0);
        grid.write(&mut canvas, "a\tb");
        assert_eq!(grid.cursor(), (9, 0));
        assert_eq!(grid.cells[8].byte, b'b');
        assert!(grid.cells[1..8].iter().all(|cell| cell.byte == b' '));
    }
}
