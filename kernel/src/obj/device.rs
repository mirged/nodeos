//! Claimed hardware devices.
//!
//! A `Device` is the kernel's answer to "how does a userspace driver touch
//! hardware without the kernel implementing the driver". It bundles exactly
//! the authority a driver process needs for one piece of hardware: an MMIO
//! range it can turn into a mappable [`Vmo`], or a byte stream fed by an
//! interrupt handler. The kernel creates the fixed set of `Device` objects at
//! boot (there is no dynamic device discovery yet) and the boot launcher hands
//! them to whichever process policy grants [`CapClass::Display`] or
//! [`CapClass::Input`] -- see `kernel/src/launch.rs`.
//!
//! A PCI function is claimed the same way, and the shape of the capability is
//! the point: the kernel reads configuration space, enables the function's
//! decoding and its bus mastering, and hands the driver a [`DeviceKind::Pci`]
//! naming the *port windows the function already decodes* plus the ability to
//! allocate DMA memory. Configuration space itself is never exposed, so a
//! driver cannot move its own window on top of another device's, and the port
//! syscalls refuse an offset outside the window. `audio` drives an AC'97 and
//! `fs` drives a virtio-blk disk with no more authority than that.
//!
//! There is deliberately no standalone `Irq` object here even though
//! [`ObjectType::Irq`] exists: the two interrupts this module deals with --
//! the PS/2 keyboard's IRQ1 and the mouse's IRQ12, one per input `Device` --
//! are entirely internal to the objects the kernel already owns and feeds. A
//! bindable `Irq` object would only earn its keep for a driver claiming an
//! arbitrary or dynamic interrupt line, which nothing in this tree does yet.

use alloc::sync::Arc;

use crate::mm::vmo::{Vmo, VmoError};
use crate::sync::IrqSpinLock;

use super::{KernelObject, ObjectHeader, ObjectType, Rights, Signals};

/// Depth of one input device's byte queue. Generous relative to how fast a
/// human can type or move a hand; a full queue drops the oldest byte rather
/// than blocking the IRQ handler, which must never block.
const INPUT_QUEUE_LEN: usize = 64;

/// What a [`Device`] actually is.
#[derive(Clone, Copy, Debug)]
pub enum DeviceKind {
    /// A fixed physical MMIO range -- here, the boot framebuffer reported by
    /// the loader.
    FramebufferMmio {
        /// Physical base address of the first pixel.
        phys_base: u64,
        /// Length of the mapped range, in bytes (page-aligned).
        size: u64,
        /// Width in pixels.
        width: u32,
        /// Height in pixels.
        height: u32,
        /// Bytes per scanline.
        pitch: u32,
        /// Bits per pixel.
        bpp: u8,
    },
    /// The PS/2 keyboard controller's data port, fed by IRQ1.
    Ps2Keyboard,
    /// The PS/2 controller's auxiliary port -- the mouse -- fed by IRQ12.
    ///
    /// `present` records whether a mouse answered when the controller was
    /// brought up. The capability exists either way: a machine with no mouse
    /// still has an auxiliary port, and a driver that could not tell the
    /// difference would wait forever on a byte stream nothing will fill.
    Ps2Mouse {
        /// Whether a mouse answered the bring-up probe.
        present: bool,
    },
    /// A claimed PCI function, reduced to what a userspace driver can use:
    /// the port windows its BARs decode, and its interrupt line.
    Pci {
        /// What the function is, as the abi's `device_kind` names it.
        role: PciRole,
        /// PCI vendor identifier, for a driver that wants to check.
        vendor: u16,
        /// PCI device identifier.
        device: u16,
        /// The port windows the function decodes. An unused window has
        /// `len == 0` and every access to it is refused.
        windows: [PortWindow; MAX_PORT_WINDOWS],
        /// Interrupt line the firmware routed the function to. Reported for
        /// completeness; NodeOS has no bindable interrupt object yet, and both
        /// drivers in this tree poll.
        irq: u8,
    },
}

/// Number of port windows a PCI device capability can carry.
///
/// Two, because AC'97 is the device that needs the most: its mixer and its
/// bus-master engine are separate BARs, and a driver that could only be given
/// one of them would need two capabilities for one chip.
pub const MAX_PORT_WINDOWS: usize = 2;

/// A contiguous range of I/O ports a device decodes.
#[derive(Clone, Copy, Debug, Default)]
pub struct PortWindow {
    /// First port in the window.
    pub base: u16,
    /// Length in ports. Zero means "no window here".
    pub len: u16,
}

/// Which driver a claimed PCI function is for.
///
/// The kernel identifies the function -- it is the only thing that can read
/// the class code -- so that a driver does not have to be trusted to work out
/// what it was handed.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum PciRole {
    /// An Intel AC'97 audio controller.
    Ac97,
    /// A legacy-interface virtio block device.
    VirtioBlk,
}

/// Errors from device operations.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum DeviceError {
    /// The operation does not apply to this device's kind.
    WrongKind,
    /// The MMIO range could not be turned into a memory object.
    NoMemory,
    /// The port window, offset or access width is outside what this device
    /// decodes.
    OutOfRange,
}

/// Bytes one input device has produced and nobody has read yet.
///
/// A ring with the drop-oldest rule a device queue wants: the producer is an
/// interrupt handler that must never block, so a full queue loses the oldest
/// byte rather than the newest -- a stale scancode or half a mouse packet is
/// worth less than the movement that just happened.
struct InputQueue {
    buf: [u8; INPUT_QUEUE_LEN],
    head: usize,
    len: usize,
}

impl InputQueue {
    const fn new() -> Self {
        Self { buf: [0; INPUT_QUEUE_LEN], head: 0, len: 0 }
    }

    fn push(&mut self, byte: u8) {
        let tail = (self.head + self.len) % INPUT_QUEUE_LEN;
        if self.len == INPUT_QUEUE_LEN {
            // Queue is full: drop the oldest byte to make room. A stuck key
            // stream is a worse failure than losing one stale scancode.
            self.head = (self.head + 1) % INPUT_QUEUE_LEN;
            self.len -= 1;
        }
        self.buf[tail] = byte;
        self.len += 1;
    }

    fn drain_into(&mut self, out: &mut [u8]) -> usize {
        let n = out.len().min(self.len);
        for slot in out.iter_mut().take(n) {
            *slot = self.buf[self.head];
            self.head = (self.head + 1) % INPUT_QUEUE_LEN;
        }
        self.len -= n;
        n
    }

    fn is_empty(&self) -> bool {
        self.len == 0
    }
}

/// A claimed hardware device, handed to a userspace driver as a capability.
pub struct Device {
    header: ObjectHeader,
    kind: DeviceKind,
    /// Built lazily: most devices never need a `Vmo`, and building one before
    /// anyone asks would commit a `Vmo::new_physical` call this object cannot
    /// yet justify holding a handle for.
    vmo: IrqSpinLock<Option<Arc<Vmo>>>,
    /// Bytes this device has produced and nobody has read yet -- scancodes for
    /// the keyboard, packet bytes for the mouse. One queue type for every
    /// input kind so that `drain` has one implementation and one contract.
    input: IrqSpinLock<InputQueue>,
}

impl Device {
    /// Claim the boot framebuffer as a `Device`.
    pub fn framebuffer(phys_base: u64, width: u32, height: u32, pitch: u32, bpp: u8) -> Arc<Self> {
        let size = (pitch as u64) * (height as u64);
        Arc::new(Self {
            header: ObjectHeader::new(ObjectType::Device, Signals::NONE),
            kind: DeviceKind::FramebufferMmio { phys_base, size, width, height, pitch, bpp },
            vmo: IrqSpinLock::new(None),
            input: IrqSpinLock::new(InputQueue::new()),
        })
    }

    /// Claim the PS/2 keyboard as a `Device`.
    pub fn ps2_keyboard() -> Arc<Self> {
        Arc::new(Self {
            header: ObjectHeader::new(ObjectType::Device, Signals::NONE),
            kind: DeviceKind::Ps2Keyboard,
            vmo: IrqSpinLock::new(None),
            input: IrqSpinLock::new(InputQueue::new()),
        })
    }

    /// Claim the PS/2 controller's auxiliary port as a `Device`.
    ///
    /// `present` is what the bring-up probe found; see [`DeviceKind::Ps2Mouse`]
    /// for why the object exists either way.
    pub fn ps2_mouse(present: bool) -> Arc<Self> {
        Arc::new(Self {
            header: ObjectHeader::new(ObjectType::Device, Signals::NONE),
            kind: DeviceKind::Ps2Mouse { present },
            vmo: IrqSpinLock::new(None),
            input: IrqSpinLock::new(InputQueue::new()),
        })
    }

    /// Claim a PCI function found by the bus scan.
    ///
    /// The function's decoding and bus mastering are enabled here, in the
    /// kernel, because both live in the command register next to the BARs --
    /// see [`crate::arch::x86_64::pci`] for why that matters.
    ///
    /// # Safety
    /// `function` must have come from a scan of the live bus, and nothing else
    /// may already hold a capability over it: this hands out the authority to
    /// drive the hardware and to point its DMA engine at memory.
    pub unsafe fn pci(role: PciRole, function: &crate::arch::x86_64::pci::Function, bars: &[usize]) -> Arc<Self> {
        let mut windows = [PortWindow::default(); MAX_PORT_WINDOWS];
        for (slot, index) in bars.iter().take(MAX_PORT_WINDOWS).enumerate() {
            if let Some(base) = function.io_port(*index) {
                // SAFETY: forwarded contract -- a live function, claimed here
                // and nowhere else, before userspace exists.
                let len = unsafe { function.bar_size(*index) };
                windows[slot] = PortWindow { base, len: len.min(u16::MAX as u64) as u16 };
            }
        }
        // SAFETY: forwarded contract.
        unsafe { crate::arch::x86_64::pci::enable(function.address) };

        Arc::new(Self {
            header: ObjectHeader::new(ObjectType::Device, Signals::NONE),
            kind: DeviceKind::Pci {
                role,
                vendor: function.vendor,
                device: function.device,
                windows,
                irq: function.irq,
            },
            vmo: IrqSpinLock::new(None),
            input: IrqSpinLock::new(InputQueue::new()),
        })
    }

    /// The port window `index` decodes, if this device has one.
    fn window(&self, index: usize) -> Result<PortWindow, DeviceError> {
        let DeviceKind::Pci { windows, .. } = self.kind else {
            return Err(DeviceError::WrongKind);
        };
        match windows.get(index) {
            Some(window) if window.len != 0 => Ok(*window),
            _ => Err(DeviceError::OutOfRange),
        }
    }

    /// Resolve `window`+`offset` to an absolute port, refusing anything the
    /// device does not decode.
    ///
    /// This is the whole of the port capability check, and it is a range test
    /// because that is all "authority over this device" means at the bus
    /// level: an `out` to a port outside the window is an instruction aimed at
    /// somebody else's hardware.
    fn port_of(&self, window: usize, offset: u16, width: u8) -> Result<u16, DeviceError> {
        let window = self.window(window)?;
        if !matches!(width, 1 | 2 | 4) {
            return Err(DeviceError::OutOfRange);
        }
        // Unaligned port access is a device-specific hazard rather than a
        // fault, and no register in either driver wants one, so refuse it
        // here instead of finding out on hardware that is not QEMU.
        if !offset.is_multiple_of(width as u16) {
            return Err(DeviceError::OutOfRange);
        }
        let end = offset.checked_add(width as u16).ok_or(DeviceError::OutOfRange)?;
        if end > window.len {
            return Err(DeviceError::OutOfRange);
        }
        // Checked as well: a window whose base is near the top of the 16-bit
        // I/O space must not wrap into somebody else's ports.
        window.base.checked_add(offset).ok_or(DeviceError::OutOfRange)
    }

    /// Read a device register through a port window.
    pub fn pio_read(&self, window: usize, offset: u16, width: u8) -> Result<u32, DeviceError> {
        let port = self.port_of(window, offset, width)?;
        // SAFETY: the port lies inside a window this device decodes, and
        // holding a handle to this object is the authority for it. A read may
        // have device side effects, which is the driver's business.
        Ok(unsafe {
            match width {
                1 => crate::arch::x86_64::port::inb(port) as u32,
                2 => crate::arch::x86_64::port::inw(port) as u32,
                _ => crate::arch::x86_64::port::inl(port),
            }
        })
    }

    /// Write a device register through a port window.
    pub fn pio_write(&self, window: usize, offset: u16, width: u8, value: u32) -> Result<(), DeviceError> {
        let port = self.port_of(window, offset, width)?;
        // SAFETY: as `pio_read`.
        unsafe {
            match width {
                1 => crate::arch::x86_64::port::outb(port, value as u8),
                2 => crate::arch::x86_64::port::outw(port, value as u16),
                _ => crate::arch::x86_64::port::outl(port, value),
            }
        }
        Ok(())
    }

    /// Allocate a physically contiguous buffer this device can be pointed at.
    ///
    /// Only PCI devices get one: bus mastering is what makes a physical
    /// address useful, and it is the one thing a `Device` capability confers
    /// that the process could not have arranged for itself.
    pub fn dma_alloc(&self, len: u64) -> Result<Arc<Vmo>, DeviceError> {
        if !matches!(self.kind, DeviceKind::Pci { .. }) {
            return Err(DeviceError::WrongKind);
        }
        Vmo::new_dma(len).map_err(|_| DeviceError::NoMemory)
    }

    /// This device's kind.
    pub fn kind(&self) -> DeviceKind {
        self.kind
    }

    /// A memory object over this device's MMIO range, building it on first
    /// use. Only valid for MMIO-backed kinds.
    pub fn vmo(&self) -> Result<Arc<Vmo>, DeviceError> {
        let DeviceKind::FramebufferMmio { phys_base, size, .. } = self.kind else {
            return Err(DeviceError::WrongKind);
        };
        let mut slot = self.vmo.lock();
        if let Some(vmo) = slot.as_ref() {
            return Ok(vmo.clone());
        }
        // SAFETY: this physical range came from the loader's framebuffer tag,
        // which the frame allocator never hands out (see `mm::init`), and
        // holding a handle to this `Device` -- checked at the syscall layer
        // via `Rights::DEVICE_IO` -- is exactly the authority `Vmo::new_physical`
        // asks for.
        let vmo = unsafe { Vmo::new_physical(phys_base, size) }.map_err(|error| match error {
            VmoError::OutOfMemory => DeviceError::NoMemory,
            _ => DeviceError::NoMemory,
        })?;
        *slot = Some(vmo.clone());
        Ok(vmo)
    }

    /// Push one byte of input, called from the controller's IRQ handler.
    ///
    /// A scancode for the keyboard, a packet byte for the mouse; the handler
    /// is one function for both because the byte stream is one stream, and
    /// `super::super::arch::x86_64::ps2::service` decides which device a byte
    /// belongs to before it gets here.
    ///
    /// Never blocks and never allocates, so it is safe to call with interrupts
    /// masked and no thread context.
    pub fn push_input(&self, byte: u8) {
        self.input.lock().push(byte);
        self.header.signals().update(Signals::READABLE, Signals::NONE);
    }

    /// Drain up to `out.len()` queued input bytes, returning how many were
    /// copied. Clears the readable signal once the queue is empty.
    pub fn drain(&self, out: &mut [u8]) -> usize {
        let mut queue = self.input.lock();
        let n = queue.drain_into(out);
        if queue.is_empty() {
            self.header.signals().update(Signals::NONE, Signals::READABLE);
        }
        n
    }

    /// Rights a freshly created handle to this device should carry.
    pub fn default_rights(&self) -> Rights {
        Rights::DUPLICATE
            .or(Rights::TRANSFER)
            .or(Rights::READ)
            .or(Rights::DEVICE_IO)
            .or(Rights::GET_PROPERTY)
            .or(Rights::WAIT)
            .or(Rights::DESTROY)
    }
}

impl KernelObject for Device {
    fn header(&self) -> &ObjectHeader {
        &self.header
    }

    fn as_any(&self) -> &dyn core::any::Any {
        self
    }
}
