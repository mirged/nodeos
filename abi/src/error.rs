//! Status codes.
//!
//! There is no `errno`. A syscall returns its status in `RAX`: values `>= 0`
//! are results, values `< 0` are the negation of an [`Error`]. Thread-local
//! error state is a hazard that NodeOS simply declines to have.

/// A syscall failure reason.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
#[repr(i64)]
pub enum Error {
    /// The syscall number is not implemented.
    NoSyscall = 1,
    /// An argument was malformed or out of range.
    InvalidArgument = 2,
    /// A handle value does not name a live handle.
    BadHandle = 3,
    /// The handle exists but lacks a required right.
    AccessDenied = 4,
    /// The handle names an object of the wrong type.
    WrongType = 5,
    /// The authority behind the handle was revoked.
    Revoked = 6,
    /// Out of memory.
    NoMemory = 7,
    /// The handle table, address space, or queue is full.
    NoSpace = 8,
    /// A user pointer was not mapped, or not mapped with the needed access.
    BadAddress = 9,
    /// The far end of a channel is gone.
    PeerClosed = 10,
    /// A non-blocking operation would have blocked (nothing to receive).
    ShouldWait = 11,
    /// A non-blocking send found the queue full.
    ShouldRetry = 12,
    /// The deadline passed before the operation completed.
    TimedOut = 13,
    /// The operation is not supported for this object.
    NotSupported = 14,
    /// The object is in the wrong state for this operation.
    BadState = 15,
    /// The requested name was not found in the namespace.
    NotFound = 16,
    /// The range overlaps an existing mapping.
    AlreadyExists = 17,
    /// The operation would widen authority.
    WouldEscalate = 18,
    /// The executable image is malformed.
    BadImage = 19,
    /// The image requires capabilities the launcher did not grant.
    CapabilityDenied = 20,
    /// The operation was cancelled.
    Cancelled = 21,
    /// A resource limit was exceeded.
    LimitExceeded = 22,
}

impl Error {
    /// The value a failing syscall returns in `RAX`.
    pub const fn to_raw(self) -> i64 {
        -(self as i64)
    }

    /// Decode a raw `RAX` value, if it is an error.
    pub const fn from_raw(raw: i64) -> Option<Self> {
        if raw >= 0 {
            return None;
        }
        Some(match -raw {
            1 => Self::NoSyscall,
            2 => Self::InvalidArgument,
            3 => Self::BadHandle,
            4 => Self::AccessDenied,
            5 => Self::WrongType,
            6 => Self::Revoked,
            7 => Self::NoMemory,
            8 => Self::NoSpace,
            9 => Self::BadAddress,
            10 => Self::PeerClosed,
            11 => Self::ShouldWait,
            12 => Self::ShouldRetry,
            13 => Self::TimedOut,
            14 => Self::NotSupported,
            15 => Self::BadState,
            16 => Self::NotFound,
            17 => Self::AlreadyExists,
            18 => Self::WouldEscalate,
            19 => Self::BadImage,
            20 => Self::CapabilityDenied,
            21 => Self::Cancelled,
            _ => Self::LimitExceeded,
        })
    }

    /// A short, stable description.
    pub const fn message(self) -> &'static str {
        match self {
            Self::NoSyscall => "no such syscall",
            Self::InvalidArgument => "invalid argument",
            Self::BadHandle => "bad handle",
            Self::AccessDenied => "access denied: handle lacks the required right",
            Self::WrongType => "handle names the wrong kind of object",
            Self::Revoked => "capability revoked",
            Self::NoMemory => "out of memory",
            Self::NoSpace => "no space",
            Self::BadAddress => "bad address",
            Self::PeerClosed => "peer closed",
            Self::ShouldWait => "would block",
            Self::ShouldRetry => "queue full",
            Self::TimedOut => "timed out",
            Self::NotSupported => "not supported",
            Self::BadState => "bad state",
            Self::NotFound => "not found",
            Self::AlreadyExists => "already exists",
            Self::WouldEscalate => "operation would widen authority",
            Self::BadImage => "malformed executable image",
            Self::CapabilityDenied => "required capability not granted",
            Self::Cancelled => "cancelled",
            Self::LimitExceeded => "limit exceeded",
        }
    }
}

/// The result of a syscall: a non-negative value, or an [`Error`].
pub type Status = Result<i64, Error>;

/// Decode a raw `RAX` value into a [`Status`].
pub const fn decode(raw: i64) -> Status {
    match Error::from_raw(raw) {
        Some(error) => Err(error),
        None => Ok(raw),
    }
}
