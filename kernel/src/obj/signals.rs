//! Object signals and wait queues.
//!
//! Every kernel object exposes a small set of *signal bits* describing its
//! observable state. `wait` blocks a thread until any of a requested set is
//! asserted on any of a set of objects. This is the only blocking primitive in
//! the kernel that userspace can reach: there are no per-object `read_blocking`
//! entry points, which keeps the number of places a thread can go to sleep
//! small enough to audit.

use alloc::sync::{Arc, Weak};
use alloc::vec::Vec;
use core::sync::atomic::{AtomicU32, Ordering};

use crate::sync::IrqSpinLock;
use crate::task::thread::Thread;

/// A set of observable state bits on an object.
#[derive(Clone, Copy, PartialEq, Eq, Default)]
pub struct Signals(pub u32);

impl Signals {
    /// No signals.
    pub const NONE: Self = Self(0);

    /// A channel has at least one message queued.
    pub const READABLE: Self = Self(1 << 0);
    /// A channel has room for another message.
    pub const WRITABLE: Self = Self(1 << 1);
    /// The far end of a channel is gone.
    pub const PEER_CLOSED: Self = Self(1 << 2);
    /// A process has exited.
    pub const PROCESS_TERMINATED: Self = Self(1 << 3);
    /// A thread has exited.
    pub const THREAD_TERMINATED: Self = Self(1 << 4);
    /// A timer's deadline has passed.
    pub const TIMER_EXPIRED: Self = Self(1 << 5);
    /// An interrupt is pending on a bound IRQ object.
    pub const IRQ_PENDING: Self = Self(1 << 6);
    /// A memory object changed size.
    pub const VMO_RESIZED: Self = Self(1 << 7);
    /// A process or thread hit an exception and is waiting for a supervisor.
    pub const EXCEPTION: Self = Self(1 << 8);
    /// The object was destroyed while someone still held a handle.
    pub const DESTROYED: Self = Self(1 << 9);

    /// First of eight bits userspace may raise itself (needs
    /// [`super::rights::Rights::SIGNAL`]).
    pub const USER_BASE: u32 = 24;
    /// Mask of the user-definable bits.
    pub const USER_MASK: Self = Self(0xFF << Self::USER_BASE);

    /// Union.
    pub const fn or(self, other: Self) -> Self {
        Self(self.0 | other.0)
    }
    /// Intersection.
    pub const fn and(self, other: Self) -> Self {
        Self(self.0 & other.0)
    }
    /// Remove bits.
    pub const fn without(self, other: Self) -> Self {
        Self(self.0 & !other.0)
    }
    /// Whether any bit is set.
    pub const fn any(self) -> bool {
        self.0 != 0
    }
    /// Whether every bit of `other` is set.
    pub const fn contains(self, other: Self) -> bool {
        self.0 & other.0 == other.0
    }
}

impl core::fmt::Debug for Signals {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        const NAMES: [(u32, &str); 10] = [
            (1 << 0, "READABLE"),
            (1 << 1, "WRITABLE"),
            (1 << 2, "PEER_CLOSED"),
            (1 << 3, "PROC_TERM"),
            (1 << 4, "THRD_TERM"),
            (1 << 5, "TIMER"),
            (1 << 6, "IRQ"),
            (1 << 7, "VMO_RESIZED"),
            (1 << 8, "EXCEPTION"),
            (1 << 9, "DESTROYED"),
        ];
        if self.0 == 0 {
            return f.write_str("none");
        }
        let mut first = true;
        for (bit, name) in NAMES {
            if self.0 & bit != 0 {
                if !first {
                    f.write_str("|")?;
                }
                f.write_str(name)?;
                first = false;
            }
        }
        let user = (self.0 & Self::USER_MASK.0) >> Self::USER_BASE;
        if user != 0 {
            if !first {
                f.write_str("|")?;
            }
            write!(f, "USER({user:#04x})")?;
        }
        Ok(())
    }
}

/// Threads parked on an object, waiting for a signal.
///
/// Weak references are used deliberately: a thread that is destroyed while
/// blocked must not keep the object alive, and a stale entry costs one failed
/// upgrade rather than a leak.
pub struct WaitQueue {
    waiters: IrqSpinLock<Vec<Weak<Thread>>>,
}

impl WaitQueue {
    /// An empty queue.
    pub const fn new() -> Self {
        Self { waiters: IrqSpinLock::new(Vec::new()) }
    }

    /// Park `thread` on this queue. The caller is responsible for actually
    /// blocking it.
    pub fn park(&self, thread: &Arc<Thread>) {
        let mut waiters = self.waiters.lock();
        if !waiters.iter().any(|w| w.as_ptr() == Arc::as_ptr(thread)) {
            waiters.push(Arc::downgrade(thread));
        }
    }

    /// Remove `thread` from this queue.
    pub fn unpark(&self, thread: &Arc<Thread>) {
        let mut waiters = self.waiters.lock();
        waiters.retain(|w| w.as_ptr() != Arc::as_ptr(thread));
    }

    /// Wake every parked thread and empty the queue.
    ///
    /// Returns the number of threads actually made runnable.
    pub fn wake_all(&self) -> usize {
        // Drain under the lock, then wake outside it: the scheduler takes its
        // own locks and a nested acquisition here would invert the ordering.
        let drained: Vec<Weak<Thread>> = {
            let mut waiters = self.waiters.lock();
            core::mem::take(&mut *waiters)
        };
        let mut woken = 0;
        for weak in drained {
            if let Some(thread) = weak.upgrade() {
                if crate::sched::wake(&thread) {
                    woken += 1;
                }
            }
        }
        woken
    }

    /// Wake at most one parked thread.
    pub fn wake_one(&self) -> bool {
        loop {
            let candidate = {
                let mut waiters = self.waiters.lock();
                if waiters.is_empty() {
                    return false;
                }
                waiters.remove(0)
            };
            if let Some(thread) = candidate.upgrade() {
                if crate::sched::wake(&thread) {
                    return true;
                }
            }
        }
    }

    /// Number of parked threads (including stale entries).
    pub fn len(&self) -> usize {
        self.waiters.lock().len()
    }

    /// Whether the queue is empty.
    pub fn is_empty(&self) -> bool {
        self.waiters.lock().is_empty()
    }
}

impl Default for WaitQueue {
    fn default() -> Self {
        Self::new()
    }
}

/// The signal word of one object, plus the threads watching it.
pub struct SignalState {
    bits: AtomicU32,
    queue: WaitQueue,
}

impl SignalState {
    /// Create state with `initial` signals asserted.
    pub const fn new(initial: Signals) -> Self {
        Self { bits: AtomicU32::new(initial.0), queue: WaitQueue::new() }
    }

    /// Currently asserted signals.
    pub fn get(&self) -> Signals {
        Signals(self.bits.load(Ordering::Acquire))
    }

    /// The wait queue for this object.
    pub fn queue(&self) -> &WaitQueue {
        &self.queue
    }

    /// Assert `set` and deassert `clear`, waking waiters if anything was newly
    /// asserted.
    pub fn update(&self, set: Signals, clear: Signals) {
        let mut current = self.bits.load(Ordering::Relaxed);
        loop {
            let next = (current & !clear.0) | set.0;
            if next == current {
                return;
            }
            match self.bits.compare_exchange_weak(current, next, Ordering::AcqRel, Ordering::Relaxed) {
                Ok(_) => break,
                Err(observed) => current = observed,
            }
        }
        if set.any() {
            self.queue.wake_all();
        }
    }
}
