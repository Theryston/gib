use crate::application::ports::{
    ObjectCursor, ObjectKey, ObjectListPage, ObjectListRequest, ObjectMetadata, ObjectRange,
    ObjectRead, ObjectWriteOptions, RepositoryStorage, STORAGE_TRANSFER_BUFFER_SIZE,
    StorageCapabilities, StorageError, StorageResult, StorageVersion, StorageWriteCondition,
    VersionedObject, copy_stream, read_stream_to_vec,
};
use sha2::{Digest, Sha256};
use std::collections::{BTreeMap, VecDeque};
#[cfg(unix)]
use std::ffi::CString;
use std::fmt;
use std::fs::{self, File, OpenOptions};
use std::io::{Cursor, Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, OnceLock, Weak};

static NEXT_TEMP_OBJECT_ID: AtomicU64 = AtomicU64::new(1);
static LOCAL_STORAGE_LOCKS: OnceLock<Mutex<BTreeMap<PathBuf, Weak<Mutex<()>>>>> = OnceLock::new();

const CAS_LOCK_FILE_NAME: &str = ".gib-head-cas.lock";
const MAX_TEMP_FILE_ATTEMPTS: usize = 32;

/// A filesystem operation that can be failed by [`LocalStorage`] for
/// conformance and fault-injection tests.
#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub enum LocalStorageOperation {
    /// The operation was rejected before a temporary object was created.
    Write,
    /// Flushing or synchronizing a temporary object failed.
    Flush,
    /// Atomic publication of a temporary object failed.
    Rename,
    /// Synchronization of a containing directory failed.
    DirectorySync,
}

/// A filesystem-backed repository storage rooted at one configured directory.
///
/// Object keys are validated logical names and are resolved relative to an
/// opened root. On Unix, every object operation traverses directory handles
/// with no-follow flags, so replacing a checked directory with a symlink cannot
/// redirect an operation outside the configured root. Writes use unique
/// sibling staging files, file synchronization, and atomic publication.
#[derive(Clone)]
pub struct LocalStorage {
    state: Arc<LocalStorageState>,
}

struct LocalStorageState {
    root: PathBuf,
    root_directory: DirectoryHandle,
    cas_lock: Arc<Mutex<()>>,
    failures: Mutex<BTreeMap<LocalStorageOperation, VecDeque<StorageError>>>,
}

impl LocalStorage {
    /// Creates a local storage rooted at `path`, creating the empty root when
    /// necessary.
    pub fn new(path: impl AsRef<Path>) -> StorageResult<Self> {
        let configured_root = path.as_ref().to_path_buf();
        if configured_root.as_os_str().is_empty() {
            return Err(StorageError::InvalidObjectKey);
        }
        fs::create_dir_all(&configured_root).map_err(map_io_error)?;
        ensure_directory_is_safe(&configured_root)?;
        let root = fs::canonicalize(&configured_root).map_err(map_io_error)?;
        ensure_directory_is_safe(&root)?;
        let root_directory = open_root_directory(&root)?;
        open_lock_file(&root_directory)?;
        let cas_lock = lock_for_root(&root)?;
        Ok(Self {
            state: Arc::new(LocalStorageState {
                root,
                root_directory,
                cas_lock,
                failures: Mutex::new(BTreeMap::new()),
            }),
        })
    }

    /// Alias for [`Self::new`] for callers that prefer an explicit backend name.
    pub fn open(path: impl AsRef<Path>) -> StorageResult<Self> {
        Self::new(path)
    }

    /// Returns the canonical configured root for manual inspection and
    /// diagnostics.
    pub fn root_path(&self) -> &Path {
        &self.state.root
    }

    /// Queues a provider-neutral failure for the next local operation of
    /// `operation`.
    ///
    /// This hook is intended for adapter conformance and fault-injection
    /// tests. The queue is shared by clones of this storage handle.
    pub fn inject_failure(&self, operation: LocalStorageOperation, error: StorageError) {
        if let Ok(mut failures) = self.state.failures.lock() {
            failures.entry(operation).or_default().push_back(error);
        }
    }

    /// Alias for [`Self::inject_failure`].
    pub fn fail_next(&self, operation: LocalStorageOperation, error: StorageError) {
        self.inject_failure(operation, error);
    }

    /// Removes all queued local fault injections.
    pub fn clear_injected_failures(&self) {
        if let Ok(mut failures) = self.state.failures.lock() {
            failures.clear();
        }
    }

    fn take_failure(&self, operation: LocalStorageOperation) -> Option<StorageError> {
        let mut failures = self.state.failures.lock().ok()?;
        let failure = failures.get_mut(&operation).and_then(VecDeque::pop_front);
        if failures.get(&operation).is_some_and(VecDeque::is_empty) {
            failures.remove(&operation);
        }
        failure
    }

    fn acquire_cas_file_lock(&self) -> StorageResult<CasFileLock> {
        let file = open_lock_file(&self.state.root_directory)?;
        lock_file(&file).map_err(map_io_error)?;
        Ok(CasFileLock { _file: file })
    }

    fn sync_directory(&self, directory: &DirectoryHandle) -> StorageResult<()> {
        if let Some(error) = self.take_failure(LocalStorageOperation::DirectorySync) {
            return Err(error);
        }
        sync_directory(directory).map_err(map_io_error)
    }

    fn object_location(
        &self,
        object_key: &ObjectKey,
        create_parents: bool,
    ) -> StorageResult<ObjectLocation> {
        validate_local_object_key(object_key)?;
        let mut components = object_key.as_str().split('/');
        let filename = components
            .next_back()
            .ok_or(StorageError::InvalidObjectKey)?
            .to_owned();
        let mut directory = clone_directory(&self.state.root_directory).map_err(map_io_error)?;

        for component in components {
            match open_directory_at(&directory, component) {
                Ok(next) => directory = next,
                Err(error) if create_parents && error.kind() == std::io::ErrorKind::NotFound => {
                    match create_directory_at(&directory, component) {
                        Ok(()) => self.sync_directory(&directory)?,
                        Err(create_error)
                            if create_error.kind() == std::io::ErrorKind::AlreadyExists => {}
                        Err(create_error) => return Err(map_path_io_error(create_error)),
                    }
                    directory =
                        open_directory_at(&directory, component).map_err(map_path_io_error)?;
                }
                Err(error) => return Err(map_path_io_error(error)),
            }
        }

        Ok(ObjectLocation {
            directory,
            filename,
        })
    }

    fn open_object_file(&self, object_key: &ObjectKey) -> StorageResult<File> {
        let location = self.object_location(object_key, false)?;
        let file =
            open_file_at(&location.directory, &location.filename).map_err(map_path_io_error)?;
        ensure_regular_file(&file)?;
        Ok(file)
    }

    fn current_version(&self, location: &ObjectLocation) -> StorageResult<Option<StorageVersion>> {
        let file = match open_file_at(&location.directory, &location.filename) {
            Ok(file) => file,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
            Err(error) => return Err(map_path_io_error(error)),
        };
        ensure_regular_file(&file)?;
        let mut file = file;
        version_for_file(&mut file).map(Some)
    }

    fn create_temporary_file(&self, location: &ObjectLocation) -> StorageResult<(String, File)> {
        for _ in 0..MAX_TEMP_FILE_ATTEMPTS {
            let identifier = NEXT_TEMP_OBJECT_ID.fetch_add(1, Ordering::Relaxed);
            let name = format!("!gib-tmp-{}-{identifier}", std::process::id());
            match create_temporary_file_at(&location.directory, &name) {
                Ok(file) => return Ok((name, file)),
                Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {}
                Err(error) => return Err(map_path_io_error(error)),
            }
        }
        Err(StorageError::Unavailable)
    }

    fn cleanup_temporary_file(&self, location: &ObjectLocation, name: &str) {
        let _ = remove_file_at(&location.directory, name);
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
        let key = ObjectKey::new(object_key)?;
        let mut object = self.read_stream(&key)?;
        let size = object.metadata().size();
        read_stream_to_vec(object.reader(), Some(size))
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
        let _file_lock = self.acquire_cas_file_lock()?;
        if let Some(error) = self.take_failure(LocalStorageOperation::Write) {
            return Err(error);
        }

        let location = self.object_location(object_key, true)?;
        let current_version = self.current_version(&location)?;
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

        let (temporary_name, mut temporary_file) = self.create_temporary_file(&location)?;
        if let Err(error) = copy_stream(source, &mut temporary_file, options.expected_size()) {
            drop(temporary_file);
            self.cleanup_temporary_file(&location, &temporary_name);
            return Err(error);
        }
        if let Some(error) = self.take_failure(LocalStorageOperation::Flush) {
            drop(temporary_file);
            self.cleanup_temporary_file(&location, &temporary_name);
            return Err(error);
        }
        if let Err(error) = temporary_file.flush().map_err(map_io_error) {
            drop(temporary_file);
            self.cleanup_temporary_file(&location, &temporary_name);
            return Err(error);
        }
        if let Err(error) = temporary_file.sync_all().map_err(map_io_error) {
            drop(temporary_file);
            self.cleanup_temporary_file(&location, &temporary_name);
            return Err(error);
        }

        let size = match temporary_file.metadata() {
            Ok(metadata) => metadata.len(),
            Err(error) => {
                drop(temporary_file);
                self.cleanup_temporary_file(&location, &temporary_name);
                return Err(map_io_error(error));
            }
        };
        let version = match version_for_file(&mut temporary_file) {
            Ok(version) => version,
            Err(error) => {
                drop(temporary_file);
                self.cleanup_temporary_file(&location, &temporary_name);
                return Err(error);
            }
        };
        drop(temporary_file);

        if let Some(error) = self.take_failure(LocalStorageOperation::Rename) {
            self.cleanup_temporary_file(&location, &temporary_name);
            return Err(error);
        }
        let publish_result = match options.condition() {
            StorageWriteCondition::IfAbsent => {
                link_file_at(&location.directory, &temporary_name, &location.filename)
            }
            StorageWriteCondition::Any | StorageWriteCondition::IfVersion(_) => {
                replace_file_at(&location.directory, &temporary_name, &location.filename)
            }
        };
        if let Err(error) = publish_result {
            self.cleanup_temporary_file(&location, &temporary_name);
            return Err(match (options.condition(), error.kind()) {
                (StorageWriteCondition::IfAbsent, std::io::ErrorKind::AlreadyExists) => {
                    StorageError::AlreadyExists
                }
                (StorageWriteCondition::IfVersion(_), std::io::ErrorKind::AlreadyExists) => {
                    StorageError::Conflict
                }
                (_, _) => map_path_io_error(error),
            });
        }
        if matches!(options.condition(), StorageWriteCondition::IfAbsent)
            && let Err(error) = remove_file_at(&location.directory, &temporary_name)
        {
            return Err(map_path_io_error(error));
        }
        self.sync_directory(&location.directory)?;
        Ok(ObjectMetadata::new(object_key.clone(), size, Some(version)))
    }

    fn delete(&self, object_key: &ObjectKey) -> StorageResult<()> {
        let _guard = self
            .state
            .cas_lock
            .lock()
            .map_err(|_| StorageError::Unavailable)?;
        let _file_lock = self.acquire_cas_file_lock()?;
        let location = self.object_location(object_key, false)?;
        let file =
            open_file_at(&location.directory, &location.filename).map_err(map_path_io_error)?;
        ensure_regular_file(&file)?;
        drop(file);
        remove_file_at(&location.directory, &location.filename).map_err(map_path_io_error)?;
        self.sync_directory(&location.directory)
    }

    fn list_page(&self, request: &ObjectListRequest) -> StorageResult<ObjectListPage> {
        request.validate()?;
        let mut object_keys = Vec::new();
        collect_object_keys(
            &self.state.root_directory,
            request.prefix().as_str(),
            "",
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
            match self.metadata(&key) {
                Ok(metadata) => objects.push(metadata),
                Err(StorageError::NotFound | StorageError::InvalidObjectKey) => {}
                Err(error) => return Err(error),
            }
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

struct ObjectLocation {
    directory: DirectoryHandle,
    filename: String,
}

struct DirectoryHandle {
    #[cfg(unix)]
    file: File,
    path: PathBuf,
}

fn validate_local_object_key(object_key: &ObjectKey) -> StorageResult<()> {
    if object_key
        .as_str()
        .split('/')
        .any(|component| component == CAS_LOCK_FILE_NAME)
    {
        return Err(StorageError::InvalidObjectKey);
    }
    Ok(())
}

fn ensure_directory_is_safe(path: &Path) -> StorageResult<()> {
    let metadata = fs::symlink_metadata(path).map_err(map_io_error)?;
    if is_link_or_reparse_point(&metadata) || !metadata.is_dir() {
        return Err(StorageError::InvalidObjectKey);
    }
    Ok(())
}

fn ensure_regular_file(file: &File) -> StorageResult<()> {
    let metadata = file.metadata().map_err(map_io_error)?;
    if is_link_or_reparse_point(&metadata) || !metadata.is_file() {
        return Err(StorageError::InvalidObjectKey);
    }
    Ok(())
}

fn map_io_error(error: std::io::Error) -> StorageError {
    StorageError::from_io_error(&error)
}

fn map_path_io_error(error: std::io::Error) -> StorageError {
    if is_path_safety_error(&error) {
        StorageError::InvalidObjectKey
    } else {
        map_io_error(error)
    }
}

#[cfg(unix)]
fn is_path_safety_error(error: &std::io::Error) -> bool {
    match error.raw_os_error() {
        Some(code) => matches!(code, libc::ELOOP | libc::ENOTDIR | libc::ENAMETOOLONG),
        None => matches!(error.kind(), std::io::ErrorKind::InvalidInput),
    }
}

#[cfg(not(unix))]
fn is_path_safety_error(error: &std::io::Error) -> bool {
    matches!(
        error.kind(),
        std::io::ErrorKind::InvalidInput | std::io::ErrorKind::NotADirectory
    )
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

fn collect_object_keys(
    directory: &DirectoryHandle,
    prefix: &str,
    relative_prefix: &str,
    object_keys: &mut Vec<String>,
) -> StorageResult<()> {
    let entries = list_directory_entries(directory).map_err(map_path_io_error)?;
    for name in entries {
        if name == CAS_LOCK_FILE_NAME || name.starts_with("!gib-tmp-") {
            continue;
        }
        let object_key = if relative_prefix.is_empty() {
            name.clone()
        } else {
            format!("{relative_prefix}/{name}")
        };
        if ObjectKey::new(object_key.clone()).is_err() {
            continue;
        }

        match open_directory_at(directory, &name) {
            Ok(child) => collect_object_keys(&child, prefix, &object_key, object_keys)?,
            Err(error) if is_not_directory_error(&error) => {
                let file = match open_file_at(directory, &name) {
                    Ok(file) => file,
                    Err(error) if is_ignored_listing_error(&error) => continue,
                    Err(error) => return Err(map_path_io_error(error)),
                };
                if !is_regular_file_metadata(&file) {
                    continue;
                }
                if matches_prefix(&object_key, prefix) {
                    object_keys.push(object_key);
                }
            }
            Err(error) if is_ignored_listing_error(&error) => {}
            Err(error) => return Err(map_path_io_error(error)),
        }
    }
    Ok(())
}

fn matches_prefix(key: &str, prefix: &str) -> bool {
    prefix.is_empty()
        || key == prefix
        || key
            .strip_prefix(prefix)
            .is_some_and(|suffix| suffix.starts_with('/'))
}

fn is_regular_file_metadata(file: &File) -> bool {
    file.metadata()
        .is_ok_and(|metadata| !is_link_or_reparse_point(&metadata) && metadata.is_file())
}

fn is_not_directory_error(error: &std::io::Error) -> bool {
    matches!(
        error.kind(),
        std::io::ErrorKind::NotADirectory | std::io::ErrorKind::InvalidInput
    ) || {
        #[cfg(unix)]
        {
            error.raw_os_error() == Some(libc::ENOTDIR)
        }
        #[cfg(not(unix))]
        {
            false
        }
    }
}

fn is_ignored_listing_error(error: &std::io::Error) -> bool {
    matches!(
        error.kind(),
        std::io::ErrorKind::NotFound | std::io::ErrorKind::InvalidInput
    ) || {
        #[cfg(unix)]
        {
            matches!(error.raw_os_error(), Some(libc::ELOOP | libc::ENOTDIR))
        }
        #[cfg(not(unix))]
        {
            false
        }
    }
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

struct CasFileLock {
    _file: File,
}

fn open_lock_file(root: &DirectoryHandle) -> StorageResult<File> {
    let file = open_lock_file_at(root).map_err(map_path_io_error)?;
    ensure_regular_file(&file)?;
    Ok(file)
}

#[cfg(unix)]
fn open_root_directory(path: &Path) -> StorageResult<DirectoryHandle> {
    use std::os::unix::fs::OpenOptionsExt;

    let mut options = OpenOptions::new();
    options
        .read(true)
        .custom_flags(libc::O_DIRECTORY | libc::O_NOFOLLOW);
    let file = options.open(path).map_err(map_path_io_error)?;
    Ok(DirectoryHandle {
        file,
        path: path.to_path_buf(),
    })
}

#[cfg(not(unix))]
fn open_root_directory(path: &Path) -> StorageResult<DirectoryHandle> {
    Ok(DirectoryHandle {
        path: path.to_path_buf(),
    })
}

#[cfg(unix)]
fn clone_directory(directory: &DirectoryHandle) -> std::io::Result<DirectoryHandle> {
    Ok(DirectoryHandle {
        file: directory.file.try_clone()?,
        path: directory.path.clone(),
    })
}

#[cfg(not(unix))]
fn clone_directory(directory: &DirectoryHandle) -> std::io::Result<DirectoryHandle> {
    Ok(DirectoryHandle {
        path: directory.path.clone(),
    })
}

#[cfg(unix)]
fn open_directory_at(
    parent: &DirectoryHandle,
    component: &str,
) -> std::io::Result<DirectoryHandle> {
    let file = open_at(
        parent,
        component,
        libc::O_RDONLY | libc::O_DIRECTORY | libc::O_NOFOLLOW | libc::O_CLOEXEC,
        0,
    )?;
    Ok(DirectoryHandle {
        file,
        path: parent.path.join(component),
    })
}

#[cfg(unix)]
fn create_directory_at(parent: &DirectoryHandle, component: &str) -> std::io::Result<()> {
    use std::os::unix::io::AsRawFd;

    let component = CString::new(component).map_err(|_| {
        std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            "directory component contains NUL",
        )
    })?;
    // SAFETY: the parent descriptor is held by `parent`; `component` remains
    // alive for the synchronous syscall and contains no interior NUL bytes.
    let result = unsafe { libc::mkdirat(parent.file.as_raw_fd(), component.as_ptr(), 0o755) };
    if result == -1 {
        Err(std::io::Error::last_os_error())
    } else {
        Ok(())
    }
}

#[cfg(not(unix))]
fn open_directory_at(
    parent: &DirectoryHandle,
    component: &str,
) -> std::io::Result<DirectoryHandle> {
    let path = parent.path.join(component);
    let metadata = fs::symlink_metadata(&path)?;
    if is_link_or_reparse_point(&metadata) {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            "directory component is a link",
        ));
    }
    if !metadata.is_dir() {
        return Err(std::io::Error::from(std::io::ErrorKind::NotADirectory));
    }
    Ok(DirectoryHandle { path })
}

#[cfg(not(unix))]
fn create_directory_at(parent: &DirectoryHandle, component: &str) -> std::io::Result<()> {
    fs::create_dir(parent.path.join(component))
}

#[cfg(unix)]
fn open_file_at(parent: &DirectoryHandle, name: &str) -> std::io::Result<File> {
    open_at(
        parent,
        name,
        libc::O_RDONLY | libc::O_NOFOLLOW | libc::O_NONBLOCK | libc::O_CLOEXEC,
        0,
    )
}

#[cfg(not(unix))]
fn open_file_at(parent: &DirectoryHandle, name: &str) -> std::io::Result<File> {
    let path = parent.path.join(name);
    let metadata = fs::symlink_metadata(&path)?;
    if is_link_or_reparse_point(&metadata) {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            "object is a link",
        ));
    }
    if !metadata.is_file() {
        return Err(std::io::Error::from(std::io::ErrorKind::InvalidInput));
    }
    let mut options = OpenOptions::new();
    options.read(true);
    #[cfg(windows)]
    {
        use std::os::windows::fs::OpenOptionsExt;

        options.custom_flags(FILE_FLAG_OPEN_REPARSE_POINT);
    }
    let file = options.open(path)?;
    let metadata = file.metadata()?;
    if is_link_or_reparse_point(&metadata) {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            "object is a link",
        ));
    }
    Ok(file)
}

#[cfg(unix)]
fn create_temporary_file_at(parent: &DirectoryHandle, name: &str) -> std::io::Result<File> {
    open_at(
        parent,
        name,
        libc::O_RDWR | libc::O_CREAT | libc::O_EXCL | libc::O_CLOEXEC,
        0o600,
    )
}

#[cfg(not(unix))]
fn create_temporary_file_at(parent: &DirectoryHandle, name: &str) -> std::io::Result<File> {
    OpenOptions::new()
        .read(true)
        .write(true)
        .create_new(true)
        .open(parent.path.join(name))
}

#[cfg(unix)]
fn open_lock_file_at(parent: &DirectoryHandle) -> std::io::Result<File> {
    open_at(
        parent,
        CAS_LOCK_FILE_NAME,
        libc::O_RDWR | libc::O_CREAT | libc::O_NOFOLLOW | libc::O_NONBLOCK | libc::O_CLOEXEC,
        0o600,
    )
}

#[cfg(not(unix))]
fn open_lock_file_at(parent: &DirectoryHandle) -> std::io::Result<File> {
    let path = parent.path.join(CAS_LOCK_FILE_NAME);
    match fs::symlink_metadata(&path) {
        Ok(metadata) if is_link_or_reparse_point(&metadata) => {
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidInput,
                "CAS lock is a link",
            ));
        }
        Ok(_) => {}
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
        Err(error) => return Err(error),
    }
    let mut options = OpenOptions::new();
    options.read(true).write(true).create(true).truncate(false);
    #[cfg(windows)]
    {
        use std::os::windows::fs::OpenOptionsExt;

        options.custom_flags(FILE_FLAG_OPEN_REPARSE_POINT);
    }
    options.open(path)
}

#[cfg(unix)]
fn open_at(
    parent: &DirectoryHandle,
    name: &str,
    flags: std::os::raw::c_int,
    mode: libc::mode_t,
) -> std::io::Result<File> {
    use std::os::unix::io::{AsRawFd, FromRawFd};

    let name = CString::new(name).map_err(|_| {
        std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            "path component contains NUL",
        )
    })?;
    // SAFETY: `parent` owns a live directory descriptor; `name` remains alive
    // for the synchronous syscall and contains no interior NUL bytes.
    let descriptor = unsafe {
        libc::openat(
            parent.file.as_raw_fd(),
            name.as_ptr(),
            flags | libc::O_CLOEXEC,
            mode,
        )
    };
    if descriptor == -1 {
        return Err(std::io::Error::last_os_error());
    }
    // SAFETY: `descriptor` is a newly owned descriptor returned by `openat`;
    // this `File` is the sole owner and closes it exactly once.
    Ok(unsafe { File::from_raw_fd(descriptor) })
}

#[cfg(unix)]
fn link_file_at(parent: &DirectoryHandle, from: &str, to: &str) -> std::io::Result<()> {
    use std::os::unix::io::AsRawFd;

    let from = CString::new(from).map_err(|_| {
        std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            "path component contains NUL",
        )
    })?;
    let to = CString::new(to).map_err(|_| {
        std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            "path component contains NUL",
        )
    })?;
    // SAFETY: both names are NUL-free and remain alive for this synchronous
    // call; the directory descriptor is owned by `parent`.
    let result = unsafe {
        libc::linkat(
            parent.file.as_raw_fd(),
            from.as_ptr(),
            parent.file.as_raw_fd(),
            to.as_ptr(),
            0,
        )
    };
    if result == -1 {
        Err(std::io::Error::last_os_error())
    } else {
        Ok(())
    }
}

#[cfg(not(unix))]
fn link_file_at(parent: &DirectoryHandle, from: &str, to: &str) -> std::io::Result<()> {
    fs::hard_link(parent.path.join(from), parent.path.join(to))
}

#[cfg(unix)]
fn replace_file_at(parent: &DirectoryHandle, from: &str, to: &str) -> std::io::Result<()> {
    use std::os::unix::io::AsRawFd;

    let from = CString::new(from).map_err(|_| {
        std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            "path component contains NUL",
        )
    })?;
    let to = CString::new(to).map_err(|_| {
        std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            "path component contains NUL",
        )
    })?;
    // SAFETY: both names are NUL-free and remain alive for this synchronous
    // call; renameat replaces only the directory entry named by `to`.
    let result = unsafe {
        libc::renameat(
            parent.file.as_raw_fd(),
            from.as_ptr(),
            parent.file.as_raw_fd(),
            to.as_ptr(),
        )
    };
    if result == -1 {
        Err(std::io::Error::last_os_error())
    } else {
        Ok(())
    }
}

#[cfg(not(unix))]
fn replace_file_at(parent: &DirectoryHandle, from: &str, to: &str) -> std::io::Result<()> {
    replace_file(&parent.path.join(from), &parent.path.join(to))
}

#[cfg(unix)]
fn remove_file_at(parent: &DirectoryHandle, name: &str) -> std::io::Result<()> {
    use std::os::unix::io::AsRawFd;

    let name = CString::new(name).map_err(|_| {
        std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            "path component contains NUL",
        )
    })?;
    // SAFETY: `name` is NUL-free and remains alive for this synchronous call;
    // unlinkat removes one directory entry below the opened parent.
    let result = unsafe { libc::unlinkat(parent.file.as_raw_fd(), name.as_ptr(), 0) };
    if result == -1 {
        Err(std::io::Error::last_os_error())
    } else {
        Ok(())
    }
}

#[cfg(not(unix))]
fn remove_file_at(parent: &DirectoryHandle, name: &str) -> std::io::Result<()> {
    fs::remove_file(parent.path.join(name))
}

#[cfg(unix)]
fn sync_directory(directory: &DirectoryHandle) -> std::io::Result<()> {
    match directory.file.sync_all() {
        Ok(()) => Ok(()),
        Err(error) if directory_sync_is_unsupported(&error) => Ok(()),
        Err(error) => Err(error),
    }
}

#[cfg(unix)]
fn directory_sync_is_unsupported(error: &std::io::Error) -> bool {
    matches!(error.kind(), std::io::ErrorKind::InvalidInput)
        || matches!(
            error.raw_os_error(),
            Some(code) if code == libc::EINVAL || code == libc::ENOTSUP || code == libc::EOPNOTSUPP
        )
}

#[cfg(not(unix))]
fn sync_directory(_directory: &DirectoryHandle) -> std::io::Result<()> {
    Ok(())
}

#[cfg(unix)]
fn list_directory_entries(directory: &DirectoryHandle) -> std::io::Result<Vec<String>> {
    use std::os::unix::io::IntoRawFd;

    let duplicate = open_at(
        directory,
        ".",
        libc::O_RDONLY | libc::O_DIRECTORY | libc::O_NOFOLLOW | libc::O_CLOEXEC,
        0,
    )?;
    let descriptor = duplicate.into_raw_fd();
    // SAFETY: `descriptor` is a duplicated directory descriptor and ownership
    // is transferred to the DIR stream by fdopendir.
    let stream = unsafe { libc::fdopendir(descriptor) };
    if stream.is_null() {
        // SAFETY: fdopendir did not take ownership when it returned null.
        unsafe { libc::close(descriptor) };
        return Err(std::io::Error::last_os_error());
    }

    let mut entries = Vec::new();
    loop {
        // SAFETY: `stream` is a live DIR* owned until closed below.
        let entry = unsafe { libc::readdir(stream) };
        if entry.is_null() {
            break;
        }
        // SAFETY: d_name is a NUL-terminated entry name owned by the DIR
        // stream and is valid until the next readdir call.
        let bytes = unsafe { std::ffi::CStr::from_ptr((*entry).d_name.as_ptr()) }.to_bytes();
        if bytes == b"." || bytes == b".." {
            continue;
        }
        if let Ok(name) = std::str::from_utf8(bytes) {
            entries.push(name.to_owned());
        }
    }
    // SAFETY: stream remains live and is closed exactly once here; fdopendir
    // owns the duplicated descriptor.
    let close_result = unsafe { libc::closedir(stream) };
    if close_result == -1 {
        return Err(std::io::Error::last_os_error());
    }
    Ok(entries)
}

#[cfg(not(unix))]
fn list_directory_entries(directory: &DirectoryHandle) -> std::io::Result<Vec<String>> {
    let mut entries = Vec::new();
    for entry in fs::read_dir(&directory.path)? {
        let entry = entry?;
        let name = entry.file_name();
        let Some(name) = name.to_str() else {
            continue;
        };
        entries.push(name.to_owned());
    }
    Ok(entries)
}

#[cfg(unix)]
fn lock_file(file: &File) -> std::io::Result<()> {
    use std::os::unix::io::AsRawFd;

    // SAFETY: the descriptor is borrowed from a live File and flock does not
    // retain the pointer or mutate memory owned by Rust.
    let result = unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX) };
    if result == -1 {
        Err(std::io::Error::last_os_error())
    } else {
        Ok(())
    }
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

#[cfg(not(any(unix, windows)))]
fn lock_file(_file: &File) -> std::io::Result<()> {
    Ok(())
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

#[cfg(not(unix))]
fn replace_file(from: &Path, to: &Path) -> std::io::Result<()> {
    #[cfg(not(windows))]
    {
        fs::rename(from, to)
    }
    #[cfg(windows)]
    {
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
        // SAFETY: both vectors are NUL-terminated UTF-16 paths that remain
        // alive for the duration of the call. MoveFileExW does not retain the
        // pointers.
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
}

#[cfg(windows)]
const MOVEFILE_REPLACE_EXISTING: u32 = 0x0000_0001;

#[cfg(windows)]
const MOVEFILE_WRITE_THROUGH: u32 = 0x0000_0008;

#[cfg(windows)]
const FILE_FLAG_OPEN_REPARSE_POINT: u32 = 0x0020_0000;

#[cfg(windows)]
#[link(name = "kernel32")]
unsafe extern "system" {
    fn MoveFileExW(existing_file_name: *const u16, new_file_name: *const u16, flags: u32) -> i32;
}

fn is_link_or_reparse_point(metadata: &fs::Metadata) -> bool {
    #[cfg(windows)]
    {
        use std::os::windows::fs::MetadataExt;

        const FILE_ATTRIBUTE_REPARSE_POINT: u32 = 0x0000_0400;
        metadata.file_type().is_symlink()
            || metadata.file_attributes() & FILE_ATTRIBUTE_REPARSE_POINT != 0
    }
    #[cfg(not(windows))]
    {
        metadata.file_type().is_symlink()
    }
}
