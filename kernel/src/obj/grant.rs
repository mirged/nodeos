//! Capability grants and revocation.
//!
//! Every handle carries a pointer to a *grant node*. When a handle is
//! duplicated or transferred, the new handle's grant is a **child** of the
//! original's. Revoking a grant marks it dead, and a handle is valid only if
//! neither its own grant nor any ancestor has been revoked.
//!
//! That gives NodeOS transitive revocation for free: revoke the grant you
//! handed to a service and every capability that service derived from it --
//! including ones it passed on to third parties -- stops working at the same
//! instant, without the kernel having to track where they went.
//!
//! The cost is a short pointer walk on each handle lookup. Grant chains are
//! bounded by [`MAX_GRANT_DEPTH`] precisely so that cost stays predictable and
//! an adversary cannot build a chain deep enough to make lookups expensive.

use alloc::sync::Arc;
use core::sync::atomic::{AtomicBool, AtomicU64, Ordering};

/// Maximum depth of a grant chain. Deriving past this fails rather than
/// silently making lookups slower for everybody.
pub const MAX_GRANT_DEPTH: u32 = 32;

static NEXT_GRANT_ID: AtomicU64 = AtomicU64::new(1);

/// One node in a grant tree.
pub struct Grant {
    id: u64,
    depth: u32,
    revoked: AtomicBool,
    parent: Option<Arc<Grant>>,
}

impl Grant {
    /// Create a root grant, owing its authority to nothing but the kernel.
    ///
    /// Only object creation makes root grants: if you did not create the
    /// object, your authority over it is always someone else's child.
    pub fn root() -> Arc<Self> {
        Arc::new(Self {
            id: NEXT_GRANT_ID.fetch_add(1, Ordering::Relaxed),
            depth: 0,
            revoked: AtomicBool::new(false),
            parent: None,
        })
    }

    /// Derive a child grant, or `None` if the chain would get too deep or the
    /// parent is already dead.
    pub fn derive(self: &Arc<Self>) -> Option<Arc<Self>> {
        if self.depth + 1 >= MAX_GRANT_DEPTH || !self.is_live() {
            return None;
        }
        Some(Arc::new(Self {
            id: NEXT_GRANT_ID.fetch_add(1, Ordering::Relaxed),
            depth: self.depth + 1,
            revoked: AtomicBool::new(false),
            parent: Some(self.clone()),
        }))
    }

    /// Globally unique identifier, for diagnostics and audit logs.
    pub fn id(&self) -> u64 {
        self.id
    }

    /// Distance from the root of the grant tree.
    pub fn depth(&self) -> u32 {
        self.depth
    }

    /// Revoke this grant and, transitively, everything derived from it.
    pub fn revoke(&self) {
        self.revoked.store(true, Ordering::Release);
    }

    /// Whether this grant and all of its ancestors are still live.
    pub fn is_live(&self) -> bool {
        if self.revoked.load(Ordering::Acquire) {
            return false;
        }
        let mut node = self.parent.as_deref();
        while let Some(grant) = node {
            if grant.revoked.load(Ordering::Acquire) {
                return false;
            }
            node = grant.parent.as_deref();
        }
        true
    }

    /// Whether `ancestor` is this grant or one of its ancestors.
    pub fn descends_from(&self, ancestor: &Arc<Grant>) -> bool {
        if self.id == ancestor.id {
            return true;
        }
        let mut node = self.parent.as_deref();
        while let Some(grant) = node {
            if grant.id == ancestor.id {
                return true;
            }
            node = grant.parent.as_deref();
        }
        false
    }
}

impl core::fmt::Debug for Grant {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("Grant")
            .field("id", &self.id)
            .field("depth", &self.depth)
            .field("live", &self.is_live())
            .finish()
    }
}
