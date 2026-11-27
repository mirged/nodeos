//! The full-screen surface: getting it, drawing on it, and handing it back.

use alloc::vec::Vec;

use libdraw::terminal_font::TerminalFont;
use libdraw::{Canvas, Rgb};
use libnode::abi::message::MessageFlags;
use libnode::abi::proto;
use libnode::abi::types::{MapFlags, MapWindow};
use libnode::mem::{Mapping, Vmo};
use libnode::{Channel, Message};

/// The text size used everywhere.
pub const FONT: TerminalFont = TerminalFont::Normal;
/// Width and height of one character cell in [`FONT`].
pub const CELL: (u32, u32) = FONT.metrics();

/// The colours a 1-bit picture is shown in.
#[derive(Clone, Copy)]
pub struct Theme {
    pub name: &'static str,
    /// The colour of a white pixel.
    pub light: Rgb,
    /// The colour of a black pixel, and of the letterbox.
    pub dark: Rgb,
}

pub const THEMES: &[Theme] = &[
    Theme { name: "Classic", light: 0xffffff, dark: 0x000000 },
    Theme { name: "Inverted", light: 0x000000, dark: 0xffffff },
    Theme { name: "Amber", light: 0xffb000, dark: 0x140b00 },
    Theme { name: "Phosphor", light: 0x4dff88, dark: 0x001408 },
    Theme { name: "Paper", light: 0xf4ecd8, dark: 0x2a2620 },
    Theme { name: "Blueprint", light: 0xdce8ff, dark: 0x0b2a5b },
];

/// The full-screen surface the display grants.
pub struct Screen<'w> {
    pub wm: &'w Channel,
    id: u32,
    view: View,
    base: *mut u32,
    pub width: usize,
    pub height: usize,
    /// Pixels per scanline.
    stride: usize,
}

/// One mapping of a surface's memory object, separate from [`Screen`] so a
/// `CONFIGURE` can replace it without destroying the surface.
struct View {
    // Field order is drop order: unmap first, then release the object, so the
    // pages are never freed while still mapped.
    mapping: Mapping,
    _vmo: Vmo,
}

impl View {
    fn map(vmo: Vmo, pitch: u32, height: u32) -> Option<Self> {
        let size = pitch as u64 * height as u64;
        let mapping = vmo
            .map(
                0,
                size,
                MapFlags::READ.or(MapFlags::WRITE).or(MapFlags::SHARED).or(MapFlags::POPULATE),
                MapWindow::Shared,
            )
            .ok()?;
        Some(Self { mapping, _vmo: vmo })
    }
}

impl<'w> Screen<'w> {
    pub fn create(wm: &'w Channel) -> Option<Self> {
        let request = proto::Encoded::new(proto::WmCreateSurface { width: 640, height: 480, flags: 0, _reserved: 0 });
        let mut payload = Vec::from(request.bytes());
        payload.extend_from_slice(b"video");
        let mut reply = wm.call(Message::new(proto::WM_CREATE_SURFACE).with_payload(&payload)).ok()?;
        if reply.flags.contains(MessageFlags::ERROR) || reply.handles.is_empty() {
            return None;
        }
        let info = proto::decode::<proto::WmSurfaceInfo>(&reply.payload)?;
        let vmo = Vmo::from_handle(reply.handles.remove(0), info.size);
        let view = View::map(vmo, info.pitch, info.height)?;
        Some(Self {
            wm,
            id: info.surface,
            base: view.mapping.base() as *mut u32,
            view,
            width: info.width as usize,
            height: info.height as usize,
            stride: info.pitch as usize / 4,
        })
    }

    /// Adopt the surface a `CONFIGURE` event replaced this one with.
    pub fn reconfigure(&mut self, handle: libnode::Handle, width: u32, height: u32) {
        let pitch = width * 4;
        let vmo = Vmo::from_handle(handle, pitch as u64 * height as u64);
        let Some(view) = View::map(vmo, pitch, height) else { return };
        self.base = view.mapping.base() as *mut u32;
        self.view = view;
        self.width = width as usize;
        self.height = height as usize;
        self.stride = pitch as usize / 4;
    }

    /// A libdraw view of the surface, for text and rectangles.
    pub fn canvas(&mut self) -> Canvas {
        // SAFETY: the mapping is `height` rows of `stride * 4` bytes and stays
        // mapped while `self` is borrowed.
        unsafe { Canvas::from_raw(self.base as *mut u8, self.width as u32, self.height as u32, self.stride as u32 * 4) }
    }

    fn row(&mut self, y: usize) -> &mut [u32] {
        // SAFETY: the mapping holds `height` rows of `stride` pixels, and
        // `&mut self` makes this the only live borrow.
        unsafe { core::slice::from_raw_parts_mut(self.base.add(y * self.stride), self.width) }
    }

    pub fn fill(&mut self, colour: Rgb) {
        for y in 0..self.height {
            self.row(y).fill(colour);
        }
    }

    /// Dim a rectangle to an eighth of its brightness: a translucent backdrop
    /// that keeps text readable over any picture.
    pub fn dim(&mut self, x: usize, y: usize, w: usize, h: usize) {
        let x1 = (x + w).min(self.width);
        for y in y..(y + h).min(self.height) {
            for pixel in &mut self.row(y)[x.min(x1)..x1] {
                *pixel = (*pixel >> 3) & 0x001f_1f1f;
            }
        }
    }

    /// Draw text with its top-left at a pixel position, over what is there.
    /// Returns the x just past the end.
    pub fn text(&mut self, x: usize, y: usize, text: &str, colour: Rgb) -> usize {
        let mut canvas = self.canvas();
        FONT.draw_text_at(&mut canvas, x as u32, y as u32, text, colour, None) as usize
    }

    /// Pixel width of `text` in [`FONT`].
    pub fn text_width(text: &str) -> usize {
        text.len() * CELL.0 as usize
    }

    pub fn rect(&mut self, x: usize, y: usize, w: usize, h: usize, colour: Rgb) {
        self.canvas().fill_rect(x as u32, y as u32, w as u32, h as u32, colour);
    }

    /// Hand the whole screen to the display. Blocks until it has been copied,
    /// which is what makes it safe to draw the next frame straight after.
    pub fn commit(&self) {
        let damage = proto::Encoded::new(proto::WmDamage {
            surface: self.id,
            x: 0,
            y: 0,
            w: self.width as u32,
            h: self.height as u32,
            _reserved: 0,
        });
        let _ = self.wm.call_with(Message::new(proto::WM_COMMIT).with_payload(damage.bytes()), 64, 1);
    }

    /// Scale a decoded frame onto the screen, filling the letterbox.
    pub fn picture(&mut self, layout: &Layout, pixels: &[u8], width: usize, theme: Theme) {
        for y in 0..layout.y {
            self.row(y).fill(theme.dark);
        }
        for y in layout.y + layout.h..self.height {
            self.row(y).fill(theme.dark);
        }
        let mut previous: Option<(u16, usize)> = None;
        for (out_y, &src_y) in layout.rows.iter().enumerate() {
            let y = layout.y + out_y;
            // Scaling up repeats rows. Copying the row already drawn is a
            // memcpy instead of a lookup per pixel, and most rows are repeats.
            if let Some((_, prev_y)) = previous.filter(|(prev_src, _)| *prev_src == src_y) {
                let stride = self.stride;
                // SAFETY: both rows are inside the mapping and distinct.
                unsafe {
                    core::ptr::copy_nonoverlapping(self.base.add(prev_y * stride), self.base.add(y * stride), self.width);
                }
                continue;
            }
            let src = &pixels[src_y as usize * width..][..width];
            let (x0, w) = (layout.x, layout.w);
            let row = self.row(y);
            row[..x0].fill(theme.dark);
            row[x0 + w..].fill(theme.dark);
            for (out, &src_x) in row[x0..x0 + w].iter_mut().zip(layout.columns.iter()) {
                *out = if src[src_x as usize] != 0 { theme.light } else { theme.dark };
            }
            previous = Some((src_y, y));
        }
    }
}

impl Drop for Screen<'_> {
    /// Give the screen back to the console straight away, rather than when the
    /// process exits -- so an error printed after this is actually seen.
    fn drop(&mut self) {
        let request = proto::Encoded::new(proto::WmSurfaceRef { surface: self.id, _reserved: 0 });
        let _ = self
            .wm
            .call_with(Message::new(proto::WM_DESTROY_SURFACE).with_payload(request.bytes()), 64, 0);
    }
}

/// How a picture lands on the screen: the largest rectangle with its aspect
/// ratio that fits, centred, nearest-neighbour.
pub struct Layout {
    pub x: usize,
    pub y: usize,
    pub w: usize,
    pub h: usize,
    /// Source column for each output column.
    columns: Vec<u16>,
    /// Source row for each output row.
    rows: Vec<u16>,
}

impl Layout {
    pub fn new(screen: &Screen, vw: usize, vh: usize) -> Self {
        let (sw, sh) = (screen.width, screen.height);
        let (w, h) = if sw * vh <= sh * vw { (sw, (sw * vh / vw).max(1)) } else { ((sh * vw / vh).max(1), sh) };
        let columns = (0..w).map(|x| (x * vw / w) as u16).collect();
        let rows = (0..h).map(|y| (y * vh / h) as u16).collect();
        Self { x: (sw - w) / 2, y: (sh - h) / 2, w, h, columns, rows }
    }
}
