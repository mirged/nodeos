//! The userspace heap.
//!
//! A bump allocator over one memory object, with per-size-class free lists for
//! reuse. This is deliberately simple: a NodeOS service's allocation pattern is
//! dominated by message buffers of a handful of sizes, and a program that needs
//! more can map its own memory objects and manage them itself -- there is no
//! reason the runtime's allocator has to be everyone's allocator.
//!
//! The classes run to 64 KiB precisely so that those message buffers are
//! covered. Anything larger is bump-allocated and never reclaimed, which is a
//! real limit rather than an oversight: a program churning blocks that big
//! should be managing a memory object itself.

use core::alloc::{GlobalAlloc, Layout};
use core::ptr;
use core::sync::atomic::{AtomicUsize, Ordering};

use nodeos_abi::types::{MapFlags, MapWindow};

use crate::mem::Vmo;
use crate::sync::SpinLock;

/// Number of size classes: 16, 32, ... 64 KiB.
///
/// The upper classes exist because of one allocation in particular: the reply
/// buffer `Channel::call` builds, which is 4 KiB by default. Anything above the
/// largest class is bump-allocated and never given back, so a service that made
/// one call per frame leaked a page per frame -- which is exactly how Doom's
/// heap ran out. Covering the message-buffer sizes makes the arena reusable for
/// the traffic a NodeOS service actually generates, which is what it was always
/// meant to be sized for.
const CLASSES: usize = 13;
const MIN_SHIFT: usize = 4;
const MAX_SMALL: usize = 1 << (MIN_SHIFT + CLASSES - 1);

struct Heap {
    cursor: usize,
    end: usize,
    free: [*mut u8; CLASSES],
}

// SAFETY: every access goes through `HEAP`, a `SpinLock`.
unsafe impl Send for Heap {}

static HEAP: SpinLock<Heap> = SpinLock::new(Heap { cursor: 0, end: 0, free: [ptr::null_mut(); CLASSES] });
static HEAP_SIZE: AtomicUsize = AtomicUsize::new(0);

/// Establish the process heap. Called once by the startup path.
///
/// Returns the number of bytes mapped.
pub fn init(size: u64) -> u64 {
    let Ok(vmo) = Vmo::create(size, false) else { return 0 };
    let Ok(mapping) = vmo.map(0, size, MapFlags::READ.or(MapFlags::WRITE), MapWindow::Heap) else {
        return 0;
    };
    let base = mapping.base() as usize;
    // The mapping and the memory object must outlive every allocation, which
    // is exactly the process's lifetime.
    core::mem::forget(mapping);
    core::mem::forget(vmo);

    let mut heap = HEAP.lock();
    heap.cursor = base;
    heap.end = base + size as usize;
    HEAP_SIZE.store(size as usize, Ordering::Relaxed);
    size
}

/// Bytes of heap mapped.
pub fn heap_size() -> usize {
    HEAP_SIZE.load(Ordering::Relaxed)
}

fn class_for(size: usize, align: usize) -> Option<usize> {
    let need = size.max(align).max(1 << MIN_SHIFT);
    if need > MAX_SMALL {
        return None;
    }
    Some(need.next_power_of_two().trailing_zeros() as usize - MIN_SHIFT)
}

struct Runtime;

// SAFETY: `alloc` returns null or a uniquely owned, correctly aligned block
// from the process heap, and `dealloc` returns such a block to its class list.
unsafe impl GlobalAlloc for Runtime {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        let mut heap = HEAP.lock();
        if let Some(class) = class_for(layout.size(), layout.align()) {
            let head = heap.free[class];
            if !head.is_null() {
                // SAFETY: the first word of a free cell holds the next pointer,
                // and the minimum class is 16 bytes.
                heap.free[class] = unsafe { *(head as *mut *mut u8) };
                return head;
            }
            let size = 1usize << (class + MIN_SHIFT);
            let base = (heap.cursor + layout.align() - 1) & !(layout.align() - 1);
            if base + size > heap.end {
                return ptr::null_mut();
            }
            heap.cursor = base + size;
            return base as *mut u8;
        }
        let align = layout.align().max(16);
        let base = (heap.cursor + align - 1) & !(align - 1);
        if base + layout.size() > heap.end {
            return ptr::null_mut();
        }
        heap.cursor = base + layout.size();
        base as *mut u8
    }

    unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
        if ptr.is_null() {
            return;
        }
        // Blocks larger than the top class are not reclaimed: a bump arena
        // cannot, and a service that churns buffers that big should be using
        // memory objects directly.
        if let Some(class) = class_for(layout.size(), layout.align()) {
            let mut heap = HEAP.lock();
            // SAFETY: `ptr` came from this allocator's class `class`, so it is
            // at least 16 bytes and can hold the free-list link.
            unsafe { *(ptr as *mut *mut u8) = heap.free[class] };
            heap.free[class] = ptr;
        }
    }
}

#[global_allocator]
static ALLOCATOR: Runtime = Runtime;
