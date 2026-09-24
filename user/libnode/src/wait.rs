//! Waiting on more than one object at once.
//!
//! [`crate::Handle::wait`] covers the common single-object case. A service
//! that must answer either a channel message or a hardware event -- the
//! console, watching both its client channel and the keyboard -- needs to
//! block on both at once and find out which one woke it, which is what this
//! module is for.

use alloc::vec::Vec;

use nodeos_abi::{Error, HandleValue, Signals, Syscall, WaitItem};

use crate::sys;

/// Block until any of `items` (a handle paired with the signals wanted on it)
/// asserts one of its wanted signals, or `deadline_ns` passes (`0` means wait
/// forever). Returns the index into `items` that became ready and the signals
/// actually observed on it.
pub fn wait_any(items: &[(HandleValue, Signals)], deadline_ns: u64) -> Result<(usize, Signals), Error> {
    let mut raw: Vec<WaitItem> =
        items.iter().map(|(handle, wanted)| WaitItem { handle: *handle, wanted: wanted.0, observed: 0, _reserved: 0 }).collect();
    // SAFETY: `raw` is a live, correctly sized buffer of `WaitItem`s that the
    // kernel both reads and writes back into.
    let raw_result =
        unsafe { sys::syscall3(Syscall::Wait, raw.as_mut_ptr() as u64, raw.len() as u64, deadline_ns) };
    let index = sys::status(raw_result)? as usize;
    Ok((index, Signals(raw[index].observed)))
}
