//! Emit the NXF container.

use nodeos_abi::nxf;

use crate::elf::Elf;
use crate::{capability_class, Options};

/// Page size used for aligning segment contents in the file.
const PAGE: usize = 4096;

/// Build an NXF image from a parsed ELF plus the packaging options.
pub fn build(elf: &Elf<'_>, options: &Options) -> Result<Vec<u8>, String> {
    let base = elf.base();
    let image_size = elf.end() - base;

    // ---- String table ----------------------------------------------------
    let mut strtab = Vec::new();
    strtab.push(0u8); // offset 0 is the empty string
    let intern = |text: &str, strtab: &mut Vec<u8>| -> u32 {
        let offset = strtab.len() as u32;
        strtab.extend_from_slice(text.as_bytes());
        strtab.push(0);
        offset
    };
    let name_offset = intern(&options.name, &mut strtab);

    // ---- Capability declarations ----------------------------------------
    let mut caps = Vec::new();
    for (class_name, optional, detail) in &options.capabilities {
        let class = capability_class(class_name)
            .ok_or_else(|| format!("unknown capability class {class_name:?}"))?;
        let detail_offset = match detail {
            Some(text) => intern(text, &mut strtab),
            None => 0,
        };
        caps.push(nxf::CapabilityRequirement {
            class,
            flags: if *optional { nxf::cap_flags::OPTIONAL } else { nxf::cap_flags::REQUIRED },
            detail_offset,
            reserved: 0,
        });
    }

    // ---- Dependencies ----------------------------------------------------
    let mut deps = Vec::new();
    for (dep_name, min_version) in &options.dependencies {
        let offset = intern(dep_name, &mut strtab);
        deps.push(nxf::Dependency { name_offset: offset, min_version: *min_version, flags: 0, reserved: 0 });
    }

    // ---- Layout ----------------------------------------------------------
    // Fixed tables first, then the string table, then page-aligned segment
    // contents. Page alignment matters: the loader copies segment bytes into a
    // memory object at their virtual offset, and keeping file and virtual
    // offsets congruent modulo the page size lets that copy stay a memcpy.
    let mut cursor = nxf::HEADER_SIZE;
    let segment_offset = cursor;
    cursor += elf.segments.len() * std::mem::size_of::<nxf::Segment>();
    let reloc_offset = cursor; // no relocations: images are position dependent
    let dep_offset = cursor;
    cursor += deps.len() * std::mem::size_of::<nxf::Dependency>();
    let cap_offset = cursor;
    cursor += caps.len() * std::mem::size_of::<nxf::CapabilityRequirement>();
    let resource_offset = cursor;
    cursor += std::mem::size_of::<nxf::ResourceEnvelope>();
    let strtab_offset = cursor;
    cursor += strtab.len();
    let contents_offset = (cursor + PAGE - 1) & !(PAGE - 1);

    let mut segments = Vec::new();
    let mut contents: Vec<(usize, &[u8])> = Vec::new();
    let mut at = contents_offset;
    for segment in &elf.segments {
        let kind = if segment.executable() {
            nxf::segment_kind::CODE
        } else if segment.writable() {
            nxf::segment_kind::DATA
        } else {
            nxf::segment_kind::RODATA
        };
        let mut flags = 0;
        if segment.readable() {
            flags |= nxf::segment_flags::READ;
        }
        if segment.writable() {
            flags |= nxf::segment_flags::WRITE;
        }
        if segment.executable() {
            flags |= nxf::segment_flags::EXECUTE;
        }

        let data = &elf.bytes[segment.file_offset as usize..(segment.file_offset + segment.file_size) as usize];
        segments.push(nxf::Segment {
            kind,
            flags,
            file_offset: at as u64,
            file_size: segment.file_size,
            virt_addr: segment.virt_addr - base,
            mem_size: segment.mem_size,
            align: segment.align.max(PAGE as u64),
        });
        contents.push((at, data));
        at += ((segment.file_size as usize) + PAGE - 1) & !(PAGE - 1);
    }
    let signature_offset = at;
    let total = signature_offset + std::mem::size_of::<nxf::Signature>();

    // ---- Header ----------------------------------------------------------
    let mut flags = options.flags | nxf::image_flags::SIGNED;
    // The signature record is always present so its offset is stable; the
    // algorithm field records whether it is real.
    flags &= !nxf::image_flags::SIGNED;

    let header = nxf::Header {
        magic: nxf::MAGIC,
        version: nxf::VERSION,
        header_size: nxf::HEADER_SIZE as u16,
        arch: nxf::ARCH_X86_64,
        abi_version: nxf::ABI_VERSION,
        flags,
        entry: elf.entry - base,
        image_base: base,
        image_size,
        segment_offset: segment_offset as u32,
        segment_count: segments.len() as u32,
        reloc_offset: reloc_offset as u32,
        reloc_count: 0,
        dep_offset: if deps.is_empty() { 0 } else { dep_offset as u32 },
        dep_count: deps.len() as u32,
        cap_offset: if caps.is_empty() { 0 } else { cap_offset as u32 },
        cap_count: caps.len() as u32,
        resource_offset: resource_offset as u32,
        strtab_offset: strtab_offset as u32,
        strtab_size: strtab.len() as u32,
        signature_offset: 0,
        signature_size: 0,
        build_id: build_id(elf.bytes),
        checksum: 0,
        name_offset,
        image_version: options.version,
        reserved: [0; 4],
    };

    // ---- Serialise -------------------------------------------------------
    let mut out = vec![0u8; total];
    write_struct(&mut out, 0, &header);
    for (index, segment) in segments.iter().enumerate() {
        write_struct(&mut out, segment_offset + index * std::mem::size_of::<nxf::Segment>(), segment);
    }
    for (index, dep) in deps.iter().enumerate() {
        write_struct(&mut out, dep_offset + index * std::mem::size_of::<nxf::Dependency>(), dep);
    }
    for (index, cap) in caps.iter().enumerate() {
        write_struct(&mut out, cap_offset + index * std::mem::size_of::<nxf::CapabilityRequirement>(), cap);
    }
    write_struct(&mut out, resource_offset, &options.resources);
    out[strtab_offset..strtab_offset + strtab.len()].copy_from_slice(&strtab);
    for (at, data) in contents {
        out[at..at + data.len()].copy_from_slice(data);
    }

    // The signature record sits at the end and is excluded from the checksum,
    // so an image can be signed after the fact without invalidating it.
    let signature = nxf::Signature {
        algorithm: 0, // unsigned
        length: 0,
        public_key: [0; 32],
        signature: [0; 64],
    };
    write_struct(&mut out, signature_offset, &signature);

    // ---- Checksum --------------------------------------------------------
    let checksum = {
        let mut hasher = nxf::crc32::Hasher::new();
        hasher.update(&out[..nxf::CHECKSUM_FIELD_OFFSET]);
        hasher.update(&[0u8; 4]);
        hasher.update(&out[nxf::CHECKSUM_FIELD_OFFSET + 4..]);
        hasher.finish()
    };
    out[nxf::CHECKSUM_FIELD_OFFSET..nxf::CHECKSUM_FIELD_OFFSET + 4].copy_from_slice(&checksum.to_le_bytes());

    Ok(out)
}

/// Write a plain-old-data structure at `at`.
fn write_struct<T: Copy>(out: &mut [u8], at: usize, value: &T) {
    let size = std::mem::size_of::<T>();
    // SAFETY: `T` is a `repr(C)` plain-data structure from the ABI crate, and
    // the destination range is inside `out` by construction of the layout.
    let bytes = unsafe { std::slice::from_raw_parts(value as *const T as *const u8, size) };
    out[at..at + size].copy_from_slice(bytes);
}

/// A cheap, deterministic build identifier: the CRC of the input, repeated.
///
/// Real builds would use the linker's build id; this keeps the tool
/// dependency-free while still giving every distinct image a distinct id.
fn build_id(bytes: &[u8]) -> [u8; 16] {
    let a = nxf::crc32::checksum(bytes);
    let b = nxf::crc32::checksum(&bytes[bytes.len() / 2..]);
    let mut id = [0u8; 16];
    id[0..4].copy_from_slice(&a.to_le_bytes());
    id[4..8].copy_from_slice(&b.to_le_bytes());
    id[8..12].copy_from_slice(&(bytes.len() as u32).to_le_bytes());
    id[12..16].copy_from_slice(&a.rotate_left(7).to_le_bytes());
    id
}
