//! Inter-process communication.
//!
//! IPC is the *only* way a NodeOS process reaches anything outside its own
//! address space. There is no shared filesystem namespace to fall back on, no
//! signals, no shared globals -- if you cannot name it with a handle you cannot
//! touch it, and the only handles you have are the ones you were given or
//! derived from them.
//!
//! Two transports:
//!
//! * [`channel`] -- typed messages with capability transfer. Bounded size,
//!   bounded queue depth, copied by the kernel. Use for control.
//! * A [`crate::mm::vmo::Vmo`] handle sent over a channel -- explicitly granted
//!   shared memory, zero copy after the grant. Use for bulk data.
//!
//! Keeping bulk data out of the message path is deliberate: it puts a hard
//! bound on the work the kernel does per IPC operation, which is what lets the
//! IPC path stay fast and analysable.

pub mod channel;
pub mod message;

pub use channel::{Channel, ChannelError};
pub use message::{Message, MessageFlags, MessageHeader};
