//! Process startup.
//!
//! The kernel enters a new process at `_start` with a single argument in `RDI`:
//! the bootstrap channel handle. There is no `argv`, no `envp`, no auxiliary
//! vector, and no stack layout convention to get wrong. Everything else arrives
//! as a message.

use core::arch::global_asm;

use nodeos_abi::message::{BootstrapMessage, KIND_BOOTSTRAP};
use nodeos_abi::HandleValue;

use crate::channel::Channel;
use crate::env::Env;
use crate::Handle;

global_asm!(
    r#"
.section .text.start, "ax"
.balign 16
.globl _start
.type _start, @function
_start:
    /* The kernel starts us with RDI = bootstrap handle and a fresh, 16-byte
       aligned stack. The SysV ABI wants RSP+8 to be 16-byte aligned at a
       function's first instruction, which is what the `call` below arranges;
       aligning first makes that true regardless of what the loader chose.  */
    xor     ebp, ebp
    and     rsp, -16
    call    __libnode_start
    /* __libnode_start never returns. If it somehow does, fault loudly rather
       than execute whatever follows.                                       */
    ud2
.size _start, . - _start
"#
);

/// Default heap when the bootstrap message does not specify one.
const DEFAULT_HEAP: u64 = 1024 * 1024;

extern "Rust" {
    /// Provided by the program via the [`crate::main!`] macro.
    fn __libnode_main(env: Env) -> i32;
}

/// The Rust half of process startup.
///
/// # Safety
/// Called exactly once, by `_start`, with `bootstrap` holding the handle value
/// the kernel placed in `RDI`.
#[no_mangle]
pub unsafe extern "C" fn __libnode_start(bootstrap: u32) -> ! {
    // The heap has to exist before anything can allocate, including the code
    // that reads the bootstrap message telling us how big the heap should be.
    // Start with a default, which is enough to receive one message.
    crate::alloc_impl::init(DEFAULT_HEAP);

    let handle = Handle::from_raw(HandleValue(bootstrap));
    let mut env = if bootstrap == 0 {
        Env::empty()
    } else {
        let channel = Channel::from_handle(handle);
        match channel.receive() {
            Ok(message) if message.kind == KIND_BOOTSTRAP
                && message.payload.len() >= core::mem::size_of::<BootstrapMessage>() =>
            {
                // SAFETY: the length check above guarantees the payload is at
                // least one `BootstrapMessage`, which is plain-old-data.
                let info: BootstrapMessage =
                    unsafe { (message.payload.as_ptr() as *const BootstrapMessage).read_unaligned() };
                Env::from_bootstrap(info, channel, message.handles)
            }
            Ok(_) | Err(_) => Env::with_channel(channel),
        }
    };
    env.finish_startup();

    // SAFETY: `__libnode_main` is defined by the program through the `main!`
    // macro, which enforces this exact signature.
    let code = unsafe { __libnode_main(env) };
    crate::exit(code)
}

/// Declare a program's entry point.
///
/// ```ignore
/// libnode::main! {
///     fn main(env: libnode::Env) -> i32 { 0 }
/// }
/// ```
#[macro_export]
macro_rules! main {
    (fn $name:ident($env:ident : $envty:ty) -> i32 $body:block) => {
        #[no_mangle]
        pub extern "Rust" fn __libnode_main($env: $envty) -> i32 $body

        #[allow(dead_code)]
        fn $name() {}
    };
}

/// The panic handler every NodeOS program shares.
///
/// A process that panics reports and exits. It does not abort the machine and
/// does not try to unwind: whatever supervises it will see its handles close
/// and decide what to do, which is the whole point of putting it in its own
/// address space.
#[panic_handler]
fn panic(info: &core::panic::PanicInfo<'_>) -> ! {
    crate::println!("panic: {}", info.message());
    if let Some(location) = info.location() {
        crate::println!("  at {}:{}", location.file(), location.line());
    }
    crate::exit(101)
}

/// Landing-pad personality routine; unreachable under `panic = "abort"`.
#[no_mangle]
pub extern "C" fn rust_eh_personality() {}

/// Unwinder resume hook; unreachable under `panic = "abort"`.
#[no_mangle]
pub extern "C" fn _Unwind_Resume() -> ! {
    crate::exit(102)
}
