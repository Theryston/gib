use std::fs::Metadata;
use std::io::SeekFrom;
use std::path::{Path, PathBuf};

use tokio::fs::{self, File};
use tokio::io::{AsyncReadExt, AsyncSeekExt};

const MAX_LOG_ROTATIONS: usize = 3;

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
