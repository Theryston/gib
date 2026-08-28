use std::fmt::Display;
use std::fs::Metadata;
use std::io::SeekFrom;
use std::path::{Path, PathBuf};

use console::style;
use indicatif::{ProgressBar, ProgressStyle};
use serde_json::Value;
use tokio::fs::{self, File};
use tokio::io::{AsyncReadExt, AsyncSeekExt};

const MAX_LOG_ROTATIONS: usize = 3;
const MAX_DISPLAY_ITEMS: usize = 8;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct FileIdentity {
    #[cfg(unix)]
    device: u64,
    #[cfg(unix)]
    inode: u64,
    #[cfg(windows)]
    creation_time: u64,
}

struct DrainResult {
    lines: Vec<String>,
    next_offset: u64,
    pending: Vec<u8>,
    reset: bool,
}

/// Follows the active JSONL log without keeping the log contents in memory.
///
/// The follower only retains the byte offset and a possible incomplete final
/// line. When the active file is rotated, it reads the old file's unread tail
/// from the rotated files before resuming at byte zero of the new active file.
pub(crate) struct LogFollower {
    path: PathBuf,
    offset: u64,
    identity: Option<FileIdentity>,
    pending: Vec<u8>,
}

pub(crate) struct InteractiveLogRenderer {
    progress: Option<ProgressBar>,
    progress_total: Option<u64>,
}

impl InteractiveLogRenderer {
    pub(crate) fn new() -> Self {
        Self {
            progress: None,
            progress_total: None,
        }
    }

    pub(crate) fn render_line(&mut self, line: &str) {
        let value = match serde_json::from_str::<Value>(line) {
            Ok(value) => value,
            Err(_) => {
                println!("{} {}", style("Log entry").yellow().bold(), line);
                return;
            }
        };

        let Some(kind) = value.get("type").and_then(Value::as_str) else {
            println!("{}", style("Log event without a type").yellow());
            return;
        };
        let data = value.get("data").unwrap_or(&Value::Null);

        if kind != "progress" {
            self.clear_progress();
        }

        match kind {
            "autostart" => self.render_autostart(data),
            "config" => self.render_config(data),
            "live" => self.render_live(data),
            "progress" => self.render_progress(data),
            "warning" => render_message("Warning", data, style("Warning").yellow().bold()),
            "error" => render_message("Error", data, style("Error").red().bold()),
            "output" => self.render_output(data),
            _ => self.render_unknown(kind, data),
        }
    }

    pub(crate) fn render_initial_line(&mut self, line: &str) {
        if is_progress_line(line) {
            return;
        }
        self.render_line(line);
    }

    fn render_autostart(&mut self, data: &Value) {
        let event = string_field(data, "event").unwrap_or("event");
        match event {
            "started" => {
                let name = string_field(data, "name").unwrap_or("unknown");
                println!(
                    "{} Autostart job '{}' started.",
                    style("OK").green().bold(),
                    name
                );
                if let Some(root) = string_field(data, "root_path") {
                    println!("  {} {}", style("Root").bold(), root);
                }
            }
            "stopped" => {
                let name = string_field(data, "name").unwrap_or("unknown");
                println!(
                    "{} Autostart job '{}' stopped.",
                    style("OK").cyan().bold(),
                    name
                );
            }
            "failed" => {
                let name = string_field(data, "name").unwrap_or("unknown");
                let message = string_field(data, "message").unwrap_or("unknown error");
                println!(
                    "{} Autostart job '{}' failed: {}",
                    style("Error").red().bold(),
                    name,
                    message
                );
            }
            "configuration_error" => {
                render_message(
                    "Autostart configuration error",
                    data,
                    style("Error").red().bold(),
                );
            }
            "secret_unavailable" => {
                render_message(
                    "Autostart secret unavailable",
                    data,
                    style("Error").red().bold(),
                );
            }
            _ => self.render_unknown("autostart", data),
        }
    }

    fn render_config(&mut self, data: &Value) {
        let loaded = data.get("loaded").and_then(Value::as_bool).unwrap_or(false);
        if loaded {
            if let Some(path) = string_field(data, "path") {
                println!("{} {}", style("Loaded local config").cyan().bold(), path);
            } else {
                println!("{}", style("Loaded local config").cyan().bold());
            }
        } else {
            println!("{}", style("No local config loaded").dim());
        }
    }

    fn render_live(&mut self, data: &Value) {
        let event = string_field(data, "event").unwrap_or("event");
        match event {
            "start" => {
                println!("{}", style("GIB live started").cyan().bold());
                if let Some(root) = string_field(data, "root") {
                    println!("{} {}", style("Root").bold(), root);
                }
                if let (Some(storage), Some(key)) =
                    (string_field(data, "storage"), string_field(data, "key"))
                {
                    println!("{} {} / {}", style("Target").bold(), storage, key);
                }
                if let Some(ignore) = string_array(data, "ignore")
                    && !ignore.is_empty()
                {
                    println!(
                        "{} {} patterns: {}",
                        style("Ignoring").bold(),
                        ignore.len(),
                        format_limited_items(&ignore)
                    );
                }
                println!(
                    "{}",
                    style("Waiting for changes... Press Ctrl+C to stop.").dim()
                );
                if let Some(poll_ms) = data.get("poll_ms").and_then(Value::as_u64) {
                    println!(
                        "{} {}",
                        style("Remote sync interval").bold(),
                        format_duration(poll_ms)
                    );
                }
            }
            "change_batch" => self.render_change_batch(data),
            // The change_batch event already announces what will be backed
            // up. The progress events that follow provide the activity
            // indicator, so a separate "backup started" line is redundant.
            "backup_start" => {}
            "backup_complete" => {
                self.render_backup_complete(data);
            }
            "synchronized" => {
                let applied_remote = number_field(data, "applied_remote");
                let merged_text = number_field(data, "merged_text");
                if applied_remote > 0 || merged_text > 0 {
                    println!(
                        "{} {} remote changes, {} text merges",
                        style("Synchronized").green().bold(),
                        applied_remote,
                        merged_text
                    );
                }
            }
            "conflict" => self.render_conflicts(data),
            "error" => render_message("Live error", data, style("Live error").red().bold()),
            "stop" => {
                println!("{}", style("GIB live stopped").cyan().bold());
            }
            _ => self.render_unknown("live", data),
        }
    }

    fn render_change_batch(&mut self, data: &Value) {
        println!(
            "{} {} created, {} changed, {} deleted",
            style("Changes").bold(),
            group_count(data, "created"),
            group_count(data, "changed"),
            group_count(data, "deleted")
        );
    }

    fn render_backup_complete(&self, data: &Value) {
        let backup = string_field(data, "backup_short")
            .or_else(|| string_field(data, "backup"))
            .unwrap_or("unknown");
        let message = string_field(data, "message").unwrap_or("");
        if message.is_empty() {
            println!("{} {}", style("Backup created").green().bold(), backup);
        } else {
            println!(
                "{} {} ({})",
                style("Backup created").green().bold(),
                backup,
                message
            );
        }
    }

    fn render_conflicts(&mut self, data: &Value) {
        let conflicts = data
            .get("conflicts")
            .and_then(Value::as_array)
            .map_or(0, Vec::len);
        let resolution = string_field(data, "resolution").unwrap_or("manual");
        println!(
            "{} {} {} — resolution: {}",
            style("Conflict").red().bold(),
            conflicts,
            if conflicts == 1 { "file" } else { "files" },
            resolution
        );
        if let Some(items) = data.get("conflicts").and_then(Value::as_array) {
            for item in items.iter().take(MAX_DISPLAY_ITEMS) {
                let path = string_field(item, "path").unwrap_or("unknown path");
                let reason = string_field(item, "reason").unwrap_or("unknown reason");
                println!("  {}: {}", path, reason);
            }
            let remaining = items.len().saturating_sub(MAX_DISPLAY_ITEMS);
            if remaining > 0 {
                println!("  {} more conflicts", remaining);
            }
        }
    }

    fn render_progress(&mut self, data: &Value) {
        let total = number_field(data, "total");
        let processed = number_field(data, "processed");
        let message = string_field(data, "message").unwrap_or("");

        if self.progress_total != Some(total) {
            self.clear_progress();
            let progress = if total == 0 {
                let progress = ProgressBar::new_spinner();
                progress.enable_steady_tick(std::time::Duration::from_millis(100));
                progress.set_style(
                    ProgressStyle::with_template("{spinner:.green} {msg}")
                        .expect("valid spinner progress template"),
                );
                progress
            } else {
                let progress = ProgressBar::new(total);
                progress.set_style(
                    ProgressStyle::with_template(
                        "[{elapsed_precise}] [{bar:40.cyan/blue}] {pos}/{len} {msg}",
                    )
                    .expect("valid progress bar template"),
                );
                progress
            };
            self.progress = Some(progress);
            self.progress_total = Some(total);
        }

        if let Some(progress) = &self.progress {
            if total > 0 {
                progress.set_position(processed.min(total));
            }
            progress.set_message(message.to_string());
        }
    }

    fn render_output(&mut self, data: &Value) {
        // Live emits a friendly backup_complete event after this internal
        // output event. Render only the former to avoid showing the same
        // backup twice in the interactive log viewer.
        if data.get("backup_short").is_some() {
            return;
        }

        if let Some(message) = string_field(data, "message") {
            println!("{} {}", style("Output").dim(), message);
        } else if let Some(items) = data.as_array() {
            println!("{} {} entries", style("Output").dim(), items.len());
        } else {
            println!("{}", style("Output completed").dim());
        }
    }

    pub(crate) fn clear_progress(&mut self) {
        if let Some(progress) = self.progress.take() {
            progress.finish_and_clear();
        }
        self.progress_total = None;
    }

    fn render_unknown(&self, kind: &str, data: &Value) {
        let event = string_field(data, "event");
        let message = string_field(data, "message");
        match (event, message) {
            (Some(event), Some(message)) => {
                println!("{} {}: {}", style(kind).dim(), event, message);
            }
            (Some(event), None) => println!("{} {}", style(kind).dim(), event),
            (None, Some(message)) => println!("{} {}", style(kind).dim(), message),
            (None, None) => println!("{} event", style(kind).dim()),
        }
    }
}

impl Drop for InteractiveLogRenderer {
    fn drop(&mut self) {
        self.clear_progress();
    }
}

fn render_message<T: Display>(label: &str, data: &Value, styled_label: T) {
    let message = string_field(data, "message").unwrap_or(label);
    println!("{} {}", styled_label, message);
}

fn is_progress_line(line: &str) -> bool {
    serde_json::from_str::<Value>(line)
        .ok()
        .and_then(|value| {
            value
                .get("type")
                .and_then(Value::as_str)
                .map(|kind| kind == "progress")
        })
        .unwrap_or(false)
}

fn string_field<'a>(value: &'a Value, field: &str) -> Option<&'a str> {
    value.get(field).and_then(Value::as_str)
}

fn string_array(value: &Value, field: &str) -> Option<Vec<String>> {
    value.get(field)?.as_array().map(|items| {
        items
            .iter()
            .filter_map(Value::as_str)
            .map(ToString::to_string)
            .collect()
    })
}

fn number_field(value: &Value, field: &str) -> u64 {
    value.get(field).and_then(Value::as_u64).unwrap_or(0)
}

fn group_count(value: &Value, field: &str) -> u64 {
    value
        .get(field)
        .map(|group| number_field(group, "count"))
        .unwrap_or(0)
}

fn format_limited_items(items: &[String]) -> String {
    let selected = items
        .iter()
        .take(MAX_DISPLAY_ITEMS)
        .cloned()
        .collect::<Vec<_>>();
    let remaining = items.len().saturating_sub(selected.len());

    if remaining == 0 {
        selected.join(", ")
    } else {
        format!("{} (+{} more)", selected.join(", "), remaining)
    }
}

fn format_duration(milliseconds: u64) -> String {
    if milliseconds % 1_000 == 0 {
        format!("{}s", milliseconds / 1_000)
    } else {
        format!("{}ms", milliseconds)
    }
}

impl LogFollower {
    pub(crate) fn new(path: PathBuf) -> Self {
        Self {
            path,
            offset: 0,
            identity: None,
            pending: Vec::new(),
        }
    }

    /// Returns complete lines that have been appended since the previous call.
    pub(crate) async fn read_available(&mut self) -> Result<Vec<String>, String> {
        let Some(metadata) = metadata_if_exists(&self.path).await? else {
            let lines = if self.offset > 0 || self.identity.is_some() {
                self.read_rotated_tail().await?
            } else {
                Vec::new()
            };
            self.reset();
            return Ok(lines);
        };

        let current_identity = file_identity(&metadata);
        let replaced = self
            .identity
            .zip(current_identity)
            .is_some_and(|(previous, current)| previous != current)
            || metadata.len() < self.offset;

        let mut lines = Vec::new();
        if replaced {
            lines.extend(self.read_rotated_tail().await?);
            self.reset();
        }

        let pending = std::mem::take(&mut self.pending);
        let start_offset = self.offset;
        let Some(result) = drain_file(&self.path, start_offset, pending).await? else {
            self.reset();
            return Ok(lines);
        };

        if result.reset {
            self.pending = result.pending;
            lines.extend(self.read_rotated_tail().await?);
            self.reset();
            return Ok(lines);
        }

        self.offset = result.next_offset;
        self.pending = result.pending;
        lines.extend(result.lines);

        self.identity = metadata_if_exists(&self.path)
            .await?
            .and_then(|value| file_identity(&value));
        Ok(lines)
    }

    fn reset(&mut self) {
        self.offset = 0;
        self.identity = None;
        self.pending.clear();
    }

    async fn read_rotated_tail(&mut self) -> Result<Vec<String>, String> {
        let previous_offset = self.offset;
        let previous_identity = self.identity;
        let mut previous_pending = std::mem::take(&mut self.pending);
        let mut lines = Vec::new();

        let matching_rotation = if let Some(previous_identity) = previous_identity {
            let mut matching_rotation = None;
            for index in 1..=MAX_LOG_ROTATIONS {
                let path = rotated_path(&self.path, index);
                let Some(metadata) = metadata_if_exists(&path).await? else {
                    continue;
                };
                if file_identity(&metadata) == Some(previous_identity) {
                    matching_rotation = Some(index);
                    break;
                }
            }
            matching_rotation
        } else {
            None
        };

        if let Some(old_index) = matching_rotation {
            // Rotation shifts .1 to .2, .2 to .3, and so on. Reading in
            // reverse index order preserves the original log order.
            for index in (1..=old_index).rev() {
                let path = rotated_path(&self.path, index);
                let offset = if index == old_index {
                    previous_offset
                } else {
                    0
                };
                let pending = if index == old_index {
                    std::mem::take(&mut previous_pending)
                } else {
                    Vec::new()
                };
                append_rotated_file(&mut lines, &path, offset, pending).await?;
            }
        } else {
            // On filesystems where a stable file identity is unavailable,
            // falling back to .1 still handles the normal single rotation.
            append_rotated_file(
                &mut lines,
                &rotated_path(&self.path, 1),
                previous_offset,
                std::mem::take(&mut previous_pending),
            )
            .await?;
        }

        if !previous_pending.is_empty() {
            lines.push(decode_line(&previous_pending));
        }
        Ok(lines)
    }
}

async fn append_rotated_file(
    lines: &mut Vec<String>,
    path: &Path,
    offset: u64,
    pending: Vec<u8>,
) -> Result<(), String> {
    let Some(result) = drain_file(path, offset, pending).await? else {
        return Ok(());
    };
    lines.extend(result.lines);
    if !result.pending.is_empty() {
        lines.push(decode_line(&result.pending));
    }
    Ok(())
}

async fn drain_file(
    path: &Path,
    offset: u64,
    mut pending: Vec<u8>,
) -> Result<Option<DrainResult>, String> {
    let mut file = match File::open(path).await {
        Ok(file) => file,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) => {
            return Err(format!(
                "Failed to read autostart log '{}': {}",
                path.display(),
                error
            ));
        }
    };

    let metadata = file.metadata().await.map_err(|error| {
        format!(
            "Failed to inspect autostart log '{}': {}",
            path.display(),
            error
        )
    })?;
    if metadata.len() < offset {
        return Ok(Some(DrainResult {
            lines: Vec::new(),
            next_offset: 0,
            pending,
            reset: true,
        }));
    }

    file.seek(SeekFrom::Start(offset)).await.map_err(|error| {
        format!(
            "Failed to seek autostart log '{}': {}",
            path.display(),
            error
        )
    })?;
    let mut bytes = Vec::new();
    file.read_to_end(&mut bytes).await.map_err(|error| {
        format!(
            "Failed to read autostart log '{}': {}",
            path.display(),
            error
        )
    })?;
    let next_offset = offset.saturating_add(bytes.len() as u64);
    pending.extend_from_slice(&bytes);
    let lines = extract_complete_lines(&mut pending);

    Ok(Some(DrainResult {
        lines,
        next_offset,
        pending,
        reset: false,
    }))
}

async fn metadata_if_exists(path: &Path) -> Result<Option<Metadata>, String> {
    match fs::metadata(path).await {
        Ok(metadata) => Ok(Some(metadata)),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(error) => Err(format!(
            "Failed to inspect autostart log '{}': {}",
            path.display(),
            error
        )),
    }
}

fn file_identity(metadata: &Metadata) -> Option<FileIdentity> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;

        Some(FileIdentity {
            device: metadata.dev(),
            inode: metadata.ino(),
        })
    }

    #[cfg(windows)]
    {
        use std::os::windows::fs::MetadataExt;

        Some(FileIdentity {
            creation_time: metadata.creation_time(),
        })
    }

    #[cfg(not(any(unix, windows)))]
    {
        let _ = metadata;
        None
    }
}

fn rotated_path(path: &Path, index: usize) -> PathBuf {
    let file_name = path
        .file_name()
        .map(|name| name.to_string_lossy().to_string())
        .unwrap_or_else(|| "gib.jsonl".to_string());
    path.with_file_name(format!("{}.{}", file_name, index))
}

fn extract_complete_lines(buffer: &mut Vec<u8>) -> Vec<String> {
    let mut lines = Vec::new();
    let mut line_start = 0;

    for index in 0..buffer.len() {
        if buffer[index] != b'\n' {
            continue;
        }
        lines.push(decode_line(&buffer[line_start..index]));
        line_start = index + 1;
    }

    if line_start > 0 {
        buffer.drain(..line_start);
    }
    lines
}

fn decode_line(bytes: &[u8]) -> String {
    let bytes = if bytes.last() == Some(&b'\r') {
        &bytes[..bytes.len() - 1]
    } else {
        bytes
    };
    String::from_utf8_lossy(bytes).into_owned()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::{SystemTime, UNIX_EPOCH};
    use tokio::io::AsyncWriteExt;

    #[test]
    fn extracts_complete_lines_and_keeps_partial_lines() {
        let mut buffer = b"first\nsecond".to_vec();

        assert_eq!(extract_complete_lines(&mut buffer), vec!["first"]);
        assert_eq!(buffer, b"second");
    }

    #[tokio::test]
    async fn follows_the_active_file_after_rotation() {
        let root = std::env::temp_dir().join(format!(
            "gib-autostart-log-test-{}-{}",
            std::process::id(),
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        fs::create_dir_all(&root).await.unwrap();
        let path = root.join("job.jsonl");
        fs::write(&path, b"first\n").await.unwrap();

        let mut follower = LogFollower::new(path.clone());
        assert_eq!(follower.read_available().await.unwrap(), vec!["first"]);

        let mut file = fs::OpenOptions::new()
            .append(true)
            .open(&path)
            .await
            .unwrap();
        file.write_all(b"second\n").await.unwrap();
        file.flush().await.unwrap();
        drop(file);
        assert_eq!(follower.read_available().await.unwrap(), vec!["second"]);

        let rotated = rotated_path(&path, 1);
        fs::rename(&path, &rotated).await.unwrap();
        fs::write(&path, b"third\n").await.unwrap();
        assert_eq!(follower.read_available().await.unwrap(), vec!["third"]);

        let mut file = fs::OpenOptions::new()
            .append(true)
            .open(&path)
            .await
            .unwrap();
        file.write_all(b"fourth\n").await.unwrap();
        file.flush().await.unwrap();
        drop(file);
        assert_eq!(follower.read_available().await.unwrap(), vec!["fourth"]);

        fs::remove_dir_all(root).await.unwrap();
    }
}
