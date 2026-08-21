//! Handle rights.
//!
//! # Rights versus capabilities
//!
//! NodeOS distinguishes two things that Unix conflates into "permissions":
//!
//! * **Rights** (this module) are per-*handle* and enforced by the kernel on
//!   every operation. A handle to a channel with only [`Rights::READ`] cannot
//!   send on it, no matter who holds it. Rights can only ever be *reduced*
//!   when a handle is duplicated or transferred -- there is no operation that
//!   widens them.
//! * **Capability classes** (see [`super::capclass`]) are names for kinds of
//!   authority -- `FILE_READ`, `NETWORK`, `GPU` -- that an executable declares
//!   it needs and a launcher decides whether to satisfy. They are *policy*.
//!   Granting one means handing the process a handle (usually a channel to a
//!   service), at which point the kernel's rights checks take over.
//!
//! There is deliberately no right that means "everything", and no principal
//! that implicitly holds every right. The closest thing to `root` in NodeOS is
//! the first userspace process, and it is powerful only because the kernel
//! handed it specific handles at boot -- handles it can, and does, give away
//! in reduced form and then drop.

/// A set of rights on a single handle.
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct Rights(pub u32);

impl Rights {
    /// No authority at all. A handle with no rights is still a *name* for an
    /// object -- useful for identity comparison and nothing else.
    pub const NONE: Self = Self(0);

    /// May be duplicated (with the same or fewer rights).
    pub const DUPLICATE: Self = Self(1 << 0);
    /// May be sent to another process over a channel.
    pub const TRANSFER: Self = Self(1 << 1);
    /// May read data out of the object.
    pub const READ: Self = Self(1 << 2);
    /// May write data into the object.
    pub const WRITE: Self = Self(1 << 3);
    /// May be mapped executable.
    pub const EXECUTE: Self = Self(1 << 4);
    /// May be mapped into an address space at all.
    pub const MAP: Self = Self(1 << 5);
    /// May read object properties (name, size, state).
    pub const GET_PROPERTY: Self = Self(1 << 6);
    /// May change object properties.
    pub const SET_PROPERTY: Self = Self(1 << 7);
    /// May raise user signal bits on the object.
    pub const SIGNAL: Self = Self(1 << 8);
    /// May be waited on.
    pub const WAIT: Self = Self(1 << 9);
    /// May destroy or kill the object.
    pub const DESTROY: Self = Self(1 << 10);
    /// May create threads or otherwise manage a process.
    pub const MANAGE_PROCESS: Self = Self(1 << 11);
    /// May suspend, resume or inspect a thread.
    pub const MANAGE_THREAD: Self = Self(1 << 12);
    /// May perform port I/O or map MMIO for a device.
    pub const DEVICE_IO: Self = Self(1 << 13);
    /// May bind an interrupt line.
    pub const IRQ_BIND: Self = Self(1 << 14);
    /// May resize a memory object.
    pub const RESIZE: Self = Self(1 << 15);
    /// May create a child object derived from this one (a channel endpoint
    /// pair from a namespace, a sub-VMO from a VMO).
    pub const DERIVE: Self = Self(1 << 16);
    /// May revoke grants derived from this handle.
    pub const REVOKE: Self = Self(1 << 17);

    /// Rights appropriate for a freshly created channel endpoint.
    pub const CHANNEL_DEFAULT: Self = Self(
        Self::DUPLICATE.0
            | Self::TRANSFER.0
            | Self::READ.0
            | Self::WRITE.0
            | Self::WAIT.0
            | Self::SIGNAL.0
            | Self::GET_PROPERTY.0
            | Self::DESTROY.0
            | Self::REVOKE.0,
    );

    /// Rights appropriate for a freshly created memory object.
    pub const VMO_DEFAULT: Self = Self(
        Self::DUPLICATE.0
            | Self::TRANSFER.0
            | Self::READ.0
            | Self::WRITE.0
            | Self::MAP.0
            | Self::RESIZE.0
            | Self::WAIT.0
            | Self::GET_PROPERTY.0
            | Self::SET_PROPERTY.0
            | Self::DERIVE.0
            | Self::DESTROY.0
            | Self::REVOKE.0,
    );

    /// Rights appropriate for a freshly created process.
    pub const PROCESS_DEFAULT: Self = Self(
        Self::DUPLICATE.0
            | Self::TRANSFER.0
            | Self::WAIT.0
            | Self::GET_PROPERTY.0
            | Self::SET_PROPERTY.0
            | Self::MANAGE_PROCESS.0
            | Self::DESTROY.0
            | Self::REVOKE.0,
    );

    /// Rights appropriate for a freshly created thread.
    pub const THREAD_DEFAULT: Self = Self(
        Self::DUPLICATE.0
            | Self::TRANSFER.0
            | Self::WAIT.0
            | Self::GET_PROPERTY.0
            | Self::MANAGE_THREAD.0
            | Self::DESTROY.0,
    );

    /// Read-only view of an object.
    pub const READ_ONLY: Self =
        Self(Self::READ.0 | Self::WAIT.0 | Self::GET_PROPERTY.0 | Self::MAP.0 | Self::DUPLICATE.0 | Self::TRANSFER.0);

    /// Union.
    pub const fn or(self, other: Self) -> Self {
        Self(self.0 | other.0)
    }

    /// Intersection. This is the only operation used when deriving a handle,
    /// which is what makes rights monotonically non-increasing.
    pub const fn and(self, other: Self) -> Self {
        Self(self.0 & other.0)
    }

    /// Remove `other`'s bits.
    pub const fn without(self, other: Self) -> Self {
        Self(self.0 & !other.0)
    }

    /// Whether every bit of `other` is present.
    pub const fn contains(self, other: Self) -> bool {
        self.0 & other.0 == other.0
    }

    /// Whether `self` grants no more than `other`.
    pub const fn is_subset_of(self, other: Self) -> bool {
        self.0 & !other.0 == 0
    }

    /// Whether no rights at all are present.
    pub const fn is_empty(self) -> bool {
        self.0 == 0
    }
}

impl core::ops::BitOr for Rights {
    type Output = Self;
    fn bitor(self, rhs: Self) -> Self {
        self.or(rhs)
    }
}

impl core::ops::BitAnd for Rights {
    type Output = Self;
    fn bitand(self, rhs: Self) -> Self {
        self.and(rhs)
    }
}

impl core::fmt::Debug for Rights {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        const NAMES: [(u32, &str); 18] = [
            (1 << 0, "DUP"),
            (1 << 1, "XFER"),
            (1 << 2, "READ"),
            (1 << 3, "WRITE"),
            (1 << 4, "EXEC"),
            (1 << 5, "MAP"),
            (1 << 6, "GETPROP"),
            (1 << 7, "SETPROP"),
            (1 << 8, "SIGNAL"),
            (1 << 9, "WAIT"),
            (1 << 10, "DESTROY"),
            (1 << 11, "MGMTPROC"),
            (1 << 12, "MGMTTHRD"),
            (1 << 13, "DEVIO"),
            (1 << 14, "IRQ"),
            (1 << 15, "RESIZE"),
            (1 << 16, "DERIVE"),
            (1 << 17, "REVOKE"),
        ];
        if self.0 == 0 {
            return f.write_str("none");
        }
        let mut first = true;
        for (bit, name) in NAMES {
            if self.0 & bit != 0 {
                if !first {
                    f.write_str("|")?;
                }
                f.write_str(name)?;
                first = false;
            }
        }
        Ok(())
    }
}
