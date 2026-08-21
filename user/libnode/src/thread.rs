//! Threads.

use nodeos_abi::types::ThreadParams;
use nodeos_abi::{Error, HandleValue, Syscall};

use crate::{sys, Handle};

/// A thread this program created.
pub struct Thread {
    handle: Handle,
}

impl Thread {
    /// Create a thread in the process named by `process`.
    pub fn create(process: &Handle, entry: u64, stack: u64, params: &ThreadParams) -> Result<Self, Error> {
        // SAFETY: `params` is a valid readable structure of the expected type.
        let raw = unsafe {
            sys::syscall4(
                Syscall::ThreadCreate,
                process.raw().0 as u64,
                entry,
                stack,
                params as *const _ as u64,
            )
        };
        let value = sys::status(raw)?;
        Ok(Self { handle: Handle::from_raw(HandleValue(value as u32)) })
    }

    /// The thread capability.
    pub fn handle(&self) -> &Handle {
        &self.handle
    }
}

/// Terminate the calling thread.
pub fn exit(code: i32) -> ! {
    // SAFETY: `thread_exit` never returns.
    unsafe { sys::syscall1(Syscall::ThreadExit, code as u64) };
    loop {
        core::hint::spin_loop();
    }
}
