use super::client::Gib;
use super::error::{ErrorCode, GibError};
use super::event::{
    GibEvent, OperationKind, OperationStarted, ProgressEvent, RestoreEvent, WarningEvent,
};
use super::repository::RepositoryRequest;
use crate::core::catalog::load_backup_manifest;
use crate::core::metadata::BackupObject;
use crate::core::restore::{RestoreProgress, restore_files_reported};
use serde::Serialize;
use std::collections::BTreeSet;
use std::fmt;
use std::path::{Path, PathBuf};
use std::sync::Arc;

/// Input for restoring a complete backup or selected paths.
pub struct RestoreRequest {
    pub repository: RepositoryRequest,
    pub backup: String,
    pub target_path: PathBuf,
    pub only: Vec<String>,
    pub prune_local: bool,
}

impl RestoreRequest {
    pub fn new(
        repository: RepositoryRequest,
        backup: impl Into<String>,
        target_path: impl Into<PathBuf>,
    ) -> Self {
        Self {
            repository,
            backup: backup.into(),
            target_path: target_path.into(),
            only: Vec::new(),
            prune_local: false,
        }
    }
}

impl fmt::Debug for RestoreRequest {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("RestoreRequest")
            .field("repository", &self.repository)
            .field("backup", &self.backup)
            .field("target_path", &self.target_path)
            .field("only", &self.only)
            .field("prune_local", &self.prune_local)
            .finish()
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct RestoreFailure {
    pub path: String,
    pub message: String,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct RestoreResult {
    pub backup: String,
    pub target_path: PathBuf,
    pub restored: u64,
    pub skipped: u64,
    pub unavailable: Vec<String>,
    pub failed: Vec<RestoreFailure>,
    pub pruned_local: Vec<String>,
}

impl Gib {
    pub async fn restore(&self, request: RestoreRequest) -> Result<RestoreResult, GibError> {
        request.repository.validate()?;
        if request.backup.trim().is_empty() {
            return Err(GibError::new(
                ErrorCode::InvalidRequest,
                "A backup reference is required",
            ));
        }
        self.events()
            .emit(GibEvent::OperationStarted(OperationStarted {
                operation: OperationKind::Restore,
            }));
        self.events().emit(GibEvent::Restore(RestoreEvent {
            event: "started".to_string(),
            path: None,
        }));
        let fs = self.backend(&request.repository.storage)?;
        self.events().emit(GibEvent::Progress(ProgressEvent {
            operation: OperationKind::Restore,
            phase: "metadata".to_string(),
            processed: 0,
            total: None,
            percentage: None,
            message: Some("Loading backup metadata...".to_string()),
        }));
        let backup_hash = crate::core::indexes::resolve_backup_reference(
            Arc::clone(&fs),
            request.repository.key.clone(),
            request.repository.password.clone(),
            &request.backup,
        )
        .await
        .map_err(map_restore_error)?;
        let backup = load_backup_manifest(
            &fs,
            &request.repository.key,
            request.repository.password.as_deref(),
            &backup_hash,
        )
        .await
        .map_err(map_restore_error)?;
        let target_path =
            super::client::path_from_context(&self.inner.context, &request.target_path);
        std::fs::create_dir_all(&target_path).map_err(|error| {
            GibError::new(
                ErrorCode::Io,
                format!(
                    "Failed to create restore target '{}': {error}",
                    target_path.display()
                ),
            )
        })?;

        let (files, unavailable) = select_files(&backup.tree, &request.only)?;
        let expected = files
            .iter()
            .map(|(path, _)| path.clone())
            .collect::<BTreeSet<_>>();
        if !unavailable.is_empty() {
            self.events().emit(GibEvent::Warning(WarningEvent {
                code: "restore_unavailable".to_string(),
                message: format!("Skipped {} unavailable restore path(s)", unavailable.len()),
            }));
        }
        let dispatcher = self.events().clone();
        let progress = Arc::new(move |progress: RestoreProgress| {
            dispatcher.emit(GibEvent::Progress(ProgressEvent {
                operation: OperationKind::Restore,
                phase: "files".to_string(),
                processed: progress.processed,
                total: Some(progress.total),
                percentage: (progress.total > 0)
                    .then(|| ((progress.processed * 100 / progress.total).min(100)) as u8),
                message: Some(format!("Restoring {}", progress.path)),
            }));
        });
        let stats = restore_files_reported(
            fs,
            request.repository.key.clone(),
            request.repository.password.clone(),
            target_path.to_string_lossy().to_string(),
            files,
            Some(progress),
        )
        .await;
        let failed = stats
            .failed
            .into_iter()
            .map(|failure| RestoreFailure {
                path: failure.path,
                message: failure.message,
            })
            .collect::<Vec<_>>();
        let pruned_local = if request.prune_local {
            prune_local_files(&target_path, &expected)?
        } else {
            Vec::new()
        };
        self.events().emit(GibEvent::Restore(RestoreEvent {
            event: "completed".to_string(),
            path: Some(target_path.to_string_lossy().to_string()),
        }));
        Ok(RestoreResult {
            backup: backup_hash,
            target_path,
            restored: stats.restored,
            skipped: stats.skipped,
            unavailable,
            failed,
            pruned_local,
        })
    }
}

fn select_files(
    tree: &std::collections::HashMap<String, BackupObject>,
    only: &[String],
) -> Result<(Vec<(String, BackupObject)>, Vec<String>), GibError> {
    if only.is_empty() {
        let mut selected = Vec::with_capacity(tree.len());
        for (path, object) in tree {
            let path = normalize_restore_path(path)?;
            selected.push((path, object.clone()));
        }
        return Ok((selected, Vec::new()));
    }
    let mut selected = Vec::new();
    let mut unavailable = Vec::new();
    for requested in only {
        let normalized = normalize_restore_path(requested)?;
        let mut found = false;
        for (path, object) in tree {
            let path = normalize_restore_path(path)?;
            if path == normalized || path.starts_with(&format!("{normalized}/")) {
                selected.push((path, object.clone()));
                found = true;
            }
        }
        if !found {
            unavailable.push(normalized);
        }
    }
    selected.sort_by(|left, right| left.0.cmp(&right.0));
    selected.dedup_by(|left, right| left.0 == right.0);
    Ok((selected, unavailable))
}

fn normalize_restore_path(value: &str) -> Result<String, GibError> {
    let value = value.trim().replace('\\', "/");
    if value.is_empty()
        || Path::new(&value).is_absolute()
        || value
            .split('/')
            .any(|part| part.is_empty() || part == "." || part == "..")
    {
        return Err(GibError::new(
            ErrorCode::InvalidRequest,
            format!("Invalid restore path '{value}'"),
        ));
    }
    Ok(value)
}

fn prune_local_files(target: &Path, expected: &BTreeSet<String>) -> Result<Vec<String>, GibError> {
    let mut removed = Vec::new();
    let walker = walkdir::WalkDir::new(target)
        .contents_first(true)
        .into_iter()
        .filter_entry(|entry| {
            entry
                .path()
                .strip_prefix(target)
                .ok()
                .is_none_or(|relative| {
                    !relative
                        .components()
                        .any(|component| component.as_os_str() == ".git")
                })
        });
    for entry in walker {
        let entry = entry.map_err(|error| {
            GibError::new(
                ErrorCode::Io,
                format!("Failed to inspect local restore target: {error}"),
            )
        })?;
        if !entry.file_type().is_file() {
            continue;
        }
        let Ok(relative) = entry.path().strip_prefix(target) else {
            continue;
        };
        let relative = relative.to_string_lossy().replace('\\', "/");
        if !expected.contains(&relative) {
            std::fs::remove_file(entry.path()).map_err(|error| {
                GibError::new(
                    ErrorCode::Io,
                    format!(
                        "Failed to prune local file '{}': {error}",
                        entry.path().display()
                    ),
                )
            })?;
            removed.push(relative);
        }
    }
    removed.sort();
    Ok(removed)
}

fn map_restore_error(error: String) -> GibError {
    let lower = error.to_ascii_lowercase();
    let code = if lower.contains("invalid password") {
        ErrorCode::InvalidPassword
    } else if lower.contains("password") {
        ErrorCode::PasswordRequired
    } else if lower.contains("not found") {
        ErrorCode::BackupNotFound
    } else if lower.contains("deserialize") || lower.contains("serialize") {
        ErrorCode::Serialization
    } else if lower.contains("decrypt") || lower.contains("encrypt") {
        ErrorCode::Encryption
    } else {
        ErrorCode::Internal
    };
    GibError::new(code, error)
}
