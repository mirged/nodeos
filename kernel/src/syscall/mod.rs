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
use nodeos_abi::types::{HandleInfo, MapFlags as AbiMapFlags, MapWindow, ThreadParams};
use nodeos_abi::{Error, Syscall};

use crate::arch::x86_64::trap::TrapFrame;
use crate::ipc::message::{Message, MessageFlags};
use crate::ipc::{Channel, ChannelError};
use crate::mm::layout::Window;
use crate::mm::space::{AddressSpace, MapFlags};
use crate::mm::uaccess;
use crate::mm::vmo::Vmo;
use crate::obj::{CapClass, Handle, HandleValue, KernelObject, ObjectError, ObjectType, Rights, Signals};
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
        Syscall::ProcessExit => sys_process_exit(args),
        Syscall::ThreadCreate => sys_thread_create(args),
        Syscall::ThreadExit => sys_thread_exit(args),
        Syscall::ThreadStart => sys_thread_start(args),
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
        let moved = parent.handles().remove(bootstrap).map_err(map_object_error)?;
        child.install(moved).map_err(map_object_error)?
    };

    let thread = Thread::new(
        &child,
        "main",
        ThreadEntry::User { entry: args.a1, stack: args.a2, arg0: bootstrap_value.0 as u64, arg1: 0 },
        SchedParams::normal(),
    )
    .ok_or(Error::NoMemory)?;
    sched::enqueue(&thread);
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
    sched::enqueue(&thread);
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
    uaccess::write_array(args.a0, &[first.0, second.0])?;
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
        let handle = process.handles().remove(HandleValue(value)).map_err(map_object_error)?;
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
    message: Message,
    descriptor_addr: u64,
    mut descriptor: MessageDescriptor,
) -> Result<i64, Error> {
    if message.payload.len() > descriptor.payload_len as usize
        || message.handles.len() > descriptor.handle_count as usize
    {
        // Report what would have been needed rather than truncating: silently
        // losing a capability is not an acceptable failure mode.
        descriptor.payload_len = message.payload.len() as u32;
        descriptor.handle_count = message.handles.len() as u32;
        uaccess::write_struct(descriptor_addr, &descriptor)?;
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

    if !message.payload.is_empty() {
        uaccess::write_bytes(descriptor.payload, &message.payload)?;
    }
    if !installed.is_empty() {
        uaccess::write_array(descriptor.handles, &installed)?;
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
    deliver_message(&process, message, args.a1, descriptor)
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
    deliver_message(&process, reply, args.a2, reply_descriptor)
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
            if deadline == 0 {
                sched::block_current();
            } else {
                // With a deadline and no timer objects yet, spin at scheduler
                // granularity rather than risk sleeping past it.
                sched::yield_now();
            }
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
