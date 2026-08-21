//! # The NodeOS application binary interface
//!
//! This crate is the complete contract between the kernel and userspace. It is
//! `#![no_std]` and dependency-free so that the kernel, the userspace runtime
//! and host tools can all share exactly one definition of every number that
//! crosses the boundary.
//!
//! ## What is deliberately absent
//!
//! There is no `open`, no `read`, no `write`, no `socket`, no `fork`, no
//! `ioctl`, no `signal`, no file descriptor, no current working directory, no
//! `errno`. Those are not omissions to be filled in later -- they are the
//! design. Files, sockets and terminals are *protocols spoken over channels* to
//! userspace services; the kernel does not know they exist.
//!
//! What remains is a set of mechanisms with no policy in them:
//!
//! ```text
//!   address spaces      memory_allocate / memory_map / memory_unmap
//!   execution           process_create / process_start / process_exit
//!                       thread_create / thread_exit / yield
//!   communication       channel_create / channel_send / channel_receive
//!                       channel_call
//!   authority           capability_duplicate / capability_revoke
//!                       capability_close / capability_query
//!   observation         wait / clock / object_signal / object_info
//! ```
//!
//! ## Register convention
//!
//! `SYSCALL` is the entry instruction. The number is in `RAX`; arguments are in
//! `RDI`, `RSI`, `RDX`, `R10`, `R8`, `R9` (not `RCX`, which the instruction
//! overwrites with the return address, and not `R11`, which takes `RFLAGS`).
//! `RAX` returns a status: negative values are [`Error`] codes, non-negative
//! values are results. `RDX` carries a second result where one is needed.

#![no_std]
#![deny(missing_docs)]

pub mod error;
pub mod message;
pub mod nxf;
pub mod proto;
pub mod syscall;
pub mod types;

pub use error::{decode as decode_status, Error, Status};
pub use message::{MessageFlags, MessageHeader};
pub use syscall::Syscall;
pub use types::{CapClass, HandleValue, Rights, SchedClass, Signals, WaitItem};
