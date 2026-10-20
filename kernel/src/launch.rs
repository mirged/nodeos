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
use nodeos_abi::{proto, HandleValue};

use crate::boot::BootInfo;
use crate::ipc::message::Message;
use crate::ipc::Channel;
use crate::loader::{self, LoadError};
use crate::obj::{CapClass, CapSet, Device, Handle, Rights};
use crate::sched;
use crate::task::thread::{Thread, ThreadEntry};
use crate::task::Process;

/// Devices the boot launcher may grant to specific modules by name.
///
/// A launcher in userspace would resolve these from a manifest; the boot set
/// is small and fixed enough that a struct built once in `kmain` says the same
/// thing more plainly.
pub struct BootDevices {
    /// The boot framebuffer, if the loader reported one. Granted to
    /// `"display"` at index 0 of its bootstrap message's capabilities.
    pub framebuffer: Option<Arc<Device>>,
    /// The PS/2 keyboard. Granted to `"display"` at index 1.
    pub keyboard: Arc<Device>,
    /// Kernel mouse device, reserved for driver tests. The shell boot does
    /// not grant this capability to any userspace service.
    pub pointer: Arc<Device>,
    /// The AC'97 audio controller, if the machine has one. Granted to
    /// `"audio"`, which is the only process that ever touches it.
    pub audio: Option<Arc<Device>>,
    /// The virtio block device, if one was attached. Granted to `"fs"`.
    pub storage: Option<Arc<Device>>,
}

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
        // The display broker is the only thing in the system that holds the
        // screen and the keyboard. Everything that wants to draw or to be
        // typed at is one of its clients, and holds a channel instead.
        "display" => base.with(CapClass::Display).with(CapClass::Input),
        // The text console gets a full-screen surface from the display
        // broker, keeping all device capabilities out of the shell session.
        "console" => base,
        // The filesystem service may create and serve files. It also holds
        // the disk, because the only thing in this system that speaks blocks
        // is the thing that speaks files: a separate block-driver process
        // would be one more address space and one more protocol for exactly
        // one client. `docs/FILESYSTEM.md` argues that trade honestly.
        "fs" => base
            .with(CapClass::FileRead)
            .with(CapClass::FileWrite)
            .with(CapClass::FileCreate)
            .with(CapClass::Device),
        // The audio service holds the sound card and nothing else -- no
        // filesystem, no display, no way to learn what it is mixing. It is
        // handed samples over a channel and it puts them on a ring.
        "audio" => base.with(CapClass::Audio).with(CapClass::Device).with(CapClass::Clock),
        // The filesystem service may create and serve files; it may not touch
        // devices or the network.
        "ramfs" => base.with(CapClass::FileRead).with(CapClass::FileWrite).with(CapClass::FileCreate),
        // The device manager is the only thing that may claim hardware.
        "devmgr" => base.with(CapClass::Device),
        // The shell is a console client like any other; it holds no device
        // capability of its own, only the channels the launcher hands it --
        // one to the console, one to the filesystem.
        // The shell launches programs entered at its prompt and forwards the
        // channels it holds to them, so its capability set is a superset of
        // theirs: `Audio` is here because Doom needs it and Doom is now an
        // ordinary disk program (`run /bin/doom.nxf`), not a boot module.
        "shell" => base
            .with(CapClass::Process)
            .with(CapClass::Load)
            .with(CapClass::Debug)
            .with(CapClass::Clock)
            .with(CapClass::FileRead)
            .with(CapClass::FileWrite)
            .with(CapClass::FileCreate)
            .with(CapClass::Audio),
        // Doom is a display broker client too. It does not go through `console`
        // -- it repaints its whole surface 35 times a second and wants raw
        // make/break scancodes, neither of which the console protocol offers
        // -- but the display broker serves both without either holding a
        // device. Doom needs the clock for its frame timing, and `Audio` for
        // the channel to the mixer -- which is still not a device: the sound
        // card belongs to `audio`, exactly as the screen belongs to the
        // display broker.
        "doom" => base.with(CapClass::Clock).with(CapClass::Audio),
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

/// A boot module that is data rather than an executable: no NXF magic, so
/// nothing to launch. It is turned into a memory object and handed to the
/// program that declared a use for it.
///
/// Doom's IWAD is the only one today. Loading it this way rather than baking
/// it into the image means the 14 MiB is mapped from where the boot loader
/// already put it -- the process reads the WAD out of a read-only mapping and
/// no copy of it is ever made.
struct DataModule {
    name: alloc::string::String,
    vmo: Arc<crate::mm::vmo::Vmo>,
}

/// Turn a boot module's physical range into a read-only memory object.
///
/// # Safety
/// `start` must be the physical base of a module the frame allocator reserved,
/// and `len` its length.
unsafe fn data_module_vmo(name: &str, start: u64, len: u64) -> Option<Arc<crate::mm::vmo::Vmo>> {
    // A physical VMO can only begin on a page boundary. Multiboot2 loaders
    // page-align modules, so this is a check rather than a code path with an
    // alternative -- if it ever fails, refusing beats handing out a mapping
    // that is silently offset from the data.
    if !start.is_multiple_of(crate::arch::PAGE_SIZE as u64) {
        crate::error!("data module {name:?} is not page-aligned at {start:#x}; ignoring it");
        return None;
    }
    // SAFETY: the frame allocator reserved the module's range at boot, so no
    // other object claims it, and the launcher is the boot-time root of
    // authority over what the loader placed in memory.
    match unsafe { crate::mm::vmo::Vmo::new_physical(start, len) } {
        Ok(vmo) => Some(vmo),
        Err(error) => {
            crate::error!("data module {name:?} could not be mapped: {error:?}");
            None
        }
    }
}

/// Launch order for the boot set: lower runs first.
///
/// Only services other modules are wired to need a rank. Everything else is
/// equal and keeps the loader's order.
fn launch_rank(name: &str) -> u32 {
    match name {
        "display" => 0,
        // Both are services with clients below them, and neither is a client
        // of anything, so they can come up in parallel with the console.
        "audio" | "fs" => 1,
        "console" => 2,
        _ => 3,
    }
}

/// Introduce `client` to an already-launched `service`, returning the client's
/// end of the new channel.
///
/// This is the only way a NodeOS program ever reaches a service it was not
/// born holding: somebody who already had the authority to talk to it makes
/// the introduction. Here that somebody is the boot launcher, because the
/// alternative at boot is a name server that nothing has a handle to yet.
fn connect_to(launched: &[Launched], service: &str, client: &str) -> Option<Handle> {
    let Some(entry) = launched.iter().find(|entry| entry.name == service) else {
        crate::warn!("{client:?} launched with no {service:?} module to connect to");
        return None;
    };

    let (client_end, server_end) = Channel::create_pair();
    let mut connect = Message::empty(proto::CONNECT);
    connect.handles.push(Handle::root(server_end, Channel::default_rights()));
    if entry.control.send(connect).is_err() {
        crate::error!("failed to introduce {client:?} to {service:?}");
        return None;
    }
    Some(Handle::root(client_end, Channel::default_rights()))
}

/// Load and start every boot module, returning what was launched.
///
/// # Safety
/// Must run after the memory manager and the scheduler are up, with
/// `boot_info` relocated to the direct physical map.
pub unsafe fn launch_boot_modules(boot_info: &BootInfo, devices: &BootDevices) -> Vec<Launched> {
    let mut launched: Vec<Launched> = Vec::new();
    let mut data: Vec<DataModule> = Vec::new();
    let mut images: Vec<(&str, &[u8])> = Vec::new();
    let mut found = 0usize;

    // First pass: separate executables from data. Data has to be in hand
    // before anything is started, because a program's bootstrap message is
    // built once and cannot be added to afterwards -- and the boot loader
    // gives no guarantee about the order modules appear in.
    for module in boot_info.modules() {
        found += 1;
        let pointer = boot_info.module_ptr(&module);
        // SAFETY: the frame allocator reserved every module's physical range,
        // and the direct map covers it, so the bytes are stable and readable.
        let bytes = unsafe { core::slice::from_raw_parts(pointer, module.len()) };

        if bytes.len() >= nodeos_abi::nxf::MAGIC.len() && bytes[..4] == nodeos_abi::nxf::MAGIC {
            images.push((module.name, bytes));
        } else {
            crate::info!("module {:?} is data ({} KiB)", module.name, module.len() / 1024);
            // SAFETY: `module.start` is the physical base the loader reported
            // for a range the frame allocator reserved.
            if let Some(vmo) = unsafe { data_module_vmo(module.name, module.start, module.len() as u64) } {
                data.push(DataModule { name: alloc::string::String::from(module.name), vmo });
            }
        }
    }

    // The launcher introduces clients to services as it goes, so a service has
    // to be running before anything that needs a channel to it starts. The
    // loader gives no ordering guarantee -- GRUB lists modules in whatever
    // order its configuration file did -- so state the order here rather than
    // depend on somebody else's file getting it right. A stable sort leaves
    // everything of equal rank in the order the loader supplied.
    images.sort_by_key(|(name, _)| launch_rank(name));

    for (name, bytes) in images {
        // Extra capabilities this module's bootstrap message should carry,
        // decided here rather than inside `launch_one`, because most of them
        // depend on another module already being launched -- which only the
        // caller can know.
        let extra_handles = match name {
            // The display broker is the one holder of the screen and the keyboard.
            // Every other program that draws gets a channel to it instead,
            // below.
            "display" => match &devices.framebuffer {
                Some(framebuffer) => alloc::vec![
                    Handle::root(framebuffer.clone(), framebuffer.default_rights()),
                    Handle::root(devices.keyboard.clone(), devices.keyboard.default_rights()),
                ],
                None => {
                    crate::warn!("display broker launched with no framebuffer to drive");
                    Vec::new()
                }
            },
            "console" => connect_to(&launched, "display", name).into_iter().collect(),
            // The audio service is handed the sound card the same way the
            // display broker is handed the screen: one device capability, held by
            // one process, for the lifetime of the boot.
            "audio" => devices
                .audio
                .as_ref()
                .map(|device| alloc::vec![Handle::root(device.clone(), device.default_rights())])
                .unwrap_or_else(|| {
                    crate::warn!("audio launched with no sound card to drive");
                    Vec::new()
                }),
            "fs" => devices
                .storage
                .as_ref()
                .map(|device| alloc::vec![Handle::root(device.clone(), device.default_rights())])
                .unwrap_or_else(|| {
                    crate::warn!("fs launched with no disk to serve");
                    Vec::new()
                }),
            // The shell reads its capabilities positionally: the console
            // first, the filesystem second. A boot with no `fs` module gives
            // it one handle and the shell's file commands report that they
            // have nowhere to look, which is better than a shell that will not
            // start.
            // The shell reads its capabilities positionally: console first,
            // filesystem second, then the two it only forwards to the programs
            // it launches -- the display broker (so a graphical program like Doom
            // can take the full screen) and the mixer (so Doom can make a noise). A
            // boot with no `audio` module simply omits the last one.
            "shell" => {
                let mut handles: Vec<Handle> = connect_to(&launched, "console", name).into_iter().collect();
                if !handles.is_empty() {
                    handles.extend(connect_to(&launched, "fs", name));
                    handles.extend(connect_to(&launched, "display", name));
                    handles.extend(connect_to(&launched, "audio", name));
                }
                handles
            }
            // Doom's whole authority is two handles: a window, and the IWAD as
            // a read-only memory object. It cannot create a process, touch a
            // filesystem, or reach any device -- including the screen it is
            // drawing on, which it now reaches only through the display broker.
            //
            // The connection has to come first or not at all: `doom` reads its
            // capabilities positionally, and handing it a WAD where it expects
            // a channel would be worse than handing it nothing.
            "doom" => match connect_to(&launched, "display", name) {
                Some(connection) => {
                    let mut handles = alloc::vec![connection];
                    match data.iter().find(|entry| entry.name.ends_with("wad")) {
                        Some(wad) => handles.push(Handle::root(wad.vmo.clone(), Rights::READ_ONLY)),
                        None => crate::warn!("doom launched with no IWAD module to read"),
                    }
                    // Sound is optional and comes last for that reason: a boot
                    // with no `audio` module leaves Doom holding two handles
                    // and playing silently, rather than failing to start over
                    // a sound card.
                    handles.extend(connect_to(&launched, "audio", name));
                    handles
                }
                None => Vec::new(),
            },
            _ => Vec::new(),
        };

        match launch_one(name, bytes, extra_handles) {
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
                crate::error!("failed to launch module {:?}: {:?}", name, error);
            }
        }
    }
    if found == 0 {
        crate::warn!("no boot modules were supplied; userspace will not start");
    }
    launched
}

fn launch_one(name: &str, bytes: &[u8], extra_handles: Vec<Handle>) -> Result<Launched, LoadError> {
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

    let info = nodeos_abi::proto::Encoded::new(BootstrapMessage {
        version: 1,
        granted_caps: granted.0,
        process: HandleValue(process_handle.0),
        space: HandleValue(space_handle.0),
        thread: HandleValue(thread_handle.0),
        namespace: HandleValue::INVALID,
        image_base: image.base,
        heap_size: requirements.resources.heap_reserve,
        extra_count: 0,
        flags: 0,
    });

    let mut message = Message::empty(KIND_BOOTSTRAP);
    message.payload.extend_from_slice(info.bytes());
    message.handles.extend(extra_handles);

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

/// Give `target` a capability, as a launcher would.
///
/// Used to wire services to each other after they are all running. Returns
/// `None` only when the process's handle table is full.
pub fn grant(target: &Launched, handle: Handle) -> Option<crate::obj::HandleValue> {
    target.process.install(handle).ok()
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
