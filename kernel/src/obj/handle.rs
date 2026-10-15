//! Process-local handle tables.
//!
//! A [`HandleValue`] is what userspace actually holds: a 32-bit integer whose
//! low bits index a slot and whose high bits are a generation tag. The tag is
//! what makes a closed handle *stay* closed -- reusing the slot bumps the
//! generation, so a stale value fails the lookup instead of silently naming
//! whatever moved in.

use alloc::sync::Arc;
use alloc::vec::Vec;

use crate::sync::IrqSpinLock;

use super::grant::Grant;
use super::{KernelObject, ObjectError, ObjectRef, ObjectType, Rights};

/// Bits of a [`HandleValue`] used for the slot index.
const INDEX_BITS: u32 = 20;
/// Maximum handles per process (about one million).
pub const MAX_HANDLES: usize = 1 << INDEX_BITS;
const INDEX_MASK: u32 = (1 << INDEX_BITS) - 1;

/// A process-local name for a capability.
///
/// Zero is reserved as the invalid handle, so a zeroed structure never
/// accidentally names something.
#[derive(Clone, Copy, PartialEq, Eq, Hash)]
pub struct HandleValue(pub u32);

impl HandleValue {
    /// The invalid handle.
    pub const INVALID: Self = Self(0);

    fn new(index: u32, generation: u32) -> Self {
        // Generation 0 is skipped so that slot 0 generation 0 cannot produce
        // the invalid handle value.
        Self((generation << INDEX_BITS) | (index & INDEX_MASK))
    }

    fn index(self) -> usize {
        (self.0 & INDEX_MASK) as usize
    }

    fn generation(self) -> u32 {
        self.0 >> INDEX_BITS
    }

    /// Whether this is the invalid handle.
    pub fn is_invalid(self) -> bool {
        self.0 == 0
    }
}

impl core::fmt::Debug for HandleValue {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        if self.is_invalid() {
            f.write_str("handle(invalid)")
        } else {
            write!(f, "handle({}:{})", self.index(), self.generation())
        }
    }
}

/// A capability: an object reference, the rights it conveys, and the grant it
/// descends from.
#[derive(Clone)]
pub struct Handle {
    object: ObjectRef,
    rights: Rights,
    grant: Arc<Grant>,
}

impl Handle {
    /// Create a root handle to a newly created object.
    pub fn root(object: ObjectRef, rights: Rights) -> Self {
        Self { object, rights, grant: Grant::root() }
    }

    /// The object this handle names.
    pub fn object(&self) -> &ObjectRef {
        &self.object
    }

    /// The rights this handle conveys.
    pub fn rights(&self) -> Rights {
        self.rights
    }

    /// The grant this handle descends from.
    pub fn grant(&self) -> &Arc<Grant> {
        &self.grant
    }

    /// Whether the authority behind this handle is still live.
    pub fn is_live(&self) -> bool {
        self.grant.is_live()
    }

    /// Derive a new handle with `rights` (which are intersected with this
    /// handle's, never widened).
    pub fn derive(&self, rights: Rights) -> Result<Self, ObjectError> {
        if !self.is_live() {
            return Err(ObjectError::Revoked);
        }
        let grant = self.grant.derive().ok_or(ObjectError::GrantTooDeep)?;
        Ok(Self { object: self.object.clone(), rights: self.rights.and(rights), grant })
    }

    /// Check that this handle carries `required`, and is still live.
    pub fn check(&self, required: Rights) -> Result<(), ObjectError> {
        if !self.is_live() {
            return Err(ObjectError::Revoked);
        }
        if !self.rights.contains(required) {
            return Err(ObjectError::AccessDenied);
        }
        Ok(())
    }

    /// Downcast to a concrete object type, checking rights first.
    pub fn as_object<T: KernelObject>(&self, required: Rights) -> Result<&T, ObjectError> {
        self.check(required)?;
        self.object.as_any().downcast_ref::<T>().ok_or(ObjectError::WrongType)
    }

    /// The type of the underlying object.
    pub fn object_type(&self) -> ObjectType {
        self.object.header().kind()
    }
}

struct Slot {
    handle: Option<Handle>,
    generation: u32,
}

/// One process's handle table.
pub struct HandleTable {
    slots: IrqSpinLock<TableInner>,
}

struct TableInner {
    slots: Vec<Slot>,
    free: Vec<u32>,
    live: usize,
}

impl HandleTable {
    /// An empty table.
    pub fn new() -> Self {
        Self { slots: IrqSpinLock::new(TableInner { slots: Vec::new(), free: Vec::new(), live: 0 }) }
    }

    /// Install `handle`, returning its process-local value.
    pub fn insert(&self, handle: Handle) -> Result<HandleValue, ObjectError> {
        let mut inner = self.slots.lock();
        let index = match inner.free.pop() {
            Some(index) => index,
            None => {
                if inner.slots.len() >= MAX_HANDLES {
                    return Err(ObjectError::OutOfHandles);
                }
                inner.slots.push(Slot { handle: None, generation: 1 });
                (inner.slots.len() - 1) as u32
            }
        };
        let slot = &mut inner.slots[index as usize];
        slot.handle = Some(handle);
        let generation = slot.generation;
        inner.live += 1;
        Ok(HandleValue::new(index, generation))
    }

    /// Look up a handle, verifying the generation tag and liveness.
    pub fn get(&self, value: HandleValue) -> Result<Handle, ObjectError> {
        if value.is_invalid() {
            return Err(ObjectError::BadHandle);
        }
        let inner = self.slots.lock();
        let slot = inner.slots.get(value.index()).ok_or(ObjectError::BadHandle)?;
        if slot.generation != value.generation() {
            return Err(ObjectError::BadHandle);
        }
        let handle = slot.handle.as_ref().ok_or(ObjectError::BadHandle)?;
        if !handle.is_live() {
            return Err(ObjectError::Revoked);
        }
        Ok(handle.clone())
    }

    /// Look up a handle and check that it carries `required`.
    pub fn get_checked(&self, value: HandleValue, required: Rights) -> Result<Handle, ObjectError> {
        let handle = self.get(value)?;
        handle.check(required)?;
        Ok(handle)
    }

    /// Take a handle out of the table without treating it as closed.
    ///
    /// Used when a handle is being *transferred* (put into a message, or moved
    /// into a child at `process_start`). The object is still owned by the
    /// handle in flight, so no "last handle closed" hook may run; running it
    /// here is what used to close a channel endpoint in the middle of handing
    /// it to its new owner.
    pub fn take(&self, value: HandleValue) -> Result<Handle, ObjectError> {
        self.detach(value, false)
    }

    /// Remove a handle, returning it. This is a close: the object's
    /// [`KernelObject::on_zero_handles`] hook runs if no other handle to it
    /// remains anywhere.
    pub fn remove(&self, value: HandleValue) -> Result<Handle, ObjectError> {
        self.detach(value, true)
    }

    fn detach(&self, value: HandleValue, notify: bool) -> Result<Handle, ObjectError> {
        if value.is_invalid() {
            return Err(ObjectError::BadHandle);
        }
        let mut inner = self.slots.lock();
        let index = value.index();
        let generation = value.generation();
        {
            let slot = inner.slots.get_mut(index).ok_or(ObjectError::BadHandle)?;
            if slot.generation != generation || slot.handle.is_none() {
                return Err(ObjectError::BadHandle);
            }
        }
        let handle = inner.slots[index].handle.take().unwrap();
        // Bumping the generation is what invalidates every copy of the old
        // handle value; wrapping is fine because a collision needs 4095
        // intervening open/close cycles on the same slot *and* a stale value
        // held across all of them.
        inner.slots[index].generation = inner.slots[index].generation.wrapping_add(1) & 0xFFF;
        if inner.slots[index].generation == 0 {
            inner.slots[index].generation = 1;
        }
        inner.free.push(index as u32);
        inner.live -= 1;
        drop(inner);

        // `handle` is the only reference this removal still holds, so a count
        // of one means it was the last handle to the object. (`<= 2` was an
        // off-by-one that fired the hook while a sibling handle -- a duplicate,
        // or the in-flight copy in a transfer -- was still live.)
        if notify && Arc::strong_count(handle.object()) == 1 {
            handle.object().on_zero_handles();
        }
        Ok(handle)
    }

    /// Duplicate a handle with reduced rights.
    pub fn duplicate(&self, value: HandleValue, rights: Rights) -> Result<HandleValue, ObjectError> {
        let handle = self.get_checked(value, Rights::DUPLICATE)?;
        let derived = handle.derive(rights)?;
        self.insert(derived)
    }

    /// Revoke the grant behind a handle, killing every capability derived from
    /// it anywhere in the system.
    pub fn revoke(&self, value: HandleValue) -> Result<(), ObjectError> {
        let handle = self.get_checked(value, Rights::REVOKE)?;
        handle.grant().revoke();
        Ok(())
    }

    /// Number of live handles.
    pub fn len(&self) -> usize {
        self.slots.lock().live
    }

    /// Whether the table is empty.
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// Visit every live handle. Used by process teardown and by diagnostics.
    pub fn for_each(&self, mut f: impl FnMut(HandleValue, &Handle)) {
        let inner = self.slots.lock();
        for (index, slot) in inner.slots.iter().enumerate() {
            if let Some(handle) = &slot.handle {
                f(HandleValue::new(index as u32, slot.generation), handle);
            }
        }
    }

    /// Drop every handle, running close hooks. Called during process teardown.
    pub fn clear(&self) {
        let drained: Vec<Handle> = {
            let mut inner = self.slots.lock();
            inner.live = 0;
            inner.free.clear();
            let mut out = Vec::new();
            for slot in inner.slots.iter_mut() {
                if let Some(handle) = slot.handle.take() {
                    out.push(handle);
                }
                slot.generation = slot.generation.wrapping_add(1) & 0xFFF;
                if slot.generation == 0 {
                    slot.generation = 1;
                }
            }
            out
        };
        for handle in drained {
            if Arc::strong_count(handle.object()) == 1 {
                handle.object().on_zero_handles();
            }
        }
    }
}

impl Default for HandleTable {
    fn default() -> Self {
        Self::new()
    }
}
