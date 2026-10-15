//! Physical frame allocation.
//!
//! # Structure
//!
//! Two layers:
//!
//! * A **frame database**: one 16-byte [`FrameInfo`] per physical page, indexed
//!   by page frame number. It holds the reference count (which is what makes
//!   copy-on-write and shared memory objects possible), the buddy order, and
//!   the intrusive free-list links.
//! * A **buddy allocator** over that database, with orders 0..[`MAX_ORDER`]
//!   (4 KiB to 4 MiB). Buddy rather than a plain bitmap because NodeOS needs
//!   physically contiguous runs for page tables, DMA buffers granted to
//!   userspace drivers, and large-page mappings.
//!
//! The database itself is carved out of physical memory by a one-shot bump
//! allocator during [`init`], before the buddy allocator exists.

use core::sync::atomic::{AtomicU32, AtomicU64, AtomicU8, Ordering};

use crate::arch::PAGE_SIZE;
use crate::boot::{BootInfo, RegionKind};
use crate::sync::IrqSpinLock;

use super::layout::PHYSMAP_BASE;

/// Page frame number: a physical address divided by [`PAGE_SIZE`].
pub type Pfn = u32;

/// Sentinel for "no frame" in the intrusive free lists.
const NO_FRAME: Pfn = Pfn::MAX;

/// Largest buddy order. Order `n` is `2^n` contiguous pages, so order 10 is
/// 4 MiB.
pub const MAX_ORDER: usize = 11;

/// Per-frame bookkeeping.
///
/// Deliberately 16 bytes: at 4 KiB pages that is 0.39% of RAM, and it keeps
/// two frames per cache line so buddy merges touch few lines.
#[repr(C)]
pub struct FrameInfo {
    /// Number of live references. Zero means the frame is free or reserved.
    refcount: AtomicU32,
    /// Free-list successor, or [`NO_FRAME`].
    next: AtomicU32,
    /// Free-list predecessor, or [`NO_FRAME`].
    prev: AtomicU32,
    /// Buddy order, meaningful only when [`FrameFlags::FREE`] is set.
    order: AtomicU8,
    /// See [`FrameFlags`].
    flags: AtomicU8,
    _pad: [u8; 2],
}

/// Bits in [`FrameInfo::flags`].
pub struct FrameFlags;

impl FrameFlags {
    /// Frame is on a buddy free list.
    pub const FREE: u8 = 1 << 0;
    /// Frame is not usable RAM (firmware reserved, MMIO hole, kernel image).
    pub const RESERVED: u8 = 1 << 1;
    /// Frame holds a page table.
    pub const PAGE_TABLE: u8 = 1 << 2;
    /// Frame belongs to a memory object and may be shared copy-on-write.
    pub const VMO: u8 = 1 << 3;
    /// Frame is pinned and must not be migrated or reclaimed.
    pub const PINNED: u8 = 1 << 4;
}

impl FrameInfo {
    const fn new() -> Self {
        Self {
            refcount: AtomicU32::new(0),
            next: AtomicU32::new(NO_FRAME),
            prev: AtomicU32::new(NO_FRAME),
            order: AtomicU8::new(0),
            flags: AtomicU8::new(FrameFlags::RESERVED),
            _pad: [0; 2],
        }
    }
}

struct Buddy {
    /// Head of each order's free list, or [`NO_FRAME`].
    free_lists: [Pfn; MAX_ORDER],
    /// Base of the frame database in the direct physical map.
    frames: *mut FrameInfo,
    /// One past the highest tracked frame number.
    max_pfn: Pfn,
    /// Frames currently on a free list.
    free_pages: u64,
    /// Frames that are usable RAM at all.
    managed_pages: u64,
}

// SAFETY: all access goes through `ALLOCATOR`, an `IrqSpinLock`.
unsafe impl Send for Buddy {}

impl Buddy {
    const fn empty() -> Self {
        Self {
            free_lists: [NO_FRAME; MAX_ORDER],
            frames: core::ptr::null_mut(),
            max_pfn: 0,
            free_pages: 0,
            managed_pages: 0,
        }
    }

    /// # Safety
    /// `pfn` must be less than `max_pfn`.
    ///
    /// Returns a `'static` reference because the frame database is allocated
    /// once at boot and never moves; tying it to `&self` would make every
    /// free-list update a borrow conflict.
    unsafe fn frame(&self, pfn: Pfn) -> &'static FrameInfo {
        debug_assert!(pfn < self.max_pfn);
        // SAFETY: the database covers every pfn below `max_pfn` and lives for
        // the lifetime of the kernel.
        unsafe { &*self.frames.add(pfn as usize) }
    }

    fn list_push(&mut self, pfn: Pfn, order: usize) {
        // SAFETY: callers only pass tracked frames.
        let frame = unsafe { self.frame(pfn) };
        let head = self.free_lists[order];
        frame.next.store(head, Ordering::Relaxed);
        frame.prev.store(NO_FRAME, Ordering::Relaxed);
        frame.order.store(order as u8, Ordering::Relaxed);
        frame.flags.fetch_or(FrameFlags::FREE, Ordering::Relaxed);
        if head != NO_FRAME {
            // SAFETY: list heads are always tracked frames.
            unsafe { self.frame(head) }.prev.store(pfn, Ordering::Relaxed);
        }
        self.free_lists[order] = pfn;
    }

    fn list_remove(&mut self, pfn: Pfn, order: usize) {
        // SAFETY: callers only pass tracked frames.
        let frame = unsafe { self.frame(pfn) };
        let next = frame.next.load(Ordering::Relaxed);
        let prev = frame.prev.load(Ordering::Relaxed);
        if prev == NO_FRAME {
            self.free_lists[order] = next;
        } else {
            // SAFETY: `prev` came from a live list link.
            unsafe { self.frame(prev) }.next.store(next, Ordering::Relaxed);
        }
        if next != NO_FRAME {
            // SAFETY: `next` came from a live list link.
            unsafe { self.frame(next) }.prev.store(prev, Ordering::Relaxed);
        }
        frame.next.store(NO_FRAME, Ordering::Relaxed);
        frame.prev.store(NO_FRAME, Ordering::Relaxed);
        frame.flags.fetch_and(!FrameFlags::FREE, Ordering::Relaxed);
    }

    fn is_free_block(&self, pfn: Pfn, order: usize) -> bool {
        if pfn >= self.max_pfn {
            return false;
        }
        // SAFETY: bounds checked immediately above.
        let frame = unsafe { self.frame(pfn) };
        let flags = frame.flags.load(Ordering::Relaxed);
        flags & FrameFlags::FREE != 0
            && flags & FrameFlags::RESERVED == 0
            && frame.order.load(Ordering::Relaxed) as usize == order
    }

    fn alloc(&mut self, order: usize) -> Option<Pfn> {
        let mut current = order;
        while current < MAX_ORDER && self.free_lists[current] == NO_FRAME {
            current += 1;
        }
        if current >= MAX_ORDER {
            return None;
        }

        let pfn = self.free_lists[current];
        self.list_remove(pfn, current);

        // Split the block down to the requested order, returning the upper
        // half of each split to the next-smaller free list.
        while current > order {
            current -= 1;
            let buddy = pfn + (1 << current);
            self.list_push(buddy, current);
        }

        // SAFETY: `pfn` is a tracked frame taken off a free list.
        let frame = unsafe { self.frame(pfn) };
        frame.order.store(order as u8, Ordering::Relaxed);
        frame.refcount.store(1, Ordering::Relaxed);
        self.free_pages -= 1 << order;
        Some(pfn)
    }

    fn free(&mut self, pfn: Pfn, order: usize) {
        debug_assert!(pfn < self.max_pfn);
        debug_assert_eq!(pfn as usize & ((1 << order) - 1), 0, "misaligned buddy free");

        // SAFETY: caller guarantees a tracked frame.
        let frame = unsafe { self.frame(pfn) };
        debug_assert!(
            frame.flags.load(Ordering::Relaxed) & FrameFlags::FREE == 0,
            "double free of frame {pfn}"
        );
        frame.refcount.store(0, Ordering::Relaxed);
        frame.flags.fetch_and(
            !(FrameFlags::PAGE_TABLE | FrameFlags::VMO | FrameFlags::PINNED),
            Ordering::Relaxed,
        );

        // The block being freed is not yet counted as free; each buddy it
        // merges with already is and was removed from its free list above. So
        // the free-page count grows by the size of the *original* block, not by
        // the size of the merged block -- adding the merged size would count
        // every absorbed buddy a second time.
        let freed_pages = 1u64 << order;
        let mut pfn = pfn;
        let mut order = order;
        while order + 1 < MAX_ORDER {
            let buddy = pfn ^ (1 << order);
            // Merging is only legal if the buddy is a *whole* free block of the
            // same order and lies inside the managed range.
            if buddy + (1 << order) > self.max_pfn || !self.is_free_block(buddy, order) {
                break;
            }
            self.list_remove(buddy, order);
            pfn = pfn.min(buddy);
            order += 1;
        }
        self.list_push(pfn, order);
        self.free_pages += freed_pages;
    }

    /// Add `[start, end)` (in frames) to the allocator, decomposing it into
    /// naturally aligned buddy blocks.
    fn add_range(&mut self, start: Pfn, end: Pfn) {
        let mut pfn = start;
        while pfn < end {
            // The largest order that is both naturally aligned at `pfn` and
            // fits in the remainder of the range.
            let align_order = if pfn == 0 { MAX_ORDER - 1 } else { pfn.trailing_zeros() as usize };
            let mut order = align_order.min(MAX_ORDER - 1);
            while (1u64 << order) > (end - pfn) as u64 {
                order -= 1;
            }
            // SAFETY: `pfn` is inside the managed range.
            let frame = unsafe { self.frame(pfn) };
            frame.flags.store(0, Ordering::Relaxed);
            frame.refcount.store(0, Ordering::Relaxed);
            self.list_push(pfn, order);
            self.free_pages += 1 << order;
            self.managed_pages += 1 << order;
            pfn += 1 << order;
        }
    }
}

static ALLOCATOR: IrqSpinLock<Buddy> = IrqSpinLock::new(Buddy::empty());

/// Total frames tracked as usable RAM.
static MANAGED_PAGES: AtomicU64 = AtomicU64::new(0);

/// Convert a physical address to its address in the direct map.
///
/// # Panics
/// Panics if `phys` is outside the direct-map window.
#[inline]
pub fn phys_to_virt(phys: u64) -> u64 {
    debug_assert!(phys < super::layout::PHYSMAP_END - PHYSMAP_BASE, "physical address {phys:#x} outside the direct map");
    PHYSMAP_BASE + phys
}

/// Convert a direct-map virtual address back to a physical address.
#[inline]
pub fn virt_to_phys(virt: u64) -> u64 {
    debug_assert!(virt >= PHYSMAP_BASE, "address {virt:#x} is not in the direct map");
    virt - PHYSMAP_BASE
}

/// Physical address of the first byte of `pfn`.
#[inline]
pub const fn pfn_to_phys(pfn: Pfn) -> u64 {
    (pfn as u64) << 12
}

/// Frame number containing `phys`.
#[inline]
pub const fn phys_to_pfn(phys: u64) -> Pfn {
    (phys >> 12) as Pfn
}

/// A physical range reserved before the allocator starts.
#[derive(Clone, Copy)]
struct Reservation {
    start: u64,
    end: u64,
}

/// Initialise the frame allocator from the firmware memory map.
///
/// # Safety
/// Must run exactly once, before any allocation, while the boot page tables
/// (which direct-map the low 4 GiB) are still active, and with `boot_info`
/// pointing at a live Multiboot2 structure.
pub unsafe fn init(boot_info: &BootInfo, kernel_phys_start: u64, kernel_phys_end: u64) {
    // ---- 1. Collect what must never be handed out ------------------------
    let mut reservations = [Reservation { start: 0, end: 0 }; 32];
    let mut reservation_count = 0usize;
    let reserve = |start: u64, end: u64, reservations: &mut [Reservation], count: &mut usize| {
        if end > start && *count < reservations.len() {
            reservations[*count] = Reservation { start, end };
            *count += 1;
        }
    };

    // The first megabyte: real-mode IVT, BIOS data area, EBDA, VGA, option
    // ROMs. Cheap to give up, endlessly troublesome to keep.
    reserve(0, 0x10_0000, &mut reservations, &mut reservation_count);
    reserve(kernel_phys_start, kernel_phys_end, &mut reservations, &mut reservation_count);
    let (mbi_start, mbi_end) = boot_info.phys_range();
    reserve(mbi_start, mbi_end, &mut reservations, &mut reservation_count);
    for module in boot_info.modules() {
        reserve(module.start, module.end, &mut reservations, &mut reservation_count);
    }

    // ---- 2. Find the highest usable frame --------------------------------
    let mut max_phys = 0u64;
    for region in boot_info.memory_map() {
        if region.kind == RegionKind::Usable {
            max_phys = max_phys.max(region.end());
        }
    }
    // The bootstrap page tables direct-map only the low 4 GiB; anything above
    // that becomes available after `mm::init` rebuilds the map.
    let boot_map_limit = 4u64 * 1024 * 1024 * 1024;
    let max_pfn = phys_to_pfn(max_phys.min(boot_map_limit));
    assert!(max_pfn > 0, "firmware reported no usable memory");

    // ---- 3. Bump-allocate the frame database -----------------------------
    let db_bytes = (max_pfn as usize) * core::mem::size_of::<FrameInfo>();
    let db_pages = db_bytes.div_ceil(PAGE_SIZE);
    let db_phys = find_bump_region(boot_info, &reservations[..reservation_count], db_pages)
        .expect("no contiguous region large enough for the frame database");
    reserve(db_phys, db_phys + (db_pages * PAGE_SIZE) as u64, &mut reservations, &mut reservation_count);

    // SAFETY: `db_phys` was chosen from a usable, unreserved region inside the
    // boot direct map, so this pointer is valid for `db_bytes` bytes.
    let frames = phys_to_virt(db_phys) as *mut FrameInfo;
    for i in 0..max_pfn as usize {
        // SAFETY: `i` is within the region we just reserved.
        unsafe { frames.add(i).write(FrameInfo::new()) };
    }

    // ---- 4. Publish free memory to the buddy allocator -------------------
    {
        let mut buddy = ALLOCATOR.lock();
        buddy.frames = frames;
        buddy.max_pfn = max_pfn;

        for region in boot_info.memory_map() {
            if region.kind != RegionKind::Usable {
                continue;
            }
            let region_start = region.base;
            let region_end = region.end().min(boot_map_limit);
            if region_end <= region_start {
                continue;
            }
            // Subtract every reservation that overlaps this region.
            let mut cursor = region_start;
            while cursor < region_end {
                // Find the next reservation starting at or after `cursor`.
                let mut next_start = region_end;
                let mut skip_to = cursor;
                for r in &reservations[..reservation_count] {
                    if r.end <= cursor || r.start >= region_end {
                        continue;
                    }
                    if r.start <= cursor {
                        skip_to = skip_to.max(r.end);
                    } else {
                        next_start = next_start.min(r.start);
                    }
                }
                if skip_to > cursor {
                    cursor = skip_to;
                    continue;
                }
                let chunk_end = next_start.min(region_end);
                let start_pfn = phys_to_pfn((cursor + PAGE_SIZE as u64 - 1) & !(PAGE_SIZE as u64 - 1));
                let end_pfn = phys_to_pfn(chunk_end & !(PAGE_SIZE as u64 - 1));
                if end_pfn > start_pfn {
                    buddy.add_range(start_pfn, end_pfn);
                }
                cursor = chunk_end;
            }
        }
        MANAGED_PAGES.store(buddy.managed_pages, Ordering::Relaxed);
    }

    let stats = stats();
    crate::info!(
        "frame allocator: {} MiB managed, {} MiB free, {} KiB frame database at {:#x}",
        stats.managed_pages * PAGE_SIZE as u64 / (1024 * 1024),
        stats.free_pages * PAGE_SIZE as u64 / (1024 * 1024),
        db_bytes / 1024,
        db_phys
    );
}

/// Find `pages` contiguous, usable, unreserved frames for the frame database.
fn find_bump_region(boot_info: &BootInfo, reservations: &[Reservation], pages: usize) -> Option<u64> {
    let needed = (pages * PAGE_SIZE) as u64;
    let boot_map_limit = 4u64 * 1024 * 1024 * 1024;
    let mut best: Option<u64> = None;
    for region in boot_info.memory_map() {
        if region.kind != RegionKind::Usable {
            continue;
        }
        let mut cursor = region.base.max(0x10_0000);
        let region_end = region.end().min(boot_map_limit);
        while cursor + needed <= region_end {
            // Advance past any reservation covering `cursor`.
            let mut moved = false;
            for r in reservations {
                if cursor < r.end && cursor + needed > r.start {
                    cursor = (r.end + PAGE_SIZE as u64 - 1) & !(PAGE_SIZE as u64 - 1);
                    moved = true;
                    break;
                }
            }
            if moved {
                continue;
            }
            // Prefer the highest such region, leaving low memory for devices
            // that can only DMA below 4 GiB (or, on ISA, below 16 MiB).
            best = Some(match best {
                Some(b) if b > cursor => b,
                _ => cursor,
            });
            break;
        }
    }
    best
}

/// Snapshot of allocator state.
#[derive(Clone, Copy, Debug)]
pub struct Stats {
    /// Frames tracked as usable RAM.
    pub managed_pages: u64,
    /// Frames currently free.
    pub free_pages: u64,
}

/// Read allocator statistics.
pub fn stats() -> Stats {
    let buddy = ALLOCATOR.lock();
    Stats { managed_pages: buddy.managed_pages, free_pages: buddy.free_pages }
}

/// Allocate `2^order` contiguous frames. The returned block has a reference
/// count of one.
pub fn alloc_order(order: usize) -> Option<Pfn> {
    assert!(order < MAX_ORDER, "buddy order {order} exceeds MAX_ORDER");
    ALLOCATOR.lock().alloc(order)
}

/// Allocate a single frame.
pub fn alloc() -> Option<Pfn> {
    alloc_order(0)
}

/// Allocate a single frame and zero it.
pub fn alloc_zeroed() -> Option<Pfn> {
    let pfn = alloc()?;
    // SAFETY: the frame was just allocated, is not aliased, and lies inside the
    // direct map, so it is valid for `PAGE_SIZE` writes.
    unsafe { core::ptr::write_bytes(phys_to_virt(pfn_to_phys(pfn)) as *mut u8, 0, PAGE_SIZE) };
    Some(pfn)
}

/// Release `2^order` frames starting at `pfn`.
///
/// # Safety
/// The block must have been produced by [`alloc_order`] with the same order,
/// must have no live references, and must not be mapped in any address space.
pub unsafe fn free_order(pfn: Pfn, order: usize) {
    ALLOCATOR.lock().free(pfn, order);
}

/// Release a single frame.
///
/// # Safety
/// See [`free_order`].
pub unsafe fn free(pfn: Pfn) {
    // SAFETY: forwarded contract.
    unsafe { free_order(pfn, 0) };
}

/// Take an additional reference to a frame (used when a page is shared or
/// mapped copy-on-write into another address space).
pub fn incref(pfn: Pfn) -> u32 {
    let buddy = ALLOCATOR.lock();
    assert!(pfn < buddy.max_pfn, "incref of untracked frame {pfn}");
    // SAFETY: bounds checked.
    unsafe { buddy.frame(pfn) }.refcount.fetch_add(1, Ordering::Relaxed) + 1
}

/// Drop a reference to a frame, freeing it when the count reaches zero.
///
/// Returns `true` if the frame was freed.
pub fn decref(pfn: Pfn) -> bool {
    let mut buddy = ALLOCATOR.lock();
    assert!(pfn < buddy.max_pfn, "decref of untracked frame {pfn}");
    // SAFETY: bounds checked.
    let previous = unsafe { buddy.frame(pfn) }.refcount.fetch_sub(1, Ordering::Relaxed);
    assert!(previous > 0, "decref of frame {pfn} with zero references");
    if previous == 1 {
        buddy.free(pfn, 0);
        true
    } else {
        false
    }
}

/// Current reference count of a frame.
pub fn refcount(pfn: Pfn) -> u32 {
    let buddy = ALLOCATOR.lock();
    if pfn >= buddy.max_pfn {
        return 0;
    }
    // SAFETY: bounds checked.
    unsafe { buddy.frame(pfn) }.refcount.load(Ordering::Relaxed)
}

/// Set flag bits on a frame.
pub fn set_flags(pfn: Pfn, flags: u8) {
    let buddy = ALLOCATOR.lock();
    if pfn < buddy.max_pfn {
        // SAFETY: bounds checked.
        unsafe { buddy.frame(pfn) }.flags.fetch_or(flags, Ordering::Relaxed);
    }
}

/// Read a frame's flag bits.
pub fn flags(pfn: Pfn) -> u8 {
    let buddy = ALLOCATOR.lock();
    if pfn >= buddy.max_pfn {
        return FrameFlags::RESERVED;
    }
    // SAFETY: bounds checked.
    unsafe { buddy.frame(pfn) }.flags.load(Ordering::Relaxed)
}
