//! # libnode -- the NodeOS userspace runtime
//!
//! There is no libc here, and not much of one is missing. A NodeOS program
//! does not open files, fork, or install signal handlers; it receives
//! capabilities, sends typed messages, and maps memory objects. This crate is
//! the thin layer that makes those three things ergonomic in Rust.
//!
//! ## Starting up
//!
//! A process begins at `_start` with exactly one thing: a bootstrap channel
//! handle in `RDI`. The runtime maps a heap, reads the bootstrap message, and
//! calls the program's `main`. Everything the program can do afterwards came
//! out of that one message.
//!
//! ```ignore
//! #![no_std]
//! #![no_main]
//!
//! libnode::main! {
//!     fn main(env: libnode::Env) -> i32 {
//!         libnode::println!("hello from pid {}", env.pid());
//!         0
//!     }
//! }
//! ```

#![no_std]
#![deny(missing_docs)]
#![deny(unsafe_op_in_unsafe_fn)]

extern crate alloc;

pub mod alloc_impl;
pub mod channel;
pub mod device;
pub mod env;
pub mod io;
pub mod mem;
pub mod process;
pub mod rt;
pub mod start;
pub mod sync;
pub mod sys;
pub mod thread;
pub mod wait;

pub use channel::{Channel, Message};
pub use env::Env;
pub use nodeos_abi as abi;
pub use nodeos_abi::{Error, HandleValue};

/// A capability held by this process.
///
/// Wrapping the raw value gives us a `Drop` that closes the handle, which is
/// the userspace half of "authority is an object you can lose".
#[derive(Debug)]
pub struct Handle(HandleValue);

impl Handle {
    /// Wrap a raw handle value, taking ownership.
    pub const fn from_raw(value: HandleValue) -> Self {
        Self(value)
    }

    /// The raw value.
    pub const fn raw(&self) -> HandleValue {
        self.0
    }

    /// Give up ownership without closing.
    pub fn into_raw(self) -> HandleValue {
        let value = self.0;
        core::mem::forget(self);
        value
    }

    /// Whether this is the invalid handle.
    pub const fn is_invalid(&self) -> bool {
        self.0.is_invalid()
    }

    /// Duplicate with reduced rights.
    pub fn duplicate(&self, rights: nodeos_abi::Rights) -> Result<Handle, Error> {
        // SAFETY: `capability_duplicate` takes a handle value and a rights
        // mask; neither is a pointer.
        let raw = unsafe {
            sys::syscall2(nodeos_abi::Syscall::CapabilityDuplicate, self.0 .0 as u64, rights.0 as u64)
        };
        sys::status(raw).map(|value| Handle(HandleValue(value as u32)))
    }

    /// Revoke this grant and everything derived from it, anywhere.
    pub fn revoke(&self) -> Result<(), Error> {
        // SAFETY: takes a handle value only.
        sys::ok(unsafe { sys::syscall1(nodeos_abi::Syscall::CapabilityRevoke, self.0 .0 as u64) })
    }

    /// Block until any of `wanted`'s signals is asserted on this object, or
    /// `deadline_ns` passes (`0` means wait forever). Returns the signals
    /// actually observed.
    pub fn wait(&self, wanted: nodeos_abi::Signals, deadline_ns: u64) -> Result<nodeos_abi::Signals, Error> {
        let mut item = nodeos_abi::WaitItem { handle: self.0, wanted: wanted.0, observed: 0, _reserved: 0 };
        // SAFETY: `&mut item` is a single, correctly sized, writable `WaitItem`.
        let raw = unsafe {
            sys::syscall3(nodeos_abi::Syscall::Wait, &mut item as *mut _ as u64, 1, deadline_ns)
        };
        sys::status(raw).map(|_| nodeos_abi::Signals(item.observed))
    }

    /// Ask the kernel what this handle is.
    pub fn query(&self) -> Result<nodeos_abi::types::HandleInfo, Error> {
        let mut info = nodeos_abi::types::HandleInfo::default();
        // SAFETY: `&mut info` is a valid, writable, correctly sized buffer.
        let raw = unsafe {
            sys::syscall2(
                nodeos_abi::Syscall::CapabilityQuery,
                self.0 .0 as u64,
                &mut info as *mut _ as u64,
            )
        };
        sys::ok(raw).map(|()| info)
    }
}

impl Drop for Handle {
    fn drop(&mut self) {
        if !self.0.is_invalid() {
            // SAFETY: takes a handle value only; failure is not actionable
            // during a drop.
            unsafe { sys::syscall1(nodeos_abi::Syscall::CapabilityClose, self.0 .0 as u64) };
        }
    }
}

/// Read the monotonic clock, in nanoseconds since boot.
pub fn clock_ns() -> u64 {
    // SAFETY: takes no arguments and cannot fail.
    unsafe { sys::syscall0(nodeos_abi::Syscall::Clock) as u64 }
}

/// Give up the rest of this thread's timeslice.
pub fn yield_now() {
    // SAFETY: takes no arguments.
    unsafe { sys::syscall0(nodeos_abi::Syscall::Yield) };
}

/// Terminate the calling process.
pub fn exit(code: i32) -> ! {
    // SAFETY: `process_exit` never returns.
    unsafe { sys::syscall1(nodeos_abi::Syscall::ProcessExit, code as u64) };
    // The kernel does not return from `process_exit`; if it somehow did,
    // spinning is preferable to running off the end of `main`.
    loop {
        core::hint::spin_loop();
    }
}

/// Wait for signals on a set of objects.
///
/// Returns the index of the first item whose wanted signals were asserted.
pub fn wait(items: &mut [nodeos_abi::WaitItem], deadline_ns: u64) -> Result<usize, Error> {
    // SAFETY: `items` is a valid, writable slice of the declared length.
    let raw = unsafe {
        sys::syscall3(
            nodeos_abi::Syscall::Wait,
            items.as_mut_ptr() as u64,
            items.len() as u64,
            deadline_ns,
        )
    };
    sys::status(raw).map(|index| index as usize)
}
