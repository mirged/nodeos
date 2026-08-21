//! Just enough ELF64 to read a statically linked, non-PIE executable.
//!
//! Deliberately hand-rolled rather than pulled from a crate: the subset that
//! matters here is the program header table, and forty lines of it that we
//! fully understand beats a dependency whose failure modes we do not.

/// One loadable program header.
#[derive(Debug, Clone, Copy)]
pub struct Segment {
    /// Offset of the contents in the file.
    pub file_offset: u64,
    /// Bytes present in the file.
    pub file_size: u64,
    /// Virtual address.
    pub virt_addr: u64,
    /// Virtual size; the excess over `file_size` is zero-filled.
    pub mem_size: u64,
    /// ELF `p_flags`: 1 = X, 2 = W, 4 = R.
    pub flags: u32,
    /// Required alignment.
    pub align: u64,
}

impl Segment {
    /// Whether the segment is readable.
    pub fn readable(&self) -> bool {
        self.flags & 4 != 0
    }
    /// Whether the segment is writable.
    pub fn writable(&self) -> bool {
        self.flags & 2 != 0
    }
    /// Whether the segment is executable.
    pub fn executable(&self) -> bool {
        self.flags & 1 != 0
    }
}

/// A parsed ELF64 executable.
pub struct Elf<'a> {
    /// The whole file.
    pub bytes: &'a [u8],
    /// Entry point virtual address.
    pub entry: u64,
    /// Loadable segments, in file order.
    pub segments: Vec<Segment>,
}

const PT_LOAD: u32 = 1;

impl<'a> Elf<'a> {
    /// Parse `bytes`.
    pub fn parse(bytes: &'a [u8]) -> Result<Self, String> {
        if bytes.len() < 64 {
            return Err("file is too short to be an ELF".into());
        }
        if &bytes[0..4] != b"\x7fELF" {
            return Err("not an ELF file".into());
        }
        if bytes[4] != 2 {
            return Err("not a 64-bit ELF".into());
        }
        if bytes[5] != 1 {
            return Err("not a little-endian ELF".into());
        }
        let machine = u16::from_le_bytes([bytes[18], bytes[19]]);
        if machine != 0x3E {
            return Err(format!("not an x86-64 ELF (e_machine = {machine:#x})"));
        }

        let entry = read_u64(bytes, 24);
        let phoff = read_u64(bytes, 32) as usize;
        let phentsize = read_u16(bytes, 54) as usize;
        let phnum = read_u16(bytes, 56) as usize;

        if phentsize < 56 || phoff + phnum * phentsize > bytes.len() {
            return Err("program header table is out of bounds".into());
        }

        let mut segments = Vec::new();
        for index in 0..phnum {
            let at = phoff + index * phentsize;
            let kind = read_u32(bytes, at);
            if kind != PT_LOAD {
                continue;
            }
            let segment = Segment {
                flags: read_u32(bytes, at + 4),
                file_offset: read_u64(bytes, at + 8),
                virt_addr: read_u64(bytes, at + 16),
                file_size: read_u64(bytes, at + 32),
                mem_size: read_u64(bytes, at + 40),
                align: read_u64(bytes, at + 48),
            };
            if segment.file_offset + segment.file_size > bytes.len() as u64 {
                return Err(format!("segment {index} runs past the end of the file"));
            }
            if segment.writable() && segment.executable() {
                return Err(format!(
                    "segment {index} at {:#x} is both writable and executable; NXF forbids it",
                    segment.virt_addr
                ));
            }
            segments.push(segment);
        }

        if segments.is_empty() {
            return Err("no loadable segments".into());
        }
        Ok(Self { bytes, entry, segments })
    }

    /// Lowest virtual address across all loadable segments.
    pub fn base(&self) -> u64 {
        self.segments.iter().map(|s| s.virt_addr).min().unwrap_or(0)
    }

    /// One past the highest virtual address.
    pub fn end(&self) -> u64 {
        self.segments.iter().map(|s| s.virt_addr + s.mem_size).max().unwrap_or(0)
    }
}

fn read_u16(bytes: &[u8], at: usize) -> u16 {
    u16::from_le_bytes([bytes[at], bytes[at + 1]])
}

fn read_u32(bytes: &[u8], at: usize) -> u32 {
    u32::from_le_bytes([bytes[at], bytes[at + 1], bytes[at + 2], bytes[at + 3]])
}

fn read_u64(bytes: &[u8], at: usize) -> u64 {
    let mut value = [0u8; 8];
    value.copy_from_slice(&bytes[at..at + 8]);
    u64::from_le_bytes(value)
}
