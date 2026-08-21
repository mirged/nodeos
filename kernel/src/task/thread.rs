//! Threads.
//!
//! A thread is a schedulable execution context: CPU state, a kernel stack, a
//! scheduler entity and an IPC state slot. It is *not* an address space and not
//! a capability holder -- those belong to the [`super::process::Process`] it
//! runs in. Splitting them this way means "create a thread" and "create a
//! protection domain" are different operations with different authority, which
//! is exactly the distinction `fork()` destroys.

use alloc::string::String;
use alloc::sync::{Arc, Weak};
use core::cell::UnsafeCell;
use core::sync::atomic::{AtomicI64, AtomicU32, AtomicU64, Ordering};

use crate::arch::x86_64::trap::TrapFrame;
use crate::arch::x86_64::{percpu, segmentation};
use crate::obj::{KernelObject, ObjectHeader, ObjectType, Signals};
use crate::sched::params::SchedParams;
use crate::sync::IrqSpinLock;

use super::context::SwitchFrame;
use super::process::Process;
use super::stack::KernelStack;

/// Lifecycle state of a thread.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
#[repr(u32)]
pub enum ThreadState {
    /// On a run queue, waiting for a CPU.
    Ready = 0,
    /// Currently executing.
    Running = 1,
    /// Waiting for a signal on some object.
    Blocked = 2,
    /// Stopped by a supervisor; will not run until resumed.
    Suspended = 3,
    /// Finished. The object may outlive this state while handles remain.
    Dead = 4,
}

impl ThreadState {
    fn from_u32(value: u32) -> Self {
        match value {
            0 => Self::Ready,
            1 => Self::Running,
            2 => Self::Blocked,
            3 => Self::Suspended,
            _ => Self::Dead,
        }
    }
}

/// What a thread should start doing.
pub enum ThreadEntry {
    /// A kernel thread: call `func(arg)`, then exit.
    Kernel {
        /// Entry function.
        func: fn(usize),
        /// Opaque argument.
        arg: usize,
    },
    /// A user thread: drop to ring 3 at `entry` with `stack`.
    User {
        /// Ring-3 instruction pointer.
        entry: u64,
        /// Ring-3 stack pointer.
        stack: u64,
        /// First argument, in RDI.
        arg0: u64,
        /// Second argument, in RSI.
        arg1: u64,
    },
    /// The boot context, adopted rather than created.
    Adopted,
}

static NEXT_TID: AtomicU64 = AtomicU64::new(1);

/// A schedulable execution context.
pub struct Thread {
    header: ObjectHeader,
    tid: u64,
    process: Weak<Process>,
    state: AtomicU32,
    /// Kernel stack. `None` for the adopted boot context, which runs on the
    /// stack the linker reserved.
    stack: Option<KernelStack>,
    /// Saved kernel stack pointer while this thread is not running.
    ///
    /// Written only by [`crate::sched::schedule`] with interrupts disabled, and
    /// only for a thread that is not currently executing on any CPU.
    saved_rsp: UnsafeCell<u64>,
    entry: IrqSpinLock<Option<ThreadEntry>>,
    params: IrqSpinLock<SchedParams>,
    exit_code: AtomicI64,
    /// CPU this thread last ran on, for affinity and cache warmth.
    last_cpu: AtomicU32,
    /// Accumulated runtime in nanoseconds.
    runtime_ns: AtomicU64,
    /// Virtual runtime, used by the fair-share class.
    pub(crate) vruntime: AtomicU64,
    /// Nanoseconds left in the current timeslice.
    pub(crate) timeslice_ns: AtomicU64,
    /// Trap frame of the thread while it is stopped in an exception.
    pub(crate) fault_frame: IrqSpinLock<Option<TrapFrame>>,
}

// SAFETY: `saved_rsp` is only touched by the scheduler, with interrupts
// disabled, for a thread that is provably not running on any CPU.
unsafe impl Sync for Thread {}

impl Thread {
    /// Create a thread that will run `entry` inside `process`.
    pub fn new(process: &Arc<Process>, name: &str, entry: ThreadEntry, params: SchedParams) -> Option<Arc<Self>> {
        let stack = KernelStack::new()?;
        let stack_top = stack.top();

        // Plant an initial switch frame so the first `switch_to` into this
        // thread returns straight into the trampoline. The trap frame area is
        // reserved above it for the user-mode entry path.
        let frame_area = stack_top - core::mem::size_of::<TrapFrame>() as u64;
        let switch_rsp = frame_area - core::mem::size_of::<SwitchFrame>() as u64;
        let switch_frame = SwitchFrame {
            r15: 0,
            r14: 0,
            r13: 0,
            r12: 0,
            rbx: 0,
            rbp: 0,
            return_address: thread_trampoline as *const () as u64,
        };
        // SAFETY: `switch_rsp` is inside the freshly allocated kernel stack,
        // which nothing else references yet, and is 8-byte aligned by
        // construction.
        unsafe { core::ptr::write(switch_rsp as *mut SwitchFrame, switch_frame) };

        let thread = Arc::new(Self {
            header: ObjectHeader::new(ObjectType::Thread, Signals::NONE),
            tid: NEXT_TID.fetch_add(1, Ordering::Relaxed),
            process: Arc::downgrade(process),
            state: AtomicU32::new(ThreadState::Ready as u32),
            stack: Some(stack),
            saved_rsp: UnsafeCell::new(switch_rsp),
            entry: IrqSpinLock::new(Some(entry)),
            params: IrqSpinLock::new(params),
            exit_code: AtomicI64::new(0),
            last_cpu: AtomicU32::new(u32::MAX),
            runtime_ns: AtomicU64::new(0),
            vruntime: AtomicU64::new(0),
            timeslice_ns: AtomicU64::new(0),
            fault_frame: IrqSpinLock::new(None),
        });
        thread.header.set_name(name);
        process.add_thread(&thread);
        Some(thread)
    }

    /// Adopt the currently executing boot context as a thread object.
    ///
    /// # Safety
    /// Must be called once per CPU, from the context that will become that
    /// CPU's idle thread, before the scheduler starts.
    pub unsafe fn adopt_current(process: &Arc<Process>, name: &str, params: SchedParams) -> Arc<Self> {
        let thread = Arc::new(Self {
            header: ObjectHeader::new(ObjectType::Thread, Signals::NONE),
            tid: NEXT_TID.fetch_add(1, Ordering::Relaxed),
            process: Arc::downgrade(process),
            state: AtomicU32::new(ThreadState::Running as u32),
            stack: None,
            saved_rsp: UnsafeCell::new(0),
            entry: IrqSpinLock::new(Some(ThreadEntry::Adopted)),
            params: IrqSpinLock::new(params),
            exit_code: AtomicI64::new(0),
            last_cpu: AtomicU32::new(0),
            runtime_ns: AtomicU64::new(0),
            vruntime: AtomicU64::new(0),
            timeslice_ns: AtomicU64::new(0),
            fault_frame: IrqSpinLock::new(None),
        });
        thread.header.set_name(name);
        process.add_thread(&thread);
        thread
    }

    /// Thread id, unique for the lifetime of the boot.
    pub fn tid(&self) -> u64 {
        self.tid
    }

    /// The process this thread belongs to, if it is still alive.
    pub fn process(&self) -> Option<Arc<Process>> {
        self.process.upgrade()
    }

    /// Current lifecycle state.
    pub fn state(&self) -> ThreadState {
        ThreadState::from_u32(self.state.load(Ordering::Acquire))
    }

    /// Set the lifecycle state.
    pub fn set_state(&self, state: ThreadState) {
        self.state.store(state as u32, Ordering::Release);
    }

    /// Compare-and-set the lifecycle state.
    pub fn transition(&self, from: ThreadState, to: ThreadState) -> bool {
        self.state
            .compare_exchange(from as u32, to as u32, Ordering::AcqRel, Ordering::Acquire)
            .is_ok()
    }

    /// Scheduling parameters.
    pub fn params(&self) -> SchedParams {
        *self.params.lock()
    }

    /// Replace the scheduling parameters.
    pub fn set_params(&self, params: SchedParams) {
        *self.params.lock() = params;
    }

    /// Pointer to the saved stack-pointer slot, for the context switch.
    ///
    /// # Safety
    /// The caller must be the scheduler, with interrupts disabled, and this
    /// thread must not be executing on another CPU.
    pub unsafe fn saved_rsp_ptr(&self) -> *mut u64 {
        self.saved_rsp.get()
    }

    /// The saved kernel stack pointer.
    pub fn saved_rsp(&self) -> u64 {
        // SAFETY: a `u64` read; races can only observe a stale value, and the
        // scheduler only reads this for threads that are not running.
        unsafe { *self.saved_rsp.get() }
    }

    /// Top of the kernel stack, which is what `TSS.RSP0` must hold while this
    /// thread runs.
    pub fn kernel_stack_top(&self) -> u64 {
        match &self.stack {
            Some(stack) => stack.top(),
            None => segmentation::kernel_stack(0),
        }
    }

    /// Whether `addr` is this thread's kernel stack guard page.
    pub fn is_stack_guard(&self, addr: u64) -> bool {
        self.stack.as_ref().is_some_and(|s| s.is_guard(addr))
    }

    /// Exit code, valid once the thread is [`ThreadState::Dead`].
    pub fn exit_code(&self) -> i64 {
        self.exit_code.load(Ordering::Acquire)
    }

    /// Record the exit code and mark the thread dead.
    pub fn set_exit_code(&self, code: i64) {
        self.exit_code.store(code, Ordering::Release);
    }

    /// Accumulated CPU time in nanoseconds.
    pub fn runtime_ns(&self) -> u64 {
        self.runtime_ns.load(Ordering::Relaxed)
    }

    /// Charge `delta` nanoseconds of CPU time.
    pub fn charge(&self, delta: u64) {
        self.runtime_ns.fetch_add(delta, Ordering::Relaxed);
    }

    /// CPU this thread last ran on.
    pub fn last_cpu(&self) -> u32 {
        self.last_cpu.load(Ordering::Relaxed)
    }

    /// Record the CPU this thread is running on.
    pub fn set_last_cpu(&self, cpu: u32) {
        self.last_cpu.store(cpu, Ordering::Relaxed);
    }

    /// Debug name.
    pub fn name(&self) -> String {
        let mut buffer = [0u8; crate::obj::NAME_LEN];
        String::from(self.header.name_into(&mut buffer))
    }

    /// Take the pending entry description, leaving `None`.
    pub(crate) fn take_entry(&self) -> Option<ThreadEntry> {
        self.entry.lock().take()
    }
}

impl KernelObject for Thread {
    fn header(&self) -> &ObjectHeader {
        &self.header
    }

    fn as_any(&self) -> &dyn core::any::Any {
        self
    }
}

/// Where a freshly switched-to thread begins executing.
///
/// Reached by `switch_to`'s `ret`, so it starts with a clean stack and no
/// caller. It must never return.
extern "C" fn thread_trampoline() -> ! {
    // The scheduler switched to us with interrupts disabled; from here on this
    // thread is an ordinary interruptible context.
    let thread = crate::sched::current_thread().expect("trampoline without a current thread");
    thread.set_state(ThreadState::Running);

    match thread.take_entry() {
        Some(ThreadEntry::Kernel { func, arg }) => {
            // SAFETY: kernel threads run with interrupts enabled; they hold no
            // locks at this point.
            unsafe { crate::arch::x86_64::cpu::interrupts_enable() };
            func(arg);
            crate::sched::exit_current(0);
        }
        Some(ThreadEntry::User { entry, stack, arg0, arg1 }) => {
            enter_user(&thread, entry, stack, arg0, arg1);
        }
        Some(ThreadEntry::Adopted) | None => {
            // An adopted context resumed through the scheduler: just continue.
            // SAFETY: same reasoning as the kernel-thread case.
            unsafe { crate::arch::x86_64::cpu::interrupts_enable() };
            crate::sched::exit_current(0);
        }
    }
}

/// Build a ring-3 trap frame on this thread's kernel stack and `iretq` into it.
fn enter_user(thread: &Arc<Thread>, entry: u64, stack: u64, arg0: u64, arg1: u64) -> ! {
    use crate::arch::x86_64::segmentation::{USER_CS, USER_SS};

    let stack_top = thread.kernel_stack_top();
    let frame_ptr = (stack_top - core::mem::size_of::<TrapFrame>() as u64) as *mut TrapFrame;

    let mut frame = TrapFrame::default();
    frame.rip = entry;
    frame.cs = USER_CS as u64;
    // IF set so the thread is preemptible; bit 1 is the reserved always-one bit.
    frame.rflags = (1 << 9) | (1 << 1);
    frame.rsp = stack;
    frame.ss = USER_SS as u64;
    frame.rdi = arg0;
    frame.rsi = arg1;

    // SAFETY: `frame_ptr` is inside this thread's own kernel stack, above the
    // switch frame we are currently running on, and is 8-byte aligned.
    unsafe { core::ptr::write(frame_ptr, frame) };

    let cpu = percpu::current().cpu_id.load(Ordering::Relaxed) as usize;
    // SAFETY: `stack_top` is the top of this thread's kernel stack, which is
    // exactly where a trap from ring 3 must land.
    unsafe { segmentation::set_kernel_stack(cpu, stack_top) };
    // SAFETY: the syscall entry stub reads this to find a kernel stack.
    unsafe { percpu::current().kernel_rsp.get().write(stack_top) };

    // SAFETY: the frame describes a ring-3 context on this thread's stack and
    // `TSS.RSP0` now points at that stack.
    unsafe { super::context::enter_user_mode(frame_ptr) }
}
