//! The `SYSCALL` fast path.
//!
//! `SYSCALL` is cheap because it does almost nothing: it loads `CS`/`SS` from
//! `IA32_STAR`, puts the return address in `RCX` and the flags in `R11`, masks
//! the flags named by `IA32_FMASK`, and jumps to `IA32_LSTAR`. In particular it
//! does **not** switch stacks, so the first thing this stub must do -- before
//! touching a single byte of memory -- is get off the user stack.
//!
//! `swapgs` makes that possible: it exchanges `GS_BASE` with
//! `IA32_KERNEL_GS_BASE`, giving the kernel a pointer to its per-CPU block
//! without needing a register that userspace has not chosen the contents of.
//! The per-CPU block's first three words are, by contract with
//! `arch::x86_64::percpu::PerCpu`:
//!
//! ```text
//!   gs:[0]   self pointer
//!   gs:[8]   scratch: the user stack pointer, parked here
//!   gs:[16]  this thread's kernel stack top
//! ```
//!
//! The stub then builds the same `TrapFrame` an interrupt would, so one frame
//! layout serves both paths and the scheduler can substitute a different
//! thread's frame on the way out.

use core::arch::global_asm;

global_asm!(
    r#"
.section .text
.balign 16
.globl syscall_entry
.type syscall_entry, @function
syscall_entry:
    swapgs                          /* GS now points at the per-CPU block  */
    mov     gs:[8], rsp             /* park the user stack pointer         */
    mov     rsp, gs:[16]            /* adopt this thread's kernel stack    */

    /* Build a TrapFrame identical in shape to the interrupt path's.        */
    push    0x23                    /* ss  = USER_SS                        */
    push    qword ptr gs:[8]        /* rsp = the user stack pointer         */
    push    r11                     /* rflags, saved by SYSCALL             */
    push    0x2b                    /* cs  = USER_CS                        */
    push    rcx                     /* rip, saved by SYSCALL                */
    push    0                       /* error code: none                     */
    push    0x100                   /* vector: the syscall pseudo-vector    */

    push    rax
    push    rbx
    push    rcx
    push    rdx
    push    rsi
    push    rdi
    push    rbp
    push    r8
    push    r9
    push    r10
    push    r11
    push    r12
    push    r13
    push    r14
    push    r15

    cld                             /* the SysV ABI requires DF clear       */
    mov     rdi, rsp
    xor     rbp, rbp                /* terminate any stack walk here        */
    call    syscall_dispatch

    /* The dispatcher returns the frame to resume on, which may belong to a
       different thread if it went through the scheduler.                   */
    mov     rsp, rax

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
    add     rsp, 16                 /* discard vector + error code          */

    swapgs                          /* restore userspace's GS               */
    iretq                           /* not sysret: the frame may have been
                                       rewritten, and iretq restores all of
                                       CS, SS, RFLAGS and RSP faithfully.   */
.size syscall_entry, . - syscall_entry
"#
);

extern "C" {
    /// The `IA32_LSTAR` target.
    pub fn syscall_entry();
}

/// Address to program into `IA32_LSTAR`.
pub fn entry_address() -> u64 {
    syscall_entry as *const () as u64
}
