// Modified from clabby/tact@a2de8ae1e0b6ce8d8f0a251a9d681dc430b247aa for Nanocodex2.
// SPDX-License-Identifier: Apache-2.0

//! Memory values rendered and manipulated by the imported TUI.
//!
//! The original definitions live in `tact-memory` 0.6.6. Store implementations and their errors
//! are deliberately absent: this module is the presentation model, not a persistence adapter.

use serde::{Deserialize, Serialize};

/// Stable identity and optimistic-concurrency version of a memory.
#[derive(Clone, Debug, Deserialize, Eq, Hash, PartialEq, Serialize)]
pub(crate) struct MemoryKey {
    /// Positive identifier allocated by the owning store.
    pub(crate) id: i64,
    /// Positive version required for compare-and-swap mutations.
    pub(crate) version: u64,
    /// Owning remote namespace, or `None` for a local memory.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) namespace: Option<String>,
}

impl MemoryKey {
    /// Creates a key owned by a local store.
    pub(crate) const fn local(id: i64, version: u64) -> Self {
        Self {
            id,
            version,
            namespace: None,
        }
    }

    /// Creates a key owned by `namespace` on a remote store.
    pub(crate) fn remote(namespace: String, id: i64, version: u64) -> Self {
        Self {
            id,
            version,
            namespace: Some(namespace),
        }
    }

    /// Returns whether this key belongs to a local store.
    pub(crate) const fn is_local(&self) -> bool {
        self.namespace.is_none()
    }
}

/// Complete durable state of a memory.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub(crate) struct MemoryRecord {
    /// Stable identity and current version.
    pub(crate) key: MemoryKey,
    /// User-authored memory content.
    pub(crate) content: String,
    /// Unix timestamp in milliseconds when this identity was created.
    pub(crate) created_at_ms: i64,
    /// Unix timestamp in milliseconds when this version was written.
    pub(crate) updated_at_ms: i64,
    /// Unix timestamp in milliseconds of the most recent matching scan.
    pub(crate) last_scanned_at_ms: Option<i64>,
    /// Number of matching scans recorded for this version.
    pub(crate) scan_count: u64,
    /// Unix timestamp in milliseconds of the most recent read.
    pub(crate) last_used_at_ms: Option<i64>,
    /// Number of reads recorded for this version.
    pub(crate) use_count: u64,
    /// Expiry time for an unused probationary record, if it remains on probation.
    pub(crate) probation_until_ms: Option<i64>,
}

/// Backend represented by a memory browser result.
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum MemorySource {
    /// Private local storage.
    Local,
    /// Namespaced remote storage.
    Remote,
}

/// Authorization role associated with a remote-memory result.
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum RemoteRole {
    /// May inspect visible memories.
    Reader,
    /// May also mutate the credential's namespace.
    Writer,
}

/// Provenance and authorization shown by the memory browser.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub(crate) struct MemoryAccess {
    /// Selected backend kind.
    pub(crate) source: MemorySource,
    /// Configured namespace for a remote backend.
    pub(crate) namespace: Option<String>,
    /// Server-authorized role for a remote backend.
    pub(crate) role: Option<RemoteRole>,
}

#[cfg(test)]
mod tests {
    use super::{MemoryKey, MemorySource, RemoteRole};

    #[test]
    fn keys_preserve_tact_local_and_remote_identity() {
        let local = MemoryKey::local(7, 3);
        assert!(local.is_local());
        assert_eq!(local.namespace, None);

        let remote = MemoryKey::remote("team".to_owned(), 7, 3);
        assert!(!remote.is_local());
        assert_eq!(remote.namespace.as_deref(), Some("team"));
    }

    #[test]
    fn wire_enum_names_match_tact() {
        assert_eq!(
            serde_json::to_value(MemorySource::Remote).unwrap(),
            "remote"
        );
        assert_eq!(serde_json::to_value(RemoteRole::Writer).unwrap(), "writer");
    }
}
