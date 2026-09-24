//! Claimed hardware devices, handed to a process at startup.

use nodeos_abi::types::{DeviceInfo, DmaInfo};
use nodeos_abi::{Error, HandleValue, Syscall};

use crate::mem::{Mapping, Vmo};
use crate::{sys, Handle};

/// A capability over one piece of hardware (the framebuffer, the keyboard).
pub struct Device {
    handle: Handle,
}

impl Device {
    /// Adopt a device handle received at startup.
    pub fn from_handle(handle: Handle) -> Self {
        Self { handle }
    }

    /// The underlying capability.
    pub fn handle(&self) -> &Handle {
        &self.handle
    }

    /// This device's kind-specific properties (e.g. framebuffer geometry).
    pub fn info(&self) -> Result<DeviceInfo, Error> {
        let mut info = DeviceInfo::default();
        // SAFETY: `&mut info` is a single, correctly sized, writable `DeviceInfo`.
        let raw = unsafe {
            sys::syscall2(Syscall::DeviceInfo, self.handle.raw().0 as u64, &mut info as *mut _ as u64)
        };
        sys::ok(raw).map(|()| info)
    }

    /// A memory object over this device's MMIO range (e.g. the framebuffer),
    /// and map it in one step.
    pub fn map(&self, len: u64, flags: nodeos_abi::types::MapFlags) -> Result<Mapping, Error> {
        // SAFETY: the handle value is a scalar.
        let raw = unsafe { sys::syscall1(Syscall::DeviceGetVmo, self.handle.raw().0 as u64) };
        let value = sys::status(raw)?;
        let vmo = Vmo::from_handle(Handle::from_raw(HandleValue(value as u32)), len);
        vmo.map(0, len, flags, nodeos_abi::types::MapWindow::Device)
    }

    /// Drain up to `out.len()` queued bytes (e.g. keyboard scancodes),
    /// non-blocking. Returns the number of bytes copied.
    pub fn read(&self, out: &mut [u8]) -> Result<usize, Error> {
        // SAFETY: `out` is a valid writable slice of the declared length.
        let raw = unsafe {
            sys::syscall3(Syscall::DeviceRead, self.handle.raw().0 as u64, out.as_mut_ptr() as u64, out.len() as u64)
        };
        sys::status(raw).map(|n| n as usize)
    }


    /// Read one register through port window `window`.
    ///
    /// `width` is 1, 2 or 4 bytes. The offset is relative to the window, not a
    /// port number: a driver in ring 3 cannot name a port at all, which is the
    /// point -- an offset the device does not decode is refused rather than
    /// aimed at somebody else's hardware.
    pub fn pio_read(&self, window: u32, offset: u16, width: u8) -> Result<u32, Error> {
        // SAFETY: all four arguments are scalars.
        let raw = unsafe {
            sys::syscall4(
                Syscall::DevicePioRead,
                self.handle.raw().0 as u64,
                window as u64,
                offset as u64,
                width as u64,
            )
        };
        sys::status(raw).map(|value| value as u32)
    }

    /// Write one register through port window `window`. See [`Device::pio_read`].
    pub fn pio_write(&self, window: u32, offset: u16, width: u8, value: u32) -> Result<(), Error> {
        // SAFETY: all five arguments are scalars.
        let raw = unsafe {
            sys::syscall5(
                Syscall::DevicePioWrite,
                self.handle.raw().0 as u64,
                window as u64,
                offset as u64,
                width as u64,
                value as u64,
            )
        };
        sys::ok(raw)
    }

    /// Read an 8-bit register.
    pub fn read8(&self, window: u32, offset: u16) -> Result<u8, Error> {
        self.pio_read(window, offset, 1).map(|value| value as u8)
    }

    /// Read a 16-bit register.
    pub fn read16(&self, window: u32, offset: u16) -> Result<u16, Error> {
        self.pio_read(window, offset, 2).map(|value| value as u16)
    }

    /// Read a 32-bit register.
    pub fn read32(&self, window: u32, offset: u16) -> Result<u32, Error> {
        self.pio_read(window, offset, 4)
    }

    /// Write an 8-bit register.
    pub fn write8(&self, window: u32, offset: u16, value: u8) -> Result<(), Error> {
        self.pio_write(window, offset, 1, value as u32)
    }

    /// Write a 16-bit register.
    pub fn write16(&self, window: u32, offset: u16, value: u16) -> Result<(), Error> {
        self.pio_write(window, offset, 2, value as u32)
    }

    /// Write a 32-bit register.
    pub fn write32(&self, window: u32, offset: u16, value: u32) -> Result<(), Error> {
        self.pio_write(window, offset, 4, value)
    }

    /// Allocate a physically contiguous buffer for this device's DMA engine,
    /// map it, and report the physical address to program into the device.
    pub fn dma_alloc(&self, len: u64) -> Result<Dma, Error> {
        let mut info = DmaInfo::default();
        // SAFETY: `&mut info` is a single, correctly sized, writable `DmaInfo`.
        let raw = unsafe {
            sys::syscall3(Syscall::DeviceDmaAlloc, self.handle.raw().0 as u64, len, &mut info as *mut _ as u64)
        };
        sys::ok(raw)?;

        let vmo = Vmo::from_handle(Handle::from_raw(HandleValue(info.handle)), info.len);
        let mapping = vmo.map(
            0,
            info.len,
            nodeos_abi::types::MapFlags::READ
                .or(nodeos_abi::types::MapFlags::WRITE)
                .or(nodeos_abi::types::MapFlags::SHARED)
                .or(nodeos_abi::types::MapFlags::POPULATE),
            nodeos_abi::types::MapWindow::Shared,
        )?;
        Ok(Dma { _vmo: vmo, phys: info.phys, base: mapping.base(), len: info.len, _mapping: mapping })
    }
}

/// A buffer a device's DMA engine can be pointed at.
///
/// Holds both addresses of the same bytes: the virtual one this process writes
/// through, and the physical one the device follows. They are different views
/// of one allocation and neither is derivable from the other in ring 3, which
/// is why the kernel returns both.
pub struct Dma {
    _vmo: Vmo,
    _mapping: Mapping,
    phys: u64,
    base: u64,
    len: u64,
}

impl Dma {
    /// Physical address to program into the device.
    pub fn phys(&self) -> u64 {
        self.phys
    }

    /// Length in bytes.
    pub fn len(&self) -> u64 {
        self.len
    }

    /// Whether the buffer is empty. Never true in practice; `clippy` asks.
    pub fn is_empty(&self) -> bool {
        self.len == 0
    }

    /// The buffer as bytes this process can write.
    pub fn bytes(&mut self) -> &mut [u8] {
        // SAFETY: the mapping covers `len` bytes, is readable and writable,
        // and `&mut self` is the only reference to it in this process.
        unsafe { core::slice::from_raw_parts_mut(self.base as *mut u8, self.len as usize) }
    }

    /// A pointer into the buffer at `offset`, for a driver writing a
    /// descriptor at a fixed place in a ring.
    ///
    /// # Safety
    /// `offset` must be within the buffer, and the caller must respect the
    /// device's rules about which parts of it the device may be reading.
    pub unsafe fn at<T>(&self, offset: u64) -> *mut T {
        // SAFETY: forwarded contract.
        unsafe { (self.base as *mut u8).add(offset as usize) as *mut T }
    }
}

impl Device {
    /// Block until at least one byte is available, then drain it.
    pub fn read_blocking(&self, out: &mut [u8]) -> Result<usize, Error> {
        loop {
            let n = self.read(out)?;
            if n > 0 {
                return Ok(n);
            }
            self.handle.wait(nodeos_abi::Signals::READABLE, 0)?;
        }
    }
}
