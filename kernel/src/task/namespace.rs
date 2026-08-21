//! Per-process resource namespaces.
//!
//! A namespace maps *names* to *capabilities*. It is how a process discovers
//! anything it was not handed directly at launch: `"/svc/fs"`, `"/dev/block0"`,
//! `"/proc/self"` are all just keys in a table the launcher populated.
//!
//! Two processes with different namespaces genuinely see different systems.
//! There is no global filesystem root to fall back to, because there is no
//! global anything -- a name that is absent from your namespace names nothing,
//! and no amount of privilege makes it resolve.

use alloc::collections::BTreeMap;
use alloc::string::{String, ToString};
use alloc::sync::Arc;
use alloc::vec::Vec;

use crate::obj::{Handle, KernelObject, ObjectHeader, ObjectType, Signals};
use crate::sync::IrqSpinLock;

/// Maximum length of a namespace path.
pub const MAX_PATH: usize = 256;
/// Maximum entries in one namespace.
pub const MAX_ENTRIES: usize = 4096;

/// A name-to-capability table.
pub struct Namespace {
    header: ObjectHeader,
    entries: IrqSpinLock<BTreeMap<String, Handle>>,
}

impl Namespace {
    /// An empty namespace: a process that can reach nothing it was not handed.
    pub fn new() -> Arc<Self> {
        Arc::new(Self {
            header: ObjectHeader::new(ObjectType::Namespace, Signals::NONE),
            entries: IrqSpinLock::new(BTreeMap::new()),
        })
    }

    /// Bind `path` to `handle`, replacing any previous binding.
    pub fn bind(&self, path: &str, handle: Handle) -> Result<(), NamespaceError> {
        if path.is_empty() || path.len() > MAX_PATH || !path.starts_with('/') {
            return Err(NamespaceError::BadPath);
        }
        let mut entries = self.entries.lock();
        if entries.len() >= MAX_ENTRIES && !entries.contains_key(path) {
            return Err(NamespaceError::Full);
        }
        entries.insert(path.to_string(), handle);
        Ok(())
    }

    /// Look up `path`.
    pub fn resolve(&self, path: &str) -> Option<Handle> {
        let entries = self.entries.lock();
        if let Some(handle) = entries.get(path) {
            return Some(handle.clone());
        }
        // Longest-prefix match, so binding `/svc` makes `/svc/fs` resolve to
        // the `/svc` directory capability, which the holder can then walk.
        entries
            .range(..=path.to_string())
            .next_back()
            .filter(|(key, _)| path.starts_with(key.as_str()) && path.as_bytes().get(key.len()) == Some(&b'/'))
            .map(|(_, handle)| handle.clone())
    }

    /// Remove a binding.
    pub fn unbind(&self, path: &str) -> Option<Handle> {
        self.entries.lock().remove(path)
    }

    /// Every bound path, for introspection.
    pub fn paths(&self) -> Vec<String> {
        self.entries.lock().keys().cloned().collect()
    }

    /// Number of bindings.
    pub fn len(&self) -> usize {
        self.entries.lock().len()
    }

    /// Whether the namespace is empty.
    pub fn is_empty(&self) -> bool {
        self.entries.lock().is_empty()
    }

    /// Drop every binding.
    pub fn clear(&self) {
        self.entries.lock().clear();
    }

    /// Create a child namespace containing only the paths in `allow`, with
    /// rights reduced to `rights`.
    ///
    /// This is how a process hands a *narrower* view of the world to a child:
    /// the child cannot name what was not copied, and cannot exceed the rights
    /// the copy carries.
    pub fn derive(&self, allow: &[&str], rights: crate::obj::Rights) -> Result<Arc<Self>, NamespaceError> {
        let child = Self::new();
        let entries = self.entries.lock();
        for path in allow {
            if let Some(handle) = entries.get(*path) {
                let derived = handle.derive(rights).map_err(|_| NamespaceError::Denied)?;
                child.bind(path, derived)?;
            }
        }
        Ok(child)
    }
}

impl KernelObject for Namespace {
    fn header(&self) -> &ObjectHeader {
        &self.header
    }

    fn as_any(&self) -> &dyn core::any::Any {
        self
    }
}

/// Errors from namespace operations.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum NamespaceError {
    /// Path is empty, too long, or not absolute.
    BadPath,
    /// The namespace is at [`MAX_ENTRIES`].
    Full,
    /// The source handle could not be derived (revoked or too deep).
    Denied,
}
