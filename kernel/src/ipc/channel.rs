//! Channels: bidirectional, typed, capability-carrying message pipes.
//!
//! A channel is created as a *pair* of endpoints. Each endpoint is a separate
//! kernel object with its own handle, so the two halves can end up in different
//! processes with different rights -- one side read-only, the other write-only,
//! if that is what the protocol wants.
//!
//! Closing one endpoint asserts [`Signals::PEER_CLOSED`] on the other. That is
//! the mechanism behind every supervision tree in NodeOS: a service that dies
//! does not need to notify anyone, because the kernel closing its handles is
//! itself the notification.

use alloc::collections::VecDeque;
use alloc::sync::{Arc, Weak};
use alloc::vec::Vec;
use core::sync::atomic::{AtomicU64, Ordering};

use crate::obj::{Handle, KernelObject, ObjectHeader, ObjectType, Rights, Signals};
use crate::sync::IrqSpinLock;

use super::message::{Message, MessageError, MessageFlags, MAX_QUEUE_BYTES, MAX_QUEUE_DEPTH};

/// Errors from channel operations.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum ChannelError {
    /// The far endpoint is gone.
    PeerClosed,
    /// No message is queued (non-blocking receive).
    Empty,
    /// The peer's queue is full (non-blocking send).
    Full,
    /// The message is malformed.
    Message(MessageError),
    /// The operation was interrupted, or the deadline passed.
    TimedOut,
    /// This endpoint has been closed.
    Closed,
}

impl From<MessageError> for ChannelError {
    fn from(error: MessageError) -> Self {
        Self::Message(error)
    }
}

struct Inbox {
    queue: VecDeque<Message>,
    bytes: usize,
    closed: bool,
}

static NEXT_TXID: AtomicU64 = AtomicU64::new(1);

/// Allocate a transaction id for a request.
pub fn next_txid() -> u64 {
    NEXT_TXID.fetch_add(1, Ordering::Relaxed)
}

/// One end of a channel.
pub struct Channel {
    header: ObjectHeader,
    inbox: IrqSpinLock<Inbox>,
    peer: IrqSpinLock<Weak<Channel>>,
    /// Threads blocked waiting for room in *this* endpoint's inbox.
    space_available: crate::obj::WaitQueue,
}

impl Channel {
    /// Create a connected pair of endpoints.
    pub fn create_pair() -> (Arc<Self>, Arc<Self>) {
        let make = || {
            Arc::new(Self {
                // A fresh endpoint is writable (its peer's queue is empty) and
                // not readable (nothing has been sent to it yet).
                header: ObjectHeader::new(ObjectType::Channel, Signals::WRITABLE),
                inbox: IrqSpinLock::new(Inbox { queue: VecDeque::new(), bytes: 0, closed: false }),
                peer: IrqSpinLock::new(Weak::new()),
                space_available: crate::obj::WaitQueue::new(),
            })
        };
        let a = make();
        let b = make();
        *a.peer.lock() = Arc::downgrade(&b);
        *b.peer.lock() = Arc::downgrade(&a);
        (a, b)
    }

    /// The far endpoint, if it is still alive.
    pub fn peer(&self) -> Option<Arc<Channel>> {
        self.peer.lock().upgrade()
    }

    /// Whether the far endpoint is gone.
    pub fn is_peer_closed(&self) -> bool {
        self.peer().is_none() || self.peer().is_some_and(|p| p.inbox.lock().closed)
    }

    /// Number of queued messages on this endpoint.
    pub fn queued(&self) -> usize {
        self.inbox.lock().queue.len()
    }

    /// Bytes queued on this endpoint.
    pub fn queued_bytes(&self) -> usize {
        self.inbox.lock().bytes
    }

    /// Deliver `message` to the peer's inbox.
    ///
    /// The message is *moved*: on success the sender no longer owns the
    /// capabilities it carried.
    pub fn send(&self, message: Message) -> Result<(), ChannelError> {
        message.validate()?;
        let peer = self.peer().ok_or(ChannelError::PeerClosed)?;

        let non_blocking = message.flags.contains(MessageFlags::NON_BLOCKING);
        let cost = message.cost();

        {
            let mut inbox = peer.inbox.lock();
            if inbox.closed {
                return Err(ChannelError::PeerClosed);
            }
            if inbox.queue.len() >= MAX_QUEUE_DEPTH || inbox.bytes + cost > MAX_QUEUE_BYTES {
                if non_blocking {
                    return Err(ChannelError::Full);
                }
                // Backpressure. Park on the peer's space queue and retry; the
                // caller re-enters `send` after being woken.
                drop(inbox);
                if let Some(thread) = crate::sched::current_thread() {
                    peer.space_available.park(&thread);
                    crate::sched::block_current();
                    peer.space_available.unpark(&thread);
                    return self.send(message);
                }
                return Err(ChannelError::Full);
            }
            inbox.bytes += cost;
            inbox.queue.push_back(message);
            let full = inbox.queue.len() >= MAX_QUEUE_DEPTH || inbox.bytes >= MAX_QUEUE_BYTES;
            drop(inbox);

            peer.header.signals().update(Signals::READABLE, Signals::NONE);
            // `update` only wakes on a *transition*, and a message queued
            // behind another one does not change the signal word: the peer was
            // already readable. That is enough for a receiver waiting for
            // "anything", and not enough for one in `call`, which is waiting
            // for a particular transaction id and has to re-run its search
            // every time the queue grows. Waking here unconditionally is the
            // difference between a display client that blocks for one frame
            // and one that blocks forever, because a compositor event
            // arriving before the reply left the reply with no wakeup of its
            // own to ride in on.
            peer.header.signals().queue().wake_all();
            if full {
                // The peer's inbox is full, so *we* are no longer writable.
                self.header.signals().update(Signals::NONE, Signals::WRITABLE);
            }
        }
        Ok(())
    }

    /// Take the next message from this endpoint's inbox.
    pub fn receive(&self) -> Result<Message, ChannelError> {
        let mut inbox = self.inbox.lock();
        if inbox.closed {
            return Err(ChannelError::Closed);
        }
        match inbox.queue.pop_front() {
            Some(message) => {
                inbox.bytes -= message.cost();
                let empty = inbox.queue.is_empty();
                drop(inbox);
                if empty {
                    self.header.signals().update(Signals::NONE, Signals::READABLE);
                }
                // Room freed here makes the *peer* writable again.
                if let Some(peer) = self.peer() {
                    peer.header.signals().update(Signals::WRITABLE, Signals::NONE);
                }
                self.space_available.wake_all();
                Ok(message)
            }
            None => {
                drop(inbox);
                if self.is_peer_closed() {
                    Err(ChannelError::PeerClosed)
                } else {
                    Err(ChannelError::Empty)
                }
            }
        }
    }

    /// Take the next message, blocking until one arrives or the peer closes.
    pub fn receive_blocking(&self) -> Result<Message, ChannelError> {
        loop {
            match self.receive() {
                Err(ChannelError::Empty) => {}
                other => return other,
            }
            let Some(thread) = crate::sched::current_thread() else {
                return Err(ChannelError::Empty);
            };
            // Park *before* re-checking, so a message that arrives in the gap
            // wakes us instead of being missed.
            self.header.signals().queue().park(&thread);
            if self.inbox.lock().queue.is_empty() && !self.is_peer_closed() {
                crate::sched::block_current();
            }
            self.header.signals().queue().unpark(&thread);
        }
    }

    /// Whether a reply with `txid` is already queued.
    fn has_reply(&self, txid: u64) -> bool {
        self.inbox
            .lock()
            .queue
            .iter()
            .any(|m| m.txid == txid && m.flags.contains(MessageFlags::REPLY))
    }

    /// Take the first queued message whose `txid` matches, leaving others in
    /// place. Used to correlate a reply with its request.
    ///
    /// Mirrors [`Channel::receive`]'s signal bookkeeping: `READABLE` must drop
    /// when this empties the inbox, not just when a plain `receive` does, or a
    /// second `call` on the same channel can never be woken. A reply removed
    /// from the middle of the queue leaves other queued traffic's readability
    /// untouched.
    pub fn receive_reply(&self, txid: u64) -> Result<Message, ChannelError> {
        let mut inbox = self.inbox.lock();
        if let Some(position) = inbox.queue.iter().position(|m| m.txid == txid && m.flags.contains(MessageFlags::REPLY))
        {
            let message = inbox.queue.remove(position).expect("position from iter");
            inbox.bytes -= message.cost();
            let empty = inbox.queue.is_empty();
            drop(inbox);
            if empty {
                self.header.signals().update(Signals::NONE, Signals::READABLE);
            }
            // Room freed here makes the *peer* writable again, as in `receive`.
            if let Some(peer) = self.peer() {
                peer.header.signals().update(Signals::WRITABLE, Signals::NONE);
            }
            self.space_available.wake_all();
            return Ok(message);
        }
        drop(inbox);
        if self.is_peer_closed() {
            Err(ChannelError::PeerClosed)
        } else {
            Err(ChannelError::Empty)
        }
    }

    /// Send a request and block until the matching reply arrives.
    ///
    /// This is the shape almost every NodeOS service call takes, and having it
    /// in the kernel means one blocking point and one wakeup instead of two.
    pub fn call(&self, mut request: Message) -> Result<Message, ChannelError> {
        let txid = if request.txid == 0 { next_txid() } else { request.txid };
        request.txid = txid;
        request.flags = request.flags.or(MessageFlags::REQUEST);
        self.send(request)?;

        loop {
            match self.receive_reply(txid) {
                Err(ChannelError::Empty) => {}
                other => return other,
            }
            let Some(thread) = crate::sched::current_thread() else {
                return Err(ChannelError::Empty);
            };
            self.header.signals().queue().park(&thread);
            // Re-check under the parked state so a reply that arrived while we
            // were registering does not leave us asleep forever.
            if !self.has_reply(txid) && !self.is_peer_closed() {
                crate::sched::block_current();
            }
            self.header.signals().queue().unpark(&thread);
        }
    }

    /// Close this endpoint, waking anyone blocked on either side.
    pub fn close(&self) {
        let dropped: Vec<Message> = {
            let mut inbox = self.inbox.lock();
            if inbox.closed {
                return;
            }
            inbox.closed = true;
            inbox.bytes = 0;
            inbox.queue.drain(..).collect()
        };
        // Undelivered capabilities are released here, which may in turn close
        // further channels -- that cascade is exactly how a dying subsystem
        // tears its subtree down.
        drop(dropped);

        self.header.signals().update(Signals::PEER_CLOSED, Signals::READABLE.or(Signals::WRITABLE));
        self.space_available.wake_all();

        if let Some(peer) = self.peer() {
            peer.header.signals().update(Signals::PEER_CLOSED, Signals::WRITABLE);
            peer.space_available.wake_all();
        }
    }

    /// Rights a freshly created endpoint handle should carry.
    pub fn default_rights() -> Rights {
        Rights::CHANNEL_DEFAULT
    }
}

impl KernelObject for Channel {
    fn header(&self) -> &ObjectHeader {
        &self.header
    }

    fn on_zero_handles(&self) {
        self.close();
    }

    fn as_any(&self) -> &dyn core::any::Any {
        self
    }
}

impl Drop for Channel {
    fn drop(&mut self) {
        if let Some(peer) = self.peer.get_mut().upgrade() {
            peer.header.signals().update(Signals::PEER_CLOSED, Signals::WRITABLE);
            peer.space_available.wake_all();
        }
    }
}

/// Create a connected pair and return handles to both ends.
pub fn create_pair_handles() -> (Handle, Handle) {
    let (a, b) = Channel::create_pair();
    (Handle::root(a, Channel::default_rights()), Handle::root(b, Channel::default_rights()))
}
