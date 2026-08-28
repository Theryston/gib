use super::error::ModelError;
use sha2::{Digest, Sha256};
use std::fs::{self, File, OpenOptions};
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

static TEMP_SEQUENCE: AtomicU64 = AtomicU64::new(0);

pub(crate) fn write_atomic(
    path: &Path,
    data: &[u8],
    unix_mode: Option<u32>,
) -> Result<(), ModelError> {
    let parent = path
        .parent()
        .ok_or_else(|| ModelError::UnsafePath(path.to_path_buf()))?;
    fs::create_dir_all(parent)
        .map_err(|error| ModelError::io("create parent directory", parent, error))?;

    let temporary = temporary_path(path);
    let result = (|| {
        let mut file = OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&temporary)
            .map_err(|error| ModelError::io("create temporary file", &temporary, error))?;
        if let Some(mode) = unix_mode {
            set_unix_mode(&temporary, mode)?;
        }
        file.write_all(data)
            .map_err(|error| ModelError::io("write temporary file", &temporary, error))?;
        file.sync_all()
            .map_err(|error| ModelError::io("sync temporary file", &temporary, error))?;
        publish_replacement(&temporary, path)?;
        sync_directory(parent)?;
        Ok(())
    })();

    if result.is_err() {
        let _ = fs::remove_file(&temporary);
    }
    result
}

pub(crate) fn hash_file(path: &Path) -> Result<(u64, String), ModelError> {
    let mut file = File::open(path).map_err(|error| ModelError::io("open file", path, error))?;
    let mut hasher = Sha256::new();
    let mut buffer = [0u8; 64 * 1024];
    let mut size = 0u64;

    loop {
        let read = file
            .read(&mut buffer)
            .map_err(|error| ModelError::io("read file", path, error))?;
        if read == 0 {
            break;
        }
        size = size.saturating_add(read as u64);
        hasher.update(&buffer[..read]);
    }

    Ok((size, format!("{:x}", hasher.finalize())))
}

pub(crate) fn now_unix_seconds() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
}

pub(crate) fn quarantine(path: &Path, reason: &str) -> Result<Option<PathBuf>, ModelError> {
    let metadata = match fs::symlink_metadata(path) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(ModelError::io("inspect file to quarantine", path, error)),
    };
    if metadata.file_type().is_symlink() || !metadata.is_file() {
        return Err(ModelError::UnsafePath(path.to_path_buf()));
    }
    let file_name = path
        .file_name()
        .and_then(|name| name.to_str())
        .ok_or_else(|| ModelError::UnsafePath(path.to_path_buf()))?;
    let suffix = format!(
        "{}-{}-{}",
        reason,
        std::process::id(),
        TEMP_SEQUENCE.fetch_add(1, Ordering::Relaxed)
    );
    let destination = path.with_file_name(format!(".{file_name}.{suffix}"));
    fs::rename(path, &destination)
        .map_err(|error| ModelError::io("quarantine file", path, error))?;
    Ok(Some(destination))
}

pub(crate) fn validate_sha256(value: &str) -> bool {
    value.len() == 64 && value.bytes().all(|byte| byte.is_ascii_hexdigit())
}

fn temporary_path(path: &Path) -> PathBuf {
    let file_name = path
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or("file");
    let stamp = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos();
    path.with_file_name(format!(
        ".{file_name}.tmp-{}-{}-{}",
        std::process::id(),
        stamp,
        TEMP_SEQUENCE.fetch_add(1, Ordering::Relaxed)
    ))
}

fn publish_replacement(temporary: &Path, destination: &Path) -> Result<(), ModelError> {
    #[cfg(windows)]
    if destination.exists() {
        fs::remove_file(destination)
            .map_err(|error| ModelError::io("replace file", destination, error))?;
    }

    fs::rename(temporary, destination)
        .map_err(|error| ModelError::io("publish file", destination, error))
}

fn sync_directory(path: &Path) -> Result<(), ModelError> {
    #[cfg(unix)]
    {
        File::open(path)
            .and_then(|file| file.sync_all())
            .map_err(|error| ModelError::io("sync directory", path, error))?;
    }
    #[cfg(not(unix))]
    {
        let _ = path;
    }
    Ok(())
}

fn set_unix_mode(path: &Path, mode: u32) -> Result<(), ModelError> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(path, fs::Permissions::from_mode(mode))
            .map_err(|error| ModelError::io("protect temporary file", path, error))?;
    }
    #[cfg(not(unix))]
    {
        let _ = (path, mode);
    }
    Ok(())
}
