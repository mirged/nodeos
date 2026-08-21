//! Kernel stacks.
//!
//! Each thread gets its own kernel stack with a guard page below it. The guard
//! is a real unmapped page, not a canary: a deep recursion or a runaway
//! `alloca` faults on the guard instead of quietly writing into whatever
//! allocation happens to sit underneath.

use crate::arch::PAGE_SIZE;
use crate::mm::paging::{self, PageFlags};
use crate::mm::phys::{self, Pfn};
use crate::mm::{kernel_mapper, layout};
use crate::sync::IrqSpinLock;

use alloc::vec::Vec;

/// Kernel stack size. Deep enough for the worst nesting NodeOS allows
/// (syscall -> object -> mm -> frame allocator), shallow enough that a
/// thousand threads cost 8 MiB.
pub const KERNEL_STACK_PAGES: usize = 4;
/// Kernel stack size in bytes.
pub const KERNEL_STACK_SIZE: usize = KERNEL_STACK_PAGES * PAGE_SIZE;
/// Virtual span reserved per stack: the stack itself plus one guard page.
const STACK_SLOT: u64 = ((KERNEL_STACK_PAGES + 1) * PAGE_SIZE) as u64;

struct StackAllocator {
    next: u64,
    free: Vec<u64>,
}

static ALLOCATOR: IrqSpinLock<StackAllocator> =
    IrqSpinLock::new(StackAllocator { next: layout::KERNEL_STACK_BASE, free: Vec::new() });

/// An owned kernel stack.
pub struct KernelStack {
    base: u64,
    frames: Vec<Pfn>,
}

impl KernelStack {
    /// Allocate a kernel stack with a guard page.
    pub fn new() -> Option<Self> {
        let base = {
            let mut allocator = ALLOCATOR.lock();
            match allocator.free.pop() {
                Some(base) => base,
                None => {
                    let base = allocator.next;
                    if base + STACK_SLOT >= layout::KERNEL_STACK_END {
                        return None;
                    }
                    allocator.next += STACK_SLOT;
                    base
                }
            }
        };

        // Slot layout: [guard page][stack pages...]. The guard sits *below* the
        // stack because stacks grow down.
        let mapper = kernel_mapper();
        let mut frames = Vec::with_capacity(KERNEL_STACK_PAGES);
        for page in 0..KERNEL_STACK_PAGES {
            let Some(pfn) = phys::alloc_zeroed() else {
                // Unwind the partial mapping rather than leak it.
                for (i, pfn) in frames.iter().enumerate() {
                    // SAFETY: we mapped exactly these pages moments ago.
                    unsafe { mapper.unmap(base + PAGE_SIZE as u64 * (1 + i as u64)) };
                    // SAFETY: the frame is no longer mapped anywhere.
                    unsafe { phys::free(*pfn) };
                }
                return None;
            };
            let virt = base + PAGE_SIZE as u64 * (1 + page as u64);
            // SAFETY: the kernel owns its own address space, and this virtual
            // range was just reserved exclusively for this stack.
            unsafe {
                mapper
                    .map(virt, phys::pfn_to_phys(pfn), PageFlags::kernel_data(), false)
                    .expect("kernel stack mapping failed");
            }
            frames.push(pfn);
        }

        // Mark the guard explicitly so a fault on it is reported as a stack
        // overflow rather than a generic unmapped access.
        // SAFETY: the guard page is never made present; this only records the
        // software GUARD bit in an otherwise absent entry.
        let _ = unsafe {
            mapper.map(base, 0, PageFlags::GUARD, true).and_then(|()| {
                mapper.protect(base, PageFlags::GUARD)
            })
        };
        // SAFETY: undo the PRESENT bit `map` set; the entry keeps its GUARD tag.
        unsafe { mapper.unmap(base) };

        Some(Self { base, frames })
    }

    /// Lowest address of the usable stack (just above the guard page).
    pub fn bottom(&self) -> u64 {
        self.base + PAGE_SIZE as u64
    }

    /// Top of the stack: the initial value of RSP.
    pub fn top(&self) -> u64 {
        self.base + STACK_SLOT
    }

    /// Address of the guard page.
    pub fn guard(&self) -> u64 {
        self.base
    }

    /// Whether `addr` lies in this stack's guard page.
    pub fn is_guard(&self, addr: u64) -> bool {
        addr >= self.base && addr < self.base + PAGE_SIZE as u64
    }
}

impl Drop for KernelStack {
    fn drop(&mut self) {
        let mapper = kernel_mapper();
        for (i, pfn) in self.frames.iter().enumerate() {
            let virt = self.base + PAGE_SIZE as u64 * (1 + i as u64);
            // SAFETY: this stack owns these mappings and is being destroyed.
            unsafe { mapper.unmap(virt) };
            // SAFETY: the frame is now unmapped and unreferenced.
            unsafe { phys::free(*pfn) };
        }
        paging::flush_all();
        ALLOCATOR.lock().free.push(self.base);
    }
}

/// Whether `addr` falls inside the kernel stack window at all.
pub fn is_kernel_stack_address(addr: u64) -> bool {
    (layout::KERNEL_STACK_BASE..layout::KERNEL_STACK_END).contains(&addr)
}
