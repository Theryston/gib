use crate::commands::restore::restore_selected_paths;
use crate::config::{
    PasswordPolicy, RepositoryOptions, load_and_report_local_config, load_local_config,
    resolve_path, resolve_repository,
};
use crate::core::catalog::{
    CatalogState, CatalogStatus, CurrentSnapshot, EntryHistory, FileRevision,
    load_latest_parentless_snapshot, lookup_path, normalize_file_path, normalize_relative_path,
    parent_directory, read_catalog_status,
};
use crate::core::explore::{
    ExplorerEntry, ExplorerKind, ExplorerNavigator, ExplorerScope, ExplorerSort, ExplorerState,
    ExplorerStatus, SelectedFile, SelectionMark, group_selected_files, revision_to_selected_file,
    scope_accepts, sort_entries,
};
use crate::core::only::TerminalGuard;
use crate::core::restore::{RestoreFailure, RestoreProgressCallback};
use crate::output::{JsonProgress, emit_output, emit_warning, is_json_mode};
use crate::utils::handle_error;
use bytesize::ByteSize;
use clap::ArgMatches;
use crossterm::{
    cursor,
    event::{self, Event, KeyCode, KeyEventKind, KeyModifiers},
    queue,
    terminal::{self, ClearType},
};
use dialoguer::{Confirm, Input};
use indicatif::{ProgressBar, ProgressStyle};
use serde::Serialize;
use std::collections::{BTreeMap, BTreeSet};
use std::io::{self, Write};
use std::sync::Arc;
use std::time::{Duration, Instant};

const DEFAULT_EXPLORE_LIMIT: usize = 100;
const NO_INDEXED_BACKUPS_MESSAGE: &str = "No indexed backups yet. New backups are indexed automatically; older snapshots remain restorable with 'gib restore'.";
const DEGRADED_INDEX_WARNING: &str = "The historical catalog is degraded; some entries may be incomplete until pending backups are indexed.";

#[derive(Debug, Clone)]
struct ExploreRequest {
    path: String,
    scope: ExplorerScope,
    query: Option<String>,
    history: bool,
    restore: bool,
    selected_paths: Vec<String>,
    revisions: BTreeMap<String, String>,
    target_path: String,
    cursor: Option<String>,
    limit: usize,
    sort: ExplorerSort,
}

#[derive(Debug, Clone)]
struct SelectionPlan {
    requested_paths: Vec<String>,
    files: Vec<SelectedFile>,
    unavailable: Vec<String>,
}

#[derive(Debug, Clone, Copy, Serialize)]
#[serde(rename_all = "snake_case")]
enum ExploreIndexStatus {
    Ready,
    Degraded,
    NoIndexedBackups,
}

#[derive(Debug, Clone, Serialize)]
struct ExploreDirectoryResponse {
    path: String,
    scope: String,
    entries: Vec<ExploreDirectoryEntry>,
    next_cursor: Option<String>,
    index_status: ExploreIndexStatus,
    #[serde(skip_serializing_if = "Option::is_none")]
    warning: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    message: Option<String>,
}

#[derive(Debug, Clone, Serialize)]
struct ExploreDirectoryEntry {
    name: String,
    path: String,
    kind: String,
    status: String,
    restorable: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    last_backup: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    size: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    content_type: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    permissions: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    entry_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    latest_revision_id: Option<String>,
}

#[derive(Debug, Clone, Serialize)]
struct ExploreSearchResponse {
    query: String,
    scope: String,
    results: Vec<ExploreSearchResult>,
    next_cursor: Option<String>,
    truncated: bool,
    index_status: ExploreIndexStatus,
    #[serde(skip_serializing_if = "Option::is_none")]
    warning: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    message: Option<String>,
}

#[derive(Debug, Clone, Serialize)]
struct ExploreSearchResult {
    name: String,
    path: String,
    kind: String,
    status: String,
    last_backup: Option<String>,
    restorable: bool,
    size: Option<u64>,
    content_type: Option<String>,
    permissions: Option<u32>,
    entry_id: String,
    latest_revision_id: Option<String>,
    restore_command: Option<String>,
}

#[derive(Debug, Clone, Serialize)]
struct ExploreFileResponse {
    path: String,
    kind: String,
    status: String,
    last_backup: Option<String>,
    restorable: bool,
    size: Option<u64>,
    content_type: Option<String>,
    permissions: Option<u32>,
    selected: bool,
    entry_id: String,
    latest_revision_id: Option<String>,
    index_status: ExploreIndexStatus,
    #[serde(skip_serializing_if = "Option::is_none")]
    warning: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    message: Option<String>,
}

#[derive(Debug, Clone, Serialize)]
struct ExploreHistoryResponse {
    path: String,
    kind: String,
    status: String,
    last_backup: Option<String>,
    revisions: Vec<ExploreRevision>,
    index_status: ExploreIndexStatus,
    #[serde(skip_serializing_if = "Option::is_none")]
    warning: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    message: Option<String>,
}

#[derive(Debug, Clone, Serialize)]
struct ExploreRevision {
    revision_id: String,
    backup: String,
    size: u64,
    restorable: bool,
    content_type: String,
    permissions: u32,
    present_from_timestamp: u64,
}

#[derive(Debug, Clone, Serialize)]
struct ExploreRestoreResponse {
    action: String,
    target_path: String,
    requested_paths: usize,
    selected_files: usize,
    source_backups: usize,
    total_size: u64,
    restored: u64,
    skipped: u64,
    unavailable: Vec<String>,
    failed: Vec<ExploreFailure>,
    groups: Vec<ExploreRestoreGroup>,
    elapsed_ms: u64,
    index_status: ExploreIndexStatus,
    #[serde(skip_serializing_if = "Option::is_none")]
    warning: Option<String>,
}

#[derive(Debug, Clone, Serialize)]
struct ExploreRestoreGroup {
    backup: String,
    selected_files: usize,
    restored: u64,
    skipped: u64,
    unavailable: Vec<String>,
    failed: Vec<ExploreFailure>,
}

#[derive(Debug, Clone, Serialize)]
struct ExploreFailure {
    path: String,
    message: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum TuiAction {
    Exit,
    Restore(Vec<SelectedFile>),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum InputMode {
    Normal,
    Search,
    Jump,
}

struct ExplorerController {
    navigator: ExplorerNavigator,
    state: ExplorerState,
    search_results: Option<Vec<ExplorerEntry>>,
    selection_marks: BTreeMap<String, SelectionMark>,
    warning: Option<String>,
    message: Option<String>,
}

#[derive(Debug, Clone)]
struct VisibleRow {
    entry: ExplorerEntry,
    depth: usize,
}

pub async fn explore(matches: &ArgMatches) {
    let request = match parse_request(matches) {
        Ok(request) => request,
        Err(error) => handle_error(error, None),
    };

    let (repository, target_path) = match get_params(matches) {
        Ok(params) => params,
        Err(error) => handle_error(error, None),
    };

    let catalog_status = match read_catalog_status(
        Arc::clone(&repository.fs),
        repository.key.clone(),
        repository.password.clone(),
    )
    .await
    {
        Ok(status) => status,
        Err(error) => handle_error(error, None),
    };
    let current_snapshot = match load_latest_parentless_snapshot(
        Arc::clone(&repository.fs),
        repository.key.clone(),
        repository.password.clone(),
    )
    .await
    {
        Ok(snapshot) => snapshot,
        Err(error) => handle_error(error, None),
    };

    let mut request = request;
    request.target_path = target_path;

    if is_json_mode() {
        if let Err(error) = run_json(repository, request, catalog_status, current_snapshot).await {
            handle_error(error, None);
        }
    } else if let Err(error) =
        run_interactive(repository, request, catalog_status, current_snapshot).await
    {
        handle_error(error, None);
    }
}

fn get_params(matches: &ArgMatches) -> Result<(RepositoryOptions, String), String> {
    let local_config = if is_json_mode() {
        load_local_config(matches)?
    } else {
        load_and_report_local_config(matches)?
    };
    let repository = resolve_repository(
        matches,
        &local_config,
        PasswordPolicy {
            required: false,
            readonly: true,
        },
        None,
    )?;
    let target_path = resolve_path(
        matches.get_one::<String>("target-path"),
        local_config.config.restore.target_path.as_ref(),
        &local_config,
    )?;
    Ok((repository, target_path))
}

fn parse_request(matches: &ArgMatches) -> Result<ExploreRequest, String> {
    let path = normalize_relative_path(
        matches
            .get_one::<String>("path")
            .map(String::as_str)
            .unwrap_or_default(),
    )?;
    let scope = match matches
        .get_one::<String>("scope")
        .map(String::as_str)
        .unwrap_or("all-history")
    {
        "current" => ExplorerScope::Current,
        "all-history" => ExplorerScope::AllHistory,
        value => return Err(format!("Invalid explore scope '{}'.", value)),
    };
    let query = matches
        .get_one::<String>("query")
        .map(|value| value.trim().to_string());
    if query.as_deref().is_some_and(str::is_empty) {
        return Err("The --query value cannot be empty.".to_string());
    }
    if query.is_some() && matches.get_flag("history") {
        return Err("--query cannot be used together with --history.".to_string());
    }

    let selected_paths = matches
        .get_many::<String>("select")
        .map(|values| {
            values
                .map(|value| normalize_file_path(value))
                .collect::<Result<Vec<_>, _>>()
        })
        .transpose()?
        .unwrap_or_default();

    let mut revisions = BTreeMap::new();
    if let Some(values) = matches.get_many::<String>("revision") {
        for value in values {
            let (path_value, backup) = match value.split_once('=') {
                Some((path, backup)) => (path, backup),
                None if !path.is_empty() => (path.as_str(), value.as_str()),
                None => {
                    return Err(format!(
                        "Invalid --revision '{}': expected PATH=BACKUP or a backup reference with --path.",
                        value
                    ));
                }
            };
            let path = normalize_file_path(path_value)?;
            let backup = backup.trim();
            if backup.is_empty() {
                return Err(format!("Invalid --revision '{}': backup is empty.", value));
            }
            if revisions.insert(path.clone(), backup.to_string()).is_some() {
                return Err(format!(
                    "More than one revision was supplied for '{}'.",
                    path
                ));
            }
        }
    }

    let restore = matches.get_flag("restore");
    if restore && query.is_some() {
        return Err("--query cannot be used together with --restore.".to_string());
    }
    if restore && matches.get_flag("history") {
        return Err("--history cannot be used together with --restore.".to_string());
    }
    if !restore && (!selected_paths.is_empty() || !revisions.is_empty()) {
        return Err("--select and --revision require --restore.".to_string());
    }

    let limit = matches
        .get_one::<usize>("limit")
        .copied()
        .unwrap_or(DEFAULT_EXPLORE_LIMIT);
    if limit == 0 {
        return Err("--limit must be greater than zero.".to_string());
    }

    let sort = match matches
        .get_one::<String>("sort")
        .map(String::as_str)
        .unwrap_or("name")
    {
        "name" => ExplorerSort::Name,
        "size" => ExplorerSort::Size,
        "status" => ExplorerSort::Status,
        "recent" => ExplorerSort::Recent,
        value => return Err(format!("Invalid explore sort '{}'.", value)),
    };

    let cursor = matches
        .get_one::<String>("cursor")
        .map(|value| value.trim().to_string())
        .filter(|value| !value.is_empty());

    if matches.get_flag("history") && path.is_empty() {
        return Err("--history requires a non-empty --path to a file.".to_string());
    }

    Ok(ExploreRequest {
        path,
        scope,
        query,
        history: matches.get_flag("history"),
        restore,
        selected_paths,
        revisions,
        target_path: String::new(),
        cursor,
        limit,
        sort,
    })
}

fn catalog_has_indexed_backups(status: Option<&CatalogStatus>) -> bool {
    status.is_some_and(|status| {
        status.indexed_backup_count > 0 && status.latest_indexed_backup.is_some()
    })
}

fn index_status(status: Option<&CatalogStatus>) -> ExploreIndexStatus {
    match status {
        None => ExploreIndexStatus::NoIndexedBackups,
        Some(status) if !catalog_has_indexed_backups(Some(status)) => {
            ExploreIndexStatus::NoIndexedBackups
        }
        Some(status) if status.state == CatalogState::Degraded => ExploreIndexStatus::Degraded,
        Some(_) => ExploreIndexStatus::Ready,
    }
}

fn index_warning(status: Option<&CatalogStatus>) -> Option<String> {
    status
        .is_some_and(|status| status.state == CatalogState::Degraded)
        .then(|| DEGRADED_INDEX_WARNING.to_string())
}

async fn run_json(
    repository: RepositoryOptions,
    request: ExploreRequest,
    catalog_status: Option<CatalogStatus>,
    current_snapshot: Option<CurrentSnapshot>,
) -> Result<(), String> {
    let status = index_status(catalog_status.as_ref());
    let warning = index_warning(catalog_status.as_ref());
    if !catalog_has_indexed_backups(catalog_status.as_ref()) {
        emit_empty_json(&request, status, warning);
        return Ok(());
    }

    let mut navigator = ExplorerNavigator::new(
        Arc::clone(&repository.fs),
        repository.key.clone(),
        repository.password.clone(),
    );
    navigator.set_current_snapshot(current_snapshot);

    if request.restore {
        return run_json_restore(&mut navigator, &repository, &request, status, warning).await;
    }

    if let Some(query) = &request.query {
        let mut entries = navigator.search(query, request.scope).await?;
        if !request.path.is_empty() {
            let prefix = lookup_path(&request.path);
            entries.retain(|entry| {
                let path = lookup_path(&entry.path);
                path == prefix || path.starts_with(&format!("{}/", prefix))
            });
        }
        return emit_json_search(&request, entries, status, warning);
    }

    if request.history {
        let history = navigator
            .history(&request.path)
            .await?
            .ok_or_else(|| format!("No catalog history found for '{}'.", request.path))?;
        if !scope_accepts(request.scope, ExplorerEntry::from_history(&history).status) {
            let mut response = history_payload(&request, &history, status, warning);
            response.message = Some(format!(
                "'{}' is not present in the current scope.",
                request.path
            ));
            response.revisions.clear();
            emit_output(&response);
            return Ok(());
        }
        return emit_json_history(&request, &history, status, warning);
    }

    if !request.path.is_empty()
        && !navigator.directory_exists(&request.path).await?
        && let Some(history) = navigator.history(&request.path).await?
    {
        if !scope_accepts(request.scope, ExplorerEntry::from_history(&history).status) {
            emit_output(&ExploreDirectoryResponse {
                path: request.path,
                scope: request.scope.label().to_ascii_lowercase().replace(' ', "-"),
                entries: Vec::new(),
                next_cursor: None,
                index_status: status,
                warning,
                message: Some("The requested file is outside the current scope.".to_string()),
            });
            return Ok(());
        }
        return emit_json_file(&history, status, warning);
    }

    let page = navigator
        .load_directory_page(&request.path, request.scope, request.cursor.as_deref())
        .await?;
    debug_assert_eq!(page.path, request.path);
    let mut entries = page.entries;
    sort_entries(&mut entries, request.sort);
    emit_output(&ExploreDirectoryResponse {
        path: request.path,
        scope: request.scope.label().to_ascii_lowercase().replace(' ', "-"),
        entries: entries.iter().map(directory_entry_payload).collect(),
        next_cursor: page.next_cursor,
        index_status: status,
        warning,
        message: None,
    });
    Ok(())
}

fn emit_empty_json(request: &ExploreRequest, status: ExploreIndexStatus, warning: Option<String>) {
    let message = Some(NO_INDEXED_BACKUPS_MESSAGE.to_string());
    if let Some(query) = &request.query {
        emit_output(&ExploreSearchResponse {
            query: query.clone(),
            scope: request.scope.label().to_ascii_lowercase().replace(' ', "-"),
            results: Vec::new(),
            next_cursor: None,
            truncated: false,
            index_status: status,
            warning,
            message,
        });
    } else if request.history {
        emit_output(&ExploreHistoryResponse {
            path: request.path.clone(),
            kind: ExplorerKind::File.label().to_string(),
            status: ExplorerStatus::Deleted.label().to_string(),
            last_backup: None,
            revisions: Vec::new(),
            index_status: status,
            warning,
            message,
        });
    } else {
        emit_output(&ExploreDirectoryResponse {
            path: request.path.clone(),
            scope: request.scope.label().to_ascii_lowercase().replace(' ', "-"),
            entries: Vec::new(),
            next_cursor: None,
            index_status: status,
            warning,
            message,
        });
    }
}

fn emit_json_search(
    request: &ExploreRequest,
    mut entries: Vec<ExplorerEntry>,
    status: ExploreIndexStatus,
    warning: Option<String>,
) -> Result<(), String> {
    // Search cursors are path cursors, so keep the JSON order aligned with the
    // cursor contract even though the interactive search view is recent-first.
    entries.sort_by(|left, right| {
        left.path
            .to_lowercase()
            .cmp(&right.path.to_lowercase())
            .then_with(|| left.path.cmp(&right.path))
            .then_with(|| left.entry_id.cmp(&right.entry_id))
    });
    if let Some(cursor) = &request.cursor {
        let cursor_key = lookup_path(cursor);
        entries.retain(|entry| {
            let entry_key = lookup_path(&entry.path);
            entry_key > cursor_key || (entry_key == cursor_key && entry.path > *cursor)
        });
    }
    let truncated = entries.len() > request.limit;
    let next_cursor = truncated.then(|| entries[request.limit - 1].path.clone());
    entries.truncate(request.limit);
    emit_output(&ExploreSearchResponse {
        query: request.query.clone().unwrap_or_default(),
        scope: request.scope.label().to_ascii_lowercase().replace(' ', "-"),
        results: entries.iter().map(search_result_payload).collect(),
        next_cursor,
        truncated,
        index_status: status,
        warning,
        message: None,
    });
    Ok(())
}

fn emit_json_history(
    request: &ExploreRequest,
    history: &EntryHistory,
    status: ExploreIndexStatus,
    warning: Option<String>,
) -> Result<(), String> {
    emit_output(&history_payload(request, history, status, warning));
    Ok(())
}

fn emit_json_file(
    history: &EntryHistory,
    status: ExploreIndexStatus,
    warning: Option<String>,
) -> Result<(), String> {
    let entry = ExplorerEntry::from_history(history);
    emit_output(&ExploreFileResponse {
        path: entry.path,
        kind: entry.kind.label().to_string(),
        status: entry.status.label().to_string(),
        last_backup: entry.last_backup.as_deref().map(short_hash),
        restorable: entry.restorable,
        size: entry.size,
        content_type: entry.content_type,
        permissions: entry.permissions,
        selected: false,
        entry_id: entry.entry_id,
        latest_revision_id: entry.latest_revision_id,
        index_status: status,
        warning,
        message: None,
    });
    Ok(())
}

fn history_payload(
    _request: &ExploreRequest,
    history: &EntryHistory,
    status: ExploreIndexStatus,
    warning: Option<String>,
) -> ExploreHistoryResponse {
    let entry = ExplorerEntry::from_history(history);
    let revisions = history
        .revisions
        .iter()
        .filter(|revision| restorable_revision(revision))
        .map(history_revision_payload)
        .collect();
    ExploreHistoryResponse {
        path: history.path.clone(),
        kind: ExplorerKind::File.label().to_string(),
        status: entry.status.label().to_string(),
        last_backup: entry.last_backup.as_deref().map(short_hash),
        revisions,
        index_status: status,
        warning,
        message: None,
    }
}

fn history_revision_payload(revision: &FileRevision) -> ExploreRevision {
    ExploreRevision {
        revision_id: revision.revision_id.clone(),
        backup: revision
            .latest_restorable_backup
            .as_deref()
            .map(short_hash)
            .unwrap_or_default(),
        size: revision.size,
        restorable: restorable_revision(revision),
        content_type: revision.content_type.clone(),
        permissions: revision.permissions,
        present_from_timestamp: revision.present_from_timestamp,
    }
}

fn directory_entry_payload(entry: &ExplorerEntry) -> ExploreDirectoryEntry {
    ExploreDirectoryEntry {
        name: entry.name.clone(),
        path: entry.path.clone(),
        kind: entry.kind.label().to_string(),
        status: entry.status.label().to_string(),
        restorable: entry.restorable,
        last_backup: entry.last_backup.as_deref().map(short_hash),
        size: entry.size,
        content_type: entry.content_type.clone(),
        permissions: entry.permissions,
        entry_id: Some(entry.entry_id.clone()),
        latest_revision_id: entry.latest_revision_id.clone(),
    }
}

fn search_result_payload(entry: &ExplorerEntry) -> ExploreSearchResult {
    let backup = entry.last_backup.as_deref().map(short_hash);
    ExploreSearchResult {
        name: entry.name.clone(),
        path: entry.path.clone(),
        kind: entry.kind.label().to_string(),
        status: entry.status.label().to_string(),
        last_backup: backup.clone(),
        restorable: entry.restorable,
        size: entry.size,
        content_type: entry.content_type.clone(),
        permissions: entry.permissions,
        entry_id: entry.entry_id.clone(),
        latest_revision_id: entry.latest_revision_id.clone(),
        restore_command: backup
            .as_deref()
            .zip(Some(entry.path.as_str()))
            .map(|(backup, path)| build_restore_command(backup, path)),
    }
}

async fn run_json_restore(
    navigator: &mut ExplorerNavigator,
    repository: &RepositoryOptions,
    request: &ExploreRequest,
    status: ExploreIndexStatus,
    warning: Option<String>,
) -> Result<(), String> {
    let plan = resolve_selection(navigator, request, request.scope, request.path.as_str()).await?;
    let started_at = Instant::now();
    let groups = group_selected_files(&plan.files);
    let progress = JsonProgress::new((plan.files.len() + plan.unavailable.len()) as u64);
    progress.set_message("Restoring selected files...");
    if !plan.unavailable.is_empty() {
        progress.inc_by(plan.unavailable.len() as u64);
    }
    let callback: RestoreProgressCallback = {
        let progress = Arc::clone(&progress);
        Arc::new(move || progress.inc_by(1))
    };

    let mut report = RestoreReport::new(&plan);
    for (backup, files) in groups {
        let paths = files
            .iter()
            .map(|file| file.path.clone())
            .collect::<Vec<_>>();
        match restore_selected_paths(
            Arc::clone(&repository.fs),
            repository.key.clone(),
            repository.password.clone(),
            request.target_path.clone(),
            backup.clone(),
            &paths,
            Some(Arc::clone(&callback)),
        )
        .await
        {
            Ok(result) => {
                progress.inc_by(result.unavailable.len() as u64);
                report.add_group(backup, files.len(), result)
            }
            Err(error) => {
                progress.inc_by(files.len() as u64);
                report.add_unavailable(paths.clone());
                report.add_failed_group(backup, files.len(), paths, error);
            }
        }
    }

    if !report.unavailable.is_empty() {
        emit_warning(
            &format!(
                "Skipped {} unavailable selected entr{}.",
                report.unavailable.len(),
                if report.unavailable.len() == 1 {
                    "y"
                } else {
                    "ies"
                }
            ),
            "restore_unavailable",
        );
    }
    emit_output(&report.into_json_response(request, status, warning, started_at.elapsed()));
    Ok(())
}

async fn resolve_selection(
    navigator: &mut ExplorerNavigator,
    request: &ExploreRequest,
    scope: ExplorerScope,
    path_fallback: &str,
) -> Result<SelectionPlan, String> {
    let mut requested = BTreeSet::new();
    for path in &request.selected_paths {
        requested.insert(path.clone());
    }
    for path in request.revisions.keys() {
        requested.insert(path.clone());
    }
    if requested.is_empty() && !path_fallback.is_empty() {
        requested.insert(path_fallback.to_string());
    }
    if requested.is_empty() {
        return Err(
            "Restore requires at least one --select path (or a file/directory --path).".to_string(),
        );
    }

    let requested_paths = requested.iter().cloned().collect::<Vec<_>>();
    let mut files = BTreeMap::<String, SelectedFile>::new();
    let mut unavailable = Vec::new();

    for path in &requested_paths {
        if !navigator.directory_exists(path).await? {
            let Some(history) = navigator.history(path).await? else {
                unavailable.push(path.clone());
                continue;
            };
            let entry = ExplorerEntry::from_history(&history);
            if !scope_accepts(scope, entry.status) {
                unavailable.push(path.clone());
                continue;
            }
            let selected =
                select_history_revision(&history, request.revisions.get(path).map(String::as_str))?;
            if let Some(file) = selected {
                files.insert(file.entry_id.clone(), file);
            } else {
                unavailable.push(path.clone());
            }
            continue;
        }

        let descendant_files = navigator.descendant_files(path, scope).await?;
        if descendant_files.is_empty() {
            unavailable.push(path.clone());
            continue;
        }

        for file in descendant_files {
            let selected = if let Some(reference) = request.revisions.get(&file.path) {
                let Some(history) = navigator.history(&file.path).await? else {
                    unavailable.push(file.path.clone());
                    continue;
                };
                select_history_revision(&history, Some(reference.as_str()))?
            } else {
                Some(file)
            };
            if let Some(file) = selected {
                files.insert(file.entry_id.clone(), file);
            } else {
                unavailable.push(path.clone());
            }
        }
    }

    unavailable.sort();
    unavailable.dedup();
    Ok(SelectionPlan {
        requested_paths,
        files: files.into_values().collect(),
        unavailable,
    })
}

fn select_history_revision(
    history: &EntryHistory,
    reference: Option<&str>,
) -> Result<Option<SelectedFile>, String> {
    let revisions = history
        .revisions
        .iter()
        .filter(|revision| restorable_revision(revision))
        .collect::<Vec<_>>();
    let Some(reference) = reference else {
        return Ok(revisions
            .last()
            .and_then(|revision| revision_to_selected_file(history, revision)));
    };

    if reference.eq_ignore_ascii_case("latest") {
        return Ok(revisions
            .last()
            .and_then(|revision| revision_to_selected_file(history, revision)));
    }

    let matches = revisions
        .into_iter()
        .filter(|revision| {
            revision.revision_id.eq_ignore_ascii_case(reference)
                || revision
                    .latest_restorable_backup
                    .as_deref()
                    .is_some_and(|backup| {
                        backup.eq_ignore_ascii_case(reference) || backup.starts_with(reference)
                    })
        })
        .collect::<Vec<_>>();
    match matches.as_slice() {
        [] => Err(format!(
            "No restorable revision '{}' found for '{}'.",
            reference, history.path
        )),
        [revision] => Ok(revision_to_selected_file(history, revision)),
        _ => Err(format!(
            "Revision reference '{}' is ambiguous for '{}'.",
            reference, history.path
        )),
    }
}

fn restorable_revision(revision: &FileRevision) -> bool {
    revision
        .latest_restorable_backup
        .as_deref()
        .is_some_and(|backup| !backup.is_empty())
}

struct RestoreReport {
    requested_paths: usize,
    selected_files: usize,
    total_size: u64,
    restored: u64,
    skipped: u64,
    unavailable: BTreeSet<String>,
    failed: Vec<ExploreFailure>,
    groups: Vec<ExploreRestoreGroup>,
}

impl RestoreReport {
    fn new(plan: &SelectionPlan) -> Self {
        Self {
            requested_paths: plan.requested_paths.len(),
            selected_files: plan.files.len(),
            total_size: plan.files.iter().map(|file| file.size).sum(),
            restored: 0,
            skipped: 0,
            unavailable: plan.unavailable.iter().cloned().collect(),
            failed: Vec::new(),
            groups: Vec::new(),
        }
    }

    fn add_group(
        &mut self,
        backup: String,
        selected_files: usize,
        result: crate::commands::restore::SelectedRestoreResult,
    ) {
        self.restored += result.stats.restored;
        self.skipped += result.stats.skipped;
        let unavailable = result.unavailable;
        self.unavailable.extend(unavailable.iter().cloned());
        let failed = result
            .stats
            .failed
            .into_iter()
            .map(failure_payload)
            .collect::<Vec<_>>();
        self.failed.extend(failed.clone());
        self.groups.push(ExploreRestoreGroup {
            backup: short_hash(&backup),
            selected_files,
            restored: result.stats.restored,
            skipped: result.stats.skipped,
            unavailable,
            failed,
        });
    }

    fn add_failed_group(
        &mut self,
        backup: String,
        selected_files: usize,
        paths: Vec<String>,
        error: String,
    ) {
        let failed = ExploreFailure {
            path: paths.first().cloned().unwrap_or_default(),
            message: error,
        };
        self.failed.push(failed.clone());
        self.groups.push(ExploreRestoreGroup {
            backup: short_hash(&backup),
            selected_files,
            restored: 0,
            skipped: 0,
            unavailable: paths,
            failed: vec![failed],
        });
    }

    fn add_unavailable(&mut self, paths: Vec<String>) {
        self.unavailable.extend(paths);
    }

    fn into_json_response(
        self,
        request: &ExploreRequest,
        status: ExploreIndexStatus,
        warning: Option<String>,
        elapsed: Duration,
    ) -> ExploreRestoreResponse {
        ExploreRestoreResponse {
            action: "restore".to_string(),
            target_path: request.target_path.clone(),
            requested_paths: self.requested_paths,
            selected_files: self.selected_files,
            source_backups: self.groups.len(),
            total_size: self.total_size,
            restored: self.restored,
            skipped: self.skipped,
            unavailable: self.unavailable.into_iter().collect(),
            failed: self.failed,
            groups: self.groups,
            elapsed_ms: elapsed.as_millis() as u64,
            index_status: status,
            warning,
        }
    }
}

fn failure_payload(failure: RestoreFailure) -> ExploreFailure {
    ExploreFailure {
        path: failure.path,
        message: failure.message,
    }
}

async fn run_interactive(
    repository: RepositoryOptions,
    request: ExploreRequest,
    catalog_status: Option<CatalogStatus>,
    current_snapshot: Option<CurrentSnapshot>,
) -> Result<(), String> {
    if !catalog_has_indexed_backups(catalog_status.as_ref()) {
        println!("{}", console::style(NO_INDEXED_BACKUPS_MESSAGE).yellow());
        return Ok(());
    }

    let status_warning = index_warning(catalog_status.as_ref());
    let mut navigator = ExplorerNavigator::new(
        Arc::clone(&repository.fs),
        repository.key.clone(),
        repository.password.clone(),
    );
    navigator.set_current_snapshot(current_snapshot);

    if request.history {
        let history = navigator
            .history(&request.path)
            .await?
            .ok_or_else(|| format!("No catalog history found for '{}'.", request.path))?;
        if !scope_accepts(request.scope, ExplorerEntry::from_history(&history).status) {
            println!("{} is not present in the current scope.", request.path);
            return Ok(());
        }
        print_history(&history);
        return Ok(());
    }

    let path_is_directory =
        request.path.is_empty() || navigator.directory_exists(&request.path).await?;
    if request.restore
        && (!request.selected_paths.is_empty()
            || !request.revisions.is_empty()
            || (!request.path.is_empty() && !path_is_directory))
    {
        let plan =
            resolve_selection(&mut navigator, &request, request.scope, &request.path).await?;
        return restore_interactively(&repository, &request, plan).await;
    }

    let initial_history = if path_is_directory {
        None
    } else {
        navigator.history(&request.path).await?
    };
    if let Some(history) = &initial_history
        && !scope_accepts(request.scope, ExplorerEntry::from_history(history).status)
    {
        println!("{} is not present in the current scope.", request.path);
        return Ok(());
    }
    let root_path = initial_history
        .as_ref()
        .map(|history| parent_directory(&history.path))
        .unwrap_or_else(|| request.path.clone());
    let mut controller = ExplorerController {
        navigator,
        state: ExplorerState::new(root_path.clone(), request.scope, request.sort),
        search_results: None,
        selection_marks: BTreeMap::new(),
        warning: status_warning,
        message: None,
    };
    controller
        .navigator
        .ensure_directory(&root_path, request.scope)
        .await?;

    if let Some(history) = initial_history {
        controller
            .navigator
            .reveal_path(
                &root_path,
                &history.path,
                request.scope,
                &mut controller.state.expanded,
            )
            .await?;
        controller.state.set_focus(history.path);
    }
    if let Some(query) = &request.query {
        controller.apply_search(query.clone()).await?;
    }

    let action = {
        let _guard = TerminalGuard::new()?;
        run_tui(&mut controller).await?
    };
    match action {
        TuiAction::Exit => Ok(()),
        TuiAction::Restore(files) => {
            restore_interactively(
                &repository,
                &request,
                SelectionPlan {
                    requested_paths: vec![request.path.clone()],
                    files,
                    unavailable: Vec::new(),
                },
            )
            .await
        }
    }
}

fn print_history(history: &EntryHistory) {
    println!("{} — History", history.path);
    let revisions = history
        .revisions
        .iter()
        .filter(|revision| restorable_revision(revision))
        .collect::<Vec<_>>();
    if revisions.is_empty() {
        println!("  No restorable revisions are available.");
        return;
    }
    for (index, revision) in revisions.iter().rev().enumerate() {
        let label = if index == 0 {
            "latest available revision"
        } else {
            "older revision"
        };
        println!(
            "  {} {} · {}",
            short_hash(
                revision
                    .latest_restorable_backup
                    .as_deref()
                    .unwrap_or_default()
            ),
            label,
            ByteSize(revision.size)
        );
    }
}

async fn restore_interactively(
    repository: &RepositoryOptions,
    request: &ExploreRequest,
    plan: SelectionPlan,
) -> Result<(), String> {
    if plan.files.is_empty() {
        println!("No selected files are currently restorable.");
        for path in plan.unavailable {
            println!("  unavailable: {}", path);
        }
        return Ok(());
    }

    let groups = group_selected_files(&plan.files);
    let total_size = plan.files.iter().map(|file| file.size).sum::<u64>();
    println!(
        "Restore {} files ({}) from {} backup group{} to {}",
        plan.files.len(),
        ByteSize(total_size),
        groups.len(),
        if groups.len() == 1 { "" } else { "s" },
        request.target_path
    );
    if !plan.unavailable.is_empty() {
        println!(
            "{} catalog entr{} will be skipped.",
            plan.unavailable.len(),
            if plan.unavailable.len() == 1 {
                "y"
            } else {
                "ies"
            }
        );
    }

    let target_path = Input::<String>::new()
        .with_prompt("Restore destination")
        .default(request.target_path.clone())
        .interact_text()
        .map_err(|error| format!("Failed to read restore destination: {}", error))?;
    if !Confirm::new()
        .with_prompt("Restore the selected files?")
        .default(true)
        .interact()
        .map_err(|error| format!("Failed to confirm restore: {}", error))?
    {
        println!("Restore cancelled.");
        return Ok(());
    }

    let mut restored = 0;
    let mut skipped = 0;
    let mut unavailable = plan.unavailable;
    let mut failures = Vec::new();
    for (backup, files) in groups {
        let paths = files
            .iter()
            .map(|file| file.path.clone())
            .collect::<Vec<_>>();
        let progress = ProgressBar::new(files.len() as u64);
        progress.set_style(
            ProgressStyle::with_template(
                "[{elapsed_precise}] [{bar:40.cyan/blue}] {pos}/{len} {msg}",
            )
            .map_err(|error| error.to_string())?,
        );
        progress.set_message(format!("Restoring {}", short_hash(&backup)));
        let progress_clone = progress.clone();
        let callback: RestoreProgressCallback = Arc::new(move || progress_clone.inc(1));
        match restore_selected_paths(
            Arc::clone(&repository.fs),
            repository.key.clone(),
            repository.password.clone(),
            target_path.clone(),
            backup,
            &paths,
            Some(callback),
        )
        .await
        {
            Ok(result) => {
                restored += result.stats.restored;
                skipped += result.stats.skipped;
                progress.inc(result.unavailable.len() as u64);
                unavailable.extend(result.unavailable);
                failures.extend(result.stats.failed);
            }
            Err(error) => {
                unavailable.extend(paths);
                emit_warning(
                    &format!("Restore group unavailable: {}", error),
                    "restore_unavailable",
                );
            }
        }
        progress.finish_and_clear();
    }

    if failures.is_empty() {
        println!("Restored {}, skipped {}.", restored, skipped);
    } else {
        println!(
            "Restored {}, skipped {}, failed {}.",
            restored,
            skipped,
            failures.len()
        );
        for failure in failures {
            println!("  failed: {}: {}", failure.path, failure.message);
        }
    }
    if !unavailable.is_empty() {
        unavailable.sort();
        unavailable.dedup();
        for path in unavailable {
            println!("  unavailable: {}", path);
        }
    }
    Ok(())
}

impl ExplorerController {
    async fn apply_search(&mut self, query: String) -> Result<(), String> {
        let results = self.navigator.search(&query, self.state.scope).await?;
        self.state.search_query = Some(query);
        self.search_results = Some(results);
        self.message = None;
        Ok(())
    }

    fn clear_search(&mut self) {
        self.state.search_query = None;
        self.search_results = None;
    }

    fn invalidate_selection_marks(&mut self) {
        self.selection_marks.clear();
    }

    async fn prepare_selection_marks(&mut self, rows: &[VisibleRow]) -> Result<(), String> {
        for row in rows.iter().filter(|row| row.entry.is_directory()) {
            let prefix = if row.entry.path.is_empty() {
                String::new()
            } else {
                format!("{}/", row.entry.path)
            };
            let has_selected_descendant = self
                .state
                .selected
                .values()
                .any(|file| file.path.starts_with(&prefix));
            if !has_selected_descendant {
                self.selection_marks
                    .insert(row.entry.path.clone(), SelectionMark::None);
                continue;
            }

            let files = self
                .navigator
                .descendant_files(&row.entry.path, self.state.scope)
                .await?;
            let mark = self.state.selection_mark(&row.entry, &files);
            self.selection_marks.insert(row.entry.path.clone(), mark);
        }
        Ok(())
    }

    fn visible_rows(&self) -> Vec<VisibleRow> {
        if let Some(results) = &self.search_results {
            return results
                .iter()
                .cloned()
                .map(|entry| VisibleRow { entry, depth: 0 })
                .collect();
        }

        let mut rows = Vec::new();
        self.append_visible_rows(&self.state.root_path, 0, &mut rows);
        rows
    }

    fn append_visible_rows(&self, path: &str, depth: usize, rows: &mut Vec<VisibleRow>) {
        let Some(page) = self.navigator.page(path, self.state.scope) else {
            return;
        };
        let mut entries = page.entries;
        sort_entries(&mut entries, self.state.sort);
        for entry in entries {
            let is_directory = entry.is_directory();
            let child_path = entry.path.clone();
            rows.push(VisibleRow { entry, depth });
            if is_directory && self.state.expanded.contains(&child_path) {
                self.append_visible_rows(&child_path, depth + 1, rows);
            }
        }
    }

    fn focused_entry(&self, rows: &[VisibleRow]) -> Option<ExplorerEntry> {
        rows.iter()
            .find(|row| row.entry.path == self.state.focus_path)
            .map(|row| row.entry.clone())
            .or_else(|| rows.first().map(|row| row.entry.clone()))
    }

    async fn toggle_scope(&mut self) -> Result<(), String> {
        let focus = self.state.focus_path.clone();
        self.state.toggle_scope();
        self.clear_search();
        self.invalidate_selection_marks();
        self.navigator.clear_cache();
        self.navigator
            .ensure_directory(&self.state.root_path, self.state.scope)
            .await?;
        if !self
            .navigator
            .reveal_path(
                &self.state.root_path,
                &focus,
                self.state.scope,
                &mut self.state.expanded,
            )
            .await?
        {
            self.state.set_focus(self.state.root_path.clone());
        }
        Ok(())
    }

    async fn refresh(&mut self) -> Result<(), String> {
        let focus = self.state.focus_path.clone();
        self.invalidate_selection_marks();
        self.navigator.clear_cache();
        self.navigator
            .ensure_directory(&self.state.root_path, self.state.scope)
            .await?;
        let revealed = self
            .navigator
            .reveal_path(
                &self.state.root_path,
                &focus,
                self.state.scope,
                &mut self.state.expanded,
            )
            .await?;
        if !revealed {
            self.state.set_focus(self.state.root_path.clone());
        }
        Ok(())
    }

    async fn open_focused_directory(&mut self, entry: &ExplorerEntry) -> Result<(), String> {
        if !entry.is_directory() {
            return Ok(());
        }
        if !self.state.expanded.contains(&entry.path) {
            self.state.toggle_expanded(&entry.path);
        }
        self.navigator
            .ensure_directory(&entry.path, self.state.scope)
            .await?;
        Ok(())
    }

    async fn select_focused(&mut self, entry: &ExplorerEntry) -> Result<(), String> {
        if entry.is_file() {
            let Some(entry) = self.navigator.entry_details(entry).await? else {
                self.message = Some("This catalog entry is no longer available.".to_string());
                return Ok(());
            };
            let Some(file) = entry.selected_file() else {
                self.message = Some("This file has no restorable revision.".to_string());
                return Ok(());
            };
            self.state.select_file(file);
        } else {
            let files = self
                .navigator
                .descendant_files(&entry.path, self.state.scope)
                .await?;
            if files.is_empty() {
                self.message = Some("This directory has no restorable files.".to_string());
            } else {
                self.state.select_directory(files);
            }
        }
        self.invalidate_selection_marks();
        Ok(())
    }

    async fn focused_restore_files(
        &mut self,
        rows: &[VisibleRow],
    ) -> Result<Vec<SelectedFile>, String> {
        let selected = self.state.selected_files();
        if !selected.is_empty() {
            return Ok(selected);
        }
        let Some(entry) = self.focused_entry(rows) else {
            return Ok(Vec::new());
        };
        if entry.is_file() {
            return Ok(self
                .navigator
                .entry_details(&entry)
                .await?
                .and_then(|entry| entry.selected_file())
                .into_iter()
                .collect());
        }
        self.navigator
            .descendant_files(&entry.path, self.state.scope)
            .await
    }
}

async fn run_tui(controller: &mut ExplorerController) -> Result<TuiAction, String> {
    let mut input_mode = InputMode::Normal;
    let mut input = String::new();
    let mut scroll_offset = 0usize;

    loop {
        let rows = controller.visible_rows();
        controller.prepare_selection_marks(&rows).await?;
        let mut cursor_index = rows
            .iter()
            .position(|row| row.entry.path == controller.state.focus_path)
            .unwrap_or(0);
        if rows.is_empty() {
            cursor_index = 0;
        }
        render_tui(
            controller,
            &rows,
            cursor_index,
            &mut scroll_offset,
            input_mode,
            &input,
        )?;

        let event = event::read().map_err(|error| format!("Failed to read input: {}", error))?;
        let Event::Key(key) = event else {
            continue;
        };
        if key.kind == KeyEventKind::Release {
            continue;
        }
        if key.modifiers.contains(KeyModifiers::CONTROL) && key.code == KeyCode::Char('c') {
            return Ok(TuiAction::Exit);
        }

        match input_mode {
            InputMode::Search | InputMode::Jump => {
                match key.code {
                    KeyCode::Esc => {
                        input_mode = InputMode::Normal;
                        input.clear();
                        if controller.state.search_query.is_some() {
                            controller.clear_search();
                        }
                    }
                    KeyCode::Enter => {
                        if input_mode == InputMode::Search {
                            if input.trim().is_empty() {
                                controller.clear_search();
                            } else {
                                controller.apply_search(input.trim().to_string()).await?;
                            }
                        } else if !input.trim().is_empty() {
                            let target = normalize_relative_path(input.trim())?;
                            controller.clear_search();
                            if controller
                                .navigator
                                .reveal_path(
                                    &controller.state.root_path,
                                    &target,
                                    controller.state.scope,
                                    &mut controller.state.expanded,
                                )
                                .await?
                            {
                                controller.state.set_focus(target);
                            } else {
                                controller.message = Some(format!("Path not found: {}", target));
                            }
                        }
                        input_mode = InputMode::Normal;
                        input.clear();
                    }
                    KeyCode::Backspace => {
                        input.pop();
                    }
                    KeyCode::Char(character)
                        if !key.modifiers.contains(KeyModifiers::CONTROL)
                            && !character.is_control() =>
                    {
                        input.push(character);
                    }
                    _ => {}
                }
                continue;
            }
            InputMode::Normal => {}
        }

        let focused = rows.get(cursor_index).map(|row| row.entry.clone());
        match key.code {
            KeyCode::Char('q') | KeyCode::Esc => return Ok(TuiAction::Exit),
            KeyCode::Char('/') => {
                input_mode = InputMode::Search;
                input = controller.state.search_query.clone().unwrap_or_default();
            }
            KeyCode::Char('g') => {
                input_mode = InputMode::Jump;
                input.clear();
            }
            KeyCode::Up | KeyCode::Char('k') => {
                if cursor_index > 0 {
                    controller
                        .state
                        .set_focus(rows[cursor_index - 1].entry.path.clone());
                }
            }
            KeyCode::Down | KeyCode::Char('j') => {
                if cursor_index + 1 < rows.len() {
                    controller
                        .state
                        .set_focus(rows[cursor_index + 1].entry.path.clone());
                }
            }
            KeyCode::PageUp => {
                if !rows.is_empty() {
                    let amount = tui_page_size()?;
                    let index = cursor_index.saturating_sub(amount);
                    controller.state.set_focus(rows[index].entry.path.clone());
                }
            }
            KeyCode::PageDown => {
                if !rows.is_empty() {
                    let amount = tui_page_size()?;
                    let index = (cursor_index + amount).min(rows.len() - 1);
                    controller.state.set_focus(rows[index].entry.path.clone());
                }
            }
            KeyCode::Enter | KeyCode::Right => {
                if let Some(entry) = &focused {
                    if entry.is_directory() {
                        controller.open_focused_directory(entry).await?;
                    } else if controller.search_results.is_some() {
                        let target = entry.path.clone();
                        controller.clear_search();
                        let _ = controller
                            .navigator
                            .reveal_path(
                                &controller.state.root_path,
                                &target,
                                controller.state.scope,
                                &mut controller.state.expanded,
                            )
                            .await?;
                        controller.state.set_focus(target);
                    }
                }
            }
            KeyCode::Left => {
                if let Some(entry) = &focused
                    && entry.is_directory()
                    && controller.state.expanded.contains(&entry.path)
                {
                    controller.state.collapse(&entry.path);
                } else if controller.state.focus_path != controller.state.root_path {
                    let parent = parent_directory(&controller.state.focus_path);
                    if parent.len() >= controller.state.root_path.len()
                        && (parent == controller.state.root_path
                            || parent.starts_with(&format!("{}/", controller.state.root_path)))
                    {
                        controller.state.set_focus(parent);
                    } else {
                        controller
                            .state
                            .set_focus(controller.state.root_path.clone());
                    }
                }
            }
            KeyCode::Tab => {
                if let Some(entry) = &focused
                    && entry.is_directory()
                {
                    if controller.state.expanded.contains(&entry.path) {
                        controller.state.collapse(&entry.path);
                    } else {
                        controller.open_focused_directory(entry).await?;
                    }
                }
            }
            KeyCode::BackTab => {
                let root = controller.state.root_path.clone();
                controller.state.expanded.clear();
                controller.state.expanded.insert(root);
            }
            KeyCode::Char(' ') => {
                if let Some(entry) = &focused {
                    controller.select_focused(entry).await?;
                }
            }
            KeyCode::Char('c') => {
                controller.state.clear_selection();
                controller.invalidate_selection_marks();
            }
            KeyCode::Char('m') => controller.toggle_scope().await?,
            KeyCode::Char('o') => controller.state.sort = controller.state.sort.next(),
            KeyCode::Char('n') => {
                let path = focused
                    .as_ref()
                    .filter(|entry| entry.is_directory())
                    .map(|entry| entry.path.clone())
                    .unwrap_or_else(|| parent_directory(&controller.state.focus_path));
                if controller
                    .navigator
                    .load_next_directory_page(&path, controller.state.scope)
                    .await?
                    .is_none()
                {
                    controller.message = Some("No more catalog entries on this path.".to_string());
                }
            }
            KeyCode::F(5) => controller.refresh().await?,
            KeyCode::Char('r') => {
                let files = controller.focused_restore_files(&rows).await?;
                if files.is_empty() {
                    controller.message = Some("No restorable files selected.".to_string());
                } else {
                    return Ok(TuiAction::Restore(files));
                }
            }
            KeyCode::Char('h') => {
                if let Some(entry) = focused
                    && entry.is_file()
                    && let Some(history) = controller.navigator.history(&entry.path).await?
                {
                    if let Some(file) = history_overlay(&history)? {
                        return Ok(TuiAction::Restore(vec![file]));
                    }
                }
            }
            KeyCode::Char('?') => render_help_overlay()?,
            _ => {}
        }
    }
}

fn render_tui(
    controller: &ExplorerController,
    rows: &[VisibleRow],
    cursor_index: usize,
    scroll_offset: &mut usize,
    input_mode: InputMode,
    input: &str,
) -> Result<(), String> {
    let (width, height) = terminal::size().map_err(|error| error.to_string())?;
    let view_height = height.saturating_sub(6) as usize;
    if view_height == 0 {
        return Err("Terminal window is too small for gib explore.".to_string());
    }
    if cursor_index < *scroll_offset {
        *scroll_offset = cursor_index;
    } else if cursor_index >= *scroll_offset + view_height {
        *scroll_offset = cursor_index + 1 - view_height;
    }

    let mut stdout = io::stdout();
    queue!(
        stdout,
        terminal::Clear(ClearType::All),
        cursor::MoveTo(0, 0)
    )
    .map_err(|error| format!("Failed to render explore: {}", error))?;
    let scope = controller.state.scope.label();
    let selected = controller.state.selected_count();
    let selected_size = ByteSize(controller.state.selected_size());
    let selected_sources = controller.state.selected_source_count();
    let header = format!(
        "gib explore · {} · Scope: {} · {} selected from {} backup{} ({}) · sort: {}",
        if controller.state.root_path.is_empty() {
            "/"
        } else {
            &controller.state.root_path
        },
        scope,
        selected,
        selected_sources,
        if selected_sources == 1 { "" } else { "s" },
        selected_size,
        controller.state.sort.label()
    );
    write_tui_line(&mut stdout, 0, 0, &header, width as usize)?;
    let subheader = if let Some(message) = &controller.message {
        message.clone()
    } else if let Some(warning) = &controller.warning {
        format!("Warning: {}", warning)
    } else if rows.is_empty() {
        if controller.search_results.is_some() {
            "No catalog entries match the current search.".to_string()
        } else {
            "No catalog entries are available at this path.".to_string()
        }
    } else {
        "Current = present in latest indexed snapshot · Deleted = historical only".to_string()
    };
    write_tui_line(&mut stdout, 0, 1, &subheader, width as usize)?;

    let left_width = ((width as usize) * 3 / 5).max(30).min(width as usize);
    let right_start = left_width.min(width as usize - 1) as u16;
    let right_width = width as usize - left_width;
    let focused_details = rows
        .get(cursor_index)
        .map(|row| detail_lines(controller, &row.entry))
        .unwrap_or_default();
    for row_index in 0..view_height {
        let screen_y = row_index + 2;
        let row = rows.get(*scroll_offset + row_index);
        let left = row
            .map(|row| {
                format_visible_row(controller, row, *scroll_offset + row_index == cursor_index)
            })
            .unwrap_or_default();
        write_tui_line(&mut stdout, 0, screen_y as u16, &left, left_width)?;
        let detail = focused_details.get(row_index).cloned().unwrap_or_default();
        write_tui_line(
            &mut stdout,
            right_start,
            screen_y as u16,
            &detail,
            right_width,
        )?;
    }

    let footer_owned;
    let footer = match input_mode {
        InputMode::Normal => {
            "↑↓/jk move · Enter/→ open · ← parent · Space select · R restore · H history · / search · G jump · M scope · O sort · N more · ? help · Q quit"
        }
        InputMode::Search => {
            footer_owned = format!("Search: {} · Enter apply · Esc cancel", input);
            footer_owned.as_str()
        }
        InputMode::Jump => {
            footer_owned = format!("Jump to path: {} · Enter go · Esc cancel", input);
            footer_owned.as_str()
        }
    };
    write_tui_line(
        &mut stdout,
        0,
        height.saturating_sub(1),
        footer,
        width as usize,
    )?;
    stdout
        .flush()
        .map_err(|error| format!("Failed to flush explore: {}", error))
}

fn format_visible_row(controller: &ExplorerController, row: &VisibleRow, focused: bool) -> String {
    let marker = match selection_mark(controller, &row.entry) {
        SelectionMark::None => " ",
        SelectionMark::Partial => "~",
        SelectionMark::Selected => "*",
    };
    let branch = if row.entry.is_directory() {
        if controller.state.expanded.contains(&row.entry.path) {
            "▾"
        } else {
            "▸"
        }
    } else {
        "·"
    };
    format!(
        "{}{} {}{} {}",
        if focused { ">" } else { " " },
        marker,
        "  ".repeat(row.depth),
        branch,
        row.entry.name
    )
}

fn detail_lines(controller: &ExplorerController, entry: &ExplorerEntry) -> Vec<String> {
    let mut lines = vec![
        format!("path: {}", entry.path),
        format!("kind: {}", entry.kind.label()),
        format!(
            "present in latest backup: {}",
            yes_no(entry.status == ExplorerStatus::Current)
        ),
        format!("restorable: {}", yes_no(entry.restorable)),
    ];
    if let Some(backup) = &entry.last_backup {
        lines.push(format!("last backup: {}", short_hash(backup)));
    }
    if let Some(size) = entry.size {
        lines.push(format!("size: {}", ByteSize(size)));
    }
    if let Some(content_type) = &entry.content_type
        && !content_type.is_empty()
    {
        lines.push(format!("content type: {}", content_type));
    }
    if let Some(permissions) = entry.permissions {
        lines.push(format!("permissions: {:o}", permissions));
    }
    lines.push(format!(
        "selected: {}",
        selection_label(selection_mark(controller, entry))
    ));
    lines
}

fn yes_no(value: bool) -> &'static str {
    if value { "yes" } else { "no" }
}

fn selection_mark(controller: &ExplorerController, entry: &ExplorerEntry) -> SelectionMark {
    if entry.is_file() {
        return controller.state.selection_mark(entry, &[]);
    }

    if let Some(mark) = controller.selection_marks.get(&entry.path) {
        return *mark;
    }

    let prefix = if entry.path.is_empty() {
        String::new()
    } else {
        format!("{}/", entry.path)
    };
    let selected_count = controller
        .state
        .selected
        .values()
        .filter(|file| file.path.starts_with(&prefix))
        .count();
    if selected_count == 0 {
        SelectionMark::None
    } else if selected_count == controller.state.selected_count() {
        SelectionMark::Selected
    } else {
        SelectionMark::Partial
    }
}

fn selection_label(mark: SelectionMark) -> &'static str {
    match mark {
        SelectionMark::None => "no",
        SelectionMark::Partial => "partial",
        SelectionMark::Selected => "yes",
    }
}

fn render_help_overlay() -> Result<(), String> {
    let (width, height) = terminal::size().map_err(|error| error.to_string())?;
    let mut stdout = io::stdout();
    queue!(
        stdout,
        terminal::Clear(ClearType::All),
        cursor::MoveTo(0, 0)
    )
    .map_err(|error| error.to_string())?;
    let lines = [
        "gib explore help",
        "",
        "Navigate the catalog with Up/Down or j/k. Enter opens a directory; Left goes to its parent.",
        "Space selects a file or all restorable descendants of a directory. c clears selections.",
        "R restores the focused entry or all selected entries after leaving the terminal UI.",
        "H shows restorable file revisions. / searches the shared catalog index; Esc restores the tree.",
        "M toggles Current/All history, O changes sorting, N loads the next lazy page, F5 refreshes.",
        "Q or Esc exits. Press any key to return.",
    ];
    for (index, line) in lines.iter().enumerate() {
        write_tui_line(&mut stdout, 0, index as u16, line, width as usize)?;
    }
    write_tui_line(
        &mut stdout,
        0,
        height.saturating_sub(1),
        "Press any key to return",
        width as usize,
    )?;
    stdout.flush().map_err(|error| error.to_string())?;
    loop {
        if let Event::Key(key) = event::read().map_err(|error| error.to_string())?
            && key.kind != KeyEventKind::Release
        {
            return Ok(());
        }
    }
}

fn history_overlay(history: &EntryHistory) -> Result<Option<SelectedFile>, String> {
    let revisions = history
        .revisions
        .iter()
        .filter(|revision| restorable_revision(revision))
        .rev()
        .collect::<Vec<_>>();
    if revisions.is_empty() {
        return Ok(None);
    }
    let mut selected = 0usize;
    loop {
        let (width, height) = terminal::size().map_err(|error| error.to_string())?;
        let mut stdout = io::stdout();
        queue!(
            stdout,
            terminal::Clear(ClearType::All),
            cursor::MoveTo(0, 0)
        )
        .map_err(|error| error.to_string())?;
        write_tui_line(
            &mut stdout,
            0,
            0,
            &format!("{} — History", history.path),
            width as usize,
        )?;
        for (index, revision) in revisions.iter().enumerate() {
            let prefix = if index == selected { ">" } else { " " };
            let label = if index == 0 {
                "latest available revision"
            } else {
                "older revision"
            };
            write_tui_line(
                &mut stdout,
                0,
                (index + 2) as u16,
                &format!(
                    "{} {} {} · {}",
                    prefix,
                    short_hash(
                        revision
                            .latest_restorable_backup
                            .as_deref()
                            .unwrap_or_default()
                    ),
                    label,
                    ByteSize(revision.size)
                ),
                width as usize,
            )?;
        }
        write_tui_line(
            &mut stdout,
            0,
            height.saturating_sub(1),
            "↑↓ choose · Enter/R restore revision · Esc close",
            width as usize,
        )?;
        stdout.flush().map_err(|error| error.to_string())?;
        let Event::Key(key) = event::read().map_err(|error| error.to_string())? else {
            continue;
        };
        if key.kind == KeyEventKind::Release {
            continue;
        }
        match key.code {
            KeyCode::Esc | KeyCode::Char('q') => return Ok(None),
            KeyCode::Up | KeyCode::Char('k') => selected = selected.saturating_sub(1),
            KeyCode::Down | KeyCode::Char('j') => {
                selected = (selected + 1).min(revisions.len() - 1)
            }
            KeyCode::Enter | KeyCode::Char('r') => {
                return Ok(revisions
                    .get(selected)
                    .and_then(|revision| revision_to_selected_file(history, revision)));
            }
            _ => {}
        }
    }
}

fn write_tui_line(
    stdout: &mut io::Stdout,
    x: u16,
    y: u16,
    text: &str,
    width: usize,
) -> Result<(), String> {
    let mut value = text.chars().take(width).collect::<String>();
    if value.chars().count() < width {
        value.push_str(&" ".repeat(width - value.chars().count()));
    }
    queue!(stdout, cursor::MoveTo(x, y), crossterm::style::Print(value))
        .map_err(|error| format!("Failed to write terminal content: {}", error))
}

fn tui_page_size() -> Result<usize, String> {
    let (_, height) = terminal::size().map_err(|error| error.to_string())?;
    Ok(height.saturating_sub(6).max(1) as usize)
}

fn short_hash(hash: &str) -> String {
    hash[..hash.len().min(8)].to_string()
}

fn shell_quote(value: &str) -> String {
    if value.chars().all(|character| {
        character.is_ascii_alphanumeric() || matches!(character, '_' | '-' | '.' | '/')
    }) {
        return value.to_string();
    }
    format!("'{}'", value.replace('\'', "'\\''"))
}

fn build_restore_command(backup: &str, path: &str) -> String {
    format!(
        "gib restore --backup {} --only {}",
        shell_quote(backup),
        shell_quote(path)
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn matches_from(arguments: &[&str]) -> clap::ArgMatches {
        let mut values = vec!["gib"];
        values.extend(arguments.iter().copied());
        let matches = crate::cli().try_get_matches_from(values).unwrap();
        matches.subcommand().unwrap().1.clone()
    }

    #[test]
    fn parses_scope_and_revision_requests() {
        let matches = matches_from(&[
            "explore",
            "--mode",
            "json",
            "--scope",
            "all-history",
            "--restore",
            "--select",
            "downloads/old.exe",
            "--revision",
            "downloads/old.exe=latest",
        ]);
        let request = parse_request(&matches).unwrap();
        assert_eq!(request.scope, ExplorerScope::AllHistory);
        assert_eq!(request.selected_paths, ["downloads/old.exe"]);
        assert_eq!(request.revisions["downloads/old.exe"], "latest");
    }

    #[test]
    fn accepts_a_bare_revision_reference_with_a_file_path() {
        let matches = matches_from(&[
            "explore",
            "--restore",
            "--path",
            "downloads/old.exe",
            "--revision",
            "latest",
        ]);
        let request = parse_request(&matches).unwrap();
        assert_eq!(request.revisions["downloads/old.exe"], "latest");
    }

    #[test]
    fn rejects_revision_without_restore() {
        let matches = matches_from(&["explore", "--revision", "old.txt=latest"]);
        assert!(parse_request(&matches).is_err());
    }

    #[test]
    fn builds_shell_safe_restore_commands() {
        assert_eq!(
            build_restore_command("abcdef12", "downloads/invoice.pdf"),
            "gib restore --backup abcdef12 --only downloads/invoice.pdf"
        );
        assert_eq!(
            build_restore_command("abcdef12", "downloads/my invoice.pdf"),
            "gib restore --backup abcdef12 --only 'downloads/my invoice.pdf'"
        );
    }

    #[test]
    fn filters_history_to_restorable_revisions() {
        let history = EntryHistory {
            entry_id: "entry".to_string(),
            path: "old.txt".to_string(),
            lookup_path: "old.txt".to_string(),
            parent_directory_id: "root".to_string(),
            name: "old.txt".to_string(),
            first_seen_backup: "first".to_string(),
            first_seen_timestamp: 1,
            last_seen_backup: "last".to_string(),
            last_seen_timestamp: 2,
            exists_in_latest_indexed_snapshot: false,
            latest_restorable_backup: Some("backup".to_string()),
            last_change_backup: None,
            revisions: vec![FileRevision {
                revision_id: "revision".to_string(),
                present_from_backup: "backup".to_string(),
                present_from_timestamp: 1,
                present_until_backup: None,
                present_until_timestamp: None,
                content_hash: "hash".to_string(),
                size: 10,
                content_type: "text/plain".to_string(),
                permissions: 0o644,
                latest_restorable_backup: Some("backup".to_string()),
            }],
        };
        let response = history_payload(
            &ExploreRequest {
                path: "old.txt".to_string(),
                scope: ExplorerScope::AllHistory,
                query: None,
                history: true,
                restore: false,
                selected_paths: Vec::new(),
                revisions: BTreeMap::new(),
                target_path: ".".to_string(),
                cursor: None,
                limit: 100,
                sort: ExplorerSort::Name,
            },
            &history,
            ExploreIndexStatus::Ready,
            None,
        );
        assert_eq!(response.revisions.len(), 1);
        assert!(response.revisions[0].restorable);
    }

    #[test]
    fn serializes_directory_json_without_terminal_control_sequences() {
        let response = ExploreDirectoryResponse {
            path: "downloads".to_string(),
            scope: "current".to_string(),
            entries: vec![ExploreDirectoryEntry {
                name: "invoice.pdf".to_string(),
                path: "downloads/invoice.pdf".to_string(),
                kind: "file".to_string(),
                status: "current".to_string(),
                restorable: true,
                last_backup: Some("abcdef12".to_string()),
                size: Some(10),
                content_type: Some("application/pdf".to_string()),
                permissions: Some(0o644),
                entry_id: Some("entry".to_string()),
                latest_revision_id: Some("revision".to_string()),
            }],
            next_cursor: Some("invoice.pdf".to_string()),
            index_status: ExploreIndexStatus::Ready,
            warning: None,
            message: None,
        };
        let serialized = serde_json::to_string(&response).unwrap();
        let value: serde_json::Value = serde_json::from_str(&serialized).unwrap();
        assert_eq!(value["path"], "downloads");
        assert_eq!(value["entries"][0]["status"], "current");
        assert_eq!(value["next_cursor"], "invoice.pdf");
        assert!(!serialized.contains('\u{1b}'));
    }

    #[test]
    fn renders_explorer_booleans_as_friendly_yes_or_no_values() {
        assert_eq!(yes_no(true), "yes");
        assert_eq!(yes_no(false), "no");
    }
}
