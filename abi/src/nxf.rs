//! # NXF: the NodeOS eXecutable Format
//!
//! ELF is a linker's format that an operating system was later taught to load.
//! Everything a *loader* needs is inferred: which segments are really code,
//! what the program will do at runtime, whether the image is the one the
//! packager signed. NXF inverts that. It is a loader's format, and the things
//! the loader must decide are stated in the header rather than deduced.
//!
//! Concretely, an NXF image declares:
//!
//! * its architecture and the ABI version it was built against, so a mismatched
//!   image is rejected at load rather than at its first bad syscall;
//! * its segments, with permissions the *format* checks for W^X;
//! * its relocations, in one flat table (no dynamic-linker state machine);
//! * its dependencies, by name and minimum version;
//! * **the capability classes it requires**, each marked required or optional;
//! * **its resource envelope**: memory, threads, handles, stack, scheduling
//!   class -- a declaration the launcher turns into a sandbox;
//! * an optional detached signature over everything above.
//!
//! The capability declaration is the interesting part. It is a *request*, not
//! a grant. The image says "I need `FILE_READ` under `/etc` and `NETWORK`";
//! the launcher decides what it actually gets, and the process starts with
//! exactly the handles that decision produced. An image that overstates its
//! needs gains nothing -- it simply fails when it uses a capability it was
//! never handed.
//!
//! ## Layout
//!
//! ```text
//!   +----------------------+ 0
//!   | Header (136 bytes)   |
//!   +----------------------+
//!   | Segment table        |  segment_count * 48
//!   +----------------------+
//!   | Relocation table     |  reloc_count * 24
//!   +----------------------+
//!   | Dependency table     |  dep_count * 16
//!   +----------------------+
//!   | Capability table     |  cap_count * 16
//!   +----------------------+
//!   | Resource envelope    |  72
//!   +----------------------+
//!   | String table         |  NUL-separated UTF-8
//!   +----------------------+
//!   | Segment contents     |  page aligned
//!   +----------------------+
//!   | Signature (optional) |
//!   +----------------------+
//! ```

use core::mem::size_of;

/// `"NXF\0"`.
pub const MAGIC: [u8; 4] = *b"NXF\0";
/// Current format version.
pub const VERSION: u16 = 1;
/// Architecture value for x86-64.
pub const ARCH_X86_64: u16 = 1;
/// Current NodeOS ABI version. An image built against a different one is
/// refused rather than tolerated.
pub const ABI_VERSION: u16 = 1;
/// Size of [`Header`].
pub const HEADER_SIZE: usize = 136;
/// Byte offset of [`Header::checksum`] within the header.
pub const CHECKSUM_FIELD_OFFSET: usize = 108;

/// Image-level flags.
pub mod image_flags {
    /// The image is position independent and carries relocations.
    pub const PIE: u32 = 1 << 0;
    /// A detached signature is present.
    pub const SIGNED: u32 = 1 << 1;
    /// The image is a long-lived system service.
    pub const SERVICE: u32 = 1 << 2;
    /// The image is a device driver and expects `DEVICE` capabilities.
    pub const DRIVER: u32 = 1 << 3;
    /// Refuse to start unless every declared capability was granted.
    pub const STRICT_CAPABILITIES: u32 = 1 << 4;
    /// The image runs correctly with no capabilities at all.
    pub const PURE: u32 = 1 << 5;
}

/// Segment kinds.
pub mod segment_kind {
    /// Executable code.
    pub const CODE: u32 = 1;
    /// Read-only data.
    pub const RODATA: u32 = 2;
    /// Initialised writable data.
    pub const DATA: u32 = 3;
    /// Zero-filled writable data.
    pub const BSS: u32 = 4;
    /// Thread-local storage template.
    pub const TLS: u32 = 5;
    /// Loader-visible metadata, not mapped.
    pub const NOTE: u32 = 6;
}

/// Segment permission bits.
pub mod segment_flags {
    /// Readable.
    pub const READ: u32 = 1 << 0;
    /// Writable.
    pub const WRITE: u32 = 1 << 1;
    /// Executable.
    pub const EXECUTE: u32 = 1 << 2;
}

/// Relocation kinds.
pub mod reloc_kind {
    /// `*(u64*)(base + offset) = base + addend`
    pub const RELATIVE: u32 = 1;
    /// `*(u64*)(base + offset) = symbol + addend`
    pub const ABSOLUTE64: u32 = 2;
    /// `*(u32*)(base + offset) = (symbol + addend) - (base + offset + 4)`
    pub const PC_RELATIVE32: u32 = 3;
}

/// Capability requirement flags.
pub mod cap_flags {
    /// The image cannot run without this capability.
    pub const REQUIRED: u32 = 1 << 0;
    /// The image adapts if this capability is withheld.
    pub const OPTIONAL: u32 = 1 << 1;
}

/// The NXF file header.
#[repr(C)]
#[derive(Clone, Copy, Debug)]
pub struct Header {
    /// [`MAGIC`].
    pub magic: [u8; 4],
    /// Format version.
    pub version: u16,
    /// Size of this header.
    pub header_size: u16,
    /// Target architecture.
    pub arch: u16,
    /// NodeOS ABI version this image was built against.
    pub abi_version: u16,
    /// See [`image_flags`].
    pub flags: u32,
    /// Entry point, relative to the image base.
    pub entry: u64,
    /// Preferred load address, or zero for position independent.
    pub image_base: u64,
    /// Total virtual size of the loaded image.
    pub image_size: u64,
    /// Offset of the segment table.
    pub segment_offset: u32,
    /// Number of segments.
    pub segment_count: u32,
    /// Offset of the relocation table.
    pub reloc_offset: u32,
    /// Number of relocations.
    pub reloc_count: u32,
    /// Offset of the dependency table.
    pub dep_offset: u32,
    /// Number of dependencies.
    pub dep_count: u32,
    /// Offset of the capability table.
    pub cap_offset: u32,
    /// Number of capability requirements.
    pub cap_count: u32,
    /// Offset of the resource envelope.
    pub resource_offset: u32,
    /// Offset of the string table.
    pub strtab_offset: u32,
    /// Size of the string table.
    pub strtab_size: u32,
    /// Offset of the detached signature, or zero.
    pub signature_offset: u32,
    /// Size of the detached signature.
    pub signature_size: u32,
    /// Build identifier, for symbolication and reproducibility checks.
    pub build_id: [u8; 16],
    /// CRC-32 of the whole file with this field and the signature excluded.
    pub checksum: u32,
    /// Offset of the image name in the string table.
    pub name_offset: u32,
    /// Image version, for dependency resolution.
    pub image_version: u32,
    /// Reserved, must be zero.
    pub reserved: [u32; 4],
}

/// One loadable segment.
#[repr(C)]
#[derive(Clone, Copy, Debug)]
pub struct Segment {
    /// See [`segment_kind`].
    pub kind: u32,
    /// See [`segment_flags`].
    pub flags: u32,
    /// Offset of the contents in the file.
    pub file_offset: u64,
    /// Bytes present in the file.
    pub file_size: u64,
    /// Virtual address, relative to the image base.
    pub virt_addr: u64,
    /// Virtual size; the excess over `file_size` is zero-filled.
    pub mem_size: u64,
    /// Required alignment.
    pub align: u64,
}

/// One relocation.
#[repr(C)]
#[derive(Clone, Copy, Debug)]
pub struct Relocation {
    /// See [`reloc_kind`].
    pub kind: u32,
    /// Index into the dependency table, for symbolic relocations.
    pub symbol: u32,
    /// Where to apply, relative to the image base.
    pub offset: u64,
    /// Constant to add.
    pub addend: i64,
}

/// One dependency on another image.
#[repr(C)]
#[derive(Clone, Copy, Debug)]
pub struct Dependency {
    /// Offset of the name in the string table.
    pub name_offset: u32,
    /// Minimum acceptable version.
    pub min_version: u32,
    /// Reserved flags.
    pub flags: u32,
    /// Padding.
    pub reserved: u32,
}

/// One declared capability requirement.
#[repr(C)]
#[derive(Clone, Copy, Debug)]
pub struct CapabilityRequirement {
    /// A [`crate::types::CapClass`] discriminant.
    pub class: u32,
    /// See [`cap_flags`].
    pub flags: u32,
    /// String-table offset of a scope qualifier -- a path prefix for
    /// `FILE_READ`, a device class for `DEVICE`, a port range for
    /// `NETWORK_BIND`. Zero means unqualified.
    pub detail_offset: u32,
    /// Padding.
    pub reserved: u32,
}

/// The resource envelope: what the image asks to be allowed to consume.
///
/// These are declarations, not privileges. A launcher may impose tighter
/// limits and is never obliged to grant what is asked. Putting them in the
/// image means the sandbox exists from the first instruction rather than being
/// configured by whoever remembers to.
#[repr(C)]
#[derive(Clone, Copy, Debug)]
pub struct ResourceEnvelope {
    /// Maximum bytes of memory the process may have mapped.
    pub max_memory_bytes: u64,
    /// Initial stack size for the first thread.
    pub stack_size: u64,
    /// Heap to reserve at startup.
    pub heap_reserve: u64,
    /// Requested period for the real-time class, in nanoseconds.
    pub cpu_period_ns: u64,
    /// Requested CPU budget per period, in nanoseconds.
    pub cpu_budget_ns: u64,
    /// Maximum threads.
    pub max_threads: u32,
    /// Maximum handles.
    pub max_handles: u32,
    /// Maximum channels.
    pub max_channels: u32,
    /// Requested scheduling class.
    pub sched_class: u32,
    /// Requested priority within the class.
    pub sched_priority: u32,
    /// Padding.
    pub reserved: [u32; 5],
}

impl Default for ResourceEnvelope {
    fn default() -> Self {
        Self {
            max_memory_bytes: 64 * 1024 * 1024,
            stack_size: 128 * 1024,
            heap_reserve: 1024 * 1024,
            cpu_period_ns: 0,
            cpu_budget_ns: 0,
            max_threads: 16,
            max_handles: 256,
            max_channels: 64,
            sched_class: crate::types::SchedClass::Normal as u32,
            sched_priority: 16,
            reserved: [0; 5],
        }
    }
}

/// Signature algorithm identifiers.
pub mod signature_algorithm {
    /// Ed25519 over the checksum-covered region.
    pub const ED25519: u32 = 1;
}

/// A detached signature.
#[repr(C)]
#[derive(Clone, Copy, Debug)]
pub struct Signature {
    /// See [`signature_algorithm`].
    pub algorithm: u32,
    /// Length of the signature bytes actually used.
    pub length: u32,
    /// Signer's public key.
    pub public_key: [u8; 32],
    /// Signature bytes.
    pub signature: [u8; 64],
}

/// Errors from parsing an NXF image.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum NxfError {
    /// The file is shorter than a header.
    TooShort,
    /// Magic mismatch.
    BadMagic,
    /// Unsupported format version.
    BadVersion,
    /// Built for another architecture.
    WrongArchitecture,
    /// Built against a different kernel ABI.
    WrongAbi,
    /// A table offset or count runs past the end of the file.
    OutOfBounds,
    /// A segment is both writable and executable.
    WriteExecute,
    /// A segment is malformed, or runs past `image_size`.
    BadSegment,
    /// The entry point is not inside an executable segment.
    BadEntry,
    /// The checksum does not match.
    BadChecksum,
    /// The signature is absent, malformed, or does not verify.
    BadSignature,
}

/// A parsed, bounds-checked view over an NXF image.
///
/// Parsing performs every structural check exactly once, so the loader can walk
/// the tables afterwards without re-validating. Nothing is copied.
pub struct Image<'a> {
    bytes: &'a [u8],
    header: Header,
}

impl<'a> Image<'a> {
    /// Parse and validate `bytes`.
    pub fn parse(bytes: &'a [u8]) -> Result<Self, NxfError> {
        if bytes.len() < HEADER_SIZE {
            return Err(NxfError::TooShort);
        }
        // SAFETY: `Header` is `repr(C)` plain-old-data and at least
        // `HEADER_SIZE` bytes are available. The read is unaligned because
        // `bytes` carries no alignment guarantee.
        let header: Header = unsafe { (bytes.as_ptr() as *const Header).read_unaligned() };

        if header.magic != MAGIC {
            return Err(NxfError::BadMagic);
        }
        if header.version != VERSION || header.header_size as usize != HEADER_SIZE {
            return Err(NxfError::BadVersion);
        }
        if header.arch != ARCH_X86_64 {
            return Err(NxfError::WrongArchitecture);
        }
        if header.abi_version != ABI_VERSION {
            return Err(NxfError::WrongAbi);
        }

        let image = Self { bytes, header };
        image.check_table(header.segment_offset, header.segment_count, size_of::<Segment>())?;
        image.check_table(header.reloc_offset, header.reloc_count, size_of::<Relocation>())?;
        image.check_table(header.dep_offset, header.dep_count, size_of::<Dependency>())?;
        image.check_table(header.cap_offset, header.cap_count, size_of::<CapabilityRequirement>())?;
        image.check_table(header.resource_offset, 1, size_of::<ResourceEnvelope>())?;
        image.check_table(header.strtab_offset, header.strtab_size, 1)?;
        if header.signature_offset != 0 {
            image.check_table(header.signature_offset, 1, size_of::<Signature>())?;
        }

        // Structural rules that make the loader's job total rather than partial.
        let mut entry_ok = false;
        for segment in image.segments() {
            if segment.kind == segment_kind::NOTE {
                continue;
            }
            let writable = segment.flags & segment_flags::WRITE != 0;
            let executable = segment.flags & segment_flags::EXECUTE != 0;
            if writable && executable {
                // W^X is a property of the *format*, checked once here, rather
                // than a policy the loader may forget to apply.
                return Err(NxfError::WriteExecute);
            }
            if segment.file_size > segment.mem_size {
                return Err(NxfError::BadSegment);
            }
            if segment.kind != segment_kind::BSS && segment.file_size > 0 {
                let end = segment.file_offset.checked_add(segment.file_size).ok_or(NxfError::OutOfBounds)?;
                if end > bytes.len() as u64 {
                    return Err(NxfError::OutOfBounds);
                }
            }
            let vend = segment.virt_addr.checked_add(segment.mem_size).ok_or(NxfError::BadSegment)?;
            if vend > header.image_size {
                return Err(NxfError::BadSegment);
            }
            if executable && header.entry >= segment.virt_addr && header.entry < vend {
                entry_ok = true;
            }
        }
        if !entry_ok {
            return Err(NxfError::BadEntry);
        }

        for reloc in image.relocations() {
            if reloc.offset.checked_add(8).ok_or(NxfError::OutOfBounds)? > header.image_size {
                return Err(NxfError::OutOfBounds);
            }
        }

        Ok(image)
    }

    fn check_table(&self, offset: u32, count: u32, stride: usize) -> Result<(), NxfError> {
        if count == 0 && offset == 0 {
            return Ok(());
        }
        let end = (offset as u64).checked_add((count as u64) * stride as u64).ok_or(NxfError::OutOfBounds)?;
        if end > self.bytes.len() as u64 {
            return Err(NxfError::OutOfBounds);
        }
        Ok(())
    }

    /// The header.
    pub fn header(&self) -> &Header {
        &self.header
    }

    /// The raw bytes.
    pub fn bytes(&self) -> &'a [u8] {
        self.bytes
    }

    fn table<T: Copy>(&self, offset: u32, count: u32) -> impl Iterator<Item = T> + '_ {
        let stride = size_of::<T>();
        (0..count as usize).map(move |i| {
            let at = offset as usize + i * stride;
            // SAFETY: `check_table` verified during parsing that
            // `offset + count * stride` fits inside `bytes`.
            unsafe { (self.bytes.as_ptr().add(at) as *const T).read_unaligned() }
        })
    }

    /// The segment table.
    pub fn segments(&self) -> impl Iterator<Item = Segment> + '_ {
        self.table(self.header.segment_offset, self.header.segment_count)
    }

    /// The relocation table.
    pub fn relocations(&self) -> impl Iterator<Item = Relocation> + '_ {
        self.table(self.header.reloc_offset, self.header.reloc_count)
    }

    /// The dependency table.
    pub fn dependencies(&self) -> impl Iterator<Item = Dependency> + '_ {
        self.table(self.header.dep_offset, self.header.dep_count)
    }

    /// The declared capability requirements.
    pub fn capabilities(&self) -> impl Iterator<Item = CapabilityRequirement> + '_ {
        self.table(self.header.cap_offset, self.header.cap_count)
    }

    /// The resource envelope, or the default if the image omitted one.
    pub fn resources(&self) -> ResourceEnvelope {
        if self.header.resource_offset == 0 {
            return ResourceEnvelope::default();
        }
        // SAFETY: bounds checked during parsing.
        unsafe {
            (self.bytes.as_ptr().add(self.header.resource_offset as usize) as *const ResourceEnvelope)
                .read_unaligned()
        }
    }

    /// Resolve a string-table offset.
    pub fn string(&self, offset: u32) -> &'a str {
        if offset >= self.header.strtab_size {
            return "";
        }
        let start = self.header.strtab_offset as usize + offset as usize;
        let limit = self.header.strtab_offset as usize + self.header.strtab_size as usize;
        if start >= limit || limit > self.bytes.len() {
            return "";
        }
        let slice = &self.bytes[start..limit];
        let end = slice.iter().position(|&b| b == 0).unwrap_or(slice.len());
        core::str::from_utf8(&slice[..end]).unwrap_or("")
    }

    /// The image's own name.
    pub fn name(&self) -> &'a str {
        self.string(self.header.name_offset)
    }

    /// Contents of a segment, as stored in the file.
    pub fn segment_data(&self, segment: &Segment) -> &'a [u8] {
        if segment.kind == segment_kind::BSS || segment.file_size == 0 {
            return &[];
        }
        let start = segment.file_offset as usize;
        let end = start + segment.file_size as usize;
        &self.bytes[start..end]
    }

    /// The detached signature, if present.
    pub fn signature(&self) -> Option<Signature> {
        if self.header.signature_offset == 0 {
            return None;
        }
        // SAFETY: bounds checked during parsing.
        Some(unsafe {
            (self.bytes.as_ptr().add(self.header.signature_offset as usize) as *const Signature).read_unaligned()
        })
    }

    /// Recompute the CRC-32 over the region the checksum covers.
    pub fn computed_checksum(&self) -> u32 {
        let mut crc = crc32::Hasher::new();
        crc.update(&self.bytes[..CHECKSUM_FIELD_OFFSET]);
        crc.update(&[0u8; 4]);
        let after = CHECKSUM_FIELD_OFFSET + 4;
        let signature_start = if self.header.signature_offset == 0 {
            self.bytes.len()
        } else {
            self.header.signature_offset as usize
        };
        crc.update(&self.bytes[after..signature_start]);
        crc.finish()
    }

    /// Verify the checksum. A zero checksum means "unchecked", which is an
    /// explicit choice a packager has to make rather than an oversight.
    pub fn verify_checksum(&self) -> Result<(), NxfError> {
        if self.header.checksum == 0 {
            return Ok(());
        }
        if self.computed_checksum() == self.header.checksum {
            Ok(())
        } else {
            Err(NxfError::BadChecksum)
        }
    }
}

/// A tiny CRC-32 (IEEE), so neither the kernel nor the host tools need a
/// dependency for an integrity check.
pub mod crc32 {
    /// Streaming CRC-32 state.
    pub struct Hasher {
        state: u32,
    }

    impl Hasher {
        /// A fresh hasher.
        pub const fn new() -> Self {
            Self { state: 0xFFFF_FFFF }
        }

        /// Absorb bytes.
        pub fn update(&mut self, bytes: &[u8]) {
            for &byte in bytes {
                self.state ^= byte as u32;
                for _ in 0..8 {
                    let mask = (self.state & 1).wrapping_neg();
                    self.state = (self.state >> 1) ^ (0xEDB8_8320 & mask);
                }
            }
        }

        /// Finish.
        pub fn finish(&self) -> u32 {
            !self.state
        }
    }

    impl Default for Hasher {
        fn default() -> Self {
            Self::new()
        }
    }

    /// One-shot CRC-32.
    pub fn checksum(bytes: &[u8]) -> u32 {
        let mut hasher = Hasher::new();
        hasher.update(bytes);
        hasher.finish()
    }
}
