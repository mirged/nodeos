//! The scheduler.
//!
//! # Interrupt discipline
//!
//! The NodeOS kernel runs with interrupts *disabled*. Userspace runs with them
//! enabled; a trap or syscall enters through a gate that clears `IF`, and the
//! kernel does not set it again until it leaves. There are exactly two
//! exceptions, both explicit: the idle loop's `sti; hlt`, and a kernel thread
//! that has opted in.
//!
//! That single decision buys a lot:
//!
//! * No nested interrupt handlers, so no re-entrancy analysis for every
//!   spin lock in the kernel.
//! * No red-zone hazard from an interrupt landing inside a compiler-generated
//!   leaf function.
//! * Context switching is not a special case: a switch can only happen where
//!   the kernel chose to allow one.
//!
//! The cost is interrupt latency bounded by the longest kernel operation, which
//! for a microkernel whose operations are "copy a message" and "map a page" is
//! a few microseconds. Real-time work is served by *preemption points*, not by
//! preemptible kernel code.

pub mod params;
pub mod runqueue;

use alloc::sync::Arc;
use alloc::vec::Vec;
use core::sync::atomic::Ordering;

use crate::arch::x86_64::percpu::{self, MAX_CPUS};
use crate::arch::x86_64::trap::TrapFrame;
use crate::arch::x86_64::{cpu, interrupt, segmentation};
use crate::obj::KernelObject;
use crate::sync::{IrqSpinLock, Once};
use crate::task::process::Process;
use crate::task::thread::{Thread, ThreadEntry, ThreadState};
use crate::time;

pub use params::{SchedClass, SchedParams};
pub use runqueue::RunQueue;

/// Per-CPU scheduler state.
struct CpuScheduler {
    queue: IrqSpinLock<RunQueue>,
    current: IrqSpinLock<Option<Arc<Thread>>>,
    idle: IrqSpinLock<Option<Arc<Thread>>>,
    /// Threads that have exited and are waiting to have their stacks reclaimed.
    /// A thread cannot free its own kernel stack while standing on it, so the
    /// next thread to run does it.
    reaper: IrqSpinLock<Vec<Arc<Thread>>>,
    /// Timestamp of the last scheduling decision, for CPU accounting.
    last_switch_ns: IrqSpinLock<u64>,
}

impl CpuScheduler {
    const fn new() -> Self {
        Self {
            queue: IrqSpinLock::new(RunQueue::new()),
            current: IrqSpinLock::new(None),
            idle: IrqSpinLock::new(None),
            reaper: IrqSpinLock::new(Vec::new()),
            last_switch_ns: IrqSpinLock::new(0),
        }
    }
}

static SCHEDULERS: [CpuScheduler; MAX_CPUS] = [const { CpuScheduler::new() }; MAX_CPUS];
static KERNEL_PROCESS: Once<Arc<Process>> = Once::new();
static ENABLED: core::sync::atomic::AtomicBool = core::sync::atomic::AtomicBool::new(false);

fn this_cpu() -> usize {
    if percpu::is_ready() {
        percpu::current().cpu_id.load(Ordering::Relaxed) as usize
    } else {
        0
    }
}

fn scheduler(cpu: usize) -> &'static CpuScheduler {
    &SCHEDULERS[cpu.min(MAX_CPUS - 1)]
}

/// The kernel's pseudo-process, owner of kernel threads.
pub fn kernel_process() -> &'static Arc<Process> {
    KERNEL_PROCESS.get()
}

/// The thread currently running on this CPU.
pub fn current_thread() -> Option<Arc<Thread>> {
    scheduler(this_cpu()).current.lock().clone()
}

/// The process the current thread belongs to.
pub fn current_process() -> Option<Arc<Process>> {
    current_thread().and_then(|t| t.process())
}

/// Whether the scheduler has been started.
pub fn is_enabled() -> bool {
    ENABLED.load(Ordering::Acquire)
}

/// Start the scheduler on the boot CPU.
///
/// The context that calls this becomes the CPU's idle thread, which is why it
/// must be the boot path and not a helper: the current stack becomes the idle
/// thread's stack.
///
/// # Safety
/// Must run once per CPU, with interrupts disabled, after the memory manager
/// and the timer are up.
pub unsafe fn init_boot_cpu() {
    let kernel = Process::new_kernel();
    KERNEL_PROCESS.init(kernel.clone());

    // SAFETY: we are executing on the boot stack, which becomes the idle
    // thread's stack; the scheduler has not started, so nothing else can be
    // running on this CPU.
    let idle = unsafe { Thread::adopt_current(&kernel, "idle/0", SchedParams::idle()) };
    let cpu = this_cpu();
    *scheduler(cpu).idle.lock() = Some(idle.clone());
    *scheduler(cpu).current.lock() = Some(idle);
    *scheduler(cpu).last_switch_ns.lock() = time::now_ns();

    // SAFETY: the timer vector already has a handler; this only adds the
    // scheduler's hook to it.
    unsafe { interrupt::set_tick_handler(tick) };
    ENABLED.store(true, Ordering::Release);
}

/// Spawn a kernel thread.
pub fn spawn_kernel_thread(name: &str, func: fn(usize), arg: usize, params: SchedParams) -> Option<Arc<Thread>> {
    let thread = Thread::new(kernel_process(), name, ThreadEntry::Kernel { func, arg }, params)?;
    enqueue(&thread);
    Some(thread)
}

/// Make a thread runnable.
pub fn enqueue(thread: &Arc<Thread>) {
    let params = thread.params();
    thread.timeslice_ns.store(params.class.default_timeslice_ns(), Ordering::Relaxed);
    // Placement policy: keep a thread on the CPU it last ran on when affinity
    // allows, otherwise pick the shortest queue it is permitted to use.
    let preferred = thread.last_cpu();
    let online = percpu::online_cpus();
    let cpu = if preferred != u32::MAX && (preferred as usize) < online && params.may_run_on(preferred) {
        preferred as usize
    } else {
        (0..online)
            .filter(|c| params.may_run_on(*c as u32))
            .min_by_key(|c| scheduler(*c).queue.lock().len())
            .unwrap_or(0)
    };
    thread.set_state(ThreadState::Ready);
    scheduler(cpu).queue.lock().enqueue(thread.clone());
}

/// Make a blocked thread runnable. Returns `true` if it was actually woken.
///
/// A thread that is *not* blocked yet is the interesting case: it is somewhere
/// between re-checking its condition and [`block_current`], and dropping the
/// wakeup here would leave it asleep on a condition that is already true. The
/// wakeup is recorded on the thread instead, and `block_current` consumes it.
pub fn wake(thread: &Arc<Thread>) -> bool {
    match thread.state() {
        ThreadState::Blocked => {
            if thread.transition(ThreadState::Blocked, ThreadState::Ready) {
                enqueue(thread);
                true
            } else {
                false
            }
        }
        ThreadState::Dead => false,
        _ => {
            thread.set_wake_pending();
            false
        }
    }
}

/// Block the current thread until something wakes it.
///
/// The caller must already have parked the thread on whatever wait queue will
/// wake it, *before* calling this, or the wakeup can be lost.
///
/// Returns without blocking if a wakeup arrived while the caller was still
/// deciding to block -- see [`Thread::set_wake_pending`]. Callers are loops
/// that re-check their condition, so an early return costs one extra pass.
pub fn block_current() {
    let Some(thread) = current_thread() else { return };
    if thread.take_wake_pending() {
        return;
    }
    thread.set_state(ThreadState::Blocked);
    schedule();
}

/// Give up the CPU voluntarily.
pub fn yield_now() {
    let Some(thread) = current_thread() else { return };
    if thread.state() == ThreadState::Running {
        thread.set_state(ThreadState::Ready);
    }
    schedule();
}

/// Terminate the current thread. Never returns.
pub fn exit_current(code: i64) -> ! {
    let thread = current_thread().expect("exit_current with no current thread");
    thread.set_exit_code(code);
    thread.set_state(ThreadState::Dead);
    thread.header().signals().update(crate::obj::Signals::THREAD_TERMINATED, crate::obj::Signals::NONE);
    if let Some(process) = thread.process() {
        process.thread_exited(&thread);
    }
    schedule();
    // `schedule` will never pick a dead thread again, so this is unreachable --
    // but if the scheduler is broken, halting is better than falling through
    // into whatever follows.
    cpu::halt_forever()
}

/// Kill `thread` from outside.
pub fn kill(thread: &Arc<Thread>) {
    thread.set_state(ThreadState::Dead);
    thread.header().signals().update(crate::obj::Signals::THREAD_TERMINATED, crate::obj::Signals::NONE);
    for cpu in 0..MAX_CPUS {
        scheduler(cpu).queue.lock().remove(thread);
    }
}

/// The scheduler tick, called from the timer interrupt.
fn tick(_frame: &mut TrapFrame) {
    if !is_enabled() {
        return;
    }
    // Sleepers first: a thread whose deadline has just passed should be on the
    // run queue before this tick decides whether to preempt, or it waits an
    // extra millisecond for no reason.
    time::wake_expired();
    let cpu = this_cpu();
    let sched = scheduler(cpu);

    let now = time::now_ns();
    let delta = {
        let mut last = sched.last_switch_ns.lock();
        let delta = now.saturating_sub(*last);
        *last = now;
        delta
    };

    let Some(current) = sched.current.lock().clone() else { return };
    current.charge(delta);

    // Charge virtual runtime inversely to weight: a thread with twice the
    // weight accumulates virtual time at half the rate, so the fair-share queue
    // (ordered by virtual time) gives it twice the CPU.
    let params = current.params();
    if params.class == SchedClass::Normal {
        let weight = params.effective_weight().max(1) as u64;
        let scaled = delta.saturating_mul(params::NICE0_WEIGHT as u64) / weight;
        current.vruntime.fetch_add(scaled, Ordering::Relaxed);
    }
    if params.class == SchedClass::RealTime && params.budget_ns > 0 {
        let mut updated = params;
        updated.budget_used_ns = updated.budget_used_ns.saturating_add(delta);
        current.set_params(updated);
    }

    // Timeslice expiry, or a higher class becoming runnable.
    let remaining = current.timeslice_ns.load(Ordering::Relaxed);
    let expired = if remaining <= delta {
        current.timeslice_ns.store(0, Ordering::Relaxed);
        true
    } else {
        current.timeslice_ns.store(remaining - delta, Ordering::Relaxed);
        false
    };

    let preempt = expired || sched.queue.lock().should_preempt(&params) || params.over_budget();
    if preempt {
        percpu::current().need_resched.store(true, Ordering::Relaxed);
    }
}

/// Run the scheduler: pick the next thread and switch to it.
///
/// Safe to call from any kernel context that holds no locks.
pub fn schedule() {
    if !is_enabled() {
        return;
    }
    let flags = cpu::interrupts_disable_save();
    let cpu = this_cpu();
    let sched = scheduler(cpu);
    percpu::current().need_resched.store(false, Ordering::Relaxed);

    // Reclaim stacks of threads that exited while running on this CPU.
    let dead: Vec<Arc<Thread>> = core::mem::take(&mut *sched.reaper.lock());
    drop(dead);

    let current = sched.current.lock().clone();
    let next = {
        let mut queue = sched.queue.lock();
        // Put the outgoing thread back before choosing, so that a thread which
        // is still the best candidate simply gets picked again.
        if let Some(thread) = &current {
            match thread.state() {
                ThreadState::Running | ThreadState::Ready => {
                    let is_idle = sched
                        .idle
                        .lock()
                        .as_ref()
                        .is_some_and(|idle| Arc::ptr_eq(idle, thread));
                    if !is_idle {
                        thread.set_state(ThreadState::Ready);
                        let params = thread.params();
                        thread
                            .timeslice_ns
                            .store(params.class.default_timeslice_ns(), Ordering::Relaxed);
                        queue.enqueue(thread.clone());
                    }
                }
                _ => {}
            }
        }
        queue.dequeue()
    };

    let next = match next {
        Some(thread) => thread,
        None => {
            // Nothing runnable: fall back to the idle thread.
            let idle = sched.idle.lock().clone();
            match idle {
                Some(idle) => idle,
                None => {
                    // SAFETY: restoring the caller's interrupt state.
                    unsafe { cpu::interrupts_restore(flags) };
                    return;
                }
            }
        }
    };

    if let Some(current) = &current {
        if Arc::ptr_eq(current, &next) {
            next.set_state(ThreadState::Running);
            // SAFETY: restoring the caller's interrupt state.
            unsafe { cpu::interrupts_restore(flags) };
            return;
        }
    }

    // Commit the decision before switching stacks: once `switch_to` runs, this
    // function's locals belong to the *outgoing* thread and will not be seen
    // again until it is scheduled back in.
    next.set_state(ThreadState::Running);
    next.set_last_cpu(cpu as u32);
    let params = next.params();
    next.timeslice_ns.store(params.class.default_timeslice_ns(), Ordering::Relaxed);

    // Traps from ring 3 must land on the incoming thread's kernel stack.
    let stack_top = next.kernel_stack_top();
    // SAFETY: `stack_top` is the top of a stack owned exclusively by `next`.
    unsafe { segmentation::set_kernel_stack(cpu, stack_top) };
    // SAFETY: the syscall entry stub reads this slot.
    unsafe { percpu::current().kernel_rsp.get().write(stack_top) };
    // The incoming thread's userspace thread pointer. `thread_set_fs_base`
    // only accepts canonical lower-half values, so this cannot fault.
    // SAFETY: the kernel never addresses through `FS`.
    unsafe { cpu::wrmsr(cpu::MSR_FS_BASE, next.fs_base()) };

    // Switch address space if the incoming thread lives in a different one.
    if let Some(process) = next.process() {
        let cr3 = process.space().cr3();
        if cpu::read_cr3() != cr3 {
            // SAFETY: `cr3` is a live PML4 whose kernel half is identical to the
            // outgoing one, so the code and stack we are executing on stay
            // mapped across the switch.
            unsafe { cpu::write_cr3(cr3) };
        }
    }

    *sched.last_switch_ns.lock() = time::now_ns();

    let next_rsp = next.saved_rsp();
    let outgoing = current.clone();
    *sched.current.lock() = Some(next);

    match outgoing {
        Some(thread) => {
            if thread.state() == ThreadState::Dead {
                sched.reaper.lock().push(thread.clone());
            }
            // SAFETY: `thread` is the thread executing this very code, so its
            // saved-stack slot is free; `next_rsp` is a kernel stack carrying a
            // valid switch frame. No locks are held across the switch.
            unsafe { crate::task::context::switch_to(thread.saved_rsp_ptr(), next_rsp) };
        }
        None => {
            // First entry into the scheduler with no outgoing thread: switch
            // onto `next` and never come back.
            let mut discard = 0u64;
            // SAFETY: `discard` is a real stack slot; nothing will ever read it
            // because there is no outgoing thread to resume.
            unsafe { crate::task::context::switch_to(&mut discard, next_rsp) };
        }
    }

    // SAFETY: we are back on our own stack; restore the caller's flags.
    unsafe { cpu::interrupts_restore(flags) };
}

/// Reschedule if an interrupt handler asked for it. Called at every preemption
/// point: on the way out of a syscall, and in the idle loop.
pub fn preempt_point() {
    if !is_enabled() || !percpu::is_ready() {
        return;
    }
    if percpu::current().need_resched.load(Ordering::Relaxed) {
        schedule();
    }
}

/// The idle loop.
///
/// `sti; hlt` is architecturally atomic, so an interrupt cannot arrive between
/// enabling interrupts and halting and thereby be lost.
pub fn idle_loop() -> ! {
    loop {
        preempt_point();
        // SAFETY: the idle loop is by definition an interruptible context, and
        // holds no locks.
        unsafe { cpu::enable_interrupts_and_halt() };
    }
}

/// Per-class runnable counts on `cpu`, for diagnostics and self-tests.
pub fn queue_counts(cpu: usize) -> [usize; 4] {
    scheduler(cpu).queue.lock().counts()
}

/// Total runnable threads across all online CPUs.
pub fn runnable() -> usize {
    (0..percpu::online_cpus()).map(|c| scheduler(c).queue.lock().len()).sum()
}
