//! Structured kernel logging over the debug console.

use core::fmt::{self, Write};
use core::sync::atomic::{AtomicU8, Ordering};

use crate::drivers::serial::CONSOLE;

/// Severity of a log record.
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Debug)]
#[repr(u8)]
pub enum Level {
    /// Unrecoverable condition; the kernel is about to stop.
    Fatal = 0,
    /// A subsystem failed but the kernel continues.
    Error = 1,
    /// Something unexpected that is nonetheless handled.
    Warn = 2,
    /// Normal lifecycle milestones.
    Info = 3,
    /// Detail useful when bringing a subsystem up.
    Debug = 4,
    /// Firehose.
    Trace = 5,
}

impl Level {
    const fn tag(self) -> &'static str {
        match self {
            Level::Fatal => "FATAL",
            Level::Error => "ERROR",
            Level::Warn => " WARN",
            Level::Info => " INFO",
            Level::Debug => "DEBUG",
            Level::Trace => "TRACE",
        }
    }

    /// ANSI SGR sequence used to colour the level tag.
    const fn colour(self) -> &'static str {
        match self {
            Level::Fatal => "\x1b[1;35m",
            Level::Error => "\x1b[1;31m",
            Level::Warn => "\x1b[1;33m",
            Level::Info => "\x1b[1;32m",
            Level::Debug => "\x1b[1;36m",
            Level::Trace => "\x1b[1;90m",
        }
    }
}

static MAX_LEVEL: AtomicU8 = AtomicU8::new(Level::Debug as u8);

/// Set the maximum level that will be emitted.
pub fn set_max_level(level: Level) {
    MAX_LEVEL.store(level as u8, Ordering::Relaxed);
}

/// Whether a record at `level` would be emitted.
#[inline]
pub fn enabled(level: Level) -> bool {
    (level as u8) <= MAX_LEVEL.load(Ordering::Relaxed)
}

#[doc(hidden)]
pub fn _log(level: Level, module: &str, args: fmt::Arguments<'_>) {
    if !enabled(level) {
        return;
    }
    let mut console = CONSOLE.lock();
    let _ = write!(console, "{}[{}]\x1b[0m \x1b[90m{}\x1b[0m: ", level.colour(), level.tag(), module);
    let _ = console.write_fmt(args);
    let _ = console.write_str("\n");
}

#[doc(hidden)]
pub fn _print(args: fmt::Arguments<'_>) {
    let mut console = CONSOLE.lock();
    let _ = console.write_fmt(args);
}

/// Write directly to the console with no level prefix.
#[macro_export]
macro_rules! kprint {
    ($($arg:tt)*) => { $crate::log::_print(format_args!($($arg)*)) };
}

/// Write a line directly to the console with no level prefix.
#[macro_export]
macro_rules! kprintln {
    () => { $crate::kprint!("\n") };
    ($($arg:tt)*) => { $crate::log::_print(format_args!("{}\n", format_args!($($arg)*))) };
}

/// Log at [`Level::Error`].
#[macro_export]
macro_rules! error {
    ($($arg:tt)*) => { $crate::log::_log($crate::log::Level::Error, module_path!(), format_args!($($arg)*)) };
}

/// Log at [`Level::Warn`].
#[macro_export]
macro_rules! warn {
    ($($arg:tt)*) => { $crate::log::_log($crate::log::Level::Warn, module_path!(), format_args!($($arg)*)) };
}

/// Log at [`Level::Info`].
#[macro_export]
macro_rules! info {
    ($($arg:tt)*) => { $crate::log::_log($crate::log::Level::Info, module_path!(), format_args!($($arg)*)) };
}

/// Log at [`Level::Debug`].
#[macro_export]
macro_rules! debug {
    ($($arg:tt)*) => { $crate::log::_log($crate::log::Level::Debug, module_path!(), format_args!($($arg)*)) };
}

/// Log at [`Level::Trace`].
#[macro_export]
macro_rules! trace {
    ($($arg:tt)*) => { $crate::log::_log($crate::log::Level::Trace, module_path!(), format_args!($($arg)*)) };
}
