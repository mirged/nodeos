//! Windows, and where they go.
//!
//! The layout is a fixed set of columns, decided once at startup and never
//! recomputed. That is a real limitation and worth stating plainly: a window's
//! content buffer is allocated when the surface is created and the client maps
//! it exactly once, so changing a window's size means replacing the memory
//! object under a client that is very likely mid-frame. The protocol has
//! `wm_event::CONFIGURE` for that day; fixed columns are what this compositor
//! can implement without it, and they are enough for the thing it was built to
//! do -- show two programs at once.

use alloc::string::String;

use libdraw::{Canvas, Rgb, GLYPH_SIZE};
use libnode::mem::{Mapping, Vmo};

/// How many windows fit on screen. Also the number of columns the screen is
/// divided into, since the two are the same decision here.
pub const MAX_WINDOWS: usize = 2;

/// Width of a window's frame border, in pixels.
pub const BORDER: u32 = 1;
/// Height of a window's title bar, in pixels.
pub const TITLE_H: u32 = GLYPH_SIZE as u32 + 4;

/// The empty screen behind every window.
pub const DESKTOP: Rgb = 0x0018_1E26;
/// Frame and title bar of the window holding the keyboard focus.
pub const FRAME_ACTIVE: Rgb = 0x0033_66A0;
/// Frame and title bar of every other window.
pub const FRAME_INACTIVE: Rgb = 0x003A_4048;
/// Title text in the focused window.
pub const TITLE_ACTIVE: Rgb = 0x00FF_FFFF;
/// Title text elsewhere.
pub const TITLE_INACTIVE: Rgb = 0x0098_A0A8;

/// A rectangle, in screen pixels.
#[derive(Clone, Copy, Debug, Default)]
pub struct Rect {
    /// Left edge.
    pub x: u32,
    /// Top edge.
    pub y: u32,
    /// Width.
    pub w: u32,
    /// Height.
    pub h: u32,
}

/// The screen geometry, split into [`MAX_WINDOWS`] columns.
#[derive(Clone, Copy, Debug)]
pub struct Layout {
    width: u32,
    height: u32,
}

impl Layout {
    /// A layout for a screen of the given size.
    pub const fn new(width: u32, height: u32) -> Self {
        Self { width, height }
    }

    /// The whole frame of column `slot`, title bar and borders included.
    pub const fn frame(&self, slot: usize) -> Rect {
        let column = self.width / MAX_WINDOWS as u32;
        Rect { x: column * slot as u32, y: 0, w: column, h: self.height }
    }

    /// The drawable area of column `slot`: the frame minus the title bar and
    /// the border. This is the size every surface in that column is allocated
    /// at, so it never changes under a client.
    pub const fn content(&self, slot: usize) -> Rect {
        let frame = self.frame(slot);
        Rect {
            x: frame.x + BORDER,
            y: frame.y + TITLE_H,
            w: frame.w - 2 * BORDER,
            h: frame.h - TITLE_H - BORDER,
        }
    }

    /// The content area when a window is maximized.
    pub const fn maximized_content(&self) -> Rect {
        Rect {
            x: BORDER,
            y: TITLE_H,
            w: self.width - 2 * BORDER,
            h: self.height - TITLE_H - BORDER,
        }
    }

    /// Select tiled or maximized geometry.
    pub const fn content_for(&self, slot: usize, maximized: bool) -> Rect {
        if maximized { self.maximized_content() } else { self.content(slot) }
    }

    /// Select tiled or maximized frame geometry.
    pub const fn frame_for(&self, slot: usize, maximized: bool) -> Rect {
        if maximized {
            Rect { x: 0, y: 0, w: self.width, h: self.height }
        } else {
            self.frame(slot)
        }
    }
}

/// A client's pixel buffer, as the compositor sees it.
///
/// The compositor allocates the memory object and keeps a mapping of it, then
/// hands the client a duplicate handle to map for itself. Allocating on this
/// side is what makes surface creation one round trip instead of two: the
/// compositor is the only party that knows how big the window is going to be,
/// so it may as well be the one that creates the buffer that size.
pub struct Surface {
    /// Kept alive for as long as the surface is mapped; read through `canvas`.
    _vmo: Vmo,
    /// As above.
    _mapping: Mapping,
    /// A view of the client's pixels. Only ever read by the compositor.
    pub canvas: Canvas,
}

/// One window: a surface, the column it occupies, and who owns it.
pub struct Window {
    /// Identifier of the client connection that created this window.
    pub client: u32,
    /// Surface identifier, unique within that connection.
    pub surface_id: u32,
    /// Which column this window occupies.
    pub slot: usize,
    /// Title bar text.
    pub title: String,
    /// The pixels.
    pub surface: Surface,
}

impl Surface {
    /// Build a surface over an already-mapped memory object.
    ///
    /// # Safety
    /// `mapping` must cover at least `pitch * height` bytes of `vmo`, and no
    /// other mapping in this process may write it.
    pub unsafe fn new(vmo: Vmo, mapping: Mapping, width: u32, height: u32, pitch: u32) -> Self {
        let base = mapping.base() as *mut u8;
        // SAFETY: the caller guarantees the mapping covers the geometry, and
        // it is kept alive by the `_mapping` field for as long as the canvas.
        let canvas = unsafe { Canvas::from_raw(base, width, height, pitch) };
        Self { _vmo: vmo, _mapping: mapping, canvas }
    }
}

/// Paint the frame and title bar of a window, without touching its content.
pub fn draw_frame(screen: &mut Canvas, layout: &Layout, window: &Window, focused: bool, maximized: bool) {
    let frame = layout.frame_for(window.slot, maximized);
    let (bar, text) = if focused {
        (FRAME_ACTIVE, TITLE_ACTIVE)
    } else {
        (FRAME_INACTIVE, TITLE_INACTIVE)
    };

    screen.fill_rect(frame.x, frame.y, frame.w, TITLE_H, bar);
    screen.stroke_rect(frame.x, frame.y, frame.w, frame.h, bar);

    // Clipped by the canvas, so a long title stops at the window edge rather
    // than running into its neighbour.
    let text_w = frame.w.saturating_sub(2 * BORDER + 4);
    let max_chars = (text_w / GLYPH_SIZE as u32) as usize;
    let title = &window.title[..window.title.len().min(max_chars)];
    screen.draw_text_at(frame.x + BORDER + 2, frame.y + 2, title, text, None);
}

/// Paint an unoccupied column.
pub fn draw_empty(screen: &mut Canvas, layout: &Layout, slot: usize) {
    let frame = layout.frame(slot);
    screen.fill_rect(frame.x, frame.y, frame.w, frame.h, DESKTOP);
}
