//! The console's full-screen surface: its pixels, its text cursor, and what changed.
//!
//! The console no longer owns a framebuffer. It owns a memory object the
//! display broker allocated for it, which it draws into at its own pace and then
//! *commits* -- tells the display broker which rectangle is worth copying to the
//! screen. Everything in this module exists to make that rectangle small: a
//! console that committed its whole surface on every keystroke would copy a
//! megabyte and a half to echo one character.

use libdraw::terminal_font::TerminalFont;
use libdraw::{Canvas, Rgb, TextGrid};
use libnode::abi::proto;
use libnode::mem::{Mapping, Vmo};
use libnode::{Channel, Message};

/// Reply buffer for a commit acknowledgement, which carries no payload.
pub const COMMIT_REPLY_CAPACITY: usize = 64;

/// White text on a black background: the console's defaults.
pub const FOREGROUND: Rgb = 0x00E3_E8EE;
/// See [`FOREGROUND`].
pub const BACKGROUND: Rgb = 0x0010_1216;

/// The union of every rectangle drawn since the last commit.
///
/// A union rather than a list: the console's drawing is either one glyph cell
/// or the whole window, and the cases in between are rare enough that the
/// bounding box costs less than tracking them separately would.
#[derive(Clone, Copy, Default)]
struct Damage {
    bounds: Option<(u32, u32, u32, u32)>,
}

impl Damage {
    fn add(&mut self, x: u32, y: u32, w: u32, h: u32) {
        if w == 0 || h == 0 {
            return;
        }
        // Saturating: the coordinates come from client messages, and a
        // `CONSOLE_FILL_RECT` with `x = u32::MAX, w = 1` would otherwise panic
        // this service on the addition alone. The display broker clips the
        // (possibly too-large) rectangle to the surface.
        let (x1, y1) = (x.saturating_add(w), y.saturating_add(h));
        self.bounds = Some(match self.bounds {
            None => (x, y, x1, y1),
            Some((ax, ay, ax1, ay1)) => (ax.min(x), ay.min(y), ax1.max(x1), ay1.max(y1)),
        });
    }

    fn take(&mut self) -> Option<(u32, u32, u32, u32)> {
        self.bounds
            .take()
            .map(|(x, y, x1, y1)| (x, y, x1 - x, y1 - y))
    }
}

/// The console's surface: pixels, a text cursor over them, and pending damage.
pub struct Surface {
    /// Kept alive for as long as the canvas; drawn through `canvas`.
    _vmo: Vmo,
    /// As above.
    _mapping: Mapping,
    canvas: Canvas,
    grid: TextGrid,
    id: u32,
    damage: Damage,
}

impl Surface {
    /// Wrap a mapped surface memory object.
    ///
    /// # Safety
    /// `mapping` must cover at least `info.pitch * info.height` bytes of
    /// `vmo`, and nothing else in this process may write it.
    pub unsafe fn new(vmo: Vmo, mapping: Mapping, info: &proto::WmSurfaceInfo) -> Self {
        // SAFETY: the caller guarantees the mapping covers the geometry, and
        // `_mapping` keeps it alive for as long as the canvas.
        let canvas = unsafe {
            Canvas::from_raw(
                mapping.base() as *mut u8,
                info.width,
                info.height,
                info.pitch,
            )
        };
        let grid = TextGrid::new(&canvas, FOREGROUND, BACKGROUND);
        Self {
            _vmo: vmo,
            _mapping: mapping,
            canvas,
            grid,
            id: info.surface,
            damage: Damage::default(),
        }
    }

    /// Adopt a new surface memory object, as a `CONFIGURE` event asks.
    ///
    /// The window changed size, so the canvas and the text grid are rebuilt
    /// around the new geometry. The old mapping is unmapped before its object
    /// is released, so the frames are never freed while still translated.
    ///
    /// # Safety
    /// `mapping` must cover at least `pitch * height` bytes of `vmo`, and no
    /// other mapping in this process may write it.
    pub unsafe fn replace(
        &mut self,
        vmo: Vmo,
        mapping: Mapping,
        width: u32,
        height: u32,
        pitch: u32,
    ) {
        // SAFETY: the caller guarantees the mapping covers the geometry, and
        // `_mapping` below keeps it alive for as long as the canvas.
        let canvas = unsafe { Canvas::from_raw(mapping.base() as *mut u8, width, height, pitch) };
        let grid = TextGrid::new(&canvas, FOREGROUND, BACKGROUND);
        self._mapping = mapping;
        self._vmo = vmo;
        self.canvas = canvas;
        self.grid = grid;
        self.damage = Damage::default();
    }

    /// Width in pixels.
    pub fn width(&self) -> u32 {
        self.canvas.width()
    }

    /// Height in pixels.
    pub fn height(&self) -> u32 {
        self.canvas.height()
    }

    /// Text columns.
    pub fn cols(&self) -> u32 {
        self.grid.cols()
    }

    /// Text rows.
    pub fn rows(&self) -> u32 {
        self.grid.rows()
    }

    /// Where the cursor is, in glyph cells.
    pub fn cursor(&self) -> (u32, u32) {
        self.grid.cursor()
    }

    /// Clear the terminal and reset the cursor.
    pub fn clear(&mut self) {
        self.grid.clear(&mut self.canvas);
        self.damage_all();
    }

    /// Move the text cursor. Draws nothing, so damages nothing.
    pub fn set_cursor(&mut self, col: u32, row: u32) {
        self.grid.erase_cursor(&mut self.canvas);
        let (old_col, old_row) = self.grid.cursor();
        self.grid.set_cursor(col, row);
        self.grid.paint_cursor(&mut self.canvas);
        let (x, y, w, h) = self.grid.cell_rect(old_col, old_row);
        self.damage.add(x, y, w, h);
        let (col, row) = self.grid.cursor();
        let (x, y, w, h) = self.grid.cell_rect(col, row);
        self.damage.add(x, y, w, h);
    }

    /// Write text at the cursor, wrapping and scrolling like a TTY.
    pub fn write_text(&mut self, text: &str) {
        let (_, start_row) = self.grid.cursor();
        let scrolled = self.grid.write(&mut self.canvas, text);
        let (_, end_row) = self.grid.cursor();

        if scrolled || end_row < start_row {
            // A scroll moves every row, so every row is damaged.
            self.damage_all();
        } else {
            let (_, y, _, cell_h) = self.grid.cell_rect(0, start_row);
            let h = (end_row - start_row + 1) * cell_h;
            self.damage.add(0, y, self.canvas.width(), h);
        }
    }

    /// Erase the character before the cursor.
    pub fn backspace(&mut self) {
        let (col, row) = self.grid.cursor();
        self.grid.backspace(&mut self.canvas);
        if col > 0 {
            let (x, y, w, h) = self.grid.cell_rect(col - 1, row);
            self.damage.add(x, y, w * 2, h);
        }
    }

    /// Set text size while retaining terminal output and scrollback.
    pub fn set_font(&mut self, id: u32) -> bool {
        let Some(font) = TerminalFont::from_id(id) else {
            return false;
        };
        let changed = self.grid.set_font(&mut self.canvas, font);
        if changed {
            self.damage_all();
        }
        changed
    }
    /// Page through terminal history without changing the live cursor.
    pub fn scroll_view(&mut self, lines: i32) {
        self.grid.scroll_view(&mut self.canvas, lines);
        self.damage_all();
    }

    fn damage_all(&mut self) {
        self.damage
            .add(0, 0, self.canvas.width(), self.canvas.height());
    }

    /// Publish everything drawn since the last call, and wait for the
    /// display broker to acknowledge that it has copied those pixels.
    ///
    /// The acknowledgement is what keeps the console from redrawing a region
    /// the display broker is still reading. It costs one round trip per batch of
    /// drawing, not per operation, which is why damage is accumulated rather
    /// than committed as it is produced.
    pub fn commit(&mut self, wm: &Channel) {
        let Some((x, y, w, h)) = self.damage.take() else {
            return;
        };
        let damage = proto::Encoded::new(proto::WmDamage {
            surface: self.id,
            x,
            y,
            w,
            h,
            _reserved: 0,
        });
        // Sized for the empty acknowledgement it gets back, not the 4 KiB
        // default: this runs on every keystroke, and the runtime allocator
        // does not reclaim blocks that large.
        let sent = wm.call_with(
            Message::new(proto::WM_COMMIT).with_payload(damage.bytes()),
            COMMIT_REPLY_CAPACITY,
            1,
        );
        if sent.is_err() {
            // Put the record back. A commit that never reached the display broker
            // leaves those pixels unpublished, and forgetting the rectangle
            // would leave that part of the window stale until something else
            // happened to redraw it.
            self.damage.add(x, y, w, h);
        }
    }
}
