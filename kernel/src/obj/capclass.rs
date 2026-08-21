//! Capability classes: the vocabulary an executable uses to say what authority
//! it needs, and a launcher uses to decide what to grant.
//!
//! These are *not* enforced by the kernel. The kernel enforces
//! [`super::rights::Rights`] on handles. A capability class is the policy-level
//! name for a bundle of handles: granting `NETWORK` means "install a handle to
//! the network service's channel in this process's namespace", and it is that
//! handle -- not the class -- that the kernel checks later.
//!
//! Keeping the two separate is what stops NodeOS from re-inventing `root`. A
//! launcher that grants every class still cannot grant more authority than it
//! itself holds, because all it can do is hand over handles it has.

/// A named class of authority declared by an NXF image.
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Debug)]
#[repr(u32)]
pub enum CapClass {
    /// Read from filesystem objects reachable in the process namespace.
    FileRead = 1,
    /// Write to filesystem objects reachable in the process namespace.
    FileWrite = 2,
    /// Create and destroy filesystem objects.
    FileCreate = 3,
    /// Open network endpoints.
    Network = 4,
    /// Bind privileged (low) network ports.
    NetworkBind = 5,
    /// Submit work to the graphics device.
    Gpu = 6,
    /// Play or capture audio.
    Audio = 7,
    /// Allocate memory beyond the image's declared working set.
    Memory = 8,
    /// Create processes.
    Process = 9,
    /// Create channels not derived from an existing one.
    Channel = 10,
    /// Claim a hardware device (MMIO, ports, interrupt line).
    Device = 11,
    /// Read the system clock at high resolution.
    Clock = 12,
    /// Observe or control other processes (debuggers, supervisors).
    Debug = 13,
    /// Load further executables into this process.
    Load = 14,
    /// Receive input events.
    Input = 15,
    /// Present to the display.
    Display = 16,
}

impl CapClass {
    /// Parse the wire encoding used in NXF headers.
    pub const fn from_u32(value: u32) -> Option<Self> {
        Some(match value {
            1 => Self::FileRead,
            2 => Self::FileWrite,
            3 => Self::FileCreate,
            4 => Self::Network,
            5 => Self::NetworkBind,
            6 => Self::Gpu,
            7 => Self::Audio,
            8 => Self::Memory,
            9 => Self::Process,
            10 => Self::Channel,
            11 => Self::Device,
            12 => Self::Clock,
            13 => Self::Debug,
            14 => Self::Load,
            15 => Self::Input,
            16 => Self::Display,
            _ => return None,
        })
    }

    /// Stable human-readable name, as it appears in NXF manifests.
    pub const fn name(self) -> &'static str {
        match self {
            Self::FileRead => "FILE_READ",
            Self::FileWrite => "FILE_WRITE",
            Self::FileCreate => "FILE_CREATE",
            Self::Network => "NETWORK",
            Self::NetworkBind => "NETWORK_BIND",
            Self::Gpu => "GPU",
            Self::Audio => "AUDIO",
            Self::Memory => "MEMORY",
            Self::Process => "PROCESS",
            Self::Channel => "CHANNEL",
            Self::Device => "DEVICE",
            Self::Clock => "CLOCK",
            Self::Debug => "DEBUG",
            Self::Load => "LOAD",
            Self::Input => "INPUT",
            Self::Display => "DISPLAY",
        }
    }

    /// Parse a manifest name.
    pub fn from_name(name: &str) -> Option<Self> {
        (1..=16).filter_map(Self::from_u32).find(|c| c.name() == name)
    }
}

/// A set of capability classes, as a bitmask over [`CapClass`] discriminants.
#[derive(Clone, Copy, PartialEq, Eq, Default)]
pub struct CapSet(pub u32);

impl CapSet {
    /// The empty set: a process that can do nothing but compute and exit.
    pub const NONE: Self = Self(0);

    /// Add a class.
    pub const fn with(self, class: CapClass) -> Self {
        Self(self.0 | (1 << (class as u32)))
    }

    /// Whether the set contains `class`.
    pub const fn contains(self, class: CapClass) -> bool {
        self.0 & (1 << (class as u32)) != 0
    }

    /// Intersection -- the only way a launcher narrows a request.
    pub const fn and(self, other: Self) -> Self {
        Self(self.0 & other.0)
    }

    /// Union.
    pub const fn or(self, other: Self) -> Self {
        Self(self.0 | other.0)
    }

    /// Classes in `self` that are absent from `other`.
    pub const fn difference(self, other: Self) -> Self {
        Self(self.0 & !other.0)
    }

    /// Whether the set is empty.
    pub const fn is_empty(self) -> bool {
        self.0 == 0
    }

    /// Iterate the classes present.
    pub fn iter(self) -> impl Iterator<Item = CapClass> {
        (1u32..=16).filter_map(CapClass::from_u32).filter(move |c| self.contains(*c))
    }
}

impl core::fmt::Debug for CapSet {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        if self.is_empty() {
            return f.write_str("{}");
        }
        f.write_str("{")?;
        let mut first = true;
        for class in self.iter() {
            if !first {
                f.write_str(", ")?;
            }
            f.write_str(class.name())?;
            first = false;
        }
        f.write_str("}")
    }
}
