//! Host-side unit tests for the editor's text logic (`make test-editor`).
//! The editor binary's own root is main.rs; this root compiles only the two
//! modules that do not touch NodeOS.
#![allow(dead_code)] // used by the editor binary, not by every test
extern crate alloc;
mod buffer;
mod highlight;
