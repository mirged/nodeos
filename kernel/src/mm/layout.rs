//! The NodeOS virtual address space layout.
//!
//! # Design
//!
//! Unix processes get one flat mmap arena and a heap that grows into it.
//! NodeOS instead gives every *kind* of mapping its own multi-terabyte window.
//! That is not cosmetic:
//!
//! * A wild pointer almost always lands in an unmapped hole rather than in
//!   somebody else's object, so bugs fault instead of corrupting.
//! * The kind of a mapping is recoverable from its address alone, which makes
//!   fault handling, `wait`-style diagnostics and capability accounting
//!   O(1) instead of a lookup.
//! * Windows can be handed out to a process *selectively*: a process that was
//!   never granted the device window simply has no address at which a device
//!   mapping could be installed.
//!
//! ```text
//!  user half
//!   0x0000_0000_0000_0000  +-- null guard (never mapped, 4 MiB) ------------+
//!   0x0000_0000_0040_0000  +-- program image (NXF segments) ---------------+
//!   0x0000_1000_0000_0000  +-- heap ---------------------------------------+
//!   0x0000_2000_0000_0000  +-- thread stacks (guard page per stack) -------+
//!   0x0000_3000_0000_0000  +-- shared-memory grants -----------------------+
//!   0x0000_4000_0000_0000  +-- IPC mappings -------------------------------+
//!   0x0000_5000_0000_0000  +-- memory-mapped files ------------------------+
//!   0x0000_6000_0000_0000  +-- device MMIO grants -------------------------+
//!   0x0000_7000_0000_0000  +-- runtime / TLS ------------------------------+
//!   0x0000_8000_0000_0000  === non-canonical hole =========================
//!  kernel half
//!   0xFFFF_8000_0000_0000  +-- direct map of all physical memory (64 TiB) -+
//!   0xFFFF_C000_0000_0000  +-- kernel object arena ------------------------+
//!   0xFFFF_C800_0000_0000  +-- ioremap window -----------------------------+
//!   0xFFFF_E000_0000_0000  +-- kernel stacks ------------------------------+
//!   0xFFFF_FFFF_8000_0000  +-- kernel image -------------------------------+
//! ```

/// Start of the user portion of the address space.
pub const USER_BASE: u64 = 0x0000_0000_0000_0000;
/// One past the last canonical user address.
pub const USER_END: u64 = 0x0000_8000_0000_0000;

/// Guard region at the bottom of every address space. Never mapped, so a null
/// dereference (and any small offset from one) always faults.
pub const NULL_GUARD_BASE: u64 = 0x0000_0000_0000_0000;
/// End of the null guard region.
pub const NULL_GUARD_END: u64 = 0x0000_0000_0040_0000;

/// Where a process image (NXF segments) is loaded.
pub const PROGRAM_BASE: u64 = 0x0000_0000_0040_0000;
/// End of the program window.
pub const PROGRAM_END: u64 = 0x0000_1000_0000_0000;

/// Process heap window.
pub const HEAP_BASE: u64 = 0x0000_1000_0000_0000;
/// End of the heap window.
pub const HEAP_END: u64 = 0x0000_2000_0000_0000;

/// Thread stack window. Stacks are allocated top-down with a guard page below
/// each one.
pub const STACK_BASE: u64 = 0x0000_2000_0000_0000;
/// End of the thread stack window.
pub const STACK_END: u64 = 0x0000_3000_0000_0000;

/// Window for explicitly granted shared-memory regions.
pub const SHARED_BASE: u64 = 0x0000_3000_0000_0000;
/// End of the shared-memory window.
pub const SHARED_END: u64 = 0x0000_4000_0000_0000;

/// Window for memory transferred as part of an IPC message.
pub const IPC_BASE: u64 = 0x0000_4000_0000_0000;
/// End of the IPC window.
pub const IPC_END: u64 = 0x0000_5000_0000_0000;

/// Window for memory-mapped file objects.
pub const MMAP_FILE_BASE: u64 = 0x0000_5000_0000_0000;
/// End of the file-mapping window.
pub const MMAP_FILE_END: u64 = 0x0000_6000_0000_0000;

/// Window for device MMIO granted to a userspace driver.
pub const DEVICE_BASE: u64 = 0x0000_6000_0000_0000;
/// End of the device window.
pub const DEVICE_END: u64 = 0x0000_7000_0000_0000;

/// Window for the userspace runtime and thread-local storage.
pub const RUNTIME_BASE: u64 = 0x0000_7000_0000_0000;
/// End of the runtime window.
pub const RUNTIME_END: u64 = 0x0000_7FFF_FFFF_F000;

/// Base of the kernel's direct map of physical memory.
pub const PHYSMAP_BASE: u64 = 0xFFFF_8000_0000_0000;
/// End of the direct map (64 TiB of addressable physical memory).
pub const PHYSMAP_END: u64 = 0xFFFF_C000_0000_0000;

/// Kernel arena for objects that need virtual (not physical) contiguity.
pub const KERNEL_ARENA_BASE: u64 = 0xFFFF_C000_0000_0000;
/// End of the kernel object arena.
pub const KERNEL_ARENA_END: u64 = 0xFFFF_C800_0000_0000;

/// Window for kernel MMIO mappings created by `ioremap`.
pub const IOREMAP_BASE: u64 = 0xFFFF_C800_0000_0000;
/// End of the ioremap window.
pub const IOREMAP_END: u64 = 0xFFFF_D000_0000_0000;

/// Window for kernel (per-thread) stacks, each with a guard page.
pub const KERNEL_STACK_BASE: u64 = 0xFFFF_E000_0000_0000;
/// End of the kernel stack window.
pub const KERNEL_STACK_END: u64 = 0xFFFF_F000_0000_0000;

/// Base of the kernel image window (`virt == phys + KERNEL_IMAGE_BASE`).
pub const KERNEL_IMAGE_BASE: u64 = 0xFFFF_FFFF_8000_0000;

/// Which window a virtual address belongs to.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Window {
    /// The never-mapped guard at address zero.
    NullGuard,
    /// Loaded program image.
    Program,
    /// Process heap.
    Heap,
    /// Thread stacks.
    Stack,
    /// Shared-memory grants.
    Shared,
    /// IPC-transferred mappings.
    Ipc,
    /// Memory-mapped files.
    MappedFile,
    /// Device MMIO.
    Device,
    /// Userspace runtime and TLS.
    Runtime,
    /// Kernel direct physical map.
    PhysMap,
    /// Kernel object arena.
    KernelArena,
    /// Kernel MMIO mappings.
    IoRemap,
    /// Kernel stacks.
    KernelStack,
    /// The kernel image itself.
    KernelImage,
    /// Not inside any defined window.
    Unassigned,
}

/// Classify a virtual address by the window it falls in.
pub const fn classify(addr: u64) -> Window {
    match addr {
        a if a < NULL_GUARD_END => Window::NullGuard,
        a if a < PROGRAM_END => Window::Program,
        a if a < HEAP_END => Window::Heap,
        a if a < STACK_END => Window::Stack,
        a if a < SHARED_END => Window::Shared,
        a if a < IPC_END => Window::Ipc,
        a if a < MMAP_FILE_END => Window::MappedFile,
        a if a < DEVICE_END => Window::Device,
        a if a < RUNTIME_END => Window::Runtime,
        a if a < USER_END => Window::Unassigned,
        a if a < PHYSMAP_BASE => Window::Unassigned,
        a if a < PHYSMAP_END => Window::PhysMap,
        a if a < KERNEL_ARENA_END => Window::KernelArena,
        a if a < IOREMAP_END => Window::IoRemap,
        a if a < KERNEL_STACK_BASE => Window::Unassigned,
        a if a < KERNEL_STACK_END => Window::KernelStack,
        a if a < KERNEL_IMAGE_BASE => Window::Unassigned,
        _ => Window::KernelImage,
    }
}

/// Whether `addr` is a canonical 48-bit address.
pub const fn is_canonical(addr: u64) -> bool {
    let sign_extended = ((addr as i64) << 16) >> 16;
    sign_extended as u64 == addr
}

/// Whether `addr` lies in the user half.
pub const fn is_user(addr: u64) -> bool {
    addr < USER_END
}

/// Whether `[addr, addr + len)` lies entirely in the user half.
pub const fn is_user_range(addr: u64, len: u64) -> bool {
    match addr.checked_add(len) {
        Some(end) => addr >= NULL_GUARD_END && end <= USER_END,
        None => false,
    }
}
