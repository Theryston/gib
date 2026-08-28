use super::error::ModelError;
use std::fs::{File, OpenOptions};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

const STALE_LOCK_AFTER: Duration = Duration::from_secs(15 * 60);
static LOCK_SEQUENCE: AtomicU64 = AtomicU64::new(0);

pub(crate) struct FileLock {
    path: PathBuf,
    token: String,
}

impl FileLock {
    pub(crate) async fn acquire(path: &Path, timeout: Duration) -> Result<Self, ModelError> {
        let parent = path
            .parent()
            .ok_or_else(|| ModelError::UnsafePath(path.to_path_buf()))?;
        std::fs::create_dir_all(parent)
            .map_err(|error| ModelError::io("create lock directory", parent, error))?;

        let token = format!(
            "{}-{}-{}",
            std::process::id(),
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap_or_default()
                .as_nanos(),
            LOCK_SEQUENCE.fetch_add(1, Ordering::Relaxed)
        );
        let started = Instant::now();

        loop {
            match OpenOptions::new().write(true).create_new(true).open(path) {
                Ok(mut file) => {
                    if let Err(error) = protect_lock(&file, path) {
                        let _ = std::fs::remove_file(path);
                        return Err(error);
                    }
                    if let Err(error) = file.write_all(token.as_bytes()) {
                        let _ = std::fs::remove_file(path);
                        return Err(ModelError::io("write lock", path, error));
                    }
                    if let Err(error) = file.sync_all() {
                        let _ = std::fs::remove_file(path);
                        return Err(ModelError::io("sync lock", path, error));
                    }
                    return Ok(Self {
                        path: path.to_path_buf(),
                        token,
                    });
                }
                Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {
                    if is_stale(path) {
                        let _ = std::fs::remove_file(path);
                        continue;
                    }
                    if started.elapsed() >= timeout {
                        return Err(ModelError::LockTimeout(path.to_path_buf()));
                    }
                    tokio::time::sleep(Duration::from_millis(25)).await;
                }
                Err(error) => return Err(ModelError::io("create lock", path, error)),
            }
        }
    }

    pub(crate) fn refresh(&self) -> Result<(), ModelError> {
        let metadata = std::fs::symlink_metadata(&self.path)
            .map_err(|error| ModelError::io("inspect lock", &self.path, error))?;
        if metadata.file_type().is_symlink() || !metadata.is_file() {
            return Err(ModelError::LockLost(self.path.clone()));
        }
        let contents = std::fs::read_to_string(&self.path)
            .map_err(|error| ModelError::io("read lock", &self.path, error))?;
        if contents != self.token {
            return Err(ModelError::LockLost(self.path.clone()));
        }
        let mut file = OpenOptions::new()
            .write(true)
            .open(&self.path)
            .map_err(|error| ModelError::io("refresh lock", &self.path, error))?;
        file.set_len(0)
            .map_err(|error| ModelError::io("refresh lock", &self.path, error))?;
        file.write_all(self.token.as_bytes())
            .map_err(|error| ModelError::io("refresh lock", &self.path, error))?;
        file.sync_all()
            .map_err(|error| ModelError::io("sync lock", &self.path, error))
    }
}

impl Drop for FileLock {
    fn drop(&mut self) {
        let owns_lock = std::fs::read_to_string(&self.path)
            .map(|contents| contents == self.token)
            .unwrap_or(false);
        if owns_lock {
            let _ = std::fs::remove_file(&self.path);
        }
    }
}

fn is_stale(path: &Path) -> bool {
    std::fs::metadata(path)
        .and_then(|metadata| metadata.modified())
        .ok()
        .and_then(|modified| SystemTime::now().duration_since(modified).ok())
        .is_some_and(|age| age > STALE_LOCK_AFTER)
}

fn protect_lock(file: &File, path: &Path) -> Result<(), ModelError> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        file.set_permissions(std::fs::Permissions::from_mode(0o600))
            .map_err(|error| ModelError::io("protect lock", path, error))?;
    }
    #[cfg(not(unix))]
    {
        let _ = (file, path);
    }
    Ok(())
}
