//! The boot launcher.
//!
//! Somebody has to hand out the first capability, and in NodeOS that somebody
//! is a hundred lines of kernel code that runs once. It reads the boot modules
//! the loader placed in memory, decides what each is allowed to do, and starts
//! them with exactly those capabilities and nothing else.
//!
//! This is deliberately the *only* ambient authority in the system. It exists
//! because a capability system needs a root, not because the kernel wants a
//! policy: once the first processes are running, every further grant is a
//! handle passed from someone who already had it.
//!
//! The policy itself is small enough to read in one sitting, which is the
//! point. Compare with the alternative -- a setuid binary, a root user, a
//! permission bitmask evaluated on every syscall -- where "what can this
//! program do?" has no local answer at all.

use alloc::sync::Arc;
use alloc::vec::Vec;

use nodeos_abi::message::{BootstrapMessage, KIND_BOOTSTRAP};
use nodeos_abi::HandleValue;

use crate::boot::BootInfo;
use crate::ipc::message::Message;
use crate::ipc::Channel;
use crate::loader::{self, LoadError};
use crate::obj::{CapClass, CapSet, Handle, Rights};
use crate::sched;
use crate::task::thread::{Thread, ThreadEntry};
use crate::task::Process;

/// The boot policy: which capability classes each named module may hold.
///
/// A launcher in userspace would read this from a manifest. Keeping it as a
/// table in the kernel for the boot set means the trusted computing base can
/// be inspected without parsing anything.
fn policy(name: &str) -> CapSet {
    let base = CapSet::NONE.with(CapClass::Memory).with(CapClass::Channel);
    match name {
        // `init` supervises everything else, so it may create processes and
        // load further images. It gets no device or filesystem authority: it
        // hands those to the services that need them, having never held them.
        "init" => base.with(CapClass::Process).with(CapClass::Load).with(CapClass::Clock).with(CapClass::Debug),
        // The log service needs nothing but its channel.
        "console" => base,
        // The filesystem service may create and serve files; it may not touch
        // devices or the network.
        "ramfs" => base.with(CapClass::FileRead).with(CapClass::FileWrite).with(CapClass::FileCreate),
        // The device manager is the only thing that may claim hardware.
        "devmgr" => base.with(CapClass::Device),
        // Anything else gets the floor: memory and channels.
        _ => base,
    }
}

/// A launched process and the channel the launcher kept.
pub struct Launched {
    /// The process object.
    pub process: Arc<Process>,
    /// The launcher's end of the bootstrap channel.
    pub control: Arc<Channel>,
    /// Module name.
    pub name: alloc::string::String,
}

/// Load and start every boot module, returning what was launched.
///
/// # Safety
/// Must run after the memory manager and the scheduler are up, with
/// `boot_info` relocated to the direct physical map.
pub unsafe fn launch_boot_modules(boot_info: &BootInfo) -> Vec<Launched> {
    let mut launched = Vec::new();
    let mut found = 0usize;

    for module in boot_info.modules() {
        found += 1;
        let pointer = boot_info.module_ptr(&module);
        // SAFETY: the frame allocator reserved every module's physical range,
        // and the direct map covers it, so the bytes are stable and readable.
        let bytes = unsafe { core::slice::from_raw_parts(pointer, module.len()) };

        match launch_one(module.name, bytes) {
            Ok(entry) => {
                crate::info!(
                    "launched {:?} as pid {} with capabilities {:?}",
                    entry.name,
                    entry.process.pid(),
                    entry.process.granted()
                );
                launched.push(entry);
            }
            Err(error) => {
                crate::error!("failed to launch module {:?}: {:?}", module.name, error);
            }
        }
    }

    if found == 0 {
        crate::warn!("no boot modules were supplied; userspace will not start");
    }
    launched
}

fn launch_one(name: &str, bytes: &[u8]) -> Result<Launched, LoadError> {
    // Inspect before committing anything, so a module that asks for more than
    // the policy allows is refused before it costs a page of memory.
    let requirements = loader::inspect(bytes)?;
    let module_name = if requirements.name.is_empty() { name } else { requirements.name.as_str() };
    let allowed = policy(module_name);

    let denied = requirements.required.difference(allowed);
    if !denied.is_empty() {
        crate::warn!("module {module_name:?} requires {denied:?}, which policy denies");
        if requirements.strict {
            let class = denied.iter().next().expect("non-empty set has a member");
            return Err(LoadError::CapabilityDenied(class));
        }
    }
    let granted = requirements.required.or(requirements.optional).and(allowed);

    let process = Process::new(module_name, None, granted).ok_or(LoadError::NoMemory)?;
    let image = loader::load(&process, bytes, granted)?;

    // The bootstrap channel: the launcher keeps one end, the child gets the
    // other and nothing else.
    let (parent_end, child_end) = Channel::create_pair();

    let bootstrap_value = process
        .install_object(child_end.clone(), Channel::default_rights())
        .map_err(|_| LoadError::NoMemory)?;

    // Self-handles. These create a reference cycle (the process's handle table
    // holds the process), which `Process::terminate` breaks by clearing the
    // table. That is deliberate: a process that cannot name itself cannot
    // create a thread in itself, and forcing every program to be handed a
    // handle to itself by its parent buys nothing.
    let process_handle = process
        .install_object(process.clone(), Rights::PROCESS_DEFAULT)
        .map_err(|_| LoadError::NoMemory)?;
    let space_handle = process
        .install_object(process.space().clone(), Rights::VMO_DEFAULT)
        .map_err(|_| LoadError::NoMemory)?;

    // Create the first thread, but do not enqueue it until the bootstrap
    // message is queued: the child's very first act is to read it.
    let thread = Thread::new(
        &process,
        "main",
        ThreadEntry::User {
            entry: image.entry,
            stack: image.stack_top,
            arg0: bootstrap_value.0 as u64,
            arg1: 0,
        },
        sched::SchedParams::normal(),
    )
    .ok_or(LoadError::NoMemory)?;

    let thread_handle = process
        .install_object(thread.clone(), Rights::THREAD_DEFAULT)
        .map_err(|_| LoadError::NoMemory)?;

    let info = BootstrapMessage {
        version: 1,
        granted_caps: granted.0,
        process: HandleValue(process_handle.0),
        space: HandleValue(space_handle.0),
        thread: HandleValue(thread_handle.0),
        namespace: HandleValue::INVALID,
        image_base: image.base,
        heap_size: requirements.resources.heap_reserve,
        extra_count: 0,
        _reserved: 0,
    };

    let mut message = Message::empty(KIND_BOOTSTRAP);
    // SAFETY: `BootstrapMessage` is `repr(C)` plain-old-data, so its bytes are
    // a valid wire representation.
    let raw = unsafe {
        core::slice::from_raw_parts(
            &info as *const BootstrapMessage as *const u8,
            core::mem::size_of::<BootstrapMessage>(),
        )
    };
    message.payload.extend_from_slice(raw);
    if parent_end.send(message).is_err() {
        crate::error!("failed to deliver the bootstrap message to {module_name:?}");
    }

    sched::enqueue(&thread);

    Ok(Launched {
        process,
        control: parent_end,
        name: alloc::string::String::from(module_name),
    })
}

/// Give `child` a capability under `name`, as a launcher would.
///
/// Used to wire services to each other after they are all running.
pub fn grant(target: &Launched, handle: Handle) -> Result<crate::obj::HandleValue, ()> {
    target.process.install(handle).map_err(|_| ())
}

/// Wait for every launched process to terminate, then report.
///
/// Used by the `nodeos.autotest` boot mode so that a boot has an *exit status*
/// rather than a log that something else has to interpret. A system whose
/// correctness can only be established by grepping is a system whose
/// correctness nobody checks.
pub fn supervise(launched: &[Launched], timeout_ns: u64) -> bool {
    use crate::task::ProcessState;

    if launched.is_empty() {
        return false;
    }

    let deadline = crate::time::now_ns() + timeout_ns;
    loop {
        let running = launched.iter().filter(|entry| entry.process.state() != ProcessState::Terminated).count();
        if running == 0 {
            break;
        }
        if crate::time::now_ns() > deadline {
            for entry in launched {
                if entry.process.state() != ProcessState::Terminated {
                    crate::error!("{:?} (pid {}) did not exit within the deadline", entry.name, entry.process.pid());
                }
            }
            return false;
        }
        sched::yield_now();
    }

    let mut ok = true;
    for entry in launched {
        let code = entry.process.exit_code();
        if code == 0 {
            crate::info!("{:?} (pid {}) exited cleanly", entry.name, entry.process.pid());
        } else {
            crate::error!("{:?} (pid {}) exited with code {}", entry.name, entry.process.pid(), code);
            ok = false;
        }
    }
    ok
}
