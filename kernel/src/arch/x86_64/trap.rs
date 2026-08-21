//! Interrupt and exception entry.
//!
//! # Frame layout
//!
//! Every vector funnels through one common stub, so by the time Rust code sees
//! the interrupt there is a single, uniform [`TrapFrame`] on the stack. The CPU
//! pushes `SS:RSP`, `RFLAGS`, `CS:RIP` and (for some vectors) an error code;
//! the stub normalises that by pushing a zero error code for the vectors that
//! do not have one, then the vector number, then the general-purpose
//! registers.
//!
//! ```text
//!   低 (rsp) -> r15 r14 r13 r12 r11 r10 r9 r8 rbp rdi rsi rdx rcx rbx rax
//!               vector  error_code
//!               rip  cs  rflags  rsp  ss   <- pushed by the CPU
//! ```
//!
//! [`dispatch`] returns the frame to resume on. That indirection is what makes
//! preemption possible: a scheduler decision can hand back a *different*
//! thread's trap frame and the exit path will simply `iretq` into it.

use core::arch::global_asm;
use core::fmt;

use super::segmentation::{IST_DEBUG, IST_DOUBLE_FAULT, IST_MACHINE_CHECK, IST_NMI, KERNEL_CS};
use super::{cpu, percpu};

/// Number of IDT vectors.
pub const IDT_ENTRIES: usize = 256;

/// First vector available for device interrupts. 0..32 are architectural.
pub const IRQ_BASE: u8 = 32;
/// Vector used by the scheduler tick.
pub const VECTOR_TIMER: u8 = 32;
/// Vector used by the legacy keyboard controller.
pub const VECTOR_KEYBOARD: u8 = 33;
/// Vector used by the debug UART.
pub const VECTOR_SERIAL: u8 = 36;
/// Vector used for the local APIC spurious interrupt.
pub const VECTOR_SPURIOUS: u8 = 0xFF;
/// Vector used for cross-CPU reschedule IPIs.
pub const VECTOR_RESCHEDULE: u8 = 0xFE;
/// Vector used for cross-CPU TLB shootdown IPIs.
pub const VECTOR_TLB_SHOOTDOWN: u8 = 0xFD;
/// Vector userspace uses for the (legacy, slow) `int 0x80` syscall path.
pub const VECTOR_SYSCALL_LEGACY: u8 = 0x80;

/// Saved machine state at the point of an interrupt or exception.
#[repr(C)]
#[derive(Clone, Copy, Default)]
pub struct TrapFrame {
    /// Callee/caller-saved general-purpose registers, in stub push order.
    pub r15: u64,
    /// See [`TrapFrame::r15`].
    pub r14: u64,
    /// See [`TrapFrame::r15`].
    pub r13: u64,
    /// See [`TrapFrame::r15`].
    pub r12: u64,
    /// See [`TrapFrame::r15`].
    pub r11: u64,
    /// See [`TrapFrame::r15`].
    pub r10: u64,
    /// See [`TrapFrame::r15`].
    pub r9: u64,
    /// See [`TrapFrame::r15`].
    pub r8: u64,
    /// See [`TrapFrame::r15`].
    pub rbp: u64,
    /// See [`TrapFrame::r15`].
    pub rdi: u64,
    /// See [`TrapFrame::r15`].
    pub rsi: u64,
    /// See [`TrapFrame::r15`].
    pub rdx: u64,
    /// See [`TrapFrame::r15`].
    pub rcx: u64,
    /// See [`TrapFrame::r15`].
    pub rbx: u64,
    /// See [`TrapFrame::r15`].
    pub rax: u64,

    /// Vector number, pushed by the per-vector stub.
    pub vector: u64,
    /// Architectural error code, or zero for vectors that do not push one.
    pub error_code: u64,

    /// Interrupted instruction pointer.
    pub rip: u64,
    /// Interrupted code selector.
    pub cs: u64,
    /// Interrupted flags.
    pub rflags: u64,
    /// Interrupted stack pointer.
    pub rsp: u64,
    /// Interrupted stack selector.
    pub ss: u64,
}

impl TrapFrame {
    /// Whether the interrupted context was running in ring 3.
    #[inline]
    pub fn from_user(&self) -> bool {
        self.cs & 3 == 3
    }
}

impl fmt::Display for TrapFrame {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        writeln!(f, "  rip {:#018x}  cs  {:#06x}  rflags {:#018x}", self.rip, self.cs, self.rflags)?;
        writeln!(f, "  rsp {:#018x}  ss  {:#06x}  error  {:#018x}", self.rsp, self.ss, self.error_code)?;
        writeln!(f, "  rax {:#018x}  rbx {:#018x}  rcx {:#018x}", self.rax, self.rbx, self.rcx)?;
        writeln!(f, "  rdx {:#018x}  rsi {:#018x}  rdi {:#018x}", self.rdx, self.rsi, self.rdi)?;
        writeln!(f, "  rbp {:#018x}  r8  {:#018x}  r9  {:#018x}", self.rbp, self.r8, self.r9)?;
        writeln!(f, "  r10 {:#018x}  r11 {:#018x}  r12 {:#018x}", self.r10, self.r11, self.r12)?;
        write!(f, "  r13 {:#018x}  r14 {:#018x}  r15 {:#018x}", self.r13, self.r14, self.r15)
    }
}

// ---------------------------------------------------------------------------
// Entry stubs
// ---------------------------------------------------------------------------
//
// 256 stubs, each padded to exactly 16 bytes so that the address of vector `v`
// is `isr_stub_base + v * 16`. That removes the need for 256 named symbols and
// makes IDT construction a loop.

/// Fixed stride between consecutive entry stubs.
pub const ISR_STUB_STRIDE: usize = 16;

global_asm!(
    r#"
.section .text
.balign 16
.globl isr_stub_base
isr_stub_base:
.set vector_index, 0
.rept 256
    .balign 16
    /* Vectors 8, 10-14, 17, 21, 29 and 30 push an architectural error code;
       for every other vector we push a zero so the frame is uniform.      */
    .if (vector_index == 8) || (vector_index >= 10 && vector_index <= 14) || (vector_index == 17) || (vector_index == 21) || (vector_index == 29) || (vector_index == 30)
        push    vector_index
    .else
        push    0
        push    vector_index
    .endif
    jmp     isr_common
.set vector_index, vector_index + 1
.endr
.balign 16
.globl isr_stub_end
isr_stub_end:

.globl isr_common
isr_common:
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

    /* If the trap came from ring 3, swap in the kernel's GS base. Testing the
       saved CS is the only reliable way to tell; `swapgs` is not idempotent
       and swapping twice would leave the kernel reading userspace's GS.    */
    mov     rax, [rsp + 0x90]          /* saved CS */
    and     eax, 3
    jz      1f
    swapgs
1:
    cld                                 /* SysV requires DF clear           */
    mov     rdi, rsp                    /* &TrapFrame                       */
    mov     rbp, rsp                    /* stop stack unwinders here        */
    call    trap_dispatch

    /* `trap_dispatch` returns the frame to resume, which may belong to a
       different thread than the one we interrupted.                        */
    mov     rsp, rax

    mov     rax, [rsp + 0x90]
    and     eax, 3
    jz      2f
    swapgs
2:
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
    add     rsp, 16                     /* discard vector + error code      */
    iretq
"#
);

extern "C" {
    /// Base of the 256 fixed-stride entry stubs (defined in the asm above).
    static isr_stub_base: u8;
}

/// Address of the entry stub for `vector`.
pub fn stub_address(vector: u8) -> u64 {
    // Taking the address of an `extern` symbol; the byte itself is never read.
    let base = core::ptr::addr_of!(isr_stub_base) as u64;
    base + (vector as u64) * ISR_STUB_STRIDE as u64
}

// ---------------------------------------------------------------------------
// IDT
// ---------------------------------------------------------------------------

/// A 64-bit interrupt/trap gate descriptor.
#[repr(C)]
#[derive(Clone, Copy, Default)]
struct IdtEntry {
    offset_low: u16,
    selector: u16,
    ist: u8,
    type_attr: u8,
    offset_mid: u16,
    offset_high: u32,
    reserved: u32,
}

impl IdtEntry {
    const fn empty() -> Self {
        Self { offset_low: 0, selector: 0, ist: 0, type_attr: 0, offset_mid: 0, offset_high: 0, reserved: 0 }
    }

    fn set(&mut self, handler: u64, ist: u8, dpl: u8, trap_gate: bool) {
        self.offset_low = handler as u16;
        self.offset_mid = (handler >> 16) as u16;
        self.offset_high = (handler >> 32) as u32;
        self.selector = KERNEL_CS;
        self.ist = ist & 0x7;
        // Present | DPL | 0 | gate type (0xE interrupt, 0xF trap).
        self.type_attr = 0x80 | ((dpl & 3) << 5) | if trap_gate { 0xF } else { 0xE };
        self.reserved = 0;
    }
}

#[repr(C, align(16))]
struct Idt {
    entries: [IdtEntry; IDT_ENTRIES],
}

#[repr(C, packed)]
struct Idtr {
    limit: u16,
    base: u64,
}

static mut IDT: Idt = Idt { entries: [IdtEntry::empty(); IDT_ENTRIES] };

/// Build the IDT. Called once on the boot CPU; application processors reuse
/// the same table (only the IST *contents* are per-CPU, via the TSS).
///
/// # Safety
/// Must run before interrupts are enabled and after
/// [`super::segmentation::init`], because the IST indices referenced here only
/// mean anything once a TSS is loaded.
pub unsafe fn init_idt() {
    // SAFETY: single-threaded during boot; we use a raw pointer rather than a
    // `&mut` to avoid asserting uniqueness over a `static mut`.
    let idt = unsafe { &mut *core::ptr::addr_of_mut!(IDT) };

    for vector in 0..IDT_ENTRIES {
        let ist = match vector as u8 {
            1 => IST_DEBUG,           // #DB  -- may fire anywhere, incl. bad stack
            2 => IST_NMI,             // NMI  -- unmaskable, can nest on anything
            8 => IST_DOUBLE_FAULT,    // #DF  -- the stack is by definition broken
            18 => IST_MACHINE_CHECK,  // #MC
            _ => 0,                   // everything else lands on TSS.RSP0
        };
        // Only the legacy syscall vector is reachable from ring 3; every other
        // vector traps with DPL 0 so userspace cannot forge an interrupt.
        let dpl = if vector as u8 == VECTOR_SYSCALL_LEGACY { 3 } else { 0 };
        idt.entries[vector].set(stub_address(vector as u8), ist, dpl, false);
    }

    // SAFETY: the table was fully populated immediately above.
    unsafe { load_idt() };
}

/// Load the IDT register on the calling CPU.
///
/// # Safety
/// [`init_idt`] must have run first.
pub unsafe fn load_idt() {
    let idtr = Idtr {
        limit: (core::mem::size_of::<Idt>() - 1) as u16,
        // Address of a static; the table outlives every CPU.
        base: core::ptr::addr_of!(IDT) as u64,
    };
    // SAFETY: `idtr` describes a well-formed table of 256 gates.
    unsafe { core::arch::asm!("lidt [{}]", in(reg) &idtr, options(readonly, nostack, preserves_flags)) };
}

// ---------------------------------------------------------------------------
// Dispatch
// ---------------------------------------------------------------------------

/// Handler installed for a device interrupt vector.
pub type IrqHandler = fn(&mut TrapFrame);

static mut IRQ_HANDLERS: [Option<IrqHandler>; IDT_ENTRIES] = [None; IDT_ENTRIES];

/// Register a handler for `vector`.
///
/// # Safety
/// Must be called with interrupts disabled, and `vector` must not already be
/// claimed by kernel-internal machinery.
pub unsafe fn register_handler(vector: u8, handler: IrqHandler) {
    // SAFETY: interrupts are disabled and registration happens during
    // single-threaded subsystem bring-up.
    unsafe {
        (*core::ptr::addr_of_mut!(IRQ_HANDLERS))[vector as usize] = Some(handler);
    }
}

/// Common dispatch point, called by `isr_common`.
///
/// Returns the trap frame to resume; normally the one passed in, but the
/// scheduler may substitute another thread's frame.
///
/// # Safety
/// Called only from `isr_common`, with `frame` pointing at a complete
/// [`TrapFrame`] on the current kernel stack.
#[no_mangle]
pub unsafe extern "C" fn trap_dispatch(frame: *mut TrapFrame) -> *mut TrapFrame {
    // SAFETY: the entry stub built this frame; it is valid, aligned and
    // uniquely borrowed for the duration of the handler.
    let frame = unsafe { &mut *frame };

    if percpu::is_ready() {
        percpu::current().interrupt_count.fetch_add(1, core::sync::atomic::Ordering::Relaxed);
    }

    let vector = frame.vector as u8;
    if vector < 32 {
        super::exception::handle(frame);
    } else {
        // SAFETY: the table is only mutated with interrupts disabled during
        // bring-up, and we are inside an interrupt handler with IF clear.
        let handler = unsafe { (*core::ptr::addr_of!(IRQ_HANDLERS))[vector as usize] };
        match handler {
            Some(handler) => handler(frame),
            None => {
                crate::warn!("unhandled interrupt vector {vector} at rip {:#x}", frame.rip);
            }
        }
        // SAFETY: we are inside the handler for `vector`, and this is the
        // single acknowledgement for it.
        unsafe { super::interrupt::end_of_interrupt(vector) };
    }

    frame as *mut TrapFrame
}

/// Whether interrupts are currently enabled on this CPU.
pub fn interrupts_enabled() -> bool {
    cpu::interrupts_enabled()
}
