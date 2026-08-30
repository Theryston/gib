use crate::application::ports::{RepositoryStorage, StorageError, StorageResult};
use crate::domain::RepositoryObject;
use std::fs::{self, OpenOptions};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

#[cfg(unix)]
use std::fs::File;

static NEXT_TEMP_OBJECT_ID: AtomicU64 = AtomicU64::new(1);

/// A filesystem-backed repository storage rooted at one configured directory.
///
/// Logical object keys are validated before they are joined to the root. New
/// objects use `create_new`, are flushed before publication, and are never
/// overwritten by the lifecycle operations.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct LocalStorage {
    root: PathBuf,
}

impl LocalStorage {
    /// Creates a local storage rooted at `path`, creating the empty root when
    /// necessary.
    pub fn new(path: impl AsRef<Path>) -> StorageResult<Self> {
        let root = path.as_ref().to_path_buf();
        if root.as_os_str().is_empty() {
            return Err(StorageError::InvalidObjectKey);
        }
        fs::create_dir_all(&root).map_err(|_| StorageError::Io)?;
        ensure_directory_is_safe(&root)?;
        Ok(Self { root })
    }

    /// Alias for [`Self::new`] for callers that prefer an explicit backend name.
    pub fn open(path: impl AsRef<Path>) -> StorageResult<Self> {
        Self::new(path)
    }

    /// Returns the configured root for manual inspection and diagnostics.
    pub fn root_path(&self) -> &Path {
        &self.root
    }
}

impl RepositoryStorage for LocalStorage {
    fn create_if_absent(&self, object_key: &str, contents: &[u8]) -> StorageResult<()> {
        let path = self.object_path(object_key, true)?;
        let filename = path
            .file_name()
            .and_then(|value| value.to_str())
            .ok_or(StorageError::InvalidObjectKey)?;
        let temporary_name = format!(
            ".{filename}.gib-tmp-{}-{}",
            std::process::id(),
            NEXT_TEMP_OBJECT_ID.fetch_add(1, Ordering::Relaxed)
        );
        let temporary_path = path.with_file_name(temporary_name);
        let mut file = match OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&temporary_path)
        {
            Ok(file) => file,
            Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {
                return Err(StorageError::AlreadyExists);
            }
            Err(_) => return Err(StorageError::Io),
        };

        if file.write_all(contents).is_err() || file.sync_all().is_err() {
            drop(file);
            let _ = fs::remove_file(&temporary_path);
            return Err(StorageError::Io);
        }
        drop(file);

        let link_result = fs::hard_link(&temporary_path, &path);
        let remove_result = fs::remove_file(&temporary_path);
        if let Err(error) = link_result {
            let _ = remove_result;
            if error.kind() == std::io::ErrorKind::AlreadyExists {
                return Err(StorageError::AlreadyExists);
            }
            return Err(StorageError::Io);
        }
        if remove_result.is_err() {
            return Err(StorageError::Io);
        }
        sync_parent(path.parent())
    }

    fn read(&self, object_key: &str) -> StorageResult<Vec<u8>> {
        let path = self.object_path(object_key, false)?;
        let metadata = fs::symlink_metadata(&path).map_err(map_io_error)?;
        if metadata.file_type().is_symlink() || !metadata.is_file() {
            return Err(StorageError::InvalidObjectKey);
        }
        fs::read(path).map_err(map_io_error)
    }
}

impl LocalStorage {
    fn object_path(&self, object_key: &str, create_parents: bool) -> StorageResult<PathBuf> {
        let object =
            RepositoryObject::new(object_key).map_err(|_| StorageError::InvalidObjectKey)?;
        ensure_directory_is_safe(&self.root)?;

        let components = object.as_str().split('/').collect::<Vec<_>>();
        let (parents, filename) = components.split_at(components.len().saturating_sub(1));
        let mut parent = self.root.clone();
        for component in parents {
            parent.push(component);
            match fs::symlink_metadata(&parent) {
                Ok(metadata) => {
                    if metadata.file_type().is_symlink() || !metadata.is_dir() {
                        return Err(StorageError::InvalidObjectKey);
                    }
                }
                Err(error) if error.kind() == std::io::ErrorKind::NotFound && create_parents => {
                    fs::create_dir(&parent).map_err(map_io_error)?;
                    ensure_directory_is_safe(&parent)?;
                    sync_parent(parent.parent())?;
                }
                Err(error) => return Err(map_io_error(error)),
            }
        }

        let Some(filename) = filename.first() else {
            return Err(StorageError::InvalidObjectKey);
        };
        let mut path = parent;
        path.push(filename);
        if !create_parents {
            let parent = path.parent().ok_or(StorageError::InvalidObjectKey)?;
            ensure_directory_is_safe(parent)?;
        }
        Ok(path)
    }
}

fn ensure_directory_is_safe(path: &Path) -> StorageResult<()> {
    let metadata = fs::symlink_metadata(path).map_err(map_io_error)?;
    if metadata.file_type().is_symlink() || !metadata.is_dir() {
        return Err(StorageError::InvalidObjectKey);
    }
    Ok(())
}

fn map_io_error(error: std::io::Error) -> StorageError {
    match error.kind() {
        std::io::ErrorKind::NotFound => StorageError::NotFound,
        std::io::ErrorKind::AlreadyExists => StorageError::AlreadyExists,
        _ => StorageError::Io,
    }
}

fn sync_parent(parent: Option<&Path>) -> StorageResult<()> {
    let Some(parent) = parent else {
        return Err(StorageError::InvalidObjectKey);
    };
    #[cfg(unix)]
    {
        File::open(parent)
            .and_then(|directory| directory.sync_all())
            .map_err(|_| StorageError::Io)?;
    }
    #[cfg(not(unix))]
    {
        let _ = parent;
    }
    Ok(())
}
