//! PCI configuration space, and nothing else.
//!
//! This module exists so that the rest of the kernel -- and every userspace
//! driver -- never has to touch configuration space. It answers exactly one
//! question, "what is on the bus and what does it decode", and the boot code
//! turns the answer into [`crate::obj::Device`] capabilities naming *just* the
//! port windows a function responds to.
//!
//! That split is the whole capability story for PCI hardware. Configuration
//! space is the one place where a device's decoding can be *changed*: a driver
//! that could write BAR0 could move its device's window on top of another
//! device's, or on top of the framebuffer. So configuration space stays here,
//! in the kernel, and a driver process is handed a window it cannot relocate
//! and a bus-master enable it did not have to ask for. The authority a
//! userspace AC'97 driver holds is "these 320 bytes of port space"; it cannot
//! name a byte outside them, and it cannot become a different device.
//!
//! The mechanism is the legacy 0xCF8/0xCFC pair rather than the memory-mapped
//! ECAM window. Both work on q35; the port pair also works on the i440fx
//! machine type and needs no ACPI table parsed to find it, and this kernel has
//! no ACPI parser.

use alloc::vec::Vec;

use super::port;

/// Configuration space address port.
const CONFIG_ADDRESS: u16 = 0xCF8;
/// Configuration space data port.
const CONFIG_DATA: u16 = 0xCFC;

/// A function's position on the bus.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Address {
    /// Bus number.
    pub bus: u8,
    /// Device (slot) number, 0..32.
    pub slot: u8,
    /// Function number, 0..8.
    pub func: u8,
}

impl Address {
    /// The 32-bit selector the address port wants, with `offset` inside the
    /// function's 256-byte configuration space.
    fn selector(self, offset: u8) -> u32 {
        (1 << 31)
            | ((self.bus as u32) << 16)
            | ((self.slot as u32) << 11)
            | ((self.func as u32) << 8)
            | ((offset as u32) & 0xFC)
    }
}

/// Read a 32-bit configuration register.
///
/// # Safety
/// Configuration space is a global resource: a concurrent reader on another
/// CPU would see this call's address-port write. The kernel only reads it
/// during boot, single-threaded, which is the discipline this relies on.
pub unsafe fn read32(address: Address, offset: u8) -> u32 {
    // SAFETY: the caller upholds the single-threaded-boot discipline above.
    unsafe {
        port::outl(CONFIG_ADDRESS, address.selector(offset));
        port::inl(CONFIG_DATA)
    }
}

/// Write a 32-bit configuration register.
///
/// # Safety
/// As [`read32`], plus: the value must be one the device tolerates. Writing a
/// BAR moves the device's window and can make another device unreachable.
pub unsafe fn write32(address: Address, offset: u8, value: u32) {
    // SAFETY: forwarded contract.
    unsafe {
        port::outl(CONFIG_ADDRESS, address.selector(offset));
        port::outl(CONFIG_DATA, value);
    }
}

/// Read a 16-bit configuration register.
///
/// # Safety
/// See [`read32`].
pub unsafe fn read16(address: Address, offset: u8) -> u16 {
    // SAFETY: forwarded contract.
    let word = unsafe { read32(address, offset) };
    ((word >> ((offset as u32 & 2) * 8)) & 0xFFFF) as u16
}

/// One PCI function, as the scan found it.
#[derive(Clone, Copy, Debug)]
pub struct Function {
    /// Where it lives.
    pub address: Address,
    /// PCI vendor identifier.
    pub vendor: u16,
    /// PCI device identifier.
    pub device: u16,
    /// Base class code.
    pub class: u8,
    /// Sub-class code.
    pub subclass: u8,
    /// The six base address registers, raw.
    pub bars: [u32; 6],
    /// Legacy interrupt line, as the firmware routed it.
    pub irq: u8,
}

impl Function {
    /// The I/O port base `bars[index]` decodes, if it is an I/O BAR.
    pub fn io_port(&self, index: usize) -> Option<u16> {
        let bar = *self.bars.get(index)?;
        // Bit 0 distinguishes an I/O BAR from a memory BAR; bits 1..2 are
        // reserved and must be masked off before the base is usable.
        if bar & 1 == 0 || bar & !0x3 == 0 {
            return None;
        }
        Some((bar & !0x3) as u16)
    }

    /// The 32-bit memory base `bars[index]` decodes, if it is a memory BAR.
    pub fn mmio_base(&self, index: usize) -> Option<u64> {
        let bar = *self.bars.get(index)?;
        if bar & 1 != 0 || bar & !0xF == 0 {
            return None;
        }
        Some((bar & !0xF) as u64)
    }

    /// The size of BAR `index`, discovered the only way PCI offers: write all
    /// ones, read back which bits stuck, put the original back.
    ///
    /// # Safety
    /// As [`write32`]. The BAR is restored before returning, but a device
    /// decoding I/O while its BAR reads all-ones is briefly claiming a very
    /// large window, so this must run before anything else is using the bus.
    pub unsafe fn bar_size(&self, index: usize) -> u64 {
        let offset = 0x10 + (index as u8) * 4;
        let original = self.bars[index];
        // SAFETY: forwarded contract; the original value is restored below.
        let mask = unsafe {
            write32(self.address, offset, 0xFFFF_FFFF);
            let probed = read32(self.address, offset);
            write32(self.address, offset, original);
            probed
        };
        let mask = if original & 1 != 0 { mask & !0x3 } else { mask & !0xF };
        if mask == 0 {
            return 0;
        }
        // The lowest set bit of the read-back mask is the alignment, which for
        // a BAR is also its length.
        (!mask).wrapping_add(1) as u64 & 0xFFFF_FFFF
    }
}

/// Command register bits.
const COMMAND: u8 = 0x04;
const COMMAND_IO_SPACE: u16 = 1 << 0;
const COMMAND_MEMORY_SPACE: u16 = 1 << 1;
const COMMAND_BUS_MASTER: u16 = 1 << 2;

/// Let a function decode its windows and initiate DMA.
///
/// Bus mastering is what makes a device able to write the guest's memory, and
/// it is enabled *here* rather than by the driver for the reason the module
/// comment gives: it lives in the command register, one word away from the
/// BARs. A driver that could set it could also move them.
///
/// # Safety
/// See [`write32`]. Enabling bus mastering on a device whose DMA registers
/// hold stale addresses would let it write those addresses; every driver in
/// this tree programs its ring before starting the engine.
pub unsafe fn enable(address: Address) {
    // SAFETY: forwarded contract.
    unsafe {
        let command = read16(address, COMMAND);
        let next = command | COMMAND_IO_SPACE | COMMAND_MEMORY_SPACE | COMMAND_BUS_MASTER;
        // The command register is the low half of a 32-bit register whose high
        // half is the status register, which is write-1-to-clear. Preserving it
        // as read would clear whatever is latched there, so write zero to it:
        // the kernel has no use for latched error bits and clearing them is
        // the polite state to leave a freshly claimed device in.
        write32(address, COMMAND, next as u32);
    }
}

/// Every function present on the bus.
///
/// A brute-force sweep of all 256 buses rather than a walk down bridges: it
/// costs 65k configuration reads once at boot -- microseconds -- and it needs
/// no bridge-secondary-bus logic to be correct on a machine type this kernel
/// has never seen.
///
/// # Safety
/// Must run once, on the boot CPU, before userspace exists. See [`read32`].
pub unsafe fn scan() -> Vec<Function> {
    let mut found = Vec::new();
    for bus in 0..=255u8 {
        for slot in 0..32u8 {
            for func in 0..8u8 {
                let address = Address { bus, slot, func };
                // SAFETY: forwarded contract.
                let ids = unsafe { read32(address, 0x00) };
                let vendor = (ids & 0xFFFF) as u16;
                if vendor == 0xFFFF {
                    // No function here. Function 0 absent means the whole slot
                    // is absent, which is worth 7 skipped probes per slot.
                    if func == 0 {
                        break;
                    }
                    continue;
                }
                // SAFETY: forwarded contract.
                let (classes, bars, irq) = unsafe {
                    let classes = read32(address, 0x08);
                    let mut bars = [0u32; 6];
                    for (index, bar) in bars.iter_mut().enumerate() {
                        *bar = read32(address, 0x10 + (index as u8) * 4);
                    }
                    (classes, bars, (read32(address, 0x3C) & 0xFF) as u8)
                };
                found.push(Function {
                    address,
                    vendor,
                    device: ((ids >> 16) & 0xFFFF) as u16,
                    class: ((classes >> 24) & 0xFF) as u8,
                    subclass: ((classes >> 16) & 0xFF) as u8,
                    bars,
                    irq,
                });
            }
        }
    }
    found
}
