//! Kernel-side thread context switching.
//!
//! # What is saved where
//!
//! NodeOS splits a thread's machine state in two:
//!
//! * The **trap frame** (`arch::trap::TrapFrame`) holds the *user* state. It is
//!   built by the interrupt/syscall entry stubs on the thread's own kernel
//!   stack and consumed by `iretq`/`sysretq` on the way out.
//! * The **switch frame** (this module) holds the *kernel* state: the six
//!   callee-saved registers and the return address, as laid out by
//!   [`switch_to`]. Everything else is caller-saved and therefore already on
//!   the stack by the time a switch happens.
//!
//! Because a context switch is a normal function call as far as the compiler is
//! concerned, the caller-saved registers need no special handling -- which is
//! why this is nine instructions rather than a full register file save.

use core::arch::global_asm;

/// The frame [`switch_to`] pushes, in memory order from the new stack pointer.
#[repr(C)]
#[derive(Debug, Default, Clone, Copy)]
pub struct SwitchFrame {
    /// Callee-saved.
    pub r15: u64,
    /// Callee-saved.
    pub r14: u64,
    /// Callee-saved.
    pub r13: u64,
    /// Callee-saved.
    pub r12: u64,
    /// Callee-saved.
    pub rbx: u64,
    /// Callee-saved.
    pub rbp: u64,
    /// Where `switch_to`'s `ret` will go.
    pub return_address: u64,
}

global_asm!(
    r#"
.section .text
.balign 16
.globl switch_to
.type switch_to, @function
switch_to:
    /* rdi = &mut current_rsp, rsi = next_rsp
       Save the callee-saved set in SwitchFrame order, park the stack pointer
       where the outgoing thread will find it, adopt the incoming stack, and
       return -- which lands on whatever return address that stack holds.
       For a thread that has run before, that is the instruction after its own
       call to switch_to; for a brand-new thread, it is the trampoline that
       `Thread::new` planted there.                                        */
    push    rbp
    push    rbx
    push    r12
    push    r13
    push    r14
    push    r15
    mov     [rdi], rsp
    mov     rsp, rsi
    pop     r15
    pop     r14
    pop     r13
    pop     r12
    pop     rbx
    pop     rbp
    ret
.size switch_to, . - switch_to

.balign 16
.globl enter_user_mode
.type enter_user_mode, @function
enter_user_mode:
    /* rdi = &TrapFrame prepared on this thread's kernel stack. Adopt it as the
       stack and take the ordinary interrupt-return path out to ring 3.     */
    mov     rsp, rdi
    swapgs
    pop     r15
    pop     r14
    pop     r13
    pop     r12
    pop     r11
    pop     r10
    pop     r9
    pop     r8
    pop     rbp
    pop     rdi
    pop     rsi
    pop     rdx
    pop     rcx
    pop     rbx
    pop     rax
    add     rsp, 16                     /* discard vector + error code */
    iretq
.size enter_user_mode, . - enter_user_mode
"#
);

extern "C" {
    /// Switch kernel stacks.
    ///
    /// # Safety
    /// `current_rsp` must point at the outgoing thread's saved-stack slot, and
    /// `next_rsp` must be a kernel stack whose top holds a valid
    /// [`SwitchFrame`]. The caller must hold no locks that the incoming thread
    /// could try to take.
    pub fn switch_to(current_rsp: *mut u64, next_rsp: u64);

    /// Drop to ring 3 using a prepared trap frame.
    ///
    /// # Safety
    /// `frame` must point at a complete `TrapFrame` on the *current* thread's
    /// kernel stack, describing a ring-3 context, and `TSS.RSP0` must already
    /// point at that stack.
    pub fn enter_user_mode(frame: *const crate::arch::x86_64::trap::TrapFrame) -> !;
}
