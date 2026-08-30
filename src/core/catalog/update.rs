use crate::core::metadata::{Backup, BackupObject};
use crate::core::metadata::{BackupSummary, ChunkIndex};
use crate::storage::FS;
use futures::stream::{self, StreamExt, TryStreamExt};
use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::sync::Arc;

use super::model::{
    Catalog, CatalogState, ChildrenShard, DirectoryChild, DirectoryChildKind, DirectoryChildren,
    EntryHistory, EntryShard, FileRevision, PendingCatalogBackup, TokenPosting, TokenShard,
};
use super::normalize::{
    directory_id, directory_paths, entry_id, file_name, lookup_path, normalize_file_path,
    normalize_relative_path, parent_directory, path_tokens, revision_id, shard_id,
};
use super::query::list_current_entry_paths;
use super::storage::{
    children_shard_path, empty_children_shard, empty_entry_shard, empty_token_shard,
    entry_shard_path, load_backup_manifest, mark_catalog_degraded, read_catalog, read_object,
    token_shard_path, update_object,
};

const MAX_PENDING_RECONCILIATIONS: usize = 16;
const MAX_RECENTLY_INDEXED_BACKUPS: usize = 64;
const MAX_CONCURRENT_CATALOG_SHARD_UPDATES: usize = 16;

/// Applies one finalized backup to the historical catalog.
///
/// The backup manifest and normal repository indexes are written before this
/// function is called. A failure is intentionally returned to the caller so it
/// can report a degraded catalog without failing the backup itself.
pub(crate) async fn index_backup_after_finalize(
    fs: Arc<dyn FS>,
    key: String,
    password: Option<String>,
    compress: i32,
    backup: &Backup,
    parent_hash: Option<&str>,
    changed_paths: Option<&BTreeSet<String>>,
) -> Result<(), String> {
    let current_pending = PendingCatalogBackup {
        backup_hash: backup.hash.clone(),
        timestamp: backup.timestamp,
        parent_hash: parent_hash.map(ToString::to_string),
    };

    let existing_catalog = match read_catalog(&fs, &key, password.as_deref()).await {
        Ok(catalog) => catalog,
        Err(error) => {
            let _ = mark_catalog_degraded(
                &fs,
                &key,
                password.as_deref(),
                compress,
                current_pending.clone(),
            )
            .await;
            return Err(error);
        }
    };
    if existing_catalog.as_ref().is_some_and(|catalog| {
        catalog.value.latest_indexed_backup.as_deref() == Some(backup.hash.as_str())
    }) {
        return Ok(());
    }

    if let Some(catalog) = &existing_catalog {
        let reconciliation_result: Result<(), String> = async {
            for pending in catalog
                .value
                .pending_backups
                .iter()
                .take(MAX_PENDING_RECONCILIATIONS)
            {
                if pending.backup_hash == backup.hash {
                    continue;
                }

                let pending_backup =
                    load_backup_manifest(&fs, &key, password.as_deref(), &pending.backup_hash)
                        .await
                        .map_err(|error| {
                            format!(
                                "Failed to reconcile pending catalog backup {}: {}",
                                pending.backup_hash, error
                            )
                        })?;
                let pending_parent = match pending.parent_hash.as_deref() {
                    Some(parent_hash) => Some(
                        load_backup_manifest(&fs, &key, password.as_deref(), parent_hash).await?,
                    ),
                    None => None,
                };
                let pending_catalog_paths = if pending_parent.is_none() {
                    Some(
                        list_current_entry_paths(Arc::clone(&fs), key.clone(), password.clone())
                            .await?,
                    )
                } else {
                    None
                };
                let pending_changes = build_snapshot_changes(
                    false,
                    pending_parent.as_ref().map(|parent| &parent.tree),
                    &pending_backup.tree,
                    None,
                    pending_catalog_paths.as_ref(),
                )?;

                apply_snapshot_shards(
                    &fs,
                    &key,
                    password.as_deref(),
                    compress,
                    &pending_changes,
                    &pending_backup.hash,
                    pending_backup.timestamp,
                    pending_parent.as_ref().map(|parent| parent.hash.as_str()),
                )
                .await
                .map_err(|error| {
                    format!(
                        "Failed to reconcile pending catalog backup {}: {}",
                        pending.backup_hash, error
                    )
                })?;

                commit_catalog_snapshot(&fs, &key, password.as_deref(), compress, &pending_backup)
                    .await?;
            }
            Ok(())
        }
        .await;

        if let Err(error) = reconciliation_result {
            let _ = mark_catalog_degraded(
                &fs,
                &key,
                password.as_deref(),
                compress,
                current_pending.clone(),
            )
            .await;
            return Err(error);
        }
    }

    let parent = match parent_hash {
        Some(parent_hash) => {
            match load_backup_manifest(&fs, &key, password.as_deref(), parent_hash).await {
                Ok(parent) => Some(parent),
                Err(error) => {
                    let message = format!(
                        "Failed to load the parent backup while updating the historical catalog: {}",
                        error
                    );
                    let _ = mark_catalog_degraded(
                        &fs,
                        &key,
                        password.as_deref(),
                        compress,
                        current_pending.clone(),
                    )
                    .await;
                    return Err(message);
                }
            }
        }
        None => None,
    };

    let catalog_is_new = existing_catalog.as_ref().is_none_or(|catalog| {
        catalog.value.indexed_backup_count == 0 && catalog.value.latest_indexed_backup.is_none()
    });
    let catalog_current_paths = if !catalog_is_new && parent.is_none() && changed_paths.is_none() {
        match list_current_entry_paths(Arc::clone(&fs), key.clone(), password.clone()).await {
            Ok(paths) => Some(paths),
            Err(error) => {
                let _ = mark_catalog_degraded(
                    &fs,
                    &key,
                    password.as_deref(),
                    compress,
                    current_pending.clone(),
                )
                .await;
                return Err(error);
            }
        }
    } else {
        None
    };
    let changes = match build_snapshot_changes(
        catalog_is_new,
        parent.as_ref().map(|parent| &parent.tree),
        &backup.tree,
        changed_paths,
        catalog_current_paths.as_ref(),
    ) {
        Ok(changes) => changes,
        Err(error) => {
            let _ = mark_catalog_degraded(
                &fs,
                &key,
                password.as_deref(),
                compress,
                current_pending.clone(),
            )
            .await;
            return Err(error);
        }
    };

    if let Err(error) = apply_snapshot_shards(
        &fs,
        &key,
        password.as_deref(),
        compress,
        &changes,
        &backup.hash,
        backup.timestamp,
        parent.as_ref().map(|parent| parent.hash.as_str()),
    )
    .await
    {
        let _ = mark_catalog_degraded(
            &fs,
            &key,
            password.as_deref(),
            compress,
            current_pending.clone(),
        )
        .await;
        return Err(error);
    }

    if let Err(error) =
        commit_catalog_snapshot(&fs, &key, password.as_deref(), compress, backup).await
    {
        let _ =
            mark_catalog_degraded(&fs, &key, password.as_deref(), compress, current_pending).await;
        return Err(error);
    }

    Ok(())
}

async fn commit_catalog_snapshot(
    fs: &Arc<dyn FS>,
    key: &str,
    password: Option<&str>,
    compress: i32,
    backup: &Backup,
) -> Result<(), String> {
    let catalog_path = super::storage::catalog_path(key);
    update_object(
        fs,
        &catalog_path,
        password,
        compress,
        Catalog::default(),
        "catalog",
        |catalog| {
            if catalog.schema_version != super::model::CATALOG_SCHEMA_VERSION {
                return Err(format!(
                    "Unsupported historical catalog schema version: {}",
                    catalog.schema_version
                ));
            }

            let already_indexed = catalog
                .recently_indexed_backups
                .iter()
                .any(|hash| hash == &backup.hash);

            if !already_indexed {
                catalog.indexed_backup_count = catalog.indexed_backup_count.saturating_add(1);
                let follows_current_latest = backup
                    .parents
                    .first()
                    .zip(catalog.latest_indexed_backup.as_ref())
                    .is_some_and(|(parent, latest)| parent == latest);
                let is_newer_by_timestamp =
                    backup.timestamp >= catalog.latest_indexed_timestamp.unwrap_or_default();
                if catalog.latest_indexed_backup.is_none()
                    || follows_current_latest
                    || is_newer_by_timestamp
                {
                    catalog.latest_indexed_backup = Some(backup.hash.clone());
                    catalog.latest_indexed_timestamp = Some(backup.timestamp);
                }
                catalog.recently_indexed_backups.push(backup.hash.clone());
                if catalog.recently_indexed_backups.len() > MAX_RECENTLY_INDEXED_BACKUPS {
                    let remove_count =
                        catalog.recently_indexed_backups.len() - MAX_RECENTLY_INDEXED_BACKUPS;
                    catalog.recently_indexed_backups.drain(..remove_count);
                }
            }

            catalog
                .pending_backups
                .retain(|pending| pending.backup_hash != backup.hash);
            catalog.state = if catalog.pending_backups.is_empty() {
                CatalogState::Ready
            } else {
                CatalogState::Degraded
            };
            Ok(())
        },
    )
    .await
    .map(|_| ())
}

pub(crate) async fn remove_backup_from_catalog(
    fs: Arc<dyn FS>,
    key: String,
    password: Option<String>,
    compress: i32,
    deleted_backup: &Backup,
    remaining_summaries: &[BackupSummary],
    chunk_indexes: &HashMap<String, ChunkIndex>,
) -> Result<(), String> {
    let Some(catalog) = read_catalog(&fs, &key, password.as_deref()).await? else {
        return Ok(());
    };

    let deleting_latest =
        catalog.value.latest_indexed_backup.as_deref() == Some(deleted_backup.hash.as_str());
    let next_backup = if deleting_latest {
        match remaining_summaries.first() {
            Some(summary) => {
                Some(load_backup_manifest(&fs, &key, password.as_deref(), &summary.hash).await?)
            }
            None => None,
        }
    } else {
        None
    };

    let mut paths = BTreeSet::new();
    for raw_path in deleted_backup.tree.keys() {
        paths.insert(normalize_file_path(raw_path)?);
    }
    if let Some(next_backup) = &next_backup {
        for raw_path in next_backup.tree.keys() {
            paths.insert(normalize_file_path(raw_path)?);
        }
    }

    let mut entry_shard_paths = BTreeMap::<String, Vec<String>>::new();
    for path in &paths {
        let id = entry_id(path);
        entry_shard_paths
            .entry(shard_id(&id))
            .or_default()
            .push(path.clone());
    }

    let mut candidate_cache = HashMap::<String, Backup>::new();
    let mut entry_actions = BTreeMap::<String, Vec<EntryDeletionAction>>::new();
    let mut directory_changes = Vec::new();
    let mut purged_entries = Vec::new();
    let mut cataloged_target = deleting_latest
        || catalog
            .value
            .recently_indexed_backups
            .iter()
            .any(|hash| hash == &deleted_backup.hash);

    for (shard, shard_paths) in entry_shard_paths {
        let shard_path = entry_shard_path(&key, &shard);
        let Some(shard_data) =
            read_object::<EntryShard>(&fs, &shard_path, password.as_deref(), "catalog entry shard")
                .await?
        else {
            continue;
        };

        for path in shard_paths {
            let id = entry_id(&path);
            let Some(mut entry) = shard_data.value.entries.get(&id).cloned() else {
                continue;
            };

            let deleted_object = find_tree_object(&deleted_backup.tree, &path);
            let previous_exists = entry.exists_in_latest_indexed_snapshot;
            let mut revisions = Vec::with_capacity(entry.revisions.len());

            for mut revision in entry.revisions {
                let references_deleted_backup = revision.latest_restorable_backup.as_deref()
                    == Some(deleted_backup.hash.as_str())
                    || revision.present_from_backup == deleted_backup.hash;
                cataloged_target |= references_deleted_backup;

                let same_content_as_deleted =
                    deleted_object.is_some_and(|object| object.hash == revision.content_hash);

                if same_content_as_deleted || references_deleted_backup {
                    let replacement = find_latest_restorable_backup(
                        &fs,
                        &key,
                        password.as_deref(),
                        &path,
                        &revision,
                        remaining_summaries,
                        chunk_indexes,
                        &mut candidate_cache,
                    )
                    .await?;

                    if let Some(replacement) = replacement {
                        revision.latest_restorable_backup = Some(replacement);
                    } else if references_deleted_backup {
                        revision.latest_restorable_backup = None;
                    }
                }

                if revision.latest_restorable_backup.is_some() {
                    revisions.push(revision);
                }
            }

            entry.revisions = revisions;

            if entry.revisions.is_empty() {
                purged_entries.push(PurgedEntry {
                    path: path.clone(),
                    entry_id: id.clone(),
                });
                directory_changes.push(DirectoryDeletionChange {
                    path,
                    entry_id: id.clone(),
                    present: false,
                    purge: true,
                });
                entry_actions
                    .entry(shard.clone())
                    .or_default()
                    .push(EntryDeletionAction::Remove { entry_id: id });
                continue;
            }

            if deleting_latest {
                let next_object = next_backup
                    .as_ref()
                    .and_then(|backup| find_tree_object(&backup.tree, &path));
                if let Some(next_object) = next_object {
                    if let Some(revision) = entry
                        .revisions
                        .iter_mut()
                        .rev()
                        .find(|revision| revision.content_hash == next_object.hash)
                    {
                        let latest_hash = remaining_summaries
                            .first()
                            .map(|summary| summary.hash.clone());
                        revision.latest_restorable_backup = latest_hash.clone();
                        entry.latest_restorable_backup = latest_hash;
                        entry.exists_in_latest_indexed_snapshot = true;
                    } else {
                        entry.exists_in_latest_indexed_snapshot = false;
                        entry.latest_restorable_backup = entry
                            .revisions
                            .iter()
                            .rev()
                            .find_map(|revision| revision.latest_restorable_backup.clone());
                    }
                } else {
                    entry.exists_in_latest_indexed_snapshot = false;
                    entry.latest_restorable_backup = entry
                        .revisions
                        .iter()
                        .rev()
                        .find_map(|revision| revision.latest_restorable_backup.clone());
                }
            } else {
                entry.latest_restorable_backup = entry
                    .revisions
                    .iter()
                    .rev()
                    .find_map(|revision| revision.latest_restorable_backup.clone());
            }

            let current_changed =
                previous_exists != entry.exists_in_latest_indexed_snapshot || deleting_latest;
            if current_changed {
                directory_changes.push(DirectoryDeletionChange {
                    path: path.clone(),
                    entry_id: id.clone(),
                    present: entry.exists_in_latest_indexed_snapshot,
                    purge: false,
                });
            }

            entry_actions
                .entry(shard.clone())
                .or_default()
                .push(EntryDeletionAction::Replace { entry });
        }
    }

    for (shard, actions) in entry_actions {
        let path = entry_shard_path(&key, &shard);
        update_object(
            &fs,
            &path,
            password.as_deref(),
            compress,
            empty_entry_shard(),
            "catalog entry shard",
            |shard_data: &mut EntryShard| {
                for action in &actions {
                    match action {
                        EntryDeletionAction::Remove { entry_id } => {
                            shard_data.entries.remove(entry_id);
                        }
                        EntryDeletionAction::Replace { entry } => {
                            if shard_data.entries.contains_key(&entry.entry_id) {
                                shard_data
                                    .entries
                                    .insert(entry.entry_id.clone(), entry.clone());
                            }
                        }
                    }
                }
                Ok(())
            },
        )
        .await?;
    }

    update_children_for_deletion(&fs, &key, password.as_deref(), compress, &directory_changes)
        .await?;
    cleanup_empty_directories(&fs, &key, password.as_deref(), compress, &purged_entries).await?;
    remove_purged_tokens(&fs, &key, password.as_deref(), compress, &purged_entries).await?;

    let catalog_path = super::storage::catalog_path(&key);
    update_object(
        &fs,
        &catalog_path,
        password.as_deref(),
        compress,
        Catalog::default(),
        "catalog",
        |catalog| {
            if catalog.schema_version != super::model::CATALOG_SCHEMA_VERSION {
                return Err(format!(
                    "Unsupported historical catalog schema version: {}",
                    catalog.schema_version
                ));
            }

            if cataloged_target && catalog.indexed_backup_count > 0 {
                catalog.indexed_backup_count -= 1;
            }

            if deleting_latest {
                if let Some(next_summary) = remaining_summaries.first() {
                    if catalog.indexed_backup_count > 0 {
                        catalog.latest_indexed_backup = Some(next_summary.hash.clone());
                        catalog.latest_indexed_timestamp = next_summary.timestamp;
                    } else {
                        catalog.latest_indexed_backup = None;
                        catalog.latest_indexed_timestamp = None;
                    }
                } else {
                    catalog.latest_indexed_backup = None;
                    catalog.latest_indexed_timestamp = None;
                }
            }

            catalog
                .pending_backups
                .retain(|pending| pending.backup_hash != deleted_backup.hash);
            catalog
                .recently_indexed_backups
                .retain(|hash| hash != &deleted_backup.hash);
            catalog.state = if catalog.pending_backups.is_empty() {
                CatalogState::Ready
            } else {
                CatalogState::Degraded
            };
            Ok(())
        },
    )
    .await?;

    Ok(())
}

#[derive(Debug, Clone)]
enum EntryDeletionAction {
    Remove { entry_id: String },
    Replace { entry: EntryHistory },
}

#[derive(Debug, Clone)]
struct PurgedEntry {
    path: String,
    entry_id: String,
}

#[derive(Debug, Clone)]
struct DirectoryDeletionChange {
    path: String,
    entry_id: String,
    present: bool,
    purge: bool,
}

fn find_tree_object<'a>(
    tree: &'a HashMap<String, BackupObject>,
    normalized_path: &str,
) -> Option<&'a BackupObject> {
    tree.get(normalized_path).or_else(|| {
        tree.iter().find_map(|(path, object)| {
            normalize_file_path(path)
                .ok()
                .filter(|path| path == normalized_path)
                .map(|_| object)
        })
    })
}

async fn find_latest_restorable_backup(
    fs: &Arc<dyn FS>,
    key: &str,
    password: Option<&str>,
    path: &str,
    revision: &FileRevision,
    summaries: &[BackupSummary],
    chunk_indexes: &HashMap<String, ChunkIndex>,
    cache: &mut HashMap<String, Backup>,
) -> Result<Option<String>, String> {
    let Some(start_index) = summaries
        .iter()
        .position(|summary| summary.hash == revision.present_from_backup)
    else {
        let first_valid_index = revision
            .present_until_backup
            .as_ref()
            .and_then(|hash| summaries.iter().position(|summary| summary.hash == *hash))
            .map(|index| index.saturating_add(1))
            .unwrap_or(0);
        for summary in summaries.iter().skip(first_valid_index) {
            let backup = load_cached_backup(fs, key, password, summary, cache).await?;
            if let Some(object) = find_tree_object(&backup.tree, path)
                && object.hash == revision.content_hash
                && object.size == revision.size
                && super::storage::backup_object_is_restorable(object, chunk_indexes)
            {
                return Ok(Some(summary.hash.clone()));
            }
        }
        return Ok(None);
    };

    let first_valid_index = revision
        .present_until_backup
        .as_ref()
        .and_then(|hash| summaries.iter().position(|summary| summary.hash == *hash))
        .map(|index| index.saturating_add(1))
        .unwrap_or(0);
    if first_valid_index > start_index {
        return Ok(None);
    }

    for summary in &summaries[first_valid_index..=start_index] {
        let backup = load_cached_backup(fs, key, password, summary, cache).await?;

        if let Some(object) = find_tree_object(&backup.tree, path)
            && object.hash == revision.content_hash
            && object.size == revision.size
            && super::storage::backup_object_is_restorable(object, chunk_indexes)
        {
            return Ok(Some(summary.hash.clone()));
        }
    }

    Ok(None)
}

async fn load_cached_backup(
    fs: &Arc<dyn FS>,
    key: &str,
    password: Option<&str>,
    summary: &BackupSummary,
    cache: &mut HashMap<String, Backup>,
) -> Result<Backup, String> {
    if let Some(backup) = cache.get(&summary.hash) {
        return Ok(backup.clone());
    }

    let backup = load_backup_manifest(fs, key, password, &summary.hash).await?;
    cache.insert(summary.hash.clone(), backup.clone());
    Ok(backup)
}

async fn update_children_for_deletion(
    fs: &Arc<dyn FS>,
    key: &str,
    password: Option<&str>,
    compress: i32,
    changes: &[DirectoryDeletionChange],
) -> Result<(), String> {
    let mut grouped = BTreeMap::<String, Vec<DirectoryDeletionChange>>::new();
    for change in changes {
        for directory in directory_paths(&change.path) {
            let id = directory_id(&directory);
            grouped
                .entry(shard_id(&id))
                .or_default()
                .push(change.clone());
        }
    }

    for (shard, shard_changes) in grouped {
        let path = super::storage::children_shard_path(key, &shard);
        update_object(
            fs,
            &path,
            password,
            compress,
            empty_children_shard(),
            "catalog children shard",
            |shard_data: &mut ChildrenShard| {
                for change in &shard_changes {
                    for directory in directory_paths(&change.path) {
                        let id = directory_id(&directory);
                        if shard_id(&id) != shard {
                            continue;
                        }

                        if let Some(record) = shard_data.directories.get_mut(&id) {
                            if change.present {
                                record.current_entry_ids.insert(change.entry_id.clone());
                            } else {
                                record.current_entry_ids.remove(&change.entry_id);
                            }
                        }
                    }

                    if !change.purge {
                        continue;
                    }

                    let parent = parent_directory(&change.path);
                    let parent_id = directory_id(&parent);
                    if shard_id(&parent_id) != shard {
                        continue;
                    }

                    if let Some(record) = shard_data.directories.get_mut(&parent_id)
                        && record
                            .children
                            .get(&file_name(&change.path))
                            .is_some_and(|child| {
                                child.kind == DirectoryChildKind::File
                                    && child.target_id == change.entry_id
                            })
                    {
                        record.children.remove(&file_name(&change.path));
                    }
                }
                Ok(())
            },
        )
        .await?;
    }

    Ok(())
}

async fn cleanup_empty_directories(
    fs: &Arc<dyn FS>,
    key: &str,
    password: Option<&str>,
    compress: i32,
    purged_entries: &[PurgedEntry],
) -> Result<(), String> {
    let mut directories = BTreeSet::new();
    for entry in purged_entries {
        for directory in directory_paths(&entry.path) {
            if !directory.is_empty() {
                directories.insert(directory);
            }
        }
    }

    let mut directories: Vec<String> = directories.into_iter().collect();
    directories.sort_by(|left, right| {
        right
            .split('/')
            .count()
            .cmp(&left.split('/').count())
            .then_with(|| right.cmp(left))
    });

    for directory in directories {
        let id = directory_id(&directory);
        let path = super::storage::children_shard_path(key, &shard_id(&id));
        let mut removed = false;

        update_object(
            fs,
            &path,
            password,
            compress,
            empty_children_shard(),
            "catalog children shard",
            |shard_data: &mut ChildrenShard| {
                if shard_data.directories.get(&id).is_some_and(|record| {
                    record.children.is_empty() && record.current_entry_ids.is_empty()
                }) {
                    shard_data.directories.remove(&id);
                    removed = true;
                }
                Ok(())
            },
        )
        .await?;

        if removed {
            remove_directory_link(
                fs,
                key,
                password,
                compress,
                &parent_directory(&directory),
                &directory,
                &id,
            )
            .await?;
        }
    }

    Ok(())
}

async fn remove_directory_link(
    fs: &Arc<dyn FS>,
    key: &str,
    password: Option<&str>,
    compress: i32,
    parent: &str,
    directory: &str,
    directory_id_value: &str,
) -> Result<(), String> {
    let parent_id = directory_id(parent);
    let path = super::storage::children_shard_path(key, &shard_id(&parent_id));
    let Some(_) =
        read_object::<ChildrenShard>(fs, &path, password, "catalog children shard").await?
    else {
        return Ok(());
    };

    update_object(
        fs,
        &path,
        password,
        compress,
        empty_children_shard(),
        "catalog children shard",
        |shard_data: &mut ChildrenShard| {
            if let Some(record) = shard_data.directories.get_mut(&parent_id)
                && record
                    .children
                    .get(&file_name(directory))
                    .is_some_and(|child| {
                        child.kind == DirectoryChildKind::Directory
                            && child.target_id == directory_id_value
                    })
            {
                record.children.remove(&file_name(directory));
            }
            Ok(())
        },
    )
    .await
    .map(|_| ())
}

async fn remove_purged_tokens(
    fs: &Arc<dyn FS>,
    key: &str,
    password: Option<&str>,
    compress: i32,
    purged_entries: &[PurgedEntry],
) -> Result<(), String> {
    let mut grouped = BTreeMap::<String, Vec<(String, String)>>::new();
    for entry in purged_entries {
        for token in path_tokens(&entry.path) {
            grouped
                .entry(shard_id(&token))
                .or_default()
                .push((token, entry.entry_id.clone()));
        }
    }

    for (shard, removals) in grouped {
        let path = super::storage::token_shard_path(key, &shard);
        let Some(_) = read_object::<TokenShard>(fs, &path, password, "catalog token shard").await?
        else {
            continue;
        };

        update_object(
            fs,
            &path,
            password,
            compress,
            empty_token_shard(),
            "catalog token shard",
            |shard_data: &mut TokenShard| {
                for (token, entry_id_value) in &removals {
                    let mut remove_posting = false;
                    if let Some(posting) = shard_data.postings.get_mut(token) {
                        posting.entry_ids.remove(entry_id_value);
                        remove_posting = posting.entry_ids.is_empty();
                    }
                    if remove_posting {
                        shard_data.postings.remove(token);
                    }
                }
                Ok(())
            },
        )
        .await?;
    }

    Ok(())
}

#[derive(Debug, Clone)]
struct SnapshotChange {
    path: String,
    before: Option<BackupObject>,
    after: Option<BackupObject>,
}

fn build_snapshot_changes(
    catalog_is_new: bool,
    parent_tree: Option<&HashMap<String, BackupObject>>,
    new_tree: &HashMap<String, BackupObject>,
    changed_paths: Option<&BTreeSet<String>>,
    catalog_current_paths: Option<&BTreeSet<String>>,
) -> Result<Vec<SnapshotChange>, String> {
    let mut paths = BTreeSet::new();

    if catalog_is_new {
        paths.extend(new_tree.keys().cloned());
    } else if let Some(changed_paths) = changed_paths {
        let changed_paths = changed_paths
            .iter()
            .map(|path| normalize_relative_path(path))
            .collect::<Result<Vec<_>, _>>()?;

        for raw_path in parent_tree
            .into_iter()
            .flat_map(|tree| tree.keys())
            .chain(new_tree.keys())
        {
            let normalized_path = normalize_file_path(raw_path)?;
            if changed_paths.iter().any(|changed_path| {
                changed_path.is_empty()
                    || normalized_path == *changed_path
                    || normalized_path.starts_with(&format!("{}/", changed_path))
            }) {
                paths.insert(raw_path.clone());
            }
        }
    } else {
        if let Some(parent_tree) = parent_tree {
            paths.extend(parent_tree.keys().cloned());
        }
        if let Some(catalog_current_paths) = catalog_current_paths {
            paths.extend(catalog_current_paths.iter().cloned());
        }
        paths.extend(new_tree.keys().cloned());
    }

    let mut normalized_paths =
        BTreeMap::<String, (Option<BackupObject>, Option<BackupObject>)>::new();

    for raw_path in paths {
        let path = normalize_file_path(&raw_path)?;
        let before = if catalog_is_new {
            None
        } else {
            parent_tree.and_then(|tree| tree.get(&raw_path).or_else(|| tree.get(&path)).cloned())
        };
        let after = new_tree
            .get(&raw_path)
            .or_else(|| new_tree.get(&path))
            .cloned();

        let slot = normalized_paths.entry(path).or_default();
        if before.is_some() {
            slot.0 = before;
        }
        if after.is_some() {
            slot.1 = after;
        }
    }

    let mut changes = Vec::new();
    for (path, (before, after)) in normalized_paths {
        if catalog_is_new
            || before.as_ref().map(|object| &object.hash)
                != after.as_ref().map(|object| &object.hash)
            || (catalog_current_paths.is_some() && after.is_none())
        {
            changes.push(SnapshotChange {
                path,
                before,
                after,
            });
        }
    }

    Ok(changes)
}

async fn apply_snapshot_shards(
    fs: &Arc<dyn FS>,
    key: &str,
    password: Option<&str>,
    compress: i32,
    changes: &[SnapshotChange],
    backup_hash: &str,
    timestamp: u64,
    parent_hash: Option<&str>,
) -> Result<(), String> {
    update_entry_shards(
        fs,
        key,
        password,
        compress,
        changes,
        backup_hash,
        timestamp,
        parent_hash,
    )
    .await?;
    update_children_shards(fs, key, password, compress, changes).await?;
    update_token_shards(fs, key, password, compress, changes).await
}

async fn update_entry_shards(
    fs: &Arc<dyn FS>,
    key: &str,
    password: Option<&str>,
    compress: i32,
    changes: &[SnapshotChange],
    backup_hash: &str,
    timestamp: u64,
    parent_hash: Option<&str>,
) -> Result<(), String> {
    let mut grouped = BTreeMap::<String, Vec<SnapshotChange>>::new();
    for change in changes {
        grouped
            .entry(shard_id(&entry_id(&change.path)))
            .or_default()
            .push(change.clone());
    }

    stream::iter(grouped.into_iter().map(|(shard, shard_changes)| {
        let path = entry_shard_path(key, &shard);
        async move {
            update_object(
                fs,
                &path,
                password,
                compress,
                empty_entry_shard(),
                "catalog entry shard",
                |shard_data: &mut EntryShard| {
                    for change in &shard_changes {
                        let id = entry_id(&change.path);
                        let existing = shard_data.entries.get(&id).cloned();

                        match &change.after {
                            Some(object) => {
                                let was_cataloged = existing.is_some();
                                let mut entry = existing.unwrap_or_else(|| {
                                    new_entry_history(
                                        &change.path,
                                        &id,
                                        object,
                                        backup_hash,
                                        timestamp,
                                    )
                                });

                                if !was_cataloged {
                                    shard_data.entries.insert(id, entry);
                                    continue;
                                }

                                if entry.last_change_backup.as_deref() == Some(backup_hash) {
                                    continue;
                                }

                                apply_present_change(&mut entry, object, backup_hash, timestamp);
                                shard_data.entries.insert(id, entry);
                            }
                            None => {
                                if let Some(mut entry) = existing {
                                    if entry.last_change_backup.as_deref() == Some(backup_hash) {
                                        continue;
                                    }

                                    apply_deleted_change(
                                        &mut entry,
                                        backup_hash,
                                        timestamp,
                                        parent_hash,
                                    );
                                    shard_data.entries.insert(id, entry);
                                }
                            }
                        }
                    }
                    Ok(())
                },
            )
            .await
            .map(|_| ())
        }
    }))
    .buffer_unordered(MAX_CONCURRENT_CATALOG_SHARD_UPDATES)
    .try_collect::<Vec<_>>()
    .await
    .map(|_| ())
}

fn new_entry_history(
    path: &str,
    id: &str,
    object: &BackupObject,
    backup_hash: &str,
    timestamp: u64,
) -> EntryHistory {
    EntryHistory {
        entry_id: id.to_string(),
        path: path.to_string(),
        lookup_path: lookup_path(path),
        parent_directory_id: directory_id(&parent_directory(path)),
        name: file_name(path),
        first_seen_backup: backup_hash.to_string(),
        first_seen_timestamp: timestamp,
        last_seen_backup: backup_hash.to_string(),
        last_seen_timestamp: timestamp,
        exists_in_latest_indexed_snapshot: true,
        latest_restorable_backup: Some(backup_hash.to_string()),
        last_change_backup: Some(backup_hash.to_string()),
        revisions: vec![new_revision(id, object, backup_hash, timestamp)],
    }
}

fn new_revision(
    entry_id_value: &str,
    object: &BackupObject,
    backup_hash: &str,
    timestamp: u64,
) -> FileRevision {
    FileRevision {
        revision_id: revision_id(entry_id_value, backup_hash),
        present_from_backup: backup_hash.to_string(),
        present_from_timestamp: timestamp,
        present_until_backup: None,
        present_until_timestamp: None,
        content_hash: object.hash.clone(),
        size: object.size,
        content_type: object.content_type.clone(),
        permissions: object.permissions,
        latest_restorable_backup: Some(backup_hash.to_string()),
    }
}

fn apply_present_change(
    entry: &mut EntryHistory,
    object: &BackupObject,
    backup_hash: &str,
    timestamp: u64,
) {
    let same_revision = entry.exists_in_latest_indexed_snapshot
        && entry
            .revisions
            .last()
            .is_some_and(|revision| revision.content_hash == object.hash);

    if !same_revision {
        if let Some(previous_revision) = entry
            .revisions
            .iter_mut()
            .rev()
            .find(|revision| revision.present_until_backup.is_none())
        {
            previous_revision.present_until_backup = Some(backup_hash.to_string());
            previous_revision.present_until_timestamp = Some(timestamp);
        }

        entry.revisions.push(new_revision(
            &entry.entry_id,
            object,
            backup_hash,
            timestamp,
        ));
    } else if let Some(revision) = entry.revisions.last_mut() {
        revision.latest_restorable_backup = Some(backup_hash.to_string());
    }

    entry.exists_in_latest_indexed_snapshot = true;
    entry.latest_restorable_backup = Some(backup_hash.to_string());
    entry.last_seen_backup = backup_hash.to_string();
    entry.last_seen_timestamp = timestamp;
    entry.last_change_backup = Some(backup_hash.to_string());
}

fn apply_deleted_change(
    entry: &mut EntryHistory,
    backup_hash: &str,
    timestamp: u64,
    parent_hash: Option<&str>,
) {
    let latest_restorable_backup = if let Some(previous_revision) = entry
        .revisions
        .iter_mut()
        .rev()
        .find(|revision| revision.present_until_backup.is_none())
    {
        previous_revision.present_until_backup = Some(backup_hash.to_string());
        previous_revision.present_until_timestamp = Some(timestamp);
        if let Some(parent_hash) = parent_hash {
            previous_revision.latest_restorable_backup = Some(parent_hash.to_string());
        }
        previous_revision.latest_restorable_backup.clone()
    } else {
        None
    };

    let latest_restorable_backup = latest_restorable_backup.or_else(|| {
        entry
            .revisions
            .iter()
            .rev()
            .find_map(|revision| revision.latest_restorable_backup.clone())
    });

    entry.exists_in_latest_indexed_snapshot = false;
    entry.latest_restorable_backup = latest_restorable_backup;
    entry.last_seen_backup = backup_hash.to_string();
    entry.last_seen_timestamp = timestamp;
    entry.last_change_backup = Some(backup_hash.to_string());
}

#[derive(Debug, Clone)]
struct DirectoryChange {
    path: String,
    entry_id: String,
    present: bool,
}

async fn update_children_shards(
    fs: &Arc<dyn FS>,
    key: &str,
    password: Option<&str>,
    compress: i32,
    changes: &[SnapshotChange],
) -> Result<(), String> {
    let directory_changes: Vec<DirectoryChange> = changes
        .iter()
        .map(|change| DirectoryChange {
            path: change.path.clone(),
            entry_id: entry_id(&change.path),
            present: change.after.is_some(),
        })
        .collect();

    let mut grouped = BTreeMap::<String, Vec<DirectoryChange>>::new();
    for change in &directory_changes {
        for path in directory_paths(&change.path) {
            let id = directory_id(&path);
            grouped
                .entry(shard_id(&id))
                .or_default()
                .push(change.clone());
        }
    }

    stream::iter(grouped.into_iter().map(|(shard, shard_changes)| {
        let path = children_shard_path(key, &shard);
        async move {
            update_object(
                fs,
                &path,
                password,
                compress,
                empty_children_shard(),
                "catalog children shard",
                |shard_data: &mut ChildrenShard| {
                    for change in &shard_changes {
                        let directories = directory_paths(&change.path);

                        for directory in &directories {
                            let id = directory_id(directory);
                            if shard_id(&id) != shard {
                                continue;
                            }
                            let record = shard_data
                                .directories
                                .entry(id.clone())
                                .or_insert_with(|| DirectoryChildren::new(id, directory.clone()));

                            if change.present {
                                record.current_entry_ids.insert(change.entry_id.clone());
                            } else {
                                record.current_entry_ids.remove(&change.entry_id);
                            }
                        }

                        if !change.present {
                            continue;
                        }

                        for directory in directories.iter().skip(1) {
                            let parent = parent_directory(directory);
                            let parent_id = directory_id(&parent);
                            if shard_id(&parent_id) != shard {
                                continue;
                            }
                            let child = DirectoryChild {
                                name: file_name(directory),
                                kind: DirectoryChildKind::Directory,
                                target_id: directory_id(directory),
                            };
                            shard_data
                                .directories
                                .entry(parent_id.clone())
                                .or_insert_with(|| {
                                    DirectoryChildren::new(parent_id, parent.clone())
                                })
                                .children
                                .entry(child.name.clone())
                                .and_modify(|current| *current = child.clone())
                                .or_insert(child);
                        }

                        let parent = parent_directory(&change.path);
                        let parent_id = directory_id(&parent);
                        if shard_id(&parent_id) != shard {
                            continue;
                        }
                        let child = DirectoryChild {
                            name: file_name(&change.path),
                            kind: DirectoryChildKind::File,
                            target_id: change.entry_id.clone(),
                        };
                        shard_data
                            .directories
                            .entry(parent_id.clone())
                            .or_insert_with(|| DirectoryChildren::new(parent_id, parent))
                            .children
                            .entry(child.name.clone())
                            .and_modify(|current| *current = child.clone())
                            .or_insert(child);
                    }
                    Ok(())
                },
            )
            .await
            .map(|_| ())
        }
    }))
    .buffer_unordered(MAX_CONCURRENT_CATALOG_SHARD_UPDATES)
    .try_collect::<Vec<_>>()
    .await
    .map(|_| ())
}

async fn update_token_shards(
    fs: &Arc<dyn FS>,
    key: &str,
    password: Option<&str>,
    compress: i32,
    changes: &[SnapshotChange],
) -> Result<(), String> {
    let mut grouped = BTreeMap::<String, Vec<(String, String)>>::new();
    for change in changes {
        // Token postings are derived from the path, not file contents. Existing
        // paths therefore do not need to rewrite their token shards for a
        // content or metadata-only change. New paths are still indexed here;
        // deletions are removed when their history is purged.
        if change.before.is_some() || change.after.is_none() {
            continue;
        }
        let id = entry_id(&change.path);
        for token in path_tokens(&change.path) {
            grouped
                .entry(shard_id(&token))
                .or_default()
                .push((token, id.clone()));
        }
    }

    stream::iter(grouped.into_iter().map(|(shard, shard_changes)| {
        let path = token_shard_path(key, &shard);
        async move {
            update_object(
                fs,
                &path,
                password,
                compress,
                empty_token_shard(),
                "catalog token shard",
                |shard_data: &mut TokenShard| {
                    for (token, id) in &shard_changes {
                        shard_data
                            .postings
                            .entry(token.clone())
                            .or_insert_with(|| TokenPosting {
                                token: token.clone(),
                                entry_ids: BTreeSet::new(),
                            })
                            .entry_ids
                            .insert(id.clone());
                    }
                    Ok(())
                },
            )
            .await
            .map(|_| ())
        }
    }))
    .buffer_unordered(MAX_CONCURRENT_CATALOG_SHARD_UPDATES)
    .try_collect::<Vec<_>>()
    .await
    .map(|_| ())
}

#[allow(dead_code)]
pub(crate) async fn catalog_exists(
    fs: Arc<dyn FS>,
    key: String,
    password: Option<String>,
) -> Result<bool, String> {
    Ok(read_catalog(&fs, &key, password.as_deref())
        .await?
        .is_some())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::core::catalog::query::{
        CatalogEntryScope, get_entry_history, get_entry_history_with_snapshot,
        list_directory_children, list_directory_children_with_snapshot,
        load_latest_parentless_snapshot, lookup_entries_by_tokens,
        lookup_entries_by_tokens_with_snapshot, read_catalog_status,
    };
    use crate::core::catalog::storage::catalog_path;
    use crate::core::metadata::{BackupObject, BackupSummary, ChunkIndex};
    use crate::storage::{FS, LocalFS};
    use crate::utils::compress_bytes;
    use async_trait::async_trait;
    use std::sync::Arc;
    use std::sync::atomic::{AtomicBool, Ordering};

    struct FailOnceCatalogWriteFs {
        inner: LocalFS,
        failed: AtomicBool,
    }

    #[async_trait]
    impl FS for FailOnceCatalogWriteFs {
        async fn read_file(&self, path: &str) -> Result<Vec<u8>, std::io::Error> {
            self.inner.read_file(path).await
        }

        async fn write_file(&self, path: &str, data: &[u8]) -> Result<(), std::io::Error> {
            self.inner.write_file(path, data).await
        }

        async fn list_files(&self, path: &str) -> Result<Vec<String>, std::io::Error> {
            self.inner.list_files(path).await
        }

        async fn delete_file(&self, path: &str) -> Result<(), std::io::Error> {
            self.inner.delete_file(path).await
        }

        async fn write_file_if_version(
            &self,
            path: &str,
            data: &[u8],
            expected_version: Option<&str>,
        ) -> Result<(), std::io::Error> {
            if path.contains("/entries/") && !self.failed.swap(true, Ordering::SeqCst) {
                return Err(std::io::Error::other("injected catalog write failure"));
            }
            self.inner
                .write_file_if_version(path, data, expected_version)
                .await
        }
    }

    fn object(hash: &str) -> BackupObject {
        BackupObject {
            hash: hash.to_string(),
            size: 1,
            content_type: "application/octet-stream".to_string(),
            permissions: 0o644,
            chunks: vec![format!("chunk-{hash}")],
        }
    }

    #[test]
    fn first_catalog_update_indexes_only_the_new_snapshot() {
        let new_tree = HashMap::from([
            ("src/main.rs".to_string(), object("one")),
            ("README.md".to_string(), object("readme")),
        ]);
        let changes = build_snapshot_changes(true, None, &new_tree, None, None).unwrap();

        assert_eq!(changes.len(), 2);
        assert!(changes.iter().all(|change| change.before.is_none()));
    }

    #[test]
    fn incremental_catalog_changes_include_add_change_and_delete() {
        let parent_tree = HashMap::from([
            ("same.txt".to_string(), object("same")),
            ("changed.txt".to_string(), object("before")),
            ("deleted.txt".to_string(), object("deleted")),
        ]);
        let new_tree = HashMap::from([
            ("same.txt".to_string(), object("same")),
            ("changed.txt".to_string(), object("after")),
            ("created.txt".to_string(), object("created")),
        ]);
        let changes =
            build_snapshot_changes(false, Some(&parent_tree), &new_tree, None, None).unwrap();

        assert_eq!(changes.len(), 3);
        assert!(changes.iter().any(|change| change.path == "created.txt"));
        assert!(changes.iter().any(|change| change.path == "changed.txt"));
        assert!(changes.iter().any(|change| change.path == "deleted.txt"));
        assert!(!changes.iter().any(|change| change.path == "same.txt"));
    }

    fn backup(
        hash: &str,
        timestamp: u64,
        tree: HashMap<String, BackupObject>,
        parents: Vec<String>,
    ) -> Backup {
        Backup {
            message: hash.to_string(),
            hash: hash.to_string(),
            timestamp,
            author: "test".to_string(),
            parents,
            tree,
        }
    }

    async fn write_manifest(fs: &Arc<dyn FS>, key: &str, backup: &Backup) {
        let bytes = rmp_serde::to_vec_named(backup).unwrap();
        let compressed = compress_bytes(&bytes, 3);
        fs.write_file(&format!("{}/backups/{}", key, backup.hash), &compressed)
            .await
            .unwrap();
    }

    #[tokio::test]
    async fn an_absent_catalog_is_valid_for_existing_repositories() {
        let suffix = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let directory = std::env::temp_dir().join(format!("gib-catalog-absent-test-{suffix}"));
        let fs: Arc<dyn FS> = Arc::new(LocalFS::new(&directory));

        assert!(
            read_catalog_status(fs, "old-project".to_string(), None)
                .await
                .unwrap()
                .is_none()
        );

        let _ = std::fs::remove_dir_all(directory);
    }

    fn summaries(backups: &[&Backup]) -> Vec<BackupSummary> {
        backups
            .iter()
            .map(|backup| BackupSummary {
                message: backup.message.clone(),
                hash: backup.hash.clone(),
                timestamp: Some(backup.timestamp),
                size: Some(1),
            })
            .collect()
    }

    #[tokio::test]
    async fn indexes_lifecycle_and_supports_current_and_historical_queries() {
        let suffix = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let directory = std::env::temp_dir().join(format!("gib-catalog-test-{suffix}"));
        let fs: Arc<dyn FS> = Arc::new(LocalFS::new(&directory));
        let key = "project".to_string();

        let first = backup(
            "backup-one",
            1,
            HashMap::from([
                ("src/main.rs".to_string(), object("main-before")),
                ("README.md".to_string(), object("readme")),
                ("unchanged.txt".to_string(), object("unchanged")),
            ]),
            Vec::new(),
        );
        write_manifest(&fs, &key, &first).await;
        index_backup_after_finalize(Arc::clone(&fs), key.clone(), None, 3, &first, None, None)
            .await
            .unwrap();

        let second = backup(
            "backup-two",
            2,
            HashMap::from([
                ("src/main.rs".to_string(), object("main-after")),
                ("src/docs/guide.md".to_string(), object("guide")),
                ("README.md".to_string(), object("readme")),
                ("unchanged.txt".to_string(), object("unchanged")),
            ]),
            vec![first.hash.clone()],
        );
        write_manifest(&fs, &key, &second).await;
        index_backup_after_finalize(
            Arc::clone(&fs),
            key.clone(),
            None,
            3,
            &second,
            Some(&first.hash),
            None,
        )
        .await
        .unwrap();

        let third = backup(
            "backup-three",
            3,
            HashMap::from([
                ("src/docs/guide.md".to_string(), object("guide")),
                ("README.md".to_string(), object("readme")),
            ]),
            vec![second.hash.clone()],
        );
        write_manifest(&fs, &key, &third).await;
        index_backup_after_finalize(
            Arc::clone(&fs),
            key.clone(),
            None,
            3,
            &third,
            Some(&second.hash),
            None,
        )
        .await
        .unwrap();

        let status = read_catalog_status(Arc::clone(&fs), key.clone(), None)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(status.indexed_backup_count, 3);
        assert_eq!(
            status.latest_indexed_backup.as_deref(),
            Some("backup-three")
        );

        let deleted_unchanged =
            get_entry_history(Arc::clone(&fs), key.clone(), None, "unchanged.txt")
                .await
                .unwrap()
                .unwrap();
        assert!(!deleted_unchanged.exists_in_latest_indexed_snapshot);
        assert_eq!(
            deleted_unchanged.latest_restorable_backup.as_deref(),
            Some("backup-two")
        );

        let main = get_entry_history(Arc::clone(&fs), key.clone(), None, "SRC/./MAIN.RS")
            .await
            .unwrap()
            .unwrap();
        assert!(!main.exists_in_latest_indexed_snapshot);
        assert_eq!(main.revisions.len(), 2);
        assert_eq!(main.latest_restorable_backup.as_deref(), Some("backup-two"));

        let current = list_directory_children(
            Arc::clone(&fs),
            key.clone(),
            None,
            "SRC",
            CatalogEntryScope::Current,
            None,
            10,
        )
        .await
        .unwrap();
        assert_eq!(current.items.len(), 1);
        assert_eq!(current.items[0].name, "docs");

        let history = list_directory_children(
            Arc::clone(&fs),
            key.clone(),
            None,
            "SRC",
            CatalogEntryScope::AllHistory,
            None,
            10,
        )
        .await
        .unwrap();
        assert_eq!(
            history
                .items
                .iter()
                .map(|item| item.name.as_str())
                .collect::<Vec<_>>(),
            vec!["docs", "main.rs"]
        );
        assert!(!history.items[1].exists_in_latest_indexed_snapshot);

        let first_page = list_directory_children(
            Arc::clone(&fs),
            key.clone(),
            None,
            "SRC",
            CatalogEntryScope::AllHistory,
            None,
            1,
        )
        .await
        .unwrap();
        assert_eq!(first_page.items.len(), 1);
        let second_page = list_directory_children(
            Arc::clone(&fs),
            key.clone(),
            None,
            "SRC",
            CatalogEntryScope::AllHistory,
            first_page.next_cursor.as_deref(),
            1,
        )
        .await
        .unwrap();
        assert_eq!(second_page.items[0].name, "main.rs");

        let token_results = lookup_entries_by_tokens(
            Arc::clone(&fs),
            key.clone(),
            None,
            &[String::from("MAIN")],
            CatalogEntryScope::AllHistory,
            None,
            10,
        )
        .await
        .unwrap();
        assert_eq!(token_results.items.len(), 1);
        assert_eq!(token_results.items[0].path, "src/main.rs");

        let chunks = HashMap::from([
            ("chunk-main-before".to_string(), ChunkIndex { refcount: 1 }),
            ("chunk-main-after".to_string(), ChunkIndex { refcount: 1 }),
            ("chunk-guide".to_string(), ChunkIndex { refcount: 2 }),
            ("chunk-readme".to_string(), ChunkIndex { refcount: 3 }),
            ("chunk-unchanged".to_string(), ChunkIndex { refcount: 2 }),
        ]);
        let remaining = summaries(&[&second, &first]);
        remove_backup_from_catalog(
            Arc::clone(&fs),
            key.clone(),
            None,
            3,
            &third,
            &remaining,
            &chunks,
        )
        .await
        .unwrap();

        let restored_main = get_entry_history(Arc::clone(&fs), key.clone(), None, "src/main.rs")
            .await
            .unwrap()
            .unwrap();
        assert!(restored_main.exists_in_latest_indexed_snapshot);
        assert_eq!(
            restored_main.latest_restorable_backup.as_deref(),
            Some("backup-two")
        );

        remove_backup_from_catalog(
            Arc::clone(&fs),
            key.clone(),
            None,
            3,
            &second,
            &[summaries(&[&first])[0].clone()],
            &chunks,
        )
        .await
        .unwrap();
        assert!(
            get_entry_history(Arc::clone(&fs), key.clone(), None, "src/docs/guide.md")
                .await
                .unwrap()
                .is_none()
        );
        let remaining_children = list_directory_children(
            Arc::clone(&fs),
            key.clone(),
            None,
            "src",
            CatalogEntryScope::AllHistory,
            None,
            10,
        )
        .await
        .unwrap();
        assert_eq!(remaining_children.items.len(), 1);
        assert_eq!(remaining_children.items[0].name, "main.rs");

        let _ = fs.delete_file(&catalog_path(&key)).await;
        let _ = std::fs::remove_dir_all(directory);
    }

    #[tokio::test]
    async fn full_snapshots_without_parents_mark_missing_entries_as_deleted() {
        let suffix = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let directory = std::env::temp_dir().join(format!("gib-catalog-empty-test-{suffix}"));
        let fs: Arc<dyn FS> = Arc::new(LocalFS::new(&directory));
        let key = "downloads".to_string();

        let first = backup(
            "backup-with-files",
            1,
            HashMap::from([
                ("document.txt".to_string(), object("document")),
                ("photo.jpg".to_string(), object("photo")),
            ]),
            Vec::new(),
        );
        let second = backup("empty-backup", 2, HashMap::new(), Vec::new());
        write_manifest(&fs, &key, &first).await;
        write_manifest(&fs, &key, &second).await;

        index_backup_after_finalize(Arc::clone(&fs), key.clone(), None, 3, &first, None, None)
            .await
            .unwrap();
        index_backup_after_finalize(Arc::clone(&fs), key.clone(), None, 3, &second, None, None)
            .await
            .unwrap();

        for path in ["document.txt", "photo.jpg"] {
            let history = get_entry_history(Arc::clone(&fs), key.clone(), None, path)
                .await
                .unwrap()
                .unwrap();
            assert!(!history.exists_in_latest_indexed_snapshot);
            assert_eq!(
                history.latest_restorable_backup.as_deref(),
                Some(first.hash.as_str())
            );
        }

        let current = list_directory_children(
            Arc::clone(&fs),
            key.clone(),
            None,
            "",
            CatalogEntryScope::Current,
            None,
            10,
        )
        .await
        .unwrap();
        assert!(current.items.is_empty());

        let history = list_directory_children(
            Arc::clone(&fs),
            key.clone(),
            None,
            "",
            CatalogEntryScope::AllHistory,
            None,
            10,
        )
        .await
        .unwrap();
        assert_eq!(history.items.len(), 2);
        assert!(
            history
                .items
                .iter()
                .all(|entry| !entry.exists_in_latest_indexed_snapshot)
        );

        let search = lookup_entries_by_tokens(
            Arc::clone(&fs),
            key.clone(),
            None,
            &[String::from("document")],
            CatalogEntryScope::AllHistory,
            None,
            10,
        )
        .await
        .unwrap();
        assert_eq!(search.items.len(), 1);
        assert!(!search.items[0].exists_in_latest_indexed_snapshot);
        assert_eq!(
            search.items[0].latest_restorable_backup.as_deref(),
            Some(first.hash.as_str())
        );

        let _ = std::fs::remove_dir_all(directory);
    }

    #[tokio::test]
    async fn read_queries_validate_a_stale_parentless_snapshot_against_its_manifest() {
        let suffix = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let directory = std::env::temp_dir().join(format!("gib-catalog-read-repair-test-{suffix}"));
        let fs: Arc<dyn FS> = Arc::new(LocalFS::new(&directory));
        let key = "downloads".to_string();

        let first = backup(
            "backup-with-files",
            1,
            HashMap::from([("document.txt".to_string(), object("document"))]),
            Vec::new(),
        );
        let second = backup("empty-backup", 2, HashMap::new(), Vec::new());
        write_manifest(&fs, &key, &first).await;
        write_manifest(&fs, &key, &second).await;
        index_backup_after_finalize(Arc::clone(&fs), key.clone(), None, 3, &first, None, None)
            .await
            .unwrap();

        // Simulate the stale state produced by the old catalog updater: the
        // catalog points at the empty snapshot, while the entry shard still
        // says that document.txt is current.
        commit_catalog_snapshot(&fs, &key, None, 3, &second)
            .await
            .unwrap();

        let snapshot = load_latest_parentless_snapshot(Arc::clone(&fs), key.clone(), None)
            .await
            .unwrap()
            .unwrap();
        assert!(snapshot.current_entry_ids.is_empty());

        let history = get_entry_history_with_snapshot(
            Arc::clone(&fs),
            key.clone(),
            None,
            "document.txt",
            Some(&snapshot),
        )
        .await
        .unwrap()
        .unwrap();
        assert!(!history.exists_in_latest_indexed_snapshot);
        assert_eq!(
            history.latest_restorable_backup.as_deref(),
            Some(first.hash.as_str())
        );

        let current = list_directory_children_with_snapshot(
            Arc::clone(&fs),
            key.clone(),
            None,
            "",
            CatalogEntryScope::Current,
            None,
            10,
            Some(&snapshot),
        )
        .await
        .unwrap();
        assert!(current.items.is_empty());

        let search = lookup_entries_by_tokens_with_snapshot(
            Arc::clone(&fs),
            key.clone(),
            None,
            &[String::from("document")],
            CatalogEntryScope::AllHistory,
            None,
            10,
            Some(&snapshot),
        )
        .await
        .unwrap();
        assert_eq!(search.items.len(), 1);
        assert!(!search.items[0].exists_in_latest_indexed_snapshot);
        assert_eq!(
            search.items[0].latest_restorable_backup.as_deref(),
            Some(first.hash.as_str())
        );

        let _ = std::fs::remove_dir_all(directory);
    }

    #[tokio::test]
    async fn replays_bounded_pending_snapshots_before_the_next_backup() {
        let suffix = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let directory = std::env::temp_dir().join(format!("gib-catalog-pending-test-{suffix}"));
        let fs: Arc<dyn FS> = Arc::new(LocalFS::new(&directory));
        let key = "project".to_string();

        let first = backup(
            "backup-one",
            1,
            HashMap::from([("file.txt".to_string(), object("one"))]),
            Vec::new(),
        );
        let second = backup(
            "backup-two",
            2,
            HashMap::from([("file.txt".to_string(), object("two"))]),
            vec![first.hash.clone()],
        );
        let third = backup(
            "backup-three",
            3,
            HashMap::from([("file.txt".to_string(), object("three"))]),
            vec![second.hash.clone()],
        );
        for snapshot in [&first, &second, &third] {
            write_manifest(&fs, &key, snapshot).await;
        }

        index_backup_after_finalize(Arc::clone(&fs), key.clone(), None, 3, &first, None, None)
            .await
            .unwrap();
        super::super::storage::mark_catalog_degraded(
            &fs,
            &key,
            None,
            3,
            PendingCatalogBackup {
                backup_hash: second.hash.clone(),
                timestamp: second.timestamp,
                parent_hash: Some(first.hash.clone()),
            },
        )
        .await
        .unwrap();

        index_backup_after_finalize(
            Arc::clone(&fs),
            key.clone(),
            None,
            3,
            &third,
            Some(&second.hash),
            None,
        )
        .await
        .unwrap();

        let status = read_catalog_status(Arc::clone(&fs), key.clone(), None)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(status.indexed_backup_count, 3);
        assert_eq!(status.pending_backups, 0);
        assert_eq!(status.state, CatalogState::Ready);

        let _ = std::fs::remove_dir_all(directory);
    }

    #[tokio::test]
    async fn catalog_write_failures_leave_a_bounded_pending_snapshot_for_replay() {
        let suffix = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let directory = std::env::temp_dir().join(format!("gib-catalog-failure-test-{suffix}"));
        let fs: Arc<dyn FS> = Arc::new(FailOnceCatalogWriteFs {
            inner: LocalFS::new(&directory),
            failed: AtomicBool::new(false),
        });
        let key = "project".to_string();
        let snapshot = backup(
            "backup-one",
            1,
            HashMap::from([("file.txt".to_string(), object("one"))]),
            Vec::new(),
        );
        write_manifest(&fs, &key, &snapshot).await;

        assert!(
            index_backup_after_finalize(
                Arc::clone(&fs),
                key.clone(),
                None,
                3,
                &snapshot,
                None,
                None
            )
            .await
            .is_err()
        );
        let degraded = read_catalog_status(Arc::clone(&fs), key.clone(), None)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(degraded.state, CatalogState::Degraded);
        assert_eq!(degraded.pending_backups, 1);

        index_backup_after_finalize(Arc::clone(&fs), key.clone(), None, 3, &snapshot, None, None)
            .await
            .unwrap();
        let ready = read_catalog_status(Arc::clone(&fs), key, None)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(ready.state, CatalogState::Ready);
        assert_eq!(ready.pending_backups, 0);

        let _ = std::fs::remove_dir_all(directory);
    }

    #[tokio::test]
    async fn concurrent_catalog_updates_merge_with_compare_and_swap() {
        let suffix = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let directory = std::env::temp_dir().join(format!("gib-catalog-concurrent-test-{suffix}"));
        let fs: Arc<dyn FS> = Arc::new(LocalFS::new(&directory));
        let key = "project".to_string();
        let first = backup(
            "backup-one",
            1,
            HashMap::from([("first.txt".to_string(), object("first"))]),
            Vec::new(),
        );
        let second = backup(
            "backup-two",
            2,
            HashMap::from([("second.txt".to_string(), object("second"))]),
            Vec::new(),
        );
        write_manifest(&fs, &key, &first).await;
        write_manifest(&fs, &key, &second).await;

        let (first_result, second_result) = tokio::join!(
            index_backup_after_finalize(Arc::clone(&fs), key.clone(), None, 3, &first, None, None),
            index_backup_after_finalize(Arc::clone(&fs), key.clone(), None, 3, &second, None, None),
        );
        first_result.unwrap();
        second_result.unwrap();

        let status = read_catalog_status(Arc::clone(&fs), key.clone(), None)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(status.indexed_backup_count, 2);
        assert!(
            get_entry_history(Arc::clone(&fs), key.clone(), None, "first.txt")
                .await
                .unwrap()
                .is_some()
        );
        assert!(
            get_entry_history(Arc::clone(&fs), key, None, "second.txt")
                .await
                .unwrap()
                .is_some()
        );

        let _ = std::fs::remove_dir_all(directory);
    }

    #[tokio::test]
    async fn stores_catalog_objects_using_the_repository_encryption_pipeline() {
        let suffix = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let directory = std::env::temp_dir().join(format!("gib-catalog-encrypted-test-{suffix}"));
        let fs: Arc<dyn FS> = Arc::new(LocalFS::new(&directory));
        let key = "project".to_string();
        let password = "catalog-secret".to_string();
        let snapshot = backup(
            "backup-one",
            1,
            HashMap::from([("secret.txt".to_string(), object("secret"))]),
            Vec::new(),
        );

        let bytes = rmp_serde::to_vec_named(&snapshot).unwrap();
        let compressed = compress_bytes(&bytes, 3);
        let encoded = crate::core::crypto::encode_file_bytes(&compressed, Some(&password)).unwrap();
        fs.write_file(&format!("{}/backups/{}", key, snapshot.hash), &encoded)
            .await
            .unwrap();
        index_backup_after_finalize(
            Arc::clone(&fs),
            key.clone(),
            Some(password.clone()),
            3,
            &snapshot,
            None,
            None,
        )
        .await
        .unwrap();

        let raw_catalog = fs.read_file(&catalog_path(&key)).await.unwrap();
        assert!(crate::utils::is_encrypted(&raw_catalog));
        assert!(
            read_catalog_status(Arc::clone(&fs), key.clone(), Some(password))
                .await
                .unwrap()
                .is_some()
        );
        assert!(
            read_catalog_status(Arc::clone(&fs), key, Some("wrong".to_string()))
                .await
                .is_err()
        );

        let _ = std::fs::remove_dir_all(directory);
    }
}
