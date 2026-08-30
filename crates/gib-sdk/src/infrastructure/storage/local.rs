use crate::application::ports::{
    RepositoryStorage, StorageError, StorageResult, StorageVersion, VersionedObject,
};
use crate::domain::RepositoryObject;
use sha2::{Digest, Sha256};
use std::collections::BTreeMap;
use std::fmt;
use std::fs::{self, OpenOptions};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, OnceLock, Weak};

use std::fs::File;

static NEXT_TEMP_OBJECT_ID: AtomicU64 = AtomicU64::new(1);
static LOCAL_STORAGE_LOCKS: OnceLock<Mutex<BTreeMap<PathBuf, Weak<Mutex<()>>>>> = OnceLock::new();

/// A filesystem-backed repository storage rooted at one configured directory.
///
/// Logical object keys are validated before they are joined to the root. New
/// objects use `create_new`, are flushed before publication, and are never
/// overwritten by the lifecycle operations.
#[derive(Clone)]
pub struct LocalStorage {
    state: Arc<LocalStorageState>,
}

struct LocalStorageState {
    root: PathBuf,
    lock_root: PathBuf,
    cas_lock: Arc<Mutex<()>>,
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
        let lock_root = fs::canonicalize(&root).map_err(|_| StorageError::Io)?;
        let cas_lock = lock_for_root(&lock_root)?;
        Ok(Self {
            state: Arc::new(LocalStorageState {
                root,
                lock_root,
                cas_lock,
            }),
        })
    }

    /// Alias for [`Self::new`] for callers that prefer an explicit backend name.
    pub fn open(path: impl AsRef<Path>) -> StorageResult<Self> {
        Self::new(path)
    }

    /// Returns the configured root for manual inspection and diagnostics.
    pub fn root_path(&self) -> &Path {
        &self.state.root
    }
}

impl fmt::Debug for LocalStorage {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("LocalStorage")
            .field("root", &self.state.root)
            .finish()
    }
}

impl PartialEq for LocalStorage {
    fn eq(&self, other: &Self) -> bool {
        self.state.root == other.state.root
    }
}

impl Eq for LocalStorage {}

impl RepositoryStorage for LocalStorage {
    fn create_if_absent(&self, object_key: &str, contents: &[u8]) -> StorageResult<()> {
        let _guard = self
            .state
            .cas_lock
            .lock()
            .map_err(|_| StorageError::Unavailable)?;
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

    fn read_with_version(&self, object_key: &str) -> StorageResult<VersionedObject> {
        let contents = self.read(object_key)?;
        let version = version_for_contents(&contents)?;
        Ok(VersionedObject::new(contents, version))
    }

    fn compare_and_swap(
        &self,
        object_key: &str,
        expected: Option<&StorageVersion>,
        contents: &[u8],
    ) -> StorageResult<StorageVersion> {
        let _guard = self
            .state
            .cas_lock
            .lock()
            .map_err(|_| StorageError::Unavailable)?;
        let _file_lock = acquire_cas_file_lock(&self.state.lock_root)?;
        let path = self.object_path(object_key, true)?;
        let current = match fs::symlink_metadata(&path) {
            Ok(metadata) => {
                if metadata.file_type().is_symlink() || !metadata.is_file() {
                    return Err(StorageError::InvalidObjectKey);
                }
                Some(fs::read(&path).map_err(map_io_error)?)
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => None,
            Err(error) => return Err(map_io_error(error)),
        };
        let current_version = current.as_deref().map(version_for_contents).transpose()?;
        if current_version.as_ref() != expected {
            return Err(StorageError::ConditionNotMet);
        }

        let temporary_path = write_temporary_file(&path, contents)?;
        let write_result = if expected.is_none() {
            fs::hard_link(&temporary_path, &path)
        } else {
            replace_file(&temporary_path, &path)
        };
        if let Err(error) = write_result {
            let _ = fs::remove_file(&temporary_path);
            if error.kind() == std::io::ErrorKind::AlreadyExists {
                return Err(StorageError::ConditionNotMet);
            }
            return Err(StorageError::Io);
        }
        if expected.is_none() && fs::remove_file(&temporary_path).is_err() {
            return Err(StorageError::Io);
        }
        sync_parent(path.parent())?;
        version_for_contents(contents)
    }
}

impl LocalStorage {
    fn object_path(&self, object_key: &str, create_parents: bool) -> StorageResult<PathBuf> {
        let object =
            RepositoryObject::new(object_key).map_err(|_| StorageError::InvalidObjectKey)?;
        ensure_directory_is_safe(&self.state.root)?;

        let components = object.as_str().split('/').collect::<Vec<_>>();
        let (parents, filename) = components.split_at(components.len().saturating_sub(1));
        let mut parent = self.state.root.clone();
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

fn lock_for_root(root: &Path) -> StorageResult<Arc<Mutex<()>>> {
    let locks = LOCAL_STORAGE_LOCKS.get_or_init(|| Mutex::new(BTreeMap::new()));
    let mut locks = locks.lock().map_err(|_| StorageError::Unavailable)?;
    locks.retain(|_, lock| lock.strong_count() != 0);
    if let Some(lock) = locks.get(root).and_then(Weak::upgrade) {
        return Ok(lock);
    }
    let lock = Arc::new(Mutex::new(()));
    locks.insert(root.to_path_buf(), Arc::downgrade(&lock));
    Ok(lock)
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

fn write_temporary_file(path: &Path, contents: &[u8]) -> StorageResult<PathBuf> {
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
    let mut file = OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&temporary_path)
        .map_err(map_io_error)?;
    if file.write_all(contents).is_err() || file.sync_all().is_err() {
        drop(file);
        let _ = fs::remove_file(&temporary_path);
        return Err(StorageError::Io);
    }
    drop(file);
    Ok(temporary_path)
}

fn version_for_contents(contents: &[u8]) -> StorageResult<StorageVersion> {
    let digest = Sha256::digest(contents);
    StorageVersion::from_bytes(digest.to_vec())
}

struct CasFileLock {
    _file: File,
}

fn acquire_cas_file_lock(root: &Path) -> StorageResult<CasFileLock> {
    let path = root.join(".gib-head-cas.lock");
    let file = OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .open(path)
        .map_err(map_io_error)?;
    lock_file(&file).map_err(|_| StorageError::Io)?;
    Ok(CasFileLock { _file: file })
}

#[cfg(unix)]
fn lock_file(file: &File) -> std::io::Result<()> {
    use std::os::unix::io::AsRawFd;

    // SAFETY: the file descriptor is borrowed from a live File and the lock
    // operation does not retain the pointer or mutate memory owned by Rust.
    let result = unsafe { flock(file.as_raw_fd(), LOCK_EX) };
    if result == -1 {
        Err(std::io::Error::last_os_error())
    } else {
        Ok(())
    }
}

#[cfg(unix)]
const LOCK_EX: std::os::raw::c_int = 2;

#[cfg(unix)]
#[link(name = "c")]
unsafe extern "C" {
    fn flock(
        file_descriptor: std::os::raw::c_int,
        operation: std::os::raw::c_int,
    ) -> std::os::raw::c_int;
}

#[cfg(windows)]
fn lock_file(file: &File) -> std::io::Result<()> {
    use std::os::windows::io::AsRawHandle;

    let mut overlapped = Overlapped::default();
    // SAFETY: the handle is borrowed from a live File and overlapped remains
    // valid for the duration of this synchronous LockFileEx call.
    let result = unsafe {
        LockFileEx(
            file.as_raw_handle(),
            LOCKFILE_EXCLUSIVE_LOCK,
            0,
            u32::MAX,
            u32::MAX,
            &mut overlapped,
        )
    };
    if result == 0 {
        Err(std::io::Error::last_os_error())
    } else {
        Ok(())
    }
}

#[cfg(windows)]
#[derive(Default)]
#[repr(C)]
struct Overlapped {
    internal: usize,
    internal_high: usize,
    offset: u32,
    offset_high: u32,
    event: *mut std::ffi::c_void,
}

#[cfg(windows)]
const LOCKFILE_EXCLUSIVE_LOCK: u32 = 0x0000_0002;

#[cfg(windows)]
#[link(name = "kernel32")]
unsafe extern "system" {
    fn LockFileEx(
        file: *mut std::ffi::c_void,
        flags: u32,
        reserved: u32,
        number_of_bytes_to_lock_low: u32,
        number_of_bytes_to_lock_high: u32,
        overlapped: *mut Overlapped,
    ) -> i32;
}

#[cfg(not(any(unix, windows)))]
fn lock_file(_file: &File) -> std::io::Result<()> {
    Ok(())
}

#[cfg(not(windows))]
fn replace_file(from: &Path, to: &Path) -> std::io::Result<()> {
    fs::rename(from, to)
}

#[cfg(windows)]
fn replace_file(from: &Path, to: &Path) -> std::io::Result<()> {
    use std::os::windows::ffi::OsStrExt;

    let from = from
        .as_os_str()
        .encode_wide()
        .chain(std::iter::once(0))
        .collect::<Vec<_>>();
    let to = to
        .as_os_str()
        .encode_wide()
        .chain(std::iter::once(0))
        .collect::<Vec<_>>();
    // SAFETY: both vectors are NUL-terminated UTF-16 paths that remain alive
    // for the duration of the call. MoveFileExW does not retain the pointers.
    let result = unsafe {
        MoveFileExW(
            from.as_ptr(),
            to.as_ptr(),
            MOVEFILE_REPLACE_EXISTING | MOVEFILE_WRITE_THROUGH,
        )
    };
    if result == 0 {
        Err(std::io::Error::last_os_error())
    } else {
        Ok(())
    }
}

#[cfg(windows)]
const MOVEFILE_REPLACE_EXISTING: u32 = 0x0000_0001;

#[cfg(windows)]
const MOVEFILE_WRITE_THROUGH: u32 = 0x0000_0008;

#[cfg(windows)]
#[link(name = "kernel32")]
unsafe extern "system" {
    fn MoveFileExW(existing_file_name: *const u16, new_file_name: *const u16, flags: u32) -> i32;
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
