//! Processes.
//!
//! A process in NodeOS is a *protection domain*: an address space, a handle
//! table, a resource namespace, and a set of threads. It is not a unit of
//! execution and not a scheduling entity -- threads are.
//!
//! There is no `fork`. A process is created empty, filled by whoever holds the
//! authority to fill it, and then started. That splits three things `fork`
//! fuses together -- creating an address space, populating it, and granting it
//! authority -- so each can be checked separately and delegated separately.

use alloc::string::String;
use alloc::sync::{Arc, Weak};
use alloc::vec::Vec;
use core::sync::atomic::{AtomicBool, AtomicI64, AtomicU64, Ordering};

use crate::mm::space::AddressSpace;
use crate::obj::{
    CapSet, Handle, HandleTable, HandleValue, KernelObject, ObjectError, ObjectHeader, ObjectType, Rights, Signals,
};
use crate::sync::IrqSpinLock;

use super::namespace::Namespace;
use super::thread::{Thread, ThreadState};

static NEXT_PID: AtomicU64 = AtomicU64::new(1);

/// Lifecycle state of a process.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum ProcessState {
    /// Created, not yet started; its address space may still be populated.
    Building,
    /// At least one thread has been started.
    Running,
    /// Every thread has exited.
    Terminated,
}

/// A protection domain.
pub struct Process {
    header: ObjectHeader,
    pid: u64,
    space: Arc<AddressSpace>,
    handles: HandleTable,
    namespace: Arc<Namespace>,
    threads: IrqSpinLock<Vec<Arc<Thread>>>,
    state: IrqSpinLock<ProcessState>,
    exit_code: AtomicI64,
    /// Capability classes the launcher actually granted, for introspection and
    /// for the `nodeos.caps` diagnostic.
    granted: IrqSpinLock<CapSet>,
    /// Set once teardown has begun, so a dying process stops accepting work.
    dying: AtomicBool,
    parent: Weak<Process>,
}

impl Process {
    /// Create an empty process with a fresh address space.
    pub fn new(name: &str, parent: Option<&Arc<Process>>, granted: CapSet) -> Option<Arc<Self>> {
        let space = AddressSpace::new_user().ok()?;
        let process = Arc::new(Self {
            header: ObjectHeader::new(ObjectType::Process, Signals::NONE),
            pid: NEXT_PID.fetch_add(1, Ordering::Relaxed),
            space,
            handles: HandleTable::new(),
            namespace: Namespace::new(),
            threads: IrqSpinLock::new(Vec::new()),
            state: IrqSpinLock::new(ProcessState::Building),
            exit_code: AtomicI64::new(0),
            granted: IrqSpinLock::new(granted),
            dying: AtomicBool::new(false),
            parent: parent.map(Arc::downgrade).unwrap_or_default(),
        });
        process.header.set_name(name);
        Some(process)
    }

    /// Create the kernel's own pseudo-process, which owns kernel threads.
    ///
    /// It has no user address space of its own; kernel threads run in whatever
    /// address space happens to be loaded, because the kernel half is identical
    /// in all of them.
    pub fn new_kernel() -> Arc<Self> {
        let space = AddressSpace::new_user().expect("failed to create the kernel process address space");
        let process = Arc::new(Self {
            header: ObjectHeader::new(ObjectType::Process, Signals::NONE),
            pid: 0,
            space,
            handles: HandleTable::new(),
            namespace: Namespace::new(),
            threads: IrqSpinLock::new(Vec::new()),
            state: IrqSpinLock::new(ProcessState::Running),
            exit_code: AtomicI64::new(0),
            granted: IrqSpinLock::new(CapSet::NONE),
            dying: AtomicBool::new(false),
            parent: Weak::new(),
        });
        process.header.set_name("kernel");
        process
    }

    /// Process id.
    pub fn pid(&self) -> u64 {
        self.pid
    }

    /// The address space.
    pub fn space(&self) -> &Arc<AddressSpace> {
        &self.space
    }

    /// The handle table.
    pub fn handles(&self) -> &HandleTable {
        &self.handles
    }

    /// The resource namespace.
    pub fn namespace(&self) -> &Arc<Namespace> {
        &self.namespace
    }

    /// Capability classes granted at launch.
    pub fn granted(&self) -> CapSet {
        *self.granted.lock()
    }

    /// Lifecycle state.
    pub fn state(&self) -> ProcessState {
        *self.state.lock()
    }

    /// Parent process, if it is still alive.
    pub fn parent(&self) -> Option<Arc<Process>> {
        self.parent.upgrade()
    }

    /// Debug name.
    pub fn name(&self) -> String {
        let mut buffer = [0u8; crate::obj::NAME_LEN];
        String::from(self.header.name_into(&mut buffer))
    }

    /// Register a thread with this process.
    pub fn add_thread(&self, thread: &Arc<Thread>) {
        self.threads.lock().push(thread.clone());
        let mut state = self.state.lock();
        if *state == ProcessState::Building {
            *state = ProcessState::Running;
        }
    }

    /// Snapshot of this process's threads.
    pub fn threads(&self) -> Vec<Arc<Thread>> {
        self.threads.lock().clone()
    }

    /// Number of threads that have not exited.
    pub fn live_threads(&self) -> usize {
        self.threads.lock().iter().filter(|t| t.state() != ThreadState::Dead).count()
    }

    /// Install a handle, returning its process-local value.
    pub fn install(&self, handle: Handle) -> Result<HandleValue, ObjectError> {
        self.handles.insert(handle)
    }

    /// Install a fresh root handle to `object` with `rights`.
    pub fn install_object(
        &self,
        object: Arc<dyn KernelObject>,
        rights: Rights,
    ) -> Result<HandleValue, ObjectError> {
        self.handles.insert(Handle::root(object, rights))
    }

    /// Whether the process is tearing down.
    pub fn is_dying(&self) -> bool {
        self.dying.load(Ordering::Acquire)
    }

    /// Exit code, valid once terminated.
    pub fn exit_code(&self) -> i64 {
        self.exit_code.load(Ordering::Acquire)
    }

    /// Terminate the process: kill its threads, drop its handles, and signal
    /// anyone waiting on it.
    pub fn terminate(self: &Arc<Self>, code: i64) {
        if self.dying.swap(true, Ordering::AcqRel) {
            return;
        }
        self.exit_code.store(code, Ordering::Release);

        let threads = self.threads();
        for thread in &threads {
            if thread.state() != ThreadState::Dead {
                crate::sched::kill(thread);
            }
        }

        // Dropping the handle table is what actually releases the process's
        // authority: closing a channel endpoint signals its peer, dropping a
        // VMO handle may free memory, and so on.
        self.handles.clear();
        self.namespace.clear();
        // And its memory: the process object may be referenced for a long time
        // yet (by a parent's handle, by the scheduler), and a program that
        // mapped hundreds of megabytes must not keep them after it exits.
        self.space.release_regions();

        *self.state.lock() = ProcessState::Terminated;
        self.header.signals().update(Signals::PROCESS_TERMINATED, Signals::NONE);
        crate::debug!("process {} ({}) terminated with code {}", self.pid, self.name(), code);
    }

    /// Called when a thread exits; terminates the process when the last one
    /// goes.
    pub fn thread_exited(self: &Arc<Self>, thread: &Arc<Thread>) {
        self.threads.lock().retain(|t| !Arc::ptr_eq(t, thread));
        if self.threads.lock().iter().all(|t| t.state() == ThreadState::Dead) && !self.is_dying() {
            self.terminate(thread.exit_code());
        }
    }
}

impl KernelObject for Process {
    fn header(&self) -> &ObjectHeader {
        &self.header
    }

    fn as_any(&self) -> &dyn core::any::Any {
        self
    }
}
