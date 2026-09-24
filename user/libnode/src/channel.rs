//! Typed channels.

use alloc::vec::Vec;

use nodeos_abi::message::{MessageDescriptor, MessageFlags};
use nodeos_abi::{Error, HandleValue, Syscall};

use crate::{sys, Handle};

/// A message as userspace builds and receives it.
#[derive(Default)]
pub struct Message {
    /// Protocol-defined message type.
    pub kind: u32,
    /// Delivery options.
    pub flags: MessageFlags,
    /// Request/reply correlation identifier.
    pub txid: u64,
    /// Payload bytes.
    pub payload: Vec<u8>,
    /// Capabilities carried by the message.
    pub handles: Vec<Handle>,
}

impl Message {
    /// A message of `kind` with no payload.
    pub fn new(kind: u32) -> Self {
        Self { kind, ..Default::default() }
    }

    /// Attach a payload.
    pub fn with_payload(mut self, payload: &[u8]) -> Self {
        self.payload.extend_from_slice(payload);
        self
    }

    /// Attach a capability.
    pub fn with_handle(mut self, handle: Handle) -> Self {
        self.handles.push(handle);
        self
    }

    /// The payload interpreted as UTF-8, if it is.
    pub fn text(&self) -> Option<&str> {
        core::str::from_utf8(&self.payload).ok()
    }
}

/// Largest message this wrapper will receive without a caller-supplied buffer.
pub const DEFAULT_RECEIVE_CAPACITY: usize = 4096;
/// Largest number of capabilities received into the default buffer.
pub const DEFAULT_HANDLE_CAPACITY: usize = 8;

/// One end of a channel.
pub struct Channel {
    handle: Handle,
}

impl Channel {
    /// Create a connected pair.
    pub fn create() -> Result<(Channel, Channel), Error> {
        let mut out = [0u32; 2];
        // SAFETY: `out` is a valid writable buffer for two handle values.
        let raw = unsafe { sys::syscall1(Syscall::ChannelCreate, out.as_mut_ptr() as u64) };
        sys::ok(raw)?;
        Ok((
            Channel { handle: Handle::from_raw(HandleValue(out[0])) },
            Channel { handle: Handle::from_raw(HandleValue(out[1])) },
        ))
    }

    /// Adopt an existing endpoint handle.
    pub const fn from_handle(handle: Handle) -> Self {
        Self { handle }
    }

    /// The underlying capability.
    pub fn handle(&self) -> &Handle {
        &self.handle
    }

    /// Give up the endpoint without closing it -- for handing it to a child.
    pub fn into_handle(self) -> Handle {
        self.handle
    }

    /// Send a message, transferring its capabilities.
    pub fn send(&self, message: Message) -> Result<(), Error> {
        let values: Vec<u32> = message.handles.iter().map(|h| h.raw().0).collect();
        let descriptor = MessageDescriptor {
            kind: message.kind,
            flags: message.flags.0,
            txid: message.txid,
            payload: message.payload.as_ptr() as u64,
            payload_len: message.payload.len() as u32,
            handle_count: values.len() as u32,
            handles: values.as_ptr() as u64,
        };
        // The kernel takes ownership of the handles on success; forget ours so
        // `Drop` does not close what is now someone else's capability.
        let result = {
            // SAFETY: the descriptor's pointers refer to live local buffers of
            // the declared lengths.
            let raw = unsafe {
                sys::syscall2(Syscall::ChannelSend, self.handle.raw().0 as u64, &descriptor as *const _ as u64)
            };
            sys::ok(raw)
        };
        for handle in message.handles {
            let _ = handle.into_raw();
        }
        result
    }

    /// Receive a message, blocking until one arrives.
    pub fn receive(&self) -> Result<Message, Error> {
        self.receive_with(DEFAULT_RECEIVE_CAPACITY, DEFAULT_HANDLE_CAPACITY, false)
    }

    /// Receive without blocking.
    pub fn try_receive(&self) -> Result<Message, Error> {
        self.receive_with(DEFAULT_RECEIVE_CAPACITY, DEFAULT_HANDLE_CAPACITY, true)
    }

    /// Receive with explicit buffer sizes.
    pub fn receive_with(&self, payload_capacity: usize, handle_capacity: usize, non_blocking: bool) -> Result<Message, Error> {
        let mut payload = alloc::vec![0u8; payload_capacity];
        let mut handles = alloc::vec![0u32; handle_capacity];
        let mut descriptor = MessageDescriptor {
            kind: 0,
            flags: if non_blocking { MessageFlags::NON_BLOCKING.0 } else { 0 },
            txid: 0,
            payload: payload.as_mut_ptr() as u64,
            payload_len: payload_capacity as u32,
            handle_count: handle_capacity as u32,
            handles: handles.as_mut_ptr() as u64,
        };
        // SAFETY: the descriptor points at live local buffers of the declared
        // capacities, and the kernel writes back the actual lengths.
        let raw = unsafe {
            sys::syscall2(Syscall::ChannelReceive, self.handle.raw().0 as u64, &mut descriptor as *mut _ as u64)
        };
        sys::ok(raw)?;

        payload.truncate(descriptor.payload_len as usize);
        let received = handles[..descriptor.handle_count as usize]
            .iter()
            .map(|value| Handle::from_raw(HandleValue(*value)))
            .collect();
        Ok(Message {
            kind: descriptor.kind,
            flags: MessageFlags(descriptor.flags),
            txid: descriptor.txid,
            payload,
            handles: received,
        })
    }

    /// Send a request and block for the matching reply.
    pub fn call(&self, request: Message) -> Result<Message, Error> {
        self.call_with(request, DEFAULT_RECEIVE_CAPACITY, DEFAULT_HANDLE_CAPACITY)
    }

    /// As [`Channel::call`], with an explicit reply buffer size.
    ///
    /// Worth reaching for whenever the reply is known to be small and the call
    /// is made often. The runtime allocator never reclaims a block larger than
    /// 2 KiB (see `alloc_impl`), so a caller that takes the 4 KiB default for
    /// an *empty* acknowledgement leaks a page per call -- which is fine at
    /// startup and fatal in a frame loop. `WM_COMMIT` is the case that found
    /// this: Doom sends one per frame and ran its heap out in seconds.
    pub fn call_with(
        &self,
        request: Message,
        reply_capacity: usize,
        reply_handle_capacity: usize,
    ) -> Result<Message, Error> {
        let values: Vec<u32> = request.handles.iter().map(|h| h.raw().0).collect();
        let request_descriptor = MessageDescriptor {
            kind: request.kind,
            flags: request.flags.or(MessageFlags::REQUEST).0,
            txid: request.txid,
            payload: request.payload.as_ptr() as u64,
            payload_len: request.payload.len() as u32,
            handle_count: values.len() as u32,
            handles: values.as_ptr() as u64,
        };

        let mut reply_payload = alloc::vec![0u8; reply_capacity];
        let mut reply_handles = alloc::vec![0u32; reply_handle_capacity];
        let mut reply_descriptor = MessageDescriptor {
            kind: 0,
            flags: 0,
            txid: 0,
            payload: reply_payload.as_mut_ptr() as u64,
            payload_len: reply_capacity as u32,
            handle_count: reply_handle_capacity as u32,
            handles: reply_handles.as_mut_ptr() as u64,
        };

        // SAFETY: both descriptors point at live local buffers of the declared
        // sizes.
        let raw = unsafe {
            sys::syscall3(
                Syscall::ChannelCall,
                self.handle.raw().0 as u64,
                &request_descriptor as *const _ as u64,
                &mut reply_descriptor as *mut _ as u64,
            )
        };
        for handle in request.handles {
            let _ = handle.into_raw();
        }
        sys::ok(raw)?;

        reply_payload.truncate(reply_descriptor.payload_len as usize);
        let received = reply_handles[..reply_descriptor.handle_count as usize]
            .iter()
            .map(|value| Handle::from_raw(HandleValue(*value)))
            .collect();
        Ok(Message {
            kind: reply_descriptor.kind,
            flags: MessageFlags(reply_descriptor.flags),
            txid: reply_descriptor.txid,
            payload: reply_payload,
            handles: received,
        })
    }

    /// Reply to a request, reusing its transaction id.
    pub fn reply(&self, request: &Message, mut reply: Message) -> Result<(), Error> {
        reply.txid = request.txid;
        reply.flags = reply.flags.or(MessageFlags::REPLY);
        self.send(reply)
    }
}
