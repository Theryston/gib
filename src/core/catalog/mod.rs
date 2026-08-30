#![allow(dead_code, unused_imports)]

mod model;
mod normalize;
mod query;
mod storage;
mod update;

pub(crate) use model::{
    Catalog, CatalogState, ChildrenShard, DirectoryChild, DirectoryChildKind, DirectoryChildren,
    EntryHistory, EntryShard, FileRevision, PendingCatalogBackup, TokenPosting, TokenShard,
};
pub(crate) use normalize::{
    directory_id, directory_paths, entry_id, file_name, lookup_path, normalize_file_path,
    normalize_relative_path, parent_directory, path_tokens, revision_id, shard_id,
};
pub(crate) use query::{
    CatalogEntryScope, CatalogEntrySummary, CatalogPage, CatalogStatus, CurrentSnapshot,
    DirectoryChildSummary, collect_entries_by_tokens, collect_entries_by_tokens_with_snapshot,
    directory_exists, get_entry_history, get_entry_history_with_snapshot, list_current_entry_paths,
    list_directory_children, list_directory_children_with_snapshot,
    load_latest_parentless_snapshot, lookup_entries_by_tokens,
    lookup_entries_by_tokens_with_snapshot, read_catalog_status,
};
pub(crate) use update::index_backup_after_finalize;

pub(crate) use storage::{load_backup_manifest, mark_catalog_degraded_state};
pub(crate) use update::remove_backup_from_catalog;
