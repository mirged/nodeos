//! Accessing user memory from the kernel.
//!
//! # The rule
//!
//! A user pointer is *data*, never a pointer, until it has been validated. The
//! kernel executes syscalls in the calling process's address space, so a bare
//! dereference of a user pointer would work -- which is exactly why every
//! access has to go through this module instead.
//!
//! Validation is three checks and one action:
//!
//! 1. the range is canonical, and lies entirely in the user half (so a user
//!    pointer can never name kernel memory, however it is arithmetically
//!    constructed);
//! 2. it does not wrap;
//! 3. every page it touches is covered by a region with at least the requested
//!    permissions;
//! 4. every such page is faulted in, so the copy itself cannot fault.
//!
//! Step 4 is what lets the copy be a plain `copy_nonoverlapping` with no fixup
//! table: by the time it runs, the pages are present.

use alloc::vec::Vec;
use nodeos_abi::Error;

use crate::arch::PAGE_SIZE;

use super::layout;

/// Largest single user copy the kernel will perform, so a bad length cannot
/// turn into an unbounded kernel loop.
pub const MAX_COPY: usize = 1024 * 1024;

/// Validate a user range and force its pages in.
///
/// `write` requests write access; otherwise read access is required.
pub fn validate(addr: u64, len: usize, write: bool) -> Result<(), Error> {
    if len == 0 {
        return Ok(());
    }
    if len > MAX_COPY {
        return Err(Error::InvalidArgument);
    }
    if !layout::is_user_range(addr, len as u64) || !layout::is_canonical(addr) {
        return Err(Error::BadAddress);
    }

    let process = crate::sched::current_process().ok_or(Error::BadState)?;
    let space = process.space();

    let first = addr & !(PAGE_SIZE as u64 - 1);
    let last = (addr + len as u64 - 1) & !(PAGE_SIZE as u64 - 1);
    let mut page = first;
    loop {
        let region = space.region_at(page).ok_or(Error::BadAddress)?;
        let needed = if write { super::space::MapFlags::WRITE } else { super::space::MapFlags::READ };
        if !region.flags.contains(needed) {
            return Err(Error::AccessDenied);
        }
        // Force the page in now so the copy below cannot fault.
        if space.mapper().translate(page).is_none() && !space.handle_fault(page, write, false) {
            return Err(Error::BadAddress);
        }
        // A copy-on-write page that is about to be written must be un-shared
        // here, not lazily inside the copy.
        if write {
            if let Some((_, flags)) = space.mapper().translate(page) {
                if !flags.contains(super::paging::PageFlags::WRITABLE) && !space.handle_fault(page, true, false) {
                    return Err(Error::AccessDenied);
                }
            }
        }
        if page == last {
            break;
        }
        page += PAGE_SIZE as u64;
    }
    Ok(())
}

/// Copy `len` bytes from user memory into a fresh buffer.
pub fn read_bytes(addr: u64, len: usize) -> Result<Vec<u8>, Error> {
    validate(addr, len, false)?;
    let mut buffer = Vec::new();
    buffer.try_reserve_exact(len).map_err(|_| Error::NoMemory)?;
    buffer.resize(len, 0);
    if len > 0 {
        // SAFETY: `validate` proved the whole range is user memory, mapped
        // readable and present, and `buffer` is a distinct kernel allocation.
        unsafe { core::ptr::copy_nonoverlapping(addr as *const u8, buffer.as_mut_ptr(), len) };
    }
    Ok(buffer)
}

/// Copy bytes from the kernel into user memory.
pub fn write_bytes(addr: u64, data: &[u8]) -> Result<(), Error> {
    validate(addr, data.len(), true)?;
    if !data.is_empty() {
        // SAFETY: `validate` proved the range is user memory, mapped writable
        // and present; the regions cannot overlap because `data` is a kernel
        // allocation and the destination is in the user half.
        unsafe { core::ptr::copy_nonoverlapping(data.as_ptr(), addr as *mut u8, data.len()) };
    }
    Ok(())
}

/// Read a plain-old-data structure from user memory.
pub fn read_struct<T: Copy>(addr: u64) -> Result<T, Error> {
    validate(addr, core::mem::size_of::<T>(), false)?;
    // SAFETY: `validate` proved `size_of::<T>()` readable bytes are present.
    // The read is unaligned because userspace chooses the address, and `T` is
    // constrained to `Copy` plain data by every call site in the syscall layer.
    Ok(unsafe { (addr as *const T).read_unaligned() })
}

/// Write a plain-old-data structure to user memory.
pub fn write_struct<T: Copy>(addr: u64, value: &T) -> Result<(), Error> {
    validate(addr, core::mem::size_of::<T>(), true)?;
    // SAFETY: `validate` proved `size_of::<T>()` writable bytes are present.
    unsafe { (addr as *mut T).write_unaligned(*value) };
    Ok(())
}

/// Read an array of plain-old-data structures from user memory.
pub fn read_array<T: Copy + Default>(addr: u64, count: usize) -> Result<Vec<T>, Error> {
    let bytes = count.checked_mul(core::mem::size_of::<T>()).ok_or(Error::InvalidArgument)?;
    validate(addr, bytes, false)?;
    let mut out = Vec::new();
    out.try_reserve_exact(count).map_err(|_| Error::NoMemory)?;
    for i in 0..count {
        // SAFETY: `validate` covered `count * size_of::<T>()` readable bytes
        // starting at `addr`.
        out.push(unsafe { ((addr as *const T).add(i)).read_unaligned() });
    }
    Ok(out)
}

/// Write an array of plain-old-data structures to user memory.
pub fn write_array<T: Copy>(addr: u64, values: &[T]) -> Result<(), Error> {
    let bytes = values.len().checked_mul(core::mem::size_of::<T>()).ok_or(Error::InvalidArgument)?;
    validate(addr, bytes, true)?;
    for (i, value) in values.iter().enumerate() {
        // SAFETY: `validate` covered the whole array's worth of writable bytes.
        unsafe { ((addr as *mut T).add(i)).write_unaligned(*value) };
    }
    Ok(())
}

/// Read a UTF-8 string of exactly `len` bytes from user memory.
pub fn read_str(addr: u64, len: usize) -> Result<alloc::string::String, Error> {
    const MAX_STRING: usize = 4096;
    if len > MAX_STRING {
        return Err(Error::InvalidArgument);
    }
    let bytes = read_bytes(addr, len)?;
    alloc::string::String::from_utf8(bytes).map_err(|_| Error::InvalidArgument)
}
