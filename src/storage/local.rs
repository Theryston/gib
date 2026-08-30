use super::FS;
use super::content_version;
use async_trait::async_trait;
use std::fs::OpenOptions;
use std::io::ErrorKind;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant, SystemTime};
use walkdir::WalkDir;

pub struct LocalFS {
    path: std::path::PathBuf,
}

static TEMPORARY_FILE_COUNTER: AtomicU64 = AtomicU64::new(0);

impl LocalFS {
    pub fn new(path: impl Into<std::path::PathBuf>) -> Self {
        Self { path: path.into() }
    }

    fn acquire_cas_lock(&self) -> Result<CasLock, std::io::Error> {
        std::fs::create_dir_all(&self.path)?;
        let lock_path = self.path.join(".gib-cas.lock");
        let started = Instant::now();

        loop {
            match OpenOptions::new()
                .write(true)
                .create_new(true)
                .open(&lock_path)
            {
                Ok(file) => {
                    drop(file);
                    return Ok(CasLock { path: lock_path });
                }
                Err(error) if error.kind() == ErrorKind::AlreadyExists => {
                    let stale = std::fs::metadata(&lock_path)
                        .and_then(|metadata| metadata.modified())
                        .ok()
                        .and_then(|modified| SystemTime::now().duration_since(modified).ok())
                        .is_some_and(|age| age > Duration::from_secs(30));

                    if stale {
                        let _ = std::fs::remove_file(&lock_path);
                        continue;
                    }

                    if started.elapsed() > Duration::from_secs(10) {
                        return Err(std::io::Error::new(
                            ErrorKind::TimedOut,
                            "timed out waiting for the repository compare-and-swap lock",
                        ));
                    }
                    std::thread::sleep(Duration::from_millis(10));
                }
                Err(error) => return Err(error),
            }
        }
    }
}

struct CasLock {
    path: PathBuf,
}

impl Drop for CasLock {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.path);
    }
}

#[async_trait]
impl FS for LocalFS {
    async fn read_file(&self, path: &str) -> Result<Vec<u8>, std::io::Error> {
        std::fs::read(&self.path.join(path))
    }

    async fn write_file(&self, path: &str, data: &[u8]) -> Result<(), std::io::Error> {
        let path = self.path.join(path);
        let parent_dir = path.parent().ok_or_else(|| {
            std::io::Error::new(ErrorKind::InvalidInput, "storage path has no parent")
        })?;

        if !parent_dir.exists() {
            std::fs::create_dir_all(parent_dir)?;
        }

        std::fs::write(path, data)
    }

    async fn list_files(&self, path: &str) -> Result<Vec<String>, std::io::Error> {
        let mut files = Vec::new();

        let full_path = self.path.join(path);

        if !full_path.exists() {
            return Ok(files);
        }

        for entry in WalkDir::new(full_path) {
            let entry = entry?;
            if entry.file_type().is_file() {
                let path_str = entry
                    .path()
                    .strip_prefix(&self.path)
                    .map_err(|_| {
                        std::io::Error::new(
                            ErrorKind::InvalidData,
                            "walked storage path is outside the storage root",
                        )
                    })?
                    .to_string_lossy()
                    .replace('\\', "/");
                files.push(path_str);
            }
        }

        Ok(files)
    }

    async fn delete_file(&self, path: &str) -> Result<(), std::io::Error> {
        let full_path = self.path.join(path);

        std::fs::remove_file(&full_path)?;

        if let Some(folder) = full_path.parent() {
            if let Ok(mut it) = folder.read_dir() {
                if it.next().is_none() {
                    let _ = std::fs::remove_dir(folder);
                }
            }
        }

        Ok(())
    }

    async fn read_file_with_version(
        &self,
        path: &str,
    ) -> Result<(Vec<u8>, String), std::io::Error> {
        let data = std::fs::read(self.path.join(path))?;
        let version = content_version(&data);
        Ok((data, version))
    }

    async fn write_file_if_version(
        &self,
        path: &str,
        data: &[u8],
        expected_version: Option<&str>,
    ) -> Result<(), std::io::Error> {
        let _lock = self.acquire_cas_lock()?;
        let target = self.path.join(path);
        let current = std::fs::read(&target);

        match (expected_version, current) {
            (Some(expected), Ok(current)) if expected == content_version(&current) => {}
            (None, Err(error)) if error.kind() == ErrorKind::NotFound => {}
            (Some(_), Err(error)) if error.kind() == ErrorKind::NotFound => {
                return Err(std::io::Error::new(
                    ErrorKind::AlreadyExists,
                    "conditional write failed because the object was removed",
                ));
            }
            (None, Ok(_)) => {
                return Err(std::io::Error::new(
                    ErrorKind::AlreadyExists,
                    "conditional write failed because the object already exists",
                ));
            }
            (_, Err(error)) => return Err(error),
            (Some(_), Ok(_)) => {
                return Err(std::io::Error::new(
                    ErrorKind::AlreadyExists,
                    "conditional write failed because the object changed",
                ));
            }
        }

        let parent_dir = target.parent().ok_or_else(|| {
            std::io::Error::new(ErrorKind::InvalidInput, "storage path has no parent")
        })?;
        std::fs::create_dir_all(parent_dir)?;
        write_atomically(&target, data)
    }
}

fn write_atomically(path: &std::path::Path, data: &[u8]) -> Result<(), std::io::Error> {
    let stamp = SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|duration| duration.as_nanos())
        .unwrap_or_default();
    let file_name = path
        .file_name()
        .map(|name| name.to_string_lossy())
        .unwrap_or_else(|| std::borrow::Cow::Borrowed("file"));
    let temporary = path.with_file_name(format!(
        ".{}.gib-cas-{}-{}",
        file_name,
        stamp,
        TEMPORARY_FILE_COUNTER.fetch_add(1, Ordering::Relaxed)
    ));

    std::fs::write(&temporary, data)?;
    match std::fs::rename(&temporary, path) {
        Ok(()) => Ok(()),
        Err(error) if error.kind() == ErrorKind::AlreadyExists => {
            std::fs::remove_file(path)?;
            std::fs::rename(temporary, path)
        }
        Err(error) => {
            let _ = std::fs::remove_file(temporary);
            Err(error)
        }
    }
}
