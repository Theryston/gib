use chrono::{DateTime, SecondsFormat, Utc};
use gib::api::{
    AutostartJob, BackupResult, DeleteBackupResult, EncryptRepositoryResult, GibError, GibEvent,
    ListBackupsResponse, ListPendingBackupsResponse, PendingBackupInfo, PruneResult, RestoreResult,
    StorageChange, StorageInfo,
};
use serde::Serialize;
use serde_json::Value;
use std::io::Write;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};

const MAX_JSON_LOG_BYTES: u64 = 5 * 1024 * 1024;
const MAX_JSON_LOG_ROTATIONS: usize = 3;

#[derive(Serialize)]
struct JsonEnvelope<T> {
    #[serde(rename = "type")]
    kind: &'static str,
    data: T,
}

#[derive(Serialize)]
struct LegacyProgressData {
    percent: u64,
    total: u64,
    processed: u64,
    #[serde(skip_serializing_if = "Option::is_none")]
    message: Option<String>,
}

#[derive(Serialize)]
struct LegacyWarningData {
    message: String,
    code: String,
}

#[derive(Serialize)]
struct TextData {
    text: String,
}

#[derive(Serialize)]
struct LegacyBackupOutput<'a> {
    backup: &'a str,
    backup_short: String,
    message: &'a str,
    author: &'a str,
    timestamp_unix: u64,
    files_total: usize,
    written_bytes: u64,
    deduplicated_bytes: u64,
    elapsed_ms: u64,
    head_published: bool,
}

#[derive(Serialize)]
struct LegacyRestoreOutput {
    backup: String,
    backup_short: String,
    restored: u64,
    skipped: u64,
    deleted_local: u64,
    target_path: String,
    elapsed_ms: u64,
}

#[derive(Serialize)]
struct LegacyLogEntry {
    backup: String,
    backup_short: String,
    message: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    timestamp: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    timestamp_unix: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    size_bytes: Option<u64>,
}

#[derive(Serialize)]
struct LegacyEncryptOutput {
    encrypted: usize,
    already_encrypted: usize,
}

#[derive(Serialize)]
struct LegacyDeleteOutput {
    backup: String,
    backup_short: String,
    deleted_chunks: usize,
    elapsed_ms: u64,
}

#[derive(Serialize)]
struct LegacyPruneOutput {
    deleted_items: usize,
    elapsed_ms: u64,
}

#[derive(Serialize)]
struct LegacyPendingEntry {
    backup: String,
    backup_short: String,
    message: String,
    uploaded_chunks: usize,
    chunk_size_bytes: u64,
    compress: i32,
    concurrency: usize,
    ignored_entries: usize,
}

pub(super) fn json_envelope<T: Serialize>(kind: &'static str, data: T) -> String {
    serde_json::to_string(&JsonEnvelope { kind, data }).unwrap_or_else(|_| {
        "{\"type\":\"error\",\"data\":{\"message\":\"Failed to serialize CLI output\",\"code\":\"serialization_error\"}}".to_string()
    })
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum OutputMode {
    Interactive,
    Json,
}

impl OutputMode {
    pub(crate) fn from_args(args: &[String]) -> Self {
        let mut values = args.iter().skip(1);
        while let Some(value) = values.next() {
            if value == "--mode" {
                return match values.next().map(|value| value.as_str()) {
                    Some("interactive") => Self::Interactive,
                    _ => Self::Json,
                };
            }
            if let Some(value) = value.strip_prefix("--mode=") {
                return if value.eq_ignore_ascii_case("interactive") {
                    Self::Interactive
                } else {
                    Self::Json
                };
            }
        }
        Self::Interactive
    }
}

#[derive(Clone)]
pub(crate) struct CliOutput {
    mode: OutputMode,
    stdout: Arc<Mutex<()>>,
    stderr: Arc<Mutex<()>>,
    json_log: Arc<Mutex<Option<PathBuf>>>,
}

impl CliOutput {
    pub(crate) fn new(mode: OutputMode) -> Self {
        Self {
            mode,
            stdout: Arc::new(Mutex::new(())),
            stderr: Arc::new(Mutex::new(())),
            json_log: Arc::new(Mutex::new(None)),
        }
    }

    pub(crate) fn is_json(&self) -> bool {
        self.mode == OutputMode::Json
    }

    pub(crate) fn set_json_log(&self, path: PathBuf) -> Result<(), GibError> {
        let parent = path.parent().ok_or_else(|| {
            GibError::new(
                gib::api::ErrorCode::Io,
                "JSON log path has no parent directory",
            )
        })?;
        std::fs::create_dir_all(parent).map_err(|error| {
            GibError::new(
                gib::api::ErrorCode::Io,
                format!("Failed to create JSON log directory: {error}"),
            )
        })?;
        *self.json_log.lock().map_err(|_| {
            GibError::new(gib::api::ErrorCode::Internal, "JSON log lock is poisoned")
        })? = Some(path);
        Ok(())
    }

    pub(crate) fn event(&self, event: GibEvent) {
        if self.is_json() {
            self.event_json(event);
            return;
        }
        match event {
            GibEvent::Progress(progress) => {
                if let Some(message) = progress.message {
                    self.line_stderr(&message);
                }
            }
            GibEvent::Warning(warning) => {
                self.line_stderr(&format!("Warning: {}", warning.message));
            }
            GibEvent::Backup(backup) if backup.event == "completed" => {
                if let Some(hash) = backup.backup {
                    self.line_stdout(&format!("Backup completed: {hash}"));
                }
            }
            GibEvent::Restore(restore) if restore.event == "completed" => {
                self.line_stdout(restore.path.as_deref().unwrap_or("Restore completed"));
            }
            GibEvent::Live(live) if live.event == "error" => {
                self.line_stderr(live.message.as_deref().unwrap_or("Live operation failed"));
            }
            _ => {}
        }
    }

    fn event_json(&self, event: GibEvent) {
        match event {
            // Operation markers are useful to embedding applications, but
            // were not part of the historical CLI JSONL stream.
            GibEvent::OperationStarted(_) => {}
            GibEvent::Progress(progress) => {
                let percent = progress.percentage.map(u64::from).unwrap_or_else(|| {
                    progress
                        .total
                        .filter(|total| *total > 0)
                        .map_or(0, |total| progress.processed.saturating_mul(100) / total)
                });
                let data = LegacyProgressData {
                    percent,
                    total: progress.total.unwrap_or(0),
                    processed: progress.processed,
                    message: progress.message,
                };
                self.json_stdout(&json_envelope("progress", data));
            }
            GibEvent::Warning(warning) => {
                self.json_stderr(&json_envelope(
                    "warning",
                    LegacyWarningData {
                        message: warning.message,
                        code: warning.code,
                    },
                ));
            }
            GibEvent::Live(mut live) => {
                live.event = match live.event.as_str() {
                    "started" => "start".to_string(),
                    "stopped" => "stop".to_string(),
                    "backup_completed" => "backup_complete".to_string(),
                    _ => live.event,
                };
                let mut data = serde_json::to_value(live).unwrap_or_else(|_| serde_json::json!({}));
                if let Some(object) = data.as_object_mut() {
                    object.remove("paths");
                    object.remove("applied_remote");
                    object.remove("merged_text");
                }
                self.json_stdout(&json_envelope("live", data));
            }
            // Registry actions are rendered from their returned typed result
            // by the CLI controller. Suppressing this internal event here
            // keeps the historical one-record-per-action protocol while the
            // event remains available to library callbacks.
            GibEvent::Autostart(_) => {}
            // The legacy CLI reports these operations through their final
            // output envelope. Keep the typed events available to library
            // callbacks without adding duplicate CLI records.
            GibEvent::Backup(_) | GibEvent::Restore(_) => {}
            _ => {}
        }
    }

    pub(crate) fn result<T: Serialize>(&self, value: &T) {
        if self.is_json() {
            self.json_stdout(&json_envelope("output", value));
        } else {
            match serde_json::to_string_pretty(value) {
                Ok(value) => self.line_stdout(&value),
                Err(error) => self.line_stderr(&format!("Failed to render output: {error}")),
            }
        }
    }

    pub(crate) fn pending_result(&self, result: &ListPendingBackupsResponse) {
        let entries = result
            .pending
            .iter()
            .map(legacy_pending_entry)
            .collect::<Vec<_>>();
        if self.is_json() {
            self.json_stdout(&json_envelope("output", entries));
        } else if entries.is_empty() {
            self.line_stdout("No pending backups found for this repository.");
        } else {
            for entry in entries {
                self.line_stdout(&format!(
                    "{} {} ({} chunks, {} chunk size, compression {}, concurrency {}, {} ignored)",
                    entry.backup_short,
                    entry.message,
                    entry.uploaded_chunks,
                    bytesize::ByteSize(entry.chunk_size_bytes),
                    entry.compress,
                    entry.concurrency,
                    entry.ignored_entries,
                ));
            }
        }
    }

    pub(crate) fn storage_added(&self, change: &StorageChange, info: Option<&StorageInfo>) {
        if self.is_json() {
            if let Some(info) = info {
                self.json_stdout(&json_envelope("output", info));
            } else {
                self.json_stdout(&json_envelope("output", change));
            }
        } else {
            let state = if change.replaced {
                "updated"
            } else {
                "written"
            };
            self.line_stdout(&format!("OK Storage '{}' {state}", change.name));
        }
    }

    pub(crate) fn autostart_list(&self, summaries: &[Value], status: bool) {
        if self.is_json() {
            self.json_stdout(&json_envelope(
                "autostart",
                serde_json::json!({
                    "event": if status { "status" } else { "listed" },
                    "jobs": summaries,
                }),
            ));
        } else if summaries.is_empty() {
            self.line_stdout("No autostart jobs configured.");
        } else {
            for summary in summaries {
                self.line_stdout(&autostart_summary_line(summary));
            }
        }
    }

    pub(crate) fn autostart_changed(
        &self,
        event: &str,
        job: &AutostartJob,
        start_now: bool,
        platform: &str,
    ) {
        if self.is_json() {
            self.json_stdout(&json_envelope(
                "autostart",
                serde_json::json!({
                    "event": event,
                    "id": job.id,
                    "name": job.name,
                    "root_path": job.root_path,
                    "enabled": job.enabled,
                    "start_now": start_now,
                    "platform": platform,
                }),
            ));
        } else {
            let state = if job.enabled && start_now {
                "enabled and started"
            } else if job.enabled {
                "enabled"
            } else {
                "disabled"
            };
            self.line_stdout(&format!("OK Autostart job '{}' is {state}.", job.name));
        }
    }

    pub(crate) fn autostart_removed(&self, job: &AutostartJob) {
        if self.is_json() {
            self.json_stdout(&json_envelope(
                "autostart",
                serde_json::json!({
                    "event": "removed",
                    "id": job.id,
                    "name": job.name,
                    "root_path": job.root_path,
                }),
            ));
        } else {
            self.line_stdout(&format!("OK Removed autostart job '{}'.", job.name));
        }
    }

    pub(crate) fn backup_result(&self, result: &BackupResult) {
        let output = LegacyBackupOutput {
            backup: &result.backup.hash,
            backup_short: short_hash(&result.backup.hash),
            message: &result.backup.message,
            author: &result.backup.author,
            timestamp_unix: result.backup.timestamp_unix,
            files_total: result.files_total,
            written_bytes: result.written_bytes,
            deduplicated_bytes: result.deduplicated_bytes,
            elapsed_ms: result.elapsed_ms,
            head_published: result.head_published,
        };
        if self.is_json() {
            self.json_stdout(&json_envelope("output", output));
        } else {
            self.line_stdout(&format!(
                "OK Backed up files ({:?}) - {} written, {} deduplicated",
                std::time::Duration::from_millis(result.elapsed_ms),
                bytesize::ByteSize(result.written_bytes),
                bytesize::ByteSize(result.deduplicated_bytes),
            ));
        }
    }

    pub(crate) fn restore_result(&self, result: &RestoreResult, elapsed_ms: u64) {
        let output = LegacyRestoreOutput {
            backup: result.backup.clone(),
            backup_short: short_hash(&result.backup),
            restored: result.restored,
            skipped: result.skipped,
            deleted_local: result.pruned_local.len() as u64,
            target_path: result.target_path.to_string_lossy().into_owned(),
            elapsed_ms,
        };
        if self.is_json() {
            self.json_stdout(&json_envelope("output", output));
        } else {
            let deleted = result.pruned_local.len();
            let suffix = if deleted == 0 {
                format!(
                    "Restored {} files, skipped {} files",
                    result.restored, result.skipped
                )
            } else {
                format!(
                    "Restored {} files, skipped {} files, deleted {} files",
                    result.restored, result.skipped, deleted
                )
            };
            self.line_stdout(&format!(
                "OK {suffix} ({:?})",
                std::time::Duration::from_millis(elapsed_ms)
            ));
        }
    }

    pub(crate) fn log_result(&self, result: &ListBackupsResponse) {
        let entries = result
            .backups
            .iter()
            .map(|backup| LegacyLogEntry {
                backup: backup.hash.clone(),
                backup_short: short_hash(&backup.hash),
                message: backup.message.clone(),
                timestamp: backup.timestamp_unix.and_then(|timestamp| {
                    DateTime::<Utc>::from_timestamp_secs(timestamp as i64)
                        .map(|value| value.to_rfc3339_opts(SecondsFormat::Secs, true))
                }),
                timestamp_unix: backup.timestamp_unix,
                size_bytes: backup.size_bytes,
            })
            .collect::<Vec<_>>();
        if self.is_json() {
            self.json_stdout(&json_envelope("output", entries));
        } else if entries.is_empty() {
            self.line_stdout("No backups found for this repository.");
        } else {
            for entry in entries {
                self.line_stdout(&format!("{} {}", entry.backup_short, entry.message));
            }
        }
    }

    pub(crate) fn encrypt_result(&self, result: &EncryptRepositoryResult) {
        let output = LegacyEncryptOutput {
            encrypted: result.encrypted_items,
            already_encrypted: result.skipped_items,
        };
        if self.is_json() {
            self.json_stdout(&json_envelope("output", output));
        } else {
            self.line_stdout(&format!(
                "OK Encrypted {} chunks ({} were already encrypted)",
                result.encrypted_items, result.skipped_items
            ));
        }
    }

    pub(crate) fn delete_result(&self, result: &DeleteBackupResult, elapsed_ms: u64) {
        let output = LegacyDeleteOutput {
            backup: result.backup.clone(),
            backup_short: short_hash(&result.backup),
            deleted_chunks: result.deleted_chunks,
            elapsed_ms,
        };
        if self.is_json() {
            self.json_stdout(&json_envelope("output", output));
        } else {
            self.line_stdout(&format!(
                "OK Deleted backup {} and {} chunks ({:?})",
                short_hash(&result.backup),
                result.deleted_chunks,
                std::time::Duration::from_millis(elapsed_ms)
            ));
        }
    }

    pub(crate) fn prune_result(&self, result: &PruneResult, elapsed_ms: u64) {
        if self.is_json() {
            self.json_stdout(&json_envelope(
                "output",
                LegacyPruneOutput {
                    deleted_items: result.deleted_items,
                    elapsed_ms,
                },
            ));
        } else {
            self.line_stdout(&format!(
                "OK Deleted {} items ({:?})",
                result.deleted_items,
                std::time::Duration::from_millis(elapsed_ms)
            ));
        }
    }

    pub(crate) fn message(&self, value: &str) {
        self.line_stdout(value);
    }

    pub(crate) fn autostart_log_following(&self, job: &AutostartJob, path: &PathBuf) {
        if self.is_json() {
            self.json_stdout(&json_envelope(
                "autostart",
                serde_json::json!({
                    "event": "log_following",
                    "id": job.id,
                    "name": job.name,
                    "log_path": path,
                }),
            ));
        } else {
            self.line_stdout(&format!(
                "Following autostart logs for '{}' (press Ctrl+C to stop): {}",
                job.name,
                path.display()
            ));
        }
    }

    pub(crate) fn autostart_log_entry(&self, job: &AutostartJob, path: &PathBuf, line: &str) {
        if self.is_json() {
            let entry = serde_json::from_str::<Value>(line)
                .unwrap_or_else(|_| Value::String(line.to_string()));
            self.json_stdout(&json_envelope(
                "autostart",
                serde_json::json!({
                    "event": "log_entry",
                    "id": job.id,
                    "name": job.name,
                    "log_path": path,
                    "entry": entry,
                }),
            ));
        } else {
            self.line_stdout(line);
        }
    }

    pub(crate) fn autostart_log_stopped(&self, job: &AutostartJob, path: &PathBuf) {
        if self.is_json() {
            self.json_stdout(&json_envelope(
                "autostart",
                serde_json::json!({
                    "event": "log_following_stopped",
                    "id": job.id,
                    "name": job.name,
                    "log_path": path,
                    "reason": "interrupted",
                }),
            ));
        }
    }

    pub(crate) fn error(&self, error: &GibError) {
        self.error_with_code(error.message(), &format_code(error.code()));
    }

    pub(crate) fn error_with_code(&self, message: &str, code: &str) {
        if self.is_json() {
            self.json_stderr(&json_envelope(
                "error",
                LegacyWarningData {
                    message: message.to_string(),
                    code: code.to_string(),
                },
            ));
        } else {
            self.line_stderr(&format!("Error: {message}"));
        }
    }

    pub(crate) fn help(&self, value: String) {
        if self.is_json() {
            self.json_stdout(&json_envelope("help", TextData { text: value }));
        } else {
            self.line_stdout(&value);
        }
    }

    pub(crate) fn version(&self, value: String) {
        if self.is_json() {
            self.json_stdout(&json_envelope("version", TextData { text: value }));
        } else {
            self.line_stdout(&value);
        }
    }

    fn line_stdout(&self, value: &str) {
        if let Ok(_guard) = self.stdout.lock() {
            let mut stdout = std::io::stdout().lock();
            let _ = writeln!(stdout, "{value}");
            let _ = stdout.flush();
        }
    }

    fn line_stderr(&self, value: &str) {
        if let Ok(_guard) = self.stderr.lock() {
            let mut stderr = std::io::stderr().lock();
            let _ = writeln!(stderr, "{value}");
            let _ = stderr.flush();
        }
    }

    fn json_stdout(&self, value: &str) {
        self.append_json_log(value);
        self.line_stdout(value);
    }

    pub(super) fn json_stderr(&self, value: &str) {
        self.append_json_log(value);
        self.line_stderr(value);
    }

    fn append_json_log(&self, value: &str) {
        let Ok(guard) = self.json_log.lock() else {
            return;
        };
        let Some(path) = guard.as_deref() else {
            return;
        };
        rotate_json_log_if_needed(path, value.len() as u64 + 1);
        let Ok(mut file) = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(path)
        else {
            return;
        };
        let _ = writeln!(file, "{value}");
    }
}

fn format_code(code: gib::api::ErrorCode) -> String {
    serde_json::to_value(code)
        .ok()
        .and_then(|value| value.as_str().map(str::to_owned))
        .unwrap_or_else(|| "internal".to_string())
}

fn rotate_json_log_if_needed(path: &std::path::Path, incoming_bytes: u64) {
    let current_size = std::fs::metadata(path)
        .map(|metadata| metadata.len())
        .unwrap_or_default();
    if current_size.saturating_add(incoming_bytes) <= MAX_JSON_LOG_BYTES {
        return;
    }
    for index in (1..=MAX_JSON_LOG_ROTATIONS).rev() {
        let destination = rotated_json_log_path(path, index);
        let source = if index == 1 {
            path.to_path_buf()
        } else {
            rotated_json_log_path(path, index - 1)
        };
        if destination.exists() {
            let _ = std::fs::remove_file(&destination);
        }
        if source.exists() {
            let _ = std::fs::rename(source, destination);
        }
    }
}

fn rotated_json_log_path(path: &std::path::Path, index: usize) -> PathBuf {
    let file_name = path
        .file_name()
        .map(|name| name.to_string_lossy().to_string())
        .unwrap_or_else(|| "gib.jsonl".to_string());
    path.with_file_name(format!("{file_name}.{index}"))
}

fn short_hash(hash: &str) -> String {
    hash[..hash.len().min(8)].to_string()
}

fn legacy_pending_entry(entry: &PendingBackupInfo) -> LegacyPendingEntry {
    LegacyPendingEntry {
        backup: entry.backup.clone(),
        backup_short: short_hash(&entry.backup),
        message: entry.message.clone(),
        uploaded_chunks: entry.uploaded_chunks,
        chunk_size_bytes: entry.chunk_size_bytes,
        compress: entry.compression,
        concurrency: entry.concurrency,
        ignored_entries: entry.ignored_entries,
    }
}

fn autostart_summary_line(summary: &Value) -> String {
    let name = summary.get("name").and_then(Value::as_str).unwrap_or("?");
    let id = summary.get("id").and_then(Value::as_str).unwrap_or("?");
    let root = summary
        .get("root_path")
        .and_then(Value::as_str)
        .unwrap_or("?");
    let enabled = summary
        .get("enabled")
        .and_then(Value::as_bool)
        .unwrap_or(false);
    let running = summary
        .get("running")
        .and_then(Value::as_bool)
        .map_or("unknown", |value| if value { "running" } else { "stopped" });
    let state = if enabled { "enabled" } else { "disabled" };
    format!("{name} ({id}) — {state} — {running} — {root}")
}
