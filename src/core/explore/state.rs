use crate::core::catalog::{CatalogEntryScope, DirectoryChildKind, EntryHistory, FileRevision};
use std::cmp::Ordering;
use std::collections::{BTreeMap, BTreeSet};

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

    pub(crate) fn label(self) -> &'static str {
        match self {
            Self::Current => "Current",
            Self::AllHistory => "All history",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ExplorerSort {
    Name,
    Size,
    Status,
    Recent,
}

impl ExplorerSort {
    pub(crate) fn next(self) -> Self {
        match self {
            Self::Name => Self::Size,
            Self::Size => Self::Status,
            Self::Status => Self::Recent,
            Self::Recent => Self::Name,
        }
    }

    pub(crate) fn label(self) -> &'static str {
        match self {
            Self::Name => "name",
            Self::Size => "size",
            Self::Status => "status",
            Self::Recent => "recent",
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

    pub(crate) fn is_directory(&self) -> bool {
        self.kind == ExplorerKind::Directory
    }

    pub(crate) fn selected_file(&self) -> Option<SelectedFile> {
        if !self.is_file() || !self.restorable {
            return None;
        }

        Some(SelectedFile {
            entry_id: self.entry_id.clone(),
            revision_id: self
                .latest_revision_id
                .clone()
                .unwrap_or_else(|| self.entry_id.clone()),
            path: self.path.clone(),
            backup_hash: self.last_backup.clone()?,
            size: self.size.unwrap_or_default(),
        })
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

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct SelectedFile {
    pub(crate) entry_id: String,
    pub(crate) revision_id: String,
    pub(crate) path: String,
    pub(crate) backup_hash: String,
    pub(crate) size: u64,
}

impl SelectedFile {
    pub(crate) fn selection_key(&self) -> String {
        format!("{}:{}", self.entry_id, self.revision_id)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum SelectionMark {
    None,
    Partial,
    Selected,
}

#[derive(Debug)]
pub(crate) struct ExplorerState {
    pub(crate) root_path: String,
    pub(crate) scope: ExplorerScope,
    pub(crate) sort: ExplorerSort,
    pub(crate) focus_path: String,
    pub(crate) expanded: BTreeSet<String>,
    pub(crate) selected: BTreeMap<String, SelectedFile>,
    pub(crate) search_query: Option<String>,
}

impl Default for ExplorerState {
    fn default() -> Self {
        Self::new(String::new(), ExplorerScope::AllHistory, ExplorerSort::Name)
    }
}

impl ExplorerState {
    pub(crate) fn new(root_path: String, scope: ExplorerScope, sort: ExplorerSort) -> Self {
        let mut expanded = BTreeSet::new();
        expanded.insert(root_path.clone());
        Self {
            root_path: root_path.clone(),
            scope,
            sort,
            focus_path: root_path,
            expanded,
            selected: BTreeMap::new(),
            search_query: None,
        }
    }

    pub(crate) fn toggle_scope(&mut self) {
        self.scope = match self.scope {
            ExplorerScope::Current => ExplorerScope::AllHistory,
            ExplorerScope::AllHistory => ExplorerScope::Current,
        };
    }

    pub(crate) fn set_focus(&mut self, path: String) {
        self.focus_path = path;
    }

    pub(crate) fn toggle_expanded(&mut self, path: &str) {
        if !self.expanded.insert(path.to_string()) {
            self.expanded.remove(path);
        }
    }

    pub(crate) fn collapse(&mut self, path: &str) {
        self.expanded.remove(path);
    }

    pub(crate) fn select_file(&mut self, file: SelectedFile) {
        let key = file.selection_key();
        if self.selected.contains_key(&key) {
            self.selected.remove(&key);
            return;
        }

        self.remove_selected_entry(&file.entry_id);
        self.selected.insert(key, file);
    }

    pub(crate) fn select_directory(&mut self, files: Vec<SelectedFile>) {
        if files.is_empty() {
            return;
        }

        let all_selected = files.iter().all(|file| {
            self.selected
                .values()
                .any(|selected| selected.entry_id == file.entry_id)
        });
        if all_selected {
            for file in files {
                self.remove_selected_entry(&file.entry_id);
            }
        } else {
            for file in files {
                self.remove_selected_entry(&file.entry_id);
                self.selected.insert(file.selection_key(), file);
            }
        }
    }

    pub(crate) fn remove_selected_entry(&mut self, entry_id: &str) {
        self.selected
            .retain(|_, selected| selected.entry_id != entry_id);
    }

    pub(crate) fn clear_selection(&mut self) {
        self.selected.clear();
    }

    pub(crate) fn is_selected(&self, entry_id: &str) -> bool {
        self.selected
            .values()
            .any(|selected| selected.entry_id == entry_id)
    }

    pub(crate) fn selection_mark(
        &self,
        entry: &ExplorerEntry,
        descendant_files: &[SelectedFile],
    ) -> SelectionMark {
        if entry.is_file() {
            return if self.is_selected(&entry.entry_id) {
                SelectionMark::Selected
            } else {
                SelectionMark::None
            };
        }

        if descendant_files.is_empty() {
            return SelectionMark::None;
        }

        let selected_count = descendant_files
            .iter()
            .filter(|file| self.is_selected(&file.entry_id))
            .count();
        if selected_count == 0 {
            SelectionMark::None
        } else if selected_count == descendant_files.len() {
            SelectionMark::Selected
        } else {
            SelectionMark::Partial
        }
    }

    pub(crate) fn selected_files(&self) -> Vec<SelectedFile> {
        self.selected.values().cloned().collect()
    }

    pub(crate) fn selected_count(&self) -> usize {
        self.selected.len()
    }

    pub(crate) fn selected_source_count(&self) -> usize {
        self.selected
            .values()
            .map(|file| file.backup_hash.as_str())
            .collect::<BTreeSet<_>>()
            .len()
    }

    pub(crate) fn selected_size(&self) -> u64 {
        self.selected.values().map(|file| file.size).sum()
    }
}

#[allow(dead_code)]
pub(crate) fn derive_directory_status(children: &[ExplorerEntry]) -> Option<ExplorerStatus> {
    if children
        .iter()
        .any(|child| child.status == ExplorerStatus::Current)
    {
        Some(ExplorerStatus::Current)
    } else if children
        .iter()
        .any(|child| child.status == ExplorerStatus::Deleted)
    {
        Some(ExplorerStatus::Deleted)
    } else {
        None
    }
}

pub(crate) fn scope_accepts(scope: ExplorerScope, status: ExplorerStatus) -> bool {
    scope == ExplorerScope::AllHistory || status == ExplorerStatus::Current
}

pub(crate) fn sort_entries(entries: &mut [ExplorerEntry], sort: ExplorerSort) {
    entries.sort_by(|left, right| {
        let kind_order = kind_order(left.kind).cmp(&kind_order(right.kind));
        if kind_order != Ordering::Equal {
            return kind_order;
        }

        let order = match sort {
            ExplorerSort::Name => Ordering::Equal,
            ExplorerSort::Size => right
                .size
                .unwrap_or_default()
                .cmp(&left.size.unwrap_or_default()),
            ExplorerSort::Status => status_order(left.status).cmp(&status_order(right.status)),
            ExplorerSort::Recent => right
                .newest_revision_timestamp
                .unwrap_or_default()
                .cmp(&left.newest_revision_timestamp.unwrap_or_default()),
        };
        order
            .then_with(|| left.name.to_lowercase().cmp(&right.name.to_lowercase()))
            .then_with(|| left.name.cmp(&right.name))
            .then_with(|| left.entry_id.cmp(&right.entry_id))
    });
}

pub(crate) fn group_selected_files(files: &[SelectedFile]) -> BTreeMap<String, Vec<SelectedFile>> {
    let mut groups = BTreeMap::<String, Vec<SelectedFile>>::new();
    for file in files {
        groups
            .entry(file.backup_hash.clone())
            .or_default()
            .push(file.clone());
    }
    for group in groups.values_mut() {
        group.sort_by(|left, right| {
            left.path
                .cmp(&right.path)
                .then_with(|| left.revision_id.cmp(&right.revision_id))
        });
    }
    groups
}

pub(crate) fn revision_to_selected_file(
    entry: &EntryHistory,
    revision: &FileRevision,
) -> Option<SelectedFile> {
    Some(SelectedFile {
        entry_id: entry.entry_id.clone(),
        revision_id: revision.revision_id.clone(),
        path: entry.path.clone(),
        backup_hash: revision.latest_restorable_backup.clone()?,
        size: revision.size,
    })
}

fn latest_restorable_revision(entry: &EntryHistory) -> Option<&FileRevision> {
    entry
        .revisions
        .iter()
        .rev()
        .find(|revision| revision.latest_restorable_backup.is_some())
}

fn kind_order(kind: ExplorerKind) -> u8 {
    match kind {
        ExplorerKind::Directory => 0,
        ExplorerKind::File => 1,
    }
}

fn status_order(status: ExplorerStatus) -> u8 {
    match status {
        ExplorerStatus::Current => 0,
        ExplorerStatus::Deleted => 1,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn file(
        entry_id: &str,
        path: &str,
        status: ExplorerStatus,
        timestamp: u64,
        size: u64,
    ) -> ExplorerEntry {
        ExplorerEntry {
            entry_id: entry_id.to_string(),
            path: path.to_string(),
            name: path.rsplit('/').next().unwrap().to_string(),
            kind: ExplorerKind::File,
            status,
            restorable: true,
            last_backup: Some("backup".to_string()),
            latest_revision_id: Some(format!("revision-{entry_id}")),
            size: Some(size),
            content_type: Some("text/plain".to_string()),
            permissions: Some(0o644),
            newest_revision_timestamp: Some(timestamp),
        }
    }

    fn selected(entry_id: &str, path: &str, backup: &str) -> SelectedFile {
        SelectedFile {
            entry_id: entry_id.to_string(),
            revision_id: format!("revision-{entry_id}"),
            path: path.to_string(),
            backup_hash: backup.to_string(),
            size: 10,
        }
    }

    #[test]
    fn derives_directory_status_from_descendants() {
        let deleted = file("deleted", "old.txt", ExplorerStatus::Deleted, 1, 10);
        let current = file("current", "new.txt", ExplorerStatus::Current, 2, 20);
        assert_eq!(
            derive_directory_status(std::slice::from_ref(&deleted)),
            Some(ExplorerStatus::Deleted)
        );
        assert_eq!(
            derive_directory_status(&[deleted, current]),
            Some(ExplorerStatus::Current)
        );
        assert_eq!(derive_directory_status(&[]), None);
    }

    #[test]
    fn scope_filters_deleted_entries_only_in_all_history() {
        assert!(scope_accepts(
            ExplorerScope::Current,
            ExplorerStatus::Current
        ));
        assert!(!scope_accepts(
            ExplorerScope::Current,
            ExplorerStatus::Deleted
        ));
        assert!(scope_accepts(
            ExplorerScope::AllHistory,
            ExplorerStatus::Deleted
        ));
    }

    #[test]
    fn sorts_directories_first_and_supports_alternative_orders() {
        let mut entries = vec![
            file("a", "z.txt", ExplorerStatus::Deleted, 10, 20),
            file("b", "a.txt", ExplorerStatus::Current, 5, 50),
        ];
        entries.push(ExplorerEntry {
            entry_id: "directory".to_string(),
            path: "folder".to_string(),
            name: "folder".to_string(),
            kind: ExplorerKind::Directory,
            status: ExplorerStatus::Current,
            restorable: true,
            last_backup: None,
            latest_revision_id: None,
            size: Some(1),
            content_type: None,
            permissions: None,
            newest_revision_timestamp: Some(1),
        });

        sort_entries(&mut entries, ExplorerSort::Size);
        assert_eq!(entries[0].kind, ExplorerKind::Directory);
        assert_eq!(entries[1].name, "a.txt");

        sort_entries(&mut entries, ExplorerSort::Recent);
        assert_eq!(entries[1].name, "z.txt");
    }

    #[test]
    fn selection_tracks_stable_entry_and_revision_ids_and_partial_state() {
        let mut state =
            ExplorerState::new(String::new(), ExplorerScope::AllHistory, ExplorerSort::Name);
        let first = selected("one", "one.txt", "backup-one");
        let second = selected("two", "two.txt", "backup-two");
        state.select_directory(vec![first.clone(), second.clone()]);
        assert_eq!(state.selected_count(), 2);
        assert_eq!(state.selected_source_count(), 2);
        assert_eq!(state.selected_size(), 20);

        let directory = ExplorerEntry {
            entry_id: "root".to_string(),
            path: String::new(),
            name: String::new(),
            kind: ExplorerKind::Directory,
            status: ExplorerStatus::Current,
            restorable: true,
            last_backup: None,
            latest_revision_id: None,
            size: None,
            content_type: None,
            permissions: None,
            newest_revision_timestamp: None,
        };
        assert_eq!(
            state.selection_mark(&directory, &[first.clone(), second.clone()]),
            SelectionMark::Selected
        );
        state.select_file(first.clone());
        assert_eq!(state.selected_count(), 1);
        assert_eq!(
            state.selection_mark(&directory, &[first, second]),
            SelectionMark::Partial
        );
        assert_eq!(
            group_selected_files(&state.selected_files())
                .keys()
                .next()
                .unwrap(),
            "backup-two"
        );
    }
}
