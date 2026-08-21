//! 4-level (and 5-level aware) x86-64 paging.
//!
//! Page tables are always reached through the direct physical map, never
//! through a recursive mapping. That costs one addition per level and buys
//! two things NodeOS cares about: a kernel that can edit *any* address space
//! (not just the active one) without switching CR3, and no reserved slot in
//! every PML4.

use core::sync::atomic::Ordering;

use crate::arch::x86_64::cpu;
use crate::arch::{LARGE_PAGE_SIZE, PAGE_SIZE};

use super::phys::{self, Pfn};

/// Hardware page-table entry flag bits.
#[derive(Clone, Copy, PartialEq, Eq)]
pub struct PageFlags(pub u64);

impl PageFlags {
    /// Entry is valid.
    pub const PRESENT: Self = Self(1 << 0);
    /// Writes are permitted.
    pub const WRITABLE: Self = Self(1 << 1);
    /// Ring 3 may access the page.
    pub const USER: Self = Self(1 << 2);
    /// Write-through caching.
    pub const WRITE_THROUGH: Self = Self(1 << 3);
    /// Caching disabled (device memory).
    pub const NO_CACHE: Self = Self(1 << 4);
    /// Set by hardware on any access.
    pub const ACCESSED: Self = Self(1 << 5);
    /// Set by hardware on a write.
    pub const DIRTY: Self = Self(1 << 6);
    /// This entry maps a large/huge page rather than a lower table.
    pub const HUGE: Self = Self(1 << 7);
    /// Translation survives a CR3 reload.
    pub const GLOBAL: Self = Self(1 << 8);
    /// Software bit: the mapping is copy-on-write.
    pub const COW: Self = Self(1 << 9);
    /// Software bit: the page is pinned (no reclaim, no migration).
    pub const PINNED: Self = Self(1 << 10);
    /// Software bit: the page is a guard and must never be made present.
    pub const GUARD: Self = Self(1 << 11);
    /// Instruction fetches fault.
    pub const NO_EXECUTE: Self = Self(1 << 63);

    /// Empty flag set.
    pub const NONE: Self = Self(0);

    /// Union of two flag sets.
    pub const fn or(self, other: Self) -> Self {
        Self(self.0 | other.0)
    }

    /// Whether every bit of `other` is set.
    pub const fn contains(self, other: Self) -> bool {
        self.0 & other.0 == other.0
    }

    /// Remove `other`'s bits.
    pub const fn without(self, other: Self) -> Self {
        Self(self.0 & !other.0)
    }

    /// Typical kernel data mapping: writable, never executable.
    pub const fn kernel_data() -> Self {
        Self(Self::PRESENT.0 | Self::WRITABLE.0 | Self::GLOBAL.0 | Self::NO_EXECUTE.0)
    }

    /// Typical kernel code mapping: read-only and executable.
    pub const fn kernel_code() -> Self {
        Self(Self::PRESENT.0 | Self::GLOBAL.0)
    }

    /// Typical kernel read-only data mapping.
    pub const fn kernel_rodata() -> Self {
        Self(Self::PRESENT.0 | Self::GLOBAL.0 | Self::NO_EXECUTE.0)
    }

    /// Device memory: writable, uncached, never executable.
    pub const fn device() -> Self {
        Self(Self::PRESENT.0 | Self::WRITABLE.0 | Self::NO_CACHE.0 | Self::WRITE_THROUGH.0 | Self::NO_EXECUTE.0)
    }
}

impl core::ops::BitOr for PageFlags {
    type Output = Self;
    fn bitor(self, rhs: Self) -> Self {
        self.or(rhs)
    }
}

impl core::fmt::Debug for PageFlags {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        let names = [
            (Self::PRESENT, "P"),
            (Self::WRITABLE, "W"),
            (Self::USER, "U"),
            (Self::NO_CACHE, "UC"),
            (Self::HUGE, "PS"),
            (Self::GLOBAL, "G"),
            (Self::COW, "COW"),
            (Self::GUARD, "GUARD"),
            (Self::NO_EXECUTE, "NX"),
        ];
        let mut first = true;
        for (flag, name) in names {
            if self.contains(flag) {
                if !first {
                    f.write_str("|")?;
                }
                f.write_str(name)?;
                first = false;
            }
        }
        if first {
            f.write_str("-")?;
        }
        Ok(())
    }
}

/// Mask selecting the physical frame address out of a page-table entry.
const ADDR_MASK: u64 = 0x000F_FFFF_FFFF_F000;

/// One page-table entry.
#[repr(transparent)]
#[derive(Clone, Copy, Default)]
pub struct Entry(pub u64);

impl Entry {
    /// An absent entry.
    pub const EMPTY: Self = Self(0);

    /// Whether the entry is present.
    #[inline]
    pub const fn is_present(self) -> bool {
        self.0 & PageFlags::PRESENT.0 != 0
    }

    /// Whether the entry maps a large/huge page.
    #[inline]
    pub const fn is_huge(self) -> bool {
        self.0 & PageFlags::HUGE.0 != 0
    }

    /// Physical address the entry points at.
    #[inline]
    pub const fn addr(self) -> u64 {
        self.0 & ADDR_MASK
    }

    /// Frame number the entry points at.
    #[inline]
    pub const fn pfn(self) -> Pfn {
        (self.addr() >> 12) as Pfn
    }

    /// Flag bits.
    #[inline]
    pub const fn flags(self) -> PageFlags {
        PageFlags(self.0 & !ADDR_MASK)
    }

    /// Build an entry from a physical address and flags.
    #[inline]
    pub const fn new(phys: u64, flags: PageFlags) -> Self {
        Self((phys & ADDR_MASK) | flags.0)
    }
}

/// A 4 KiB page table: 512 entries.
#[repr(C, align(4096))]
pub struct PageTable {
    /// The entries. Public so self-tests and the mapper can walk them.
    pub entries: [Entry; 512],
}

impl PageTable {
    /// Index into this level for `virt`.
    #[inline]
    pub const fn index(virt: u64, level: usize) -> usize {
        ((virt >> (12 + 9 * level)) & 0x1FF) as usize
    }
}

/// Errors from mapping operations.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum MapError {
    /// Out of physical memory while allocating an intermediate table.
    OutOfMemory,
    /// The virtual address already has a mapping and `overwrite` was not set.
    AlreadyMapped,
    /// The walk hit a large page where a table was expected.
    HugePageInTheWay,
    /// The address is not canonical, or not page aligned.
    BadAddress,
}

/// A handle to one address space's translation tables.
///
/// This is a *borrowed* view: dropping it does not free anything. Ownership
/// lives in [`super::space::AddressSpace`].
#[derive(Clone, Copy)]
pub struct Mapper {
    root: Pfn,
}

impl Mapper {
    /// Wrap the PML4 at `root`.
    ///
    /// # Safety
    /// `root` must be a live PML4 owned by the caller for the duration of the
    /// returned handle.
    pub const unsafe fn new(root: Pfn) -> Self {
        Self { root }
    }

    /// The root frame number.
    #[inline]
    pub const fn root(&self) -> Pfn {
        self.root
    }

    /// Value to load into CR3 for this address space.
    #[inline]
    pub const fn cr3(&self) -> u64 {
        phys::pfn_to_phys(self.root)
    }

    /// Borrow a table by frame number.
    ///
    /// # Safety
    /// `pfn` must hold a live page table.
    #[inline]
    unsafe fn table(pfn: Pfn) -> *mut PageTable {
        phys::phys_to_virt(phys::pfn_to_phys(pfn)) as *mut PageTable
    }

    /// Translate `virt`, returning the physical address and the effective flags.
    pub fn translate(&self, virt: u64) -> Option<(u64, PageFlags)> {
        let mut pfn = self.root;
        for level in (0..4).rev() {
            // SAFETY: `pfn` is either the root (a live PML4) or came from a
            // present, non-huge entry, so it holds a live page table.
            let table = unsafe { &*Self::table(pfn) };
            let entry = table.entries[PageTable::index(virt, level)];
            if !entry.is_present() {
                return None;
            }
            if entry.is_huge() || level == 0 {
                let page_size = 1u64 << (12 + 9 * level);
                let offset = virt & (page_size - 1);
                return Some((entry.addr() + offset, entry.flags()));
            }
            pfn = entry.pfn();
        }
        None
    }

    /// Look up the leaf entry for `virt` without following huge pages.
    pub fn leaf_entry(&self, virt: u64) -> Option<Entry> {
        let mut pfn = self.root;
        for level in (0..4).rev() {
            // SAFETY: see `translate`.
            let table = unsafe { &*Self::table(pfn) };
            let entry = table.entries[PageTable::index(virt, level)];
            if !entry.is_present() && level != 0 {
                return None;
            }
            if level == 0 {
                return Some(entry);
            }
            if entry.is_huge() {
                return Some(entry);
            }
            pfn = entry.pfn();
        }
        None
    }

    /// Walk to the level-0 table for `virt`, creating intermediate tables.
    ///
    /// `user` controls whether the intermediate entries carry the USER bit;
    /// the hardware ANDs permissions down the walk, so a kernel-only
    /// intermediate entry makes the whole subtree kernel-only.
    ///
    /// # Safety
    /// The caller must own this address space.
    unsafe fn walk_create(&self, virt: u64, user: bool) -> Result<*mut Entry, MapError> {
        let mut pfn = self.root;
        for level in (1..4).rev() {
            // SAFETY: `pfn` holds a live page table (root, or a table we
            // created or followed).
            let table = unsafe { &mut *Self::table(pfn) };
            let index = PageTable::index(virt, level);
            let entry = table.entries[index];
            if entry.is_present() {
                if entry.is_huge() {
                    return Err(MapError::HugePageInTheWay);
                }
                // Widen permissions on the path if this subtree is now shared
                // with userspace; narrowing never happens here.
                if user && !entry.flags().contains(PageFlags::USER) {
                    table.entries[index] = Entry(entry.0 | PageFlags::USER.0);
                }
                pfn = entry.pfn();
            } else {
                let new = phys::alloc_zeroed().ok_or(MapError::OutOfMemory)?;
                phys::set_flags(new, phys::FrameFlags::PAGE_TABLE);
                let mut flags = PageFlags::PRESENT.or(PageFlags::WRITABLE);
                if user {
                    flags = flags.or(PageFlags::USER);
                }
                table.entries[index] = Entry::new(phys::pfn_to_phys(new), flags);
                pfn = new;
            }
        }
        // SAFETY: `pfn` now holds the level-0 table.
        let table = unsafe { &mut *Self::table(pfn) };
        Ok(&mut table.entries[PageTable::index(virt, 0)])
    }

    /// Map one 4 KiB page.
    ///
    /// # Safety
    /// The caller must own this address space and guarantee that installing
    /// this translation does not violate any other subsystem's invariants
    /// (aliasing a frame writably into two address spaces, for instance).
    pub unsafe fn map(&self, virt: u64, phys_addr: u64, flags: PageFlags, overwrite: bool) -> Result<(), MapError> {
        if virt % PAGE_SIZE as u64 != 0 || !super::layout::is_canonical(virt) {
            return Err(MapError::BadAddress);
        }
        // SAFETY: caller owns the address space.
        let slot = unsafe { self.walk_create(virt, flags.contains(PageFlags::USER))? };
        // SAFETY: `slot` points into a live level-0 table.
        let existing = unsafe { *slot };
        if existing.is_present() && !overwrite {
            return Err(MapError::AlreadyMapped);
        }
        // SAFETY: as above.
        unsafe { *slot = Entry::new(phys_addr, flags.or(PageFlags::PRESENT)) };
        if existing.is_present() {
            flush(virt);
        }
        Ok(())
    }

    /// Map one 2 MiB large page.
    ///
    /// # Safety
    /// See [`Mapper::map`]; additionally `virt` and `phys_addr` must be 2 MiB
    /// aligned.
    pub unsafe fn map_large(&self, virt: u64, phys_addr: u64, flags: PageFlags, overwrite: bool) -> Result<(), MapError> {
        if virt % LARGE_PAGE_SIZE as u64 != 0 || phys_addr % LARGE_PAGE_SIZE as u64 != 0 {
            return Err(MapError::BadAddress);
        }
        let user = flags.contains(PageFlags::USER);
        let mut pfn = self.root;
        for level in (2..4).rev() {
            // SAFETY: `pfn` holds a live page table.
            let table = unsafe { &mut *Self::table(pfn) };
            let index = PageTable::index(virt, level);
            let entry = table.entries[index];
            if entry.is_present() {
                if entry.is_huge() {
                    return Err(MapError::HugePageInTheWay);
                }
                pfn = entry.pfn();
            } else {
                let new = phys::alloc_zeroed().ok_or(MapError::OutOfMemory)?;
                phys::set_flags(new, phys::FrameFlags::PAGE_TABLE);
                let mut table_flags = PageFlags::PRESENT.or(PageFlags::WRITABLE);
                if user {
                    table_flags = table_flags.or(PageFlags::USER);
                }
                table.entries[index] = Entry::new(phys::pfn_to_phys(new), table_flags);
                pfn = new;
            }
        }
        // SAFETY: `pfn` holds the page directory.
        let table = unsafe { &mut *Self::table(pfn) };
        let index = PageTable::index(virt, 1);
        if table.entries[index].is_present() && !overwrite {
            return Err(MapError::AlreadyMapped);
        }
        table.entries[index] = Entry::new(phys_addr, flags.or(PageFlags::PRESENT).or(PageFlags::HUGE));
        Ok(())
    }

    /// Remove a mapping, returning the entry that was there.
    ///
    /// # Safety
    /// The caller must own this address space and must handle the frame's
    /// reference count.
    pub unsafe fn unmap(&self, virt: u64) -> Option<Entry> {
        let mut pfn = self.root;
        for level in (1..4).rev() {
            // SAFETY: `pfn` holds a live page table.
            let table = unsafe { &*Self::table(pfn) };
            let entry = table.entries[PageTable::index(virt, level)];
            if !entry.is_present() || entry.is_huge() {
                return None;
            }
            pfn = entry.pfn();
        }
        // SAFETY: `pfn` holds the level-0 table.
        let table = unsafe { &mut *Self::table(pfn) };
        let index = PageTable::index(virt, 0);
        let old = table.entries[index];
        if !old.is_present() {
            return None;
        }
        table.entries[index] = Entry::EMPTY;
        flush(virt);
        Some(old)
    }

    /// Change the flags of an existing mapping.
    ///
    /// # Safety
    /// The caller must own this address space.
    pub unsafe fn protect(&self, virt: u64, flags: PageFlags) -> Result<(), MapError> {
        let mut pfn = self.root;
        for level in (1..4).rev() {
            // SAFETY: `pfn` holds a live page table.
            let table = unsafe { &*Self::table(pfn) };
            let entry = table.entries[PageTable::index(virt, level)];
            if !entry.is_present() {
                return Err(MapError::BadAddress);
            }
            if entry.is_huge() {
                return Err(MapError::HugePageInTheWay);
            }
            pfn = entry.pfn();
        }
        // SAFETY: `pfn` holds the level-0 table.
        let table = unsafe { &mut *Self::table(pfn) };
        let index = PageTable::index(virt, 0);
        let old = table.entries[index];
        if !old.is_present() {
            return Err(MapError::BadAddress);
        }
        table.entries[index] = Entry::new(old.addr(), flags.or(PageFlags::PRESENT));
        flush(virt);
        Ok(())
    }

    /// Copy the kernel half (PML4 entries 256..512) from `source`.
    ///
    /// Every address space shares the kernel's upper-half tables, so a change
    /// to the kernel mapping is visible everywhere without a shootdown of the
    /// page tables themselves.
    ///
    /// # Safety
    /// Both tables must be live PML4s owned by the caller.
    pub unsafe fn clone_kernel_half(&self, source: &Mapper) {
        // SAFETY: both frames hold live PML4s.
        let dst = unsafe { &mut *Self::table(self.root) };
        // SAFETY: as above.
        let src = unsafe { &*Self::table(source.root) };
        dst.entries[256..512].copy_from_slice(&src.entries[256..512]);
    }
}

/// Invalidate one TLB entry on the calling CPU.
#[inline]
pub fn flush(virt: u64) {
    // SAFETY: `invlpg` only affects the TLB and cannot violate memory safety.
    unsafe { cpu::invlpg(virt) };
}

/// Flush the entire non-global TLB on the calling CPU.
pub fn flush_all() {
    // SAFETY: reloading CR3 with its current value is always architecturally
    // valid and only discards cached translations.
    unsafe { cpu::write_cr3(cpu::read_cr3()) };
}

/// Allocate a fresh, zeroed PML4.
pub fn alloc_page_table() -> Option<Pfn> {
    let pfn = phys::alloc_zeroed()?;
    phys::set_flags(pfn, phys::FrameFlags::PAGE_TABLE);
    Some(pfn)
}

/// Whether the CPU supports 1 GiB pages.
pub fn supports_gigabyte_pages() -> bool {
    if cpu::cpuid_max_extended_leaf() < 0x8000_0001 {
        return false;
    }
    let (_, _, _, edx) = cpu::cpuid(0x8000_0001, 0);
    edx & (1 << 26) != 0
}

/// Number of frames currently used by page tables (diagnostics).
pub fn page_table_frames() -> u64 {
    PAGE_TABLE_FRAMES.load(Ordering::Relaxed)
}

pub(crate) static PAGE_TABLE_FRAMES: core::sync::atomic::AtomicU64 = core::sync::atomic::AtomicU64::new(0);
