use super::client::Gib;
use super::error::{ErrorCode, GibError};
use super::event::{GibEvent, OperationKind, OperationStarted};
use super::repository::RepositoryRequest;
use super::restore::{RestoreFailure, RestoreRequest};
use crate::core::catalog::{
    CatalogEntryScope, CatalogState, collect_entries_by_tokens_with_snapshot,
    get_entry_history_with_snapshot, load_latest_parentless_snapshot, lookup_path,
    normalize_relative_path, path_tokens, read_catalog_status,
};
use crate::core::crypto::read_file_maybe_decrypt;
use crate::core::metadata::{BackupSummary as CoreBackupSummary, PendingBackup};
use serde::Serialize;
use std::sync::Arc;

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ListBackupsRequest {
    pub repository: RepositoryRequest,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct BackupSummary {
    pub hash: String,
    pub message: String,
    pub timestamp_unix: Option<u64>,
    pub size_bytes: Option<u64>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct ListBackupsResponse {
    pub backups: Vec<BackupSummary>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ListPendingBackupsRequest {
    pub repository: RepositoryRequest,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct PendingBackupInfo {
    pub backup: String,
    pub message: String,
    pub uploaded_chunks: usize,
    pub chunk_size_bytes: u64,
    pub compression: i32,
    pub concurrency: usize,
    pub ignored_entries: usize,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct ListPendingBackupsResponse {
    pub pending: Vec<PendingBackupInfo>,
}

impl Gib {
    pub async fn list_backups(
        &self,
        request: ListBackupsRequest,
    ) -> Result<ListBackupsResponse, GibError> {
        request.repository.validate()?;
        self.events()
            .emit(GibEvent::OperationStarted(OperationStarted {
                operation: OperationKind::Catalog,
            }));
        let fs = self.backend(&request.repository.storage)?;
        let summaries = crate::core::indexes::list_backup_summaries(
            fs,
            request.repository.key,
            request.repository.password,
        )
        .await
        .map_err(map_catalog_error)?;
        Ok(ListBackupsResponse {
            backups: summaries
                .into_iter()
                .map(BackupSummary::from_core)
                .collect(),
        })
    }

    pub async fn list_pending_backups(
        &self,
        request: ListPendingBackupsRequest,
    ) -> Result<ListPendingBackupsResponse, GibError> {
        request.repository.validate()?;
        self.events()
            .emit(GibEvent::OperationStarted(OperationStarted {
                operation: OperationKind::Catalog,
            }));
        let fs = self.backend(&request.repository.storage)?;
        let key = request.repository.key;
        let password = request.repository.password;
        let paths = fs
            .list_files(&format!("{key}/indexes"))
            .await
            .map_err(|error| GibError::new(ErrorCode::Io, error.to_string()))?;
        let mut pending = Vec::new();
        for path in paths {
            let Some(hash) = path
                .rsplit('/')
                .next()
                .and_then(|name| name.strip_prefix("pending_"))
            else {
                continue;
            };
            let result = read_file_maybe_decrypt(
                &fs,
                &path,
                password.as_deref(),
                "Pending backup is encrypted but no password provided",
            )
            .await
            .map_err(map_catalog_error)?;
            let bytes = crate::utils::decompress_bytes(&result.bytes);
            let value: PendingBackup = rmp_serde::from_slice(&bytes).map_err(|error| {
                GibError::new(
                    ErrorCode::Serialization,
                    format!("Failed to deserialize pending backup '{path}': {error}"),
                )
            })?;
            pending.push(PendingBackupInfo {
                backup: hash.to_string(),
                message: value.message,
                uploaded_chunks: value.processed_chunks.len(),
                chunk_size_bytes: value.chunk_size,
                compression: value.compress,
                concurrency: value.concurrency,
                ignored_entries: value.ignore_patterns.len(),
            });
        }
        pending.sort_by(|left, right| left.backup.cmp(&right.backup));
        Ok(ListPendingBackupsResponse { pending })
    }
}

impl BackupSummary {
    fn from_core(summary: CoreBackupSummary) -> Self {
        Self {
            hash: summary.hash,
            message: summary.message,
            timestamp_unix: summary.timestamp,
            size_bytes: summary.size,
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SearchRequest {
    pub repository: RepositoryRequest,
    pub query: String,
    pub path_prefix: Option<String>,
    pub extension: Option<String>,
    pub limit: usize,
}

impl SearchRequest {
    pub fn new(repository: RepositoryRequest, query: impl Into<String>) -> Result<Self, GibError> {
        let query = query.into();
        validate_search_values(&query, None, None, 100)?;
        Ok(Self {
            repository,
            query,
            path_prefix: None,
            extension: None,
            limit: 100,
        })
    }

    pub fn with_path_prefix(mut self, path: impl Into<String>) -> Result<Self, GibError> {
        let path = path.into();
        validate_search_values(
            &self.query,
            Some(&path),
            self.extension.as_deref(),
            self.limit,
        )?;
        self.path_prefix = Some(path);
        Ok(self)
    }

    pub fn with_extension(mut self, extension: impl Into<String>) -> Result<Self, GibError> {
        let extension = extension.into();
        validate_search_values(
            &self.query,
            self.path_prefix.as_deref(),
            Some(&extension),
            self.limit,
        )?;
        self.extension = Some(extension);
        Ok(self)
    }

    pub fn with_limit(mut self, limit: usize) -> Result<Self, GibError> {
        validate_search_values(
            &self.query,
            self.path_prefix.as_deref(),
            self.extension.as_deref(),
            limit,
        )?;
        self.limit = limit;
        Ok(self)
    }
}

#[non_exhaustive]
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum SearchIndexStatus {
    Ready,
    Degraded,
    NoIndexedBackups,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct SearchResult {
    pub path: String,
    pub last_backup: String,
    pub restore_command: String,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct SearchResponse {
    pub query: String,
    pub index_status: SearchIndexStatus,
    pub results: Vec<SearchResult>,
    pub truncated: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub warning: Option<String>,
}

impl Gib {
    pub async fn search(&self, request: SearchRequest) -> Result<SearchResponse, GibError> {
        validate_search_values(
            &request.query,
            request.path_prefix.as_deref(),
            request.extension.as_deref(),
            request.limit,
        )?;
        request.repository.validate()?;
        self.events()
            .emit(GibEvent::OperationStarted(OperationStarted {
                operation: OperationKind::Search,
            }));
        let fs = self.backend(&request.repository.storage)?;
        let status = read_catalog_status(
            Arc::clone(&fs),
            request.repository.key.clone(),
            request.repository.password.clone(),
        )
        .await
        .map_err(map_catalog_error)?;
        let Some(status) = status else {
            return Ok(empty_search(
                request.query,
                SearchIndexStatus::NoIndexedBackups,
                None,
            ));
        };
        if status.indexed_backup_count == 0 || status.latest_indexed_backup.is_none() {
            return Ok(empty_search(
                request.query,
                SearchIndexStatus::NoIndexedBackups,
                None,
            ));
        }
        let index_status = if status.state == CatalogState::Degraded {
            SearchIndexStatus::Degraded
        } else {
            SearchIndexStatus::Ready
        };
        let current_snapshot = load_latest_parentless_snapshot(
            Arc::clone(&fs),
            request.repository.key.clone(),
            request.repository.password.clone(),
        )
        .await
        .map_err(map_catalog_error)?;
        let query_tokens = path_tokens(&request.query);
        let candidates = collect_entries_by_tokens_with_snapshot(
            fs,
            request.repository.key,
            request.repository.password,
            &query_tokens,
            CatalogEntryScope::AllHistory,
            current_snapshot.as_ref(),
        )
        .await
        .map_err(map_catalog_error)?;
        let mut ranked = candidates
            .into_iter()
            .filter_map(|summary| {
                let backup = summary.latest_restorable_backup?;
                if !matches_prefix(&summary.path, request.path_prefix.as_deref())
                    || !matches_extension(&summary.path, request.extension.as_deref())
                {
                    return None;
                }
                let short = short_hash(&backup);
                Some((
                    relevance(&summary.path, &request.query, &query_tokens),
                    summary.newest_revision_timestamp,
                    SearchResult {
                        path: summary.path.clone(),
                        last_backup: short.clone(),
                        restore_command: format!(
                            "gib restore --backup {} --only {}",
                            shell_quote(&short),
                            shell_quote(&summary.path)
                        ),
                    },
                ))
            })
            .collect::<Vec<_>>();
        ranked.sort_by(|left, right| {
            right
                .0
                .cmp(&left.0)
                .then_with(|| right.1.cmp(&left.1))
                .then_with(|| left.2.path.cmp(&right.2.path))
        });
        let truncated = ranked.len() > request.limit;
        ranked.truncate(request.limit);
        Ok(SearchResponse {
            query: request.query,
            index_status,
            results: ranked.into_iter().map(|item| item.2).collect(),
            truncated,
            warning: (index_status == SearchIndexStatus::Degraded).then(|| {
                "The historical search catalog is degraded; search results may be incomplete until pending backups are indexed.".to_string()
            }),
        })
    }
}

fn validate_search_values(
    query: &str,
    prefix: Option<&str>,
    extension: Option<&str>,
    limit: usize,
) -> Result<(), GibError> {
    if query.trim().is_empty() || path_tokens(query).is_empty() {
        return Err(GibError::new(
            ErrorCode::InvalidRequest,
            "Search query must contain at least one searchable token",
        ));
    }
    if limit == 0 {
        return Err(GibError::new(
            ErrorCode::InvalidRequest,
            "Search limit must be greater than zero",
        ));
    }
    if let Some(prefix) = prefix {
        normalize_relative_path(prefix)
            .map_err(|error| GibError::new(ErrorCode::InvalidRequest, error))?;
    }
    if let Some(extension) = extension
        && (extension.trim().is_empty()
            || extension.starts_with('.')
            || extension.contains('/')
            || extension.contains('\\')
            || extension.chars().any(char::is_whitespace))
    {
        return Err(GibError::new(
            ErrorCode::InvalidRequest,
            "Search extension is invalid",
        ));
    }
    Ok(())
}

fn empty_search(
    query: String,
    index_status: SearchIndexStatus,
    warning: Option<String>,
) -> SearchResponse {
    SearchResponse {
        query,
        index_status,
        results: Vec::new(),
        truncated: false,
        warning,
    }
}

fn matches_prefix(path: &str, prefix: Option<&str>) -> bool {
    let Some(prefix) = prefix else {
        return true;
    };
    let path = lookup_path(path);
    let prefix = lookup_path(prefix);
    path == prefix || path.starts_with(&format!("{prefix}/"))
}

fn matches_extension(path: &str, extension: Option<&str>) -> bool {
    let Some(extension) = extension else {
        return true;
    };
    let name = path.rsplit('/').next().unwrap_or(path).to_ascii_lowercase();
    let suffix = format!(".{}", extension.to_ascii_lowercase());
    name.len() > suffix.len() && name.ends_with(&suffix)
}

fn relevance(path: &str, query: &str, query_tokens: &[String]) -> i64 {
    let path = lookup_path(path);
    let query = lookup_path(query);
    let name = path.rsplit('/').next().unwrap_or(&path);
    let stem = name.rsplit_once('.').map_or(name, |(stem, _)| stem);
    let mut score = 0;
    if path == query {
        score += 5_000_000;
    }
    if name == query {
        score += 4_000_000;
    }
    if stem == query {
        score += 3_000_000;
    }
    if name.starts_with(&query) {
        score += 2_000_000 + compactness(&query, name);
    } else if name.contains(&query) {
        score += 1_400_000 + compactness(&query, name);
    }
    if stem.starts_with(&query) {
        score += 700_000 + compactness(&query, stem);
    } else if stem.contains(&query) {
        score += 450_000 + compactness(&query, stem);
    }
    let name_tokens = path_tokens(name);
    let path_tokens = path_tokens(&path);
    for token in query_tokens {
        let name_score = token_score(token, &name_tokens);
        let path_score = token_score(token, &path_tokens);
        score += name_score
            + if name_score == 0 {
                path_score / 2
            } else {
                path_score / 10
            };
    }
    score
}

fn compactness(query: &str, candidate: &str) -> i64 {
    (query.chars().count() as i64 * 100 / candidate.chars().count().max(1) as i64).min(100)
}

fn token_score(query: &str, candidates: &[String]) -> i64 {
    candidates
        .iter()
        .map(|candidate| {
            if candidate == query {
                800_000 + compactness(query, candidate)
            } else if candidate.starts_with(query) {
                550_000 + compactness(query, candidate)
            } else if candidate.contains(query) {
                350_000 + compactness(query, candidate)
            } else {
                0
            }
        })
        .max()
        .unwrap_or_default()
}

fn short_hash(hash: &str) -> String {
    hash[..hash.len().min(8)].to_string()
}

fn shell_quote(value: &str) -> String {
    if value.chars().all(|character| {
        character.is_ascii_alphanumeric() || matches!(character, '_' | '-' | '.' | '/')
    }) {
        value.to_string()
    } else {
        format!("'{}'", value.replace('\'', "'\\''"))
    }
}

#[non_exhaustive]
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ExploreScope {
    Current,
    AllHistory,
}

impl ExploreScope {
    fn core(self) -> crate::core::explore::ExplorerScope {
        match self {
            Self::Current => crate::core::explore::ExplorerScope::Current,
            Self::AllHistory => crate::core::explore::ExplorerScope::AllHistory,
        }
    }
}

#[non_exhaustive]
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ExploreSort {
    Name,
    Size,
    Status,
    Recent,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ExploreDirectoryRequest {
    pub repository: RepositoryRequest,
    pub path: String,
    pub scope: ExploreScope,
    pub cursor: Option<String>,
    pub limit: usize,
    pub sort: ExploreSort,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct ExploreEntry {
    pub path: String,
    pub name: String,
    pub kind: String,
    pub status: String,
    pub restorable: bool,
    pub last_backup: Option<String>,
    pub size: Option<u64>,
    pub content_type: Option<String>,
    pub permissions: Option<u32>,
    pub newest_revision_timestamp: Option<u64>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct ExploreDirectoryResponse {
    pub path: String,
    pub scope: ExploreScope,
    pub entries: Vec<ExploreEntry>,
    pub next_cursor: Option<String>,
    pub index_status: SearchIndexStatus,
    pub warning: Option<String>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ExploreSearchRequest {
    pub repository: RepositoryRequest,
    pub query: String,
    pub scope: ExploreScope,
    pub limit: usize,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct ExploreSearchResponse {
    pub query: String,
    pub scope: ExploreScope,
    pub results: Vec<ExploreEntry>,
    pub index_status: SearchIndexStatus,
    pub warning: Option<String>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ExploreHistoryRequest {
    pub repository: RepositoryRequest,
    pub path: String,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct FileRevisionInfo {
    pub revision_id: String,
    pub backup: Option<String>,
    pub timestamp_unix: u64,
    pub size: u64,
    pub content_hash: String,
    pub content_type: String,
    pub permissions: u32,
    pub restorable: bool,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct ExploreHistoryResponse {
    pub path: String,
    pub entry_id: String,
    pub exists_in_latest_snapshot: bool,
    pub revisions: Vec<FileRevisionInfo>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ExploreFileRequest {
    pub repository: RepositoryRequest,
    pub path: String,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct ExploreFileResponse {
    pub entry: Option<ExploreEntry>,
    pub history: Option<ExploreHistoryResponse>,
}

/// One path selected for a historical restore. When `backup` is omitted, the
/// newest restorable revision known by the catalog is used.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ExploreSelection {
    pub path: String,
    pub backup: Option<String>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ExploreRestoreRequest {
    pub repository: RepositoryRequest,
    pub target_path: std::path::PathBuf,
    pub selections: Vec<ExploreSelection>,
    pub prune_local: bool,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct ExploreRestoreResult {
    pub restored: u64,
    pub skipped: u64,
    pub backups: Vec<String>,
    pub unavailable: Vec<String>,
    pub failed: Vec<RestoreFailure>,
    pub pruned_local: Vec<String>,
}

impl Gib {
    /// Restores selections that may come from different historical backups.
    /// Each backup group is delegated to the same safe [`Gib::restore`] API
    /// used by ordinary restore requests.
    pub async fn restore_explore_selection(
        &self,
        request: ExploreRestoreRequest,
    ) -> Result<ExploreRestoreResult, GibError> {
        request.repository.validate()?;
        if request.selections.is_empty() {
            return Err(GibError::new(
                ErrorCode::InvalidRequest,
                "At least one Explore selection is required",
            ));
        }
        let mut result = ExploreRestoreResult {
            restored: 0,
            skipped: 0,
            backups: Vec::new(),
            unavailable: Vec::new(),
            failed: Vec::new(),
            pruned_local: Vec::new(),
        };
        for (index, selection) in request.selections.iter().enumerate() {
            let backup = match selection.backup.clone() {
                Some(backup) => backup,
                None => {
                    let history = self
                        .explore_history(ExploreHistoryRequest {
                            repository: request.repository.clone(),
                            path: selection.path.clone(),
                        })
                        .await?;
                    let Some(history) = history else {
                        result.unavailable.push(selection.path.clone());
                        continue;
                    };
                    let Some(backup) = history
                        .revisions
                        .iter()
                        .rev()
                        .find_map(|revision| revision.backup.clone())
                    else {
                        result.unavailable.push(selection.path.clone());
                        continue;
                    };
                    backup
                }
            };
            match self
                .restore(RestoreRequest {
                    repository: request.repository.clone(),
                    backup: backup.clone(),
                    target_path: request.target_path.clone(),
                    only: vec![selection.path.clone()],
                    prune_local: request.prune_local && index + 1 == request.selections.len(),
                })
                .await
            {
                Ok(restored) => {
                    result.restored += restored.restored;
                    result.skipped += restored.skipped;
                    result.unavailable.extend(restored.unavailable);
                    result.failed.extend(restored.failed);
                    result.pruned_local.extend(restored.pruned_local);
                    if !result.backups.contains(&backup) {
                        result.backups.push(backup);
                    }
                }
                Err(error) => result.failed.push(RestoreFailure {
                    path: selection.path.clone(),
                    message: error.message().to_string(),
                }),
            }
        }
        Ok(result)
    }
}

impl Gib {
    pub async fn explore_directory(
        &self,
        request: ExploreDirectoryRequest,
    ) -> Result<ExploreDirectoryResponse, GibError> {
        request.repository.validate()?;
        if request.limit == 0 {
            return Err(GibError::new(
                ErrorCode::InvalidRequest,
                "Explore limit must be greater than zero",
            ));
        }
        self.events()
            .emit(GibEvent::OperationStarted(OperationStarted {
                operation: OperationKind::Explore,
            }));
        let (mut navigator, status, warning) = self.explorer(&request.repository).await?;
        let page = navigator
            .load_directory_page(
                &request.path,
                request.scope.core(),
                request.cursor.as_deref(),
            )
            .await
            .map_err(map_catalog_error)?;
        let mut entries = page
            .entries
            .into_iter()
            .map(entry_from_core)
            .collect::<Vec<_>>();
        sort_entries(&mut entries, request.sort);
        entries.truncate(request.limit);
        Ok(ExploreDirectoryResponse {
            path: page.path,
            scope: request.scope,
            entries,
            next_cursor: page.next_cursor,
            index_status: status,
            warning,
        })
    }

    pub async fn explore_search(
        &self,
        request: ExploreSearchRequest,
    ) -> Result<ExploreSearchResponse, GibError> {
        request.repository.validate()?;
        if request.limit == 0 || path_tokens(&request.query).is_empty() {
            return Err(GibError::new(
                ErrorCode::InvalidRequest,
                "Explore query and limit are invalid",
            ));
        }
        self.events()
            .emit(GibEvent::OperationStarted(OperationStarted {
                operation: OperationKind::Explore,
            }));
        let (mut navigator, status, warning) = self.explorer(&request.repository).await?;
        let mut results = navigator
            .search(&request.query, request.scope.core())
            .await
            .map_err(map_catalog_error)?;
        results.truncate(request.limit);
        Ok(ExploreSearchResponse {
            query: request.query,
            scope: request.scope,
            results: results.into_iter().map(entry_from_core).collect(),
            index_status: status,
            warning,
        })
    }

    pub async fn explore_history(
        &self,
        request: ExploreHistoryRequest,
    ) -> Result<Option<ExploreHistoryResponse>, GibError> {
        request.repository.validate()?;
        self.events()
            .emit(GibEvent::OperationStarted(OperationStarted {
                operation: OperationKind::Explore,
            }));
        let fs = self.backend(&request.repository.storage)?;
        let snapshot = load_latest_parentless_snapshot(
            Arc::clone(&fs),
            request.repository.key.clone(),
            request.repository.password.clone(),
        )
        .await
        .map_err(map_catalog_error)?;
        let history = get_entry_history_with_snapshot(
            Arc::clone(&fs),
            request.repository.key,
            request.repository.password,
            &request.path,
            snapshot.as_ref(),
        )
        .await
        .map_err(map_catalog_error)?;
        Ok(history.map(history_from_core))
    }

    pub async fn explore_file(
        &self,
        request: ExploreFileRequest,
    ) -> Result<ExploreFileResponse, GibError> {
        let history = self
            .explore_history(ExploreHistoryRequest {
                repository: request.repository,
                path: request.path,
            })
            .await?;
        let entry = history.as_ref().map(|history| ExploreEntry {
            path: history.path.clone(),
            name: history
                .path
                .rsplit('/')
                .next()
                .unwrap_or(&history.path)
                .to_string(),
            kind: "file".to_string(),
            status: if history.exists_in_latest_snapshot {
                "current"
            } else {
                "deleted"
            }
            .to_string(),
            restorable: history.revisions.iter().any(|revision| revision.restorable),
            last_backup: history
                .revisions
                .iter()
                .rev()
                .find_map(|revision| revision.backup.clone()),
            size: history.revisions.last().map(|revision| revision.size),
            content_type: history
                .revisions
                .last()
                .map(|revision| revision.content_type.clone()),
            permissions: history
                .revisions
                .last()
                .map(|revision| revision.permissions),
            newest_revision_timestamp: history
                .revisions
                .last()
                .map(|revision| revision.timestamp_unix),
        });
        Ok(ExploreFileResponse { entry, history })
    }

    async fn explorer(
        &self,
        repository: &RepositoryRequest,
    ) -> Result<
        (
            crate::core::explore::ExplorerNavigator,
            SearchIndexStatus,
            Option<String>,
        ),
        GibError,
    > {
        let fs = self.backend(&repository.storage)?;
        let status_value = read_catalog_status(
            Arc::clone(&fs),
            repository.key.clone(),
            repository.password.clone(),
        )
        .await
        .map_err(map_catalog_error)?;
        let status = status_value
            .as_ref()
            .map_or(SearchIndexStatus::NoIndexedBackups, |status| {
                if status.indexed_backup_count == 0 || status.latest_indexed_backup.is_none() {
                    SearchIndexStatus::NoIndexedBackups
                } else if status.state == CatalogState::Degraded {
                    SearchIndexStatus::Degraded
                } else {
                    SearchIndexStatus::Ready
                }
            });
        let warning = (status == SearchIndexStatus::Degraded).then(|| {
            "The historical catalog is degraded; some entries may be incomplete until pending backups are indexed."
                .to_string()
        });
        let snapshot = load_latest_parentless_snapshot(
            Arc::clone(&fs),
            repository.key.clone(),
            repository.password.clone(),
        )
        .await
        .map_err(map_catalog_error)?;
        let mut navigator = crate::core::explore::ExplorerNavigator::new(
            fs,
            repository.key.clone(),
            repository.password.clone(),
        );
        navigator.set_current_snapshot(snapshot);
        Ok((navigator, status, warning))
    }
}

fn entry_from_core(entry: crate::core::explore::ExplorerEntry) -> ExploreEntry {
    ExploreEntry {
        path: entry.path,
        name: entry.name,
        kind: entry.kind.label().to_string(),
        status: entry.status.label().to_string(),
        restorable: entry.restorable,
        last_backup: entry.last_backup,
        size: entry.size,
        content_type: entry.content_type,
        permissions: entry.permissions,
        newest_revision_timestamp: entry.newest_revision_timestamp,
    }
}

fn sort_entries(entries: &mut [ExploreEntry], sort: ExploreSort) {
    entries.sort_by(|left, right| {
        let kind_order = |kind: &str| u8::from(kind != "directory");
        let status_order = |status: &str| u8::from(status != "current");
        kind_order(&left.kind)
            .cmp(&kind_order(&right.kind))
            .then_with(|| match sort {
                ExploreSort::Name => std::cmp::Ordering::Equal,
                ExploreSort::Size => right
                    .size
                    .unwrap_or_default()
                    .cmp(&left.size.unwrap_or_default()),
                ExploreSort::Status => status_order(&left.status).cmp(&status_order(&right.status)),
                ExploreSort::Recent => right
                    .newest_revision_timestamp
                    .unwrap_or_default()
                    .cmp(&left.newest_revision_timestamp.unwrap_or_default()),
            })
            .then_with(|| {
                left.name
                    .to_ascii_lowercase()
                    .cmp(&right.name.to_ascii_lowercase())
            })
            .then_with(|| left.name.cmp(&right.name))
            .then_with(|| left.path.cmp(&right.path))
    });
}

fn history_from_core(history: crate::core::catalog::EntryHistory) -> ExploreHistoryResponse {
    ExploreHistoryResponse {
        path: history.path,
        entry_id: history.entry_id,
        exists_in_latest_snapshot: history.exists_in_latest_indexed_snapshot,
        revisions: history
            .revisions
            .into_iter()
            .map(|revision| {
                let restorable = revision.latest_restorable_backup.is_some();
                FileRevisionInfo {
                    revision_id: revision.revision_id,
                    backup: revision.latest_restorable_backup,
                    timestamp_unix: revision.present_from_timestamp,
                    size: revision.size,
                    content_hash: revision.content_hash,
                    content_type: revision.content_type,
                    permissions: revision.permissions,
                    restorable,
                }
            })
            .collect(),
    }
}

fn map_catalog_error(error: String) -> GibError {
    let lower = error.to_ascii_lowercase();
    let code = if lower.contains("invalid password") {
        ErrorCode::InvalidPassword
    } else if lower.contains("password") {
        ErrorCode::PasswordRequired
    } else if lower.contains("not found") {
        ErrorCode::BackupNotFound
    } else if lower.contains("decrypt") || lower.contains("encrypt") {
        ErrorCode::Encryption
    } else if lower.contains("deserialize") || lower.contains("serialize") {
        ErrorCode::Serialization
    } else {
        ErrorCode::Internal
    };
    GibError::new(code, error)
}
