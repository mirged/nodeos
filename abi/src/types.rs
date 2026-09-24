//! Shared value types.

/// A process-local capability name.
///
/// Zero is always invalid, so a zeroed structure never accidentally names an
/// object.
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug, Default)]
#[repr(transparent)]
pub struct HandleValue(pub u32);

impl HandleValue {
    /// The invalid handle.
    pub const INVALID: Self = Self(0);

    /// Whether this handle is the invalid one.
    pub const fn is_invalid(self) -> bool {
        self.0 == 0
    }
}

/// Per-handle rights. See the kernel's `obj::rights` for the authoritative
/// discussion; these constants are the wire values.
#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
#[repr(transparent)]
pub struct Rights(pub u32);

impl Rights {
    /// No authority.
    pub const NONE: Self = Self(0);
    /// May be duplicated.
    pub const DUPLICATE: Self = Self(1 << 0);
    /// May be sent over a channel.
    pub const TRANSFER: Self = Self(1 << 1);
    /// May be read.
    pub const READ: Self = Self(1 << 2);
    /// May be written.
    pub const WRITE: Self = Self(1 << 3);
    /// May be mapped executable.
    pub const EXECUTE: Self = Self(1 << 4);
    /// May be mapped.
    pub const MAP: Self = Self(1 << 5);
    /// May read properties.
    pub const GET_PROPERTY: Self = Self(1 << 6);
    /// May set properties.
    pub const SET_PROPERTY: Self = Self(1 << 7);
    /// May raise user signals.
    pub const SIGNAL: Self = Self(1 << 8);
    /// May be waited on.
    pub const WAIT: Self = Self(1 << 9);
    /// May be destroyed.
    pub const DESTROY: Self = Self(1 << 10);
    /// May manage a process.
    pub const MANAGE_PROCESS: Self = Self(1 << 11);
    /// May manage a thread.
    pub const MANAGE_THREAD: Self = Self(1 << 12);
    /// May perform device I/O.
    pub const DEVICE_IO: Self = Self(1 << 13);
    /// May bind an interrupt.
    pub const IRQ_BIND: Self = Self(1 << 14);
    /// May resize.
    pub const RESIZE: Self = Self(1 << 15);
    /// May derive child objects.
    pub const DERIVE: Self = Self(1 << 16);
    /// May revoke derived grants.
    pub const REVOKE: Self = Self(1 << 17);

    /// Everything a freshly created object's handle carries.
    pub const DEFAULT: Self = Self(0x3FFFF);

    /// Union.
    pub const fn or(self, other: Self) -> Self {
        Self(self.0 | other.0)
    }
    /// Whether every bit of `other` is set.
    pub const fn contains(self, other: Self) -> bool {
        self.0 & other.0 == other.0
    }
}

/// Observable object state bits.
#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
#[repr(transparent)]
pub struct Signals(pub u32);

impl Signals {
    /// No signals.
    pub const NONE: Self = Self(0);
    /// A channel has a message queued.
    pub const READABLE: Self = Self(1 << 0);
    /// A channel has queue space.
    pub const WRITABLE: Self = Self(1 << 1);
    /// The far end of a channel is gone.
    pub const PEER_CLOSED: Self = Self(1 << 2);
    /// A process exited.
    pub const PROCESS_TERMINATED: Self = Self(1 << 3);
    /// A thread exited.
    pub const THREAD_TERMINATED: Self = Self(1 << 4);
    /// A timer fired.
    pub const TIMER_EXPIRED: Self = Self(1 << 5);
    /// An interrupt is pending.
    pub const IRQ_PENDING: Self = Self(1 << 6);
    /// A memory object was resized.
    pub const VMO_RESIZED: Self = Self(1 << 7);
    /// A thread faulted and is waiting for a supervisor.
    pub const EXCEPTION: Self = Self(1 << 8);
    /// The object was destroyed.
    pub const DESTROYED: Self = Self(1 << 9);

    /// Union.
    pub const fn or(self, other: Self) -> Self {
        Self(self.0 | other.0)
    }
    /// Whether every bit of `other` is set.
    pub const fn contains(self, other: Self) -> bool {
        self.0 & other.0 == other.0
    }
}

/// One entry in a [`crate::Syscall::Wait`] array.
#[repr(C)]
#[derive(Clone, Copy, Debug, Default)]
pub struct WaitItem {
    /// Object to watch.
    pub handle: HandleValue,
    /// Signals of interest.
    pub wanted: u32,
    /// Filled in by the kernel with the signals that were actually asserted.
    pub observed: u32,
    /// Padding to keep the structure 16 bytes.
    pub _reserved: u32,
}

/// Scheduling class, as passed to `thread_create`.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
#[repr(u32)]
pub enum SchedClass {
    /// Deadline driven.
    RealTime = 0,
    /// Low latency, fixed priority.
    Interactive = 1,
    /// Weighted fair share.
    Normal = 2,
    /// Idle-time only.
    Background = 3,
}

/// Scheduling parameters, as passed to `thread_create`.
#[repr(C)]
#[derive(Clone, Copy, Debug)]
pub struct ThreadParams {
    /// Scheduling class discriminant.
    pub class: u32,
    /// Priority within the class, 0..=31.
    pub priority: u32,
    /// Period in nanoseconds, for the real-time class.
    pub period_ns: u64,
    /// CPU budget per period in nanoseconds, for the real-time class.
    pub budget_ns: u64,
    /// CPU affinity mask; zero means any CPU.
    pub affinity: u64,
}

/// Result written by `process_load`: the entry point and initial stack for the
/// loaded image, both in the child process's address space.
#[repr(C)]
#[derive(Clone, Copy, Debug, Default)]
pub struct LoadedImageInfo {
    /// Absolute entry address.
    pub entry: u64,
    /// Initial stack pointer.
    pub stack: u64,
}

impl Default for ThreadParams {
    fn default() -> Self {
        Self { class: SchedClass::Normal as u32, priority: 16, period_ns: 0, budget_ns: 0, affinity: 0 }
    }
}

/// Mapping permissions, as passed to `memory_map`.
#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
#[repr(transparent)]
pub struct MapFlags(pub u32);

impl MapFlags {
    /// Readable.
    pub const READ: Self = Self(1 << 0);
    /// Writable.
    pub const WRITE: Self = Self(1 << 1);
    /// Executable.
    pub const EXECUTE: Self = Self(1 << 2);
    /// Writes create a private copy.
    pub const COW: Self = Self(1 << 3);
    /// Writes are visible to other mappings of the object.
    pub const SHARED: Self = Self(1 << 4);
    /// Commit and map eagerly.
    pub const POPULATE: Self = Self(1 << 5);
    /// Uncached device memory.
    pub const DEVICE: Self = Self(1 << 6);

    /// Union.
    pub const fn or(self, other: Self) -> Self {
        Self(self.0 | other.0)
    }
}

/// Which layout window a mapping should be placed in.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
#[repr(u32)]
pub enum MapWindow {
    /// Program image.
    Program = 0,
    /// Process heap.
    Heap = 1,
    /// Thread stacks.
    Stack = 2,
    /// Shared-memory grants.
    Shared = 3,
    /// IPC-transferred mappings.
    Ipc = 4,
    /// Memory-mapped files.
    MappedFile = 5,
    /// Device MMIO.
    Device = 6,
    /// Runtime and TLS.
    Runtime = 7,
}

/// Capability classes an NXF image can declare.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
#[repr(u32)]
pub enum CapClass {
    /// Read filesystem objects.
    FileRead = 1,
    /// Write filesystem objects.
    FileWrite = 2,
    /// Create filesystem objects.
    FileCreate = 3,
    /// Use the network.
    Network = 4,
    /// Bind privileged ports.
    NetworkBind = 5,
    /// Use the GPU.
    Gpu = 6,
    /// Use audio.
    Audio = 7,
    /// Allocate beyond the declared working set.
    Memory = 8,
    /// Create processes.
    Process = 9,
    /// Create channels.
    Channel = 10,
    /// Claim a device.
    Device = 11,
    /// Read the high-resolution clock.
    Clock = 12,
    /// Debug other processes.
    Debug = 13,
    /// Load further executables.
    Load = 14,
    /// Receive input.
    Input = 15,
    /// Present to a display.
    Display = 16,
}

/// Information returned by `capability_query`.
#[repr(C)]
#[derive(Clone, Copy, Debug, Default)]
pub struct HandleInfo {
    /// Kernel object identifier.
    pub koid: u64,
    /// Object type discriminant.
    pub object_type: u32,
    /// Rights this handle carries.
    pub rights: u32,
    /// Currently asserted signals.
    pub signals: u32,
    /// Depth of the grant chain behind this handle.
    pub grant_depth: u32,
    /// Size in bytes, for objects that have one (a memory object); `0`
    /// otherwise. Without this a process handed a memory object it did not
    /// create -- the usual case, since capabilities arrive by message -- has
    /// no way to learn how much of it to map.
    pub size: u64,
}

/// Device kinds reported by `device_info`.
pub mod device_kind {
    /// A framebuffer MMIO device.
    pub const FRAMEBUFFER: u32 = 1;
    /// The PS/2 keyboard.
    pub const PS2_KEYBOARD: u32 = 2;
    /// An Intel AC'97 audio controller, as two port windows: the mixer
    /// (window 0) and the bus-master engine (window 1).
    pub const AC97: u32 = 3;
    /// A virtio block device on its legacy port-I/O interface (window 0).
    pub const VIRTIO_BLK: u32 = 4;
    /// The PS/2 mouse, on the 8042's auxiliary port.
    pub const PS2_MOUSE: u32 = 5;
}

/// Information returned by `device_info`. Fields not meaningful for a given
/// [`device_kind`] are zero.
#[repr(C)]
#[derive(Clone, Copy, Debug, Default)]
pub struct DeviceInfo {
    /// See [`device_kind`].
    pub kind: u32,
    /// Framebuffer width, in pixels.
    pub width: u32,
    /// Framebuffer height, in pixels.
    pub height: u32,
    /// Framebuffer bytes per scanline.
    pub pitch: u32,
    /// Framebuffer bits per pixel.
    pub bpp: u32,
    /// PCI vendor identifier, for a PCI-backed device.
    pub vendor: u32,
    /// PCI device identifier, for a PCI-backed device.
    pub device: u32,
    /// Base port of each window the device decodes; `0` where there is none.
    ///
    /// A driver never uses these to issue a port instruction -- it cannot, it
    /// is in ring 3 -- but knowing the window is non-empty is how it tells a
    /// device it can drive from one it cannot.
    pub port_base: [u32; 2],
    /// Length in ports of each window; `0` where there is none.
    pub port_len: [u32; 2],
    /// Interrupt line the firmware routed the device to, or `0`.
    pub irq: u32,
    /// 1 when the device answered at bring-up, 0 when it did not.
    ///
    /// Only a device the kernel probes can be absent while its capability
    /// still exists: the PS/2 mouse is a capability whether or not a mouse is
    /// plugged into the machine, and a driver that skipped this check would
    /// wait forever on a byte stream nothing will ever fill.
    pub present: u32,
    /// Padding.
    pub _reserved: u32,
}

/// Result of `device_dma_alloc`: a memory object plus the physical address the
/// device must be pointed at.
///
/// The physical address is the entire reason this syscall exists. It is
/// meaningless to a process -- it cannot be dereferenced, and the object still
/// has to be mapped to be touched -- and it is exactly what goes into a
/// descriptor ring, because the device has no page tables to consult.
#[repr(C)]
#[derive(Clone, Copy, Debug, Default)]
pub struct DmaInfo {
    /// Handle to the memory object.
    pub handle: u32,
    /// Padding.
    pub _reserved: u32,
    /// Physical base address of the buffer.
    pub phys: u64,
    /// Length in bytes, rounded up to the allocator's block size.
    pub len: u64,
}
