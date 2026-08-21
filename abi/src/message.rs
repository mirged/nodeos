//! Message layout as it crosses the syscall boundary.

use crate::types::HandleValue;

/// Largest inline payload.
pub const MAX_PAYLOAD: usize = 64 * 1024;
/// Largest number of capabilities in one message.
pub const MAX_HANDLES: usize = 64;

/// Delivery options.
#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
#[repr(transparent)]
pub struct MessageFlags(pub u32);

impl MessageFlags {
    /// No options.
    pub const NONE: Self = Self(0);
    /// A request expecting a reply with the same transaction id.
    pub const REQUEST: Self = Self(1 << 0);
    /// A reply to the request with the same transaction id.
    pub const REPLY: Self = Self(1 << 1);
    /// Fail instead of blocking.
    pub const NON_BLOCKING: Self = Self(1 << 2);
    /// The payload refers to shared memory rather than carrying data.
    pub const SHARED_MEMORY: Self = Self(1 << 3);
    /// The payload is an error status.
    pub const ERROR: Self = Self(1 << 4);

    /// Union.
    pub const fn or(self, other: Self) -> Self {
        Self(self.0 | other.0)
    }
    /// Whether every bit of `other` is set.
    pub const fn contains(self, other: Self) -> bool {
        self.0 & other.0 == other.0
    }
}

/// The message descriptor userspace passes to `channel_send`/`channel_receive`.
///
/// The kernel copies `payload_len` bytes from `payload` and transfers the
/// `handle_count` capabilities named by `handles`. On receive, the same
/// structure describes where to put them, and the kernel writes back the actual
/// lengths.
#[repr(C)]
#[derive(Clone, Copy, Debug, Default)]
pub struct MessageDescriptor {
    /// Protocol-defined message type.
    pub kind: u32,
    /// See [`MessageFlags`].
    pub flags: u32,
    /// Request/reply correlation identifier. Zero means "allocate one".
    pub txid: u64,
    /// Pointer to the payload bytes.
    pub payload: u64,
    /// On send: bytes to send. On receive: buffer capacity, overwritten with
    /// the number of bytes actually received.
    pub payload_len: u32,
    /// On send: capabilities to transfer. On receive: buffer capacity,
    /// overwritten with the number actually received.
    pub handle_count: u32,
    /// Pointer to an array of [`HandleValue`].
    pub handles: u64,
}

/// The fixed header of a message, for protocols that frame their own payloads.
#[repr(C)]
#[derive(Clone, Copy, Debug, Default)]
pub struct MessageHeader {
    /// Protocol-defined message type.
    pub kind: u32,
    /// See [`MessageFlags`].
    pub flags: u32,
    /// Correlation identifier.
    pub txid: u64,
    /// Payload length.
    pub payload_len: u32,
    /// Capability count.
    pub handle_count: u32,
}

/// The first message every process receives on its bootstrap channel.
///
/// A NodeOS process starts with exactly one handle: the bootstrap channel, in
/// `RDI`. Everything else -- its own process handle, its address space, its
/// namespace, the services it was granted -- arrives in this message. There is
/// no ambient way to obtain any of them, which is what makes "what can this
/// process do?" answerable by reading one message rather than auditing a
/// kernel.
#[repr(C)]
#[derive(Clone, Copy, Debug, Default)]
pub struct BootstrapMessage {
    /// ABI version of this structure.
    pub version: u32,
    /// Number of capability classes granted, as a bitmask.
    pub granted_caps: u32,
    /// Handle to the process itself.
    pub process: HandleValue,
    /// Handle to the process's address space.
    pub space: HandleValue,
    /// Handle to the initial thread.
    pub thread: HandleValue,
    /// Handle to the process's namespace directory channel, if any.
    pub namespace: HandleValue,
    /// Base address of the loaded program image.
    pub image_base: u64,
    /// Size of the initial heap the loader reserved.
    pub heap_size: u64,
    /// Number of `(name, handle)` pairs following this structure.
    pub extra_count: u32,
    /// Padding.
    pub _reserved: u32,
}

/// Message kind of the bootstrap message.
pub const KIND_BOOTSTRAP: u32 = 0x4E4F_4445;
