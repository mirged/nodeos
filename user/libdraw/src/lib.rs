//! # libdraw -- pixels, glyphs and a scrolling text grid
//!
//! The full-screen display broker owns the physical framebuffer; console
//! and explicitly launched applications draw shared-memory surfaces. The
//! console's text grid keeps visible cells, a cursor and bounded scrollback.
//!
//! There is exactly one pixel format handled here: 32 bits per pixel,
//! `0x00RRGGBB` little-endian in memory (byte order B, G, R, X) -- what every
//! VBE/GOP linear-framebuffer mode NodeOS's boot path asks for actually hands
//! back, and what a surface memory object is defined to be. A different depth
//! is not translated; the caller refuses it, the same way the kernel refuses
//! a malformed NXF image rather than guessing.

#![no_std]
#![deny(missing_docs)]
#![deny(unsafe_op_in_unsafe_fn)]

extern crate alloc;

pub mod canvas;
pub mod font;
pub mod text;
pub mod terminal_font;

pub use canvas::{Canvas, Rgb};
pub use font::GLYPH_SIZE;
pub use text::TextGrid;
