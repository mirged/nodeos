//! Syscall numbers.

/// The complete NodeOS syscall set.
///
/// Twenty-four entry points. Compare against roughly 350 on Linux: the
/// difference is not minimalism for its own sake, it is that every operation a
/// Unix kernel performs on behalf of a *policy* -- naming files, resolving
/// hostnames, deciding who may signal whom -- happens in a userspace service
/// here, and services are reached with `channel_call`, which is one syscall.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
#[repr(u64)]
pub enum Syscall {
    /// Reserved; always fails.
    Invalid = 0,

    // ---- Execution -------------------------------------------------------
    /// Create an empty process. Returns a process handle.
    ProcessCreate = 1,
    /// Start a created process's first thread.
    ProcessStart = 2,
    /// Terminate the calling process. Does not return.
    ProcessExit = 3,
    /// Create a thread in a process. Returns a thread handle.
    ThreadCreate = 4,
    /// Terminate the calling thread. Does not return.
    ThreadExit = 5,
    /// Start a created thread.
    ThreadStart = 6,
    /// Yield the CPU.
    Yield = 7,

    // ---- Memory ----------------------------------------------------------
    /// Create a memory object. Returns a VMO handle.
    MemoryAllocate = 8,
    /// Map a memory object into an address space. Returns the base address.
    MemoryMap = 9,
    /// Remove a mapping.
    MemoryUnmap = 10,
    /// Change a mapping's permissions (narrowing only).
    MemoryProtect = 11,
    /// Read or write a memory object without mapping it.
    MemoryTransfer = 12,

    // ---- Communication ---------------------------------------------------
    /// Create a connected channel pair. Returns two handles.
    ChannelCreate = 13,
    /// Send a typed message, transferring any capabilities it carries.
    ChannelSend = 14,
    /// Receive a typed message.
    ChannelReceive = 15,
    /// Send a request and block for the matching reply.
    ChannelCall = 16,

    // ---- Authority -------------------------------------------------------
    /// Duplicate a handle with reduced rights.
    CapabilityDuplicate = 17,
    /// Revoke a grant and everything derived from it.
    CapabilityRevoke = 18,
    /// Close a handle.
    CapabilityClose = 19,
    /// Report a handle's type, rights and KOID.
    CapabilityQuery = 20,

    // ---- Observation -----------------------------------------------------
    /// Block until any of a set of objects asserts any of a set of signals.
    Wait = 21,
    /// Raise or lower user-defined signal bits on an object.
    ObjectSignal = 22,
    /// Read the monotonic clock, in nanoseconds since boot.
    Clock = 23,
    /// Write a diagnostic string to the kernel log.
    DebugLog = 24,
}

impl Syscall {
    /// Highest implemented syscall number.
    pub const MAX: u64 = Self::DebugLog as u64;

    /// Decode a raw syscall number.
    pub const fn from_u64(value: u64) -> Option<Self> {
        if value == 0 || value > Self::MAX {
            return None;
        }
        // SAFETY-free: the range check above guarantees a valid discriminant,
        // and the enum is `repr(u64)` with contiguous values 1..=MAX.
        Some(match value {
            1 => Self::ProcessCreate,
            2 => Self::ProcessStart,
            3 => Self::ProcessExit,
            4 => Self::ThreadCreate,
            5 => Self::ThreadExit,
            6 => Self::ThreadStart,
            7 => Self::Yield,
            8 => Self::MemoryAllocate,
            9 => Self::MemoryMap,
            10 => Self::MemoryUnmap,
            11 => Self::MemoryProtect,
            12 => Self::MemoryTransfer,
            13 => Self::ChannelCreate,
            14 => Self::ChannelSend,
            15 => Self::ChannelReceive,
            16 => Self::ChannelCall,
            17 => Self::CapabilityDuplicate,
            18 => Self::CapabilityRevoke,
            19 => Self::CapabilityClose,
            20 => Self::CapabilityQuery,
            21 => Self::Wait,
            22 => Self::ObjectSignal,
            23 => Self::Clock,
            _ => Self::DebugLog,
        })
    }

    /// Name, for tracing.
    pub const fn name(self) -> &'static str {
        match self {
            Self::Invalid => "invalid",
            Self::ProcessCreate => "process_create",
            Self::ProcessStart => "process_start",
            Self::ProcessExit => "process_exit",
            Self::ThreadCreate => "thread_create",
            Self::ThreadExit => "thread_exit",
            Self::ThreadStart => "thread_start",
            Self::Yield => "yield",
            Self::MemoryAllocate => "memory_allocate",
            Self::MemoryMap => "memory_map",
            Self::MemoryUnmap => "memory_unmap",
            Self::MemoryProtect => "memory_protect",
            Self::MemoryTransfer => "memory_transfer",
            Self::ChannelCreate => "channel_create",
            Self::ChannelSend => "channel_send",
            Self::ChannelReceive => "channel_receive",
            Self::ChannelCall => "channel_call",
            Self::CapabilityDuplicate => "capability_duplicate",
            Self::CapabilityRevoke => "capability_revoke",
            Self::CapabilityClose => "capability_close",
            Self::CapabilityQuery => "capability_query",
            Self::Wait => "wait",
            Self::ObjectSignal => "object_signal",
            Self::Clock => "clock",
            Self::DebugLog => "debug_log",
        }
    }
}

/// Pseudo-vector recorded in the trap frame for a `SYSCALL` entry, so that one
/// frame layout serves both interrupts and syscalls.
pub const SYSCALL_VECTOR: u64 = 0x100;
