//! Memory objects.
//!
//! A VMO ("virtual memory object") is a range of pages that exists
//! independently of any address space. It is the *only* way memory is named in
//! NodeOS: there is no `brk`, no anonymous `mmap`, no `/dev/mem`. A process
//! that wants memory asks for a VMO, gets a handle with rights, and maps it.
//!
//! Two properties follow, and both are the point:
//!
//! * **Memory is transferable.** A VMO handle can be sent over a channel, so
//!   "hand this buffer to the filesystem service" is a capability transfer, not
//!   a copy and not a shared global namespace.
//! * **Rights are checked at map time and at fault time.** A handle without
//!   [`Rights::WRITE`] cannot produce a writable mapping, however the mapping
//!   is requested, and a later `protect` cannot widen it.

use alloc::sync::Arc;
use alloc::vec::Vec;

use crate::arch::PAGE_SIZE;
use crate::obj::{KernelObject, ObjectHeader, ObjectType, Rights, Signals};
use crate::sync::IrqSpinLock;

use super::phys::{self, Pfn};

/// Where a VMO's pages come from.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum VmoKind {
    /// Zero-filled pages, committed on demand.
    Anonymous,
    /// A fixed physical range: device MMIO, or a framebuffer.
    Physical,
    /// Pages are supplied by a userspace pager over a channel. Faults on an
    /// uncommitted page block the faulting thread and send a request.
    Paged,
}

/// Errors from memory object operations.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum VmoError {
    /// Offset or length is outside the object.
    OutOfRange,
    /// No physical memory available.
    OutOfMemory,
    /// The object is not resizable.
    NotResizable,
    /// The operation is not valid for this kind of object.
    WrongKind,
}

struct VmoInner {
    /// Size in bytes; always a multiple of [`PAGE_SIZE`].
    size: u64,
    /// One slot per page. `None` means "not yet committed".
    pages: Vec<Option<Pfn>>,
    /// For [`VmoKind::Physical`], the physical base address.
    phys_base: u64,
}

/// A range of memory that can be mapped into address spaces.
pub struct Vmo {
    header: ObjectHeader,
    kind: VmoKind,
    resizable: bool,
    inner: IrqSpinLock<VmoInner>,
}

impl Vmo {
    /// Create an anonymous, zero-filled memory object of `size` bytes.
    ///
    /// Pages are committed lazily: creating a gigabyte-sized VMO costs one
    /// allocation of the page vector, not a gigabyte.
    pub fn new_anonymous(size: u64, resizable: bool) -> Result<Arc<Self>, VmoError> {
        let size = align_up(size);
        let pages = (size / PAGE_SIZE as u64) as usize;
        let mut slots = Vec::new();
        slots.try_reserve_exact(pages).map_err(|_| VmoError::OutOfMemory)?;
        slots.resize(pages, None);
        Ok(Arc::new(Self {
            header: ObjectHeader::new(ObjectType::Vmo, Signals::NONE),
            kind: VmoKind::Anonymous,
            resizable,
            inner: IrqSpinLock::new(VmoInner { size, pages: slots, phys_base: 0 }),
        }))
    }

    /// Wrap a fixed physical range, for device MMIO handed to a userspace
    /// driver.
    ///
    /// # Safety
    /// `phys_base` must name a physical range that no other object claims, and
    /// the caller must have the authority to hand it out (in practice: it holds
    /// a `Device` capability covering the range).
    pub unsafe fn new_physical(phys_base: u64, size: u64) -> Result<Arc<Self>, VmoError> {
        let size = align_up(size);
        let pages = (size / PAGE_SIZE as u64) as usize;
        let mut slots = Vec::new();
        slots.try_reserve_exact(pages).map_err(|_| VmoError::OutOfMemory)?;
        for i in 0..pages {
            slots.push(Some(phys::phys_to_pfn(phys_base + (i * PAGE_SIZE) as u64)));
        }
        Ok(Arc::new(Self {
            header: ObjectHeader::new(ObjectType::Vmo, Signals::NONE),
            kind: VmoKind::Physical,
            resizable: false,
            inner: IrqSpinLock::new(VmoInner { size, pages: slots, phys_base }),
        }))
    }

    /// Create a VMO backed by a userspace pager.
    pub fn new_paged(size: u64) -> Result<Arc<Self>, VmoError> {
        let vmo = Self::new_anonymous(size, false)?;
        // SAFETY-free: `kind` is only read, and `Arc::get_mut` succeeds because
        // this is the sole reference.
        let mut vmo = vmo;
        Arc::get_mut(&mut vmo).expect("fresh Arc is unique").kind = VmoKind::Paged;
        Ok(vmo)
    }

    /// The object's kind.
    pub fn kind(&self) -> VmoKind {
        self.kind
    }

    /// Size in bytes.
    pub fn size(&self) -> u64 {
        self.inner.lock().size
    }

    /// Number of pages currently committed.
    pub fn committed_pages(&self) -> usize {
        self.inner.lock().pages.iter().filter(|p| p.is_some()).count()
    }

    /// Frame backing page `index`, committing it if necessary.
    ///
    /// Returns `None` for a [`VmoKind::Paged`] object whose page has not been
    /// supplied yet; the caller is expected to ask the pager.
    pub fn page(&self, index: usize, commit: bool) -> Result<Option<Pfn>, VmoError> {
        let mut inner = self.inner.lock();
        let slot = inner.pages.get(index).copied().ok_or(VmoError::OutOfRange)?;
        if let Some(pfn) = slot {
            return Ok(Some(pfn));
        }
        if !commit || self.kind == VmoKind::Paged {
            return Ok(None);
        }
        let pfn = phys::alloc_zeroed().ok_or(VmoError::OutOfMemory)?;
        phys::set_flags(pfn, phys::FrameFlags::VMO);
        inner.pages[index] = Some(pfn);
        Ok(Some(pfn))
    }

    /// Install `pfn` as the backing for page `index`. Used by pagers and by the
    /// loader, which writes program segments straight into the object.
    ///
    /// # Safety
    /// `pfn` must be a frame the caller owns and is transferring to this object.
    pub unsafe fn set_page(&self, index: usize, pfn: Pfn) -> Result<(), VmoError> {
        let mut inner = self.inner.lock();
        if index >= inner.pages.len() {
            return Err(VmoError::OutOfRange);
        }
        if let Some(old) = inner.pages[index].replace(pfn) {
            phys::decref(old);
        }
        phys::set_flags(pfn, phys::FrameFlags::VMO);
        self.header.signals().update(Signals::READABLE, Signals::NONE);
        Ok(())
    }

    /// Copy bytes into the object, committing pages as needed.
    pub fn write(&self, offset: u64, data: &[u8]) -> Result<(), VmoError> {
        if offset + data.len() as u64 > self.size() {
            return Err(VmoError::OutOfRange);
        }
        let mut written = 0usize;
        while written < data.len() {
            let position = offset + written as u64;
            let index = (position / PAGE_SIZE as u64) as usize;
            let page_offset = (position % PAGE_SIZE as u64) as usize;
            let chunk = (PAGE_SIZE - page_offset).min(data.len() - written);
            let pfn = self.page(index, true)?.ok_or(VmoError::WrongKind)?;
            let dst = phys::phys_to_virt(phys::pfn_to_phys(pfn)) as *mut u8;
            // SAFETY: `pfn` is a committed page of this object, reachable
            // through the direct map, and `page_offset + chunk <= PAGE_SIZE`.
            unsafe { core::ptr::copy_nonoverlapping(data.as_ptr().add(written), dst.add(page_offset), chunk) };
            written += chunk;
        }
        Ok(())
    }

    /// Copy bytes out of the object. Uncommitted pages read as zero.
    pub fn read(&self, offset: u64, out: &mut [u8]) -> Result<(), VmoError> {
        if offset + out.len() as u64 > self.size() {
            return Err(VmoError::OutOfRange);
        }
        let mut read = 0usize;
        while read < out.len() {
            let position = offset + read as u64;
            let index = (position / PAGE_SIZE as u64) as usize;
            let page_offset = (position % PAGE_SIZE as u64) as usize;
            let chunk = (PAGE_SIZE - page_offset).min(out.len() - read);
            match self.page(index, false)? {
                Some(pfn) => {
                    let src = phys::phys_to_virt(phys::pfn_to_phys(pfn)) as *const u8;
                    // SAFETY: committed page reachable through the direct map.
                    unsafe { core::ptr::copy_nonoverlapping(src.add(page_offset), out.as_mut_ptr().add(read), chunk) };
                }
                None => out[read..read + chunk].fill(0),
            }
            read += chunk;
        }
        Ok(())
    }

    /// Grow or shrink the object.
    pub fn resize(&self, size: u64) -> Result<(), VmoError> {
        if !self.resizable {
            return Err(VmoError::NotResizable);
        }
        let size = align_up(size);
        let mut inner = self.inner.lock();
        let pages = (size / PAGE_SIZE as u64) as usize;
        if pages < inner.pages.len() {
            for slot in inner.pages.drain(pages..) {
                if let Some(pfn) = slot {
                    phys::decref(pfn);
                }
            }
        } else {
            let extra = pages - inner.pages.len();
            inner.pages.try_reserve(extra).map_err(|_| VmoError::OutOfMemory)?;
            inner.pages.resize(pages, None);
        }
        inner.size = size;
        drop(inner);
        self.header.signals().update(Signals::VMO_RESIZED, Signals::NONE);
        Ok(())
    }

    /// Physical base, for [`VmoKind::Physical`] objects.
    pub fn phys_base(&self) -> u64 {
        self.inner.lock().phys_base
    }

    /// Rights a freshly created handle to this object should carry.
    pub fn default_rights(&self) -> Rights {
        match self.kind {
            VmoKind::Physical => Rights::VMO_DEFAULT.without(Rights::RESIZE),
            _ => Rights::VMO_DEFAULT,
        }
    }
}

impl KernelObject for Vmo {
    fn header(&self) -> &ObjectHeader {
        &self.header
    }

    fn as_any(&self) -> &dyn core::any::Any {
        self
    }
}

impl Drop for Vmo {
    fn drop(&mut self) {
        // Physical VMOs do not own their frames; anonymous and paged ones do.
        if self.kind == VmoKind::Physical {
            return;
        }
        let inner = self.inner.get_mut();
        for slot in inner.pages.iter().flatten() {
            phys::decref(*slot);
        }
    }
}

fn align_up(size: u64) -> u64 {
    (size + PAGE_SIZE as u64 - 1) & !(PAGE_SIZE as u64 - 1)
}
