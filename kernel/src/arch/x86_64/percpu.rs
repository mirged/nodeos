//! Per-CPU state, reachable through `GS`.
//!
//! # The GS convention
//!
//! While executing in ring 0, `GS_BASE` points at this CPU's [`PerCpu`] block
//! and `KERNEL_GS_BASE` holds whatever userspace had. While executing in
//! ring 3 the two are swapped. Every kernel entry from user mode therefore
//! begins with `swapgs` and every return to user mode ends with one. Entries
//! from kernel mode must *not* swap, which is why the trap stubs test
//! `CS & 3` before deciding.

use core::cell::UnsafeCell;
use core::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering};

use super::cpu;

/// Maximum number of logical CPUs NodeOS supports.
pub const MAX_CPUS: usize = 16;

/// Per-CPU kernel state.
///
/// The layout of the first two fields is depended upon by assembly: offset 0
/// is a self-pointer (so `mov rax, gs:0` yields the block's linear address)
/// and offset 8 is the scratch slot the `SYSCALL` entry stub uses to park the
/// user stack pointer before it has a stack of its own.
#[repr(C, align(64))]
pub struct PerCpu {
    /// Offset 0: linear address of this very structure.
    pub self_ptr: UnsafeCell<u64>,
    /// Offset 8: scratch space for the syscall entry stub (saved user RSP).
    pub user_rsp_scratch: UnsafeCell<u64>,
    /// Offset 16: kernel stack to load on `SYSCALL` entry.
    pub kernel_rsp: UnsafeCell<u64>,

    /// Logical CPU index.
    pub cpu_id: AtomicU32,
    /// Local APIC ID.
    pub lapic_id: AtomicU32,
    /// Pointer to the currently running `Thread`, or null when idle.
    pub current_thread: AtomicU64,
    /// Pointer to this CPU's idle `Thread`.
    pub idle_thread: AtomicU64,
    /// Set by an interrupt handler to request a reschedule at the next
    /// preemption point.
    pub need_resched: AtomicBool,
    /// Depth of nested "no preemption" regions.
    pub preempt_disable: AtomicU32,
    /// Number of interrupts serviced, for diagnostics.
    pub interrupt_count: AtomicU64,
    /// Monotonic tick counter maintained by the timer interrupt.
    pub ticks: AtomicU64,
    /// Depth of nested page faults; >1 means the fault handler itself faulted.
    pub fault_depth: AtomicU32,
}

// SAFETY: every field is either atomic or only touched by the owning CPU (with
// interrupts disabled), and the `UnsafeCell` fields are written exclusively by
// that CPU's entry/exit assembly.
unsafe impl Sync for PerCpu {}

impl PerCpu {
    const fn new() -> Self {
        Self {
            self_ptr: UnsafeCell::new(0),
            user_rsp_scratch: UnsafeCell::new(0),
            kernel_rsp: UnsafeCell::new(0),
            cpu_id: AtomicU32::new(u32::MAX),
            lapic_id: AtomicU32::new(u32::MAX),
            current_thread: AtomicU64::new(0),
            idle_thread: AtomicU64::new(0),
            need_resched: AtomicBool::new(false),
            preempt_disable: AtomicU32::new(0),
            interrupt_count: AtomicU64::new(0),
            ticks: AtomicU64::new(0),
            fault_depth: AtomicU32::new(0),
        }
    }
}

#[allow(clippy::declare_interior_mutable_const)]
const PERCPU_INIT: PerCpu = PerCpu::new();
static PERCPU: [PerCpu; MAX_CPUS] = [PERCPU_INIT; MAX_CPUS];

/// Number of CPUs that have run [`init`]. Everything that balances work across
/// CPUs must consult this rather than [`MAX_CPUS`]: placing a runnable thread
/// on a CPU that was never brought up is an invisible way to lose it forever.
static ONLINE: AtomicU32 = AtomicU32::new(0);

/// How many CPUs are online.
pub fn online_cpus() -> usize {
    ONLINE.load(Ordering::Acquire).max(1) as usize
}

/// Install `GS_BASE`/`KERNEL_GS_BASE` for the calling CPU.
///
/// # Safety
/// Must be called exactly once per CPU, early, with interrupts disabled and
/// before anything calls [`current`].
pub unsafe fn init(cpu_id: u32, lapic_id: u32) {
    assert!((cpu_id as usize) < MAX_CPUS, "cpu id {cpu_id} exceeds MAX_CPUS");
    let block = &PERCPU[cpu_id as usize];
    block.cpu_id.store(cpu_id, Ordering::Relaxed);
    ONLINE.fetch_max(cpu_id + 1, Ordering::AcqRel);
    block.lapic_id.store(lapic_id, Ordering::Relaxed);
    let addr = block as *const PerCpu as u64;
    // SAFETY: exclusive access during single-threaded CPU bring-up.
    unsafe { block.self_ptr.get().write(addr) };
    // SAFETY: pointing GS at a static of the right shape is exactly what the
    // entry assembly expects; both bases are set so a stray `swapgs` (from a
    // kernel-mode NMI, say) still leaves GS usable.
    unsafe {
        cpu::wrmsr(cpu::MSR_GS_BASE, addr);
        cpu::wrmsr(cpu::MSR_KERNEL_GS_BASE, addr);
    }
}

/// The calling CPU's per-CPU block.
///
/// # Panics
/// Panics if [`init`] has not run on this CPU (`GS_BASE` still zero).
#[inline]
pub fn current() -> &'static PerCpu {
    let addr: u64;
    // SAFETY: `GS_BASE` was installed by `init`; offset 0 is the self-pointer
    // by the `#[repr(C)]` layout contract documented on `PerCpu`.
    unsafe { core::arch::asm!("mov {}, gs:[0]", out(reg) addr, options(nostack, preserves_flags)) };
    assert!(addr != 0, "per-CPU block not initialised on this CPU");
    // SAFETY: the self-pointer was written by `init` from a `&'static PerCpu`.
    unsafe { &*(addr as *const PerCpu) }
}

/// Whether per-CPU state is available yet on this CPU.
///
/// Reads `GS_BASE` from the MSR rather than dereferencing `gs:[0]`, because
/// before `init` runs `GS_BASE` is zero and `gs:[0]` would silently read the
/// real-mode interrupt vector table instead of faulting.
#[inline]
pub fn is_ready() -> bool {
    // SAFETY: `IA32_GS_BASE` exists on every x86-64 CPU.
    unsafe { cpu::rdmsr(cpu::MSR_GS_BASE) != 0 }
}

/// The per-CPU block for `cpu_id`, for cross-CPU operations such as IPIs.
pub fn by_id(cpu_id: usize) -> Option<&'static PerCpu> {
    PERCPU.get(cpu_id)
}
