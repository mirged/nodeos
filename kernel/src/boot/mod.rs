//! Boot-time information handling.

pub mod multiboot2;

pub use multiboot2::{BootInfo, Framebuffer, MemoryRegion, Module, RegionKind};
