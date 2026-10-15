//! A virtio block device driver, in ring 3.
//!
//! Same deal as `audio`'s AC'97: a `Device` capability whose one port window
//! is the controller's legacy register file, plus a DMA buffer the kernel
//! allocated contiguously. No configuration space, no interrupt, no ability to
//! name a port this device does not decode.
//!
//! # The queue
//!
//! Virtio's transport is a ring of *descriptors* in guest memory that the
//! device reads. A request is a chain of three:
//!
//! ```text
//!   desc[0] ── header: {type, reserved, sector}      device reads
//!   desc[1] ── the data                              device reads (write) /
//!                                                    device writes (read)
//!   desc[2] ── one status byte                       device writes
//! ```
//!
//! The driver publishes the head of the chain in the *available* ring, writes
//! the notify register, and the device puts the head back in the *used* ring
//! when it is done. This driver then spins on the used index, because NodeOS
//! has no bindable interrupt object -- see `docs/FILESYSTEM.md`. A disk
//! request is microseconds under emulation, and the alternative would be an
//! interrupt capability that only this one driver would ever use.
//!
//! # Legacy, deliberately
//!
//! This speaks the legacy (0x1AF4:0x1001) interface: registers in port space,
//! one `Queue Address` register taking a page frame number. The modern
//! interface puts the same registers behind PCI capability structures in MMIO,
//! which would mean teaching the kernel to hand out configuration space or a
//! second BAR. The legacy path is what QEMU presents by default and it needs
//! neither.

use libnode::device::{Device, Dma};
use libnode::Error;

/// The device's register file is port window 0.
const IO: u32 = 0;

// --- Legacy virtio-pci registers -------------------------------------------
/// Features the device offers.
const REG_DEVICE_FEATURES: u16 = 0x00;
/// Features the driver accepts.
const REG_DRIVER_FEATURES: u16 = 0x04;
/// Physical page frame number of the selected queue.
const REG_QUEUE_ADDRESS: u16 = 0x08;
/// Size of the selected queue, in descriptors.
const REG_QUEUE_SIZE: u16 = 0x0C;
/// Which queue the queue registers refer to.
const REG_QUEUE_SELECT: u16 = 0x0E;
/// Writing a queue number here tells the device to look at its available ring.
const REG_QUEUE_NOTIFY: u16 = 0x10;
/// Device status.
const REG_STATUS: u16 = 0x12;
/// Start of the device-specific configuration; for a block device, capacity.
const REG_CONFIG: u16 = 0x14;

/// Status bit: the driver has noticed the device.
const STATUS_ACKNOWLEDGE: u8 = 1;
/// Status bit: the driver knows how to drive it.
const STATUS_DRIVER: u8 = 2;
/// Status bit: setup is complete and the device may run.
const STATUS_DRIVER_OK: u8 = 4;
/// Status bit: something went wrong and the device must be reset.
const STATUS_FAILED: u8 = 128;

/// Request type: read from the disk.
const BLK_IN: u32 = 0;
/// Request type: write to the disk.
const BLK_OUT: u32 = 1;

/// Descriptor flag: the chain continues at `next`.
const DESC_NEXT: u16 = 1;
/// Descriptor flag: the device writes this buffer rather than reading it.
const DESC_WRITE: u16 = 2;

/// Bytes in a disk sector, as virtio defines it regardless of the underlying
/// medium.
pub const SECTOR_SIZE: usize = 512;

/// Largest transfer in one request. Sixteen sectors is a FAT cluster at the
/// sizes `mformat` produces, so a whole cluster moves in one round trip.
pub const MAX_TRANSFER: usize = 64 * 1024;

/// One descriptor, as the device reads it.
#[repr(C)]
#[derive(Clone, Copy, Default)]
struct Descriptor {
    address: u64,
    length: u32,
    flags: u16,
    next: u16,
}

/// The block request header, as the device reads it.
#[repr(C)]
#[derive(Clone, Copy, Default)]
struct BlockHeader {
    kind: u32,
    _reserved: u32,
    sector: u64,
}

/// A claimed virtio block device with its queue set up.
pub struct VirtioBlk {
    device: Device,
    dma: Dma,
    /// Descriptors in the queue.
    queue_size: u16,
    /// Offsets within the DMA buffer.
    avail_offset: u64,
    used_offset: u64,
    header_offset: u64,
    status_offset: u64,
    data_offset: u64,
    /// Our copy of the available ring's index, which only this driver writes.
    avail_index: u16,
    /// The used ring index last observed, to recognise completion.
    used_index: u16,
    /// Sectors on the disk.
    capacity: u64,
    /// Requests completed, for diagnostics.
    requests: u64,
}

impl VirtioBlk {
    /// Reset the device, build the queue, and declare the driver ready.
    pub fn new(device: Device) -> Result<Self, Error> {
        // Reset, then acknowledge in the order the specification requires: a
        // device that sees DRIVER_OK before its queue is programmed is
        // entitled to start reading a queue that does not exist yet.
        device.write8(IO, REG_STATUS, 0)?;
        device.write8(IO, REG_STATUS, STATUS_ACKNOWLEDGE)?;
        device.write8(IO, REG_STATUS, STATUS_ACKNOWLEDGE | STATUS_DRIVER)?;

        // Accept no optional features. Everything this driver uses -- one
        // queue, read and write, a capacity in the configuration space -- is
        // in the base device, so there is nothing to negotiate and nothing
        // that can go wrong in the negotiation.
        let _offered = device.read32(IO, REG_DEVICE_FEATURES)?;
        device.write32(IO, REG_DRIVER_FEATURES, 0)?;

        device.write16(IO, REG_QUEUE_SELECT, 0)?;
        let queue_size = device.read16(IO, REG_QUEUE_SIZE)?;
        if queue_size == 0 {
            device.write8(IO, REG_STATUS, STATUS_FAILED)?;
            return Err(Error::NotSupported);
        }

        // The legacy queue layout is three arrays in one contiguous block,
        // with the used ring starting on a page boundary. Laying it out here
        // rather than deriving it at each use keeps the arithmetic in one
        // place, which is worth doing once: an off-by-one in the used ring's
        // offset shows up as a device that never completes anything.
        let descriptors = 16u64 * queue_size as u64;
        let avail_offset = descriptors;
        let avail_bytes = 6 + 2 * queue_size as u64;
        let used_offset = align_up(avail_offset + avail_bytes, 4096);
        let used_bytes = 6 + 8 * queue_size as u64;

        let header_offset = align_up(used_offset + used_bytes, 16);
        let status_offset = header_offset + 16;
        let data_offset = align_up(status_offset + 1, 16);
        let total = data_offset + MAX_TRANSFER as u64;

        let dma = device.dma_alloc(total)?;
        // The queue must start at a page boundary because the device is given
        // a page frame number, not an address -- the low twelve bits are not
        // representable. `dma_alloc` returns whole pages, so this holds by
        // construction; assert it rather than discover it as data corruption.
        debug_assert_eq!(dma.phys() % 4096, 0);
        device.write32(IO, REG_QUEUE_ADDRESS, (dma.phys() / 4096) as u32)?;

        // Capacity is the first field of the block device's configuration
        // space, in 512-byte sectors, and it is read as two halves because the
        // port window is 32 bits wide.
        let low = device.read32(IO, REG_CONFIG)? as u64;
        let high = device.read32(IO, REG_CONFIG + 4)? as u64;
        let capacity = low | (high << 32);

        device.write8(IO, REG_STATUS, STATUS_ACKNOWLEDGE | STATUS_DRIVER | STATUS_DRIVER_OK)?;

        Ok(Self {
            device,
            dma,
            queue_size,
            avail_offset,
            used_offset,
            header_offset,
            status_offset,
            data_offset,
            avail_index: 0,
            used_index: 0,
            capacity,
            requests: 0,
        })
    }

    /// Sectors on the disk.
    pub fn capacity(&self) -> u64 {
        self.capacity
    }

    /// Requests completed since boot.
    pub fn requests(&self) -> u64 {
        self.requests
    }

    /// Read `out.len()` bytes, which must be a whole number of sectors,
    /// starting at `sector`.
    pub fn read(&mut self, sector: u64, out: &mut [u8]) -> Result<(), Error> {
        if out.len() % SECTOR_SIZE != 0 || out.is_empty() || out.len() > MAX_TRANSFER {
            return Err(Error::InvalidArgument);
        }
        self.request(BLK_IN, sector, out.len())?;
        // SAFETY: the data area is `MAX_TRANSFER` bytes inside the DMA buffer
        // and the device has finished writing it -- `request` did not return
        // until the descriptor came back on the used ring.
        unsafe {
            let source: *const u8 = self.dma.at(self.data_offset);
            core::ptr::copy_nonoverlapping(source, out.as_mut_ptr(), out.len());
        }
        Ok(())
    }

    /// Write `data`, which must be a whole number of sectors, at `sector`.
    pub fn write(&mut self, sector: u64, data: &[u8]) -> Result<(), Error> {
        if data.len() % SECTOR_SIZE != 0 || data.is_empty() || data.len() > MAX_TRANSFER {
            return Err(Error::InvalidArgument);
        }
        // SAFETY: as `read`; the device is not reading this area until the
        // descriptor below is published.
        unsafe {
            let destination: *mut u8 = self.dma.at(self.data_offset);
            core::ptr::copy_nonoverlapping(data.as_ptr(), destination, data.len());
        }
        self.request(BLK_OUT, sector, data.len())
    }

    /// Publish one three-descriptor chain and wait for it to come back.
    fn request(&mut self, kind: u32, sector: u64, length: usize) -> Result<(), Error> {
        // The whole transfer has to fit, not just its first sector: a request
        // at the last sector with a multi-sector length would otherwise address
        // past the end of the device. Compare in sectors and with a checked
        // add, so neither a huge sector nor a huge length can wrap.
        let sectors = (length as u64).div_ceil(SECTOR_SIZE as u64);
        let end = sector.checked_add(sectors).ok_or(Error::InvalidArgument)?;
        if end > self.capacity {
            return Err(Error::InvalidArgument);
        }

        // SAFETY: every offset below is inside the DMA allocation by
        // construction, and this driver issues one request at a time, so no
        // descriptor being written here is one the device is reading.
        unsafe {
            let header: *mut BlockHeader = self.dma.at(self.header_offset);
            header.write_volatile(BlockHeader { kind, _reserved: 0, sector });

            let status: *mut u8 = self.dma.at(self.status_offset);
            // 0xFF is not a status the device defines, so the check after the
            // wait distinguishes "the device said OK" from "the device never
            // wrote anything", which are very different bugs.
            status.write_volatile(0xFF);

            let descriptors: *mut Descriptor = self.dma.at(0);
            descriptors.write_volatile(Descriptor {
                address: self.dma.phys() + self.header_offset,
                length: 16,
                flags: DESC_NEXT,
                next: 1,
            });
            descriptors.add(1).write_volatile(Descriptor {
                address: self.dma.phys() + self.data_offset,
                length: length as u32,
                // The direction is from the *device's* point of view: it writes
                // the buffer for a read and reads it for a write.
                flags: DESC_NEXT | if kind == BLK_IN { DESC_WRITE } else { 0 },
                next: 2,
            });
            descriptors.add(2).write_volatile(Descriptor {
                address: self.dma.phys() + self.status_offset,
                length: 1,
                flags: DESC_WRITE,
                next: 0,
            });

            // Available ring: a flags word, an index, then the ring itself.
            let ring: *mut u16 = self.dma.at(self.avail_offset + 4);
            ring.add((self.avail_index % self.queue_size) as usize).write_volatile(0);
            // The device may look the moment the index moves, so everything
            // above must be visible first. On x86 the store order is already
            // guaranteed; the fence is against the *compiler* reordering, which
            // is not.
            core::sync::atomic::fence(core::sync::atomic::Ordering::SeqCst);
            self.avail_index = self.avail_index.wrapping_add(1);
            let index: *mut u16 = self.dma.at(self.avail_offset + 2);
            index.write_volatile(self.avail_index);
        }

        self.device.write16(IO, REG_QUEUE_NOTIFY, 0)?;

        // Spin for the completion. The bound is not a timeout in any real
        // sense -- it is the difference between a wedged disk stalling this
        // service forever and reporting an error the caller can print.
        let mut spins = 0u64;
        loop {
            // SAFETY: the used ring is inside the DMA buffer; its index is at
            // offset 2, after the flags word.
            let used = unsafe {
                let index: *const u16 = self.dma.at(self.used_offset + 2);
                index.read_volatile()
            };
            if used != self.used_index {
                self.used_index = used;
                break;
            }
            spins += 1;
            if spins > 50_000_000 {
                return Err(Error::TimedOut);
            }
            // Yielding rather than pausing: another process may be what makes
            // the emulated device progress, and this service has nothing else
            // to do until the sector arrives.
            if spins % 64 == 0 {
                libnode::yield_now();
            }
        }

        // SAFETY: the device has written the status byte, as the used ring
        // entry above attests.
        let status = unsafe {
            let status: *const u8 = self.dma.at(self.status_offset);
            status.read_volatile()
        };
        self.requests += 1;
        match status {
            0 => Ok(()),
            // 1 is IOERR, 2 is UNSUPP, 0xFF is "the device never answered".
            _ => Err(Error::BadState),
        }
    }
}

fn align_up(value: u64, alignment: u64) -> u64 {
    (value + alignment - 1) & !(alignment - 1)
}
