#![no_std]

extern crate alloc;

use alloc::vec::Vec;
use core::ptr::{copy_nonoverlapping, read_volatile, write_volatile};
use core::sync::atomic::{fence, Ordering};

use libnode::abi::message::MessageFlags;
use libnode::abi::types::{device_kind, MapFlags};
use libnode::device::{Device, Dma};
use libnode::mem::Mapping;
use libnode::{Channel, Error, Message};

/// Request/reply for retrieving the link state and station MAC.
pub const NET_INFO: u32 = 0x4E45_0001;
/// Request/reply for transmitting one Ethernet frame.
pub const NET_TRANSMIT: u32 = 0x4E45_0002;
/// Non-blocking request/reply for receiving one Ethernet frame.
pub const NET_RECEIVE: u32 = 0x4E45_0003;
/// Maximum frame size returned by the network service.
pub const MAX_RX_PACKET_SIZE: usize = 2048;

/// Link properties returned by [`NetworkClient::link_info`].
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct LinkInfo {
    /// Station MAC address.
    pub mac_address: [u8; 6],
    /// Whether carrier is currently reported.
    pub link_up: bool,
}

/// Client for the network service's raw-Ethernet channel protocol.
///
/// Transmit calls copy their payload into a driver-owned DMA buffer. Receive
/// calls return an owned frame, or `None` when no frame is queued.
pub struct NetworkClient {
    channel: Channel,
}

impl NetworkClient {
    /// Use a network channel capability received from the launcher.
    pub const fn from_channel(channel: Channel) -> Self {
        Self { channel }
    }

    /// Query the current link state and station MAC address.
    pub fn link_info(&self) -> Result<LinkInfo, Error> {
        let reply = self.channel.call_with(Message::new(NET_INFO), 8, 0)?;
        check_reply(&reply)?;
        if reply.payload.len() != 7 || reply.payload[6] > 1 {
            return Err(Error::BadState);
        }
        let mut mac_address = [0; 6];
        mac_address.copy_from_slice(&reply.payload[..6]);
        Ok(LinkInfo { mac_address, link_up: reply.payload[6] != 0 })
    }

    /// Send one raw Ethernet frame.
    pub fn transmit(&self, frame: &[u8]) -> Result<(), Error> {
        if frame.is_empty() || frame.len() > MAX_TX_PACKET_SIZE {
            return Err(Error::InvalidArgument);
        }
        let reply = self.channel.call_with(Message::new(NET_TRANSMIT).with_payload(frame), 8, 0)?;
        check_reply(&reply)
    }

    /// Return the next queued raw Ethernet frame, if one is available.
    pub fn receive(&self) -> Result<Option<Vec<u8>>, Error> {
        let reply = self.channel.call_with(Message::new(NET_RECEIVE), MAX_RX_PACKET_SIZE, 0)?;
        check_reply(&reply)?;
        if reply.payload.is_empty() {
            Ok(None)
        } else {
            Ok(Some(reply.payload))
        }
    }
}

fn check_reply(reply: &Message) -> Result<(), Error> {
    if !reply.flags.contains(MessageFlags::ERROR) {
        return Ok(());
    }
    let raw = reply
        .payload
        .get(..8)
        .and_then(|bytes| bytes.try_into().ok())
        .map(i64::from_le_bytes)
        .ok_or(Error::BadState)?;
    Err(Error::from_raw(raw).unwrap_or(Error::BadState))
}

const E1000_CTRL: usize = 0x0000;
const E1000_STATUS: usize = 0x0008;
const E1000_CTRL_RST: u32 = 1 << 26;
const E1000_CTRL_SLU: u32 = 1 << 6;
const E1000_STATUS_LINK_UP: u32 = 1 << 1;
const E1000_IMC: usize = 0x00D8;
const E1000_RCTL: usize = 0x0100;
const E1000_TCTL: usize = 0x0400;
const E1000_TIPG: usize = 0x0410;
const E1000_RAL0: usize = 0x5400;
const E1000_RAH0: usize = 0x5404;
const E1000_TDBAL: usize = 0x3800;
const E1000_TDBAH: usize = 0x3804;
const E1000_TDLEN: usize = 0x3808;
const E1000_TDH: usize = 0x3810;
const E1000_TDT: usize = 0x3818;
const E1000_RDBAL: usize = 0x2800;
const E1000_RDBAH: usize = 0x2804;
const E1000_RDLEN: usize = 0x2808;
const E1000_RDH: usize = 0x2810;
const E1000_RDT: usize = 0x2818;

/// Minimum e1000 BAR0 span required by this driver.
pub const E1000_BAR0_LEN: u64 = 0x6000;
const NUM_TX_DESCRIPTORS: usize = 64;
const NUM_RX_DESCRIPTORS: usize = 64;
/// Receive buffer capacity, in bytes.
pub const RX_BUFFER_SIZE: usize = 2048;
/// Maximum untagged Ethernet frame size accepted for transmission.
pub const MAX_TX_PACKET_SIZE: usize = 1518;
const TX_BUFFER_SIZE: usize = 2048;
const DESCRIPTOR_SIZE: usize = 16;
const TX_RING_OFFSET: usize = 0;
const RX_RING_OFFSET: usize = 4096;
const RX_BUFFER_OFFSET: usize = 8192;
const TX_BUFFER_OFFSET: usize = RX_BUFFER_OFFSET + NUM_RX_DESCRIPTORS * RX_BUFFER_SIZE;
const DMA_ARENA_LEN: u64 = (TX_BUFFER_OFFSET + NUM_TX_DESCRIPTORS * TX_BUFFER_SIZE) as u64;
const DESC_DD: u8 = 1;
const RX_STATUS_EOP: u8 = 1 << 1;
const TX_CMD_EOP: u8 = 1;
const TX_CMD_IFCS: u8 = 1 << 1;
const TX_CMD_RS: u8 = 1 << 3;
const RESET_POLL_LIMIT: usize = 1_000_000;

#[repr(C)]
#[derive(Clone, Copy, Default)]
struct TxDescriptor {
    addr: u64,
    length: u16,
    checksum_offset: u8,
    command: u8,
    status: u8,
    checksum_start: u8,
    special: u16,
}

#[repr(C)]
#[derive(Clone, Copy, Default)]
struct RxDescriptor {
    addr: u64,
    length: u16,
    checksum: u16,
    status: u8,
    errors: u8,
    special: u16,
}

/// Why a received Ethernet frame could not be copied to a caller's buffer.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ReceiveError {
    /// The frame remains queued; retry with a buffer at least this large.
    BufferTooSmall { needed: usize },
}

/// A polled e1000 driver backed by a device capability and contiguous DMA VMO.
pub struct E1000Driver {
    _device: Device,
    _mmio: Mapping,
    dma: Dma,
    dma_base: *mut u8,
    mmio_base: usize,
    mac: [u8; 6],
    tx_next_to_use: usize,
    tx_next_to_clean: usize,
    rx_next_to_clean: usize,
}

impl E1000Driver {
    /// Validate and initialize the claimed Intel 82540EM-compatible device.
    pub fn new(device: Device) -> Result<Self, Error> {
        let info = device.info()?;
        if info.kind != device_kind::E1000 || info.vendor != 0x8086 || info.device != 0x100E {
            return Err(Error::WrongType);
        }

        let mmio = device.map(
            E1000_BAR0_LEN,
            MapFlags::READ
                .or(MapFlags::WRITE)
                .or(MapFlags::DEVICE)
                .or(MapFlags::POPULATE),
        )?;
        let mut dma = device.dma_alloc(DMA_ARENA_LEN)?;
        let dma_base = dma.bytes().as_mut_ptr();
        let mmio_base = mmio.base() as usize;

        // SAFETY: DeviceDmaAlloc returns an exclusively owned mapping that is
        // physically contiguous, and both descriptor rings fit in this arena.
        let tx_ring = unsafe { dma_base.add(TX_RING_OFFSET).cast::<TxDescriptor>() };
        let rx_ring = unsafe { dma_base.add(RX_RING_OFFSET).cast::<RxDescriptor>() };
        for index in 0..NUM_TX_DESCRIPTORS {
            unsafe { write_volatile(tx_ring.add(index), TxDescriptor::default()) };
        }
        for index in 0..NUM_RX_DESCRIPTORS {
            let descriptor = RxDescriptor {
                addr: dma.phys() + (RX_BUFFER_OFFSET + index * RX_BUFFER_SIZE) as u64,
                ..RxDescriptor::default()
            };
            unsafe { write_volatile(rx_ring.add(index), descriptor) };
        }

        let mut driver = Self {
            _device: device,
            _mmio: mmio,
            dma,
            dma_base,
            mmio_base,
            mac: [0; 6],
            tx_next_to_use: 0,
            tx_next_to_clean: 0,
            rx_next_to_clean: NUM_RX_DESCRIPTORS - 1,
        };

        driver.write_reg(E1000_IMC, u32::MAX);
        driver.write_reg(E1000_CTRL, driver.read_reg(E1000_CTRL) | E1000_CTRL_RST);
        let mut reset = false;
        for _ in 0..RESET_POLL_LIMIT {
            if driver.read_reg(E1000_CTRL) & E1000_CTRL_RST == 0 {
                reset = true;
                break;
            }
            core::hint::spin_loop();
        }
        if !reset {
            return Err(Error::TimedOut);
        }

        driver.write_reg(E1000_IMC, u32::MAX);
        driver.write_reg(E1000_CTRL, driver.read_reg(E1000_CTRL) | E1000_CTRL_SLU);
        driver.mac = driver.read_mac()?;

        let tx_phys = driver.dma.phys() + TX_RING_OFFSET as u64;
        driver.write_reg(E1000_TDBAL, tx_phys as u32);
        driver.write_reg(E1000_TDBAH, (tx_phys >> 32) as u32);
        driver.write_reg(E1000_TDLEN, (NUM_TX_DESCRIPTORS * DESCRIPTOR_SIZE) as u32);
        driver.write_reg(E1000_TDH, 0);
        driver.write_reg(E1000_TDT, 0);
        driver.write_reg(E1000_TIPG, 10 | (8 << 10) | (6 << 20));
        driver.write_reg(E1000_TCTL, (1 << 1) | (1 << 3) | (0x0F << 4) | (0x40 << 12));

        let rx_phys = driver.dma.phys() + RX_RING_OFFSET as u64;
        driver.write_reg(E1000_RDBAL, rx_phys as u32);
        driver.write_reg(E1000_RDBAH, (rx_phys >> 32) as u32);
        driver.write_reg(E1000_RDLEN, (NUM_RX_DESCRIPTORS * DESCRIPTOR_SIZE) as u32);
        driver.write_reg(E1000_RDH, 0);
        driver.write_reg(E1000_RDT, (NUM_RX_DESCRIPTORS - 1) as u32);
        // Enable receive, accept broadcast, and strip the Ethernet CRC.
        driver.write_reg(E1000_RCTL, (1 << 1) | (1 << 15) | (1 << 26));
        fence(Ordering::SeqCst);
        Ok(driver)
    }

    /// The hardware's station MAC address.
    pub fn mac_address(&self) -> [u8; 6] {
        self.mac
    }

    /// Whether the controller currently reports carrier/link.
    pub fn link_up(&self) -> bool {
        self.read_reg(E1000_STATUS) & E1000_STATUS_LINK_UP != 0
    }

    /// Queue one raw Ethernet frame for transmission.
    ///
    /// The frame is copied into a DMA-owned bounce buffer, so callers may pass
    /// any ordinary slice without needing physical-address translation.
    pub fn transmit(&mut self, frame: &[u8]) -> Result<(), Error> {
        if frame.is_empty() || frame.len() > MAX_TX_PACKET_SIZE {
            return Err(Error::InvalidArgument);
        }
        self.clean_tx();

        let index = self.tx_next_to_use;
        if (index + 1) % NUM_TX_DESCRIPTORS == self.tx_next_to_clean {
            return Err(Error::ShouldRetry);
        }
        let buffer_offset = TX_BUFFER_OFFSET + index * TX_BUFFER_SIZE;
        unsafe {
            copy_nonoverlapping(
                frame.as_ptr(),
                self.dma_base.add(buffer_offset),
                frame.len(),
            );
            let descriptor = self.tx_descriptor(index);
            write_volatile(
                core::ptr::addr_of_mut!((*descriptor).addr),
                self.dma.phys() + buffer_offset as u64,
            );
            write_volatile(
                core::ptr::addr_of_mut!((*descriptor).length),
                frame.len() as u16,
            );
            write_volatile(
                core::ptr::addr_of_mut!((*descriptor).command),
                TX_CMD_EOP | TX_CMD_IFCS | TX_CMD_RS,
            );
            write_volatile(core::ptr::addr_of_mut!((*descriptor).status), 0);
        }
        fence(Ordering::Release);
        self.tx_next_to_use = (index + 1) % NUM_TX_DESCRIPTORS;
        self.write_reg(E1000_TDT, self.tx_next_to_use as u32);
        Ok(())
    }

    /// Poll for one frame and copy it into caller-owned storage.
    ///
    /// The descriptor is returned to hardware only after the copy completes.
    /// An undersized output buffer leaves the frame pending for a retry.
    pub fn receive_into(&mut self, out: &mut [u8]) -> Result<Option<usize>, ReceiveError> {
        for _ in 0..NUM_RX_DESCRIPTORS {
            let index = (self.rx_next_to_clean + 1) % NUM_RX_DESCRIPTORS;
            let descriptor = self.rx_descriptor(index);
            let status = unsafe { read_volatile(core::ptr::addr_of!((*descriptor).status)) };
            if status & DESC_DD == 0 {
                return Ok(None);
            }
            fence(Ordering::Acquire);
            let length =
                unsafe { read_volatile(core::ptr::addr_of!((*descriptor).length)) as usize };
            let errors = unsafe { read_volatile(core::ptr::addr_of!((*descriptor).errors)) };
            if errors == 0 && status & RX_STATUS_EOP != 0 && length != 0 && length <= RX_BUFFER_SIZE
            {
                if out.len() < length {
                    return Err(ReceiveError::BufferTooSmall { needed: length });
                }
                let buffer_offset = RX_BUFFER_OFFSET + index * RX_BUFFER_SIZE;
                unsafe {
                    copy_nonoverlapping(self.dma_base.add(buffer_offset), out.as_mut_ptr(), length)
                };
                self.rearm_rx(index, descriptor);
                return Ok(Some(length));
            }
            self.rearm_rx(index, descriptor);
        }
        Ok(None)
    }

    fn read_mac(&self) -> Result<[u8; 6], Error> {
        let low = self.read_reg(E1000_RAL0);
        let high = self.read_reg(E1000_RAH0);
        if high & (1 << 31) == 0 {
            return Err(Error::BadState);
        }
        Ok([
            low as u8,
            (low >> 8) as u8,
            (low >> 16) as u8,
            (low >> 24) as u8,
            high as u8,
            (high >> 8) as u8,
        ])
    }

    fn clean_tx(&mut self) {
        while self.tx_next_to_clean != self.tx_next_to_use {
            let descriptor = self.tx_descriptor(self.tx_next_to_clean);
            let status = unsafe { read_volatile(core::ptr::addr_of!((*descriptor).status)) };
            if status & DESC_DD == 0 {
                break;
            }
            fence(Ordering::Acquire);
            self.tx_next_to_clean = (self.tx_next_to_clean + 1) % NUM_TX_DESCRIPTORS;
        }
    }

    fn rearm_rx(&mut self, index: usize, descriptor: *mut RxDescriptor) {
        unsafe {
            write_volatile(core::ptr::addr_of_mut!((*descriptor).status), 0);
            write_volatile(core::ptr::addr_of_mut!((*descriptor).errors), 0);
            write_volatile(core::ptr::addr_of_mut!((*descriptor).length), 0);
        }
        fence(Ordering::Release);
        self.write_reg(E1000_RDT, index as u32);
        self.rx_next_to_clean = index;
    }

    fn tx_descriptor(&self, index: usize) -> *mut TxDescriptor {
        unsafe {
            self.dma_base
                .add(TX_RING_OFFSET)
                .cast::<TxDescriptor>()
                .add(index)
        }
    }

    fn rx_descriptor(&self, index: usize) -> *mut RxDescriptor {
        unsafe {
            self.dma_base
                .add(RX_RING_OFFSET)
                .cast::<RxDescriptor>()
                .add(index)
        }
    }

    fn write_reg(&self, offset: usize, value: u32) {
        unsafe { write_volatile((self.mmio_base + offset) as *mut u32, value) };
    }

    fn read_reg(&self, offset: usize) -> u32 {
        unsafe { read_volatile((self.mmio_base + offset) as *const u32) }
    }
}

impl Drop for E1000Driver {
    fn drop(&mut self) {
        self.write_reg(E1000_IMC, u32::MAX);
        self.write_reg(E1000_RCTL, 0);
        self.write_reg(E1000_TCTL, 0);
        fence(Ordering::SeqCst);
    }
}
