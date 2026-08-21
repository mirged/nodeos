//! Wire protocols for the standard NodeOS services.
//!
//! A NodeOS "system call" for anything above the nine kernel primitives is a
//! message on a channel. That means the protocols *are* the operating system's
//! interface, and they belong in the ABI crate alongside the syscall numbers
//! rather than in whichever service happens to implement them today.
//!
//! Every protocol follows the same shape:
//!
//! * request kinds are `0xNNNN`, replies are the request kind with `0x8000`
//!   set, so a reply is always recognisable without tracking state;
//! * a failed reply sets [`crate::message::MessageFlags::ERROR`] and carries a
//!   little-endian `i32` status in the payload;
//! * capabilities travel in the message's handle array, never encoded in the
//!   payload -- there is no such thing as a "file descriptor number" that means
//!   something to another process.

/// Turn a request kind into its reply kind.
pub const fn reply_kind(request: u32) -> u32 {
    request | 0x8000
}

/// Encode an error status as a reply payload.
pub fn encode_error(status: crate::Error) -> [u8; 4] {
    (status.to_raw() as i32).to_le_bytes()
}

/// Decode an error status from a reply payload.
pub fn decode_error(payload: &[u8]) -> Option<crate::Error> {
    if payload.len() < 4 {
        return None;
    }
    let raw = i32::from_le_bytes([payload[0], payload[1], payload[2], payload[3]]) as i64;
    crate::Error::from_raw(raw)
}

// ---------------------------------------------------------------------------
// Launcher -> service
// ---------------------------------------------------------------------------

/// A launcher introducing a new client to a service. The client's server-side
/// channel endpoint travels in the handle array.
pub const CONNECT: u32 = 0x0101;

// ---------------------------------------------------------------------------
// Log service
// ---------------------------------------------------------------------------

/// Log a line. Payload: one severity byte followed by UTF-8 text.
pub const LOG_WRITE: u32 = 0x1001;
/// Ask the log service for its statistics.
pub const LOG_STATS: u32 = 0x1002;

/// Severity levels understood by the log service.
pub mod log_level {
    /// Something failed.
    pub const ERROR: u8 = 0;
    /// Something unexpected but handled.
    pub const WARN: u8 = 1;
    /// Normal progress.
    pub const INFO: u8 = 2;
    /// Detail.
    pub const DEBUG: u8 = 3;
}

// ---------------------------------------------------------------------------
// Service registry
// ---------------------------------------------------------------------------

/// Publish a service under a name. Handle array: the service's listening
/// endpoint.
pub const REGISTRY_PUBLISH: u32 = 0x2001;
/// Look up a service by name. Reply handle array: a fresh connection to it.
pub const REGISTRY_LOOKUP: u32 = 0x2002;
/// List the names visible to this client.
pub const REGISTRY_LIST: u32 = 0x2003;

// ---------------------------------------------------------------------------
// Filesystem
// ---------------------------------------------------------------------------

/// Create a file. Payload: path.
pub const FS_CREATE: u32 = 0x3001;
/// Open an existing node. Payload: path. Reply payload: [`FsOpenReply`].
pub const FS_OPEN: u32 = 0x3002;
/// Read from a node. Payload: [`FsReadRequest`]. Reply payload: the bytes.
pub const FS_READ: u32 = 0x3003;
/// Write to a node. Payload: [`FsWriteHeader`] followed by the bytes.
pub const FS_WRITE: u32 = 0x3004;
/// Stat a node. Payload: path. Reply payload: [`FsStat`].
pub const FS_STAT: u32 = 0x3005;
/// List a directory. Payload: path. Reply payload: NUL-separated names.
pub const FS_LIST: u32 = 0x3006;
/// Remove a node. Payload: path.
pub const FS_UNLINK: u32 = 0x3007;
/// Obtain a memory object for a file's contents, for zero-copy access.
/// Payload: path. Reply handle array: the memory object.
pub const FS_GET_VMO: u32 = 0x3008;
/// Create a directory. Payload: path.
pub const FS_MKDIR: u32 = 0x3009;
/// Report filesystem statistics.
pub const FS_STATFS: u32 = 0x300A;

/// Node kinds.
pub mod fs_kind {
    /// A regular file.
    pub const FILE: u32 = 1;
    /// A directory.
    pub const DIRECTORY: u32 = 2;
}

/// Reply to [`FS_OPEN`].
#[repr(C)]
#[derive(Clone, Copy, Debug, Default)]
pub struct FsOpenReply {
    /// Stable object identifier within the filesystem.
    pub node: u64,
    /// Size in bytes.
    pub size: u64,
    /// See [`fs_kind`].
    pub kind: u32,
    /// Padding.
    pub _reserved: u32,
}

/// Payload of [`FS_READ`].
#[repr(C)]
#[derive(Clone, Copy, Debug, Default)]
pub struct FsReadRequest {
    /// Node identifier from [`FS_OPEN`].
    pub node: u64,
    /// Byte offset.
    pub offset: u64,
    /// Bytes requested.
    pub length: u32,
    /// Padding.
    pub _reserved: u32,
}

/// Header of a [`FS_WRITE`] payload; the bytes to write follow it.
#[repr(C)]
#[derive(Clone, Copy, Debug, Default)]
pub struct FsWriteHeader {
    /// Node identifier from [`FS_OPEN`].
    pub node: u64,
    /// Byte offset.
    pub offset: u64,
    /// Bytes to write.
    pub length: u32,
    /// Padding.
    pub _reserved: u32,
}

/// Reply to [`FS_STAT`].
#[repr(C)]
#[derive(Clone, Copy, Debug, Default)]
pub struct FsStat {
    /// Stable object identifier.
    pub node: u64,
    /// Size in bytes.
    pub size: u64,
    /// Number of entries, for a directory.
    pub entries: u32,
    /// See [`fs_kind`].
    pub kind: u32,
    /// Creation time, nanoseconds since boot.
    pub created_ns: u64,
    /// Last modification time, nanoseconds since boot.
    pub modified_ns: u64,
}

/// Reply to [`FS_STATFS`].
#[repr(C)]
#[derive(Clone, Copy, Debug, Default)]
pub struct FsStatFs {
    /// Total nodes.
    pub nodes: u64,
    /// Total bytes stored.
    pub bytes: u64,
    /// Bytes of memory objects committed.
    pub committed: u64,
    /// Requests served since boot.
    pub requests: u64,
}
