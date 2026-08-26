use crate::fs::FS;
use std::collections::{BTreeSet, HashMap};
use std::sync::Arc;

use super::model::{
    Catalog, CatalogState, ChildrenShard, DirectoryChildKind, EntryHistory, EntryShard, TokenShard,
};
use super::normalize::{
    directory_id, entry_id, normalize_file_path, normalize_relative_path, shard_id,
};
use super::storage::{
    children_shard_path, entry_shard_path, load_backup_manifest, read_catalog, read_object,
    token_shard_path,
};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum CatalogEntryScope {
    Current,
    AllHistory,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct CatalogStatus {
    pub(crate) state: CatalogState,
    pub(crate) indexed_backup_count: u64,
    pub(crate) latest_indexed_backup: Option<String>,
    pub(crate) latest_indexed_timestamp: Option<u64>,
    pub(crate) pending_backups: usize,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct CurrentSnapshot {
    pub(crate) backup_hash: String,
    pub(crate) current_entry_ids: BTreeSet<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct CatalogPage<T> {
    pub(crate) items: Vec<T>,
    pub(crate) next_cursor: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct CatalogEntrySummary {
    pub(crate) entry_id: String,
    pub(crate) path: String,
    pub(crate) exists_in_latest_indexed_snapshot: bool,
    pub(crate) latest_restorable_backup: Option<String>,
    pub(crate) latest_revision_id: Option<String>,
    pub(crate) size: Option<u64>,
    pub(crate) content_type: Option<String>,
    pub(crate) permissions: Option<u32>,
    pub(crate) newest_revision_timestamp: u64,
    pub(crate) revision_count: usize,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct DirectoryChildSummary {
    pub(crate) name: String,
    pub(crate) kind: DirectoryChildKind,
    pub(crate) target_id: String,
    pub(crate) exists_in_latest_indexed_snapshot: bool,
}

pub(crate) async fn read_catalog_status(
    fs: Arc<dyn FS>,
    key: String,
    password: Option<String>,
) -> Result<Option<CatalogStatus>, String> {
    let catalog = read_catalog(&fs, &key, password.as_deref()).await?;
    Ok(catalog.map(|catalog| status_from_catalog(&catalog.value)))
}

/// Loads the current file IDs from the latest parentless snapshot.
///
/// Parentless backups are complete snapshots. Reading their manifest lets
/// read-only commands correct a stale catalog in memory without downloading
/// any file chunks or changing repository data. Parent-based snapshots remain
/// fully served by the incremental catalog index.
pub(crate) async fn load_latest_parentless_snapshot(
    fs: Arc<dyn FS>,
    key: String,
    password: Option<String>,
) -> Result<Option<CurrentSnapshot>, String> {
    let Some(catalog) = read_catalog(&fs, &key, password.as_deref()).await? else {
        return Ok(None);
    };
    let Some(backup_hash) = catalog.value.latest_indexed_backup else {
        return Ok(None);
    };

    let backup = match load_backup_manifest(&fs, &key, password.as_deref(), &backup_hash).await {
        Ok(backup) => backup,
        Err(error) if error.contains("not found") || error.contains("is empty") => {
            // Keep the catalog fallback available for repositories that do not
            // retain a manifest for an older indexed backup.
            return Ok(None);
        }
        Err(error) => return Err(error),
    };
    if !backup.parents.is_empty() {
        return Ok(None);
    }

    let mut current_entry_ids = BTreeSet::new();
    for raw_path in backup.tree.keys() {
        let path = normalize_file_path(raw_path)?;
        current_entry_ids.insert(entry_id(&path));
    }

    Ok(Some(CurrentSnapshot {
        backup_hash,
        current_entry_ids,
    }))
}

/// Returns the paths that the catalog currently considers present.
///
/// A full snapshot can be created without a parent manifest. In that case the
/// catalog updater uses this metadata-only view as the previous snapshot when
/// it needs to record paths that disappeared from the new manifest.
pub(crate) async fn list_current_entry_paths(
    fs: Arc<dyn FS>,
    key: String,
    password: Option<String>,
) -> Result<BTreeSet<String>, String> {
    let mut shard_paths = fs
        .list_files(&format!("{}/{}/entries", key, super::storage::CATALOG_ROOT))
        .await
        .map_err(|error| format!("Failed to list catalog entry shards: {}", error))?;
    shard_paths.sort();
    shard_paths.dedup();

    let mut paths = BTreeSet::new();
    for shard_path in shard_paths {
        let Some(shard_data) =
            read_object::<EntryShard>(&fs, &shard_path, password.as_deref(), "catalog entry shard")
                .await?
        else {
            continue;
        };

        for entry in shard_data
            .value
            .entries
            .values()
            .filter(|entry| entry.exists_in_latest_indexed_snapshot)
        {
            paths.insert(normalize_file_path(&entry.path)?);
        }
    }

    Ok(paths)
}

pub(crate) async fn get_entry_history(
    fs: Arc<dyn FS>,
    key: String,
    password: Option<String>,
    path: &str,
) -> Result<Option<EntryHistory>, String> {
    get_entry_history_with_snapshot(fs, key, password, path, None).await
}

pub(crate) async fn get_entry_history_with_snapshot(
    fs: Arc<dyn FS>,
    key: String,
    password: Option<String>,
    path: &str,
    current_snapshot: Option<&CurrentSnapshot>,
) -> Result<Option<EntryHistory>, String> {
    let normalized = normalize_file_path(path)?;
    let Some(mut entry) =
        find_entry_by_lookup_path(&fs, &key, password.as_deref(), &normalized).await?
    else {
        return Ok(None);
    };

    if let Some(current_snapshot) = current_snapshot {
        entry.exists_in_latest_indexed_snapshot =
            current_snapshot.current_entry_ids.contains(&entry.entry_id);
    }

    if entry.exists_in_latest_indexed_snapshot
        && let Some(catalog) = read_catalog(&fs, &key, password.as_deref()).await?
    {
        if let Some(latest_backup) = catalog.value.latest_indexed_backup {
            entry.last_seen_backup = latest_backup.clone();
            entry.latest_restorable_backup = Some(latest_backup);
        }
        if let Some(latest_timestamp) = catalog.value.latest_indexed_timestamp {
            entry.last_seen_timestamp = latest_timestamp;
        }
    }

    Ok(Some(entry))
}

pub(crate) async fn list_directory_children(
    fs: Arc<dyn FS>,
    key: String,
    password: Option<String>,
    path: &str,
    scope: CatalogEntryScope,
    cursor: Option<&str>,
    limit: usize,
) -> Result<CatalogPage<DirectoryChildSummary>, String> {
    list_directory_children_with_snapshot(fs, key, password, path, scope, cursor, limit, None).await
}

pub(crate) async fn list_directory_children_with_snapshot(
    fs: Arc<dyn FS>,
    key: String,
    password: Option<String>,
    path: &str,
    scope: CatalogEntryScope,
    cursor: Option<&str>,
    limit: usize,
    current_snapshot: Option<&CurrentSnapshot>,
) -> Result<CatalogPage<DirectoryChildSummary>, String> {
    let normalized = normalize_relative_path(path)?;
    let Some(directory) =
        find_directory_by_lookup_path(&fs, &key, password.as_deref(), &normalized).await?
    else {
        return Ok(CatalogPage {
            items: Vec::new(),
            next_cursor: None,
        });
    };

    let limit = limit.max(1);
    let mut entry_shard_cache = HashMap::<String, EntryShard>::new();
    let mut items = Vec::with_capacity(limit);
    let mut has_more = false;

    for (name, child) in &directory.children {
        if cursor.is_some_and(|cursor| name.as_str() <= cursor) {
            continue;
        }

        let current = child_is_current(
            &fs,
            &key,
            password.as_deref(),
            child,
            current_snapshot,
            &mut entry_shard_cache,
        )
        .await?;

        if scope == CatalogEntryScope::Current && !current {
            continue;
        }

        if items.len() == limit {
            has_more = true;
            break;
        }

        items.push(DirectoryChildSummary {
            name: name.clone(),
            kind: child.kind,
            target_id: child.target_id.clone(),
            exists_in_latest_indexed_snapshot: current,
        });
    }

    let next_cursor = if has_more {
        items.last().map(|item| item.name.clone())
    } else {
        None
    };

    Ok(CatalogPage { items, next_cursor })
}

pub(crate) async fn directory_exists(
    fs: Arc<dyn FS>,
    key: String,
    password: Option<String>,
    path: &str,
) -> Result<bool, String> {
    let normalized = normalize_relative_path(path)?;
    Ok(
        find_directory_by_lookup_path(&fs, &key, password.as_deref(), &normalized)
            .await?
            .is_some(),
    )
}

async fn find_entry_by_lookup_path(
    fs: &Arc<dyn FS>,
    key: &str,
    password: Option<&str>,
    path: &str,
) -> Result<Option<EntryHistory>, String> {
    let requested_lookup = super::normalize::lookup_path(path);
    let exact_id = entry_id(path);
    let exact_shard = shard_id(&exact_id);

    if let Some(shard_data) = read_object::<EntryShard>(
        fs,
        &entry_shard_path(key, &exact_shard),
        password,
        "catalog entry shard",
    )
    .await?
    {
        if let Some(entry) = shard_data.value.entries.get(&exact_id) {
            return Ok(Some(entry.clone()));
        }
    }

    let mut shard_paths = fs
        .list_files(&format!("{}/{}/entries", key, super::storage::CATALOG_ROOT))
        .await
        .map_err(|error| format!("Failed to list catalog entry shards: {}", error))?;
    shard_paths.sort();
    shard_paths.dedup();

    let mut matches = Vec::new();
    for shard_path in shard_paths {
        let Some(shard_data) =
            read_object::<EntryShard>(fs, &shard_path, password, "catalog entry shard").await?
        else {
            continue;
        };
        matches.extend(
            shard_data
                .value
                .entries
                .values()
                .filter(|entry| entry.lookup_path == requested_lookup)
                .cloned(),
        );
    }

    matches.sort_by(|left, right| left.path.cmp(&right.path));
    Ok(matches.into_iter().next())
}

async fn find_directory_by_lookup_path(
    fs: &Arc<dyn FS>,
    key: &str,
    password: Option<&str>,
    path: &str,
) -> Result<Option<super::model::DirectoryChildren>, String> {
    let requested_lookup = super::normalize::lookup_path(path);
    let exact_id = directory_id(path);
    let exact_shard = shard_id(&exact_id);

    if let Some(shard_data) = read_object::<ChildrenShard>(
        fs,
        &children_shard_path(key, &exact_shard),
        password,
        "catalog children shard",
    )
    .await?
        && let Some(directory) = shard_data.value.directories.get(&exact_id)
    {
        return Ok(Some(directory.clone()));
    }

    let mut shard_paths = fs
        .list_files(&format!(
            "{}/{}/children",
            key,
            super::storage::CATALOG_ROOT
        ))
        .await
        .map_err(|error| format!("Failed to list catalog children shards: {}", error))?;
    shard_paths.sort();
    shard_paths.dedup();

    let mut matches = Vec::new();
    for shard_path in shard_paths {
        let Some(shard_data) =
            read_object::<ChildrenShard>(fs, &shard_path, password, "catalog children shard")
                .await?
        else {
            continue;
        };
        matches.extend(
            shard_data
                .value
                .directories
                .values()
                .filter(|directory| {
                    super::normalize::lookup_path(&directory.path) == requested_lookup
                })
                .cloned(),
        );
    }

    matches.sort_by(|left, right| left.path.cmp(&right.path));
    Ok(matches.into_iter().next())
}

pub(crate) async fn lookup_entries_by_tokens(
    fs: Arc<dyn FS>,
    key: String,
    password: Option<String>,
    tokens: &[String],
    scope: CatalogEntryScope,
    cursor: Option<&str>,
    limit: usize,
) -> Result<CatalogPage<CatalogEntrySummary>, String> {
    lookup_entries_by_tokens_with_snapshot(fs, key, password, tokens, scope, cursor, limit, None)
        .await
}

pub(crate) async fn lookup_entries_by_tokens_with_snapshot(
    fs: Arc<dyn FS>,
    key: String,
    password: Option<String>,
    tokens: &[String],
    scope: CatalogEntryScope,
    cursor: Option<&str>,
    limit: usize,
    current_snapshot: Option<&CurrentSnapshot>,
) -> Result<CatalogPage<CatalogEntrySummary>, String> {
    let latest_indexed_backup = read_catalog(&fs, &key, password.as_deref())
        .await?
        .and_then(|catalog| catalog.value.latest_indexed_backup);

    let mut normalized_tokens = tokens
        .iter()
        .flat_map(|token| super::normalize::path_tokens(token))
        .collect::<Vec<_>>();
    normalized_tokens.sort();
    normalized_tokens.dedup();

    if normalized_tokens.is_empty() {
        return Ok(CatalogPage {
            items: Vec::new(),
            next_cursor: None,
        });
    }

    let mut matching_ids: Option<BTreeSet<String>> = None;
    for token in normalized_tokens {
        let shard = shard_id(&token);
        let Some(shard_data) = read_object::<TokenShard>(
            &fs,
            &token_shard_path(&key, &shard),
            password.as_deref(),
            "catalog token shard",
        )
        .await?
        else {
            return Ok(CatalogPage {
                items: Vec::new(),
                next_cursor: None,
            });
        };

        let Some(posting) = shard_data.value.postings.get(&token) else {
            return Ok(CatalogPage {
                items: Vec::new(),
                next_cursor: None,
            });
        };

        matching_ids = Some(match matching_ids {
            Some(current) => current.intersection(&posting.entry_ids).cloned().collect(),
            None => posting.entry_ids.clone(),
        });
    }

    let mut results = Vec::new();
    let mut entry_cache = HashMap::<String, EntryShard>::new();
    for id in matching_ids.unwrap_or_default() {
        let entry_shard = shard_id(&id);
        let shard_data = if let Some(shard_data) = entry_cache.get(&entry_shard) {
            shard_data.clone()
        } else {
            let Some(shard_data) = read_object::<EntryShard>(
                &fs,
                &entry_shard_path(&key, &entry_shard),
                password.as_deref(),
                "catalog entry shard",
            )
            .await?
            .map(|shard| shard.value) else {
                continue;
            };
            entry_cache.insert(entry_shard, shard_data.clone());
            shard_data
        };

        let Some(entry) = shard_data.entries.get(&id) else {
            continue;
        };
        let exists_in_latest_indexed_snapshot = current_snapshot
            .map_or(entry.exists_in_latest_indexed_snapshot, |snapshot| {
                snapshot.current_entry_ids.contains(&entry.entry_id)
            });
        if scope == CatalogEntryScope::Current && !exists_in_latest_indexed_snapshot {
            continue;
        }

        let latest_revision = entry
            .revisions
            .iter()
            .rev()
            .find(|revision| revision.latest_restorable_backup.is_some());

        results.push(CatalogEntrySummary {
            entry_id: entry.entry_id.clone(),
            path: entry.path.clone(),
            exists_in_latest_indexed_snapshot,
            latest_restorable_backup: if exists_in_latest_indexed_snapshot {
                current_snapshot
                    .map(|snapshot| snapshot.backup_hash.clone())
                    .or_else(|| latest_indexed_backup.clone())
                    .or_else(|| entry.latest_restorable_backup.clone())
            } else {
                entry.latest_restorable_backup.clone()
            },
            latest_revision_id: latest_revision.map(|revision| revision.revision_id.clone()),
            size: latest_revision.map(|revision| revision.size),
            content_type: latest_revision.map(|revision| revision.content_type.clone()),
            permissions: latest_revision.map(|revision| revision.permissions),
            newest_revision_timestamp: entry
                .revisions
                .iter()
                .filter(|revision| revision.latest_restorable_backup.is_some())
                .map(|revision| revision.present_from_timestamp)
                .max()
                .unwrap_or_default(),
            revision_count: entry.revisions.len(),
        });
    }

    results.sort_by(|left, right| {
        left.path
            .to_lowercase()
            .cmp(&right.path.to_lowercase())
            .then_with(|| left.path.cmp(&right.path))
            .then_with(|| left.entry_id.cmp(&right.entry_id))
    });

    let limit = limit.max(1);
    let mut filtered = Vec::with_capacity(limit);
    let mut has_more = false;
    for result in results {
        if cursor.is_some_and(|cursor| result.path.as_str() <= cursor) {
            continue;
        }
        if filtered.len() == limit {
            has_more = true;
            break;
        }
        filtered.push(result);
    }

    let next_cursor = if has_more {
        filtered.last().map(|item| item.path.clone())
    } else {
        None
    };

    Ok(CatalogPage {
        items: filtered,
        next_cursor,
    })
}

pub(crate) async fn collect_entries_by_tokens(
    fs: Arc<dyn FS>,
    key: String,
    password: Option<String>,
    tokens: &[String],
    scope: CatalogEntryScope,
) -> Result<Vec<CatalogEntrySummary>, String> {
    collect_entries_by_tokens_with_snapshot(fs, key, password, tokens, scope, None).await
}

pub(crate) async fn collect_entries_by_tokens_with_snapshot(
    fs: Arc<dyn FS>,
    key: String,
    password: Option<String>,
    tokens: &[String],
    scope: CatalogEntryScope,
    current_snapshot: Option<&CurrentSnapshot>,
) -> Result<Vec<CatalogEntrySummary>, String> {
    const PAGE_SIZE: usize = 256;

    let mut cursor = None;
    let mut entries = Vec::new();
    loop {
        let page = lookup_entries_by_tokens_with_snapshot(
            Arc::clone(&fs),
            key.clone(),
            password.clone(),
            tokens,
            scope,
            cursor.as_deref(),
            PAGE_SIZE,
            current_snapshot,
        )
        .await?;
        let next_cursor = page.next_cursor.clone();
        entries.extend(page.items);

        match next_cursor {
            Some(next) if cursor.as_deref() != Some(next.as_str()) => cursor = Some(next),
            _ => break,
        }
    }

    Ok(entries)
}

fn status_from_catalog(catalog: &Catalog) -> CatalogStatus {
    CatalogStatus {
        state: catalog.state,
        indexed_backup_count: catalog.indexed_backup_count,
        latest_indexed_backup: catalog.latest_indexed_backup.clone(),
        latest_indexed_timestamp: catalog.latest_indexed_timestamp,
        pending_backups: catalog.pending_backups.len(),
    }
}

async fn child_is_current(
    fs: &Arc<dyn FS>,
    key: &str,
    password: Option<&str>,
    child: &super::model::DirectoryChild,
    current_snapshot: Option<&CurrentSnapshot>,
    entry_shard_cache: &mut HashMap<String, EntryShard>,
) -> Result<bool, String> {
    match child.kind {
        DirectoryChildKind::Directory => {
            let shard = shard_id(&child.target_id);
            let Some(shard_data) = read_object::<ChildrenShard>(
                fs,
                &children_shard_path(key, &shard),
                password,
                "catalog children shard",
            )
            .await?
            else {
                return Ok(false);
            };
            if let Some(current_snapshot) = current_snapshot {
                return Ok(shard_data
                    .value
                    .directories
                    .get(&child.target_id)
                    .is_some_and(|directory| {
                        directory
                            .current_entry_ids
                            .iter()
                            .any(|id| current_snapshot.current_entry_ids.contains(id))
                    }));
            }
            Ok(shard_data
                .value
                .directories
                .get(&child.target_id)
                .is_some_and(|directory| !directory.current_entry_ids.is_empty()))
        }
        DirectoryChildKind::File => {
            let shard = shard_id(&child.target_id);
            if !entry_shard_cache.contains_key(&shard) {
                let Some(shard_data) = read_object::<EntryShard>(
                    fs,
                    &entry_shard_path(key, &shard),
                    password,
                    "catalog entry shard",
                )
                .await?
                .map(|shard| shard.value) else {
                    return Ok(false);
                };
                entry_shard_cache.insert(shard.clone(), shard_data);
            }

            if let Some(current_snapshot) = current_snapshot {
                return Ok(current_snapshot
                    .current_entry_ids
                    .contains(&child.target_id));
            }

            Ok(entry_shard_cache
                .get(&shard)
                .and_then(|shard| shard.entries.get(&child.target_id))
                .is_some_and(|entry| entry.exists_in_latest_indexed_snapshot))
        }
    }
}
