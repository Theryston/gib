use crate::commands::backup::{
    BackupMode, BackupResult, ResolvedBackup, is_ignored_path, load_backup, resolve_backup,
    run_backup_with_parents,
};
use crate::core::indexes::read_or_initialize_repository_head;
use crate::core::metadata::Backup;
use crate::core::reconcile::{
    ReconcileConflict, apply_remote_change, reconcile_worktree, worktree_matches_backup,
};
use crate::core::watch_state::{WatchState, load_watch_state, save_watch_state};
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
struct WatchStartPayload {
    event: &'static str,
    root: String,
    storage: String,
    key: String,
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
struct WatchBatchPayload {
    event: &'static str,
    message: String,
    created: ChangeGroupPayload,
    changed: ChangeGroupPayload,
    deleted: ChangeGroupPayload,
}

#[derive(Serialize)]
struct WatchBackupStartedPayload {
    event: &'static str,
    message: String,
}

#[derive(Serialize)]
struct WatchBackupCompletedPayload {
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
struct WatchSyncPayload {
    event: &'static str,
    applied_remote: usize,
    merged_text: usize,
}

#[derive(Serialize)]
struct WatchConflictItem {
    path: String,
    reason: String,
}

#[derive(Serialize)]
struct WatchConflictPayload {
    event: &'static str,
    conflicts: Vec<WatchConflictItem>,
    recoverable: bool,
}

#[derive(Serialize)]
struct WatchErrorPayload {
    event: &'static str,
    message: String,
    recoverable: bool,
}

#[derive(Serialize)]
struct WatchStopPayload {
    event: &'static str,
}

pub async fn watch(matches: &ArgMatches) {
    let resolved = match resolve_backup(matches, BackupMode::Watch).await {
        Ok(resolved) => resolved,
        Err(error) => handle_error(error, None),
    };

    let root = PathBuf::from(&resolved.options.root_path_string);
    if !root.is_dir() {
        handle_error(
            format!(
                "Watch root '{}' is not an existing directory",
                root.display()
            ),
            None,
        );
    }

    if is_json_mode() {
        emit_named_event(
            "watch",
            &WatchStartPayload {
                event: "start",
                root: root.to_string_lossy().to_string(),
                storage: resolved.options.storage.clone(),
                key: resolved.options.key.clone(),
                recursive: true,
                debounce_ms: resolved.watch_debounce_ms,
                poll_ms: resolved.watch_poll_ms,
                ignore: resolved.options.ignore_patterns.clone(),
            },
        );
    } else {
        println!("{}", style("GIB watch started").cyan().bold());
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
            format_duration(resolved.watch_poll_ms)
        );
    }

    if let Err(error) = watch_loop(resolved).await {
        handle_error(error, None);
    }
}

async fn watch_loop(resolved: ResolvedBackup) -> Result<(), String> {
    let root = PathBuf::from(&resolved.options.root_path_string);
    let ignore_patterns = resolved.options.ignore_patterns.clone();
    let debounce_window = Duration::from_millis(resolved.watch_debounce_ms);
    let (mut state, first_run) = initialize_watch_state(&resolved, &root).await?;
    let startup_message = if first_run {
        "[WATCH] initial synchronization"
    } else {
        "[WATCH] resumed synchronization"
    };
    match synchronize_watch(&resolved, &mut state, Some(startup_message.to_string())).await {
        Ok(outcome) => emit_sync_outcome(outcome),
        Err(error) => emit_watch_error(format!("Initial synchronization failed: {}", error), true),
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
        .map_err(|error| format!("Failed to watch '{}': {}", root.display(), error))?;

    let ctrl_c = tokio::signal::ctrl_c();
    tokio::pin!(ctrl_c);
    let mut poll_interval = tokio::time::interval(Duration::from_millis(resolved.watch_poll_ms));
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
                process_remote_sync(&resolved, &mut state).await;
                None
            }
            _ = &mut ctrl_c => {
                emit_watch_stop();
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
                    emit_watch_stop();
                    return Ok(());
                }
            }
        }

        if batch.is_empty() {
            continue;
        }

        process_batch(&resolved, &batch, &mut state).await;
    }
}

async fn initialize_watch_state(
    resolved: &ResolvedBackup,
    root: &Path,
) -> Result<(WatchState, bool), String> {
    let mut state = load_watch_state(root, &resolved.options.storage, &resolved.options.key)?;
    if state.initialized {
        return Ok((state, false));
    }

    state.initialized = true;
    state.base_backup = None;
    save_watch_state(
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
            emit_watch_error(error.to_string(), true);
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

async fn process_remote_sync(resolved: &ResolvedBackup, state: &mut WatchState) {
    match synchronize_watch(resolved, state, None).await {
        Ok(outcome) => emit_sync_outcome(outcome),
        Err(error) => emit_watch_error(format!("Synchronization failed: {}", error), true),
    }
}

async fn process_batch(resolved: &ResolvedBackup, batch: &ChangeBatch, state: &mut WatchState) {
    let message = build_watch_message(&resolved.message, batch);
    emit_watch_batch(batch, &message);

    match synchronize_watch(resolved, state, Some(message)).await {
        Ok(outcome) => emit_sync_outcome(outcome),
        Err(error) => emit_watch_error(format!("Backup failed: {}", error), true),
    }
}

#[derive(Default)]
struct SyncOutcome {
    backup: Option<BackupResult>,
    applied_remote: usize,
    merged_text: usize,
}

async fn synchronize_watch(
    resolved: &ResolvedBackup,
    state: &mut WatchState,
    message: Option<String>,
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
                if is_json_mode() {
                    emit_watch_conflicts(&reconciliation.conflicts);
                    return Ok(outcome);
                }
                resolve_interactive_conflicts(
                    &root,
                    reconciliation.conflicts,
                    Arc::clone(&resolved.options.fs),
                    &resolved.options.key,
                    resolved.options.password.as_deref(),
                )
                .await?;
            }
        }

        if worktree_matches_backup(&root, &resolved.options.ignore_patterns, &remote_tree)? {
            state.initialized = true;
            state.base_backup = remote_hash;
            save_watch_state(
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
                "[WATCH] synchronized remote changes".to_string()
            } else {
                "[WATCH] local changes".to_string()
            }
        });
        emit_watch_backup_started(&backup_message);

        let result =
            run_backup_with_parents(resolved.options.clone(), backup_message, parents, None)
                .await?;

        if result.head_published {
            state.initialized = true;
            state.base_backup = Some(result.backup.hash.clone());
            save_watch_state(
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
                "The repository changed while publishing the backup; please retry after the other watch finishes"
                    .to_string(),
            );
        }
    }

    Err("Failed to synchronize the watch after several concurrent changes".to_string())
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
) -> Result<(), String> {
    for conflict in conflicts {
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

        if selection == 1 {
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

fn emit_watch_batch(batch: &ChangeBatch, message: &str) {
    if is_json_mode() {
        emit_named_event(
            "watch",
            &WatchBatchPayload {
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
            "watch",
            &WatchBackupCompletedPayload {
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

fn emit_watch_backup_started(message: &str) {
    if is_json_mode() {
        emit_named_event(
            "watch",
            &WatchBackupStartedPayload {
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
                "watch",
                &WatchSyncPayload {
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

fn emit_watch_conflicts(conflicts: &[ReconcileConflict]) {
    if is_json_mode() {
        emit_named_event(
            "watch",
            &WatchConflictPayload {
                event: "conflict",
                conflicts: conflicts
                    .iter()
                    .map(|conflict| WatchConflictItem {
                        path: conflict.path.clone(),
                        reason: conflict.reason.clone(),
                    })
                    .collect(),
                recoverable: true,
            },
        );
    }
}

fn emit_watch_error(message: String, recoverable: bool) {
    if is_json_mode() {
        emit_named_event(
            "watch",
            &WatchErrorPayload {
                event: "error",
                message,
                recoverable,
            },
        );
    } else {
        eprintln!("{} {}", style("Watch error").red().bold(), message);
    }
}

fn emit_watch_stop() {
    if is_json_mode() {
        emit_named_event("watch", &WatchStopPayload { event: "stop" });
    } else {
        println!("{}", style("GIB watch stopped").cyan().bold());
    }
}

fn build_watch_message(prefix: &str, batch: &ChangeBatch) -> String {
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
        format!("[WATCH] {}", summary)
    } else {
        format!("[WATCH] {} — {}", prefix, summary)
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
        let path = std::env::temp_dir().join(format!("gib-watch-test-{suffix}"));
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
    fn watch_message_contains_actions_and_counts_only() {
        let mut batch = ChangeBatch::default();
        batch.record("created.txt".to_string(), ChangeKind::Created);
        batch.record("changed-a.txt".to_string(), ChangeKind::Changed);
        batch.record("changed-b.txt".to_string(), ChangeKind::Changed);
        batch.record("deleted.txt".to_string(), ChangeKind::Deleted);

        let message = build_watch_message("autosave", &batch);
        assert_eq!(
            message,
            "[WATCH] autosave — created: 1 file; changed: 2 files; deleted: 1 file"
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
            "[WATCH] initial".to_string(),
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
            watch_debounce_ms: 300,
            watch_poll_ms: 2_000,
        };
        let resolved_two = ResolvedBackup {
            options: options_two,
            message: String::new(),
            parent_hash: None,
            pending_backup: None,
            watch_debounce_ms: 300,
            watch_poll_ms: 2_000,
        };

        let mut state_one = WatchState {
            version: 1,
            initialized: true,
            base_backup: Some(initial.backup.hash.clone()),
        };
        let mut state_two = WatchState {
            version: 1,
            initialized: true,
            base_backup: Some(initial.backup.hash),
        };

        std::fs::write(source_one.join("shared.txt"), b"base\nfrom one\n").unwrap();
        let first_sync = synchronize_watch(
            &resolved_one,
            &mut state_one,
            Some("[WATCH] changed".to_string()),
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

        let second_sync = synchronize_watch(&resolved_two, &mut state_two, None)
            .await
            .unwrap();

        assert_eq!(second_sync.applied_remote, 1);
        assert_eq!(
            std::fs::read_to_string(source_two.join("shared.txt")).unwrap(),
            "base\nfrom one\n"
        );
        assert_eq!(
            state_two.base_backup.as_deref(),
            Some(published_hash.as_str())
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
            watch_debounce_ms: 300,
            watch_poll_ms: 2_000,
        };
        let (mut state_three, first_run) = initialize_watch_state(&resolved_three, &source_three)
            .await
            .unwrap();
        assert!(first_run);
        synchronize_watch(
            &resolved_three,
            &mut state_three,
            Some("[WATCH] initial synchronization".to_string()),
        )
        .await
        .unwrap();
        assert_eq!(
            std::fs::read_to_string(source_three.join("shared.txt")).unwrap(),
            "base\nfrom one\n"
        );

        let _ = std::fs::remove_dir_all(fixture);
    }
}
