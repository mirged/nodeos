//! Host tests run the real FAT implementation against an in-memory block device.
extern crate alloc;
extern crate self as libnode;
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Error {
    AccessDenied,
    AlreadyExists,
    BadState,
    InvalidArgument,
    NoMemory,
    NoSpace,
    NotFound,
    NotSupported,
}
mod virtio {
    use crate::Error;
    use std::{cell::RefCell, rc::Rc};
    pub const SECTOR_SIZE: usize = 512;
    pub const MAX_TRANSFER: usize = 64 * 1024;
    pub struct VirtioBlk {
        pub bytes: Vec<u8>,
        pub reads: Rc<RefCell<Vec<(u64, usize)>>>,
    }
    impl VirtioBlk {
        pub fn read(&mut self, sector: u64, out: &mut [u8]) -> Result<(), Error> {
            assert!(!out.is_empty() && out.len() <= MAX_TRANSFER && out.len() % SECTOR_SIZE == 0);
            self.reads.borrow_mut().push((sector, out.len()));
            let start = sector as usize * 512;
            out.copy_from_slice(
                self.bytes
                    .get(start..start + out.len())
                    .ok_or(Error::BadState)?,
            );
            Ok(())
        }
        pub fn write(&mut self, sector: u64, bytes: &[u8]) -> Result<(), Error> {
            let start = sector as usize * 512;
            self.bytes
                .get_mut(start..start + bytes.len())
                .ok_or(Error::BadState)?
                .copy_from_slice(bytes);
            Ok(())
        }
        pub fn requests(&self) -> u64 {
            self.reads.borrow().len() as u64
        }
    }
}
#[path = "../src/fat.rs"]
#[allow(dead_code)]
mod fat;
use std::{cell::RefCell, rc::Rc};
fn volume(
    chain: &[u16],
) -> (
    fat::Fat,
    fat::Entry,
    Rc<RefCell<Vec<(u64, usize)>>>,
    Vec<u8>,
) {
    let mut bytes = vec![0; 8192 * 512];
    bytes[11..13].copy_from_slice(&512u16.to_le_bytes());
    bytes[13] = 1;
    bytes[14..16].copy_from_slice(&1u16.to_le_bytes());
    bytes[16] = 1;
    bytes[17..19].copy_from_slice(&16u16.to_le_bytes());
    bytes[19..21].copy_from_slice(&8192u16.to_le_bytes());
    bytes[22..24].copy_from_slice(&32u16.to_le_bytes());
    let mut expected = Vec::new();
    for (index, &cluster) in chain.iter().enumerate() {
        let next = chain.get(index + 1).copied().unwrap_or(0xffff);
        bytes[512 + cluster as usize * 2..514 + cluster as usize * 2]
            .copy_from_slice(&next.to_le_bytes());
        let sector = 34 + (cluster as usize - 2);
        for pos in 0..512 {
            let value = ((index * 512 + pos) * 31 % 251) as u8;
            bytes[sector * 512 + pos] = value;
            expected.push(value);
        }
    }
    // A real root entry lets truncate/write exercise the directory update too.
    bytes[33 * 512..33 * 512 + 11].copy_from_slice(b"TEST    BIN");
    bytes[33 * 512 + 26..33 * 512 + 28].copy_from_slice(&chain[0].to_le_bytes());
    bytes[33 * 512 + 28..33 * 512 + 32].copy_from_slice(&(expected.len() as u32).to_le_bytes());
    let entry = fat::Entry {
        cluster: chain[0] as u32,
        size: expected.len() as u32,
        attr: 0,
        record_sector: 33,
        record_offset: 0,
    };
    let reads = Rc::new(RefCell::new(Vec::new()));
    let disk = virtio::VirtioBlk {
        bytes,
        reads: reads.clone(),
    };
    (fat::Fat::mount(disk).unwrap(), entry, reads, expected)
}
#[test]
fn contiguous_reads_are_batched() {
    let chain: Vec<u16> = (2..258).collect();
    let (mut fs, entry, reads, expected) = volume(&chain);
    for offset in (0..expected.len()).step_by(32768) {
        assert_eq!(
            fs.read(&entry, offset as u64, 32768).unwrap(),
            expected[offset..offset + 32768]
        );
    }
    let data_reads = reads
        .borrow()
        .iter()
        .filter(|(sector, _)| *sector >= 34)
        .count();
    assert_eq!(data_reads, 4, "one data request per 32 KiB, not per sector");
    let fat_reads = reads
        .borrow()
        .iter()
        .filter(|(sector, _)| (1..33).contains(sector))
        .count();
    assert_eq!(
        fat_reads, 2,
        "read each FAT sector once across sequential requests"
    );
}
#[test]
fn fragmented_unaligned_random_and_eof_reads() {
    let (mut fs, mut entry, _, expected) = volume(&[2, 17, 5, 300, 9, 600]);
    for (offset, count) in [
        (7, 2000),
        (510, 1029),
        (1700, 333),
        (0, 50),
        (3000, 800),
        (3072, 20),
        (100, 0),
    ] {
        let end = (offset + count).min(expected.len());
        assert_eq!(
            fs.read(&entry, offset as u64, count).unwrap(),
            expected[offset.min(expected.len())..end]
        );
    }
    entry.size = 777;
    assert_eq!(fs.read(&entry, 700, 500).unwrap(), expected[700..777]);
}
#[test]
fn modifying_chain_invalidates_read_index() {
    let (mut fs, mut entry, _, expected) = volume(&[2, 17, 5, 300]);
    assert_eq!(fs.read(&entry, 0, 2048).unwrap(), expected);
    fs.truncate(&mut entry, 512).unwrap();
    let replacement = vec![0x5a; 1536];
    fs.write(&mut entry, 512, &replacement).unwrap();
    assert_eq!(fs.read(&entry, 512, 1536).unwrap(), replacement);
    assert_eq!(fs.read(&entry, 0, 512).unwrap(), expected[..512]);
}
