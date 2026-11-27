//! Legible terminal fonts baked from DejaVu Sans Mono. See terminal-font/LICENSE.txt.
use crate::{Canvas, Rgb};
/// Inset between the terminal's cells and the edge of the screen.
pub const MARGIN: u32 = 16;
/// Terminal text sizes. The compact bitmap font remains available to other apps.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TerminalFont {
    /// Compact 13-pixel glyphs in 8 by 18 cells.
    Small,
    /// Default 16-pixel glyphs in 10 by 22 cells.
    Normal,
    /// Large 20-pixel glyphs in 13 by 26 cells.
    Large,
}
impl TerminalFont {
    /// Decode the console's font-size setting.
    pub fn from_id(id: u32) -> Option<Self> {
        match id {
            0 => Some(Self::Small),
            1 => Some(Self::Normal),
            2 => Some(Self::Large),
            _ => None,
        }
    }
    /// Cell width and line height, including breathing room between lines.
    pub const fn metrics(self) -> (u32, u32) {
        match self {
            Self::Small => (8, 18),
            Self::Normal => (10, 22),
            Self::Large => (13, 26),
        }
    }
    /// Number of cells fitting inside a surface's margins.
    pub fn geometry(self, c: &Canvas) -> (u32, u32) {
        let (w, h) = self.metrics();
        (
            c.width().saturating_sub(MARGIN * 2) / w,
            c.height().saturating_sub(MARGIN * 2) / h,
        )
    }
    /// Paint a complete opaque terminal cell with antialiased glyph coverage.
    pub fn draw(self, c: &mut Canvas, col: u32, row: u32, byte: u8, fg: Rgb, bg: Rgb) {
        let (w, h) = self.metrics();
        let (x, y) = (MARGIN + col * w, MARGIN + row * h);
        c.fill_rect(x, y, w, h, bg);
        self.draw_at(c, x, y, byte, fg, Some(bg));
    }
    /// Draw one glyph with its cell's top-left corner at a pixel position.
    /// With `bg` the coverage blends against that colour, as a terminal cell
    /// does; with `None` it blends against whatever is already on the canvas,
    /// which is what text over a picture wants. Nothing outside the glyph's
    /// own coverage is touched.
    pub fn draw_at(self, c: &mut Canvas, x: u32, y: u32, byte: u8, fg: Rgb, bg: Option<Rgb>) {
        let (w, h) = self.metrics();
        let atlas: &[u8] = match self {
            Self::Small => include_bytes!("terminal-font/small.alpha"),
            Self::Normal => include_bytes!("terminal-font/normal.alpha"),
            Self::Large => include_bytes!("terminal-font/large.alpha"),
        };
        let byte = if (32..=126).contains(&byte) {
            byte
        } else {
            b'?'
        };
        let start = (byte - 32) as usize * (w * h) as usize;
        for dy in 0..h {
            for dx in 0..w {
                let a = atlas[start + (dy * w + dx) as usize] as u32;
                if a == 0 {
                    continue;
                }
                let under = match bg {
                    Some(bg) => bg,
                    None => match c.pixel(x + dx, y + dy) {
                        Some(pixel) => pixel,
                        None => continue,
                    },
                };
                let blend = |s: u32| -> u32 {
                    (((fg >> s) & 255) * a + ((under >> s) & 255) * (255 - a) + 127) / 255
                };
                c.put_pixel(
                    x + dx,
                    y + dy,
                    (blend(16) << 16) | (blend(8) << 8) | blend(0),
                );
            }
        }
    }
    /// Draw a string from a pixel position, without wrapping; see
    /// [`TerminalFont::draw_at`]. Returns the x just past the last glyph.
    pub fn draw_text_at(self, c: &mut Canvas, x: u32, y: u32, text: &str, fg: Rgb, bg: Option<Rgb>) -> u32 {
        let (w, _) = self.metrics();
        let mut px = x;
        for byte in text.bytes() {
            if px + w > c.width() {
                break;
            }
            self.draw_at(c, px, y, byte, fg, bg);
            px += w;
        }
        px
    }
}
