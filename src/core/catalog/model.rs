use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, BTreeSet};

pub(crate) const CATALOG_SCHEMA_VERSION: u32 = 1;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub(crate) enum CatalogState {
    #[default]
    Ready,
    Degraded,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct Catalog {
    pub(crate) schema_version: u32,
    pub(crate) indexed_backup_count: u64,
    pub(crate) latest_indexed_backup: Option<String>,
    pub(crate) latest_indexed_timestamp: Option<u64>,
    pub(crate) state: CatalogState,
    #[serde(default)]
    pub(crate) pending_backups: Vec<PendingCatalogBackup>,
    /// A bounded replay guard for concurrent writers. File history remains
    /// unbounded only when content actually changes.
    #[serde(default)]
    pub(crate) recently_indexed_backups: Vec<String>,
}

impl Default for Catalog {
    fn default() -> Self {
        Self {
            schema_version: CATALOG_SCHEMA_VERSION,
            indexed_backup_count: 0,
            latest_indexed_backup: None,
            latest_indexed_timestamp: None,
            state: CatalogState::Ready,
            pending_backups: Vec::new(),
            recently_indexed_backups: Vec::new(),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct PendingCatalogBackup {
    pub(crate) backup_hash: String,
    pub(crate) timestamp: u64,
    pub(crate) parent_hash: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct EntryShard {
    #[serde(default)]
    pub(crate) entries: BTreeMap<String, EntryHistory>,
}

impl Default for EntryShard {
    fn default() -> Self {
        Self {
            entries: BTreeMap::new(),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct EntryHistory {
    pub(crate) entry_id: String,
    pub(crate) path: String,
    pub(crate) lookup_path: String,
    pub(crate) parent_directory_id: String,
    pub(crate) name: String,
    pub(crate) first_seen_backup: String,
    pub(crate) first_seen_timestamp: u64,
    pub(crate) last_seen_backup: String,
    pub(crate) last_seen_timestamp: u64,
    pub(crate) exists_in_latest_indexed_snapshot: bool,
    pub(crate) latest_restorable_backup: Option<String>,
    #[serde(default)]
    pub(crate) last_change_backup: Option<String>,
    pub(crate) revisions: Vec<FileRevision>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct FileRevision {
    pub(crate) revision_id: String,
    pub(crate) present_from_backup: String,
    pub(crate) present_from_timestamp: u64,
    pub(crate) present_until_backup: Option<String>,
    pub(crate) present_until_timestamp: Option<u64>,
    pub(crate) content_hash: String,
    pub(crate) size: u64,
    #[serde(default)]
    pub(crate) content_type: String,
    #[serde(default)]
    pub(crate) permissions: u32,
    #[serde(default)]
    pub(crate) latest_restorable_backup: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum DirectoryChildKind {
    File,
    Directory,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct DirectoryChild {
    pub(crate) name: String,
    pub(crate) kind: DirectoryChildKind,
    pub(crate) target_id: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct DirectoryChildren {
    pub(crate) directory_id: String,
    pub(crate) path: String,
    #[serde(default)]
    pub(crate) children: BTreeMap<String, DirectoryChild>,
    #[serde(default)]
    pub(crate) current_entry_ids: BTreeSet<String>,
}

impl DirectoryChildren {
    pub(crate) fn new(directory_id: String, path: String) -> Self {
        Self {
            directory_id,
            path,
            children: BTreeMap::new(),
            current_entry_ids: BTreeSet::new(),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct ChildrenShard {
    #[serde(default)]
    pub(crate) directories: BTreeMap<String, DirectoryChildren>,
}

impl Default for ChildrenShard {
    fn default() -> Self {
        Self {
            directories: BTreeMap::new(),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct TokenPosting {
    pub(crate) token: String,
    #[serde(default)]
    pub(crate) entry_ids: BTreeSet<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct TokenShard {
    #[serde(default)]
    pub(crate) postings: BTreeMap<String, TokenPosting>,
}

impl Default for TokenShard {
    fn default() -> Self {
        Self {
            postings: BTreeMap::new(),
        }
    }
}
