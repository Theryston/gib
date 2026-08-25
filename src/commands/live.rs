use crate::commands::backup::{
    BackupMode, BackupResult, ResolvedBackup, is_ignored_path, load_backup, resolve_backup,
    run_backup_with_parents,
};
use crate::core::indexes::read_or_initialize_repository_head;
use crate::core::live_state::{LiveState, load_live_state, save_live_state};
use crate::core::metadata::Backup;
use crate::core::reconcile::{
    ReconcileConflict, apply_remote_change, reconcile_worktree, worktree_matches_backup,
};
use crate::output::{emit_named_event, is_json_mode};
use crate::utils::handle_error;
use clap::ArgMatches;
use console::style;
use dialoguer::Select;
use notify::{Config, Event, EventKind, RecommendedWatcher, RecursiveMode, Watcher};
use serde::Serialize;
use std::collections::BTreeSet;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::mpsc::unbounded_channel;

const MAX_EVENT_PATHS: usize = 8;
const MAX_DISPLAY_PATHS: usize = 8;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum ConflictPolicy {
    Local,
    Remote,
}

impl ConflictPolicy {
    pub(crate) fn as_str(self) -> &'static str {
        match self {
            Self::Local => "local",
            Self::Remote => "remote",
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum ChangeKind {
    Created,
    Changed,
    Deleted,
}

#[derive(Default, Debug)]
struct ChangeBatch {
    created: BTreeSet<String>,
    changed: BTreeSet<String>,
    deleted: BTreeSet<String>,
}

impl ChangeBatch {
    fn is_empty(&self) -> bool {
        self.created.is_empty() && self.changed.is_empty() && self.deleted.is_empty()
    }

    fn record(&mut self, path: String, kind: ChangeKind) {
        match kind {
            ChangeKind::Created => {
                self.deleted.remove(&path);
                self.changed.remove(&path);
                self.created.insert(path);
            }
            ChangeKind::Changed => {
                if !self.created.contains(&path) && !self.deleted.contains(&path) {
                    self.changed.insert(path);
                }
            }
            ChangeKind::Deleted => {
                self.created.remove(&path);
                self.changed.remove(&path);
                self.deleted.insert(path);
            }
        }
    }

    fn paths(&self, kind: ChangeKind) -> &BTreeSet<String> {
        match kind {
            ChangeKind::Created => &self.created,
            ChangeKind::Changed => &self.changed,
            ChangeKind::Deleted => &self.deleted,
        }
    }
}

#[derive(Serialize)]
struct LiveStartPayload {
    event: &'static str,
    root: String,
    storage: String,
    key: String,
    conflict: &'static str,
    recursive: bool,
    debounce_ms: u64,
    poll_ms: u64,
    ignore: Vec<String>,
}

#[derive(Serialize)]
struct ChangeGroupPayload {
    count: usize,
    paths: Vec<String>,
    truncated: usize,
}

#[derive(Serialize)]
struct LiveBatchPayload {
    event: &'static str,
    message: String,
    created: ChangeGroupPayload,
    changed: ChangeGroupPayload,
    deleted: ChangeGroupPayload,
}

#[derive(Serialize)]
struct LiveBackupStartedPayload {
    event: &'static str,
    message: String,
}

#[derive(Serialize)]
struct LiveBackupCompletedPayload {
    event: &'static str,
    backup: String,
    backup_short: String,
    message: String,
    files_total: usize,
    written_bytes: u64,
    deduplicated_bytes: u64,
    elapsed_ms: u64,
}

#[derive(Serialize)]
struct LiveSyncPayload {
    event: &'static str,
    applied_remote: usize,
    merged_text: usize,
}

#[derive(Serialize)]
struct LiveConflictItem {
    path: String,
    reason: String,
}

#[derive(Serialize)]
struct LiveConflictPayload {
    event: &'static str,
    conflicts: Vec<LiveConflictItem>,
    recoverable: bool,
    resolution: &'static str,
}

#[derive(Serialize)]
struct LiveErrorPayload {
    event: &'static str,
    message: String,
    recoverable: bool,
}

#[derive(Serialize)]
struct LiveStopPayload {
    event: &'static str,
}

pub(crate) fn conflict_policy_from_name(value: &str) -> Result<ConflictPolicy, String> {
    match value {
        "local" => Ok(ConflictPolicy::Local),
        "remote" => Ok(ConflictPolicy::Remote),
        _ => Err(format!(
            "Unsupported conflict policy '{}'; use 'local' or 'remote'",
            value
        )),
    }
}

fn resolve_conflict_policy(matches: &ArgMatches) -> Result<Option<ConflictPolicy>, String> {
    match matches.get_one::<String>("conflict").map(String::as_str) {
        Some(value) => conflict_policy_from_name(value).map(Some),
        None if is_json_mode() => Err(
            "The --conflict flag is required when --mode json is used with gib live; choose 'local' or 'remote'"
                .to_string(),
        ),
        None => Ok(None),
    }
}

pub(crate) fn emit_live_start(resolved: &ResolvedBackup, policy: ConflictPolicy) {
    let root = PathBuf::from(&resolved.options.root_path_string);
    emit_named_event(
        "live",
        &LiveStartPayload {
            event: "start",
            root: root.to_string_lossy().to_string(),
            storage: resolved.options.storage.clone(),
            key: resolved.options.key.clone(),
            conflict: policy.as_str(),
            recursive: true,
            debounce_ms: resolved.live_debounce_ms,
            poll_ms: resolved.live_poll_ms,
            ignore: resolved.options.ignore_patterns.clone(),
        },
    );
}

pub(crate) async fn run_live(
    resolved: ResolvedBackup,
    conflict_policy: ConflictPolicy,
) -> Result<(), String> {
    live_loop(resolved, Some(conflict_policy)).await
}

pub async fn live(matches: &ArgMatches) {
    let conflict_policy = match resolve_conflict_policy(matches) {
        Ok(policy) => policy,
        Err(error) => handle_error(error, None),
    };
    let resolved = match resolve_backup(matches, BackupMode::Live).await {
        Ok(resolved) => resolved,
        Err(error) => handle_error(error, None),
    };

    let root = PathBuf::from(&resolved.options.root_path_string);
    if !root.is_dir() {
        handle_error(
            format!(
                "Live root '{}' is not an existing directory",
                root.display()
            ),
            None,
        );
    }

    if is_json_mode() {
        if let Some(policy) = conflict_policy {
            emit_live_start(&resolved, policy);
        }
    } else {
        println!("{}", style("GIB live started").cyan().bold());
        println!("{} {}", style("Root").bold(), root.to_string_lossy());
        println!(
            "{} {} / {}",
            style("Target").bold(),
            resolved.options.storage,
            resolved.options.key
        );
        if !resolved.options.ignore_patterns.is_empty() {
            println!(
                "{} {}",
                style("Ignoring").bold(),
                resolved.options.ignore_patterns.join(", ")
            );
        }
        println!(
            "{}",
            style("Waiting for changes... Press Ctrl+C to stop.").dim()
        );
        println!(
            "{} {}",
            style("Remote sync interval").bold(),
            format_duration(resolved.live_poll_ms)
        );
    }

    if let Err(error) = match conflict_policy {
        Some(policy) => run_live(resolved, policy).await,
        None => live_loop(resolved, None).await,
    } {
        handle_error(error, None);
    }
}

async fn live_loop(
    resolved: ResolvedBackup,
    conflict_policy: Option<ConflictPolicy>,
) -> Result<(), String> {
    let root = PathBuf::from(&resolved.options.root_path_string);
    let ignore_patterns = resolved.options.ignore_patterns.clone();
    let debounce_window = Duration::from_millis(resolved.live_debounce_ms);
    let (mut state, first_run) = initialize_live_state(&resolved, &root).await?;
    let startup_message = if first_run {
        "[LIVE] initial synchronization"
    } else {
        "[LIVE] resumed synchronization"
    };
    match synchronize_live(
        &resolved,
        &mut state,
        Some(startup_message.to_string()),
        conflict_policy,
    )
    .await
    {
        Ok(outcome) => emit_sync_outcome(outcome),
        Err(error) => emit_live_error(format!("Initial synchronization failed: {}", error), true),
    }
    let (sender, mut receiver) = unbounded_channel::<notify::Result<Event>>();

    let mut watcher = RecommendedWatcher::new(
        move |result| {
            let _ = sender.send(result);
        },
        Config::default(),
    )
    .map_err(|error| format!("Failed to start filesystem watcher: {}", error))?;

    watcher
        .watch(&root, RecursiveMode::Recursive)
        .map_err(|error| format!("Failed to monitor '{}': {}", root.display(), error))?;

    let ctrl_c = tokio::signal::ctrl_c();
    tokio::pin!(ctrl_c);
    let mut poll_interval = tokio::time::interval(Duration::from_millis(resolved.live_poll_ms));
    poll_interval.tick().await;

    loop {
        let first_event = tokio::select! {
            event = receiver.recv() => {
                let Some(event) = event else {
                    return Err("Filesystem watcher stopped unexpectedly".to_string());
                };
                Some(event)
            }
            _ = poll_interval.tick() => {
                process_remote_sync(&resolved, &mut state, conflict_policy).await;
                None
            }
            _ = &mut ctrl_c => {
                emit_live_stop();
                return Ok(());
            }
        };

        let Some(first_event) = first_event else {
            continue;
        };

        let mut batch = ChangeBatch::default();
        record_notify_result(first_event, &root, &ignore_patterns, &mut batch);

        loop {
            let quiet_period = tokio::time::sleep(debounce_window);
            tokio::pin!(quiet_period);

            tokio::select! {
                _ = &mut quiet_period => break,
                event = receiver.recv() => {
                    let Some(event) = event else {
                        return Err("Filesystem watcher stopped unexpectedly".to_string());
                    };
                    record_notify_result(event, &root, &ignore_patterns, &mut batch);
                }
                _ = &mut ctrl_c => {
                    emit_live_stop();
                    return Ok(());
                }
            }
        }

        if batch.is_empty() {
            continue;
        }

        process_batch(&resolved, &batch, &mut state, conflict_policy).await;
    }
}

async fn initialize_live_state(
    resolved: &ResolvedBackup,
    root: &Path,
) -> Result<(LiveState, bool), String> {
    let mut state = load_live_state(root, &resolved.options.storage, &resolved.options.key)?;
    if state.initialized {
        return Ok((state, false));
    }

    state.initialized = true;
    state.base_backup = None;
    save_live_state(
        root,
        &resolved.options.storage,
        &resolved.options.key,
        &state,
    )?;
    Ok((state, true))
}

fn record_notify_result(
    result: notify::Result<Event>,
    root: &Path,
    ignore_patterns: &[String],
    batch: &mut ChangeBatch,
) {
    let event = match result {
        Ok(event) => event,
        Err(error) => {
            emit_live_error(error.to_string(), true);
            return;
        }
    };

    let kind = match event.kind {
        EventKind::Create(_) => ChangeKind::Created,
        EventKind::Modify(_) => ChangeKind::Changed,
        EventKind::Remove(_) => ChangeKind::Deleted,
        _ => return,
    };

    for path in event.paths {
        if path == root || is_ignored_path(&path, root, ignore_patterns) {
            continue;
        }
        if kind != ChangeKind::Deleted && path.is_dir() {
            continue;
        }

        let relative_path = path
            .strip_prefix(root)
            .unwrap_or(&path)
            .to_string_lossy()
            .replace('\\', "/")
            .trim_matches('/')
            .to_string();

        if !relative_path.is_empty() {
            batch.record(relative_path, kind);
        }
    }
}

async fn process_remote_sync(
    resolved: &ResolvedBackup,
    state: &mut LiveState,
    conflict_policy: Option<ConflictPolicy>,
) {
    match synchronize_live(resolved, state, None, conflict_policy).await {
        Ok(outcome) => emit_sync_outcome(outcome),
        Err(error) => emit_live_error(format!("Synchronization failed: {}", error), true),
    }
}

async fn process_batch(
    resolved: &ResolvedBackup,
    batch: &ChangeBatch,
    state: &mut LiveState,
    conflict_policy: Option<ConflictPolicy>,
) {
    let message = build_live_message(&resolved.message, batch);
    emit_live_batch(batch, &message);

    match synchronize_live(resolved, state, Some(message), conflict_policy).await {
        Ok(outcome) => emit_sync_outcome(outcome),
        Err(error) => emit_live_error(format!("Backup failed: {}", error), true),
    }
}

#[derive(Default)]
struct SyncOutcome {
    backup: Option<BackupResult>,
    applied_remote: usize,
    merged_text: usize,
}

async fn synchronize_live(
    resolved: &ResolvedBackup,
    state: &mut LiveState,
    message: Option<String>,
    conflict_policy: Option<ConflictPolicy>,
) -> Result<SyncOutcome, String> {
    let root = PathBuf::from(&resolved.options.root_path_string);

    for attempt in 0..3 {
        let head = read_or_initialize_repository_head(
            Arc::clone(&resolved.options.fs),
            resolved.options.key.clone(),
            resolved.options.password.clone(),
        )
        .await?;
        let remote_hash = head.head.backup.clone();
        let base_hash = state.base_backup.clone();

        let remote_backup = match remote_hash.as_deref() {
            Some(hash) => Some(
                load_backup(
                    Arc::clone(&resolved.options.fs),
                    resolved.options.key.clone(),
                    resolved.options.password.clone(),
                    hash,
                )
                .await?,
            ),
            None => None,
        };
        let base_backup = match base_hash.as_deref() {
            Some(hash) if Some(hash) == remote_hash.as_deref() => remote_backup.clone(),
            Some(hash) => Some(
                load_backup(
                    Arc::clone(&resolved.options.fs),
                    resolved.options.key.clone(),
                    resolved.options.password.clone(),
                    hash,
                )
                .await?,
            ),
            None => None,
        };

        let remote_changed = base_hash != remote_hash;
        let remote_tree = remote_backup.clone().unwrap_or_else(empty_backup);
        let mut outcome = SyncOutcome::default();

        if !remote_changed && message.is_none() {
            return Ok(outcome);
        }

        if remote_changed {
            let reconciliation = reconcile_worktree(
                &root,
                &resolved.options.ignore_patterns,
                base_backup.as_ref(),
                &remote_tree,
                Arc::clone(&resolved.options.fs),
                &resolved.options.key,
                resolved.options.password.as_deref(),
            )
            .await?;
            outcome.applied_remote = reconciliation.applied_remote;
            outcome.merged_text = reconciliation.merged_text;

            if !reconciliation.conflicts.is_empty() {
                if let Some(policy) = conflict_policy.filter(|_| is_json_mode()) {
                    emit_live_conflicts(&reconciliation.conflicts, policy);
                }
                resolve_interactive_conflicts(
                    &root,
                    reconciliation.conflicts,
                    Arc::clone(&resolved.options.fs),
                    &resolved.options.key,
                    resolved.options.password.as_deref(),
                    conflict_policy,
                )
                .await?;
            }
        }

        if worktree_matches_backup(&root, &resolved.options.ignore_patterns, &remote_tree)? {
            state.initialized = true;
            state.base_backup = remote_hash;
            save_live_state(
                &root,
                &resolved.options.storage,
                &resolved.options.key,
                state,
            )?;
            return Ok(outcome);
        }

        let mut parents = Vec::new();
        if let Some(hash) = remote_hash {
            parents.push(hash);
        }
        if let Some(hash) = base_hash
            && !parents.iter().any(|parent| parent == &hash)
        {
            parents.push(hash);
        }

        let backup_message = message.clone().unwrap_or_else(|| {
            if remote_changed {
                "[LIVE] synchronized remote changes".to_string()
            } else {
                "[LIVE] local changes".to_string()
            }
        });
        emit_live_backup_started(&backup_message);

        let result =
            run_backup_with_parents(resolved.options.clone(), backup_message, parents, None)
                .await?;

        if result.head_published {
            state.initialized = true;
            state.base_backup = Some(result.backup.hash.clone());
            save_live_state(
                &root,
                &resolved.options.storage,
                &resolved.options.key,
                state,
            )?;
            outcome.backup = Some(result);
            return Ok(outcome);
        }

        if attempt == 2 {
            return Err(
                "The repository changed while publishing the backup; please retry after the other live process finishes"
                    .to_string(),
            );
        }
    }

    Err("Failed to synchronize live changes after several concurrent updates".to_string())
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

async fn resolve_interactive_conflicts(
    root: &Path,
    conflicts: Vec<ReconcileConflict>,
    fs: Arc<dyn crate::fs::FS>,
    key: &str,
    password: Option<&str>,
    conflict_policy: Option<ConflictPolicy>,
) -> Result<(), String> {
    for conflict in conflicts {
        let use_remote = match conflict_policy {
            Some(ConflictPolicy::Local) => false,
            Some(ConflictPolicy::Remote) => true,
            None => {
                let choices = ["Keep local", "Use remote"];
                let selection = Select::new()
                    .with_prompt(format!(
                        "Conflict in '{}' ({})",
                        conflict.path, conflict.reason
                    ))
                    .items(choices)
                    .default(0)
                    .interact()
                    .map_err(|error| format!("Failed to resolve conflict: {}", error))?;
                selection == 1
            }
        };

        if use_remote {
            apply_remote_change(
                root,
                &conflict.path,
                conflict.remote.as_ref(),
                Arc::clone(&fs),
                key,
                password,
            )
            .await?;
        }
    }
    Ok(())
}

fn emit_live_batch(batch: &ChangeBatch, message: &str) {
    if is_json_mode() {
        emit_named_event(
            "live",
            &LiveBatchPayload {
                event: "change_batch",
                message: message.to_string(),
                created: change_group_payload(batch.paths(ChangeKind::Created)),
                changed: change_group_payload(batch.paths(ChangeKind::Changed)),
                deleted: change_group_payload(batch.paths(ChangeKind::Deleted)),
            },
        );
    } else {
        println!(
            "{} {} created, {} changed, {} deleted",
            style("Changes").bold(),
            batch.created.len(),
            batch.changed.len(),
            batch.deleted.len()
        );
        for (label, kind) in [
            ("created", ChangeKind::Created),
            ("changed", ChangeKind::Changed),
            ("deleted", ChangeKind::Deleted),
        ] {
            let paths = batch.paths(kind);
            if !paths.is_empty() {
                println!("  {}: {}", style(label).dim(), format_limited_paths(paths));
            }
        }
    }
}

fn emit_backup_completed(result: BackupResult) {
    let backup_short = result.backup.hash[..8.min(result.backup.hash.len())].to_string();

    if is_json_mode() {
        emit_named_event(
            "live",
            &LiveBackupCompletedPayload {
                event: "backup_complete",
                backup: result.backup.hash,
                backup_short,
                message: result.backup.message,
                files_total: result.files_total,
                written_bytes: result.written_bytes,
                deduplicated_bytes: result.deduplicated_bytes,
                elapsed_ms: result.elapsed_ms,
            },
        );
    } else {
        println!(
            "{} {} ({})",
            style("Backup created").green().bold(),
            backup_short,
            result.backup.message
        );
    }
}

fn emit_live_backup_started(message: &str) {
    if is_json_mode() {
        emit_named_event(
            "live",
            &LiveBackupStartedPayload {
                event: "backup_start",
                message: message.to_string(),
            },
        );
    } else {
        println!("{} {}", style("Backup").bold(), message);
    }
}

fn emit_sync_outcome(outcome: SyncOutcome) {
    if outcome.applied_remote > 0 || outcome.merged_text > 0 {
        if is_json_mode() {
            emit_named_event(
                "live",
                &LiveSyncPayload {
                    event: "synchronized",
                    applied_remote: outcome.applied_remote,
                    merged_text: outcome.merged_text,
                },
            );
        } else {
            println!(
                "{} {} remote changes, {} text merges",
                style("Synchronized").green().bold(),
                outcome.applied_remote,
                outcome.merged_text
            );
        }
    }

    if let Some(result) = outcome.backup {
        emit_backup_completed(result);
    }
}

fn emit_live_conflicts(conflicts: &[ReconcileConflict], policy: ConflictPolicy) {
    if is_json_mode() {
        emit_named_event(
            "live",
            &LiveConflictPayload {
                event: "conflict",
                conflicts: conflicts
                    .iter()
                    .map(|conflict| LiveConflictItem {
                        path: conflict.path.clone(),
                        reason: conflict.reason.clone(),
                    })
                    .collect(),
                recoverable: true,
                resolution: policy.as_str(),
            },
        );
    }
}

fn emit_live_error(message: String, recoverable: bool) {
    if is_json_mode() {
        emit_named_event(
            "live",
            &LiveErrorPayload {
                event: "error",
                message,
                recoverable,
            },
        );
    } else {
        eprintln!("{} {}", style("Live error").red().bold(), message);
    }
}

fn emit_live_stop() {
    if is_json_mode() {
        emit_named_event("live", &LiveStopPayload { event: "stop" });
    } else {
        println!("{}", style("GIB live stopped").cyan().bold());
    }
}

fn build_live_message(prefix: &str, batch: &ChangeBatch) -> String {
    let mut sections = Vec::new();
    for (label, kind) in [
        ("created", ChangeKind::Created),
        ("changed", ChangeKind::Changed),
        ("deleted", ChangeKind::Deleted),
    ] {
        let paths = batch.paths(kind);
        if !paths.is_empty() {
            sections.push(format!("{}: {}", label, format_file_count(paths.len())));
        }
    }

    let summary = sections.join("; ");
    let prefix = prefix.trim();
    if prefix.is_empty() {
        format!("[LIVE] {}", summary)
    } else {
        format!("[LIVE] {} — {}", prefix, summary)
    }
}

fn format_file_count(count: usize) -> String {
    let noun = if count == 1 { "file" } else { "files" };
    format!("{} {}", count, noun)
}

fn format_duration(milliseconds: u64) -> String {
    if milliseconds % 1_000 == 0 {
        format!("{}s", milliseconds / 1_000)
    } else {
        format!("{}ms", milliseconds)
    }
}

fn format_limited_paths(paths: &BTreeSet<String>) -> String {
    let selected = paths
        .iter()
        .take(MAX_DISPLAY_PATHS)
        .cloned()
        .collect::<Vec<_>>();
    let remaining = paths.len().saturating_sub(selected.len());

    if remaining == 0 {
        selected.join(", ")
    } else {
        format!("{} (+{} more)", selected.join(", "), remaining)
    }
}

fn change_group_payload(paths: &BTreeSet<String>) -> ChangeGroupPayload {
    let selected = paths
        .iter()
        .take(MAX_EVENT_PATHS)
        .cloned()
        .collect::<Vec<_>>();
    ChangeGroupPayload {
        count: paths.len(),
        truncated: paths.len().saturating_sub(selected.len()),
        paths: selected,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::commands::backup::{BackupOptions, run_backup};
    use crate::fs::LocalFS;
    use std::sync::Arc;
    use std::time::{SystemTime, UNIX_EPOCH};

    fn temporary_directory() -> PathBuf {
        let suffix = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("system clock should be after unix epoch")
            .as_nanos();
        let path = std::env::temp_dir().join(format!("gib-live-test-{suffix}"));
        std::fs::create_dir_all(&path).expect("temporary directory should be created");
        path
    }

    #[test]
    fn change_batch_keeps_the_final_state_per_path() {
        let mut batch = ChangeBatch::default();
        batch.record("created.txt".to_string(), ChangeKind::Created);
        batch.record("created.txt".to_string(), ChangeKind::Changed);
        batch.record("deleted.txt".to_string(), ChangeKind::Changed);
        batch.record("deleted.txt".to_string(), ChangeKind::Deleted);
        batch.record("replaced.txt".to_string(), ChangeKind::Deleted);
        batch.record("replaced.txt".to_string(), ChangeKind::Created);

        assert!(batch.created.contains("created.txt"));
        assert!(batch.deleted.contains("deleted.txt"));
        assert!(batch.created.contains("replaced.txt"));
        assert!(batch.changed.is_empty());
    }

    #[test]
    fn live_message_contains_actions_and_counts_only() {
        let mut batch = ChangeBatch::default();
        batch.record("created.txt".to_string(), ChangeKind::Created);
        batch.record("changed-a.txt".to_string(), ChangeKind::Changed);
        batch.record("changed-b.txt".to_string(), ChangeKind::Changed);
        batch.record("deleted.txt".to_string(), ChangeKind::Deleted);

        let message = build_live_message("autosave", &batch);
        assert_eq!(
            message,
            "[LIVE] autosave — created: 1 file; changed: 2 files; deleted: 1 file"
        );
        assert!(!message.contains("created.txt"));
        assert!(!message.contains("changed-a.txt"));
    }

    #[test]
    fn ignored_paths_match_backup_name_filtering() {
        let root = Path::new("/workspace");
        let patterns = vec!["node_modules".to_string(), ".git".to_string()];

        assert!(is_ignored_path(
            Path::new("/workspace/node_modules/pkg/index.js"),
            root,
            &patterns
        ));
        assert!(!is_ignored_path(
            Path::new("/workspace/src/main.rs"),
            root,
            &patterns
        ));
    }

    #[tokio::test]
    async fn synchronizes_a_remote_change_before_the_next_local_backup() {
        let fixture = temporary_directory();
        let source_one = fixture.join("machine-one");
        let source_two = fixture.join("machine-two");
        let source_three = fixture.join("machine-three");
        let storage = fixture.join("storage");
        std::fs::create_dir_all(&source_one).unwrap();
        std::fs::create_dir_all(&source_two).unwrap();
        std::fs::create_dir_all(&source_three).unwrap();
        std::fs::write(source_one.join("shared.txt"), b"base\n").unwrap();
        std::fs::write(source_two.join("shared.txt"), b"base\n").unwrap();

        let options_one = BackupOptions {
            key: "project".to_string(),
            root_path_string: source_one.to_string_lossy().to_string(),
            storage: "test".to_string(),
            fs: Arc::new(LocalFS::new(&storage)),
            author: "tester <tester@example.com>".to_string(),
            compress: 3,
            password: None,
            chunk_size: 1024,
            ignore_patterns: Vec::new(),
            concurrency: 1,
        };
        let options_two = BackupOptions {
            root_path_string: source_two.to_string_lossy().to_string(),
            ..options_one.clone()
        };

        let initial = run_backup(
            options_one.clone(),
            "[LIVE] initial".to_string(),
            None,
            None,
        )
        .await
        .unwrap();

        let resolved_one = ResolvedBackup {
            options: options_one,
            message: String::new(),
            parent_hash: None,
            pending_backup: None,
            live_debounce_ms: 300,
            live_poll_ms: 2_000,
        };
        let resolved_two = ResolvedBackup {
            options: options_two,
            message: String::new(),
            parent_hash: None,
            pending_backup: None,
            live_debounce_ms: 300,
            live_poll_ms: 2_000,
        };

        let mut state_one = LiveState {
            version: 1,
            initialized: true,
            base_backup: Some(initial.backup.hash.clone()),
        };
        let mut state_two = LiveState {
            version: 1,
            initialized: true,
            base_backup: Some(initial.backup.hash),
        };

        std::fs::write(source_one.join("shared.txt"), b"base\nfrom one\n").unwrap();
        let first_sync = synchronize_live(
            &resolved_one,
            &mut state_one,
            Some("[LIVE] changed".to_string()),
            None,
        )
        .await
        .unwrap();
        let published_hash = first_sync
            .backup
            .as_ref()
            .expect("machine one should publish a backup")
            .backup
            .hash
            .clone();

        std::fs::write(source_two.join("shared.txt"), b"base\nfrom two\n").unwrap();
        let second_sync = synchronize_live(
            &resolved_two,
            &mut state_two,
            None,
            Some(ConflictPolicy::Remote),
        )
        .await
        .unwrap();

        assert_eq!(second_sync.applied_remote, 0);
        assert_eq!(
            std::fs::read_to_string(source_two.join("shared.txt")).unwrap(),
            "base\nfrom one\n"
        );
        assert_eq!(
            state_two.base_backup.as_deref(),
            Some(published_hash.as_str())
        );

        std::fs::write(source_one.join("shared.txt"), b"base\nfrom upstream\n").unwrap();
        let second_remote_sync = synchronize_live(
            &resolved_one,
            &mut state_one,
            Some("[LIVE] upstream change".to_string()),
            None,
        )
        .await
        .unwrap();
        let second_published_hash = second_remote_sync
            .backup
            .as_ref()
            .expect("machine one should publish the second backup")
            .backup
            .hash
            .clone();

        std::fs::write(source_two.join("shared.txt"), b"base\nfrom local\n").unwrap();
        let local_sync = synchronize_live(
            &resolved_two,
            &mut state_two,
            None,
            Some(ConflictPolicy::Local),
        )
        .await
        .unwrap();

        assert!(local_sync.backup.is_some());
        assert_eq!(
            std::fs::read_to_string(source_two.join("shared.txt")).unwrap(),
            "base\nfrom local\n"
        );
        assert_ne!(
            state_two.base_backup.as_deref(),
            Some(second_published_hash.as_str())
        );

        let options_three = BackupOptions {
            root_path_string: source_three.to_string_lossy().to_string(),
            ..resolved_two.options.clone()
        };
        let resolved_three = ResolvedBackup {
            options: options_three,
            message: String::new(),
            parent_hash: None,
            pending_backup: None,
            live_debounce_ms: 300,
            live_poll_ms: 2_000,
        };
        let (mut state_three, first_run) = initialize_live_state(&resolved_three, &source_three)
            .await
            .unwrap();
        assert!(first_run);
        synchronize_live(
            &resolved_three,
            &mut state_three,
            Some("[LIVE] initial synchronization".to_string()),
            None,
        )
        .await
        .unwrap();
        assert_eq!(
            std::fs::read_to_string(source_three.join("shared.txt")).unwrap(),
            "base\nfrom local\n"
        );

        let _ = std::fs::remove_dir_all(fixture);
    }
}
