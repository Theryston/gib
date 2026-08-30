use super::backup::BackupRequest;
use super::client::{Gib, path_from_context};
use super::error::{ErrorCode, GibError};
use super::event::{GibEvent, LiveEvent};
use super::repository::RepositoryRequest;
use crate::core::catalog::load_backup_manifest;
use crate::core::indexes::read_or_initialize_repository_head;
use crate::core::live_state::{LiveFileCache, LiveState, load_live_state_at, save_live_state_at};
use crate::core::metadata::Backup;
use crate::core::reconcile::{
    apply_remote_change, reconcile_worktree, update_worktree_cache_from_backup,
    worktree_matches_backup_with_cache,
};
use serde::Serialize;
use std::collections::BTreeMap;
use std::fmt;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use tokio::sync::Notify;
use tokio::task::JoinHandle;
use tokio::time::{Duration, sleep};

#[non_exhaustive]
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ConflictPolicy {
    Local,
    Remote,
}

#[derive(Clone)]
pub struct LiveRequest {
    pub repository: RepositoryRequest,
    pub root_path: PathBuf,
    pub message: Option<String>,
    pub compression: i32,
    pub chunk_size: u64,
    pub ignore_patterns: Vec<String>,
    pub include_git: bool,
    pub concurrency: usize,
    pub conflict: ConflictPolicy,
    pub debounce: Duration,
    pub poll_interval: Duration,
}

impl LiveRequest {
    pub fn new(repository: RepositoryRequest, root_path: impl Into<PathBuf>) -> Self {
        Self {
            repository,
            root_path: root_path.into(),
            message: None,
            compression: 3,
            chunk_size: 5 * 1024 * 1024,
            ignore_patterns: Vec::new(),
            include_git: false,
            concurrency: num_cpus::get().saturating_mul(2).max(1),
            conflict: ConflictPolicy::Local,
            debounce: Duration::from_millis(300),
            poll_interval: Duration::from_secs(2),
        }
    }
}

impl fmt::Debug for LiveRequest {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("LiveRequest")
            .field("repository", &self.repository)
            .field("root_path", &self.root_path)
            .field("message", &self.message)
            .field("compression", &self.compression)
            .field("chunk_size", &self.chunk_size)
            .field("ignore_patterns", &self.ignore_patterns)
            .field("include_git", &self.include_git)
            .field("concurrency", &self.concurrency)
            .field("conflict", &self.conflict)
            .field("debounce", &self.debounce)
            .field("poll_interval", &self.poll_interval)
            .finish()
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct LiveResult {
    pub backups_created: u64,
    pub stopped: bool,
}

struct StopSignal {
    requested: AtomicBool,
    notify: Notify,
}

pub struct LiveHandle {
    stop: Arc<StopSignal>,
    done: Mutex<Option<JoinHandle<Result<LiveResult, GibError>>>>,
}

impl fmt::Debug for LiveHandle {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("LiveHandle")
            .field(
                "stop_requested",
                &self.stop.requested.load(Ordering::Acquire),
            )
            .finish()
    }
}

impl LiveHandle {
    /// Requests cancellation. The current filesystem or storage operation is
    /// allowed to finish and the live task exits at its next boundary.
    pub async fn stop(&self) -> Result<(), GibError> {
        self.stop.requested.store(true, Ordering::Release);
        self.stop.notify.notify_waiters();
        Ok(())
    }

    pub async fn wait(&self) -> Result<LiveResult, GibError> {
        let handle = self
            .done
            .lock()
            .map_err(|_| GibError::new(ErrorCode::Internal, "Live handle lock is poisoned"))?
            .take()
            .ok_or_else(|| GibError::new(ErrorCode::Internal, "Live handle was already awaited"))?;
        handle.await.map_err(|error| {
            GibError::new(ErrorCode::Internal, format!("Live task failed: {error}"))
        })?
    }
}

impl Drop for LiveHandle {
    fn drop(&mut self) {
        self.stop.requested.store(true, Ordering::Release);
        self.stop.notify.notify_waiters();
    }
}

#[derive(Default)]
struct SyncOutcome {
    backup: Option<super::backup::BackupResult>,
    applied_remote: usize,
    merged_text: usize,
    remote_changed: bool,
}

impl Gib {
    pub async fn start_live(&self, request: LiveRequest) -> Result<LiveHandle, GibError> {
        validate_live_request(&request)?;
        let root = path_from_context(&self.inner.context, &request.root_path);
        if !root.is_dir() {
            return Err(GibError::new(
                ErrorCode::InvalidRequest,
                format!("Live root '{}' is not a directory", root.display()),
            ));
        }

        let stop = Arc::new(StopSignal {
            requested: AtomicBool::new(false),
            notify: Notify::new(),
        });
        let stop_task = Arc::clone(&stop);
        let client = self.clone();
        let task =
            tokio::spawn(async move { run_live_task(client, request, root, stop_task).await });
        Ok(LiveHandle {
            stop,
            done: Mutex::new(Some(task)),
        })
    }
}

async fn run_live_task(
    client: Gib,
    request: LiveRequest,
    root: PathBuf,
    stop: Arc<StopSignal>,
) -> Result<LiveResult, GibError> {
    client.events().emit(GibEvent::Live(LiveEvent {
        event: "started".to_string(),
        message: Some(format!("Conflict policy: {:?}", request.conflict)),
        paths: None,
        applied_remote: None,
        merged_text: None,
    }));

    let mut state = load_live_state_at(
        &client.inner.context.data_dir,
        &root,
        &request.repository.storage,
        &request.repository.key,
    )
    .map_err(map_live_error)?;
    let mut cache = state.files.clone();
    let mut previous_signature = directory_signature(&root)?;
    let mut result = LiveResult {
        backups_created: 0,
        stopped: false,
    };

    match synchronize_live(&client, &request, &root, &mut state, &mut cache, true).await {
        Ok(outcome) => {
            record_sync_events(&client, &outcome);
            if outcome.backup.is_some() {
                result.backups_created = 1;
            }
        }
        Err(error) => {
            emit_live_error(&client, &error);
            client.events().emit(GibEvent::Live(LiveEvent {
                event: "stopped".to_string(),
                message: Some("Initial synchronization failed".to_string()),
                paths: None,
                applied_remote: None,
                merged_text: None,
            }));
            return Err(error);
        }
    }

    loop {
        if stop.requested.load(Ordering::Acquire) {
            result.stopped = true;
            break;
        }
        tokio::select! {
            _ = stop.notify.notified() => {
                result.stopped = true;
                break;
            }
            _ = sleep(request.poll_interval) => {
                let current_signature = match directory_signature(&root) {
                    Ok(signature) => signature,
                    Err(error) => {
                        emit_live_error(&client, &error);
                        continue;
                    }
                };
                let local_changed = current_signature != previous_signature;
                if local_changed {
                    client.events().emit(GibEvent::Live(LiveEvent {
                        event: "change_batch".to_string(),
                        message: Some("Local filesystem changes detected".to_string()),
                        paths: None,
                        applied_remote: None,
                        merged_text: None,
                    }));
                    tokio::select! {
                        _ = stop.notify.notified() => {
                            result.stopped = true;
                            break;
                        }
                        _ = sleep(request.debounce) => {}
                    }
                }
                match synchronize_live(
                    &client,
                    &request,
                    &root,
                    &mut state,
                    &mut cache,
                    local_changed,
                ).await {
                    Ok(outcome) => {
                        record_sync_events(&client, &outcome);
                        if outcome.backup.is_some() {
                            result.backups_created = result.backups_created.saturating_add(1);
                        }
                    }
                    Err(error) => emit_live_error(&client, &error),
                }
                previous_signature = current_signature;
            }
        }
    }

    client.events().emit(GibEvent::Live(LiveEvent {
        event: "stopped".to_string(),
        message: None,
        paths: None,
        applied_remote: None,
        merged_text: None,
    }));
    Ok(result)
}

async fn synchronize_live(
    client: &Gib,
    request: &LiveRequest,
    root: &Path,
    state: &mut LiveState,
    cache: &mut BTreeMap<String, LiveFileCache>,
    local_changed: bool,
) -> Result<SyncOutcome, GibError> {
    let fs = client.backend(&request.repository.storage)?;
    let head = read_or_initialize_repository_head(
        Arc::clone(&fs),
        request.repository.key.clone(),
        request.repository.password.clone(),
    )
    .await
    .map_err(map_live_error)?;
    let remote_hash = head.head.backup.clone();
    let remote_backup = match remote_hash.as_deref() {
        Some(hash) => Some(
            load_backup_manifest(
                &fs,
                &request.repository.key,
                request.repository.password.as_deref(),
                hash,
            )
            .await
            .map_err(map_live_error)?,
        ),
        None => None,
    };
    let base_hash = state.base_backup.clone();
    let remote_changed = base_hash != remote_hash;
    if !local_changed && !remote_changed && state.initialized {
        return Ok(SyncOutcome::default());
    }

    let mut outcome = SyncOutcome {
        remote_changed,
        ..SyncOutcome::default()
    };
    if remote_changed {
        let base_backup = match base_hash.as_deref() {
            Some(hash) if Some(hash) == remote_hash.as_deref() => remote_backup.clone(),
            Some(hash) => Some(
                load_backup_manifest(
                    &fs,
                    &request.repository.key,
                    request.repository.password.as_deref(),
                    hash,
                )
                .await
                .map_err(map_live_error)?,
            ),
            None => None,
        };
        let remote_tree = remote_backup.clone().unwrap_or_else(empty_backup);
        let reconciliation = reconcile_worktree(
            root,
            &request.ignore_patterns,
            base_backup.as_ref(),
            &remote_tree,
            Arc::clone(&fs),
            &request.repository.key,
            request.repository.password.as_deref(),
            cache,
        )
        .await
        .map_err(map_live_error)?;
        outcome.applied_remote = reconciliation.applied_remote;
        outcome.merged_text = reconciliation.merged_text;

        let mut local_changes = reconciliation.local_changes;
        if !reconciliation.conflicts.is_empty() {
            let conflicts = reconciliation.conflicts;
            let paths = conflicts
                .iter()
                .map(|conflict| conflict.path.clone())
                .collect::<Vec<_>>();
            client.events().emit(GibEvent::Live(LiveEvent {
                event: "conflict".to_string(),
                message: Some(format!("{} live conflict(s) detected", paths.len())),
                paths: Some(paths.clone()),
                applied_remote: None,
                merged_text: None,
            }));
            if request.conflict == ConflictPolicy::Remote {
                for conflict in conflicts {
                    apply_remote_change(
                        root,
                        &conflict.path,
                        conflict.remote.as_ref(),
                        Arc::clone(&fs),
                        &request.repository.key,
                        request.repository.password.as_deref(),
                    )
                    .await
                    .map_err(map_live_error)?;
                    local_changes.remove(&conflict.path);
                }
                client.events().emit(GibEvent::Live(LiveEvent {
                    event: "conflict_resolved".to_string(),
                    message: Some("Remote conflict resolution applied".to_string()),
                    paths: Some(paths),
                    applied_remote: None,
                    merged_text: None,
                }));
            }
        }

        if local_changes.is_empty() {
            if let Some(remote) = remote_backup.as_ref() {
                update_worktree_cache_from_backup(
                    root,
                    &request.ignore_patterns,
                    cache,
                    remote,
                    None,
                )
                .map_err(map_live_error)?;
            } else {
                cache.clear();
            }
            state.initialized = true;
            state.base_backup = remote_hash;
            state.files = cache.clone();
            save_live_state_at(
                &client.inner.context.data_dir,
                root,
                &request.repository.storage,
                &request.repository.key,
                state,
            )
            .map_err(map_live_error)?;
            return Ok(outcome);
        }
    }

    if !remote_changed && !local_changed {
        return Ok(outcome);
    }
    if let Some(remote) = remote_backup.as_ref()
        && worktree_matches_backup_with_cache(root, &request.ignore_patterns, remote, cache)
            .map_err(map_live_error)?
    {
        state.initialized = true;
        state.base_backup = remote_hash;
        state.files = cache.clone();
        save_live_state_at(
            &client.inner.context.data_dir,
            root,
            &request.repository.storage,
            &request.repository.key,
            state,
        )
        .map_err(map_live_error)?;
        return Ok(outcome);
    }

    let author = client
        .get_identity()
        .map(|identity| identity.author)
        .unwrap_or_else(|_| "anonymous <anonymous@trygib.org>".to_string());
    let message = request.message.clone().unwrap_or_else(|| {
        if remote_changed {
            "[LIVE] synchronized remote changes".to_string()
        } else {
            "[LIVE] local changes".to_string()
        }
    });
    client.events().emit(GibEvent::Live(LiveEvent {
        event: "backup_started".to_string(),
        message: Some(message.clone()),
        paths: None,
        applied_remote: None,
        merged_text: None,
    }));
    let backup = client
        .backup(BackupRequest {
            repository: request.repository.clone(),
            source_root: root.to_path_buf(),
            message,
            author,
            compression: request.compression,
            chunk_size: request.chunk_size,
            ignore_patterns: request.ignore_patterns.clone(),
            include_git: request.include_git,
            concurrency: request.concurrency,
            parent: remote_hash.clone(),
            resume: None,
        })
        .await?;
    let backup_manifest = load_backup_manifest(
        &fs,
        &request.repository.key,
        request.repository.password.as_deref(),
        &backup.backup.hash,
    )
    .await
    .map_err(map_live_error)?;
    state.initialized = true;
    state.base_backup = Some(backup.backup.hash.clone());
    update_worktree_cache_from_backup(
        root,
        &request.ignore_patterns,
        cache,
        &backup_manifest,
        None,
    )
    .map_err(map_live_error)?;
    state.files = cache.clone();
    save_live_state_at(
        &client.inner.context.data_dir,
        root,
        &request.repository.storage,
        &request.repository.key,
        state,
    )
    .map_err(map_live_error)?;
    outcome.backup = Some(backup);
    Ok(outcome)
}

fn record_sync_events(client: &Gib, outcome: &SyncOutcome) {
    if outcome.remote_changed || outcome.applied_remote > 0 || outcome.merged_text > 0 {
        client.events().emit(GibEvent::Live(LiveEvent {
            event: "synchronized".to_string(),
            message: None,
            paths: None,
            applied_remote: Some(outcome.applied_remote as u64),
            merged_text: Some(outcome.merged_text as u64),
        }));
    }
    if let Some(backup) = &outcome.backup {
        client.events().emit(GibEvent::Live(LiveEvent {
            event: "backup_completed".to_string(),
            message: Some(backup.backup.message.clone()),
            paths: None,
            applied_remote: None,
            merged_text: None,
        }));
    }
}

fn empty_backup() -> Backup {
    Backup {
        message: String::new(),
        hash: String::new(),
        timestamp: 0,
        author: String::new(),
        parents: Vec::new(),
        tree: std::collections::HashMap::new(),
    }
}

fn validate_live_request(request: &LiveRequest) -> Result<(), GibError> {
    request.repository.validate()?;
    if !(1..=22).contains(&request.compression)
        || request.chunk_size == 0
        || request.concurrency == 0
    {
        return Err(GibError::new(
            ErrorCode::InvalidRequest,
            "Live backup tuning values are invalid",
        ));
    }
    if request.poll_interval.is_zero() || request.debounce.is_zero() {
        return Err(GibError::new(
            ErrorCode::InvalidRequest,
            "Live timing values must be greater than zero",
        ));
    }
    Ok(())
}

fn directory_signature(path: &Path) -> Result<u64, GibError> {
    let mut signature = 0_u64;
    for entry in walkdir::WalkDir::new(path).into_iter() {
        let entry = entry.map_err(|error| GibError::new(ErrorCode::Io, error.to_string()))?;
        if !entry.file_type().is_file() {
            continue;
        }
        let metadata = entry
            .metadata()
            .map_err(|error| GibError::new(ErrorCode::Io, error.to_string()))?;
        signature = signature.wrapping_add(metadata.len()).wrapping_add(
            metadata
                .modified()
                .ok()
                .and_then(|time| time.duration_since(std::time::UNIX_EPOCH).ok())
                .map_or(0, |duration| duration.as_nanos() as u64),
        );
    }
    Ok(signature)
}

fn emit_live_error(client: &Gib, error: &GibError) {
    client.events().emit(GibEvent::Live(LiveEvent {
        event: "error".to_string(),
        message: Some(error.message().to_string()),
        paths: None,
        applied_remote: None,
        merged_text: None,
    }));
}

fn map_live_error(error: String) -> GibError {
    let lower = error.to_ascii_lowercase();
    let code = if lower.contains("invalid password") {
        ErrorCode::InvalidPassword
    } else if lower.contains("password") {
        ErrorCode::PasswordRequired
    } else if lower.contains("not found") {
        ErrorCode::BackupNotFound
    } else if lower.contains("decrypt") || lower.contains("encrypt") {
        ErrorCode::Encryption
    } else if lower.contains("serialize") || lower.contains("deserialize") {
        ErrorCode::Serialization
    } else {
        ErrorCode::Internal
    };
    GibError::new(code, error)
}
