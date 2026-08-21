//! Memory management.
//!
//! Layering, bottom up:
//!
//! * [`phys`] -- physical frames, reference counted, buddy allocated.
//! * [`paging`] -- the hardware translation tables.
//! * [`heap`] -- the kernel's `GlobalAlloc`.
//! * [`vmo`] -- *memory objects*: the capability-controlled unit of memory.
//! * [`space`] -- address spaces, built out of memory-object mappings.
//! * [`uaccess`] -- fault-tolerant access to user memory from the kernel.
//!
//! Nothing above [`phys`] ever names a physical address directly. Userspace
//! never names one at all: a process describes memory only as "this many bytes
//! of this memory object, mapped here, with these rights", and the rights come
//! from a capability it was explicitly given.

pub mod heap;
pub mod layout;
pub mod paging;
pub mod phys;
pub mod space;
pub mod vmo;

use crate::arch::x86_64::cpu;
use crate::arch::{LARGE_PAGE_SIZE, PAGE_SIZE};
use crate::boot::{BootInfo, RegionKind};
use crate::sync::Once;

use paging::{Mapper, PageFlags};
use phys::Pfn;

extern "C" {
    static __image_phys_start: u8;
    static __image_phys_end: u8;
    static __text_start: u8;
    static __text_end: u8;
    static __rodata_start: u8;
    static __rodata_end: u8;
    static __data_start: u8;
    static __bss_end: u8;
}

/// The kernel's own address space root, shared by every process's upper half.
static KERNEL_MAPPER: Once<Mapper> = Once::new();

/// Physical address range occupied by the kernel image.
pub fn kernel_image_range() -> (u64, u64) {
    // Taking the addresses of linker-provided symbols; the bytes are never read.
    let start = core::ptr::addr_of!(__image_phys_start) as u64;
    let end = core::ptr::addr_of!(__image_phys_end) as u64;
    (start, end)
}

/// The kernel address space.
pub fn kernel_mapper() -> &'static Mapper {
    KERNEL_MAPPER.get()
}

/// Bring up memory management.
///
/// Order of operations matters and is not negotiable:
///
/// 1. Parse the memory map and start the frame allocator, using the bootstrap
///    page tables (which direct-map the low 4 GiB).
/// 2. Build the kernel's real page tables: a direct map sized to actual RAM,
///    plus the kernel image with per-section permissions.
/// 3. Switch CR3 and drop the identity map, so a stray low pointer faults.
///
/// # Safety
/// Must run exactly once, on the boot CPU, with interrupts disabled, before
/// anything allocates.
pub unsafe fn init(boot_info: &BootInfo) -> u64 {
    let (kernel_start, kernel_end) = kernel_image_range();

    // SAFETY: first call, boot tables still active.
    unsafe { phys::init(boot_info, kernel_start, kernel_end) };

    // SAFETY: the frame allocator is live, so we can allocate page tables.
    let mapper = unsafe { build_kernel_tables(boot_info) };
    KERNEL_MAPPER.init(mapper);

    let cr3 = mapper.cr3();
    // SAFETY: the new tables map the kernel image (including the code we are
    // executing and the stack we are on) and the direct map, so execution
    // continues uninterrupted across the switch.
    unsafe { cpu::write_cr3(cr3) };

    crate::info!(
        "kernel address space online: cr3 {:#x}, {} MiB direct-mapped",
        cr3,
        direct_map_bytes() / (1024 * 1024)
    );
    cr3
}

static DIRECT_MAP_BYTES: core::sync::atomic::AtomicU64 = core::sync::atomic::AtomicU64::new(0);

/// How much physical memory the direct map covers.
pub fn direct_map_bytes() -> u64 {
    DIRECT_MAP_BYTES.load(core::sync::atomic::Ordering::Relaxed)
}

/// Construct the kernel's page tables.
///
/// # Safety
/// The frame allocator must be initialised and the bootstrap direct map must
/// still be active.
unsafe fn build_kernel_tables(boot_info: &BootInfo) -> Mapper {
    let root = paging::alloc_page_table().expect("out of memory building the kernel PML4");
    // SAFETY: `root` is a freshly allocated, zeroed PML4 owned by the kernel.
    let mapper = unsafe { Mapper::new(root) };

    // ---- Pre-populate the entire kernel half of the PML4 -----------------
    // Address spaces share the kernel half by *copying* PML4 entries 256..512
    // when they are created. If a kernel mapping later needs a PML4 slot that
    // was empty at copy time -- a per-thread kernel stack, say -- it would
    // appear only in the kernel's own tables, and a thread running in any other
    // address space would fault on its own stack and then double-fault trying
    // to report it.
    //
    // Allocating all 256 second-level tables up front (1 MiB, once) makes
    // "the kernel half is shared" true by construction: every later kernel
    // mapping edits a table that every address space already points at.
    for index in 256..512 {
        // SAFETY: `root` holds a live, zeroed PML4 that only we reference.
        let table = unsafe { &mut *(phys::phys_to_virt(phys::pfn_to_phys(root)) as *mut paging::PageTable) };
        if table.entries[index].is_present() {
            continue;
        }
        let pdpt = paging::alloc_page_table().expect("out of memory pre-populating the kernel PML4");
        table.entries[index] =
            paging::Entry::new(phys::pfn_to_phys(pdpt), PageFlags::PRESENT.or(PageFlags::WRITABLE));
    }

    // ---- Direct physical map --------------------------------------------
    // Cover every byte of RAM the firmware reported (including reserved
    // ranges, so device memory and ACPI tables are reachable), rounded out to
    // large-page boundaries.
    let mut top_of_ram = 0u64;
    for region in boot_info.memory_map() {
        if matches!(region.kind, RegionKind::Usable | RegionKind::AcpiReclaimable | RegionKind::AcpiNvs) {
            top_of_ram = top_of_ram.max(region.end());
        }
    }
    // Always cover at least the low 4 GiB so that MMIO apertures (local APIC,
    // I/O APIC, PCI ECAM, framebuffer) stay reachable.
    let map_limit = top_of_ram.max(4 * 1024 * 1024 * 1024);
    let map_limit = (map_limit + LARGE_PAGE_SIZE as u64 - 1) & !(LARGE_PAGE_SIZE as u64 - 1);
    DIRECT_MAP_BYTES.store(map_limit, core::sync::atomic::Ordering::Relaxed);

    let flags = PageFlags::kernel_data();
    let mut phys_addr = 0u64;
    while phys_addr < map_limit {
        // SAFETY: we own this address space; the direct map is kernel-only and
        // non-executable, and each 2 MiB slot is written exactly once.
        unsafe {
            mapper
                .map_large(layout::PHYSMAP_BASE + phys_addr, phys_addr, flags, true)
                .expect("failed to build the direct physical map");
        }
        phys_addr += LARGE_PAGE_SIZE as u64;
    }

    // ---- Kernel image, with real permissions ----------------------------
    // The bootstrap tables mapped the whole image RWX. Re-map it here as
    // R-X / R-- / RW-NX so that W^X actually holds for the kernel.
    let text_start = core::ptr::addr_of!(__text_start) as u64;
    let text_end = core::ptr::addr_of!(__text_end) as u64;
    let rodata_start = core::ptr::addr_of!(__rodata_start) as u64;
    let rodata_end = core::ptr::addr_of!(__rodata_end) as u64;
    let data_start = core::ptr::addr_of!(__data_start) as u64;
    let bss_end = core::ptr::addr_of!(__bss_end) as u64;

    // The `.boot` section holds the 32-bit trampoline and the bootstrap page
    // tables; it lives at its physical address and is dead after this point,
    // so it is deliberately not re-mapped.
    map_kernel_section(&mapper, text_start, text_end, PageFlags::kernel_code());
    map_kernel_section(&mapper, rodata_start, rodata_end, PageFlags::kernel_rodata());
    map_kernel_section(&mapper, data_start, bss_end, PageFlags::kernel_data());

    mapper
}

/// Map one linked kernel section, translating virtual to physical by the fixed
/// image offset.
fn map_kernel_section(mapper: &Mapper, start: u64, end: u64, flags: PageFlags) {
    let start = start & !(PAGE_SIZE as u64 - 1);
    let end = (end + PAGE_SIZE as u64 - 1) & !(PAGE_SIZE as u64 - 1);
    let mut virt = start;
    while virt < end {
        let phys_addr = virt - layout::KERNEL_IMAGE_BASE;
        // SAFETY: we own this address space; the kernel image frames are
        // reserved, so nothing else can map or free them.
        unsafe {
            mapper.map(virt, phys_addr, flags, true).expect("failed to map a kernel section");
        }
        virt += PAGE_SIZE as u64;
    }
}

/// Read `len` bytes of physical memory through the direct map.
///
/// # Safety
/// `[phys_addr, phys_addr + len)` must lie inside the direct map and hold
/// initialised bytes that no one is concurrently mutating.
pub unsafe fn phys_slice<'a>(phys_addr: u64, len: usize) -> &'a [u8] {
    // SAFETY: forwarded contract; the direct map is linear so one slice covers
    // the whole range.
    unsafe { core::slice::from_raw_parts(phys::phys_to_virt(phys_addr) as *const u8, len) }
}

/// Zero a physical frame through the direct map.
///
/// # Safety
/// `pfn` must be a frame the caller owns exclusively.
pub unsafe fn zero_frame(pfn: Pfn) {
    // SAFETY: forwarded contract.
    unsafe { core::ptr::write_bytes(phys::phys_to_virt(phys::pfn_to_phys(pfn)) as *mut u8, 0, PAGE_SIZE) };
}

/// Copy one physical frame to another through the direct map.
///
/// # Safety
/// Both frames must be live and non-overlapping, and `dst` must be owned
/// exclusively by the caller.
pub unsafe fn copy_frame(dst: Pfn, src: Pfn) {
    // SAFETY: forwarded contract; distinct frames never overlap.
    unsafe {
        core::ptr::copy_nonoverlapping(
            phys::phys_to_virt(phys::pfn_to_phys(src)) as *const u8,
            phys::phys_to_virt(phys::pfn_to_phys(dst)) as *mut u8,
            PAGE_SIZE,
        )
    };
}

/// In-kernel checks that the memory manager actually does what it claims.
///
/// These run at boot rather than in a test harness because the properties they
/// check (W^X on the kernel image, the identity map being gone, reference
/// counting under aliasing) are exactly the ones whose violation is invisible
/// until something is silently corrupted much later.
pub fn selftest() {
    use alloc::boxed::Box;
    use alloc::collections::BTreeMap;
    use alloc::vec::Vec;

    let mapper = kernel_mapper();

    // 1. The kernel text is mapped, executable and *not* writable.
    let text = core::ptr::addr_of!(__text_start) as u64;
    let (text_phys, text_flags) = mapper.translate(text).expect("kernel text is not mapped");
    assert_eq!(text_phys, text - layout::KERNEL_IMAGE_BASE, "kernel text translates to the wrong frame");
    assert!(!text_flags.contains(PageFlags::WRITABLE), "kernel text is writable: W^X is broken");
    assert!(!text_flags.contains(PageFlags::NO_EXECUTE), "kernel text is not executable");
    assert!(!text_flags.contains(PageFlags::USER), "kernel text is reachable from ring 3");

    // 2. Kernel data is writable but never executable.
    let data = core::ptr::addr_of!(__data_start) as u64;
    let (_, data_flags) = mapper.translate(data).expect("kernel data is not mapped");
    assert!(data_flags.contains(PageFlags::WRITABLE), "kernel data is read-only");
    assert!(data_flags.contains(PageFlags::NO_EXECUTE), "kernel data is executable: W^X is broken");

    // 3. The bootstrap identity map is gone, so a stray low pointer faults
    //    instead of silently hitting whatever used to live at that address.
    assert!(mapper.translate(0x1000).is_none(), "the identity map survived into the kernel tables");
    assert!(
        mapper.translate(layout::PROGRAM_BASE).is_none(),
        "the kernel address space has user mappings"
    );

    // 4. The direct map round-trips.
    let probe = phys::alloc_zeroed().expect("frame allocation failed");
    let virt = phys::phys_to_virt(phys::pfn_to_phys(probe));
    assert_eq!(phys::virt_to_phys(virt), phys::pfn_to_phys(probe));
    let (mapped, _) = mapper.translate(virt).expect("direct map is missing a frame");
    assert_eq!(mapped, phys::pfn_to_phys(probe), "direct map translates incorrectly");

    // 5. Reference counting: a frame is only freed when the last reference goes.
    assert_eq!(phys::refcount(probe), 1);
    phys::incref(probe);
    assert_eq!(phys::refcount(probe), 2);
    assert!(!phys::decref(probe), "frame freed while still referenced");
    assert!(phys::decref(probe), "frame not freed when the last reference went");

    // 6. The buddy allocator returns naturally aligned, contiguous blocks.
    let order = 4; // 16 pages
    let block = phys::alloc_order(order).expect("contiguous allocation failed");
    assert_eq!(block as usize % (1 << order), 0, "buddy block is not naturally aligned");
    // SAFETY: the block was just allocated and is not mapped anywhere.
    unsafe { phys::free_order(block, order) };

    // 7. The heap actually works, including growth and freeing.
    let mut v: Vec<u64> = Vec::new();
    for i in 0..4096u64 {
        v.push(i * i);
    }
    assert_eq!(v[4095], 4095 * 4095);
    let boxed = Box::new([0xABu8; 3000]);
    assert_eq!(boxed[2999], 0xAB);
    let mut map = BTreeMap::new();
    for i in 0..1000u32 {
        map.insert(i, i * 3);
    }
    assert_eq!(map.get(&999), Some(&2997));
    drop(v);
    drop(boxed);
    drop(map);

    crate::info!(
        "mm self-test passed: {} MiB free, heap {} pages / {} KiB live",
        phys::stats().free_pages * PAGE_SIZE as u64 / (1024 * 1024),
        heap::pages_in_use(),
        heap::live_bytes() / 1024
    );
}

/// Resolve a page fault against the faulting thread's address space.
///
/// Returns `true` when a mapping was installed and the instruction should be
/// retried.
fn page_fault(address: u64, error: paging_fault::PageFaultError, _frame: &mut crate::arch::x86_64::trap::TrapFrame) -> bool {
    // A kernel-mode fault on a kernel stack guard page is a stack overflow, and
    // deserves to be reported as one rather than as a mysterious unmapped
    // access two frames deep in whatever recursed.
    if let Some(thread) = crate::sched::current_thread() {
        if thread.is_stack_guard(address) {
            crate::error!(
                "kernel stack overflow in thread {} ({}) at guard page {:#x}",
                thread.tid(),
                thread.name(),
                address
            );
            return false;
        }
    }

    if !layout::is_user(address) {
        return false;
    }

    let Some(process) = crate::sched::current_process() else { return false };
    process.space().handle_fault(address, error.write(), error.instruction_fetch())
}

use crate::arch::x86_64::exception as paging_fault;

/// Install the demand-paging fault handler.
///
/// # Safety
/// Call once, with interrupts disabled, after the scheduler exists.
pub unsafe fn init_fault_handler() {
    // SAFETY: single-threaded bring-up with interrupts disabled.
    unsafe { paging_fault::set_page_fault_handler(page_fault) };
}
