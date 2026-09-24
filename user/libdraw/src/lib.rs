//! # libdraw -- pixels, glyphs and a scrolling text grid
//!
//! Two processes now draw in NodeOS: `compositor`, which owns the physical
//! framebuffer, and every client, which owns a shared memory object the
//! compositor blits from. Both are 32-bit `0x00RRGGBB` linear buffers that
//! differ only in where the bytes live, so the drawing code belongs to
//! neither of them.
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

pub mod canvas;
pub mod font;
pub mod text;

pub use canvas::{Canvas, Rgb};
pub use font::GLYPH_SIZE;
pub use text::TextGrid;
