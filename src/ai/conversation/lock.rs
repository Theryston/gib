use super::error::ConversationError;
use std::fs::{self, File, OpenOptions};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::thread;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

const STALE_LOCK_AFTER: Duration = Duration::from_secs(15 * 60);
static LOCK_SEQUENCE: AtomicU64 = AtomicU64::new(0);

/// A small cross-process ownership lock for one conversation or the global
/// AI configuration.
///
/// Lock acquisition is synchronous by design. Conversation service methods run
/// it inside tokio::task::spawn_blocking, so polling never blocks an async
/// executor. A crashed process leaves its lock file behind; after the
/// documented fifteen-minute lease, another process may reclaim it. The
/// owner token prevents a delayed process from deleting a newer owner's lock.
pub(crate) struct ConversationLock {
    path: PathBuf,
    token: String,
}

impl ConversationLock {
    pub(crate) fn acquire(
        path: &Path,
        scope: &str,
        timeout: Duration,
    ) -> Result<Self, ConversationError> {
        let parent = path.parent().ok_or(ConversationError::UnsafePath)?;
        fs::create_dir_all(parent).map_err(|_| ConversationError::io("create lock directory"))?;

        let token = owner_token();
        let started = Instant::now();
        loop {
            match OpenOptions::new().write(true).create_new(true).open(path) {
                Ok(mut file) => {
                    protect_lock(&file)?;
                    file.write_all(token.as_bytes())
                        .map_err(|_| ConversationError::io("write lock"))?;
                    file.sync_all()
                        .map_err(|_| ConversationError::io("sync lock"))?;
                    return Ok(Self {
                        path: path.to_path_buf(),
                        token,
                    });
                }
                Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {
                    match fs::symlink_metadata(path) {
                        Ok(metadata) if metadata.file_type().is_symlink() => {
                            return Err(ConversationError::UnsafePath);
                        }
                        Ok(metadata) if !metadata.is_file() => {
                            return Err(ConversationError::UnsafePath);
                        }
                        Err(not_found) if not_found.kind() == std::io::ErrorKind::NotFound => {
                            continue;
                        }
                        Err(_) => return Err(ConversationError::io("inspect lock")),
                        Ok(_) => {}
                    }

                    if is_stale(path) {
                        let _ = fs::remove_file(path);
                        continue;
                    }
                    if started.elapsed() >= timeout {
                        return Err(ConversationError::LockTimeout {
                            scope: scope.to_string(),
                        });
                    }
                    thread::sleep(Duration::from_millis(25));
                }
                Err(_) => return Err(ConversationError::io("create lock")),
            }
        }
    }

    #[cfg(test)]
    pub(crate) fn path(&self) -> &Path {
        &self.path
    }
}

impl Drop for ConversationLock {
    fn drop(&mut self) {
        let owns_lock = fs::read_to_string(&self.path)
            .map(|contents| contents == self.token)
            .unwrap_or(false);
        if owns_lock {
            let _ = fs::remove_file(&self.path);
        }
    }
}

fn owner_token() -> String {
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default();
    format!(
        "pid={}\ncreated_at={}\nnonce={}\n",
        std::process::id(),
        now.as_secs(),
        LOCK_SEQUENCE.fetch_add(1, Ordering::Relaxed)
    )
}

fn is_stale(path: &Path) -> bool {
    if let Ok(contents) = fs::read_to_string(path)
        && let Some(created_at) = contents.lines().find_map(|line| {
            line.strip_prefix("created_at=")
                .and_then(|value| value.parse::<u64>().ok())
        })
    {
        return SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|now| now.as_secs().saturating_sub(created_at) > STALE_LOCK_AFTER.as_secs())
            .unwrap_or(false);
    }

    fs::metadata(path)
        .and_then(|metadata| metadata.modified())
        .ok()
        .and_then(|modified| SystemTime::now().duration_since(modified).ok())
        .is_some_and(|age| age > STALE_LOCK_AFTER)
}

fn protect_lock(file: &File) -> Result<(), ConversationError> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        file.set_permissions(fs::Permissions::from_mode(0o600))
            .map_err(|_| ConversationError::io("protect lock"))?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    fn temporary_path(name: &str) -> PathBuf {
        let path = std::env::temp_dir().join(format!(
            "gib-conversation-lock-{}-{}",
            name,
            LOCK_SEQUENCE.fetch_add(1, Ordering::Relaxed)
        ));
        fs::create_dir_all(path.parent().expect("temporary path has parent"))
            .expect("temporary parent should exist");
        path
    }

    #[test]
    fn lock_has_ownership_and_is_removed_on_drop() {
        let path = temporary_path("ownership");
        {
            let lock = ConversationLock::acquire(&path, "test", Duration::ZERO)
                .expect("lock should be acquired");
            assert!(lock.path().is_file());
            assert!(matches!(
                ConversationLock::acquire(&path, "test", Duration::ZERO),
                Err(ConversationError::LockTimeout { .. })
            ));
        }
        assert!(!path.exists());
    }

    #[test]
    fn stale_lock_is_reclaimed_from_its_lease_timestamp() {
        let path = temporary_path("stale");
        fs::write(&path, "pid=1\ncreated_at=0\nnonce=old\n").expect("stale lock should be written");
        let _lock = ConversationLock::acquire(&path, "test", Duration::ZERO)
            .expect("stale lock should be reclaimed");
    }
}
