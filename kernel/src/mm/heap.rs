//! The kernel heap.
//!
//! A two-tier allocator:
//!
//! * **Small** allocations (up to 2 KiB) come from segregated free lists, one
//!   per power-of-two size class. Each list is refilled a page at a time from
//!   the frame allocator and carved into equal-sized cells. Freed cells go back
//!   on the list; pages are never returned, which is the right trade for a
//!   kernel whose small-object working set only grows during bring-up.
//! * **Large** allocations go straight to the buddy allocator, rounded up to a
//!   power-of-two page count, and are returned on free.
//!
//! Heap memory is addressed through the direct physical map, so an allocation
//! is physically contiguous and `virt_to_phys` works on it. That is exactly
//! what page tables, DMA buffers and IPC bounce buffers need.

use core::alloc::{GlobalAlloc, Layout};
use core::ptr;
use core::sync::atomic::{AtomicU64, Ordering};

use crate::arch::PAGE_SIZE;
use crate::sync::IrqSpinLock;

use super::phys;

/// Number of size classes: 16, 32, 64, ... 2048 bytes.
const NUM_CLASSES: usize = 8;
/// Smallest allocation unit.
const MIN_CLASS_SHIFT: usize = 4;
/// Largest small allocation.
const MAX_SMALL: usize = 1 << (MIN_CLASS_SHIFT + NUM_CLASSES - 1);

/// A free cell. The first word of a free cell holds the next pointer, which is
/// why the minimum class is 16 bytes.
#[repr(C)]
struct FreeCell {
    next: *mut FreeCell,
}

struct Heap {
    free_lists: [*mut FreeCell; NUM_CLASSES],
}

// SAFETY: all access is serialised by `HEAP`, an `IrqSpinLock`; the pointers
// refer to kernel-owned direct-map memory that outlives the allocator.
unsafe impl Send for Heap {}

impl Heap {
    const fn new() -> Self {
        Self { free_lists: [ptr::null_mut(); NUM_CLASSES] }
    }

    fn class_for(size: usize, align: usize) -> Option<usize> {
        let need = size.max(align).max(1 << MIN_CLASS_SHIFT);
        if need > MAX_SMALL {
            return None;
        }
        let shift = need.next_power_of_two().trailing_zeros() as usize;
        Some(shift - MIN_CLASS_SHIFT)
    }

    /// Carve one fresh page into cells of class `class`.
    fn refill(&mut self, class: usize) -> bool {
        let cell_size = 1usize << (class + MIN_CLASS_SHIFT);
        let Some(pfn) = phys::alloc() else { return false };
        let base = phys::phys_to_virt(phys::pfn_to_phys(pfn));
        HEAP_PAGES.fetch_add(1, Ordering::Relaxed);

        let count = PAGE_SIZE / cell_size;
        // Thread the cells onto the free list back to front so the list ends up
        // in ascending address order, which is friendlier to the prefetcher.
        for i in (0..count).rev() {
            let cell = (base as usize + i * cell_size) as *mut FreeCell;
            // SAFETY: `cell` is inside the freshly allocated, exclusively owned
            // page, and is aligned to `cell_size >= 16`.
            unsafe { (*cell).next = self.free_lists[class] };
            self.free_lists[class] = cell;
        }
        true
    }

    fn alloc_small(&mut self, class: usize) -> *mut u8 {
        if self.free_lists[class].is_null() && !self.refill(class) {
            return ptr::null_mut();
        }
        let cell = self.free_lists[class];
        if cell.is_null() {
            return ptr::null_mut();
        }
        // SAFETY: `cell` came off our own free list, so it points at a live,
        // unowned cell of this class.
        self.free_lists[class] = unsafe { (*cell).next };
        cell as *mut u8
    }

    fn free_small(&mut self, ptr: *mut u8, class: usize) {
        let cell = ptr as *mut FreeCell;
        // SAFETY: the caller is returning a cell this allocator handed out.
        unsafe { (*cell).next = self.free_lists[class] };
        self.free_lists[class] = cell;
    }
}

static HEAP: IrqSpinLock<Heap> = IrqSpinLock::new(Heap::new());
static HEAP_PAGES: AtomicU64 = AtomicU64::new(0);
static LIVE_BYTES: AtomicU64 = AtomicU64::new(0);

/// Pages the heap has taken from the frame allocator.
pub fn pages_in_use() -> u64 {
    HEAP_PAGES.load(Ordering::Relaxed)
}

/// Bytes currently handed out (excluding per-class rounding).
pub fn live_bytes() -> u64 {
    LIVE_BYTES.load(Ordering::Relaxed)
}

/// Buddy order needed for a large allocation of `size` bytes.
///
/// Returns `None` when the rounded-up request is larger than the largest
/// buddy block, which is a failed allocation rather than a reason to panic:
/// `GlobalAlloc::alloc` is required to return null, not unwind.
fn large_order(size: usize) -> Option<usize> {
    let pages = size.div_ceil(PAGE_SIZE);
    let order = pages.next_power_of_two().trailing_zeros() as usize;
    (order < phys::MAX_ORDER).then_some(order)
}

/// The kernel's global allocator.
pub struct KernelAllocator;

// SAFETY: `alloc` returns either null or a pointer to `layout.size()` bytes of
// uniquely owned, correctly aligned direct-map memory, and `dealloc` returns
// exactly such a pointer to the pool it came from.
unsafe impl GlobalAlloc for KernelAllocator {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        let result = match Heap::class_for(layout.size(), layout.align()) {
            Some(class) => HEAP.lock().alloc_small(class),
            None => {
                // Large: straight from the buddy allocator. Alignments above a
                // page are satisfied automatically because buddy blocks are
                // naturally aligned to their own size.
                let Some(order) = large_order(layout.size().max(layout.align())) else {
                    return ptr::null_mut();
                };
                match phys::alloc_order(order) {
                    Some(pfn) => {
                        HEAP_PAGES.fetch_add(1 << order, Ordering::Relaxed);
                        phys::phys_to_virt(phys::pfn_to_phys(pfn)) as *mut u8
                    }
                    None => ptr::null_mut(),
                }
            }
        };
        if !result.is_null() {
            LIVE_BYTES.fetch_add(layout.size() as u64, Ordering::Relaxed);
        }
        result
    }

    unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
        if ptr.is_null() {
            return;
        }
        LIVE_BYTES.fetch_sub(layout.size() as u64, Ordering::Relaxed);
        match Heap::class_for(layout.size(), layout.align()) {
            Some(class) => HEAP.lock().free_small(ptr, class),
            None => {
                // A larger-than-max block could never have been handed out, so
                // there is nothing to return to the buddy allocator.
                let Some(order) = large_order(layout.size().max(layout.align())) else { return };
                let pfn = phys::phys_to_pfn(phys::virt_to_phys(ptr as u64));
                HEAP_PAGES.fetch_sub(1 << order, Ordering::Relaxed);
                // SAFETY: the block came from `alloc_order` with this order and
                // the caller guarantees it is no longer referenced.
                unsafe { phys::free_order(pfn, order) };
            }
        }
    }
}

#[global_allocator]
static ALLOCATOR: KernelAllocator = KernelAllocator;

/// Called when an allocation fails.
///
/// A kernel that cannot allocate cannot make progress, and NodeOS has no
/// swap and no OOM killer inside the kernel (reclaim is a userspace policy
/// service's job). So this is fatal, loudly.
#[cfg(not(test))]
#[no_mangle]
pub extern "C" fn __rust_alloc_error_handler(size: usize, align: usize) -> ! {
    panic!("kernel heap exhausted allocating {size} bytes (align {align}); {} pages in use", pages_in_use());
}
