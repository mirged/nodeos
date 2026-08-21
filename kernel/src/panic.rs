//! Kernel panic handling.

use core::panic::PanicInfo;
use core::sync::atomic::{AtomicUsize, Ordering};

use crate::arch::x86_64::cpu;
use crate::drivers::serial;
use crate::kprintln;

static PANIC_DEPTH: AtomicUsize = AtomicUsize::new(0);

/// The kernel's panic entry point.
///
/// A kernel panic is unconditionally fatal: NodeOS has no unwinding, and a
/// broken kernel invariant cannot be contained the way a broken *userspace*
/// invariant can (a faulting service is just a process that dies and gets
/// restarted by its supervisor, which is the entire point of the microkernel
/// split).
#[panic_handler]
fn panic(info: &PanicInfo<'_>) -> ! {
    cpu::interrupts_disable();

    let depth = PANIC_DEPTH.fetch_add(1, Ordering::SeqCst);
    if depth > 2 {
        // Panicking inside the panic printer. Stop trying to be helpful.
        cpu::halt_forever();
    }

    // The panicking CPU may already hold the console lock (for example when a
    // formatting impl panics). Break it: correctness of the lock no longer
    // matters, getting the message out does.
    // SAFETY: the machine is dead; a torn line of output is preferable to no
    // output at all.
    unsafe { serial::force_unlock() };

    kprintln!("\n\x1b[1;35m================ NodeOS kernel panic ================\x1b[0m");
    if let Some(location) = info.location() {
        kprintln!("  at {}:{}:{}", location.file(), location.line(), location.column());
    }
    kprintln!("  {}", info.message());
    kprintln!("\x1b[1;35m=====================================================\x1b[0m");

    crate::arch::x86_64::cpu::halt_forever()
}
