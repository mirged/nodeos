//! Linux `errno` values, and the mapping from NodeOS errors onto them.

use nodeos_abi::Error;

pub const ENOENT: i64 = 2;
pub const EIO: i64 = 5;
pub const EBADF: i64 = 9;
pub const ENOMEM: i64 = 12;
pub const EACCES: i64 = 13;
pub const EFAULT: i64 = 14;
pub const EEXIST: i64 = 17;
pub const ENOTDIR: i64 = 20;
pub const EISDIR: i64 = 21;
pub const EINVAL: i64 = 22;
pub const EMFILE: i64 = 24;
pub const ENOTTY: i64 = 25;
pub const ENOSPC: i64 = 28;
pub const ESPIPE: i64 = 29;
pub const EPIPE: i64 = 32;
pub const ERANGE: i64 = 34;
pub const ENOSYS: i64 = 38;
pub const ENAMETOOLONG: i64 = 36;

/// The `errno` a Linux program expects for a NodeOS failure.
pub fn from_error(error: Error) -> i64 {
    match error {
        Error::NotFound => ENOENT,
        Error::AlreadyExists => EEXIST,
        Error::NoMemory => ENOMEM,
        Error::NoSpace => ENOSPC,
        Error::InvalidArgument => EINVAL,
        Error::BadHandle => EBADF,
        Error::AccessDenied | Error::CapabilityDenied | Error::WouldEscalate => EACCES,
        Error::BadAddress => EFAULT,
        Error::PeerClosed => EPIPE,
        Error::NoSyscall | Error::NotSupported => ENOSYS,
        _ => EIO,
    }
}
