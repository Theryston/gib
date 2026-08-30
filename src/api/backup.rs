use super::client::Gib;
use super::error::{ErrorCode, GibError};
use super::event::{
    BackupEvent, GibEvent, OperationKind, OperationStarted, ProgressEvent, WarningEvent,
};
use super::repository::RepositoryRequest;
use crate::core::backup::{BackupInput, CoreProgress};
use serde::Serialize;
use std::fmt;
use std::path::PathBuf;
use std::sync::Arc;

/// Input for creating a repository snapshot.
pub struct BackupRequest {
    pub repository: RepositoryRequest,
    pub source_root: PathBuf,
    pub message: String,
    pub author: String,
    pub compression: i32,
    pub chunk_size: u64,
    pub ignore_patterns: Vec<String>,
    pub include_git: bool,
    pub concurrency: usize,
    pub parent: Option<String>,
    pub resume: Option<String>,
}

impl BackupRequest {
    pub fn new(
        repository: RepositoryRequest,
        source_root: impl Into<PathBuf>,
        message: impl Into<String>,
        author: impl Into<String>,
    ) -> Self {
        Self {
            repository,
            source_root: source_root.into(),
            message: message.into(),
            author: author.into(),
            compression: 3,
            chunk_size: 5 * 1024 * 1024,
            ignore_patterns: Vec::new(),
            include_git: false,
            concurrency: num_cpus::get().saturating_mul(2).max(1),
            parent: None,
            resume: None,
        }
    }
}

impl fmt::Debug for BackupRequest {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("BackupRequest")
            .field("repository", &self.repository)
            .field("source_root", &self.source_root)
            .field("message", &self.message)
            .field("author", &self.author)
            .field("compression", &self.compression)
            .field("chunk_size", &self.chunk_size)
            .field("ignore_patterns", &self.ignore_patterns)
            .field("include_git", &self.include_git)
            .field("concurrency", &self.concurrency)
            .field("parent", &self.parent)
            .field("resume", &self.resume)
            .finish()
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct BackupInfo {
    pub hash: String,
    pub message: String,
    pub author: String,
    pub timestamp_unix: u64,
    pub parents: Vec<String>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct BackupWarning {
    pub code: String,
    pub message: String,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct BackupResult {
    pub backup: BackupInfo,
    pub files_total: usize,
    pub written_bytes: u64,
    pub deduplicated_bytes: u64,
    pub elapsed_ms: u64,
    pub head_published: bool,
    pub warnings: Vec<BackupWarning>,
}

impl Gib {
    pub async fn backup(&self, request: BackupRequest) -> Result<BackupResult, GibError> {
        let source_root =
            super::client::path_from_context(&self.inner.context, &request.source_root);
        validate_request(&request, &source_root)?;
        let fs = self.backend(&request.repository.storage)?;
        self.events()
            .emit(GibEvent::OperationStarted(OperationStarted {
                operation: OperationKind::Backup,
            }));
        self.events().emit(GibEvent::Backup(BackupEvent {
            event: "started".to_string(),
            backup: None,
            files: None,
        }));

        let dispatcher = self.events().clone();
        let progress = Arc::new(move |progress: CoreProgress| {
            let percentage = progress.total.and_then(|total| {
                (total > 0)
                    .then(|| ((progress.processed.saturating_mul(100) / total).min(100)) as u8)
            });
            dispatcher.emit(GibEvent::Progress(ProgressEvent {
                operation: OperationKind::Backup,
                phase: progress.phase,
                processed: progress.processed,
                total: progress.total,
                percentage,
                message: progress.message,
            }));
        });
        let outcome = crate::core::backup::run(
            BackupInput {
                key: request.repository.key.clone(),
                root: source_root,
                fs,
                author: request.author,
                message: request.message,
                password: request.repository.password,
                compression: request.compression,
                chunk_size: request.chunk_size,
                ignore_patterns: request.ignore_patterns,
                include_git: request.include_git,
                concurrency: request.concurrency,
                parent: request.parent,
                resume: request.resume,
            },
            Some(progress),
        )
        .await
        .map_err(|error| map_backup_error(error))?;

        for warning in &outcome.warnings {
            self.events().emit(GibEvent::Warning(WarningEvent {
                code: warning.code.clone(),
                message: warning.message.clone(),
            }));
        }
        self.events().emit(GibEvent::Backup(BackupEvent {
            event: "completed".to_string(),
            backup: Some(outcome.backup.hash.clone()),
            files: Some(outcome.files_total as u64),
        }));
        Ok(BackupResult {
            backup: BackupInfo {
                hash: outcome.backup.hash,
                message: outcome.backup.message,
                author: outcome.backup.author,
                timestamp_unix: outcome.backup.timestamp,
                parents: outcome.backup.parents,
            },
            files_total: outcome.files_total,
            written_bytes: outcome.written_bytes,
            deduplicated_bytes: outcome.deduplicated_bytes,
            elapsed_ms: outcome.elapsed_ms,
            head_published: outcome.head_published,
            warnings: outcome
                .warnings
                .into_iter()
                .map(|warning| BackupWarning {
                    code: warning.code,
                    message: warning.message,
                })
                .collect(),
        })
    }
}

fn validate_request(
    request: &BackupRequest,
    source_root: &std::path::Path,
) -> Result<(), GibError> {
    request.repository.validate()?;
    if request.message.is_empty() {
        return Err(GibError::new(
            ErrorCode::InvalidRequest,
            "Backup message cannot be empty",
        ));
    }
    if request.author.is_empty() {
        return Err(GibError::new(
            ErrorCode::InvalidRequest,
            "Backup author cannot be empty",
        ));
    }
    if !(1..=22).contains(&request.compression) {
        return Err(GibError::new(
            ErrorCode::InvalidRequest,
            "Backup compression level must be between 1 and 22",
        ));
    }
    if request.chunk_size == 0 || usize::try_from(request.chunk_size).is_err() {
        return Err(GibError::new(
            ErrorCode::InvalidRequest,
            "Backup chunk size must be greater than zero and fit in memory",
        ));
    }
    if request.concurrency == 0 {
        return Err(GibError::new(
            ErrorCode::InvalidRequest,
            "Backup concurrency must be greater than zero",
        ));
    }
    if !source_root.is_dir() {
        return Err(GibError::new(
            ErrorCode::InvalidRequest,
            format!(
                "Backup root '{}' is not an existing directory",
                source_root.display()
            ),
        ));
    }
    Ok(())
}

fn map_backup_error(error: String) -> GibError {
    let lower = error.to_ascii_lowercase();
    let code = if lower.contains("invalid password") {
        ErrorCode::InvalidPassword
    } else if lower.contains("password") {
        ErrorCode::PasswordRequired
    } else if lower.contains("not found") {
        ErrorCode::BackupNotFound
    } else if lower.contains("serialize") || lower.contains("deserialize") {
        ErrorCode::Serialization
    } else if lower.contains("encrypt") || lower.contains("decrypt") {
        ErrorCode::Encryption
    } else {
        ErrorCode::Internal
    };
    GibError::new(code, error)
}
