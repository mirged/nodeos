//! A scrolling text cursor over a [`Canvas`].
//!
//! Deliberately separate from the canvas: the compositor draws glyphs (window
//! titles) without wanting a cursor, and the console wants a cursor over
//! whatever buffer it happens to have been given. Keeping the state here means
//! the grid can be re-attached to a new canvas when a window is resized
//! without losing where the cursor was.

use crate::canvas::{Canvas, Rgb};
use crate::font::GLYPH_SIZE;

/// A TTY-style cursor: writes at the cursor, wraps at the right edge, scrolls
/// at the bottom.
pub struct TextGrid {
    cols: u32,
    rows: u32,
    col: u32,
    row: u32,
    /// Colour the canvas is cleared and scrolled with.
    background: Rgb,
    foreground: Rgb,
}

impl TextGrid {
    /// A grid sized for `canvas`, with the given colours.
    pub fn new(canvas: &Canvas, foreground: Rgb, background: Rgb) -> Self {
        Self { cols: canvas.cols(), rows: canvas.rows(), col: 0, row: 0, background, foreground }
    }

    /// Re-size the grid to a new canvas, clamping the cursor into it.
    pub fn resize(&mut self, canvas: &Canvas) {
        self.cols = canvas.cols();
        self.rows = canvas.rows();
        self.col = self.col.min(self.cols.saturating_sub(1));
        self.row = self.row.min(self.rows.saturating_sub(1));
    }

    /// Columns in the grid.
    pub const fn cols(&self) -> u32 {
        self.cols
    }

    /// Rows in the grid.
    pub const fn rows(&self) -> u32 {
        self.rows
    }

    /// The foreground colour.
    pub const fn foreground(&self) -> Rgb {
        self.foreground
    }

    /// The background colour.
    pub const fn background(&self) -> Rgb {
        self.background
    }

    /// Move the cursor. Out-of-range coordinates are clamped.
    pub fn set_cursor(&mut self, col: u32, row: u32) {
        self.col = col.min(self.cols.saturating_sub(1));
        self.row = row.min(self.rows.saturating_sub(1));
    }

    /// The cursor's cell.
    pub const fn cursor(&self) -> (u32, u32) {
        (self.col, self.row)
    }

    /// Clear the canvas and reset the cursor to the origin.
    pub fn clear(&mut self, canvas: &mut Canvas) {
        canvas.clear(self.background);
        self.col = 0;
        self.row = 0;
    }

    /// Advance to the start of the next row, scrolling if there is none.
    /// Returns whether the canvas scrolled, because a scroll moves every
    /// pixel and a caller tracking damage has to know that.
    fn newline(&mut self, canvas: &mut Canvas) -> bool {
        self.col = 0;
        if self.row + 1 >= self.rows {
            canvas.scroll_up(GLYPH_SIZE as u32, self.background);
            true
        } else {
            self.row += 1;
            false
        }
    }

    /// Erase the character before the cursor and move back onto it, for line
    /// editing. Does nothing at the start of a row -- backspacing across a
    /// wrapped or scrolled line is not this grid's job to reconstruct.
    pub fn backspace(&mut self, canvas: &mut Canvas) {
        if self.col == 0 {
            return;
        }
        self.col -= 1;
        canvas.draw_glyph(self.col, self.row, b' ', self.foreground, self.background);
    }

    /// Write text at the cursor, wrapping and scrolling like a TTY. Returns
    /// whether the canvas scrolled while doing so -- a scroll moves every
    /// pixel in the canvas, and a caller tracking damaged regions has no way
    /// to work that out from the cursor position alone.
    pub fn write(&mut self, canvas: &mut Canvas, text: &str) -> bool {
        self.write_colored(canvas, text, self.foreground, self.background)
    }

    /// As [`TextGrid::write`], with explicit colours.
    pub fn write_colored(&mut self, canvas: &mut Canvas, text: &str, fg: Rgb, bg: Rgb) -> bool {
        let mut scrolled = false;
        for byte in text.bytes() {
            if byte == b'\n' {
                scrolled |= self.newline(canvas);
                continue;
            }
            canvas.draw_glyph(self.col, self.row, byte, fg, bg);
            self.col += 1;
            if self.col >= self.cols {
                scrolled |= self.newline(canvas);
            }
        }
        scrolled
    }
}
