use super::client::Gib;
use super::error::{ErrorCode, GibError};
use super::event::{GibEvent, OperationKind, OperationStarted, WarningEvent};
use super::repository::RepositoryRequest;
use crate::core::catalog::{
    load_backup_manifest, mark_catalog_degraded_state, remove_backup_from_catalog,
};
use crate::core::crypto::encode_file_bytes;
use crate::core::indexes::{
    load_chunk_indexes_with_version, remove_backup_summary, resolve_backup_reference,
    set_repository_head,
};
use crate::core::metadata::ChunkIndex;
use serde::Serialize;
use std::collections::HashMap;
use std::sync::{Arc, Mutex};

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DeleteBackupRequest {
    pub repository: RepositoryRequest,
    pub backup: String,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct DeleteBackupResult {
    pub backup: String,
    pub deleted_chunks: usize,
    pub remaining_backups: usize,
    pub head_published: bool,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct PruneItem {
    pub path: String,
    pub kind: String,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PruneRequest {
    pub repository: RepositoryRequest,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct PrunePlan {
    pub repository: RepositoryRequest,
    pub items: Vec<PruneItem>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct PruneFailure {
    pub path: String,
    pub message: String,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct PruneResult {
    pub deleted_items: usize,
    pub failures: Vec<PruneFailure>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct EncryptRepositoryRequest {
    pub repository: RepositoryRequest,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct EncryptRepositoryResult {
    pub encrypted_items: usize,
    pub skipped_items: usize,
}

impl Gib {
    pub async fn delete_backup(
        &self,
        request: DeleteBackupRequest,
    ) -> Result<DeleteBackupResult, GibError> {
        request.repository.validate()?;
        if request.backup.trim().is_empty() {
            return Err(GibError::new(
                ErrorCode::InvalidRequest,
                "A backup reference is required",
            ));
        }
        self.events()
            .emit(GibEvent::OperationStarted(OperationStarted {
                operation: OperationKind::Delete,
            }));
        let fs = self.backend(&request.repository.storage)?;
        let backup_hash = resolve_backup_reference(
            Arc::clone(&fs),
            request.repository.key.clone(),
            request.repository.password.clone(),
            &request.backup,
        )
        .await
        .map_err(map_repository_error)?;
        let backup = load_backup_manifest(
            &fs,
            &request.repository.key,
            request.repository.password.as_deref(),
            &backup_hash,
        )
        .await
        .map_err(map_repository_error)?;
        let (mut indexes, initial_version) = load_chunk_indexes_with_version(
            Arc::clone(&fs),
            request.repository.key.clone(),
            request.repository.password.clone(),
            Arc::new(Mutex::new(false)),
        )
        .await
        .map_err(map_repository_error)?;
        let initial_indexes = indexes.clone();
        for object in backup.tree.values() {
            decrement_refs(&mut indexes, object);
        }
        let remaining = remove_backup_summary(
            Arc::clone(&fs),
            request.repository.key.clone(),
            request.repository.password.clone(),
            3,
            &backup_hash,
        )
        .await
        .map_err(map_repository_error)?;
        crate::core::indexes::write_chunk_indexes_with_merge(
            Arc::clone(&fs),
            request.repository.key.clone(),
            request.repository.password.clone(),
            3,
            indexes.clone(),
            initial_indexes,
            initial_version,
        )
        .await
        .map_err(map_repository_error)?;
        if let Err(error) = remove_backup_from_catalog(
            Arc::clone(&fs),
            request.repository.key.clone(),
            request.repository.password.clone(),
            3,
            &backup,
            &remaining,
            &indexes,
        )
        .await
        {
            let _ = mark_catalog_degraded_state(
                &fs,
                &request.repository.key,
                request.repository.password.as_deref(),
                3,
            )
            .await;
            self.events().emit(GibEvent::Warning(WarningEvent {
                code: "catalog_degraded".to_string(),
                message: format!(
                    "Historical catalog cleanup was deferred; the backup deletion completed: {error}"
                ),
            }));
        }
        let mut deleted_chunks = 0;
        for object in backup.tree.values() {
            for hash in &object.chunks {
                if !indexes.contains_key(hash) {
                    let path = chunk_path(&request.repository.key, hash)?;
                    match fs.delete_file(&path).await {
                        Ok(()) => deleted_chunks += 1,
                        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
                        Err(error) => {
                            return Err(GibError::new(ErrorCode::Io, error.to_string()));
                        }
                    }
                }
            }
        }
        fs.delete_file(&format!(
            "{}/backups/{}",
            request.repository.key, backup_hash
        ))
        .await
        .map_err(|error| GibError::new(ErrorCode::Io, error.to_string()))?;

        let mut head_published = false;
        if let Some(head) = crate::core::indexes::read_repository_head(
            Arc::clone(&fs),
            request.repository.key.clone(),
            request.repository.password.clone(),
        )
        .await
        .map_err(map_repository_error)?
            && head.head.backup.as_deref() == Some(backup_hash.as_str())
        {
            head_published = set_repository_head(
                fs,
                request.repository.key.clone(),
                request.repository.password.clone(),
                &head,
                remaining.first().map(|summary| summary.hash.as_str()),
            )
            .await
            .map_err(map_repository_error)?;
        }
        Ok(DeleteBackupResult {
            backup: backup_hash,
            deleted_chunks,
            remaining_backups: remaining.len(),
            head_published,
        })
    }

    pub async fn plan_prune(&self, request: PruneRequest) -> Result<PrunePlan, GibError> {
        request.repository.validate()?;
        self.events()
            .emit(GibEvent::OperationStarted(OperationStarted {
                operation: OperationKind::Prune,
            }));
        let fs = self.backend(&request.repository.storage)?;
        let (indexes, _) = load_chunk_indexes_with_version(
            Arc::clone(&fs),
            request.repository.key.clone(),
            request.repository.password.clone(),
            Arc::new(Mutex::new(false)),
        )
        .await
        .map_err(map_repository_error)?;
        let chunk_paths = fs
            .list_files(&format!("{}/chunks", request.repository.key))
            .await
            .map_err(|error| GibError::new(ErrorCode::Io, error.to_string()))?;
        let mut items = chunk_paths
            .into_iter()
            .filter(|path| {
                let parts = path.rsplit('/').take(2).collect::<Vec<_>>();
                let hash = if parts.len() == 2 {
                    format!("{}{}", parts[1], parts[0])
                } else {
                    String::new()
                };
                !indexes.contains_key(&hash)
            })
            .map(|path| PruneItem {
                path,
                kind: "chunk".to_string(),
            })
            .collect::<Vec<_>>();
        let index_paths = fs
            .list_files(&format!("{}/indexes", request.repository.key))
            .await
            .map_err(|error| GibError::new(ErrorCode::Io, error.to_string()))?;
        items.extend(
            index_paths
                .into_iter()
                .filter(|path| {
                    path.rsplit('/')
                        .next()
                        .is_some_and(|name| name.starts_with("pending_"))
                })
                .map(|path| PruneItem {
                    path,
                    kind: "pending_backup".to_string(),
                }),
        );
        items.sort_by(|left, right| left.path.cmp(&right.path));
        Ok(PrunePlan {
            repository: request.repository,
            items,
        })
    }

    pub async fn execute_prune(&self, plan: PrunePlan) -> Result<PruneResult, GibError> {
        plan.repository.validate()?;
        self.events()
            .emit(GibEvent::OperationStarted(OperationStarted {
                operation: OperationKind::Prune,
            }));
        let fs = self.backend(&plan.repository.storage)?;
        let mut failures = Vec::new();
        for item in &plan.items {
            if let Err(error) = fs.delete_file(&item.path).await
                && error.kind() != std::io::ErrorKind::NotFound
            {
                failures.push(PruneFailure {
                    path: item.path.clone(),
                    message: error.to_string(),
                });
            }
        }
        Ok(PruneResult {
            deleted_items: plan.items.len().saturating_sub(failures.len()),
            failures,
        })
    }

    pub async fn encrypt_repository(
        &self,
        request: EncryptRepositoryRequest,
    ) -> Result<EncryptRepositoryResult, GibError> {
        request.repository.validate()?;
        self.events()
            .emit(GibEvent::OperationStarted(OperationStarted {
                operation: OperationKind::Encrypt,
            }));
        let password = request.repository.password.as_deref().ok_or_else(|| {
            GibError::new(
                ErrorCode::PasswordRequired,
                "Repository password is required",
            )
        })?;
        let fs = self.backend(&request.repository.storage)?;
        let paths = fs
            .list_files(&request.repository.key)
            .await
            .map_err(|error| GibError::new(ErrorCode::Io, error.to_string()))?;
        let mut encrypted_items = 0;
        let mut skipped_items = 0;
        for path in paths {
            let bytes = fs
                .read_file(&path)
                .await
                .map_err(|error| GibError::new(ErrorCode::Io, error.to_string()))?;
            if bytes.is_empty() || crate::utils::is_encrypted(&bytes) {
                skipped_items += 1;
                continue;
            }
            let encoded =
                encode_file_bytes(&bytes, Some(password)).map_err(map_repository_error)?;
            fs.write_file(&path, &encoded)
                .await
                .map_err(|error| GibError::new(ErrorCode::Io, error.to_string()))?;
            encrypted_items += 1;
        }
        Ok(EncryptRepositoryResult {
            encrypted_items,
            skipped_items,
        })
    }
}

fn decrement_refs(
    indexes: &mut HashMap<String, ChunkIndex>,
    backup: &crate::core::metadata::BackupObject,
) {
    for hash in &backup.chunks {
        if let Some(index) = indexes.get_mut(hash) {
            index.refcount = index.refcount.saturating_sub(1);
            if index.refcount == 0 {
                indexes.remove(hash);
            }
        }
    }
}

fn chunk_path(key: &str, hash: &str) -> Result<String, GibError> {
    let (prefix, rest) = hash
        .split_at_checked(2)
        .ok_or_else(|| GibError::new(ErrorCode::InvalidRequest, "Invalid chunk hash"))?;
    Ok(format!("{key}/chunks/{prefix}/{rest}"))
}

fn map_repository_error(error: String) -> GibError {
    let lower = error.to_ascii_lowercase();
    let code = if lower.contains("invalid password") {
        ErrorCode::InvalidPassword
    } else if lower.contains("password") {
        ErrorCode::PasswordRequired
    } else if lower.contains("not found") {
        ErrorCode::BackupNotFound
    } else if lower.contains("encrypt") || lower.contains("decrypt") {
        ErrorCode::Encryption
    } else if lower.contains("serialize") || lower.contains("deserialize") {
        ErrorCode::Serialization
    } else {
        ErrorCode::Internal
    };
    GibError::new(code, error)
}
