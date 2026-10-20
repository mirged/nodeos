//! `mmap` and `munmap`, over memory objects.
//!
//! Every mapping is a fresh, demand-paged VMO mapped into the heap window.
//! The VMO handle is closed as soon as it is mapped: the mapping keeps the
//! object alive, and unmapping it is then all it takes to free the memory.
//!
//! Two Linux behaviours are approximated, both documented in docs/MUSL.md
//! (K5): protections never narrow below read-write (`PROT_NONE` reservations
//! are mapped usable, and `mprotect` succeeds without changing anything), and
//! an `munmap` of part of a mapping leaves it mapped. musl's allocator does
//! neither in a way that matters: it unmaps what it mapped, whole.

use alloc::vec::Vec;

use libnode::sync::SpinLock;
use nodeos_abi::types::{MapFlags, MapWindow};
use nodeos_abi::Syscall;

use crate::errno::*;

const PAGE: u64 = 4096;

const PROT_WRITE: i32 = 0x2;
const PROT_EXEC: i32 = 0x4;
const MAP_FIXED: i32 = 0x10;
const MAP_ANONYMOUS: i32 = 0x20;

/// `(base, len)` of every mapping made here, so that `munmap` can tell a
/// whole mapping from part of one.
static REGIONS: SpinLock<Vec<(u64, u64)>> = SpinLock::new(Vec::new());

pub unsafe fn mmap(addr: u64, len: u64, prot: i32, flags: i32, fd: i32, offset: u64) -> i64 {
    if len == 0 || offset % PAGE != 0 {
        return -EINVAL;
    }
    let len = len.div_ceil(PAGE) * PAGE;

    // SAFETY: scalar arguments.
    let raw = unsafe { libnode::sys::syscall2(Syscall::MemoryAllocate, len, 0) };
    let vmo = match libnode::sys::status(raw) {
        Ok(value) => value as u64,
        Err(error) => return -from_error(error),
    };

    // A file mapping is a private copy of the file's bytes: correct for
    // `MAP_PRIVATE`, which is what programs reading a file this way use.
    if flags & MAP_ANONYMOUS == 0 {
        if let Err(errno) = crate::fs::fill_vmo(fd, vmo, offset, len) {
            close(vmo);
            return -errno;
        }
    }

    // W^X: an executable mapping is read-execute and nothing else. The bytes
    // were written through the object, not the mapping, so nothing is lost.
    let map_flags = if prot & PROT_EXEC != 0 {
        if prot & PROT_WRITE != 0 {
            close(vmo);
            return -EACCES;
        }
        MapFlags::READ.or(MapFlags::EXECUTE)
    } else {
        MapFlags::READ.or(MapFlags::WRITE)
    };
    let hint = if flags & MAP_FIXED != 0 { addr } else { 0 };
    let encoded = map_flags.0 as u64 | ((MapWindow::Heap as u64) << 16);
    // SAFETY: scalar arguments; the kernel validates the range and the hint.
    let raw = unsafe { libnode::sys::syscall6(Syscall::MemoryMap, 0, vmo, 0, len, encoded, hint) };
    close(vmo);
    match libnode::sys::status(raw) {
        Ok(base) => {
            REGIONS.lock().push((base as u64, len));
            base
        }
        Err(error) => -from_error(error),
    }
}

pub fn munmap(addr: u64, len: u64) -> i64 {
    let mut regions = REGIONS.lock();
    let Some(index) = regions.iter().position(|(base, size)| addr >= *base && addr < base + size) else {
        // Linux allows unmapping nothing.
        return 0;
    };
    let (base, size) = regions[index];
    if addr == base && len >= size {
        regions.remove(index);
        // SAFETY: scalar arguments.
        unsafe { libnode::sys::syscall2(Syscall::MemoryUnmap, 0, base) };
    }
    0
}

fn close(handle: u64) {
    // SAFETY: scalar argument.
    unsafe { libnode::sys::syscall1(Syscall::CapabilityClose, handle) };
}
