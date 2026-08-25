use crate::commands::backup::{
    BackupMode, BackupResult, ResolvedBackup, is_ignored_path, latest_backup_hash, resolve_backup,
    run_backup,
};
use crate::output::{emit_named_event, is_json_mode};
use crate::utils::handle_error;
use clap::ArgMatches;
use console::style;
use notify::{Config, Event, EventKind, RecommendedWatcher, RecursiveMode, Watcher};
use serde::Serialize;
use std::collections::BTreeSet;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::mpsc::unbounded_channel;

const DEBOUNCE_WINDOW: Duration = Duration::from_millis(300);
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
    }

    if let Err(error) = watch_loop(resolved).await {
        handle_error(error, None);
    }
}

async fn watch_loop(resolved: ResolvedBackup) -> Result<(), String> {
    let root = PathBuf::from(&resolved.options.root_path_string);
    let ignore_patterns = resolved.options.ignore_patterns.clone();
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

    loop {
        let first_event = tokio::select! {
            event = receiver.recv() => event,
            _ = &mut ctrl_c => {
                emit_watch_stop();
                return Ok(());
            }
        };

        let Some(first_event) = first_event else {
            return Err("Filesystem watcher stopped unexpectedly".to_string());
        };

        let mut batch = ChangeBatch::default();
        record_notify_result(first_event, &root, &ignore_patterns, &mut batch);

        loop {
            let quiet_period = tokio::time::sleep(DEBOUNCE_WINDOW);
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

        process_batch(&resolved, &batch).await;
    }
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

async fn process_batch(resolved: &ResolvedBackup, batch: &ChangeBatch) {
    let message = build_watch_message(&resolved.message, batch);
    emit_watch_batch(batch, &message);

    let parent_hash = match latest_backup_hash(
        Arc::clone(&resolved.options.fs),
        resolved.options.key.clone(),
        resolved.options.password.clone(),
    )
    .await
    {
        Ok(parent_hash) => parent_hash,
        Err(error) => {
            emit_watch_error(
                format!("Failed to find the latest completed backup: {}", error),
                true,
            );
            return;
        }
    };

    if is_json_mode() {
        emit_named_event(
            "watch",
            &WatchBackupStartedPayload {
                event: "backup_start",
                message: message.clone(),
            },
        );
    } else {
        println!("{} {}", style("Batch").bold(), message);
    }

    match run_backup(resolved.options.clone(), message, parent_hash, None).await {
        Ok(result) => emit_backup_completed(result),
        Err(error) => emit_watch_error(format!("Backup failed: {}", error), true),
    }
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
}
