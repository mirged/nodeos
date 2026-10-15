//! FAT16 and FAT32, read and write.
//!
//! # Why FAT
//!
//! Because the disk has to be readable by something other than NodeOS. A
//! bespoke on-disk format would have been less code -- no 8.3 names, no two
//! copies of the allocation table, no long-name entries to reassemble -- and
//! it would have made the filesystem a closed loop: the only way to get a file
//! onto it would be a program written against it, and the only way to check
//! what NodeOS wrote would be to boot NodeOS. `make disk` builds this image
//! with `mformat` and `mcopy`, and anything written here can be read back on
//! the host afterwards. That is worth more than the simplicity.
//!
//! # The layout, once
//!
//! ```text
//!   [ reserved ][ FAT #1 ][ FAT #2 ][ root dir ][ data clusters ... ]
//!     boot         chain     copy      FAT16      cluster 2 first
//!     sector       table               only
//! ```
//!
//! A file is a *chain*: its directory entry names a first cluster, and the
//! allocation table says which cluster follows each one. So a seek is a walk,
//! which is why [`Fat::read`] starts from the first cluster every time rather
//! than caching a position -- correct, and honest about the cost.
//!
//! On FAT32 the root directory is an ordinary chain like any other; on FAT16
//! it is a fixed region before the data, with a fixed number of slots. That
//! difference is the only place in this file where the two are not the same
//! code, and [`Dir`] is the type that names it.
//!
//! # What is not implemented
//!
//! * **FAT12.** Twelve-bit entries straddle byte boundaries; nothing this side
//!   of a floppy uses it.
//! * **Creating long names.** Long names are *read* -- the assembly of
//!   `0x0F` entries is in [`Fat::list`] -- but a file created here gets an
//!   8.3 name, and a request for a name that does not fit one is refused
//!   rather than silently mangled.
//! * **Timestamps.** NodeOS's clock is nanoseconds since boot; there is no
//!   wall clock to write into a directory entry, so created and modified
//!   dates are left as the host tools set them.

use alloc::string::String;
use alloc::vec;
use alloc::vec::Vec;

use libnode::Error;

use crate::virtio::{VirtioBlk, SECTOR_SIZE};

/// Attribute bit: read-only.
const ATTR_READ_ONLY: u8 = 0x01;
/// Attribute bit: this entry names the volume, not a file.
const ATTR_VOLUME_ID: u8 = 0x08;
/// Attribute bit: a directory.
pub const ATTR_DIRECTORY: u8 = 0x10;
/// All four bits set marks a long-name fragment rather than a real entry.
const ATTR_LONG_NAME: u8 = 0x0F;

/// Marker in `name[0]`: this slot and every slot after it is free.
const ENTRY_END: u8 = 0x00;
/// Marker in `name[0]`: this slot held a file that was deleted.
const ENTRY_DELETED: u8 = 0xE5;

/// Bytes in one directory entry.
const ENTRY_SIZE: usize = 32;

/// The chain value meaning "no more clusters".
const CLUSTER_END: u32 = 0x0FFF_FFF8;
/// The chain value for a cluster that is not allocated.
const CLUSTER_FREE: u32 = 0;

/// Which flavour of FAT this volume is.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Flavour {
    /// 16-bit table entries, fixed-size root directory.
    Fat16,
    /// 32-bit table entries, root directory is a normal chain.
    Fat32,
}

/// Where a directory's entries live.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Dir {
    /// FAT16's fixed root region.
    FixedRoot,
    /// A cluster chain, starting here.
    Chain(u32),
}

/// A resolved directory entry: what a file *is*, plus where the record of it
/// lives so that a write can update it.
#[derive(Clone, Debug)]
pub struct Entry {
    /// First cluster of the contents, or 0 for an empty file.
    pub cluster: u32,
    /// Length in bytes. Meaningless for a directory, which is as long as its
    /// chain.
    pub size: u32,
    /// FAT attribute byte.
    pub attr: u8,
    /// Sector holding this entry's 32-byte record, or 0 for the root itself.
    pub record_sector: u64,
    /// Byte offset of the record within that sector.
    pub record_offset: usize,
}

impl Entry {
    /// Whether this entry is a directory.
    pub fn is_dir(&self) -> bool {
        self.attr & ATTR_DIRECTORY != 0
    }

    /// This entry's contents, as a place to look for further entries.
    pub fn as_dir(&self, flavour: Flavour) -> Dir {
        // On FAT16 a subdirectory whose first cluster is zero cannot happen --
        // every directory has at least `.` and `..` -- but the *root* is
        // recorded as cluster zero, and that is the case this maps.
        if self.cluster == 0 && flavour == Flavour::Fat16 {
            Dir::FixedRoot
        } else {
            Dir::Chain(self.cluster)
        }
    }
}

/// One entry as [`Fat::list`] reports it.
pub struct Listing {
    /// Long name where there was one, 8.3 otherwise.
    pub name: String,
    /// FAT attribute byte.
    pub attr: u8,
    /// Length in bytes.
    pub size: u32,
}

/// A mounted FAT volume.
pub struct Fat {
    disk: VirtioBlk,
    flavour: Flavour,
    bytes_per_sector: u32,
    sectors_per_cluster: u32,
    /// First sector of the first allocation table.
    fat_start: u64,
    /// Sectors in one allocation table.
    fat_sectors: u32,
    /// How many copies of the table the volume keeps. Every write updates all
    /// of them, which is the entire point of their existing.
    fat_copies: u32,
    /// First sector of the FAT16 root directory region.
    root_start: u64,
    /// Sectors in that region.
    root_sectors: u32,
    /// FAT32's root directory cluster.
    root_cluster: u32,
    /// First sector of the data area (cluster 2).
    data_start: u64,
    /// Clusters on the volume, counted from 2.
    clusters: u32,
    /// One cached allocation-table sector. A chain walk reads the same sector
    /// repeatedly -- consecutive clusters share one -- and every hit here is a
    /// disk round trip that does not happen.
    fat_cache: Option<(u64, Vec<u8>)>,
    /// Whether the cached sector has been modified and not yet written back.
    fat_dirty: bool,
}

impl Fat {
    /// Read the boot sector and work out the geometry.
    pub fn mount(mut disk: VirtioBlk) -> Result<Self, Error> {
        let mut boot = vec![0u8; SECTOR_SIZE];
        disk.read(0, &mut boot)?;

        let bytes_per_sector = u16(&boot, 0x0B) as u32;
        let sectors_per_cluster = boot[0x0D] as u32;
        let reserved = u16(&boot, 0x0E) as u32;
        let fat_copies = boot[0x10] as u32;
        let root_entries = u16(&boot, 0x11) as u32;
        let total_16 = u16(&boot, 0x13) as u32;
        let fat_16 = u16(&boot, 0x16) as u32;
        let total_32 = u32(&boot, 0x20);
        let fat_32 = u32(&boot, 0x24);

        // A boot sector that is not one at all reads as zeroes, and every
        // derived number below would divide by zero. Check the shape first.
        if bytes_per_sector as usize != SECTOR_SIZE || sectors_per_cluster == 0 || fat_copies == 0 {
            return Err(Error::BadState);
        }

        let fat_sectors = if fat_16 != 0 { fat_16 } else { fat_32 };
        let total_sectors = if total_16 != 0 { total_16 } else { total_32 };
        if fat_sectors == 0 || total_sectors == 0 {
            return Err(Error::BadState);
        }

        let root_sectors = (root_entries * ENTRY_SIZE as u32).div_ceil(bytes_per_sector);
        let root_start = reserved as u64 + (fat_copies as u64 * fat_sectors as u64);
        let data_start = root_start + root_sectors as u64;

        // Compute the cluster count in 64 bits and reject anything that does
        // not fit the rest of this file's `clusters + 2` bound. Doing the
        // subtraction in `u32` (as this used to) truncated `data_start` and
        // could produce a `clusters` value within two of `u32::MAX`, making
        // every later `clusters + 2` overflow and panic on a crafted volume.
        if data_start >= total_sectors as u64 {
            return Err(Error::BadState);
        }
        let cluster_count = (total_sectors as u64 - data_start) / sectors_per_cluster as u64;
        if cluster_count > (u32::MAX - 2) as u64 {
            return Err(Error::NotSupported);
        }
        let clusters = cluster_count as u32;

        // The count of clusters is what decides the flavour -- not the label
        // in the boot sector, which is documentation. This is the rule from
        // Microsoft's own specification, and the two-cluster offset is because
        // cluster numbering starts at 2.
        let flavour = if clusters < 4085 {
            return Err(Error::NotSupported);
        } else if clusters < 65525 {
            Flavour::Fat16
        } else {
            Flavour::Fat32
        };

        // A FAT32 root directory lives in a real cluster chain, and clusters
        // below 2 are not valid. A zero here would make `cluster_sector`
        // subtract two from zero the first time the root fills up.
        let root_cluster = if flavour == Flavour::Fat32 { u32(&boot, 0x2C) } else { 0 };
        if flavour == Flavour::Fat32 && root_cluster < 2 {
            return Err(Error::BadState);
        }

        Ok(Self {
            disk,
            flavour,
            bytes_per_sector,
            sectors_per_cluster,
            fat_start: reserved as u64,
            fat_sectors,
            fat_copies,
            root_start,
            root_sectors,
            root_cluster,
            data_start,
            clusters,
            fat_cache: None,
            fat_dirty: false,
        })
    }

    /// Which flavour this volume is.
    pub fn flavour(&self) -> Flavour {
        self.flavour
    }

    /// Bytes in one cluster.
    pub fn cluster_bytes(&self) -> u32 {
        self.bytes_per_sector * self.sectors_per_cluster
    }

    /// Total and free bytes, and the number of requests the disk has served.
    pub fn usage(&mut self) -> Result<(u64, u64, u64), Error> {
        let mut free = 0u64;
        for cluster in 2..(self.clusters + 2) {
            if self.chain_next(cluster)? == CLUSTER_FREE {
                free += 1;
            }
        }
        let total = self.clusters as u64 * self.cluster_bytes() as u64;
        Ok((total, free * self.cluster_bytes() as u64, self.disk.requests()))
    }

    /// The root directory, as an entry.
    pub fn root(&self) -> Entry {
        Entry {
            cluster: if self.flavour == Flavour::Fat32 { self.root_cluster } else { 0 },
            size: 0,
            attr: ATTR_DIRECTORY,
            record_sector: 0,
            record_offset: 0,
        }
    }

    // -----------------------------------------------------------------------
    // The allocation table
    // -----------------------------------------------------------------------

    /// The sector of the table holding `cluster`'s entry, and the byte offset
    /// within it.
    fn fat_position(&self, cluster: u32) -> (u64, usize) {
        let width = match self.flavour {
            Flavour::Fat16 => 2u64,
            Flavour::Fat32 => 4u64,
        };
        let offset = cluster as u64 * width;
        (self.fat_start + offset / self.bytes_per_sector as u64, (offset % self.bytes_per_sector as u64) as usize)
    }

    /// Load a table sector into the cache, writing back a dirty one first.
    fn fat_sector(&mut self, sector: u64) -> Result<&mut Vec<u8>, Error> {
        if self.fat_cache.as_ref().is_some_and(|(cached, _)| *cached == sector) {
            return Ok(&mut self.fat_cache.as_mut().expect("just checked").1);
        }
        self.flush_fat()?;
        let mut bytes = vec![0u8; SECTOR_SIZE];
        self.disk.read(sector, &mut bytes)?;
        self.fat_cache = Some((sector, bytes));
        Ok(&mut self.fat_cache.as_mut().expect("just stored").1)
    }

    /// Write the cached table sector back to every copy of the table.
    pub fn flush_fat(&mut self) -> Result<(), Error> {
        if !self.fat_dirty {
            return Ok(());
        }
        let Some((sector, bytes)) = self.fat_cache.take() else {
            self.fat_dirty = false;
            return Ok(());
        };
        // Both copies, always. A volume whose second table disagrees with its
        // first is one that `fsck` on another operating system will "repair"
        // by picking one, and which one is not this filesystem's decision to
        // leave to chance.
        for copy in 0..self.fat_copies as u64 {
            self.disk.write(sector + copy * self.fat_sectors as u64, &bytes)?;
        }
        self.fat_cache = Some((sector, bytes));
        self.fat_dirty = false;
        Ok(())
    }

    /// The cluster that follows `cluster` in its chain.
    fn chain_next(&mut self, cluster: u32) -> Result<u32, Error> {
        if cluster < 2 || cluster >= self.clusters + 2 {
            return Err(Error::InvalidArgument);
        }
        let (sector, offset) = self.fat_position(cluster);
        let flavour = self.flavour;
        let bytes = self.fat_sector(sector)?;
        Ok(match flavour {
            Flavour::Fat16 => {
                let value = u16(bytes, offset) as u32;
                // Sign-extend the end-of-chain marker into the 32-bit space
                // the rest of this file works in, so one comparison covers
                // both flavours.
                if value >= 0xFFF8 {
                    CLUSTER_END
                } else {
                    value
                }
            }
            // The top four bits of a FAT32 entry are reserved and must be
            // ignored on read -- a volume that has been through certain
            // formatters has them set.
            Flavour::Fat32 => u32(bytes, offset) & 0x0FFF_FFFF,
        })
    }

    /// Point `cluster`'s chain entry at `value`.
    fn chain_set(&mut self, cluster: u32, value: u32) -> Result<(), Error> {
        if cluster < 2 || cluster >= self.clusters + 2 {
            return Err(Error::InvalidArgument);
        }
        let (sector, offset) = self.fat_position(cluster);
        let flavour = self.flavour;
        let bytes = self.fat_sector(sector)?;
        match flavour {
            Flavour::Fat16 => {
                let value = if value >= CLUSTER_END { 0xFFFF } else { value as u16 };
                bytes[offset..offset + 2].copy_from_slice(&value.to_le_bytes());
            }
            Flavour::Fat32 => {
                let existing = u32(bytes, offset) & 0xF000_0000;
                let value = if value >= CLUSTER_END { 0x0FFF_FFFF } else { value & 0x0FFF_FFFF };
                bytes[offset..offset + 4].copy_from_slice(&(existing | value).to_le_bytes());
            }
        }
        self.fat_dirty = true;
        Ok(())
    }

    /// Take a free cluster, mark it as the end of a chain, and zero it.
    fn allocate(&mut self) -> Result<u32, Error> {
        for cluster in 2..(self.clusters + 2) {
            if self.chain_next(cluster)? == CLUSTER_FREE {
                self.chain_set(cluster, CLUSTER_END)?;
                self.flush_fat()?;
                // A freshly allocated cluster still holds whatever the last
                // file to own it left there. For a directory that is fatal --
                // the stale bytes would read as entries -- and for a file it
                // would be somebody else's data appearing in the middle of
                // yours after a partial write.
                let zeros = vec![0u8; (self.sectors_per_cluster * self.bytes_per_sector) as usize];
                let sector = self.cluster_sector(cluster);
                self.disk.write(sector, &zeros)?;
                return Ok(cluster);
            }
        }
        Err(Error::NoSpace)
    }

    /// Release every cluster in the chain starting at `cluster`.
    fn free_chain(&mut self, mut cluster: u32) -> Result<(), Error> {
        while cluster >= 2 && cluster < self.clusters + 2 {
            let next = self.chain_next(cluster)?;
            self.chain_set(cluster, CLUSTER_FREE)?;
            if next >= CLUSTER_END || next < 2 {
                break;
            }
            cluster = next;
        }
        self.flush_fat()
    }

    /// The first sector of `cluster`.
    fn cluster_sector(&self, cluster: u32) -> u64 {
        self.data_start + (cluster as u64 - 2) * self.sectors_per_cluster as u64
    }

    /// Walk `steps` links along a chain, extending it if asked and needed.
    fn seek_cluster(&mut self, start: u32, steps: u32, extend: bool) -> Result<u32, Error> {
        let mut cluster = start;
        for _ in 0..steps {
            let next = self.chain_next(cluster)?;
            if next >= CLUSTER_END || next < 2 {
                if !extend {
                    return Err(Error::NotFound);
                }
                let fresh = self.allocate()?;
                self.chain_set(cluster, fresh)?;
                self.flush_fat()?;
                cluster = fresh;
            } else {
                cluster = next;
            }
        }
        Ok(cluster)
    }

    // -----------------------------------------------------------------------
    // Directories
    // -----------------------------------------------------------------------

    /// The sectors a directory's entries occupy, in order.
    ///
    /// Materialised as a vector rather than an iterator because walking it
    /// needs `&mut self` to read the allocation table, and an iterator holding
    /// that borrow could not also read a sector.
    fn dir_sectors(&mut self, dir: Dir) -> Result<Vec<u64>, Error> {
        match dir {
            Dir::FixedRoot => {
                Ok((0..self.root_sectors as u64).map(|index| self.root_start + index).collect())
            }
            Dir::Chain(start) => {
                let mut sectors = Vec::new();
                let mut cluster = start;
                // A corrupt chain that loops would otherwise spin here
                // forever; the bound is the whole volume, which no real chain
                // can exceed.
                let mut guard = self.clusters as usize + 2;
                while cluster >= 2 && cluster < self.clusters + 2 && guard > 0 {
                    let base = self.cluster_sector(cluster);
                    for index in 0..self.sectors_per_cluster as u64 {
                        sectors.push(base + index);
                    }
                    let next = self.chain_next(cluster)?;
                    if next >= CLUSTER_END || next < 2 {
                        break;
                    }
                    cluster = next;
                    guard -= 1;
                }
                Ok(sectors)
            }
        }
    }

    /// Every real entry in `dir`, with long names reassembled.
    pub fn list(&mut self, dir: Dir) -> Result<Vec<Listing>, Error> {
        let sectors = self.dir_sectors(dir)?;
        let mut out = Vec::new();
        let mut long: Vec<Option<String>> = Vec::new();

        for sector in sectors {
            let mut bytes = vec![0u8; SECTOR_SIZE];
            self.disk.read(sector, &mut bytes)?;
            for offset in (0..SECTOR_SIZE).step_by(ENTRY_SIZE) {
                let record = &bytes[offset..offset + ENTRY_SIZE];
                match record[0] {
                    ENTRY_END => return Ok(out),
                    ENTRY_DELETED => {
                        long.clear();
                        continue;
                    }
                    _ => {}
                }
                let attr = record[0x0B];
                if attr == ATTR_LONG_NAME {
                    // Long-name fragments come *before* the entry they belong
                    // to and in reverse order, numbered from 1 in the low bits
                    // of the first byte. Collecting them into a sparse vector
                    // indexed by that number reassembles them without trusting
                    // the order they arrive in.
                    let index = (record[0] & 0x1F) as usize;
                    if index >= 1 {
                        if long.len() < index {
                            long.resize(index, None);
                        }
                        long[index - 1] = Some(long_name_part(record));
                    }
                    continue;
                }
                if attr & ATTR_VOLUME_ID != 0 {
                    long.clear();
                    continue;
                }

                let name = if long.iter().all(|part| part.is_some()) && !long.is_empty() {
                    long.iter().flatten().fold(String::new(), |mut all, part| {
                        all.push_str(part);
                        all
                    })
                } else {
                    short_name(record)
                };
                long.clear();
                out.push(Listing { name, attr, size: u32(&bytes, offset + 0x1C) });
            }
        }
        Ok(out)
    }

    /// Find `name` directly inside `dir`.
    fn find(&mut self, dir: Dir, name: &str) -> Result<Entry, Error> {
        let sectors = self.dir_sectors(dir)?;
        let mut long: Vec<Option<String>> = Vec::new();

        for sector in sectors {
            let mut bytes = vec![0u8; SECTOR_SIZE];
            self.disk.read(sector, &mut bytes)?;
            for offset in (0..SECTOR_SIZE).step_by(ENTRY_SIZE) {
                let record = &bytes[offset..offset + ENTRY_SIZE];
                match record[0] {
                    ENTRY_END => return Err(Error::NotFound),
                    ENTRY_DELETED => {
                        long.clear();
                        continue;
                    }
                    _ => {}
                }
                let attr = record[0x0B];
                if attr == ATTR_LONG_NAME {
                    let index = (record[0] & 0x1F) as usize;
                    if index >= 1 {
                        if long.len() < index {
                            long.resize(index, None);
                        }
                        long[index - 1] = Some(long_name_part(record));
                    }
                    continue;
                }
                if attr & ATTR_VOLUME_ID != 0 {
                    long.clear();
                    continue;
                }

                let short = short_name(record);
                let full = if !long.is_empty() && long.iter().all(|part| part.is_some()) {
                    long.iter().flatten().fold(String::new(), |mut all, part| {
                        all.push_str(part);
                        all
                    })
                } else {
                    short.clone()
                };
                long.clear();

                // Case-insensitively, because FAT is: a file created as
                // `Notes.txt` on a host is the same file as `NOTES.TXT` here,
                // and a shell that could not find it would be wrong rather
                // than strict.
                if equal_ignoring_case(&full, name) || equal_ignoring_case(&short, name) {
                    return Ok(Entry {
                        cluster: (u16(&bytes, offset + 0x14) as u32) << 16 | u16(&bytes, offset + 0x1A) as u32,
                        size: u32(&bytes, offset + 0x1C),
                        attr,
                        record_sector: sector,
                        record_offset: offset,
                    });
                }
            }
        }
        Err(Error::NotFound)
    }

    /// Resolve a whole path from the root.
    pub fn lookup(&mut self, path: &str) -> Result<Entry, Error> {
        let mut entry = self.root();
        for component in path.split('/').filter(|part| !part.is_empty() && *part != ".") {
            if !entry.is_dir() {
                return Err(Error::NotFound);
            }
            let dir = entry.as_dir(self.flavour);
            entry = self.find(dir, component)?;
        }
        Ok(entry)
    }

    /// Split a path into its parent directory and its final component.
    fn split_parent(&mut self, path: &str) -> Result<(Entry, String), Error> {
        let trimmed = path.trim_matches('/');
        let (parent, name) = match trimmed.rfind('/') {
            Some(index) => (&trimmed[..index], &trimmed[index + 1..]),
            None => ("", trimmed),
        };
        if name.is_empty() {
            return Err(Error::InvalidArgument);
        }
        let parent = self.lookup(parent)?;
        if !parent.is_dir() {
            return Err(Error::InvalidArgument);
        }
        Ok((parent, String::from(name)))
    }

    // -----------------------------------------------------------------------
    // Files
    // -----------------------------------------------------------------------

    /// Read up to `length` bytes of `entry` from `offset`.
    pub fn read(&mut self, entry: &Entry, offset: u64, length: usize) -> Result<Vec<u8>, Error> {
        if entry.cluster < 2 {
            return Ok(Vec::new());
        }
        let end = if entry.is_dir() { u64::MAX } else { entry.size as u64 };
        if offset >= end {
            return Ok(Vec::new());
        }
        let length = length.min((end - offset) as usize);
        let cluster_bytes = self.cluster_bytes() as u64;

        let mut out = Vec::new();
        out.try_reserve_exact(length).map_err(|_| Error::NoMemory)?;
        let mut position = offset;
        while out.len() < length {
            let cluster = self.seek_cluster(entry.cluster, (position / cluster_bytes) as u32, false)?;
            let within = position % cluster_bytes;
            let sector = self.cluster_sector(cluster) + within / self.bytes_per_sector as u64;
            let sector_offset = (within % self.bytes_per_sector as u64) as usize;

            let mut bytes = vec![0u8; SECTOR_SIZE];
            self.disk.read(sector, &mut bytes)?;
            let take = (SECTOR_SIZE - sector_offset).min(length - out.len());
            out.extend_from_slice(&bytes[sector_offset..sector_offset + take]);
            position += take as u64;
        }
        Ok(out)
    }

    /// Write `data` into `entry` at `offset`, extending it if necessary.
    ///
    /// Returns the number of bytes written, which is all of them or an error:
    /// a short write would leave the caller to work out what happened to a
    /// file whose size has already changed.
    pub fn write(&mut self, entry: &mut Entry, offset: u64, data: &[u8]) -> Result<usize, Error> {
        if entry.is_dir() {
            return Err(Error::InvalidArgument);
        }
        if data.is_empty() {
            return Ok(0);
        }
        // A FAT directory entry stores the file size in 32 bits. An offset or
        // length that would push the end past that is refused rather than
        // silently truncated: `end as u32` used to wrap, so a write at
        // `2^32 * cluster_bytes` would land back at the start of the file and
        // corrupt it.
        let end = offset.checked_add(data.len() as u64).ok_or(Error::InvalidArgument)?;
        if end > u32::MAX as u64 {
            return Err(Error::InvalidArgument);
        }
        let cluster_bytes = self.cluster_bytes() as u64;

        // A file created empty has no first cluster; it gets one on the first
        // byte written, which is also when its directory entry stops saying
        // zero.
        if entry.cluster < 2 {
            entry.cluster = self.allocate()?;
        }

        let mut written = 0usize;
        while written < data.len() {
            let position = offset + written as u64;
            let cluster = self.seek_cluster(entry.cluster, (position / cluster_bytes) as u32, true)?;
            let within = position % cluster_bytes;
            let sector = self.cluster_sector(cluster) + within / self.bytes_per_sector as u64;
            let sector_offset = (within % self.bytes_per_sector as u64) as usize;
            let take = (SECTOR_SIZE - sector_offset).min(data.len() - written);

            // Read-modify-write whenever the write does not cover a whole
            // sector: the disk has no finer granularity, and writing a partial
            // sector without reading it first would zero the rest of it.
            let mut bytes = vec![0u8; SECTOR_SIZE];
            if take != SECTOR_SIZE {
                self.disk.read(sector, &mut bytes)?;
            }
            bytes[sector_offset..sector_offset + take].copy_from_slice(&data[written..written + take]);
            self.disk.write(sector, &bytes)?;
            written += take;
        }

        let end = offset + written as u64;
        if end > entry.size as u64 {
            // The guard above keeps this inside u32.
            entry.size = end as u32;
        }
        self.write_back(entry)?;
        self.flush_fat()?;
        Ok(written)
    }

    /// Set a file's length, releasing any clusters past the new end.
    pub fn truncate(&mut self, entry: &mut Entry, length: u64) -> Result<(), Error> {
        if entry.is_dir() {
            return Err(Error::InvalidArgument);
        }
        // See `write`: a FAT size is 32 bits, and wrapping is corruption.
        if length > u32::MAX as u64 {
            return Err(Error::InvalidArgument);
        }
        if length >= entry.size as u64 {
            if length > entry.size as u64 {
                // Extending is not just a bigger number in the directory
                // entry: the clusters have to exist and the gap has to read as
                // zero, or a subsequent read would fail on a chain that ends
                // early (or return the previous owner's bytes).
                self.zero_extend(entry, length)?;
            } else {
                self.write_back(entry)?;
            }
            return Ok(());
        }
        entry.size = length as u32;
        if length == 0 {
            if entry.cluster >= 2 {
                self.free_chain(entry.cluster)?;
            }
            entry.cluster = 0;
        } else {
            let cluster_bytes = self.cluster_bytes() as u64;
            let last = self.seek_cluster(entry.cluster, ((length - 1) / cluster_bytes) as u32, false)?;
            let tail = self.chain_next(last)?;
            self.chain_set(last, CLUSTER_END)?;
            if tail >= 2 && tail < CLUSTER_END {
                self.free_chain(tail)?;
            }
            self.flush_fat()?;
        }
        self.write_back(entry)
    }

    /// Grow `entry` to `length` bytes by writing zeros over the new range.
    ///
    /// Chunked through [`Fat::write`] rather than buffering the whole range,
    /// which can be up to 4 GiB. `write` allocates and links the clusters and
    /// keeps `entry.size` in step, so no separate chain surgery is needed.
    fn zero_extend(&mut self, entry: &mut Entry, length: u64) -> Result<(), Error> {
        const CHUNK: usize = 16 * 1024;
        let zeros = vec![0u8; CHUNK];
        let mut at = entry.size as u64;
        while at < length {
            let take = (length - at).min(CHUNK as u64) as usize;
            self.write(entry, at, &zeros[..take])?;
            at += take as u64;
        }
        Ok(())
    }

    /// Update the on-disk record of `entry` from its in-memory fields.
    fn write_back(&mut self, entry: &Entry) -> Result<(), Error> {
        if entry.record_sector == 0 {
            // The root directory has no record to update, which is also why it
            // has no size.
            return Ok(());
        }
        let mut bytes = vec![0u8; SECTOR_SIZE];
        self.disk.read(entry.record_sector, &mut bytes)?;
        let at = entry.record_offset;
        bytes[at + 0x14..at + 0x16].copy_from_slice(&(((entry.cluster >> 16) as u16).to_le_bytes()));
        bytes[at + 0x1A..at + 0x1C].copy_from_slice(&((entry.cluster as u16).to_le_bytes()));
        bytes[at + 0x1C..at + 0x20].copy_from_slice(&entry.size.to_le_bytes());
        self.disk.write(entry.record_sector, &bytes)
    }

    // -----------------------------------------------------------------------
    // Creating and removing
    // -----------------------------------------------------------------------

    /// Find a free slot in `dir`, growing the directory if it is a chain.
    fn free_slot(&mut self, dir: Dir) -> Result<(u64, usize), Error> {
        let sectors = self.dir_sectors(dir)?;
        for sector in &sectors {
            let mut bytes = vec![0u8; SECTOR_SIZE];
            self.disk.read(*sector, &mut bytes)?;
            for offset in (0..SECTOR_SIZE).step_by(ENTRY_SIZE) {
                if bytes[offset] == ENTRY_END || bytes[offset] == ENTRY_DELETED {
                    return Ok((*sector, offset));
                }
            }
        }
        // Full. A FAT16 root cannot grow -- its size was fixed when the volume
        // was formatted -- and saying so is better than a confusing NoSpace
        // from somewhere deeper.
        let Dir::Chain(start) = dir else { return Err(Error::NoSpace) };
        let last = sectors.len() as u32 / self.sectors_per_cluster;
        let cluster = self.seek_cluster(start, last, true)?;
        Ok((self.cluster_sector(cluster), 0))
    }

    /// Create a file or directory at `path`.
    pub fn create(&mut self, path: &str, directory: bool) -> Result<Entry, Error> {
        let (parent, name) = self.split_parent(path)?;
        let dir = parent.as_dir(self.flavour);
        if self.find(dir, &name).is_ok() {
            return Err(Error::AlreadyExists);
        }
        let short = short_name_bytes(&name).ok_or(Error::InvalidArgument)?;

        // A directory's cluster is allocated before its record because the
        // record has to name it (and its own `.` entry points back at it). If
        // there is no room for the record after all -- a full FAT16 root, say
        // -- release the cluster again rather than leaking it.
        let cluster = if directory { self.allocate()? } else { 0 };
        let (sector, offset) = match self.free_slot(dir) {
            Ok(slot) => slot,
            Err(error) => {
                if cluster >= 2 {
                    let _ = self.free_chain(cluster);
                }
                return Err(error);
            }
        };
        let mut bytes = vec![0u8; SECTOR_SIZE];
        self.disk.read(sector, &mut bytes)?;
        let record = &mut bytes[offset..offset + ENTRY_SIZE];
        record.fill(0);
        record[..11].copy_from_slice(&short);
        record[0x0B] = if directory { ATTR_DIRECTORY } else { 0x20 };
        record[0x14..0x16].copy_from_slice(&(((cluster >> 16) as u16).to_le_bytes()));
        record[0x1A..0x1C].copy_from_slice(&((cluster as u16).to_le_bytes()));
        self.disk.write(sector, &bytes)?;

        if directory {
            self.write_dot_entries(cluster, &parent)?;
        }

        Ok(Entry {
            cluster,
            size: 0,
            attr: if directory { ATTR_DIRECTORY } else { 0x20 },
            record_sector: sector,
            record_offset: offset,
        })
    }

    /// Write the `.` and `..` entries a new directory must start with.
    ///
    /// `..` pointing at cluster 0 is how FAT spells "the root", on both
    /// flavours, even on FAT32 where the root has a real cluster number.
    fn write_dot_entries(&mut self, cluster: u32, parent: &Entry) -> Result<(), Error> {
        let sector = self.cluster_sector(cluster);
        let mut bytes = vec![0u8; SECTOR_SIZE];
        let parent_cluster = if parent.record_sector == 0 { 0 } else { parent.cluster };

        for (index, (name, target)) in [(b".          ", cluster), (b"..         ", parent_cluster)]
            .iter()
            .enumerate()
        {
            let at = index * ENTRY_SIZE;
            bytes[at..at + 11].copy_from_slice(*name);
            bytes[at + 0x0B] = ATTR_DIRECTORY;
            bytes[at + 0x14..at + 0x16].copy_from_slice(&(((target >> 16) as u16).to_le_bytes()));
            bytes[at + 0x1A..at + 0x1C].copy_from_slice(&((*target as u16).to_le_bytes()));
        }
        self.disk.write(sector, &bytes)
    }

    /// Remove a file, or an empty directory.
    pub fn unlink(&mut self, path: &str) -> Result<(), Error> {
        let entry = self.lookup(path)?;
        if entry.record_sector == 0 {
            return Err(Error::InvalidArgument);
        }
        if entry.attr & ATTR_READ_ONLY != 0 {
            return Err(Error::AccessDenied);
        }
        if entry.is_dir() {
            // Refusing a non-empty directory rather than recursing: a
            // filesystem that deletes a tree on one request is a filesystem
            // where one typo is unrecoverable, and the shell can walk it
            // itself if that is what the user meant.
            let remaining = self
                .list(entry.as_dir(self.flavour))?
                .into_iter()
                .filter(|listing| listing.name != "." && listing.name != "..")
                .count();
            if remaining > 0 {
                return Err(Error::BadState);
            }
        }

        if entry.cluster >= 2 {
            self.free_chain(entry.cluster)?;
        }
        // The record itself is only marked, not erased. That is what FAT does,
        // and it is why the long-name entries in front of it have to be marked
        // too -- an orphaned long name in front of the next file would attach
        // itself to that file's record on the next listing.
        let mut bytes = vec![0u8; SECTOR_SIZE];
        self.disk.read(entry.record_sector, &mut bytes)?;
        bytes[entry.record_offset] = ENTRY_DELETED;
        let mut offset = entry.record_offset;
        while offset >= ENTRY_SIZE {
            offset -= ENTRY_SIZE;
            if bytes[offset + 0x0B] != ATTR_LONG_NAME || bytes[offset] == ENTRY_DELETED {
                break;
            }
            bytes[offset] = ENTRY_DELETED;
        }
        self.disk.write(entry.record_sector, &bytes)?;
        self.flush_fat()
    }
}

// ---------------------------------------------------------------------------
// Names
// ---------------------------------------------------------------------------

/// The five, six and two UTF-16 characters one long-name entry carries.
fn long_name_part(record: &[u8]) -> String {
    let mut out = String::new();
    for range in [1..11usize, 14..26, 28..32] {
        for pair in record[range].chunks_exact(2) {
            let value = u16::from_le_bytes([pair[0], pair[1]]);
            match value {
                0x0000 | 0xFFFF => return out,
                // Anything outside ASCII is replaced rather than decoded: this
                // filesystem has no case-folding tables and no business
                // pretending to handle Unicode names it cannot compare.
                0x0001..=0x007F => out.push(value as u8 as char),
                _ => out.push('?'),
            }
        }
    }
    out
}

/// The 8.3 name in a directory record, as `NAME.EXT`.
fn short_name(record: &[u8]) -> String {
    let base = core::str::from_utf8(&record[..8]).unwrap_or("").trim_end();
    let extension = core::str::from_utf8(&record[8..11]).unwrap_or("").trim_end();
    let mut out = String::from(base);
    if !extension.is_empty() {
        out.push('.');
        out.push_str(extension);
    }
    out
}

/// Pack `name` into the eleven bytes a directory record wants, or refuse.
///
/// Refusing is the honest answer for a name that does not fit: silently
/// truncating `configuration.toml` to `CONFIGUR.TOM` would create a file the
/// caller cannot then open by the name it asked for.
fn short_name_bytes(name: &str) -> Option<[u8; 11]> {
    let mut parts = name.rsplitn(2, '.');
    let (base, extension) = match name.rfind('.') {
        Some(_) => {
            let extension = parts.next()?;
            let base = parts.next()?;
            (base, extension)
        }
        None => (name, ""),
    };
    if base.is_empty() || base.len() > 8 || extension.len() > 3 {
        return None;
    }

    let mut out = [b' '; 11];
    for (index, byte) in base.bytes().enumerate() {
        if !valid_short_char(byte) {
            return None;
        }
        out[index] = byte.to_ascii_uppercase();
    }
    for (index, byte) in extension.bytes().enumerate() {
        if !valid_short_char(byte) {
            return None;
        }
        out[8 + index] = byte.to_ascii_uppercase();
    }
    Some(out)
}

/// Whether a byte may appear in an 8.3 name.
fn valid_short_char(byte: u8) -> bool {
    byte.is_ascii_alphanumeric() || b"$%'-_@~`!(){}^#&".contains(&byte)
}

fn equal_ignoring_case(left: &str, right: &str) -> bool {
    left.len() == right.len() && left.bytes().zip(right.bytes()).all(|(a, b)| a.eq_ignore_ascii_case(&b))
}

fn u16(bytes: &[u8], offset: usize) -> u16 {
    u16::from_le_bytes([bytes[offset], bytes[offset + 1]])
}

fn u32(bytes: &[u8], offset: usize) -> u32 {
    u32::from_le_bytes([bytes[offset], bytes[offset + 1], bytes[offset + 2], bytes[offset + 3]])
}
