//! The syscall layer.
//!
//! Every entry point here is a *mechanism*. None of them implement a policy,
//! and none of them consult an identity: authority comes from the handle
//! argument, and the kernel's only question is "does this handle carry the
//! right for this operation?".
//!
//! See [`nodeos_abi::syscall`] for the numbering and the register convention.

pub mod entry;

use alloc::sync::Arc;
use alloc::vec::Vec;

use nodeos_abi::message::{MessageDescriptor, MAX_HANDLES, MAX_PAYLOAD};
use nodeos_abi::types::{HandleInfo, LoadedImageInfo, MapFlags as AbiMapFlags, MapWindow, ThreadParams};
use nodeos_abi::{Error, Syscall};

use crate::arch::x86_64::trap::TrapFrame;
use crate::ipc::message::{Message, MessageFlags};
use crate::ipc::{Channel, ChannelError};
use crate::mm::layout::Window;
use crate::mm::space::{AddressSpace, MapFlags};
use crate::mm::uaccess;
use crate::mm::vmo::Vmo;
use crate::obj::{CapClass, Device, Handle, HandleValue, KernelObject, ObjectError, ObjectType, Rights, Signals};
use crate::obj::device::DeviceError;
use crate::sched::{self, SchedParams};
use crate::task::thread::{Thread, ThreadEntry};
use crate::task::Process;
use crate::time;

/// Install the `SYSCALL` entry point on the calling CPU.
///
/// # Safety
/// Must run with interrupts disabled, after the GDT is loaded.
pub unsafe fn init() {
    // SAFETY: the GDT has the layout `init_syscall` documents, and
    // `entry_address` is the address of the stub in `entry.rs`.
    unsafe { crate::arch::x86_64::segmentation::init_syscall(entry::entry_address()) };
    crate::info!("syscall ABI online: {} entry points", Syscall::MAX);
}

fn map_object_error(error: ObjectError) -> Error {
    match error {
        ObjectError::BadHandle => Error::BadHandle,
        ObjectError::AccessDenied => Error::AccessDenied,
        ObjectError::WrongType => Error::WrongType,
        ObjectError::OutOfHandles => Error::NoSpace,
        ObjectError::GrantTooDeep => Error::LimitExceeded,
        ObjectError::Revoked => Error::Revoked,
    }
}

fn map_channel_error(error: ChannelError) -> Error {
    match error {
        ChannelError::PeerClosed | ChannelError::Closed => Error::PeerClosed,
        ChannelError::Empty => Error::ShouldWait,
        ChannelError::Full => Error::ShouldRetry,
        ChannelError::TimedOut => Error::TimedOut,
        ChannelError::Message(_) => Error::InvalidArgument,
    }
}

/// Arguments as they arrive in the trap frame.
struct Args {
    a0: u64,
    a1: u64,
    a2: u64,
    a3: u64,
    a4: u64,
    a5: u64,
}

impl Args {
    fn from(frame: &TrapFrame) -> Self {
        // RCX and R11 are destroyed by SYSCALL, so the fourth argument travels
        // in R10 -- the same convention the Linux ABI adopted for the same
        // hardware reason.
        Self { a0: frame.rdi, a1: frame.rsi, a2: frame.rdx, a3: frame.r10, a4: frame.r8, a5: frame.r9 }
    }
}

/// Dispatch a syscall. Called by the `SYSCALL` entry stub.
///
/// Returns the trap frame to resume on, which lets a syscall that blocks or
/// yields hand back a different thread's frame.
///
/// # Safety
/// Called only from `syscall_entry`, with `frame` pointing at a complete
/// [`TrapFrame`] on the calling thread's kernel stack.
#[no_mangle]
pub unsafe extern "C" fn syscall_dispatch(frame: *mut TrapFrame) -> *mut TrapFrame {
    // SAFETY: the entry stub built this frame; it is valid and uniquely
    // borrowed for the duration of the call.
    let frame = unsafe { &mut *frame };

    let number = frame.rax;
    let args = Args::from(frame);

    let result = match Syscall::from_u64(number) {
        Some(syscall) => dispatch(syscall, &args, frame),
        None => Err(Error::NoSyscall),
    };

    frame.rax = match result {
        Ok(value) => value as u64,
        Err(error) => error.to_raw() as u64,
    };

    // The only place kernel code voluntarily gives up the CPU on the way back
    // to userspace. Preemption requested by a timer interrupt is serviced here,
    // on this thread's own kernel stack, where blocking is safe.
    sched::preempt_point();

    frame as *mut TrapFrame
}

fn dispatch(syscall: Syscall, args: &Args, frame: &mut TrapFrame) -> Result<i64, Error> {
    crate::trace!("syscall {} ({:#x}, {:#x}, {:#x})", syscall.name(), args.a0, args.a1, args.a2);
    match syscall {
        Syscall::Invalid => Err(Error::NoSyscall),

        Syscall::ProcessCreate => sys_process_create(args),
        Syscall::ProcessStart => sys_process_start(args),
        Syscall::ProcessLoad => sys_process_load(args),
        Syscall::ProcessExit => sys_process_exit(args),
        Syscall::ThreadCreate => sys_thread_create(args),
        Syscall::ThreadExit => sys_thread_exit(args),
        Syscall::ThreadStart => sys_thread_start(args),
        Syscall::ThreadSetFsBase => sys_thread_set_fs_base(args),
        Syscall::Yield => {
            sched::yield_now();
            Ok(0)
        }

        Syscall::MemoryAllocate => sys_memory_allocate(args),
        Syscall::MemoryMap => sys_memory_map(args),
        Syscall::MemoryUnmap => sys_memory_unmap(args),
        Syscall::MemoryProtect => sys_memory_protect(args),
        Syscall::MemoryTransfer => sys_memory_transfer(args),

        Syscall::ChannelCreate => sys_channel_create(args),
        Syscall::ChannelSend => sys_channel_send(args),
        Syscall::ChannelReceive => sys_channel_receive(args),
        Syscall::ChannelCall => sys_channel_call(args),

        Syscall::CapabilityDuplicate => sys_capability_duplicate(args),
        Syscall::CapabilityRevoke => sys_capability_revoke(args),
        Syscall::CapabilityClose => sys_capability_close(args),
        Syscall::CapabilityQuery => sys_capability_query(args),

        Syscall::Wait => sys_wait(args),
        Syscall::ObjectSignal => sys_object_signal(args),
        Syscall::Clock => Ok(time::now_ns() as i64),
        Syscall::DebugLog => sys_debug_log(args, frame),

        Syscall::DeviceGetVmo => sys_device_get_vmo(args),
        Syscall::DeviceRead => sys_device_read(args),
        Syscall::DeviceInfo => sys_device_info(args),
        Syscall::DevicePioRead => sys_device_pio_read(args),
        Syscall::DevicePioWrite => sys_device_pio_write(args),
        Syscall::DeviceDmaAlloc => sys_device_dma_alloc(args),
    }
}

fn current() -> Result<Arc<Process>, Error> {
    sched::current_process().ok_or(Error::BadState)
}

fn handle_of(value: u64) -> HandleValue {
    HandleValue(value as u32)
}

// ---------------------------------------------------------------------------
// Execution
// ---------------------------------------------------------------------------

/// `process_create(name_ptr, name_len, requested_caps) -> handle`
///
/// Creating a process is itself an authority: without the `PROCESS` capability
/// class a program cannot make one, which is what stops a compromised leaf
/// service from spawning helpers.
fn sys_process_create(args: &Args) -> Result<i64, Error> {
    let parent = current()?;
    if !parent.granted().contains(CapClass::Process) {
        return Err(Error::CapabilityDenied);
    }
    let name = uaccess::read_str(args.a0, args.a1 as usize)?;

    // A child can never be granted a class the parent does not hold. This is
    // the process-level analogue of rights intersection on handles.
    let requested = crate::obj::CapSet(args.a2 as u32);
    let granted = requested.and(parent.granted());

    let child = Process::new(&name, Some(&parent), granted).ok_or(Error::NoMemory)?;
    let handle = parent
        .install_object(child, Rights::PROCESS_DEFAULT)
        .map_err(map_object_error)?;
    Ok(handle.0 as i64)
}

/// `process_start(process, entry, stack, bootstrap_handle) -> status`
///
/// The bootstrap handle is *moved* into the child and delivered in `RDI`. It is
/// the only handle the child starts with; everything else must arrive as a
/// message on it. That single constraint is what makes a NodeOS process's
/// authority auditable: read one channel's traffic and you have seen everything
/// it was ever given.
fn sys_process_start(args: &Args) -> Result<i64, Error> {
    let parent = current()?;
    let handle = parent
        .handles()
        .get_checked(handle_of(args.a0), Rights::MANAGE_PROCESS)
        .map_err(map_object_error)?;
    let child = process_arc(handle.object())?;

    if child.live_threads() > 0 {
        return Err(Error::BadState);
    }

    // Move the bootstrap capability out of the parent and into the child. It is
    // a move, not a copy: after this the parent no longer holds it, so "who can
    // talk to this process" has exactly one answer.
    let bootstrap = handle_of(args.a3);
    let bootstrap_value = if bootstrap.is_invalid() {
        HandleValue::INVALID
    } else {
        let moved = parent.handles().take(bootstrap).map_err(map_object_error)?;
        child.install(moved).map_err(map_object_error)?
    };

    let thread = Thread::new(
        &child,
        "main",
        ThreadEntry::User { entry: args.a1, stack: args.a2, arg0: bootstrap_value.0 as u64, arg1: 0 },
        SchedParams::normal(),
    )
    .ok_or(Error::NoMemory)?;

    // A thread holds only a `Weak` reference to its process, so now that this
    // process is about to run, nothing but its own handle table keeps it alive
    // -- the parent may drop its handle a moment later, and the child must not
    // be torn down from under its running thread. The boot launcher gives every
    // process a handle to itself for exactly this reason; do the same, here
    // rather than at `process_create` so that a process which is created and
    // never started can still be freed by dropping the parent's handle.
    // `Process::terminate` breaks the resulting cycle by clearing the table.
    if child.install_object(child.clone(), Rights::PROCESS_DEFAULT).is_err() {
        return Err(Error::NoMemory);
    }

    sched::enqueue(&thread);
    Ok(0)
}

/// `process_load(process, image_vmo, image_len, result_ptr) -> status`
///
/// The caller supplies an NXF image in a readable VMO. The kernel validates
/// and maps it with the existing loader, using the capabilities already
/// granted to the child by `process_create`.
fn sys_process_load(args: &Args) -> Result<i64, Error> {
    let parent = current()?;
    if !parent.granted().contains(CapClass::Load) {
        return Err(Error::CapabilityDenied);
    }
    let process_handle = parent
        .handles()
        .get_checked(handle_of(args.a0), Rights::MANAGE_PROCESS)
        .map_err(map_object_error)?;
    let child = process_arc(process_handle.object())?;
    if child.live_threads() > 0 {
        return Err(Error::BadState);
    }
    let image_handle = parent
        .handles()
        .get_checked(handle_of(args.a1), Rights::READ)
        .map_err(map_object_error)?;
    let image = object_arc::<Vmo>(image_handle.object())?;
    let size = args.a2 as usize;
    if size == 0 || size > image.size() as usize || size > 64 * 1024 * 1024 {
        return Err(Error::InvalidArgument);
    }
    let mut bytes = Vec::new();
    bytes.try_reserve_exact(size).map_err(|_| Error::NoMemory)?;
    bytes.resize(size, 0);
    image.read(0, &mut bytes).map_err(|_| Error::InvalidArgument)?;
    let loaded = crate::loader::load(&child, &bytes, child.granted()).map_err(|error| match error {
        crate::loader::LoadError::NoMemory => Error::NoMemory,
        crate::loader::LoadError::CapabilityDenied(_) => Error::CapabilityDenied,
        crate::loader::LoadError::Format(_)
        | crate::loader::LoadError::Mapping
        | crate::loader::LoadError::MissingDependency(_) => Error::InvalidArgument,
    })?;
    let info = LoadedImageInfo { entry: loaded.entry, stack: loaded.stack_top };
    uaccess::write_struct(args.a3, &info)?;
    Ok(0)
}

/// Recover an owned `Arc<Process>` from an object reference.
fn process_arc(object: &crate::obj::ObjectRef) -> Result<Arc<Process>, Error> {
    if crate::obj::downcast::<Process>(object).is_none() {
        return Err(Error::WrongType);
    }
    let cloned = object.clone();
    let raw = Arc::into_raw(cloned) as *const Process;
    // SAFETY: the downcast above proved the concrete type is `Process`, and
    // `Arc::into_raw`/`from_raw` round-trip the same allocation with the same
    // reference count.
    Ok(unsafe { Arc::from_raw(raw) })
}

/// Recover an owned `Arc<Thread>` from an object reference.
fn thread_arc(object: &crate::obj::ObjectRef) -> Result<Arc<Thread>, Error> {
    if crate::obj::downcast::<Thread>(object).is_none() {
        return Err(Error::WrongType);
    }
    let cloned = object.clone();
    let raw = Arc::into_raw(cloned) as *const Thread;
    // SAFETY: as in `process_arc`.
    Ok(unsafe { Arc::from_raw(raw) })
}

/// Recover an owned `Arc<T>` for any kernel object type.
fn object_arc<T: KernelObject>(object: &crate::obj::ObjectRef) -> Result<Arc<T>, Error> {
    if object.as_any().downcast_ref::<T>().is_none() {
        return Err(Error::WrongType);
    }
    let cloned = object.clone();
    let raw = Arc::into_raw(cloned) as *const T;
    // SAFETY: as in `process_arc`.
    Ok(unsafe { Arc::from_raw(raw) })
}

/// `process_exit(code)` -- does not return.
fn sys_process_exit(args: &Args) -> Result<i64, Error> {
    let process = current()?;
    process.terminate(args.a0 as i64);
    sched::exit_current(args.a0 as i64);
}

/// `thread_create(process, entry, stack, params_ptr) -> handle`
fn sys_thread_create(args: &Args) -> Result<i64, Error> {
    let caller = current()?;
    let handle = caller
        .handles()
        .get_checked(handle_of(args.a0), Rights::MANAGE_PROCESS)
        .map_err(map_object_error)?;
    let target = process_arc(handle.object())?;

    let params = if args.a3 == 0 {
        ThreadParams::default()
    } else {
        uaccess::read_struct::<ThreadParams>(args.a3)?
    };

    let class = crate::sched::SchedClass::from_u32(params.class).ok_or(Error::InvalidArgument)?;
    // A process cannot promote itself into the real-time class without the
    // authority to do so: an unbounded real-time thread is a denial of service
    // against every other thread on the machine.
    if class == crate::sched::SchedClass::RealTime && !caller.granted().contains(CapClass::Clock) {
        return Err(Error::CapabilityDenied);
    }

    let mut sched_params = SchedParams { class, priority: params.priority.min(31) as u8, ..SchedParams::normal() };
    sched_params.period_ns = params.period_ns;
    sched_params.budget_ns = params.budget_ns;
    sched_params.affinity = params.affinity;

    let thread = Thread::new(
        &target,
        "thread",
        ThreadEntry::User { entry: args.a1, stack: args.a2, arg0: 0, arg1: 0 },
        sched_params,
    )
    .ok_or(Error::NoMemory)?;

    let value = caller
        .install_object(thread.clone(), Rights::THREAD_DEFAULT)
        .map_err(map_object_error)?;
    sched::enqueue(&thread);
    Ok(value.0 as i64)
}

/// `thread_exit(code)` -- does not return.
fn sys_thread_exit(args: &Args) -> Result<i64, Error> {
    sched::exit_current(args.a0 as i64);
}

/// `thread_start(thread) -> status`
fn sys_thread_start(args: &Args) -> Result<i64, Error> {
    let caller = current()?;
    let handle = caller
        .handles()
        .get_checked(handle_of(args.a0), Rights::MANAGE_THREAD)
        .map_err(map_object_error)?;
    let thread = thread_arc(handle.object())?;
    // `thread_create` already makes a new thread runnable, and `Thread::new`
    // starts in `Ready`. Enqueueing an already-runnable thread a second time
    // would push a duplicate entry (and, for the weighted-fair-sharing class,
    // bump the run-queue count without adding a distinct entry, corrupting
    // `len`/`is_empty`). A start on a thread that is already running is a
    // no-op; a start on a blocked thread is refused rather than silently
    // dropped, because only a wakeup may make a blocked thread runnable.
    match thread.state() {
        crate::task::ThreadState::Ready | crate::task::ThreadState::Running => Ok(0),
        crate::task::ThreadState::Suspended => {
            sched::enqueue(&thread);
            Ok(0)
        }
        crate::task::ThreadState::Blocked | crate::task::ThreadState::Dead => Err(Error::BadState),
    }
}

/// `thread_set_fs_base(thread, value) -> status`
///
/// The invalid handle means the calling thread, as it means the caller's own
/// address space for `memory_map`. The value must be a canonical lower-half
/// address: `wrmsr` with a non-canonical one would fault inside the kernel,
/// and a kernel address would hand userspace nothing it could use.
fn sys_thread_set_fs_base(args: &Args) -> Result<i64, Error> {
    let value = args.a1;
    if value >= 0x0000_8000_0000_0000 {
        return Err(Error::InvalidArgument);
    }
    if args.a0 == 0 {
        let thread = sched::current_thread().ok_or(Error::BadState)?;
        thread.set_fs_base(value);
        // SAFETY: `value` is canonical (checked above), and the kernel never
        // addresses through `FS`.
        unsafe { crate::arch::x86_64::cpu::wrmsr(crate::arch::x86_64::cpu::MSR_FS_BASE, value) };
        return Ok(0);
    }
    let caller = current()?;
    let handle = caller
        .handles()
        .get_checked(handle_of(args.a0), Rights::MANAGE_THREAD)
        .map_err(map_object_error)?;
    let thread = thread_arc(handle.object())?;
    if sched::current_thread().is_some_and(|current| Arc::ptr_eq(&current, &thread)) {
        // SAFETY: as above.
        unsafe { crate::arch::x86_64::cpu::wrmsr(crate::arch::x86_64::cpu::MSR_FS_BASE, value) };
    }
    thread.set_fs_base(value);
    Ok(0)
}

// ---------------------------------------------------------------------------
// Memory
// ---------------------------------------------------------------------------

/// `memory_allocate(size, flags) -> vmo handle`
fn sys_memory_allocate(args: &Args) -> Result<i64, Error> {
    let process = current()?;
    let size = args.a0;
    if size == 0 || size > 1 << 40 {
        return Err(Error::InvalidArgument);
    }
    let resizable = args.a1 & 1 != 0;
    let vmo = Vmo::new_anonymous(size, resizable).map_err(|_| Error::NoMemory)?;
    let rights = vmo.default_rights();
    let value = process.install_object(vmo, rights).map_err(map_object_error)?;
    Ok(value.0 as i64)
}

fn decode_window(raw: u32) -> Result<Window, Error> {
    Ok(match raw {
        x if x == MapWindow::Program as u32 => Window::Program,
        x if x == MapWindow::Heap as u32 => Window::Heap,
        x if x == MapWindow::Stack as u32 => Window::Stack,
        x if x == MapWindow::Shared as u32 => Window::Shared,
        x if x == MapWindow::Ipc as u32 => Window::Ipc,
        x if x == MapWindow::MappedFile as u32 => Window::MappedFile,
        x if x == MapWindow::Device as u32 => Window::Device,
        x if x == MapWindow::Runtime as u32 => Window::Runtime,
        _ => return Err(Error::InvalidArgument),
    })
}

fn decode_map_flags(raw: u32) -> MapFlags {
    let mut flags = MapFlags(0);
    if raw & AbiMapFlags::READ.0 != 0 {
        flags = flags.or(MapFlags::READ);
    }
    if raw & AbiMapFlags::WRITE.0 != 0 {
        flags = flags.or(MapFlags::WRITE);
    }
    if raw & AbiMapFlags::EXECUTE.0 != 0 {
        flags = flags.or(MapFlags::EXECUTE);
    }
    if raw & AbiMapFlags::COW.0 != 0 {
        flags = flags.or(MapFlags::COW);
    }
    if raw & AbiMapFlags::SHARED.0 != 0 {
        flags = flags.or(MapFlags::SHARED);
    }
    if raw & AbiMapFlags::POPULATE.0 != 0 {
        flags = flags.or(MapFlags::POPULATE);
    }
    if raw & AbiMapFlags::DEVICE.0 != 0 {
        flags = flags.or(MapFlags::DEVICE);
    }
    flags
}

/// `memory_map(space, vmo, offset, len, flags | window << 16, hint) -> address`
fn sys_memory_map(args: &Args) -> Result<i64, Error> {
    let process = current()?;
    let space = resolve_space(&process, args.a0)?;

    let flags = decode_map_flags(args.a4 as u32 & 0xFFFF);
    let window = decode_window((args.a4 >> 16) as u32)?;

    // The rights the *handle* must carry are derived from the permissions the
    // mapping asks for. This is the single check that stops a read-only VMO
    // handle from ever producing a writable page.
    let required = flags.required_rights();
    let handle = process
        .handles()
        .get_checked(handle_of(args.a1), required)
        .map_err(map_object_error)?;
    let vmo = object_arc::<Vmo>(handle.object())?;

    let hint = if args.a5 == 0 { None } else { Some(args.a5) };
    let base = space
        .map(vmo, args.a2, args.a3, flags, window, hint)
        .map_err(|error| match error {
            crate::mm::space::SpaceError::Overlap => Error::AlreadyExists,
            crate::mm::space::SpaceError::NoSpace => Error::NoSpace,
            crate::mm::space::SpaceError::OutOfMemory => Error::NoMemory,
            crate::mm::space::SpaceError::PermissionEscalation => Error::WouldEscalate,
            _ => Error::BadAddress,
        })?;
    Ok(base as i64)
}

fn resolve_space(process: &Arc<Process>, value: u64) -> Result<Arc<AddressSpace>, Error> {
    if value == 0 {
        // The invalid handle means "my own address space". This is not ambient
        // authority: a process can always act on itself, and denying it would
        // only force every program to carry a handle to itself.
        return Ok(process.space().clone());
    }
    let handle = process
        .handles()
        .get_checked(handle_of(value), Rights::WRITE)
        .map_err(map_object_error)?;
    object_arc::<AddressSpace>(handle.object())
}

/// `memory_unmap(space, address) -> status`
fn sys_memory_unmap(args: &Args) -> Result<i64, Error> {
    let process = current()?;
    let space = resolve_space(&process, args.a0)?;
    space.unmap(args.a1).map_err(|_| Error::NotFound)?;
    Ok(0)
}

/// `memory_protect(space, address, flags) -> status`
fn sys_memory_protect(args: &Args) -> Result<i64, Error> {
    let process = current()?;
    let space = resolve_space(&process, args.a0)?;
    let flags = decode_map_flags(args.a2 as u32);
    space.protect(args.a1, flags).map_err(|error| match error {
        crate::mm::space::SpaceError::PermissionEscalation => Error::WouldEscalate,
        crate::mm::space::SpaceError::NotMapped => Error::NotFound,
        _ => Error::InvalidArgument,
    })?;
    Ok(0)
}

/// `memory_transfer(vmo, offset, user_ptr, len, write) -> bytes`
///
/// Read or write a memory object without mapping it. Useful for a loader
/// filling in a child's image, and for small reads where a mapping would cost
/// more than the copy.
fn sys_memory_transfer(args: &Args) -> Result<i64, Error> {
    let process = current()?;
    let write = args.a4 != 0;
    let required = if write { Rights::WRITE } else { Rights::READ };
    let handle = process
        .handles()
        .get_checked(handle_of(args.a0), required)
        .map_err(map_object_error)?;
    let vmo = handle.as_object::<Vmo>(required).map_err(map_object_error)?;

    let len = args.a3 as usize;
    if len > uaccess::MAX_COPY {
        return Err(Error::InvalidArgument);
    }
    if write {
        let bytes = uaccess::read_bytes(args.a2, len)?;
        vmo.write(args.a1, &bytes).map_err(|_| Error::InvalidArgument)?;
    } else {
        let mut bytes = Vec::new();
        bytes.try_reserve_exact(len).map_err(|_| Error::NoMemory)?;
        bytes.resize(len, 0);
        vmo.read(args.a1, &mut bytes).map_err(|_| Error::InvalidArgument)?;
        uaccess::write_bytes(args.a2, &bytes)?;
    }
    Ok(len as i64)
}

// ---------------------------------------------------------------------------
// Devices
// ---------------------------------------------------------------------------

fn map_device_error(error: DeviceError) -> Error {
    match error {
        DeviceError::WrongKind => Error::WrongType,
        DeviceError::NoMemory => Error::NoMemory,
        DeviceError::OutOfRange => Error::InvalidArgument,
    }
}

/// `device_get_vmo(device) -> handle`
///
/// Turns a `Device` capability into a mappable memory object over its MMIO
/// range. Only meaningful for MMIO-backed devices (today: the framebuffer);
/// a device with no MMIO range fails with `WrongType`.
fn sys_device_get_vmo(args: &Args) -> Result<i64, Error> {
    let process = current()?;
    let handle = process
        .handles()
        .get_checked(handle_of(args.a0), Rights::DEVICE_IO)
        .map_err(map_object_error)?;
    let device = object_arc::<Device>(handle.object())?;
    let vmo = device.vmo().map_err(map_device_error)?;
    let rights = vmo.default_rights();
    let handle = process.install_object(vmo, rights).map_err(map_object_error)?;
    Ok(handle.0 as i64)
}

/// `device_info(device, out_ptr) -> status`
///
/// Reports a device's kind-specific properties -- today, just the
/// framebuffer's geometry, which a driver needs before it can address a
/// single pixel and which nothing generic like `capability_query` should
/// carry.
fn sys_device_info(args: &Args) -> Result<i64, Error> {
    use crate::obj::device::DeviceKind;
    use nodeos_abi::types::{device_kind, DeviceInfo};

    let process = current()?;
    let handle = process
        .handles()
        .get_checked(handle_of(args.a0), Rights::GET_PROPERTY)
        .map_err(map_object_error)?;
    let device = object_arc::<Device>(handle.object())?;

    let info = match device.kind() {
        DeviceKind::FramebufferMmio { width, height, pitch, bpp, .. } => {
            DeviceInfo { kind: device_kind::FRAMEBUFFER, width, height, pitch, bpp: bpp as u32, present: 1, ..Default::default() }
        }
        DeviceKind::Ps2Keyboard => DeviceInfo { kind: device_kind::PS2_KEYBOARD, irq: 1, present: 1, ..Default::default() },
        DeviceKind::Ps2Mouse { present } => DeviceInfo {
            kind: device_kind::PS2_MOUSE,
            irq: 12,
            present: present as u32,
            ..Default::default()
        },
        DeviceKind::Pci { role, vendor, device, windows, irq } => {
            let mut info = DeviceInfo {
                kind: match role {
                    crate::obj::device::PciRole::Ac97 => device_kind::AC97,
                    crate::obj::device::PciRole::VirtioBlk => device_kind::VIRTIO_BLK,
                },
                vendor: vendor as u32,
                device: device as u32,
                irq: irq as u32,
                present: 1,
                ..Default::default()
            };
            for (index, window) in windows.iter().enumerate() {
                info.port_base[index] = window.base as u32;
                info.port_len[index] = window.len as u32;
            }
            info
        }
    };
    uaccess::write_struct(args.a1, &info)?;
    Ok(0)
}

/// `device_read(device, buf_ptr, buf_len) -> bytes`
///
/// Drains up to `buf_len` queued bytes (e.g. keyboard scancodes) into the
/// caller's buffer, non-blocking: an empty device reads back `0`. Callers
/// `wait` on the handle's `READABLE` signal first, the same idiom as a
/// channel.
fn sys_device_read(args: &Args) -> Result<i64, Error> {
    let process = current()?;
    let handle = process
        .handles()
        .get_checked(handle_of(args.a0), Rights::READ)
        .map_err(map_object_error)?;
    let device = object_arc::<Device>(handle.object())?;

    let len = (args.a2 as usize).min(uaccess::MAX_COPY);
    let mut bytes = Vec::new();
    bytes.try_reserve_exact(len).map_err(|_| Error::NoMemory)?;
    bytes.resize(len, 0);
    let n = device.drain(&mut bytes);
    uaccess::write_bytes(args.a1, &bytes[..n])?;
    Ok(n as i64)
}

/// `device_pio_read(device, window, offset, width) -> value`
///
/// The narrow waist of userspace driver support: ring 3 cannot execute `in`,
/// so it asks the kernel to, and the kernel's only question is whether the
/// port is inside a window this device decodes. There is no port number in
/// this interface at all -- the caller names an offset within a window it was
/// granted, so a driver that gets the arithmetic wrong reads its own device's
/// register or an error, never somebody else's hardware.
fn sys_device_pio_read(args: &Args) -> Result<i64, Error> {
    let process = current()?;
    let handle = process
        .handles()
        .get_checked(handle_of(args.a0), Rights::DEVICE_IO)
        .map_err(map_object_error)?;
    let device = object_arc::<Device>(handle.object())?;
    device
        .pio_read(args.a1 as usize, args.a2 as u16, args.a3 as u8)
        .map(|value| value as i64)
        .map_err(map_device_error)
}

/// `device_pio_write(device, window, offset, width, value) -> status`
///
/// See [`sys_device_pio_read`].
fn sys_device_pio_write(args: &Args) -> Result<i64, Error> {
    let process = current()?;
    let handle = process
        .handles()
        .get_checked(handle_of(args.a0), Rights::DEVICE_IO)
        .map_err(map_object_error)?;
    let device = object_arc::<Device>(handle.object())?;
    device
        .pio_write(args.a1 as usize, args.a2 as u16, args.a3 as u8, args.a4 as u32)
        .map(|()| 0)
        .map_err(map_device_error)
}

/// `device_dma_alloc(device, len, out_ptr) -> status`
///
/// Returns a memory object and the physical address a bus-mastering device
/// must be pointed at. Gated on the device handle rather than on a memory
/// right, because knowing a physical address is only useful to something that
/// can make a device follow it, and that is what the device capability is.
fn sys_device_dma_alloc(args: &Args) -> Result<i64, Error> {
    use nodeos_abi::types::DmaInfo;

    let process = current()?;
    let handle = process
        .handles()
        .get_checked(handle_of(args.a0), Rights::DEVICE_IO)
        .map_err(map_object_error)?;
    let device = object_arc::<Device>(handle.object())?;

    let vmo = device.dma_alloc(args.a1).map_err(map_device_error)?;
    let info = DmaInfo {
        handle: 0,
        _reserved: 0,
        phys: vmo.phys_base(),
        len: vmo.size(),
    };
    let rights = vmo.default_rights();
    let value = process
        .handles()
        .insert(Handle::root(vmo, rights))
        .map_err(map_object_error)?;
    let info = DmaInfo { handle: value.0, ..info };
    // Written after the handle exists so a failed copy-out cannot leave the
    // caller with a physical address and no object, which is the one ordering
    // that would matter here.
    if let Err(error) = uaccess::write_struct(args.a2, &info) {
        let _ = process.handles().remove(value);
        return Err(error);
    }
    Ok(0)
}

// ---------------------------------------------------------------------------
// Communication
// ---------------------------------------------------------------------------

/// `channel_create(out_ptr) -> status`; writes two handle values.
fn sys_channel_create(args: &Args) -> Result<i64, Error> {
    let process = current()?;
    if !process.granted().contains(CapClass::Channel) && process.pid() != 1 {
        return Err(Error::CapabilityDenied);
    }
    let (a, b) = Channel::create_pair();
    let first = process
        .install_object(a, Channel::default_rights())
        .map_err(map_object_error)?;
    let second = match process.install_object(b, Channel::default_rights()) {
        Ok(value) => value,
        Err(error) => {
            let _ = process.handles().remove(first);
            return Err(map_object_error(error));
        }
    };
    // Both endpoints already exist in the caller's table; if the handle values
    // cannot be written back, close them rather than leaking a channel the
    // process has no way to name.
    if let Err(error) = uaccess::write_array(args.a0, &[first.0, second.0]) {
        let _ = process.handles().remove(first);
        let _ = process.handles().remove(second);
        return Err(error);
    }
    Ok(0)
}

/// Build a kernel message from a user descriptor, consuming the named handles.
fn take_message(process: &Arc<Process>, descriptor: &MessageDescriptor) -> Result<Message, Error> {
    let payload_len = descriptor.payload_len as usize;
    if payload_len > MAX_PAYLOAD {
        return Err(Error::InvalidArgument);
    }
    let handle_count = descriptor.handle_count as usize;
    if handle_count > MAX_HANDLES {
        return Err(Error::InvalidArgument);
    }

    let payload = if payload_len == 0 { Vec::new() } else { uaccess::read_bytes(descriptor.payload, payload_len)? };
    let values: Vec<u32> =
        if handle_count == 0 { Vec::new() } else { uaccess::read_array::<u32>(descriptor.handles, handle_count)? };

    // Remove the handles from the sender *before* the send. If the send fails
    // the caller has lost them, which is the honest outcome: a half-transferred
    // capability set is worse than a lost one.
    let mut handles = Vec::new();
    handles.try_reserve_exact(handle_count).map_err(|_| Error::NoMemory)?;
    for value in values {
        // A transfer, not a close: the handle is now owned by the message in
        // flight and may not be treated as the last reference to its object.
        let handle = process.handles().take(HandleValue(value)).map_err(map_object_error)?;
        handles.push(handle);
    }

    Ok(Message {
        kind: descriptor.kind,
        flags: MessageFlags(descriptor.flags),
        txid: descriptor.txid,
        payload,
        handles,
    })
}

/// Deliver a received message into user memory, installing its capabilities.
fn deliver_message(
    process: &Arc<Process>,
    channel: &Arc<Channel>,
    message: Message,
    descriptor_addr: u64,
    mut descriptor: MessageDescriptor,
) -> Result<i64, Error> {
    if message.payload.len() > descriptor.payload_len as usize
        || message.handles.len() > descriptor.handle_count as usize
    {
        // Report what would have been needed rather than truncating, and put
        // the message back so the caller can retry: silently losing a
        // capability -- or, worse, dropping a message that has already been
        // removed from the queue -- is not an acceptable failure mode.
        descriptor.payload_len = message.payload.len() as u32;
        descriptor.handle_count = message.handles.len() as u32;
        uaccess::write_struct(descriptor_addr, &descriptor)?;
        channel.requeue_front(message);
        return Err(Error::NoSpace);
    }

    let mut installed = Vec::new();
    for handle in message.handles {
        match process.install(handle) {
            Ok(value) => installed.push(value.0),
            Err(error) => {
                for value in &installed {
                    let _ = process.handles().remove(HandleValue(*value));
                }
                return Err(map_object_error(error));
            }
        }
    }

    // Copy out. If a user pointer turns out to be bad, undo the capability
    // installation rather than leaking handles the caller can never name.
    let copy = (|| {
        if !message.payload.is_empty() {
            uaccess::write_bytes(descriptor.payload, &message.payload)?;
        }
        if !installed.is_empty() {
            uaccess::write_array(descriptor.handles, &installed)?;
        }
        Ok::<(), Error>(())
    })();
    if let Err(error) = copy {
        for value in &installed {
            let _ = process.handles().remove(HandleValue(*value));
        }
        return Err(error);
    }

    descriptor.kind = message.kind;
    descriptor.flags = message.flags.0;
    descriptor.txid = message.txid;
    descriptor.payload_len = message.payload.len() as u32;
    descriptor.handle_count = installed.len() as u32;
    uaccess::write_struct(descriptor_addr, &descriptor)?;
    Ok(0)
}

/// `channel_send(channel, descriptor_ptr) -> status`
fn sys_channel_send(args: &Args) -> Result<i64, Error> {
    let process = current()?;
    let handle = process
        .handles()
        .get_checked(handle_of(args.a0), Rights::WRITE)
        .map_err(map_object_error)?;
    let channel = object_arc::<Channel>(handle.object())?;
    let descriptor = uaccess::read_struct::<MessageDescriptor>(args.a1)?;
    let message = take_message(&process, &descriptor)?;
    channel.send(message).map_err(map_channel_error)?;
    Ok(0)
}

/// `channel_receive(channel, descriptor_ptr) -> status`
fn sys_channel_receive(args: &Args) -> Result<i64, Error> {
    let process = current()?;
    let handle = process
        .handles()
        .get_checked(handle_of(args.a0), Rights::READ)
        .map_err(map_object_error)?;
    let channel = object_arc::<Channel>(handle.object())?;
    let descriptor = uaccess::read_struct::<MessageDescriptor>(args.a1)?;

    let blocking = MessageFlags(descriptor.flags).contains(MessageFlags::NON_BLOCKING);
    let message = if blocking {
        channel.receive().map_err(map_channel_error)?
    } else {
        channel.receive_blocking().map_err(map_channel_error)?
    };
    deliver_message(&process, &channel, message, args.a1, descriptor)
}

/// `channel_call(channel, request_ptr, reply_ptr) -> status`
fn sys_channel_call(args: &Args) -> Result<i64, Error> {
    let process = current()?;
    let handle = process
        .handles()
        .get_checked(handle_of(args.a0), Rights::READ.or(Rights::WRITE))
        .map_err(map_object_error)?;
    let channel = object_arc::<Channel>(handle.object())?;

    let request_descriptor = uaccess::read_struct::<MessageDescriptor>(args.a1)?;
    let reply_descriptor = uaccess::read_struct::<MessageDescriptor>(args.a2)?;
    let request = take_message(&process, &request_descriptor)?;
    let reply = channel.call(request).map_err(map_channel_error)?;
    deliver_message(&process, &channel, reply, args.a2, reply_descriptor)
}

// ---------------------------------------------------------------------------
// Authority
// ---------------------------------------------------------------------------

/// `capability_duplicate(handle, rights) -> handle`
fn sys_capability_duplicate(args: &Args) -> Result<i64, Error> {
    let process = current()?;
    let value = process
        .handles()
        .duplicate(handle_of(args.a0), Rights(args.a1 as u32))
        .map_err(map_object_error)?;
    Ok(value.0 as i64)
}

/// `capability_revoke(handle) -> status`
fn sys_capability_revoke(args: &Args) -> Result<i64, Error> {
    let process = current()?;
    process.handles().revoke(handle_of(args.a0)).map_err(map_object_error)?;
    Ok(0)
}

/// `capability_close(handle) -> status`
fn sys_capability_close(args: &Args) -> Result<i64, Error> {
    let process = current()?;
    process.handles().remove(handle_of(args.a0)).map_err(map_object_error)?;
    Ok(0)
}

/// `capability_query(handle, info_ptr) -> status`
fn sys_capability_query(args: &Args) -> Result<i64, Error> {
    let process = current()?;
    let handle: Handle = process.handles().get(handle_of(args.a0)).map_err(map_object_error)?;
    let header = handle.object().header();
    let info = HandleInfo {
        koid: header.koid(),
        object_type: object_type_code(header.kind()),
        rights: handle.rights().0,
        signals: header.signals().get().0,
        grant_depth: handle.grant().depth(),
        size: crate::obj::downcast::<Vmo>(handle.object()).map_or(0, |vmo| vmo.size()),
        exit_code: crate::obj::downcast::<Process>(handle.object())
            .filter(|process| process.is_dying())
            .map_or(0, |process| process.exit_code()),
    };
    uaccess::write_struct(args.a1, &info)?;
    Ok(0)
}

fn object_type_code(kind: ObjectType) -> u32 {
    match kind {
        ObjectType::Process => 1,
        ObjectType::Thread => 2,
        ObjectType::Channel => 3,
        ObjectType::Vmo => 4,
        ObjectType::AddressSpace => 5,
        ObjectType::Irq => 6,
        ObjectType::Timer => 7,
        ObjectType::Device => 8,
        ObjectType::Namespace => 9,
        ObjectType::Job => 10,
    }
}

// ---------------------------------------------------------------------------
// Observation
// ---------------------------------------------------------------------------

/// `wait(items_ptr, count, deadline_ns) -> index`
///
/// Blocks until any watched object asserts any signal the caller asked for, and
/// returns the index of the first such item. The observed signals are written
/// back into each item, so one call reports the whole picture.
fn sys_wait(args: &Args) -> Result<i64, Error> {
    use nodeos_abi::types::WaitItem;

    let process = current()?;
    let count = args.a1 as usize;
    if count == 0 || count > 64 {
        return Err(Error::InvalidArgument);
    }
    let deadline = args.a2;

    let mut items = uaccess::read_array::<WaitItem>(args.a0, count)?;

    // Resolve every handle once, up front: a handle that goes away mid-wait
    // should not change which object we are parked on.
    let mut objects = Vec::new();
    for item in &items {
        let handle = process
            .handles()
            .get_checked(HandleValue(item.handle.0), Rights::WAIT)
            .map_err(map_object_error)?;
        objects.push(handle.object().clone());
    }

    loop {
        let mut ready = None;
        for (index, item) in items.iter_mut().enumerate() {
            let observed = objects[index].header().signals().get();
            item.observed = observed.0;
            if observed.0 & item.wanted != 0 && ready.is_none() {
                ready = Some(index);
            }
        }
        if let Some(index) = ready {
            uaccess::write_array(args.a0, &items)?;
            return Ok(index as i64);
        }
        if deadline != 0 && time::now_ns() >= deadline {
            uaccess::write_array(args.a0, &items)?;
            return Err(Error::TimedOut);
        }

        let Some(thread) = sched::current_thread() else { return Err(Error::BadState) };
        for object in &objects {
            object.header().signals().queue().park(&thread);
        }
        // Re-check after parking so a signal raised in the gap is not missed.
        let raised = items
            .iter()
            .enumerate()
            .any(|(index, item)| objects[index].header().signals().get().0 & item.wanted != 0);
        if !raised {
            if deadline != 0 {
                // Two things can end this wait -- a signal or the clock -- so
                // the thread is registered with both and blocks once. Spinning
                // here instead (which is what this did before there was a
                // sleeper list) made a service polling on a few milliseconds
                // starve every other process on the machine: it was runnable
                // at every scheduling decision and nothing else ever got
                // picked. See `time::wake_expired`.
                time::sleep_until(&thread, deadline);
            }
            sched::block_current();
        }
        if deadline != 0 {
            time::cancel_sleep(&thread);
        }
        for object in &objects {
            object.header().signals().queue().unpark(&thread);
        }
    }
}

/// `object_signal(handle, set, clear) -> status`
fn sys_object_signal(args: &Args) -> Result<i64, Error> {
    let process = current()?;
    let handle = process
        .handles()
        .get_checked(handle_of(args.a0), Rights::SIGNAL)
        .map_err(map_object_error)?;
    // Userspace may only touch the user-defined bits; the kernel-defined ones
    // describe facts about the object, and letting a process lie about them
    // would make every `wait` untrustworthy.
    let set = Signals(args.a1 as u32).and(Signals::USER_MASK);
    let clear = Signals(args.a2 as u32).and(Signals::USER_MASK);
    handle.object().header().signals().update(set, clear);
    Ok(0)
}

/// `debug_log(ptr, len) -> status`
fn sys_debug_log(args: &Args, _frame: &TrapFrame) -> Result<i64, Error> {
    let process = current()?;
    let text = uaccess::read_str(args.a0, args.a1 as usize)?;
    crate::kprintln!("\x1b[35m[{}]\x1b[0m {}", process.name(), text.trim_end());
    Ok(text.len() as i64)
}
