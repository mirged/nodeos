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
                let blend = |s: u32| -> u32 {
                    (((fg >> s) & 255) * a + ((bg >> s) & 255) * (255 - a) + 127) / 255
                };
                c.put_pixel(
                    x + dx,
                    y + dy,
                    (blend(16) << 16) | (blend(8) << 8) | blend(0),
                );
            }
        }
    }
}
