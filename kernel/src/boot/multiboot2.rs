//! Multiboot2 information structure parser.
//!
//! The boot loader leaves a tag list in physical memory and hands us a pointer
//! to it. This module turns that into typed, bounds-checked iterators. It runs
//! before the frame allocator exists, so it allocates nothing and borrows
//! directly from the loader's buffer -- which is why every accessor is tied to
//! the lifetime of the [`BootInfo`] handle.

use core::ffi::CStr;
use core::mem;

/// Multiboot2 information tag types we understand.
mod tag_type {
    pub const END: u32 = 0;
    pub const CMDLINE: u32 = 1;
    pub const BOOTLOADER_NAME: u32 = 2;
    pub const MODULE: u32 = 3;
    #[allow(dead_code)]
    pub const BASIC_MEMINFO: u32 = 4;
    pub const MEMORY_MAP: u32 = 6;
    pub const FRAMEBUFFER: u32 = 8;
    pub const ELF_SECTIONS: u32 = 9;
    pub const ACPI_RSDP_V1: u32 = 14;
    pub const ACPI_RSDP_V2: u32 = 15;
}

#[repr(C)]
#[derive(Clone, Copy)]
struct TagHeader {
    kind: u32,
    size: u32,
}

/// A borrowed view over the loader's information structure.
#[derive(Clone, Copy)]
pub struct BootInfo {
    base: *const u8,
    phys_base: u64,
    total_size: usize,
}

// SAFETY: the structure is immutable for the lifetime of the kernel; the boot
// path reserves its physical range before the frame allocator can hand it out.
unsafe impl Send for BootInfo {}
// SAFETY: as above -- read-only shared data.
unsafe impl Sync for BootInfo {}

impl BootInfo {
    /// Wrap a Multiboot2 information pointer.
    ///
    /// # Safety
    /// `ptr` must be the value the Multiboot2 loader left in `EBX`, and the
    /// memory it points at must still be mapped and untouched.
    pub unsafe fn from_ptr(ptr: *const u8) -> Option<Self> {
        if ptr.is_null() || (ptr as usize) & 7 != 0 {
            return None;
        }
        // SAFETY: caller guarantees the loader wrote a valid header here.
        let total_size = unsafe { (ptr as *const u32).read() } as usize;
        if total_size < 8 {
            return None;
        }
        Some(Self { base: ptr, phys_base: ptr as u64, total_size })
    }

    /// Physical address range occupied by the information structure, so the
    /// frame allocator can reserve it.
    pub fn phys_range(&self) -> (u64, u64) {
        (self.phys_base, self.phys_base + self.total_size as u64)
    }

    /// Re-point this view at `phys_base + offset`.
    ///
    /// The loader hands us a *physical* address, which is directly usable only
    /// while the bootstrap identity map is live. Once the kernel installs its
    /// own tables and drops that map, the same structure has to be reached
    /// through the direct physical map instead.
    ///
    /// # Safety
    /// `offset` must be the base of a mapping that covers the structure's
    /// physical range.
    pub unsafe fn relocated(&self, offset: u64) -> Self {
        Self {
            base: (self.phys_base + offset) as *const u8,
            phys_base: self.phys_base,
            total_size: self.total_size,
        }
    }

    /// Physical address of a boot module's contents, translated through
    /// whatever mapping this view is using.
    pub fn module_ptr(&self, module: &Module<'_>) -> *const u8 {
        let offset = self.base as u64 - self.phys_base;
        (module.start + offset) as *const u8
    }

    fn tags(&self) -> TagIter<'_> {
        TagIter {
            // SAFETY: the first tag starts 8 bytes in, past `total_size` and
            // `reserved`; the iterator bounds-checks everything after that.
            cursor: unsafe { self.base.add(8) },
            end: unsafe { self.base.add(self.total_size) },
            _marker: core::marker::PhantomData,
        }
    }

    fn find(&self, kind: u32) -> Option<(*const u8, usize)> {
        self.tags().find(|(k, _, _)| *k == kind).map(|(_, ptr, len)| (ptr, len))
    }

    /// The kernel command line, if the loader supplied one.
    pub fn cmdline(&self) -> Option<&str> {
        let (ptr, len) = self.find(tag_type::CMDLINE)?;
        // SAFETY: the tag body is a NUL-terminated string within the tag.
        unsafe { cstr_in(ptr, len) }
    }

    /// The boot loader's self-reported name.
    pub fn bootloader_name(&self) -> Option<&str> {
        let (ptr, len) = self.find(tag_type::BOOTLOADER_NAME)?;
        // SAFETY: as above.
        unsafe { cstr_in(ptr, len) }
    }

    /// Iterator over boot modules (userspace images loaded by the loader).
    pub fn modules(&self) -> impl Iterator<Item = Module<'_>> + '_ {
        self.tags().filter(|(k, _, _)| *k == tag_type::MODULE).filter_map(|(_, ptr, len)| {
            if len < 8 {
                return None;
            }
            // SAFETY: the tag body is at least 8 bytes, checked above.
            let (start, end) = unsafe { ((ptr as *const u32).read(), (ptr as *const u32).add(1).read()) };
            // SAFETY: the remainder of the tag is a NUL-terminated name.
            let name = unsafe { cstr_in(ptr.add(8), len - 8) }.unwrap_or("");
            Some(Module { start: start as u64, end: end as u64, name })
        })
    }

    /// Iterator over the firmware memory map.
    pub fn memory_map(&self) -> impl Iterator<Item = MemoryRegion> + '_ {
        let tag = self.find(tag_type::MEMORY_MAP);
        MemoryMapIter::new(tag)
    }

    /// Physical address of the ACPI RSDP, if the loader found one.
    pub fn rsdp(&self) -> Option<*const u8> {
        self.find(tag_type::ACPI_RSDP_V2)
            .or_else(|| self.find(tag_type::ACPI_RSDP_V1))
            .map(|(ptr, _)| ptr)
    }

    /// Framebuffer description, if the loader set a graphics mode.
    pub fn framebuffer(&self) -> Option<Framebuffer> {
        let (ptr, len) = self.find(tag_type::FRAMEBUFFER)?;
        if len < 22 {
            return None;
        }
        // SAFETY: length checked above; fields are at fixed offsets defined by
        // the Multiboot2 specification and are naturally aligned within the tag.
        unsafe {
            Some(Framebuffer {
                addr: (ptr as *const u64).read_unaligned(),
                pitch: (ptr.add(8) as *const u32).read_unaligned(),
                width: (ptr.add(12) as *const u32).read_unaligned(),
                height: (ptr.add(16) as *const u32).read_unaligned(),
                bpp: ptr.add(20).read(),
                kind: ptr.add(21).read(),
            })
        }
    }

    /// Whether the loader reported any ELF section headers (used to keep the
    /// kernel image's own frames reserved).
    pub fn has_elf_sections(&self) -> bool {
        self.find(tag_type::ELF_SECTIONS).is_some()
    }
}

/// A boot module: a blob the loader placed in memory for us, used by NodeOS to
/// deliver the initial userspace NXF images.
#[derive(Clone, Copy, Debug)]
pub struct Module<'a> {
    /// Physical start address.
    pub start: u64,
    /// Physical end address (exclusive).
    pub end: u64,
    /// Module name from the loader configuration.
    pub name: &'a str,
}

impl Module<'_> {
    /// Length of the module in bytes.
    pub fn len(&self) -> usize {
        (self.end - self.start) as usize
    }

    /// Whether the module is empty.
    pub fn is_empty(&self) -> bool {
        self.end <= self.start
    }
}

/// Firmware memory map entry types.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum RegionKind {
    /// Free for the kernel to use.
    Usable,
    /// Firmware-reserved.
    Reserved,
    /// ACPI tables; reclaimable once parsed.
    AcpiReclaimable,
    /// ACPI NVS; must be preserved across sleep.
    AcpiNvs,
    /// Known-bad memory.
    Defective,
    /// Anything the firmware invented that we do not recognise.
    Unknown(u32),
}

/// One entry of the firmware memory map.
#[derive(Clone, Copy, Debug)]
pub struct MemoryRegion {
    /// Physical base address.
    pub base: u64,
    /// Length in bytes.
    pub length: u64,
    /// What the firmware says this range is for.
    pub kind: RegionKind,
}

impl MemoryRegion {
    /// Physical end address (exclusive).
    pub fn end(&self) -> u64 {
        self.base + self.length
    }
}

/// Framebuffer description from the loader.
#[derive(Clone, Copy, Debug)]
pub struct Framebuffer {
    /// Physical address of the first pixel.
    pub addr: u64,
    /// Bytes per scanline.
    pub pitch: u32,
    /// Width in pixels.
    pub width: u32,
    /// Height in pixels.
    pub height: u32,
    /// Bits per pixel.
    pub bpp: u8,
    /// 0 = indexed, 1 = RGB, 2 = EGA text.
    pub kind: u8,
}

struct TagIter<'a> {
    cursor: *const u8,
    end: *const u8,
    _marker: core::marker::PhantomData<&'a [u8]>,
}

impl<'a> Iterator for TagIter<'a> {
    /// `(tag type, pointer to tag body, body length)`
    type Item = (u32, *const u8, usize);

    fn next(&mut self) -> Option<Self::Item> {
        loop {
            // SAFETY: `cursor` stays within `[base, end)` by construction, and
            // we check that a whole header fits before reading one.
            if (self.end as usize).checked_sub(self.cursor as usize)? < mem::size_of::<TagHeader>() {
                return None;
            }
            // SAFETY: bounds checked immediately above; the loader guarantees
            // 8-byte alignment of every tag.
            let header = unsafe { (self.cursor as *const TagHeader).read() };
            if header.kind == tag_type::END {
                return None;
            }
            let size = header.size as usize;
            if size < mem::size_of::<TagHeader>() {
                return None; // malformed: would not make progress
            }
            // SAFETY: the tag body follows the header and is `size - 8` bytes.
            let body = unsafe { self.cursor.add(mem::size_of::<TagHeader>()) };
            let body_len = size - mem::size_of::<TagHeader>();
            if (self.end as usize) - (body as usize) < body_len {
                return None;
            }
            // Tags are padded up to an 8-byte boundary.
            let advance = (size + 7) & !7;
            // SAFETY: `advance >= size`, and we verified the tag fits.
            self.cursor = unsafe { self.cursor.add(advance) };
            return Some((header.kind, body, body_len));
        }
    }
}

struct MemoryMapIter {
    cursor: *const u8,
    end: *const u8,
    entry_size: usize,
}

impl MemoryMapIter {
    fn new(tag: Option<(*const u8, usize)>) -> Self {
        match tag {
            Some((ptr, len)) if len >= 8 => {
                // SAFETY: length checked; both fields are 4-byte aligned.
                let entry_size = unsafe { (ptr as *const u32).read() } as usize;
                if entry_size < 24 {
                    return Self::empty();
                }
                Self {
                    // SAFETY: entries begin after `entry_size` and `entry_version`.
                    cursor: unsafe { ptr.add(8) },
                    // SAFETY: within the tag body.
                    end: unsafe { ptr.add(len) },
                    entry_size,
                }
            }
            _ => Self::empty(),
        }
    }

    fn empty() -> Self {
        Self { cursor: core::ptr::null(), end: core::ptr::null(), entry_size: 24 }
    }
}

impl Iterator for MemoryMapIter {
    type Item = MemoryRegion;

    fn next(&mut self) -> Option<MemoryRegion> {
        if self.cursor.is_null() || (self.end as usize) - (self.cursor as usize) < self.entry_size {
            return None;
        }
        // SAFETY: we verified a whole entry fits; the layout is fixed by the
        // Multiboot2 specification (u64 base, u64 length, u32 type, u32 pad).
        let region = unsafe {
            MemoryRegion {
                base: (self.cursor as *const u64).read_unaligned(),
                length: (self.cursor.add(8) as *const u64).read_unaligned(),
                kind: match (self.cursor.add(16) as *const u32).read_unaligned() {
                    1 => RegionKind::Usable,
                    2 => RegionKind::Reserved,
                    3 => RegionKind::AcpiReclaimable,
                    4 => RegionKind::AcpiNvs,
                    5 => RegionKind::Defective,
                    other => RegionKind::Unknown(other),
                },
            }
        };
        // SAFETY: bounds checked above.
        self.cursor = unsafe { self.cursor.add(self.entry_size) };
        Some(region)
    }
}

/// Interpret a tag body as a NUL-terminated UTF-8 string.
///
/// # Safety
/// `ptr` must be valid for reads of `len` bytes.
unsafe fn cstr_in<'a>(ptr: *const u8, len: usize) -> Option<&'a str> {
    if len == 0 {
        return Some("");
    }
    // SAFETY: caller guarantees `len` readable bytes.
    let bytes = unsafe { core::slice::from_raw_parts(ptr, len) };
    let nul = bytes.iter().position(|&b| b == 0)?;
    // SAFETY: `nul` is in range and the slice contains a terminator.
    let cstr = unsafe { CStr::from_bytes_with_nul_unchecked(&bytes[..=nul]) };
    cstr.to_str().ok()
}
