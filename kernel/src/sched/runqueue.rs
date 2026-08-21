//! Per-CPU run queues, one structure per scheduling class.

use alloc::collections::{BTreeMap, VecDeque};
use alloc::sync::Arc;

use crate::task::thread::Thread;

use super::params::{SchedClass, MAX_PRIORITY};

/// The runnable threads on one CPU.
pub struct RunQueue {
    /// Earliest-deadline-first, keyed by `(deadline, tid)` so equal deadlines
    /// break ties deterministically.
    realtime: BTreeMap<(u64, u64), Arc<Thread>>,
    /// One FIFO per priority level.
    interactive: [VecDeque<Arc<Thread>>; MAX_PRIORITY as usize + 1],
    /// Bit `p` set when `interactive[p]` is non-empty, so selection is a single
    /// leading-zero count instead of a scan.
    interactive_mask: u32,
    /// Weighted fair sharing, keyed by `(vruntime, tid)`.
    normal: BTreeMap<(u64, u64), Arc<Thread>>,
    /// Smallest virtual runtime seen, used to place newly woken threads so they
    /// neither starve nor get an unbounded head start.
    min_vruntime: u64,
    /// Strict FIFO.
    background: VecDeque<Arc<Thread>>,
    /// Total runnable threads.
    count: usize,
}

impl RunQueue {
    /// An empty run queue.
    pub const fn new() -> Self {
        Self {
            realtime: BTreeMap::new(),
            interactive: [const { VecDeque::new() }; MAX_PRIORITY as usize + 1],
            interactive_mask: 0,
            normal: BTreeMap::new(),
            min_vruntime: 0,
            background: VecDeque::new(),
            count: 0,
        }
    }

    /// Number of runnable threads.
    pub fn len(&self) -> usize {
        self.count
    }

    /// Whether anything is runnable.
    pub fn is_empty(&self) -> bool {
        self.count == 0
    }

    /// The highest class with a runnable thread.
    pub fn top_class(&self) -> Option<SchedClass> {
        if !self.realtime.is_empty() {
            Some(SchedClass::RealTime)
        } else if self.interactive_mask != 0 {
            Some(SchedClass::Interactive)
        } else if !self.normal.is_empty() {
            Some(SchedClass::Normal)
        } else if !self.background.is_empty() {
            Some(SchedClass::Background)
        } else {
            None
        }
    }

    /// Add a runnable thread.
    pub fn enqueue(&mut self, thread: Arc<Thread>) {
        let params = thread.params();
        let tid = thread.tid();
        self.count += 1;
        match params.class {
            SchedClass::RealTime => {
                // A real-time thread that has burned its budget is parked at the
                // back of the ordering rather than dropped, so it still runs if
                // the machine is otherwise idle.
                let key = if params.over_budget() { u64::MAX - tid } else { params.deadline_ns };
                self.realtime.insert((key, tid), thread);
            }
            SchedClass::Interactive => {
                let priority = params.priority.min(MAX_PRIORITY) as usize;
                self.interactive[priority].push_back(thread);
                self.interactive_mask |= 1 << priority;
            }
            SchedClass::Normal => {
                // Clamp a waking thread's virtual runtime forward to the queue
                // minimum: without this, a thread that slept for a minute would
                // monopolise the CPU when it woke.
                let vruntime = thread.vruntime.load(core::sync::atomic::Ordering::Relaxed).max(self.min_vruntime);
                thread.vruntime.store(vruntime, core::sync::atomic::Ordering::Relaxed);
                self.normal.insert((vruntime, tid), thread);
            }
            SchedClass::Background => self.background.push_back(thread),
        }
    }

    /// Remove and return the thread that should run next.
    pub fn dequeue(&mut self) -> Option<Arc<Thread>> {
        if let Some(key) = self.realtime.keys().next().copied() {
            self.count -= 1;
            return self.realtime.remove(&key);
        }
        if self.interactive_mask != 0 {
            let priority = (31 - self.interactive_mask.leading_zeros()) as usize;
            let thread = self.interactive[priority].pop_front();
            if self.interactive[priority].is_empty() {
                self.interactive_mask &= !(1 << priority);
            }
            if thread.is_some() {
                self.count -= 1;
            }
            return thread;
        }
        if let Some(key) = self.normal.keys().next().copied() {
            self.min_vruntime = self.min_vruntime.max(key.0);
            self.count -= 1;
            return self.normal.remove(&key);
        }
        let thread = self.background.pop_front();
        if thread.is_some() {
            self.count -= 1;
        }
        thread
    }

    /// Remove a specific thread, if it is queued.
    pub fn remove(&mut self, thread: &Arc<Thread>) -> bool {
        let tid = thread.tid();
        let before = self.count;

        self.realtime.retain(|(_, t), _| *t != tid);
        for (priority, queue) in self.interactive.iter_mut().enumerate() {
            let len = queue.len();
            queue.retain(|t| t.tid() != tid);
            if queue.len() != len {
                self.count -= len - queue.len();
            }
            if queue.is_empty() {
                self.interactive_mask &= !(1 << priority);
            }
        }
        let normal_before = self.normal.len();
        self.normal.retain(|(_, t), _| *t != tid);
        self.count -= normal_before - self.normal.len();
        let background_before = self.background.len();
        self.background.retain(|t| t.tid() != tid);
        self.count -= background_before - self.background.len();

        // `realtime.retain` above changed the map without adjusting the count;
        // recompute rather than track it twice.
        self.count = self.realtime.len()
            + self.interactive.iter().map(|q| q.len()).sum::<usize>()
            + self.normal.len()
            + self.background.len();
        before != self.count
    }

    /// Whether `thread` would preempt a currently running thread with `current`
    /// parameters.
    pub fn should_preempt(&self, current: &super::params::SchedParams) -> bool {
        match self.top_class() {
            Some(class) => class < current.class,
            None => false,
        }
    }

    /// Per-class counts, for diagnostics.
    pub fn counts(&self) -> [usize; 4] {
        [
            self.realtime.len(),
            self.interactive.iter().map(|q| q.len()).sum(),
            self.normal.len(),
            self.background.len(),
        ]
    }
}

impl Default for RunQueue {
    fn default() -> Self {
        Self::new()
    }
}
