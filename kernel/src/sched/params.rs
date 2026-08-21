//! Scheduling classes and per-thread parameters.

/// Which scheduling policy governs a thread.
///
/// Classes are strictly ordered: a runnable thread in a higher class always
/// preempts a lower one. Within a class the policy differs, and that is the
/// whole point of having classes rather than one universal heuristic -- a
/// deadline-driven audio thread and a batch compile want opposite things, and
/// no single "fairness" knob serves both.
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Debug)]
#[repr(u32)]
pub enum SchedClass {
    /// Earliest-deadline-first with an enforced CPU budget. Misses are
    /// reported, not hidden: a thread that overruns its budget is demoted for
    /// the rest of its period rather than allowed to starve the machine.
    RealTime = 0,
    /// Fixed-priority round robin with a short quantum, for threads that must
    /// respond quickly but have no hard deadline (input, compositor, IPC
    /// servers on the critical path).
    Interactive = 1,
    /// Weighted fair sharing by virtual runtime. The default.
    Normal = 2,
    /// Runs only when nothing else is runnable.
    Background = 3,
}

impl SchedClass {
    /// All classes, in dispatch order.
    pub const ALL: [SchedClass; 4] =
        [SchedClass::RealTime, SchedClass::Interactive, SchedClass::Normal, SchedClass::Background];

    /// Short name for diagnostics.
    pub const fn name(self) -> &'static str {
        match self {
            Self::RealTime => "realtime",
            Self::Interactive => "interactive",
            Self::Normal => "normal",
            Self::Background => "background",
        }
    }

    /// Decode the wire value used by the `thread_create` syscall.
    pub const fn from_u32(value: u32) -> Option<Self> {
        Some(match value {
            0 => Self::RealTime,
            1 => Self::Interactive,
            2 => Self::Normal,
            3 => Self::Background,
            _ => return None,
        })
    }

    /// Default timeslice for the class, in nanoseconds.
    pub const fn default_timeslice_ns(self) -> u64 {
        match self {
            // Real-time threads run to their budget, not to a quantum.
            Self::RealTime => 1_000_000,
            Self::Interactive => 2_000_000,
            Self::Normal => 6_000_000,
            Self::Background => 20_000_000,
        }
    }
}

/// Highest priority value within a class.
pub const MAX_PRIORITY: u8 = 31;
/// The neutral weight, corresponding to priority 16 in the Normal class.
pub const NICE0_WEIGHT: u32 = 1024;

/// Per-thread scheduling parameters.
#[derive(Clone, Copy, Debug)]
pub struct SchedParams {
    /// Which policy applies.
    pub class: SchedClass,
    /// Priority within the class, 0..=[`MAX_PRIORITY`], higher is more urgent.
    pub priority: u8,
    /// Share weight for [`SchedClass::Normal`].
    pub weight: u32,
    /// Absolute deadline in nanoseconds since boot, for
    /// [`SchedClass::RealTime`].
    pub deadline_ns: u64,
    /// Repetition period in nanoseconds, for [`SchedClass::RealTime`].
    pub period_ns: u64,
    /// CPU time this thread may consume per period, in nanoseconds.
    pub budget_ns: u64,
    /// Budget consumed so far in the current period.
    pub budget_used_ns: u64,
    /// Bitmask of CPUs this thread may run on. Zero means "any".
    pub affinity: u64,
}

impl SchedParams {
    /// The default: fair share at neutral weight.
    pub const fn normal() -> Self {
        Self {
            class: SchedClass::Normal,
            priority: 16,
            weight: NICE0_WEIGHT,
            deadline_ns: 0,
            period_ns: 0,
            budget_ns: 0,
            budget_used_ns: 0,
            affinity: 0,
        }
    }

    /// Interactive at `priority`.
    pub const fn interactive(priority: u8) -> Self {
        Self { class: SchedClass::Interactive, priority, ..Self::normal() }
    }

    /// Background.
    pub const fn background() -> Self {
        Self { class: SchedClass::Background, priority: 0, ..Self::normal() }
    }

    /// Real-time with a period and a budget.
    pub const fn realtime(period_ns: u64, budget_ns: u64) -> Self {
        Self {
            class: SchedClass::RealTime,
            priority: MAX_PRIORITY,
            weight: NICE0_WEIGHT,
            deadline_ns: 0,
            period_ns,
            budget_ns,
            budget_used_ns: 0,
            affinity: 0,
        }
    }

    /// The idle thread's parameters.
    pub const fn idle() -> Self {
        Self { class: SchedClass::Background, priority: 0, weight: 1, ..Self::normal() }
    }

    /// Weight implied by the priority, for the fair-share class.
    ///
    /// Each priority step is worth about 25%, which is the same geometric
    /// spacing Unix nice values use and for the same reason: it makes the
    /// *ratio* between two threads' shares depend only on their difference.
    pub fn effective_weight(&self) -> u32 {
        if self.class != SchedClass::Normal {
            return self.weight.max(1);
        }
        let steps = self.priority as i32 - 16;
        let mut weight = NICE0_WEIGHT as i64;
        if steps > 0 {
            for _ in 0..steps {
                weight = weight * 5 / 4;
            }
        } else {
            for _ in 0..(-steps) {
                weight = weight * 4 / 5;
            }
        }
        weight.clamp(1, 1 << 20) as u32
    }

    /// Whether this thread may run on `cpu`.
    pub fn may_run_on(&self, cpu: u32) -> bool {
        self.affinity == 0 || self.affinity & (1u64 << (cpu & 63)) != 0
    }

    /// Whether the thread has exhausted its real-time budget this period.
    pub fn over_budget(&self) -> bool {
        self.class == SchedClass::RealTime && self.budget_ns > 0 && self.budget_used_ns >= self.budget_ns
    }
}

impl Default for SchedParams {
    fn default() -> Self {
        Self::normal()
    }
}
