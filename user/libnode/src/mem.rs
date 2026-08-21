//! Memory objects and mappings.

use nodeos_abi::types::{MapFlags, MapWindow};
use nodeos_abi::{Error, HandleValue, Syscall};

use crate::{sys, Handle};

/// A memory object.
pub struct Vmo {
    handle: Handle,
    size: u64,
}

impl Vmo {
    /// Create an anonymous, zero-filled memory object.
    pub fn create(size: u64, resizable: bool) -> Result<Self, Error> {
        // SAFETY: both arguments are scalars.
        let raw = unsafe { sys::syscall2(Syscall::MemoryAllocate, size, resizable as u64) };
        let value = sys::status(raw)?;
        Ok(Self { handle: Handle::from_raw(HandleValue(value as u32)), size })
    }

    /// Adopt an existing memory object handle.
    pub fn from_handle(handle: Handle, size: u64) -> Self {
        Self { handle, size }
    }

    /// The underlying capability.
    pub fn handle(&self) -> &Handle {
        &self.handle
    }

    /// Size in bytes.
    pub fn size(&self) -> u64 {
        self.size
    }

    /// Map this object into the calling process's address space.
    pub fn map(&self, offset: u64, len: u64, flags: MapFlags, window: MapWindow) -> Result<Mapping, Error> {
        let encoded = flags.0 as u64 | ((window as u64) << 16);
        // SAFETY: all arguments are scalars; the kernel validates the range.
        let raw = unsafe {
            sys::syscall6(
                Syscall::MemoryMap,
                0, // the calling process's own address space
                self.handle.raw().0 as u64,
                offset,
                len,
                encoded,
                0,
            )
        };
        let base = sys::status(raw)? as u64;
        Ok(Mapping { base, len })
    }

    /// Copy bytes into the object without mapping it.
    pub fn write(&self, offset: u64, data: &[u8]) -> Result<usize, Error> {
        // SAFETY: `data` is a valid readable slice of the declared length.
        let raw = unsafe {
            sys::syscall6(
                Syscall::MemoryTransfer,
                self.handle.raw().0 as u64,
                offset,
                data.as_ptr() as u64,
                data.len() as u64,
                1,
                0,
            )
        };
        sys::status(raw).map(|n| n as usize)
    }

    /// Copy bytes out of the object without mapping it.
    pub fn read(&self, offset: u64, out: &mut [u8]) -> Result<usize, Error> {
        // SAFETY: `out` is a valid writable slice of the declared length.
        let raw = unsafe {
            sys::syscall6(
                Syscall::MemoryTransfer,
                self.handle.raw().0 as u64,
                offset,
                out.as_mut_ptr() as u64,
                out.len() as u64,
                0,
                0,
            )
        };
        sys::status(raw).map(|n| n as usize)
    }
}

/// A live mapping of a memory object.
pub struct Mapping {
    base: u64,
    len: u64,
}

impl Mapping {
    /// Base address.
    pub fn base(&self) -> u64 {
        self.base
    }

    /// Length in bytes.
    pub fn len(&self) -> u64 {
        self.len
    }

    /// Whether the mapping is empty.
    pub fn is_empty(&self) -> bool {
        self.len == 0
    }

    /// Borrow the mapping as a byte slice.
    ///
    /// # Safety
    /// The caller must ensure nothing else is concurrently writing the range,
    /// which for a shared mapping means coordinating with the other holders.
    pub unsafe fn as_slice(&self) -> &[u8] {
        // SAFETY: the kernel mapped `len` readable bytes at `base`; the caller
        // guarantees the absence of concurrent writers.
        unsafe { core::slice::from_raw_parts(self.base as *const u8, self.len as usize) }
    }

    /// Borrow the mapping as a mutable byte slice.
    ///
    /// # Safety
    /// As [`Mapping::as_slice`], and the mapping must be writable.
    pub unsafe fn as_mut_slice(&mut self) -> &mut [u8] {
        // SAFETY: as above, plus the caller asserts write permission.
        unsafe { core::slice::from_raw_parts_mut(self.base as *mut u8, self.len as usize) }
    }
}

impl Drop for Mapping {
    fn drop(&mut self) {
        // SAFETY: both arguments are scalars; failure is not actionable here.
        unsafe { sys::syscall2(Syscall::MemoryUnmap, 0, self.base) };
    }
}
