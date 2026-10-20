//! Address spaces.
//!
//! An address space is a kernel object in its own right, separate from the
//! process that uses it. Splitting them is what makes it possible to hand a
//! loader or a debugger the authority to map into a process without handing it
//! the authority to run code there.
//!
//! Regions are the unit of bookkeeping: a contiguous range of virtual pages
//! backed by one [`Vmo`], with fixed permissions. Page tables are populated
//! lazily from regions by the fault handler, so mapping a gigabyte costs one
//! `BTreeMap` insert.

use alloc::collections::BTreeMap;
use alloc::sync::Arc;
use alloc::vec::Vec;

use crate::arch::PAGE_SIZE;
use crate::obj::{KernelObject, ObjectHeader, ObjectType, Rights, Signals};
use crate::sync::IrqSpinLock;

use super::layout::{self, Window};
use super::paging::{self, Mapper, PageFlags};
use super::phys;
use super::vmo::{Vmo, VmoKind};

/// How a region may be accessed.
#[derive(Clone, Copy, PartialEq, Eq)]
pub struct MapFlags(pub u32);

impl MapFlags {
    /// Readable.
    pub const READ: Self = Self(1 << 0);
    /// Writable.
    pub const WRITE: Self = Self(1 << 1);
    /// Executable.
    pub const EXECUTE: Self = Self(1 << 2);
    /// Writes create a private copy instead of modifying the backing object.
    pub const COW: Self = Self(1 << 3);
    /// Mapping is shared: writes are visible to every other mapping of the VMO.
    pub const SHARED: Self = Self(1 << 4);
    /// Pages are committed and mapped eagerly rather than on first touch.
    pub const POPULATE: Self = Self(1 << 5);
    /// Uncached, for device memory.
    pub const DEVICE: Self = Self(1 << 6);

    /// Union.
    pub const fn or(self, other: Self) -> Self {
        Self(self.0 | other.0)
    }
    /// Whether every bit of `other` is set.
    pub const fn contains(self, other: Self) -> bool {
        self.0 & other.0 == other.0
    }

    /// The rights a handle must carry to create a mapping with these flags.
    pub const fn required_rights(self) -> Rights {
        let mut rights = Rights::MAP.0;
        if self.0 & Self::READ.0 != 0 {
            rights |= Rights::READ.0;
        }
        // A copy-on-write mapping never writes to the backing object, so it
        // needs only READ on the handle -- this is what lets a read-only
        // executable handle produce a writable private data segment.
        if self.0 & Self::WRITE.0 != 0 && self.0 & Self::COW.0 == 0 {
            rights |= Rights::WRITE.0;
        }
        if self.0 & Self::EXECUTE.0 != 0 {
            rights |= Rights::EXECUTE.0;
        }
        Rights(rights)
    }

    fn to_page_flags(self, user: bool) -> PageFlags {
        let mut flags = PageFlags::PRESENT;
        if user {
            flags = flags.or(PageFlags::USER);
        }
        // A COW mapping is installed read-only regardless of the requested
        // write permission; the fault handler is what upgrades it.
        if self.contains(Self::WRITE) && !self.contains(Self::COW) {
            flags = flags.or(PageFlags::WRITABLE);
        }
        if self.contains(Self::COW) {
            flags = flags.or(PageFlags::COW);
        }
        if !self.contains(Self::EXECUTE) {
            flags = flags.or(PageFlags::NO_EXECUTE);
        }
        if self.contains(Self::DEVICE) {
            flags = flags.or(PageFlags::NO_CACHE).or(PageFlags::WRITE_THROUGH);
        }
        flags
    }
}

impl core::fmt::Debug for MapFlags {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        write!(
            f,
            "{}{}{}{}{}",
            if self.contains(Self::READ) { "r" } else { "-" },
            if self.contains(Self::WRITE) { "w" } else { "-" },
            if self.contains(Self::EXECUTE) { "x" } else { "-" },
            if self.contains(Self::COW) { "c" } else { "-" },
            if self.contains(Self::SHARED) { "s" } else { "-" },
        )
    }
}

/// Errors from address space operations.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum SpaceError {
    /// The requested range overlaps an existing region.
    Overlap,
    /// No free range of the requested size in the requested window.
    NoSpace,
    /// The address is not in the requested window, or not page aligned.
    BadAddress,
    /// Out of physical memory.
    OutOfMemory,
    /// No region covers the address.
    NotMapped,
    /// The operation would widen permissions.
    PermissionEscalation,
}

/// One mapped range.
#[derive(Clone)]
pub struct Region {
    /// First virtual address.
    pub base: u64,
    /// Length in bytes.
    pub len: u64,
    /// Backing object.
    pub vmo: Arc<Vmo>,
    /// Byte offset into `vmo` that `base` corresponds to.
    pub vmo_offset: u64,
    /// Permissions.
    pub flags: MapFlags,
    /// Which layout window this region lives in.
    pub window: Window,
}

impl Region {
    /// One past the last address.
    pub fn end(&self) -> u64 {
        self.base + self.len
    }

    /// Whether `addr` is inside this region.
    pub fn contains(&self, addr: u64) -> bool {
        addr >= self.base && addr < self.end()
    }
}

/// A set of translations plus the regions that describe them.
pub struct AddressSpace {
    header: ObjectHeader,
    root: phys::Pfn,
    regions: IrqSpinLock<BTreeMap<u64, Region>>,
    is_kernel: bool,
}

impl AddressSpace {
    /// Create a user address space, sharing the kernel's upper half.
    pub fn new_user() -> Result<Arc<Self>, SpaceError> {
        let root = paging::alloc_page_table().ok_or(SpaceError::OutOfMemory)?;
        // SAFETY: `root` is a freshly allocated, zeroed PML4 that this object
        // now owns.
        let mapper = unsafe { Mapper::new(root) };
        // Every address space shares the kernel's upper-half PML4 entries, so
        // a kernel mapping change is globally visible without touching each
        // process's tables.
        // SAFETY: both are live PML4s; the kernel half is stable after boot.
        unsafe { mapper.clone_kernel_half(super::kernel_mapper()) };

        Ok(Arc::new(Self {
            header: ObjectHeader::new(ObjectType::AddressSpace, Signals::NONE),
            root,
            regions: IrqSpinLock::new(BTreeMap::new()),
            is_kernel: false,
        }))
    }

    /// The page-table handle for this space.
    pub fn mapper(&self) -> Mapper {
        // SAFETY: `root` is owned by this object for its whole lifetime.
        unsafe { Mapper::new(self.root) }
    }

    /// Value to load into CR3.
    pub fn cr3(&self) -> u64 {
        phys::pfn_to_phys(self.root)
    }

    /// Snapshot of the regions, for diagnostics.
    pub fn regions(&self) -> Vec<Region> {
        self.regions.lock().values().cloned().collect()
    }

    /// Total bytes mapped.
    pub fn mapped_bytes(&self) -> u64 {
        self.regions.lock().values().map(|r| r.len).sum()
    }

    /// Find a free range of `len` bytes inside `window`.
    fn find_free(&self, regions: &BTreeMap<u64, Region>, window: Window, len: u64) -> Option<u64> {
        let (start, end) = window_bounds(window)?;
        // One page of separation between regions acts as a guard: an overrun of
        // one mapping faults rather than silently landing in the next.
        let stride = PAGE_SIZE as u64;
        let mut candidate = start;
        for region in regions.values() {
            if region.end() <= candidate {
                continue;
            }
            if region.base >= candidate + len + stride {
                break;
            }
            candidate = region.end() + stride;
        }
        if candidate + len <= end {
            Some(candidate)
        } else {
            None
        }
    }

    /// Map `len` bytes of `vmo` into `window`, at `hint` if given.
    pub fn map(
        &self,
        vmo: Arc<Vmo>,
        vmo_offset: u64,
        len: u64,
        flags: MapFlags,
        window: Window,
        hint: Option<u64>,
    ) -> Result<u64, SpaceError> {
        let len = align_up(len);
        if len == 0 {
            return Err(SpaceError::BadAddress);
        }
        let mut regions = self.regions.lock();

        let base = match hint {
            Some(addr) => {
                if addr % PAGE_SIZE as u64 != 0 {
                    return Err(SpaceError::BadAddress);
                }
                let (start, end) = window_bounds(window).ok_or(SpaceError::BadAddress)?;
                if addr < start || addr + len > end {
                    return Err(SpaceError::BadAddress);
                }
                if regions.values().any(|r| addr < r.end() && r.base < addr + len) {
                    return Err(SpaceError::Overlap);
                }
                addr
            }
            None => self.find_free(&regions, window, len).ok_or(SpaceError::NoSpace)?,
        };

        let region = Region { base, len, vmo: vmo.clone(), vmo_offset, flags, window };

        if flags.contains(MapFlags::POPULATE) || flags.contains(MapFlags::DEVICE) {
            let mapper = self.mapper();
            let page_flags = flags.to_page_flags(!self.is_kernel);
            let pages = (len / PAGE_SIZE as u64) as usize;
            for i in 0..pages {
                let index = ((vmo_offset / PAGE_SIZE as u64) as usize) + i;
                let Ok(Some(pfn)) = vmo.page(index, true) else {
                    return Err(SpaceError::OutOfMemory);
                };
                // A physical VMO's frames are device MMIO, outside the buddy
                // allocator's managed range entirely (see `Vmo::new_physical`
                // and its `Drop` impl, which likewise never decrefs them) --
                // refcounting them would panic, and there is no pool for them
                // to be freed back into regardless.
                if vmo.kind() != VmoKind::Physical {
                    phys::incref(pfn);
                }
                // SAFETY: this address space is owned by this object; `base+i`
                // was verified free above.
                unsafe {
                    mapper
                        .map(base + (i * PAGE_SIZE) as u64, phys::pfn_to_phys(pfn), page_flags, true)
                        .map_err(|_| SpaceError::OutOfMemory)?;
                }
            }
        }

        regions.insert(base, region);
        Ok(base)
    }

    /// Remove the region starting at `base`.
    pub fn unmap(&self, base: u64) -> Result<(), SpaceError> {
        let mut regions = self.regions.lock();
        let region = regions.remove(&base).ok_or(SpaceError::NotMapped)?;
        drop(regions);
        self.tear_down_region(&region);
        Ok(())
    }

    /// Remove every mapping, releasing the memory objects behind them.
    ///
    /// A terminated process's address space can outlive it for as long as
    /// anything holds the process object; its memory should not. The page
    /// tables themselves stay until the space is dropped.
    pub fn release_regions(&self) {
        let regions = core::mem::take(&mut *self.regions.lock());
        for region in regions.values() {
            self.tear_down_region(region);
        }
    }

    fn tear_down_region(&self, region: &Region) {
        let mapper = self.mapper();
        let pages = (region.len / PAGE_SIZE as u64) as usize;
        // See the matching comment in `map`: a physical VMO's frames are not
        // in the refcounted pool, so unmapping one must not decref it either.
        let tracked = region.vmo.kind() != VmoKind::Physical;
        for i in 0..pages {
            let virt = region.base + (i * PAGE_SIZE) as u64;
            // SAFETY: this space owns these mappings and is removing them.
            if let Some(entry) = unsafe { mapper.unmap(virt) } {
                if tracked && entry.is_present() {
                    phys::decref(entry.pfn());
                }
            }
        }
    }

    /// Change the permissions of an existing region.
    ///
    /// Permissions can only be narrowed relative to what the region was created
    /// with; widening would let a process launder a read-only handle into a
    /// writable mapping.
    pub fn protect(&self, base: u64, flags: MapFlags) -> Result<(), SpaceError> {
        let mut regions = self.regions.lock();
        let region = regions.get_mut(&base).ok_or(SpaceError::NotMapped)?;
        // A COW write is weaker authority than a direct write: it needs only
        // READ on the backing object because the fault handler makes a private
        // copy. Dropping COW while keeping WRITE would therefore turn a
        // read-only handle into a mapping that writes the shared object, which
        // is exactly the laundering the escalation check exists to stop.
        let old_direct_write = region.flags.contains(MapFlags::WRITE) && !region.flags.contains(MapFlags::COW);
        let new_direct_write = flags.contains(MapFlags::WRITE) && !flags.contains(MapFlags::COW);
        if (new_direct_write && !old_direct_write)
            || (flags.contains(MapFlags::EXECUTE) && !region.flags.contains(MapFlags::EXECUTE))
        {
            return Err(SpaceError::PermissionEscalation);
        }
        region.flags = flags;
        let region = region.clone();
        drop(regions);

        let mapper = self.mapper();
        let page_flags = flags.to_page_flags(!self.is_kernel);
        let pages = (region.len / PAGE_SIZE as u64) as usize;
        for i in 0..pages {
            let virt = region.base + (i * PAGE_SIZE) as u64;
            // SAFETY: this space owns the mapping; absent pages are skipped.
            let _ = unsafe { mapper.protect(virt, page_flags) };
        }
        Ok(())
    }

    /// The region covering `addr`, if any.
    pub fn region_at(&self, addr: u64) -> Option<Region> {
        let regions = self.regions.lock();
        regions.range(..=addr).next_back().filter(|(_, r)| r.contains(addr)).map(|(_, r)| r.clone())
    }

    /// Resolve a fault at `addr`.
    ///
    /// Returns `true` if a mapping was installed and the faulting instruction
    /// should be retried.
    pub fn handle_fault(&self, addr: u64, write: bool, instruction_fetch: bool) -> bool {
        let Some(region) = self.region_at(addr) else { return false };

        if instruction_fetch && !region.flags.contains(MapFlags::EXECUTE) {
            return false;
        }
        if write && !region.flags.contains(MapFlags::WRITE) {
            return false;
        }

        let page_addr = addr & !(PAGE_SIZE as u64 - 1);
        let offset = page_addr - region.base + region.vmo_offset;
        let index = (offset / PAGE_SIZE as u64) as usize;
        let mapper = self.mapper();

        // Copy-on-write: the page is present and read-only, and the write must
        // produce a private copy.
        if write && region.flags.contains(MapFlags::COW) {
            if let Some(entry) = mapper.leaf_entry(page_addr) {
                if entry.is_present() {
                    let old = entry.pfn();
                    // If we hold the only reference, the copy is unnecessary --
                    // just make it writable.
                    let flags = region.flags.to_page_flags(!self.is_kernel).or(PageFlags::WRITABLE);
                    if phys::refcount(old) == 1 {
                        // SAFETY: this space owns the mapping.
                        let _ = unsafe { mapper.protect(page_addr, flags.without(PageFlags::COW)) };
                        return true;
                    }
                    let Some(new) = phys::alloc() else { return false };
                    // SAFETY: `new` is exclusively owned; `old` is a live frame.
                    unsafe { super::copy_frame(new, old) };
                    phys::set_flags(new, phys::FrameFlags::VMO);
                    // SAFETY: replacing our own mapping with a private copy.
                    unsafe {
                        if mapper
                            .map(page_addr, phys::pfn_to_phys(new), flags.without(PageFlags::COW), true)
                            .is_err()
                        {
                            phys::free(new);
                            return false;
                        }
                    }
                    phys::decref(old);
                    return true;
                }
            }
        }

        // Demand paging: commit the backing page and install it.
        let Ok(Some(pfn)) = region.vmo.page(index, true) else { return false };
        // See the matching comment in `map`: a physical VMO's frames are not
        // in the refcounted pool at all.
        let tracked = region.vmo.kind() != VmoKind::Physical;
        if tracked {
            phys::incref(pfn);
        }
        let page_flags = region.flags.to_page_flags(!self.is_kernel);
        // SAFETY: this address space is owned by this object, and the region
        // covering `page_addr` authorises exactly these permissions.
        match unsafe { mapper.map(page_addr, phys::pfn_to_phys(pfn), page_flags, true) } {
            Ok(()) => true,
            Err(_) => {
                if tracked {
                    phys::decref(pfn);
                }
                false
            }
        }
    }

    /// Reserve a range so nothing else can be mapped there (used for guard
    /// regions around stacks).
    pub fn reserve(&self, base: u64, len: u64, window: Window) -> Result<(), SpaceError> {
        let vmo = Vmo::new_anonymous(0, false).map_err(|_| SpaceError::OutOfMemory)?;
        let mut regions = self.regions.lock();
        if regions.values().any(|r| base < r.end() && r.base < base + len) {
            return Err(SpaceError::Overlap);
        }
        regions.insert(
            base,
            Region { base, len, vmo, vmo_offset: 0, flags: MapFlags(0), window },
        );
        Ok(())
    }
}

impl KernelObject for AddressSpace {
    fn header(&self) -> &ObjectHeader {
        &self.header
    }

    fn as_any(&self) -> &dyn core::any::Any {
        self
    }
}

impl Drop for AddressSpace {
    fn drop(&mut self) {
        let regions: Vec<Region> = self.regions.get_mut().values().cloned().collect();
        for region in &regions {
            self.tear_down_region(region);
        }
        // Free the user half of the page-table tree. The kernel half is shared
        // and must not be touched.
        // SAFETY: this space is being destroyed and nothing references it.
        unsafe { free_user_tables(self.root) };
    }
}

/// Recursively free the lower-half page tables of `root`.
///
/// # Safety
/// `root` must be a PML4 that no CPU has loaded and no other object references.
unsafe fn free_user_tables(root: phys::Pfn) {
    // SAFETY: caller guarantees exclusive ownership of the tree.
    unsafe {
        free_level(root, 3);
    }

    /// # Safety
    /// `pfn` must hold a live page table at `level`, exclusively owned.
    unsafe fn free_level(pfn: phys::Pfn, level: usize) {
        let table = phys::phys_to_virt(phys::pfn_to_phys(pfn)) as *const paging::PageTable;
        // The kernel half is entries 256..512 of the PML4; never free those.
        let limit = if level == 3 { 256 } else { 512 };
        for i in 0..limit {
            // SAFETY: caller guarantees `pfn` holds a live table.
            let entry = unsafe { (*table).entries[i] };
            if !entry.is_present() || entry.is_huge() {
                continue;
            }
            if level > 0 {
                // SAFETY: a present, non-huge entry points at a lower table.
                unsafe { free_level(entry.pfn(), level - 1) };
            }
        }
        // Every table in the tree is owned by this address space, including the
        // level-0 page tables (and the PML4 itself) -- the `level > 0` guard
        // that used to be here leaked one page-table frame per touched 2 MiB
        // region on every process teardown.
        // SAFETY: every child has been freed; this table is now garbage.
        unsafe { phys::free(pfn) };
    }
}

/// Address bounds of a layout window.
pub fn window_bounds(window: Window) -> Option<(u64, u64)> {
    Some(match window {
        Window::Program => (layout::PROGRAM_BASE, layout::PROGRAM_END),
        Window::Heap => (layout::HEAP_BASE, layout::HEAP_END),
        Window::Stack => (layout::STACK_BASE, layout::STACK_END),
        Window::Shared => (layout::SHARED_BASE, layout::SHARED_END),
        Window::Ipc => (layout::IPC_BASE, layout::IPC_END),
        Window::MappedFile => (layout::MMAP_FILE_BASE, layout::MMAP_FILE_END),
        Window::Device => (layout::DEVICE_BASE, layout::DEVICE_END),
        Window::Runtime => (layout::RUNTIME_BASE, layout::RUNTIME_END),
        Window::IoRemap => (layout::IOREMAP_BASE, layout::IOREMAP_END),
        Window::KernelArena => (layout::KERNEL_ARENA_BASE, layout::KERNEL_ARENA_END),
        _ => return None,
    })
}

fn align_up(size: u64) -> u64 {
    (size + PAGE_SIZE as u64 - 1) & !(PAGE_SIZE as u64 - 1)
}
