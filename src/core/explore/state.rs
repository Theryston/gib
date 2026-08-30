use crate::core::catalog::{CatalogEntryScope, DirectoryChildKind, EntryHistory, FileRevision};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub(crate) enum ExplorerScope {
    Current,
    AllHistory,
}

impl ExplorerScope {
    pub(crate) fn catalog_scope(self) -> CatalogEntryScope {
        match self {
            Self::Current => CatalogEntryScope::Current,
            Self::AllHistory => CatalogEntryScope::AllHistory,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ExplorerKind {
    File,
    Directory,
}

impl ExplorerKind {
    pub(crate) fn from_catalog(kind: DirectoryChildKind) -> Self {
        match kind {
            DirectoryChildKind::File => Self::File,
            DirectoryChildKind::Directory => Self::Directory,
        }
    }

    pub(crate) fn label(self) -> &'static str {
        match self {
            Self::File => "file",
            Self::Directory => "directory",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ExplorerStatus {
    Current,
    Deleted,
}

impl ExplorerStatus {
    pub(crate) fn label(self) -> &'static str {
        match self {
            Self::Current => "current",
            Self::Deleted => "deleted",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ExplorerEntry {
    pub(crate) entry_id: String,
    pub(crate) path: String,
    pub(crate) name: String,
    pub(crate) kind: ExplorerKind,
    pub(crate) status: ExplorerStatus,
    pub(crate) restorable: bool,
    pub(crate) last_backup: Option<String>,
    pub(crate) latest_revision_id: Option<String>,
    pub(crate) size: Option<u64>,
    pub(crate) content_type: Option<String>,
    pub(crate) permissions: Option<u32>,
    pub(crate) newest_revision_timestamp: Option<u64>,
}

impl ExplorerEntry {
    pub(crate) fn is_file(&self) -> bool {
        self.kind == ExplorerKind::File
    }

    pub(crate) fn from_summary(
        entry_id: String,
        path: String,
        exists_in_latest_snapshot: bool,
        last_backup: Option<String>,
        latest_revision_id: Option<String>,
        size: Option<u64>,
        content_type: Option<String>,
        permissions: Option<u32>,
        newest_revision_timestamp: Option<u64>,
    ) -> Self {
        let name = path.rsplit('/').next().unwrap_or(&path).to_string();
        Self {
            entry_id,
            path,
            name,
            kind: ExplorerKind::File,
            status: if exists_in_latest_snapshot {
                ExplorerStatus::Current
            } else {
                ExplorerStatus::Deleted
            },
            restorable: last_backup.is_some(),
            last_backup,
            latest_revision_id,
            size,
            content_type,
            permissions,
            newest_revision_timestamp,
        }
    }

    pub(crate) fn from_history(history: &EntryHistory) -> Self {
        let latest_revision = latest_restorable_revision(history);
        Self::from_summary(
            history.entry_id.clone(),
            history.path.clone(),
            history.exists_in_latest_indexed_snapshot,
            history.latest_restorable_backup.clone(),
            latest_revision.map(|revision| revision.revision_id.clone()),
            latest_revision.map(|revision| revision.size),
            latest_revision.map(|revision| revision.content_type.clone()),
            latest_revision.map(|revision| revision.permissions),
            latest_revision.map(|revision| revision.present_from_timestamp),
        )
    }
}

fn latest_restorable_revision(entry: &EntryHistory) -> Option<&FileRevision> {
    entry
        .revisions
        .iter()
        .rev()
        .find(|revision| revision.latest_restorable_backup.is_some())
}
