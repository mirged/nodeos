//! Global Descriptor Table, Task State Segment and interrupt stacks.
//!
//! # Descriptor layout
//!
//! The order of the descriptors is not free: `SYSCALL`/`SYSRET` derive four
//! selectors from the two 16-bit halves of `IA32_STAR`, so the table has to
//! read
//!
//! ```text
//!   0x00  null
//!   0x08  kernel code64      <- STAR[47:32]
//!   0x10  kernel data        <- STAR[47:32] + 8
//!   0x18  user code32        <- STAR[63:48]        (never used, but SYSRET
//!   0x20  user data          <- STAR[63:48] + 8     arithmetic requires the
//!   0x28  user code64        <- STAR[63:48] + 16    slot to exist)
//!   0x30  TSS (16 bytes, occupies 0x30 and 0x38)
//! ```
//!
//! # Interrupt stacks
//!
//! NodeOS uses the IST only for faults that mean *the current stack cannot be
//! trusted*: `#DF`, `#NMI`, `#MC`, `#DB`. Everything else -- including `#PF`
//! and every hardware IRQ -- lands on `TSS.RSP0`, i.e. the kernel stack of the
//! thread that was interrupted. That is what makes blocking inside a page
//! fault handler (demand paging) or preempting a thread from a timer IRQ
//! possible: the trap frame is already on a stack that belongs to exactly one
//! thread.
//!
//! The red zone hazard that usually forces kernels to use the IST everywhere
//! does not apply here, because the NodeOS kernel executes with interrupts
//! disabled (see `docs/ARCHITECTURE.md`, "Interrupt discipline").

use core::mem::size_of;

use super::cpu;
use super::percpu::MAX_CPUS;
use crate::sync::SpinLock;

/// Kernel code selector.
pub const KERNEL_CS: u16 = 0x08;
/// Kernel data/stack selector.
pub const KERNEL_SS: u16 = 0x10;
/// 32-bit user code selector (present only to satisfy `SYSRET` arithmetic).
pub const USER_CS32: u16 = 0x18 | 3;
/// User data/stack selector.
pub const USER_SS: u16 = 0x20 | 3;
/// 64-bit user code selector.
pub const USER_CS: u16 = 0x28 | 3;
/// TSS selector.
pub const TSS_SEL: u16 = 0x30;

/// IST index for `#DF` (double fault).
pub const IST_DOUBLE_FAULT: u8 = 1;
/// IST index for NMI.
pub const IST_NMI: u8 = 2;
/// IST index for `#MC` (machine check).
pub const IST_MACHINE_CHECK: u8 = 3;
/// IST index for `#DB` (debug).
pub const IST_DEBUG: u8 = 4;

/// Size of each dedicated interrupt stack.
pub const IST_STACK_SIZE: usize = 16 * 1024;
/// Size of the boot/idle kernel stack installed in `TSS.RSP0` before threads
/// exist.
pub const BOOT_KERNEL_STACK_SIZE: usize = 32 * 1024;

/// A 64-bit Task State Segment.
///
/// In long mode the TSS no longer holds a task context; it is just a table of
/// stack pointers plus the I/O permission bitmap offset.
#[repr(C, packed)]
#[derive(Clone, Copy)]
pub struct Tss {
    _reserved0: u32,
    /// Stack pointer loaded on a privilege change into ring 0.
    pub rsp0: u64,
    /// Stack pointer for ring 1 (unused).
    pub rsp1: u64,
    /// Stack pointer for ring 2 (unused).
    pub rsp2: u64,
    _reserved1: u64,
    /// Interrupt Stack Table entries 1..=7.
    pub ist: [u64; 7],
    _reserved2: u64,
    _reserved3: u16,
    /// Offset from the TSS base to the I/O permission bitmap.
    pub iomap_base: u16,
}

impl Tss {
    const fn new() -> Self {
        Self {
            _reserved0: 0,
            rsp0: 0,
            rsp1: 0,
            rsp2: 0,
            _reserved1: 0,
            ist: [0; 7],
            _reserved2: 0,
            _reserved3: 0,
            // Point past the end of the TSS: no I/O port is permitted from
            // ring 3. Port access is a `Device` capability, brokered by a
            // userspace driver process through IPC, never by the CPU's IOPB.
            iomap_base: size_of::<Tss>() as u16,
        }
    }
}

/// The Global Descriptor Table: seven 8-byte entries, the last two of which
/// form the 16-byte TSS descriptor.
#[repr(C, align(16))]
struct Gdt {
    entries: [u64; 8],
}

#[repr(C, packed)]
struct DescriptorTablePointer {
    limit: u16,
    base: u64,
}

/// Per-CPU segmentation state. Each CPU needs its own TSS (different stacks)
/// and, because the TSS descriptor lives in the GDT, its own GDT.
struct CpuSegments {
    gdt: Gdt,
    tss: Tss,
}

impl CpuSegments {
    const fn new() -> Self {
        Self { gdt: Gdt { entries: [0; 8] }, tss: Tss::new() }
    }
}

// Access byte / flag bits for a 64-bit code or data descriptor.
const ACCESS_PRESENT: u64 = 1 << 47;
const ACCESS_DPL3: u64 = 3 << 45;
const ACCESS_SEGMENT: u64 = 1 << 44; // code/data rather than system
const ACCESS_EXECUTABLE: u64 = 1 << 43;
const ACCESS_RW: u64 = 1 << 41;
const FLAG_LONG_MODE: u64 = 1 << 53;
const FLAG_DB: u64 = 1 << 54; // 32-bit default operand size
const FLAG_GRANULARITY: u64 = 1 << 55;
const LIMIT_MAX: u64 = 0xFFFF | (0xF << 48);

static SEGMENTS: SpinLock<[CpuSegments; MAX_CPUS]> = SpinLock::new([const { CpuSegments::new() }; MAX_CPUS]);

// Dedicated stacks. `.bss` is zeroed by the boot trampoline.
#[repr(C, align(16))]
struct Stack<const N: usize>([u8; N]);

static mut IST_STACKS: [[Stack<IST_STACK_SIZE>; 4]; MAX_CPUS] =
    [const { [const { Stack([0; IST_STACK_SIZE]) }; 4] }; MAX_CPUS];

static mut BOOT_KERNEL_STACKS: [Stack<BOOT_KERNEL_STACK_SIZE>; MAX_CPUS] =
    [const { Stack([0; BOOT_KERNEL_STACK_SIZE]) }; MAX_CPUS];

/// Build and load the GDT, TSS and interrupt stacks for the calling CPU.
///
/// # Safety
/// Must run exactly once per CPU, with interrupts disabled, before the IDT is
/// loaded (the IST indices referenced by IDT entries only become valid here).
pub unsafe fn init(cpu_id: usize) {
    assert!(cpu_id < MAX_CPUS);

    // SAFETY: `cpu_id` uniquely identifies this CPU's stacks, so no other CPU
    // is touching these arrays. Taking a raw pointer (rather than a reference)
    // to a `static mut` avoids creating an aliasing `&mut`.
    let ist_base = unsafe { core::ptr::addr_of_mut!(IST_STACKS[cpu_id]) };
    let boot_stack = unsafe { core::ptr::addr_of_mut!(BOOT_KERNEL_STACKS[cpu_id]) };

    let mut segments = SEGMENTS.lock();
    let cpu = &mut segments[cpu_id];

    // Stacks grow down, so the pointer we install is the *top*.
    for (i, ist_index) in
        [IST_DOUBLE_FAULT, IST_NMI, IST_MACHINE_CHECK, IST_DEBUG].into_iter().enumerate()
    {
        // SAFETY: `ist_base` points at an array of four stacks owned by this
        // CPU; `i` is in 0..4.
        let top = unsafe { (*ist_base).as_mut_ptr().add(i) as u64 + IST_STACK_SIZE as u64 };
        cpu.tss.ist[(ist_index - 1) as usize] = top;
    }
    // SAFETY: `boot_stack` points at this CPU's boot kernel stack.
    cpu.tss.rsp0 = boot_stack as u64 + BOOT_KERNEL_STACK_SIZE as u64;

    let code_common = ACCESS_PRESENT | ACCESS_SEGMENT | ACCESS_EXECUTABLE | ACCESS_RW;
    let data_common = ACCESS_PRESENT | ACCESS_SEGMENT | ACCESS_RW;

    cpu.gdt.entries[0] = 0;
    cpu.gdt.entries[1] = code_common | FLAG_LONG_MODE | FLAG_GRANULARITY | LIMIT_MAX;
    cpu.gdt.entries[2] = data_common | FLAG_DB | FLAG_GRANULARITY | LIMIT_MAX;
    cpu.gdt.entries[3] = code_common | ACCESS_DPL3 | FLAG_DB | FLAG_GRANULARITY | LIMIT_MAX;
    cpu.gdt.entries[4] = data_common | ACCESS_DPL3 | FLAG_DB | FLAG_GRANULARITY | LIMIT_MAX;
    cpu.gdt.entries[5] = code_common | ACCESS_DPL3 | FLAG_LONG_MODE | FLAG_GRANULARITY | LIMIT_MAX;

    // 64-bit TSS descriptor: type 0b1001 (available 64-bit TSS), spanning two
    // GDT slots so it can hold a 64-bit base.
    let tss_base = &cpu.tss as *const Tss as u64;
    let tss_limit = (size_of::<Tss>() - 1) as u64;
    cpu.gdt.entries[6] = tss_limit & 0xFFFF
        | (tss_base & 0xFF_FFFF) << 16
        | 0b1001u64 << 40
        | ACCESS_PRESENT
        | ((tss_limit >> 16) & 0xF) << 48
        | ((tss_base >> 24) & 0xFF) << 56;
    cpu.gdt.entries[7] = tss_base >> 32;

    let gdtr = DescriptorTablePointer {
        limit: (size_of::<Gdt>() - 1) as u16,
        base: &cpu.gdt as *const Gdt as u64,
    };

    // SAFETY: the descriptors above are well-formed; the far return reloads CS
    // with a selector describing the very code we are executing, and the data
    // selectors are flat. `ltr` marks the TSS busy, which is why each CPU must
    // have its own.
    unsafe {
        core::arch::asm!(
            "lgdt [{gdtr}]",
            // Reloading CS needs a far transfer; `retfq` is the cheapest one.
            // Push the new selector and the destination, then far-return.
            "push {cs}",
            "lea {tmp}, [rip + 2f]",
            "push {tmp}",
            "retfq",
            "2:",
            "mov ds, {ds:x}",
            "mov es, {ds:x}",
            "mov ss, {ds:x}",
            "xor {tmp:e}, {tmp:e}",
            "mov fs, {tmp:x}",
            "mov gs, {tmp:x}",
            "ltr {tss:x}",
            gdtr = in(reg) &gdtr,
            cs = in(reg) KERNEL_CS as u64,
            ds = in(reg) KERNEL_SS as u32,
            tss = in(reg) TSS_SEL,
            tmp = out(reg) _,
            options(preserves_flags)
        );
    }
}

/// Set `TSS.RSP0`, the stack the CPU switches to on a ring 3 -> ring 0 trap.
///
/// Called by the scheduler on every context switch so that traps always land
/// on the incoming thread's own kernel stack.
///
/// # Safety
/// `stack_top` must be the top of a mapped, 16-byte aligned kernel stack that
/// belongs exclusively to the thread about to run on this CPU.
pub unsafe fn set_kernel_stack(cpu_id: usize, stack_top: u64) {
    let mut segments = SEGMENTS.lock();
    segments[cpu_id].tss.rsp0 = stack_top;
}

/// Read back `TSS.RSP0` (diagnostics and self-tests).
pub fn kernel_stack(cpu_id: usize) -> u64 {
    SEGMENTS.lock()[cpu_id].tss.rsp0
}

/// Configure `IA32_STAR`, `IA32_LSTAR` and `IA32_FMASK` for `SYSCALL`.
///
/// # Safety
/// `entry` must be the address of a valid syscall entry stub, and the GDT must
/// already have the layout documented at the top of this module.
pub unsafe fn init_syscall(entry: u64) {
    // Enable SYSCALL/SYSRET.
    // SAFETY: EFER.SCE is architecturally defined; setting it only enables an
    // instruction pair we are about to configure correctly.
    unsafe {
        let efer = cpu::rdmsr(cpu::MSR_EFER);
        cpu::wrmsr(cpu::MSR_EFER, efer | 1);

        // STAR[47:32] = kernel CS, STAR[63:48] = user CS32 base.
        let star = ((KERNEL_CS as u64) << 32) | (((USER_CS32 & !3) as u64) << 48);
        cpu::wrmsr(cpu::MSR_STAR, star);
        cpu::wrmsr(cpu::MSR_LSTAR, entry);

        // Bits cleared in RFLAGS on entry. Clearing IF is the load-bearing one:
        // the NodeOS kernel runs with interrupts disabled. DF must be clear for
        // the SysV ABI, TF/NT/AC are cleared so userspace cannot single-step
        // or alignment-check the kernel.
        const RFLAGS_IF: u64 = 1 << 9;
        const RFLAGS_DF: u64 = 1 << 10;
        const RFLAGS_TF: u64 = 1 << 8;
        const RFLAGS_NT: u64 = 1 << 14;
        const RFLAGS_AC: u64 = 1 << 18;
        cpu::wrmsr(cpu::MSR_SFMASK, RFLAGS_IF | RFLAGS_DF | RFLAGS_TF | RFLAGS_NT | RFLAGS_AC);
    }
}
