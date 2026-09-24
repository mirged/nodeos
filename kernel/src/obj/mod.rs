//! Kernel objects.
//!
//! Every resource the kernel manages is a *typed kernel object*. Userspace
//! never names one directly: it holds [`handle::Handle`]s, which are
//! process-local indices carrying [`rights::Rights`] and a
//! [`grant::Grant`]. That indirection is the whole security model --
//! authority is the handle, so there is nothing to check an identity against,
//! and nothing for a "superuser" bit to override.

pub mod capclass;
pub mod device;
pub mod grant;
pub mod handle;
pub mod rights;
pub mod signals;

use alloc::sync::Arc;
use core::any::Any;
use core::sync::atomic::{AtomicU64, Ordering};

use crate::sync::IrqSpinLock;

pub use capclass::{CapClass, CapSet};
pub use device::Device;
pub use grant::Grant;
pub use handle::{Handle, HandleTable, HandleValue};
pub use rights::Rights;
pub use signals::{SignalState, Signals, WaitQueue};

/// Kernel object identifier: unique for the lifetime of the boot, never reused.
///
/// KOIDs exist so that logs, audit trails and debuggers can talk about an
/// object without holding a handle to it. Knowing a KOID conveys no authority.
pub type Koid = u64;

static NEXT_KOID: AtomicU64 = AtomicU64::new(1);

/// Allocate a fresh KOID.
pub fn next_koid() -> Koid {
    NEXT_KOID.fetch_add(1, Ordering::Relaxed)
}

/// The kinds of object the kernel implements.
///
/// This list is deliberately short and closed. Adding a kind means adding
/// kernel code and kernel policy, so "should this be a kernel object?" is a
/// question with real weight -- the answer for filesystems, sockets, terminals
/// and windows is *no*, they are userspace services reached over channels.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum ObjectType {
    /// An address space plus a handle table plus a namespace.
    Process,
    /// A schedulable execution context inside a process.
    Thread,
    /// One end of a bidirectional typed message pipe.
    Channel,
    /// A range of memory that can be mapped into address spaces.
    Vmo,
    /// A set of translations, separable from the process that owns it.
    AddressSpace,
    /// A bound hardware interrupt line.
    Irq,
    /// A one-shot or periodic deadline.
    Timer,
    /// A claimed hardware device (MMIO window, port range, IRQ).
    Device,
    /// A named set of handles forming a process's view of the system.
    Namespace,
    /// A resource-accounting group holding processes and other jobs.
    Job,
}

impl ObjectType {
    /// Short name used in diagnostics.
    pub const fn name(self) -> &'static str {
        match self {
            Self::Process => "process",
            Self::Thread => "thread",
            Self::Channel => "channel",
            Self::Vmo => "vmo",
            Self::AddressSpace => "aspace",
            Self::Irq => "irq",
            Self::Timer => "timer",
            Self::Device => "device",
            Self::Namespace => "namespace",
            Self::Job => "job",
        }
    }
}

/// Maximum length of an object's debug name.
pub const NAME_LEN: usize = 32;

/// State shared by every kernel object.
pub struct ObjectHeader {
    koid: Koid,
    kind: ObjectType,
    name: IrqSpinLock<[u8; NAME_LEN]>,
    signals: SignalState,
}

impl ObjectHeader {
    /// Create a header with the given type and initial signals.
    pub fn new(kind: ObjectType, initial: Signals) -> Self {
        Self {
            koid: next_koid(),
            kind,
            name: IrqSpinLock::new([0; NAME_LEN]),
            signals: SignalState::new(initial),
        }
    }

    /// This object's KOID.
    pub fn koid(&self) -> Koid {
        self.koid
    }

    /// This object's type.
    pub fn kind(&self) -> ObjectType {
        self.kind
    }

    /// Signal state.
    pub fn signals(&self) -> &SignalState {
        &self.signals
    }

    /// Set the debug name (truncated to [`NAME_LEN`]).
    pub fn set_name(&self, name: &str) {
        let mut buffer = self.name.lock();
        *buffer = [0; NAME_LEN];
        let bytes = name.as_bytes();
        let n = bytes.len().min(NAME_LEN);
        buffer[..n].copy_from_slice(&bytes[..n]);
    }

    /// Copy the debug name into `out`, returning the portion written.
    pub fn name_into<'a>(&self, out: &'a mut [u8; NAME_LEN]) -> &'a str {
        *out = *self.name.lock();
        let end = out.iter().position(|&b| b == 0).unwrap_or(NAME_LEN);
        core::str::from_utf8(&out[..end]).unwrap_or("<invalid>")
    }
}

/// The trait every kernel object implements.
pub trait KernelObject: Any + Send + Sync {
    /// Shared object state.
    fn header(&self) -> &ObjectHeader;

    /// Called when the last handle to this object is closed. The object itself
    /// may outlive this (the kernel may still hold references); this is the
    /// hook for "no userspace holder remains", which is what closes a channel
    /// peer or cancels a timer.
    fn on_zero_handles(&self) {}

    /// Upcast for downcasting. Implementations are always `fn as_any(&self) -> &dyn Any { self }`.
    fn as_any(&self) -> &dyn Any;
}

/// Convenience alias for a reference-counted object.
pub type ObjectRef = Arc<dyn KernelObject>;

/// Downcast an object reference to a concrete type.
pub fn downcast<T: KernelObject>(object: &ObjectRef) -> Option<&T> {
    object.as_any().downcast_ref::<T>()
}

/// Errors produced by the object and handle layer.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum ObjectError {
    /// The handle value does not name a live handle in this process.
    BadHandle,
    /// The handle exists but lacks a required right.
    AccessDenied,
    /// The handle names an object of the wrong type for this operation.
    WrongType,
    /// The handle table is full.
    OutOfHandles,
    /// A grant chain would exceed [`grant::MAX_GRANT_DEPTH`].
    GrantTooDeep,
    /// The authority behind this handle has been revoked.
    Revoked,
}
