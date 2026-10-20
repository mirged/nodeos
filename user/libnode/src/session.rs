//! Launch disk applications with session channels and a private display connection.

use crate::abi::message::{BootstrapMessage, MessageFlags, KIND_BOOTSTRAP};
use crate::abi::proto;
use crate::abi::{CapClass, Rights};
use crate::mem::Vmo;
use crate::process::Process;
use crate::{Channel, Error, Message};
use alloc::vec::Vec;

fn call(channel: &Channel, request: Message, capacity: usize) -> Result<Message, Error> {
    let reply = channel.call_with(request, capacity, 0)?;
    if reply.flags.contains(MessageFlags::ERROR) {
        return Err(proto::decode_error(&reply.payload).unwrap_or(Error::BadState));
    }
    Ok(reply)
}

/// Start an NXF from `path`, passing arguments and the caller's session.
/// Each child gets a private display connection so its input cannot be
/// consumed by another application. Files and audio retain the session policy.
pub fn launch(
    console: &Channel,
    files: &Channel,
    wm: Option<&Channel>,
    audio: Option<&Channel>,
    path: &str,
    args: &[&str],
) -> Result<Process, Error> {
    let mut bootstrap_size = core::mem::size_of::<BootstrapMessage>() + path.len() + 1;
    for arg in args {
        bootstrap_size = bootstrap_size
            .checked_add(arg.len() + 1)
            .ok_or(Error::LimitExceeded)?;
    }
    if bootstrap_size > crate::channel::DEFAULT_RECEIVE_CAPACITY {
        return Err(Error::LimitExceeded);
    }
    let reply = call(
        files,
        Message::new(proto::FS_OPEN).with_payload(path.as_bytes()),
        128,
    )?;
    let node = proto::decode::<proto::FsOpenReply>(&reply.payload).ok_or(Error::BadState)?;
    let staged = (|| {
        if node.kind != proto::fs_kind::FILE || node.size < 4 {
            return Err(Error::InvalidArgument);
        }
        let image = Vmo::create(node.size, false)?;
        let mut offset = 0;
        while offset < node.size {
            let request = proto::Encoded::new(proto::FsReadRequest {
                node: node.node,
                offset,
                length: 32768,
                _reserved: 0,
            });
            let reply = call(
                files,
                Message::new(proto::FS_READ).with_payload(request.bytes()),
                32768,
            )?;
            if reply.payload.is_empty() || reply.payload.len() as u64 > node.size - offset {
                return Err(Error::BadState);
            }
            if offset == 0 && reply.payload.get(..4) != Some(b"NXF\0") {
                return Err(Error::InvalidArgument);
            }
            image.write(offset, &reply.payload)?;
            offset += reply.payload.len() as u64;
        }
        Ok(image)
    })();
    let close = proto::Encoded::new(proto::FsCloseRequest { node: node.node });
    let _ = call(
        files,
        Message::new(proto::FS_CLOSE).with_payload(close.bytes()),
        64,
    );
    let image = staged?;
    let process = Process::create(path, u32::MAX)?;
    let loaded = process.load(&image, node.size)?;
    let display = if let Some(wm) = wm {
        let (client, server) = Channel::create()?;
        call(
            wm,
            Message::new(proto::CONNECT).with_handle(server.into_handle()),
            64,
        )?;
        Some(client)
    } else {
        None
    };
    let sources = [Some(console), Some(files), display.as_ref(), audio];
    let granted_caps = [
        CapClass::Process,
        CapClass::Load,
        CapClass::Channel,
        CapClass::Memory,
        CapClass::Clock,
        CapClass::FileRead,
        CapClass::FileWrite,
        CapClass::FileCreate,
        CapClass::Audio,
    ]
    .into_iter()
    .fold(0u32, |mask, cap| mask | (1 << cap as u32));
    let info = proto::Encoded::new(BootstrapMessage {
        version: 1,
        granted_caps,
        process: crate::HandleValue::INVALID,
        space: crate::HandleValue::INVALID,
        thread: crate::HandleValue::INVALID,
        namespace: crate::HandleValue::INVALID,
        image_base: 0,
        heap_size: 16 * 1024 * 1024,
        extra_count: sources.iter().flatten().count() as u32,
        _reserved: 0,
    });
    let mut payload = Vec::from(info.bytes());
    for arg in core::iter::once(path).chain(args.iter().copied()) {
        payload.extend_from_slice(arg.as_bytes());
        payload.push(0);
    }
    let mut message = Message::new(KIND_BOOTSTRAP).with_payload(&payload);
    for source in sources.into_iter().flatten() {
        message = message.with_handle(source.handle().duplicate(Rights::DEFAULT)?);
    }
    let (parent, child) = Channel::create()?;
    parent.send(message)?;
    process.start(loaded.entry, loaded.stack, child.into_handle())?;
    Ok(process)
}
