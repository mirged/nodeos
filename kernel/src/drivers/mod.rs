//! The kernel's minimal in-kernel device support.
//!
//! NodeOS keeps exactly one driver in the kernel -- the debug UART -- because
//! the panic path must not depend on userspace being alive. Everything else
//! (block, network, input, GPU, timers beyond the scheduler tick) is a
//! userspace service holding an `Irq` and `Device` capability.

pub mod serial;
