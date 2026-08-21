//! Typed IPC messages.
//!
//! A NodeOS message is not a byte stream. It is a *typed record*:
//!
//! ```text
//! Message {
//!     kind         u32   what this message means to the protocol
//!     flags        u32   delivery options
//!     txid         u64   correlates a reply with its request
//!     payload      bytes protocol-defined body
//!     capabilities [..]  handles transferred with the message
//! }
//! ```
//!
//! The `kind` field is what makes the whole system legible: a channel is not
//! "a pipe carrying whatever", it is an endpoint speaking a protocol whose
//! message kinds are enumerated. Services validate `kind` before they look at a
//! byte of payload, and a message of an unknown kind is a protocol error rather
//! than a parsing adventure.
//!
//! Capabilities travel *with* the message and are transferred, not copied: the
//! sender's handle is consumed and the receiver's handle table gains an
//! equivalent one. That is the primitive on top of which everything else is
//! built -- opening a file is "send a request, receive a channel", granting
//! shared memory is "send a VMO handle".

use alloc::vec::Vec;

use crate::obj::Handle;

/// Largest payload a single message may carry.
///
/// Bigger transfers are the job of a shared VMO whose handle travels in the
/// message: copying megabytes through the kernel would make the IPC path's cost
/// unbounded, and unbounded kernel work is how a microkernel loses.
pub const MAX_PAYLOAD: usize = 64 * 1024;

/// Largest number of capabilities in a single message.
pub const MAX_HANDLES: usize = 64;

/// Maximum messages queued on one endpoint before senders block.
pub const MAX_QUEUE_DEPTH: usize = 256;

/// Maximum bytes queued on one endpoint before senders block.
pub const MAX_QUEUE_BYTES: usize = 512 * 1024;

/// Delivery options.
#[derive(Clone, Copy, PartialEq, Eq, Default)]
pub struct MessageFlags(pub u32);

impl MessageFlags {
    /// No options.
    pub const NONE: Self = Self(0);
    /// This message is a request expecting a reply with the same `txid`.
    pub const REQUEST: Self = Self(1 << 0);
    /// This message is a reply to the request with the same `txid`.
    pub const REPLY: Self = Self(1 << 1);
    /// The sender does not want to block if the queue is full; fail instead.
    pub const NON_BLOCKING: Self = Self(1 << 2);
    /// The payload names a shared VMO rather than carrying data inline.
    pub const SHARED_MEMORY: Self = Self(1 << 3);
    /// The message reports an error; the payload is a status code.
    pub const ERROR: Self = Self(1 << 4);

    /// Whether every bit of `other` is set.
    pub const fn contains(self, other: Self) -> bool {
        self.0 & other.0 == other.0
    }
    /// Union.
    pub const fn or(self, other: Self) -> Self {
        Self(self.0 | other.0)
    }
}

impl core::fmt::Debug for MessageFlags {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        const NAMES: [(u32, &str); 5] =
            [(1, "REQUEST"), (2, "REPLY"), (4, "NONBLOCK"), (8, "SHM"), (16, "ERROR")];
        let mut first = true;
        for (bit, name) in NAMES {
            if self.0 & bit != 0 {
                if !first {
                    f.write_str("|")?;
                }
                f.write_str(name)?;
                first = false;
            }
        }
        if first {
            f.write_str("none")?;
        }
        Ok(())
    }
}

/// The fixed-size part of a message, as it appears in the syscall ABI.
#[repr(C)]
#[derive(Clone, Copy, Debug, Default)]
pub struct MessageHeader {
    /// Protocol-defined message type.
    pub kind: u32,
    /// Delivery options.
    pub flags: u32,
    /// Request/reply correlation identifier.
    pub txid: u64,
    /// Payload length in bytes.
    pub payload_len: u32,
    /// Number of capabilities.
    pub handle_count: u32,
}

/// A message in flight inside the kernel.
pub struct Message {
    /// Protocol-defined message type.
    pub kind: u32,
    /// Delivery options.
    pub flags: MessageFlags,
    /// Request/reply correlation identifier.
    pub txid: u64,
    /// Payload bytes.
    pub payload: Vec<u8>,
    /// Capabilities transferred with this message.
    pub handles: Vec<Handle>,
}

impl Message {
    /// A message with no payload and no capabilities.
    pub fn empty(kind: u32) -> Self {
        Self { kind, flags: MessageFlags::NONE, txid: 0, payload: Vec::new(), handles: Vec::new() }
    }

    /// Total queue cost of this message, in bytes.
    pub fn cost(&self) -> usize {
        self.payload.len() + self.handles.len() * core::mem::size_of::<Handle>()
    }

    /// Whether this message is well formed.
    pub fn validate(&self) -> Result<(), MessageError> {
        if self.payload.len() > MAX_PAYLOAD {
            return Err(MessageError::PayloadTooLarge);
        }
        if self.handles.len() > MAX_HANDLES {
            return Err(MessageError::TooManyHandles);
        }
        // Every transferred capability must actually be transferable, and must
        // still be live: a revoked capability is not a gift, it is a trap.
        for handle in &self.handles {
            if !handle.rights().contains(crate::obj::Rights::TRANSFER) {
                return Err(MessageError::HandleNotTransferable);
            }
            if !handle.is_live() {
                return Err(MessageError::HandleRevoked);
            }
        }
        Ok(())
    }

    /// The header describing this message.
    pub fn header(&self) -> MessageHeader {
        MessageHeader {
            kind: self.kind,
            flags: self.flags.0,
            txid: self.txid,
            payload_len: self.payload.len() as u32,
            handle_count: self.handles.len() as u32,
        }
    }
}

/// Errors from message construction and delivery.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum MessageError {
    /// Payload exceeds [`MAX_PAYLOAD`].
    PayloadTooLarge,
    /// More than [`MAX_HANDLES`] capabilities.
    TooManyHandles,
    /// A capability lacks `TRANSFER`.
    HandleNotTransferable,
    /// A capability has been revoked.
    HandleRevoked,
}
