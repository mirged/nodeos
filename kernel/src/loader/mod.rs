//! The NXF image loader.
//!
//! Loading an image is where NodeOS's two halves meet: the *format* declares
//! what a program needs, and the *launcher* decides what it gets. This module
//! implements the mechanical half -- validate, map, relocate -- and returns a
//! description of what the image asked for so the launcher can answer.
//!
//! The loader never grants anything. It produces a process with an address
//! space and no handles at all. Authority arrives afterwards, over the
//! bootstrap channel, entirely at the launcher's discretion.

use alloc::string::{String, ToString};
use alloc::sync::Arc;
use alloc::vec::Vec;

use nodeos_abi::nxf::{self, Image, NxfError};

use crate::arch::PAGE_SIZE;
use crate::mm::layout::Window;
use crate::mm::space::MapFlags;
use crate::mm::vmo::Vmo;
use crate::obj::{CapClass, CapSet};
use crate::task::Process;

/// What an image asked the launcher for.
#[derive(Debug)]
pub struct Requirements {
    /// Image name from the string table.
    pub name: String,
    /// Capability classes marked required.
    pub required: CapSet,
    /// Capability classes marked optional.
    pub optional: CapSet,
    /// Scope qualifiers, parallel to the declaration order.
    pub details: Vec<(CapClass, String)>,
    /// The declared resource envelope.
    pub resources: nxf::ResourceEnvelope,
    /// Dependencies by name and minimum version.
    pub dependencies: Vec<(String, u32)>,
    /// Whether the image refuses to run with anything withheld.
    pub strict: bool,
}

/// A loaded, not-yet-started image.
pub struct LoadedImage {
    /// Where the image was placed.
    pub base: u64,
    /// Absolute entry point.
    pub entry: u64,
    /// Total virtual size.
    pub size: u64,
    /// Top of the initial stack.
    pub stack_top: u64,
    /// What the image declared.
    pub requirements: Requirements,
}

/// Errors from loading.
#[derive(Debug)]
pub enum LoadError {
    /// The image is malformed.
    Format(NxfError),
    /// Out of memory.
    NoMemory,
    /// The address space rejected a mapping.
    Mapping,
    /// A required capability was not granted.
    CapabilityDenied(CapClass),
    /// A dependency could not be satisfied.
    MissingDependency(String),
}

impl From<NxfError> for LoadError {
    fn from(error: NxfError) -> Self {
        Self::Format(error)
    }
}

/// Inspect an image without loading it, so a launcher can decide before
/// committing any memory.
pub fn inspect(bytes: &[u8]) -> Result<Requirements, LoadError> {
    let image = Image::parse(bytes)?;
    image.verify_checksum()?;
    Ok(requirements(&image))
}

fn requirements(image: &Image<'_>) -> Requirements {
    let mut required = CapSet::NONE;
    let mut optional = CapSet::NONE;
    let mut details = Vec::new();

    for declaration in image.capabilities() {
        let Some(class) = CapClass::from_u32(declaration.class) else { continue };
        if declaration.flags & nxf::cap_flags::OPTIONAL != 0 {
            optional = optional.with(class);
        } else {
            required = required.with(class);
        }
        if declaration.detail_offset != 0 {
            details.push((class, image.string(declaration.detail_offset).to_string()));
        }
    }

    let dependencies = image
        .dependencies()
        .map(|dependency| (image.string(dependency.name_offset).to_string(), dependency.min_version))
        .collect();

    Requirements {
        name: image.name().to_string(),
        required,
        optional,
        details,
        resources: image.resources(),
        dependencies,
        strict: image.header().flags & nxf::image_flags::STRICT_CAPABILITIES != 0,
    }
}

/// Load `bytes` into `process`, granting nothing.
///
/// `granted` is what the launcher has decided to provide; the load fails if a
/// *required* class is missing, because starting a program that will certainly
/// fail is worse than refusing to start it.
pub fn load(process: &Arc<Process>, bytes: &[u8], granted: CapSet) -> Result<LoadedImage, LoadError> {
    let image = Image::parse(bytes)?;
    image.verify_checksum()?;
    let requirements = requirements(&image);

    for class in requirements.required.iter() {
        if !granted.contains(class) {
            return Err(LoadError::CapabilityDenied(class));
        }
    }

    let header = image.header();
    let base = if header.image_base != 0 { header.image_base } else { crate::mm::layout::PROGRAM_BASE };
    let space = process.space();

    // One VMO holds the whole image. Segments become separate *mappings* of it
    // with different permissions, so W^X is enforced by the page tables rather
    // than by the loader remembering to.
    let image_size = (header.image_size + PAGE_SIZE as u64 - 1) & !(PAGE_SIZE as u64 - 1);
    let vmo = Vmo::new_anonymous(image_size, false).map_err(|_| LoadError::NoMemory)?;

    for segment in image.segments() {
        if segment.kind == nxf::segment_kind::NOTE {
            continue;
        }
        let data = image.segment_data(&segment);
        if !data.is_empty() {
            vmo.write(segment.virt_addr, data).map_err(|_| LoadError::Mapping)?;
        }
        // The tail between file_size and mem_size is already zero: VMO pages
        // are committed zero-filled.
    }

    // Relocate before mapping, while the image is still only a memory object
    // and nothing has an executable view of it.
    if header.flags & nxf::image_flags::PIE != 0 {
        apply_relocations(&image, &vmo, base)?;
    }

    // Map each segment with exactly the permissions it declared.
    for segment in image.segments() {
        if segment.kind == nxf::segment_kind::NOTE || segment.mem_size == 0 {
            continue;
        }
        let start = segment.virt_addr & !(PAGE_SIZE as u64 - 1);
        let end = (segment.virt_addr + segment.mem_size + PAGE_SIZE as u64 - 1) & !(PAGE_SIZE as u64 - 1);

        let mut flags = MapFlags(0);
        if segment.flags & nxf::segment_flags::READ != 0 {
            flags = flags.or(MapFlags::READ);
        }
        if segment.flags & nxf::segment_flags::WRITE != 0 {
            flags = flags.or(MapFlags::WRITE);
        }
        if segment.flags & nxf::segment_flags::EXECUTE != 0 {
            flags = flags.or(MapFlags::EXECUTE);
        }
        flags = flags.or(MapFlags::POPULATE);

        space
            .map(vmo.clone(), start, end - start, flags, Window::Program, Some(base + start))
            .map_err(|_| LoadError::Mapping)?;
    }

    // The initial stack: its own VMO in the stack window, with a guard page
    // below it that is deliberately left unmapped.
    let stack_size = requirements.resources.stack_size.max(64 * 1024);
    let stack_size = (stack_size + PAGE_SIZE as u64 - 1) & !(PAGE_SIZE as u64 - 1);
    let stack_vmo = Vmo::new_anonymous(stack_size, false).map_err(|_| LoadError::NoMemory)?;
    let stack_base = space
        .map(
            stack_vmo,
            0,
            stack_size,
            MapFlags::READ.or(MapFlags::WRITE).or(MapFlags::POPULATE),
            Window::Stack,
            None,
        )
        .map_err(|_| LoadError::Mapping)?;

    // Leave a 16-byte aligned stack pointer with a null return address below it,
    // so a stray `ret` out of the entry point faults on a null instruction
    // fetch rather than wandering.
    let stack_top = (stack_base + stack_size - 16) & !0xF;

    Ok(LoadedImage {
        base,
        entry: base + header.entry,
        size: image_size,
        stack_top,
        requirements,
    })
}

/// Apply relocations to the image while it is still just a memory object.
fn apply_relocations(image: &Image<'_>, vmo: &Arc<Vmo>, base: u64) -> Result<(), LoadError> {
    for reloc in image.relocations() {
        match reloc.kind {
            nxf::reloc_kind::RELATIVE => {
                let value = base.wrapping_add(reloc.addend as u64);
                vmo.write(reloc.offset, &value.to_le_bytes()).map_err(|_| LoadError::Mapping)?;
            }
            nxf::reloc_kind::ABSOLUTE64 | nxf::reloc_kind::PC_RELATIVE32 => {
                // Symbolic relocations need an export table from a dependency,
                // which arrives with the dynamic loading work. Refusing here is
                // better than writing a plausible-looking wrong address.
                return Err(LoadError::Format(NxfError::BadSegment));
            }
            _ => return Err(LoadError::Format(NxfError::BadSegment)),
        }
    }
    Ok(())
}
