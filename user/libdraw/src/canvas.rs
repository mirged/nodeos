//! A linear 32-bits-per-pixel drawing surface.
//!
//! A [`Canvas`] is a borrowed view of memory somebody else owns -- the
//! compositor's mapping of the framebuffer device, or a client's mapping of
//! its surface memory object -- so it carries no allocation and no `Drop`.
//! The owner keeps the mapping alive; the canvas only says how to interpret
//! it.

use crate::font::{self, GLYPH_SIZE};

/// A packed `0x00RRGGBB` colour.
pub type Rgb = u32;

/// A linear 32bpp `0x00RRGGBB` pixel buffer.
///
/// Every operation clips to the canvas rather than reporting an error: a
/// caller drawing a window title one pixel too wide wants the title, not a
/// `Result` it has nothing useful to do with.
pub struct Canvas {
    base: *mut u8,
    width: u32,
    height: u32,
    pitch: u32,
}

// SAFETY: a `Canvas` is a plain pointer plus geometry. Sending one to another
// thread is sound exactly when sending the mapping it describes is, which is
// the owner's business to establish -- as it would be for a `*mut u8`.
unsafe impl Send for Canvas {}

impl Canvas {
    /// Wrap `width * height` pixels at `base`, `pitch` bytes per scanline.
    ///
    /// # Safety
    /// `base` must point at a writable mapping of at least
    /// `pitch * height` bytes that outlives the canvas, and nothing else may
    /// write it concurrently.
    pub const unsafe fn from_raw(base: *mut u8, width: u32, height: u32, pitch: u32) -> Self {
        Self { base, width, height, pitch }
    }

    /// Width in pixels.
    pub const fn width(&self) -> u32 {
        self.width
    }

    /// Height in pixels.
    pub const fn height(&self) -> u32 {
        self.height
    }

    /// Bytes per scanline.
    pub const fn pitch(&self) -> u32 {
        self.pitch
    }

    /// Text columns this canvas holds at the built-in glyph size.
    pub const fn cols(&self) -> u32 {
        self.width / GLYPH_SIZE as u32
    }

    /// Text rows this canvas holds at the built-in glyph size.
    pub const fn rows(&self) -> u32 {
        self.height / GLYPH_SIZE as u32
    }

    /// Byte offset of a pixel, or `None` if it lies outside the canvas.
    const fn offset(&self, x: u32, y: u32) -> Option<usize> {
        if x >= self.width || y >= self.height {
            return None;
        }
        Some(y as usize * self.pitch as usize + x as usize * 4)
    }

    /// Set one pixel. Out-of-bounds coordinates are silently ignored, the same
    /// tolerance a framebuffer device would have no way to report anyway.
    pub fn put_pixel(&mut self, x: u32, y: u32, color: Rgb) {
        let Some(offset) = self.offset(x, y) else { return };
        // SAFETY: `offset` is in bounds by the check above, and the mapping is
        // writable by the contract on `from_raw`.
        unsafe { core::ptr::write_volatile(self.base.add(offset) as *mut u32, color & 0x00FF_FFFF) };
    }

    /// Read one pixel, or `None` outside the canvas.
    ///
    /// The read is volatile for the same reason the write is: this memory is
    /// either a device's MMIO range or a mapping another process is writing,
    /// so it is not memory the compiler may assume it knows the contents of.
    /// The compositor reads it for exactly that case -- saving the pixels a
    /// client's window is about to be blitted over.
    pub fn pixel(&self, x: u32, y: u32) -> Option<Rgb> {
        let offset = self.offset(x, y)?;
        // SAFETY: `offset` is in bounds by the check above, and the mapping is
        // readable by the contract on `from_raw`.
        Some(unsafe { core::ptr::read_volatile(self.base.add(offset) as *const u32) })
    }

    /// Fill an axis-aligned rectangle, clipped to the canvas.
    ///
    /// Fills a scanline at a time rather than a pixel at a time: clearing a
    /// 1024x768 screen is the compositor's most frequent large operation and
    /// the per-pixel bounds test dominated it.
    pub fn fill_rect(&mut self, x: u32, y: u32, w: u32, h: u32, color: Rgb) {
        let x_end = x.saturating_add(w).min(self.width);
        let y_end = y.saturating_add(h).min(self.height);
        if x >= x_end || y >= y_end {
            return;
        }
        let pixels = (x_end - x) as usize;
        let color = color & 0x00FF_FFFF;
        for py in y..y_end {
            // SAFETY: `x..x_end` and `py` are within the canvas, so the whole
            // run lies inside the mapping.
            let mut row = unsafe { self.base.add(self.offset(x, py).expect("clipped above")) as *mut u32 };
            for _ in 0..pixels {
                // SAFETY: as above; `row` walks `pixels` in-bounds slots.
                unsafe {
                    core::ptr::write_volatile(row, color);
                    row = row.add(1);
                }
            }
        }
    }

    /// Fill the whole canvas.
    pub fn clear(&mut self, color: Rgb) {
        self.fill_rect(0, 0, self.width, self.height, color);
    }

    /// Draw a one-pixel-wide line with Bresenham's algorithm.
    pub fn draw_line(&mut self, x0: u32, y0: u32, x1: u32, y1: u32, color: Rgb) {
        let (mut x0, mut y0) = (x0 as i64, y0 as i64);
        let (x1, y1) = (x1 as i64, y1 as i64);
        let dx = (x1 - x0).abs();
        let sx: i64 = if x0 < x1 { 1 } else { -1 };
        let dy = -(y1 - y0).abs();
        let sy: i64 = if y0 < y1 { 1 } else { -1 };
        let mut err = dx + dy;
        loop {
            self.put_pixel(x0 as u32, y0 as u32, color);
            if x0 == x1 && y0 == y1 {
                break;
            }
            let e2 = 2 * err;
            if e2 >= dy {
                err += dy;
                x0 += sx;
            }
            if e2 <= dx {
                err += dx;
                y0 += sy;
            }
        }
    }

    /// Draw a one-pixel-wide unfilled rectangle.
    pub fn stroke_rect(&mut self, x: u32, y: u32, w: u32, h: u32, color: Rgb) {
        if w == 0 || h == 0 {
            return;
        }
        self.fill_rect(x, y, w, 1, color);
        self.fill_rect(x, y + h - 1, w, 1, color);
        self.fill_rect(x, y, 1, h, color);
        self.fill_rect(x + w - 1, y, 1, h, color);
    }

    /// Draw one glyph with its top-left corner at a pixel position.
    pub fn draw_glyph_at(&mut self, x: u32, y: u32, ch: u8, fg: Rgb, bg: Option<Rgb>) {
        let rows = font::glyph(ch);
        for (dy, bits) in rows.iter().enumerate() {
            for dx in 0..GLYPH_SIZE {
                let on = bits & (1 << (7 - dx)) != 0;
                let color = if on {
                    fg
                } else {
                    match bg {
                        Some(bg) => bg,
                        // A transparent background is what a title bar wants:
                        // the glyph sits on whatever the bar was painted with.
                        None => continue,
                    }
                };
                self.put_pixel(x + dx as u32, y + dy as u32, color);
            }
        }
    }

    /// Draw one glyph in a glyph-sized cell.
    pub fn draw_glyph(&mut self, col: u32, row: u32, ch: u8, fg: Rgb, bg: Rgb) {
        self.draw_glyph_at(col * GLYPH_SIZE as u32, row * GLYPH_SIZE as u32, ch, fg, Some(bg));
    }

    /// Draw a string at a pixel position, without wrapping. Stops at the right
    /// edge; newlines are not interpreted -- that is [`crate::TextGrid`]'s job.
    pub fn draw_text_at(&mut self, x: u32, y: u32, text: &str, fg: Rgb, bg: Option<Rgb>) {
        let mut px = x;
        for byte in text.bytes() {
            if px >= self.width {
                break;
            }
            self.draw_glyph_at(px, y, byte, fg, bg);
            px += GLYPH_SIZE as u32;
        }
    }

    /// Move the contents up by `pixels` rows, filling the exposed strip at the
    /// bottom with `background` -- what a scrolling terminal does when it runs
    /// out of screen.
    pub fn scroll_up(&mut self, pixels: u32, background: Rgb) {
        if pixels == 0 {
            return;
        }
        if pixels >= self.height {
            self.clear(background);
            return;
        }
        let shift = pixels as usize * self.pitch as usize;
        let total = self.height as usize * self.pitch as usize;
        // SAFETY: both ranges lie within the mapping (which covers
        // `pitch * height` bytes), and `copy` is used because they overlap.
        unsafe { core::ptr::copy(self.base.add(shift), self.base, total - shift) };
        self.fill_rect(0, self.height - pixels, self.width, pixels, background);
    }

    /// Copy a `w * h` rectangle from `src` at `(src_x, src_y)` to `(dst_x,
    /// dst_y)` in this canvas, clipped to both.
    ///
    /// This is the compositor's inner loop: one call per damaged region per
    /// commit. `src` must not alias this canvas -- in practice it never does,
    /// because the source is another process's memory object and the
    /// destination is the framebuffer.
    pub fn blit(&mut self, dst_x: u32, dst_y: u32, src: &Canvas, src_x: u32, src_y: u32, w: u32, h: u32) {
        // Clip against both rectangles before touching memory, so the copy
        // loop below has no per-row conditions in it.
        let w = w.min(src.width.saturating_sub(src_x)).min(self.width.saturating_sub(dst_x));
        let h = h.min(src.height.saturating_sub(src_y)).min(self.height.saturating_sub(dst_y));
        if w == 0 || h == 0 {
            return;
        }
        let row_bytes = w as usize * 4;
        for row in 0..h {
            let src_offset = (src_y + row) as usize * src.pitch as usize + src_x as usize * 4;
            let dst_offset = (dst_y + row) as usize * self.pitch as usize + dst_x as usize * 4;
            // SAFETY: the clipping above keeps both runs inside their
            // mappings, and the two canvases do not alias (see the doc
            // comment), so a non-overlapping copy is correct.
            unsafe {
                core::ptr::copy_nonoverlapping(
                    src.base.add(src_offset) as *const u8,
                    self.base.add(dst_offset),
                    row_bytes,
                );
            }
        }
    }
}
