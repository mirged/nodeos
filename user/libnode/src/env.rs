//! What a process was given at startup.

use alloc::vec::Vec;

use nodeos_abi::message::BootstrapMessage;
use nodeos_abi::{CapClass, HandleValue};

use crate::channel::Channel;
use crate::Handle;

/// The capabilities and facts a process starts with.
///
/// Everything here came from the bootstrap message. There is no way to obtain
/// more except by asking something that already trusts you, which is why
/// auditing a NodeOS program's authority means reading one message rather than
/// reasoning about a whole kernel's worth of implicit permissions.
pub struct Env {
    bootstrap: Option<Channel>,
    info: BootstrapMessage,
    extras: Vec<Handle>,
    args: Vec<alloc::string::String>,
}

impl Env {
    /// An environment with nothing in it.
    pub fn empty() -> Self {
        Self { bootstrap: None, info: BootstrapMessage::default(), extras: Vec::new(), args: Vec::new() }
    }

    /// An environment with only a bootstrap channel.
    pub fn with_channel(channel: Channel) -> Self {
        Self {
            bootstrap: Some(channel),
            info: BootstrapMessage::default(),
            extras: Vec::new(),
            args: Vec::new(),
        }
    }

    /// Build from a received bootstrap message.
    pub fn from_bootstrap(
        info: BootstrapMessage,
        channel: Channel,
        extras: Vec<Handle>,
        args: Vec<alloc::string::String>,
    ) -> Self {
        Self { bootstrap: Some(channel), info, extras, args }
    }

    /// Resize the heap to whatever the launcher asked for, now that the
    /// bootstrap message has been read.
    pub(crate) fn finish_startup(&mut self) {
        if self.info.heap_size > crate::alloc_impl::heap_size() as u64 {
            crate::alloc_impl::init(self.info.heap_size);
        }
    }

    /// The bootstrap channel, if the process has one.
    pub fn bootstrap(&self) -> Option<&Channel> {
        self.bootstrap.as_ref()
    }

    /// Take the bootstrap channel.
    pub fn take_bootstrap(&mut self) -> Option<Channel> {
        self.bootstrap.take()
    }

    /// Handle to this process, if one was granted.
    pub fn process_handle(&self) -> HandleValue {
        self.info.process
    }

    /// Handle to this process's address space, if one was granted.
    pub fn space_handle(&self) -> HandleValue {
        self.info.space
    }

    /// Base address of the loaded image.
    pub fn image_base(&self) -> u64 {
        self.info.image_base
    }

    /// Whether the launcher granted `class`.
    pub fn has_capability(&self, class: CapClass) -> bool {
        self.info.granted_caps & (1 << (class as u32)) != 0
    }

    /// The raw granted-capability mask.
    pub fn granted_mask(&self) -> u32 {
        self.info.granted_caps
    }

    /// Extra capabilities that arrived with the bootstrap message, in the order
    /// the launcher sent them.
    pub fn extras(&self) -> &[Handle] {
        &self.extras
    }

    /// Take an extra capability by index.
    pub fn take_extra(&mut self, index: usize) -> Option<Handle> {
        if index < self.extras.len() {
            Some(self.extras.remove(index))
        } else {
            None
        }
    }

    /// The command line, `argv`-style: element 0 is the program path the shell
    /// resolved, the rest are the words that followed `run`. Empty when the
    /// program was started without one.
    pub fn args(&self) -> &[alloc::string::String] {
        &self.args
    }
}
