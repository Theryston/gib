use super::state::{ExplorerEntry, ExplorerKind, ExplorerScope, ExplorerStatus, SelectedFile};
use crate::core::catalog::{
    CatalogEntrySummary, EntryHistory, collect_entries_by_tokens, directory_exists,
    get_entry_history, list_directory_children, lookup_path, normalize_file_path,
    normalize_relative_path, path_tokens,
};
use crate::fs::FS;
use std::collections::HashMap;
use std::sync::Arc;

const DIRECTORY_PAGE_SIZE: usize = 64;

#[derive(Debug, Clone)]
pub(crate) struct DirectoryPage {
    pub(crate) path: String,
    pub(crate) entries: Vec<ExplorerEntry>,
    pub(crate) next_cursor: Option<String>,
}

#[derive(Debug, Default)]
struct LoadedDirectory {
    entries: Vec<ExplorerEntry>,
    next_cursor: Option<String>,
    loaded: bool,
}

pub(crate) struct ExplorerNavigator {
    fs: Arc<dyn FS>,
    key: String,
    password: Option<String>,
    directories: HashMap<(String, ExplorerScope), LoadedDirectory>,
    entry_details: HashMap<String, EntryHistory>,
}

impl ExplorerNavigator {
    pub(crate) fn new(fs: Arc<dyn FS>, key: String, password: Option<String>) -> Self {
        Self {
            fs,
            key,
            password,
            directories: HashMap::new(),
            entry_details: HashMap::new(),
        }
    }

    pub(crate) fn clear_cache(&mut self) {
        self.directories.clear();
        self.entry_details.clear();
    }

    pub(crate) fn page(&self, path: &str, scope: ExplorerScope) -> Option<DirectoryPage> {
        self.directories
            .get(&(path.to_string(), scope))
            .map(|directory| DirectoryPage {
                path: path.to_string(),
                entries: directory.entries.clone(),
                next_cursor: directory.next_cursor.clone(),
            })
    }

    pub(crate) fn next_cursor(&self, path: &str, scope: ExplorerScope) -> Option<String> {
        self.directories
            .get(&(path.to_string(), scope))
            .and_then(|directory| directory.next_cursor.clone())
    }

    pub(crate) async fn load_directory_page(
        &mut self,
        path: &str,
        scope: ExplorerScope,
        cursor: Option<&str>,
    ) -> Result<DirectoryPage, String> {
        let path = normalize_relative_path(path)?;
        let cache_key = (path.clone(), scope);
        if cursor.is_none()
            && self
                .directories
                .get(&cache_key)
                .is_some_and(|directory| directory.loaded)
        {
            return Ok(self.page(&path, scope).unwrap_or(DirectoryPage {
                path,
                entries: Vec::new(),
                next_cursor: None,
            }));
        }

        let catalog_page = list_directory_children(
            Arc::clone(&self.fs),
            self.key.clone(),
            self.password.clone(),
            &path,
            scope.catalog_scope(),
            cursor,
            DIRECTORY_PAGE_SIZE,
        )
        .await?;
        let mut new_entries = Vec::with_capacity(catalog_page.items.len());
        for child in catalog_page.items {
            let child_path = if path.is_empty() {
                child.name.clone()
            } else {
                format!("{}/{}", path, child.name)
            };
            let entry = ExplorerEntry {
                entry_id: child.target_id,
                path: child_path,
                name: child.name,
                kind: ExplorerKind::from_catalog(child.kind),
                status: if child.exists_in_latest_indexed_snapshot {
                    ExplorerStatus::Current
                } else {
                    ExplorerStatus::Deleted
                },
                restorable: false,
                last_backup: None,
                latest_revision_id: None,
                size: None,
                content_type: None,
                permissions: None,
                newest_revision_timestamp: None,
            };
            let entry = if entry.is_file() {
                self.entry_details(&entry).await?.unwrap_or(entry)
            } else {
                entry
            };
            new_entries.push(entry);
        }

        let directory = self.directories.entry(cache_key).or_default();
        if cursor.is_none() {
            directory.entries = new_entries;
        } else {
            directory.entries.extend(new_entries);
        }
        directory.next_cursor = catalog_page.next_cursor;
        directory.loaded = true;

        Ok(self.page(&path, scope).unwrap_or(DirectoryPage {
            path,
            entries: Vec::new(),
            next_cursor: None,
        }))
    }

    pub(crate) async fn ensure_directory(
        &mut self,
        path: &str,
        scope: ExplorerScope,
    ) -> Result<DirectoryPage, String> {
        self.load_directory_page(path, scope, None).await
    }

    pub(crate) async fn directory_exists(&self, path: &str) -> Result<bool, String> {
        directory_exists(
            Arc::clone(&self.fs),
            self.key.clone(),
            self.password.clone(),
            path,
        )
        .await
    }

    pub(crate) async fn load_next_directory_page(
        &mut self,
        path: &str,
        scope: ExplorerScope,
    ) -> Result<Option<DirectoryPage>, String> {
        let Some(cursor) = self.next_cursor(path, scope) else {
            return Ok(None);
        };
        self.load_directory_page(path, scope, Some(&cursor))
            .await
            .map(Some)
    }

    pub(crate) async fn entry_details(
        &mut self,
        entry: &ExplorerEntry,
    ) -> Result<Option<ExplorerEntry>, String> {
        if !entry.is_file() {
            return Ok(Some(entry.clone()));
        }

        if let Some(history) = self.entry_details.get(&entry.entry_id) {
            return Ok(Some(ExplorerEntry::from_history(history)));
        }

        let Some(history) = get_entry_history(
            Arc::clone(&self.fs),
            self.key.clone(),
            self.password.clone(),
            &entry.path,
        )
        .await?
        else {
            return Ok(None);
        };
        let enriched = ExplorerEntry::from_history(&history);
        self.entry_details.insert(entry.entry_id.clone(), history);
        Ok(Some(enriched))
    }

    pub(crate) async fn history(&mut self, path: &str) -> Result<Option<EntryHistory>, String> {
        let path = normalize_file_path(path)?;
        if let Some((_, history)) = self
            .entry_details
            .iter()
            .find(|(_, history)| lookup_path(&history.path) == lookup_path(&path))
        {
            return Ok(Some(history.clone()));
        }

        let history = get_entry_history(
            Arc::clone(&self.fs),
            self.key.clone(),
            self.password.clone(),
            &path,
        )
        .await?;
        if let Some(history) = &history {
            self.entry_details
                .insert(history.entry_id.clone(), history.clone());
        }
        Ok(history)
    }

    pub(crate) async fn search(
        &mut self,
        query: &str,
        scope: ExplorerScope,
    ) -> Result<Vec<ExplorerEntry>, String> {
        let tokens = path_tokens(query.trim());
        if tokens.is_empty() {
            return Ok(Vec::new());
        }

        let summaries = collect_entries_by_tokens(
            Arc::clone(&self.fs),
            self.key.clone(),
            self.password.clone(),
            &tokens,
            scope.catalog_scope(),
        )
        .await?;
        let mut entries = summaries
            .into_iter()
            .filter_map(entry_from_summary)
            .collect::<Vec<_>>();
        entries.sort_by(|left, right| {
            right
                .newest_revision_timestamp
                .unwrap_or_default()
                .cmp(&left.newest_revision_timestamp.unwrap_or_default())
                .then_with(|| left.path.cmp(&right.path))
                .then_with(|| left.entry_id.cmp(&right.entry_id))
        });
        Ok(entries)
    }

    pub(crate) async fn descendant_files(
        &mut self,
        path: &str,
        scope: ExplorerScope,
    ) -> Result<Vec<SelectedFile>, String> {
        let entries = self.descendant_entries(path, scope).await?;
        Ok(entries
            .iter()
            .filter_map(ExplorerEntry::selected_file)
            .collect())
    }

    pub(crate) async fn reveal_path(
        &mut self,
        root_path: &str,
        target_path: &str,
        scope: ExplorerScope,
        expanded: &mut std::collections::BTreeSet<String>,
    ) -> Result<bool, String> {
        let root_path = normalize_relative_path(root_path)?;
        let target_path = normalize_relative_path(target_path)?;
        if target_path == root_path {
            return Ok(true);
        }

        let relative_target = if root_path.is_empty() {
            target_path.clone()
        } else if let Some(relative) = target_path.strip_prefix(&format!("{}/", root_path)) {
            relative.to_string()
        } else {
            return Ok(false);
        };

        let mut current_path = root_path;
        for component in relative_target.split('/') {
            if component.is_empty() {
                continue;
            }

            let found = loop {
                let page = self.ensure_directory(&current_path, scope).await?;
                let found = page
                    .entries
                    .iter()
                    .find(|entry| lookup_path(&entry.name) == lookup_path(component))
                    .cloned();
                if found.is_some() || page.next_cursor.is_none() {
                    break found;
                }
                self.load_next_directory_page(&current_path, scope).await?;
            };

            let Some(entry) = found else {
                return Ok(false);
            };
            expanded.insert(current_path.clone());
            current_path = entry.path;
        }

        Ok(true)
    }

    async fn descendant_entries(
        &mut self,
        path: &str,
        scope: ExplorerScope,
    ) -> Result<Vec<ExplorerEntry>, String> {
        let mut entries = Vec::new();
        let mut pending_directories = vec![path.to_string()];
        while let Some(directory_path) = pending_directories.pop() {
            let mut cursor = None;
            loop {
                let page = self
                    .load_directory_page(&directory_path, scope, cursor.as_deref())
                    .await?;
                let page_entries = page.entries;
                let next_cursor = page.next_cursor;

                for entry in page_entries {
                    if entry.is_file() {
                        if let Some(entry) = self.entry_details(&entry).await?
                            && entry.restorable
                        {
                            entries.push(entry);
                        }
                    } else {
                        pending_directories.push(entry.path);
                    }
                }

                match next_cursor {
                    Some(next) if cursor.as_deref() != Some(next.as_str()) => cursor = Some(next),
                    _ => break,
                }
            }
        }
        Ok(entries)
    }
}

fn entry_from_summary(summary: CatalogEntrySummary) -> Option<ExplorerEntry> {
    if summary
        .latest_restorable_backup
        .as_ref()
        .is_none_or(String::is_empty)
    {
        return None;
    }
    Some(ExplorerEntry::from_summary(
        summary.entry_id,
        summary.path,
        summary.exists_in_latest_indexed_snapshot,
        summary.latest_restorable_backup,
        summary.latest_revision_id,
        summary.size,
        summary.content_type,
        summary.permissions,
        Some(summary.newest_revision_timestamp),
    ))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::core::catalog::index_backup_after_finalize;
    use crate::core::crypto::encode_file_bytes;
    use crate::core::metadata::{Backup, BackupObject};
    use crate::fs::LocalFS;
    use crate::utils::compress_bytes;
    use std::collections::BTreeSet;
    use std::path::PathBuf;

    #[test]
    fn builds_relative_child_paths() {
        let entry = ExplorerEntry {
            entry_id: "id".to_string(),
            path: "folder/file.txt".to_string(),
            name: "file.txt".to_string(),
            kind: ExplorerKind::File,
            status: ExplorerStatus::Current,
            restorable: true,
            last_backup: Some("backup".to_string()),
            latest_revision_id: Some("revision".to_string()),
            size: Some(3),
            content_type: Some("text/plain".to_string()),
            permissions: Some(0o644),
            newest_revision_timestamp: Some(1),
        };
        assert_eq!(entry.path, "folder/file.txt");
    }

    fn test_directory(label: &str) -> PathBuf {
        let suffix = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let path = std::env::temp_dir().join(format!("gib-explore-{label}-{suffix}"));
        std::fs::create_dir_all(&path).unwrap();
        path
    }

    fn object(hash: &str) -> BackupObject {
        BackupObject {
            hash: hash.to_string(),
            size: 1,
            content_type: "text/plain".to_string(),
            permissions: 0o644,
            chunks: vec![format!("chunk-{hash}")],
        }
    }

    fn backup(hash: &str, timestamp: u64, tree: HashMap<String, BackupObject>) -> Backup {
        Backup {
            message: hash.to_string(),
            hash: hash.to_string(),
            timestamp,
            author: "explore-test".to_string(),
            parents: Vec::new(),
            tree,
        }
    }

    async fn write_manifest(fs: &Arc<dyn FS>, key: &str, backup: &Backup) {
        let bytes = rmp_serde::to_vec_named(backup).unwrap();
        let compressed = compress_bytes(&bytes, 3);
        let encoded = encode_file_bytes(&compressed, None).unwrap();
        fs.write_file(&format!("{key}/backups/{}", backup.hash), &encoded)
            .await
            .unwrap();
    }

    #[tokio::test]
    async fn navigates_current_and_deleted_tree_entries_without_manifests() {
        let directory = test_directory("tree");
        let fs: Arc<dyn FS> = Arc::new(LocalFS::new(&directory));
        let key = "project";
        let first = backup(
            "backup-one",
            1,
            HashMap::from([
                ("current.txt".to_string(), object("current-one")),
                ("old/old.txt".to_string(), object("old-one")),
            ]),
        );
        let mut second = backup(
            "backup-two",
            2,
            HashMap::from([("current.txt".to_string(), object("current-two"))]),
        );
        second.parents = vec![first.hash.clone()];
        write_manifest(&fs, key, &first).await;
        write_manifest(&fs, key, &second).await;
        index_backup_after_finalize(
            Arc::clone(&fs),
            key.to_string(),
            None,
            3,
            &first,
            None,
            None,
        )
        .await
        .unwrap();
        index_backup_after_finalize(
            Arc::clone(&fs),
            key.to_string(),
            None,
            3,
            &second,
            Some(&first.hash),
            None,
        )
        .await
        .unwrap();

        let mut navigator = ExplorerNavigator::new(Arc::clone(&fs), key.to_string(), None);
        let all_history = navigator
            .ensure_directory("", ExplorerScope::AllHistory)
            .await
            .unwrap();
        let old_directory = all_history
            .entries
            .iter()
            .find(|entry| entry.path == "old")
            .unwrap();
        assert_eq!(old_directory.kind, ExplorerKind::Directory);
        assert_eq!(old_directory.status, ExplorerStatus::Deleted);
        assert!(navigator.directory_exists("old").await.unwrap());

        let current = navigator
            .ensure_directory("", ExplorerScope::Current)
            .await
            .unwrap();
        assert!(current.entries.iter().all(|entry| entry.path != "old"));

        let old_file = navigator
            .ensure_directory("old", ExplorerScope::AllHistory)
            .await
            .unwrap()
            .entries
            .into_iter()
            .find(|entry| entry.path == "old/old.txt")
            .unwrap();
        assert_eq!(old_file.status, ExplorerStatus::Deleted);
        assert_eq!(old_file.last_backup.as_deref(), Some("backup-one"));
        assert!(old_file.selected_file().is_some());

        let search = navigator
            .search("old", ExplorerScope::AllHistory)
            .await
            .unwrap();
        assert_eq!(
            search
                .iter()
                .map(|entry| entry.path.as_str())
                .collect::<Vec<_>>(),
            ["old/old.txt"]
        );

        let mut expanded = BTreeSet::new();
        assert!(
            navigator
                .reveal_path("", "old/old.txt", ExplorerScope::AllHistory, &mut expanded)
                .await
                .unwrap()
        );
        assert!(expanded.contains("") && expanded.contains("old"));

        let _ = std::fs::remove_dir_all(directory);
    }
}
