//! Core CPU state manipulation: RFLAGS, control registers, MSRs, halting.

use core::arch::asm;

/// Read RFLAGS.
#[inline]
pub fn rflags() -> u64 {
    let flags: u64;
    // SAFETY: reading flags via the stack has no side effects; the `pushfq`
    // and `popq` pair leaves RSP unchanged, so the compiler's stack model
    // stays valid (we still declare it as touching the stack).
    unsafe { asm!("pushfq; pop {}", out(reg) flags, options(nomem, preserves_flags)) };
    flags
}

/// `RFLAGS.IF`, i.e. whether maskable interrupts are enabled on this CPU.
#[inline]
pub fn interrupts_enabled() -> bool {
    rflags() & (1 << 9) != 0
}

/// Enable maskable interrupts.
///
/// # Safety
/// Enabling interrupts in a section that assumes atomicity (holding a plain
/// [`crate::sync::SpinLock`] shared with an interrupt handler, or mid-way
/// through a context switch) corrupts kernel state.
#[inline]
pub unsafe fn interrupts_enable() {
    // SAFETY: caller asserts this point is interruptible.
    unsafe { asm!("sti", options(nomem, nostack)) };
}

/// Disable maskable interrupts.
#[inline]
pub fn interrupts_disable() {
    // SAFETY: masking interrupts is always memory-safe; it can only affect
    // liveness, which is the caller's problem, not a soundness one.
    unsafe { asm!("cli", options(nomem, nostack)) };
}

/// Disable interrupts and return the previous RFLAGS for later restoration.
#[inline]
pub fn interrupts_disable_save() -> u64 {
    let flags = rflags();
    interrupts_disable();
    flags
}

/// Restore the interrupt flag captured by [`interrupts_disable_save`].
///
/// # Safety
/// `flags` must come from [`interrupts_disable_save`] on this same CPU, and the
/// caller must be at a point where re-enabling interrupts is legal.
#[inline]
pub unsafe fn interrupts_restore(flags: u64) {
    if flags & (1 << 9) != 0 {
        // SAFETY: interrupts were enabled when the flags were captured, so the
        // caller was already in an interruptible context.
        unsafe { interrupts_enable() };
    }
}

/// Halt until the next interrupt.
#[inline]
pub fn halt() {
    // SAFETY: `hlt` in ring 0 only stops the CPU until an interrupt arrives.
    unsafe { asm!("hlt", options(nomem, nostack, preserves_flags)) };
}

/// Disable interrupts and halt forever. Never returns.
pub fn halt_forever() -> ! {
    loop {
        interrupts_disable();
        halt();
    }
}

/// Enable interrupts, then halt. This is the idle loop's atomic
/// "release the CPU" step: `sti` has a one-instruction interrupt shadow, so an
/// interrupt cannot slip in between `sti` and `hlt` and cause a lost wakeup.
///
/// # Safety
/// Callable only from the idle path, where being interrupted is the point.
#[inline]
pub unsafe fn enable_interrupts_and_halt() {
    // SAFETY: the `sti; hlt` pairing is architecturally atomic.
    unsafe { asm!("sti; hlt", options(nomem, nostack, preserves_flags)) };
}

/// Emit a `pause` hint for spin-wait loops.
#[inline]
pub fn pause() {
    core::hint::spin_loop();
}

// ---------------------------------------------------------------------------
// Model-specific registers
// ---------------------------------------------------------------------------

/// `IA32_EFER`
pub const MSR_EFER: u32 = 0xC000_0080;
/// `IA32_STAR`: SYSCALL/SYSRET segment selectors.
pub const MSR_STAR: u32 = 0xC000_0081;
/// `IA32_LSTAR`: 64-bit SYSCALL entry point.
pub const MSR_LSTAR: u32 = 0xC000_0082;
/// `IA32_FMASK`: RFLAGS bits cleared on SYSCALL.
pub const MSR_SFMASK: u32 = 0xC000_0084;
/// `IA32_FS_BASE`
pub const MSR_FS_BASE: u32 = 0xC000_0100;
/// `IA32_GS_BASE`
pub const MSR_GS_BASE: u32 = 0xC000_0101;
/// `IA32_KERNEL_GS_BASE`: the value `swapgs` exchanges with `GS_BASE`.
pub const MSR_KERNEL_GS_BASE: u32 = 0xC000_0102;
/// `IA32_APIC_BASE`
pub const MSR_APIC_BASE: u32 = 0x0000_001B;
/// `IA32_TSC_DEADLINE`
pub const MSR_TSC_DEADLINE: u32 = 0x0000_06E0;

/// Read a model-specific register.
///
/// # Safety
/// `msr` must exist on this CPU, otherwise `rdmsr` raises `#GP`.
#[inline]
pub unsafe fn rdmsr(msr: u32) -> u64 {
    let (low, high): (u32, u32);
    // SAFETY: caller guarantees the MSR is implemented.
    unsafe {
        asm!("rdmsr", in("ecx") msr, out("eax") low, out("edx") high,
             options(nomem, nostack, preserves_flags))
    };
    ((high as u64) << 32) | (low as u64)
}

/// Write a model-specific register.
///
/// # Safety
/// `msr` must exist, and `value` must be architecturally valid for it. Many
/// MSRs (EFER, LSTAR, GS_BASE) reconfigure the CPU in ways that invalidate
/// every assumption the rest of the kernel makes.
#[inline]
pub unsafe fn wrmsr(msr: u32, value: u64) {
    // SAFETY: caller guarantees validity.
    unsafe {
        asm!("wrmsr", in("ecx") msr, in("eax") value as u32, in("edx") (value >> 32) as u32,
             options(nomem, nostack, preserves_flags))
    };
}

// ---------------------------------------------------------------------------
// Control registers
// ---------------------------------------------------------------------------

/// Read CR0.
#[inline]
pub fn read_cr0() -> u64 {
    let v: u64;
    // SAFETY: reading a control register has no side effects.
    unsafe { asm!("mov {}, cr0", out(reg) v, options(nomem, nostack, preserves_flags)) };
    v
}

/// Write CR0.
///
/// # Safety
/// CR0 controls paging, protection and FPU trapping. A wrong value faults on
/// the next instruction.
#[inline]
pub unsafe fn write_cr0(v: u64) {
    // SAFETY: caller guarantees the value keeps the CPU in a valid state.
    unsafe { asm!("mov cr0, {}", in(reg) v, options(nomem, nostack, preserves_flags)) };
}

/// Read CR2, the faulting linear address recorded by a `#PF`.
#[inline]
pub fn read_cr2() -> u64 {
    let v: u64;
    // SAFETY: read-only.
    unsafe { asm!("mov {}, cr2", out(reg) v, options(nomem, nostack, preserves_flags)) };
    v
}

/// Read CR3, the physical address of the active PML4.
#[inline]
pub fn read_cr3() -> u64 {
    let v: u64;
    // SAFETY: read-only.
    unsafe { asm!("mov {}, cr3", out(reg) v, options(nomem, nostack, preserves_flags)) };
    v
}

/// Write CR3, switching address spaces and flushing non-global TLB entries.
///
/// # Safety
/// `v` must be the physical address of a valid PML4 that maps the currently
/// executing instruction stream and stack.
#[inline]
pub unsafe fn write_cr3(v: u64) {
    // SAFETY: caller guarantees the new tables map the current RIP and RSP.
    unsafe { asm!("mov cr3, {}", in(reg) v, options(nostack, preserves_flags)) };
}

/// Read CR4.
#[inline]
pub fn read_cr4() -> u64 {
    let v: u64;
    // SAFETY: read-only.
    unsafe { asm!("mov {}, cr4", out(reg) v, options(nomem, nostack, preserves_flags)) };
    v
}

/// Write CR4.
///
/// # Safety
/// CR4 gates PAE, global pages, SMEP/SMAP and the FPU state layout.
#[inline]
pub unsafe fn write_cr4(v: u64) {
    // SAFETY: caller guarantees validity.
    unsafe { asm!("mov cr4, {}", in(reg) v, options(nomem, nostack, preserves_flags)) };
}

/// Invalidate a single TLB entry.
///
/// # Safety
/// Safe in isolation, but callers must have already updated the page tables,
/// otherwise the flush is a no-op against a stale entry.
#[inline]
pub unsafe fn invlpg(addr: u64) {
    // SAFETY: `invlpg` only affects the TLB.
    unsafe { asm!("invlpg [{}]", in(reg) addr, options(nostack, preserves_flags)) };
}

/// Read the time-stamp counter.
#[inline]
pub fn rdtsc() -> u64 {
    let (low, high): (u32, u32);
    // SAFETY: `rdtsc` is unprivileged and side-effect free.
    unsafe { asm!("rdtsc", out("eax") low, out("edx") high, options(nomem, nostack, preserves_flags)) };
    ((high as u64) << 32) | (low as u64)
}

/// Raw CPUID leaf query, returning `(eax, ebx, ecx, edx)`.
///
/// Delegates to `core::arch` rather than hand-rolling the RBX dance, because
/// RBX is the LLVM base pointer and clobbering it from `asm!` is a footgun.
#[inline]
pub fn cpuid(leaf: u32, subleaf: u32) -> (u32, u32, u32, u32) {
    // SAFETY: CPUID is unprivileged, has no side effects, and every x86-64 CPU
    // implements it (the boot trampoline already verified this).
    let r = core::arch::x86_64::__cpuid_count(leaf, subleaf);
    (r.eax, r.ebx, r.ecx, r.edx)
}

/// Highest supported basic CPUID leaf.
#[inline]
pub fn cpuid_max_leaf() -> u32 {
    cpuid(0, 0).0
}

/// Highest supported extended CPUID leaf.
#[inline]
pub fn cpuid_max_extended_leaf() -> u32 {
    cpuid(0x8000_0000, 0).0
}
