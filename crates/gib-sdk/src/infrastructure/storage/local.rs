use crate::application::ports::{
    ObjectCursor, ObjectKey, ObjectListPage, ObjectListRequest, ObjectMetadata, ObjectRange,
    ObjectRead, ObjectWriteOptions, RepositoryStorage, STORAGE_TRANSFER_BUFFER_SIZE,
    StorageCapabilities, StorageError, StorageResult, StorageVersion, StorageWriteCondition,
    VersionedObject, copy_stream, read_stream_to_vec,
};
use crate::domain::RepositoryObject;
use sha2::{Digest, Sha256};
use std::collections::BTreeMap;
use std::fmt;
use std::fs::{self, OpenOptions};
use std::io::{Cursor, Read, Seek, SeekFrom};
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
        fs::create_dir_all(&root).map_err(map_io_error)?;
        ensure_directory_is_safe(&root)?;
        let lock_root = fs::canonicalize(&root).map_err(map_io_error)?;
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
        let key = ObjectKey::new(object_key)?;
        let mut source = Cursor::new(contents);
        self.write_stream(&key, &mut source, ObjectWriteOptions::if_absent())
            .map(|_| ())
    }

    fn read(&self, object_key: &str) -> StorageResult<Vec<u8>> {
        let path = self.object_path(object_key, false)?;
        let metadata = fs::symlink_metadata(&path).map_err(map_io_error)?;
        if metadata.file_type().is_symlink() || !metadata.is_file() {
            return Err(StorageError::InvalidObjectKey);
        }
        fs::read(path).map_err(map_io_error)
    }

    fn capabilities(&self) -> StorageCapabilities {
        StorageCapabilities::ALL
    }

    fn read_stream(&self, object_key: &ObjectKey) -> StorageResult<ObjectRead> {
        let mut file = self.open_object_file(object_key)?;
        let size = file.metadata().map_err(map_io_error)?.len();
        let version = version_for_file(&mut file)?;
        file.seek(SeekFrom::Start(0)).map_err(map_io_error)?;
        let metadata = ObjectMetadata::new(object_key.clone(), size, Some(version));
        Ok(ObjectRead::new(metadata, file))
    }

    fn read_range(&self, object_key: &ObjectKey, range: ObjectRange) -> StorageResult<ObjectRead> {
        let mut file = self.open_object_file(object_key)?;
        let size = file.metadata().map_err(map_io_error)?.len();
        if range.end() > size {
            return Err(StorageError::InvalidRange);
        }
        let version = version_for_file(&mut file)?;
        file.seek(SeekFrom::Start(range.start()))
            .map_err(map_io_error)?;
        let metadata = ObjectMetadata::new(object_key.clone(), size, Some(version));
        let reader = LimitedFileReader {
            file,
            remaining: range.length(),
        };
        Ok(ObjectRead::new(metadata, reader))
    }

    fn metadata(&self, object_key: &ObjectKey) -> StorageResult<ObjectMetadata> {
        let mut file = self.open_object_file(object_key)?;
        let size = file.metadata().map_err(map_io_error)?.len();
        let version = version_for_file(&mut file)?;
        Ok(ObjectMetadata::new(object_key.clone(), size, Some(version)))
    }

    fn write_stream(
        &self,
        object_key: &ObjectKey,
        source: &mut dyn Read,
        options: ObjectWriteOptions,
    ) -> StorageResult<ObjectMetadata> {
        let _guard = self
            .state
            .cas_lock
            .lock()
            .map_err(|_| StorageError::Unavailable)?;
        let _file_lock = acquire_cas_file_lock(&self.state.lock_root)?;
        let path = self.object_path(object_key.as_str(), true)?;
        let current_version = current_version_for_path(&path)?;
        match options.condition() {
            StorageWriteCondition::Any => {}
            StorageWriteCondition::IfAbsent if current_version.is_some() => {
                return Err(StorageError::AlreadyExists);
            }
            StorageWriteCondition::IfAbsent => {}
            StorageWriteCondition::IfVersion(expected)
                if current_version.as_ref() == Some(expected) => {}
            StorageWriteCondition::IfVersion(_) => return Err(StorageError::Conflict),
        }

        let temporary_path = write_temporary_stream(&path, source, options.expected_size())?;
        let version = match version_for_path(&temporary_path) {
            Ok(version) => version,
            Err(error) => {
                let _ = fs::remove_file(&temporary_path);
                return Err(error);
            }
        };
        let size = match fs::metadata(&temporary_path) {
            Ok(metadata) => metadata.len(),
            Err(error) => {
                let _ = fs::remove_file(&temporary_path);
                return Err(map_io_error(error));
            }
        };

        let publish_result = match options.condition() {
            StorageWriteCondition::IfAbsent => fs::hard_link(&temporary_path, &path),
            StorageWriteCondition::Any | StorageWriteCondition::IfVersion(_) => {
                replace_file(&temporary_path, &path)
            }
        };
        if let Err(error) = publish_result {
            let _ = fs::remove_file(&temporary_path);
            return Err(match (options.condition(), error.kind()) {
                (StorageWriteCondition::IfAbsent, std::io::ErrorKind::AlreadyExists) => {
                    StorageError::AlreadyExists
                }
                (StorageWriteCondition::IfVersion(_), std::io::ErrorKind::AlreadyExists) => {
                    StorageError::Conflict
                }
                (_, _) => map_io_error(error),
            });
        }
        if matches!(options.condition(), StorageWriteCondition::IfAbsent)
            && fs::remove_file(&temporary_path).is_err()
        {
            return Err(StorageError::Io);
        }
        sync_parent(path.parent())?;
        Ok(ObjectMetadata::new(object_key.clone(), size, Some(version)))
    }

    fn delete(&self, object_key: &ObjectKey) -> StorageResult<()> {
        let _guard = self
            .state
            .cas_lock
            .lock()
            .map_err(|_| StorageError::Unavailable)?;
        let _file_lock = acquire_cas_file_lock(&self.state.lock_root)?;
        let path = self.object_path(object_key.as_str(), false)?;
        let metadata = fs::symlink_metadata(&path).map_err(map_io_error)?;
        if metadata.file_type().is_symlink() || !metadata.is_file() {
            return Err(StorageError::InvalidObjectKey);
        }
        fs::remove_file(&path).map_err(map_io_error)?;
        sync_parent(path.parent())
    }

    fn list_objects(&self, prefix: &str) -> StorageResult<Vec<String>> {
        RepositoryObject::new(prefix).map_err(|_| StorageError::InvalidObjectKey)?;
        ensure_directory_is_safe(&self.state.root)?;

        let mut object_keys = Vec::new();
        collect_object_keys(&self.state.root, &self.state.root, prefix, &mut object_keys)?;
        object_keys.sort();
        Ok(object_keys)
    }

    fn list_page(&self, request: &ObjectListRequest) -> StorageResult<ObjectListPage> {
        request.validate()?;
        let mut object_keys = Vec::new();
        collect_object_keys(
            &self.state.root,
            &self.state.root,
            request.prefix().as_str(),
            &mut object_keys,
        )?;
        object_keys.sort();
        let cursor = request.cursor().map(|value| value.as_str());
        let mut objects: Vec<ObjectMetadata> = Vec::with_capacity(request.limit());
        let mut next_cursor = None;
        for object_key in object_keys {
            if cursor.is_some_and(|value| object_key.as_str() <= value) {
                continue;
            }
            if objects.len() == request.limit() {
                let last_key = objects
                    .last()
                    .map(|object| object.key().as_str().to_owned())
                    .ok_or(StorageError::Unavailable)?;
                next_cursor = Some(ObjectCursor::new(last_key)?);
                break;
            }
            let key = ObjectKey::new(object_key)?;
            objects.push(self.metadata(&key)?);
        }
        Ok(ObjectListPage::new(objects, next_cursor))
    }

    fn read_with_version(&self, object_key: &str) -> StorageResult<VersionedObject> {
        let key = ObjectKey::new(object_key)?;
        let mut object = self.read_stream(&key)?;
        let version = object
            .metadata()
            .version()
            .cloned()
            .ok_or(StorageError::InvalidVersion)?;
        let size = object.metadata().size();
        let contents = read_stream_to_vec(object.reader(), Some(size))?;
        Ok(VersionedObject::new(contents, version))
    }

    fn compare_and_swap(
        &self,
        object_key: &str,
        expected: Option<&StorageVersion>,
        contents: &[u8],
    ) -> StorageResult<StorageVersion> {
        let key = ObjectKey::new(object_key)?;
        let mut source = Cursor::new(contents);
        let options = match expected {
            Some(version) => ObjectWriteOptions::if_version(version.clone()),
            None => ObjectWriteOptions::if_absent(),
        };
        match self.write_stream(&key, &mut source, options) {
            Ok(metadata) => metadata
                .version()
                .cloned()
                .ok_or(StorageError::InvalidVersion),
            Err(StorageError::AlreadyExists | StorageError::Conflict) => {
                Err(StorageError::ConditionNotMet)
            }
            Err(error) => Err(error),
        }
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
    StorageError::from_io_error(&error)
}

fn write_temporary_stream(
    path: &Path,
    source: &mut dyn Read,
    expected_size: Option<u64>,
) -> StorageResult<PathBuf> {
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
    if let Err(error) = copy_stream(source, &mut file, expected_size) {
        drop(file);
        let _ = fs::remove_file(&temporary_path);
        return Err(error);
    }
    if let Err(error) = file.sync_all() {
        drop(file);
        let _ = fs::remove_file(&temporary_path);
        return Err(map_io_error(error));
    }
    drop(file);
    Ok(temporary_path)
}

fn version_for_file(file: &mut File) -> StorageResult<StorageVersion> {
    file.seek(SeekFrom::Start(0)).map_err(map_io_error)?;
    let mut digest = Sha256::new();
    let mut buffer = [0_u8; STORAGE_TRANSFER_BUFFER_SIZE];
    loop {
        let read = file.read(&mut buffer).map_err(map_io_error)?;
        if read == 0 {
            break;
        }
        digest.update(&buffer[..read]);
    }
    file.seek(SeekFrom::Start(0)).map_err(map_io_error)?;
    StorageVersion::from_bytes(digest.finalize().to_vec())
}

fn version_for_path(path: &Path) -> StorageResult<StorageVersion> {
    let mut file = open_file_at_path(path)?;
    version_for_file(&mut file)
}

fn current_version_for_path(path: &Path) -> StorageResult<Option<StorageVersion>> {
    match fs::symlink_metadata(path) {
        Ok(metadata) => {
            if metadata.file_type().is_symlink() || !metadata.is_file() {
                return Err(StorageError::InvalidObjectKey);
            }
            version_for_path(path).map(Some)
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(error) => Err(map_io_error(error)),
    }
}

impl LocalStorage {
    fn open_object_file(&self, object_key: &ObjectKey) -> StorageResult<File> {
        let path = self.object_path(object_key.as_str(), false)?;
        open_file_at_path(&path)
    }
}

fn open_file_at_path(path: &Path) -> StorageResult<File> {
    let metadata = fs::symlink_metadata(path).map_err(map_io_error)?;
    if metadata.file_type().is_symlink() || !metadata.is_file() {
        return Err(StorageError::InvalidObjectKey);
    }
    let file = OpenOptions::new()
        .read(true)
        .open(path)
        .map_err(map_io_error)?;
    let metadata = file.metadata().map_err(map_io_error)?;
    if !metadata.is_file() {
        return Err(StorageError::InvalidObjectKey);
    }
    Ok(file)
}

struct LimitedFileReader {
    file: File,
    remaining: u64,
}

impl Read for LimitedFileReader {
    fn read(&mut self, buffer: &mut [u8]) -> std::io::Result<usize> {
        if self.remaining == 0 || buffer.is_empty() {
            return Ok(0);
        }
        let amount = self
            .remaining
            .min(u64::try_from(buffer.len()).unwrap_or(u64::MAX));
        let amount = usize::try_from(amount).unwrap_or(buffer.len());
        let read = self.file.read(&mut buffer[..amount])?;
        if read == 0 {
            return Err(std::io::Error::new(
                std::io::ErrorKind::UnexpectedEof,
                "storage object changed during range read",
            ));
        }
        self.remaining = self.remaining.saturating_sub(read as u64);
        Ok(read)
    }
}

fn collect_object_keys(
    root: &Path,
    directory: &Path,
    prefix: &str,
    object_keys: &mut Vec<String>,
) -> StorageResult<()> {
    for entry in fs::read_dir(directory).map_err(map_io_error)? {
        let entry = entry.map_err(map_io_error)?;
        let path = entry.path();
        let metadata = fs::symlink_metadata(&path).map_err(map_io_error)?;
        if metadata.file_type().is_symlink() {
            continue;
        }
        if metadata.is_dir() {
            collect_object_keys(root, &path, prefix, object_keys)?;
            continue;
        }
        if !metadata.is_file() {
            continue;
        }

        let relative = path
            .strip_prefix(root)
            .map_err(|_| StorageError::InvalidObjectKey)?;
        let Some(relative) = relative.to_str() else {
            return Err(StorageError::InvalidObjectKey);
        };
        let object_key = relative.replace(std::path::MAIN_SEPARATOR, "/");
        if path
            .file_name()
            .and_then(|value| value.to_str())
            .is_some_and(|value| value.contains(".gib-tmp-") || value == ".gib-head-cas.lock")
        {
            continue;
        }
        let valid = RepositoryObject::new(&object_key).is_ok();
        if !valid {
            continue;
        }
        let prefix_with_separator = format!("{prefix}/");
        if prefix.is_empty()
            || object_key == prefix
            || object_key.starts_with(&prefix_with_separator)
        {
            object_keys.push(object_key);
        }
    }
    Ok(())
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
