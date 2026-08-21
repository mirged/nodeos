//! Diagnostic output.
//!
//! `println!` here goes to the kernel log, which is the one output path that
//! exists before any service does. A program with a console capability should
//! use that instead; this is for bring-up and for reporting failures that
//! happen when the console is exactly what is broken.

use core::fmt::{self, Write};

use nodeos_abi::Syscall;

use crate::sys;

/// Writer that forwards to the kernel log.
pub struct DebugLog;

impl Write for DebugLog {
    fn write_str(&mut self, s: &str) -> fmt::Result {
        // SAFETY: `s` is a valid readable slice of the declared length.
        unsafe { sys::syscall2(Syscall::DebugLog, s.as_ptr() as u64, s.len() as u64) };
        Ok(())
    }
}

#[doc(hidden)]
pub fn _print(args: fmt::Arguments<'_>) {
    // A stack buffer keeps the whole line in one syscall, so concurrent writers
    // interleave by line rather than by fragment.
    let mut buffer = LineBuffer::new();
    let _ = buffer.write_fmt(args);
    buffer.flush();
}

const LINE_CAPACITY: usize = 512;

struct LineBuffer {
    bytes: [u8; LINE_CAPACITY],
    len: usize,
}

impl LineBuffer {
    fn new() -> Self {
        Self { bytes: [0; LINE_CAPACITY], len: 0 }
    }

    fn flush(&mut self) {
        if self.len == 0 {
            return;
        }
        // SAFETY: `bytes[..len]` is initialised and valid for reads.
        unsafe { sys::syscall2(Syscall::DebugLog, self.bytes.as_ptr() as u64, self.len as u64) };
        self.len = 0;
    }
}

impl Write for LineBuffer {
    fn write_str(&mut self, s: &str) -> fmt::Result {
        for &byte in s.as_bytes() {
            if self.len == LINE_CAPACITY {
                self.flush();
            }
            self.bytes[self.len] = byte;
            self.len += 1;
        }
        Ok(())
    }
}

/// Print to the kernel log.
#[macro_export]
macro_rules! print {
    ($($arg:tt)*) => { $crate::io::_print(format_args!($($arg)*)) };
}

/// Print a line to the kernel log.
#[macro_export]
macro_rules! println {
    () => { $crate::print!("\n") };
    ($($arg:tt)*) => { $crate::io::_print(format_args!("{}\n", format_args!($($arg)*))) };
}
