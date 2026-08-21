//! Creating and starting processes.

use nodeos_abi::{Error, HandleValue, Syscall};

use crate::{sys, Handle};

/// A process this program created.
pub struct Process {
    handle: Handle,
}

impl Process {
    /// Create an empty process, requesting `caps` for it.
    ///
    /// The kernel intersects `caps` with what the *caller* holds, so a process
    /// can never manufacture authority it does not have for a child.
    pub fn create(name: &str, caps: u32) -> Result<Self, Error> {
        // SAFETY: `name` is a valid readable slice of the declared length.
        let raw = unsafe {
            sys::syscall3(Syscall::ProcessCreate, name.as_ptr() as u64, name.len() as u64, caps as u64)
        };
        let value = sys::status(raw)?;
        Ok(Self { handle: Handle::from_raw(HandleValue(value as u32)) })
    }

    /// The process capability.
    pub fn handle(&self) -> &Handle {
        &self.handle
    }

    /// Start the process's first thread, moving `bootstrap` into it.
    pub fn start(&self, entry: u64, stack: u64, bootstrap: Handle) -> Result<(), Error> {
        let value = bootstrap.into_raw();
        // SAFETY: all four arguments are scalars.
        let raw = unsafe {
            sys::syscall4(
                Syscall::ProcessStart,
                self.handle.raw().0 as u64,
                entry,
                stack,
                value.0 as u64,
            )
        };
        sys::ok(raw)
    }
}
