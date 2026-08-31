use crate::application::ports::{
    CredentialReference, CredentialStore, CredentialStoreError, CredentialStoreOperation,
    STORAGE_CONFIGURATION_FILE_SUFFIX, StorageBackend, StorageConfiguration,
    StorageConfigurationError, StorageConfigurationOperation, StorageConfigurationResult,
    StorageCredentials, StorageName,
};
use crate::format::{
    DecodedStorageConfiguration, PersistedStorageBackend, decode_storage_configuration,
    encode_storage_configuration,
};
use std::collections::{BTreeMap, VecDeque};
use std::fs::{self, File, OpenOptions};
use std::io::{self, Read, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, OnceLock, Weak};

static NEXT_STORAGE_CONFIGURATION_TEMP_ID: AtomicU64 = AtomicU64::new(1);
static NEXT_MEMORY_CREDENTIAL_ID: AtomicU64 = AtomicU64::new(1);
static STORAGE_CONFIGURATION_LOCKS: OnceLock<Mutex<BTreeMap<PathBuf, Weak<Mutex<()>>>>> =
    OnceLock::new();

const MAX_STORAGE_CONFIGURATION_TEMP_ATTEMPTS: usize = 32;

/// A cloneable type-erased handle for an approved credential store.
#[derive(Clone)]
pub struct CredentialStoreHandle {
    inner: Arc<dyn CredentialStore>,
}

impl CredentialStoreHandle {
    /// Wraps a credential-store adapter.
    pub fn new<S>(store: S) -> Self
    where
        S: CredentialStore + 'static,
    {
        Self {
            inner: Arc::new(store),
        }
    }

    /// Wraps an existing type-erased credential store.
    pub fn from_arc(store: Arc<dyn CredentialStore>) -> Self {
        Self { inner: store }
    }

    /// Returns the underlying credential-store capability.
    pub fn as_store(&self) -> &dyn CredentialStore {
        self.inner.as_ref()
    }
}

impl CredentialStore for CredentialStoreHandle {
    fn store(
        &self,
        credentials: &StorageCredentials,
    ) -> Result<CredentialReference, CredentialStoreError> {
        self.as_store().store(credentials)
    }

    fn load(
        &self,
        reference: &CredentialReference,
    ) -> Result<StorageCredentials, CredentialStoreError> {
        self.as_store().load(reference)
    }

    fn delete(&self, reference: &CredentialReference) -> Result<(), CredentialStoreError> {
        self.as_store().delete(reference)
    }
}

impl std::fmt::Debug for CredentialStoreHandle {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str("CredentialStoreHandle(..)")
    }
}

/// An in-memory credential store for tests and applications that supply their
/// own process-lifetime secret protection.
///
/// This type deliberately does not claim durable encryption. Production code
/// should inject an implementation backed by the platform's approved
/// encrypted credential store.
#[derive(Clone, Default)]
pub struct MemoryCredentialStore {
    state: Arc<MemoryCredentialState>,
}

#[derive(Default)]
struct MemoryCredentialState {
    entries: Mutex<BTreeMap<CredentialReference, StorageCredentials>>,
    failures: Mutex<BTreeMap<CredentialStoreOperation, VecDeque<CredentialStoreError>>>,
}

impl MemoryCredentialStore {
    /// Creates an empty in-memory credential store.
    pub fn new() -> Self {
        Self::default()
    }

    /// Returns the number of credentials currently stored.
    pub fn len(&self) -> usize {
        self.state.entries.lock().map_or(0, |entries| entries.len())
    }

    /// Returns whether no credentials are currently stored.
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// Returns whether a reference is currently present.
    pub fn contains(&self, reference: &CredentialReference) -> bool {
        self.state
            .entries
            .lock()
            .is_ok_and(|entries| entries.contains_key(reference))
    }

    /// Queues a credential-store failure for tests.
    pub fn inject_failure(&self, operation: CredentialStoreOperation, error: CredentialStoreError) {
        if let Ok(mut failures) = self.state.failures.lock() {
            failures.entry(operation).or_default().push_back(error);
        }
    }

    /// Removes all queued test failures.
    pub fn clear_injected_failures(&self) {
        if let Ok(mut failures) = self.state.failures.lock() {
            failures.clear();
        }
    }

    fn take_failure(&self, operation: CredentialStoreOperation) -> Option<CredentialStoreError> {
        let mut failures = self.state.failures.lock().ok()?;
        let failure = failures.get_mut(&operation).and_then(VecDeque::pop_front);
        if failures.get(&operation).is_some_and(VecDeque::is_empty) {
            failures.remove(&operation);
        }
        failure
    }
}

impl std::fmt::Debug for MemoryCredentialStore {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("MemoryCredentialStore")
            .field("credential_count", &self.len())
            .finish()
    }
}

impl CredentialStore for MemoryCredentialStore {
    fn store(
        &self,
        credentials: &StorageCredentials,
    ) -> Result<CredentialReference, CredentialStoreError> {
        if let Some(error) = self.take_failure(CredentialStoreOperation::Store) {
            return Err(error);
        }
        let reference = CredentialReference::new(format!(
            "memory-{}-{}",
            std::process::id(),
            NEXT_MEMORY_CREDENTIAL_ID.fetch_add(1, Ordering::Relaxed)
        ))?;
        self.state
            .entries
            .lock()
            .map_err(|_| CredentialStoreError::Unavailable)?
            .insert(reference.clone(), credentials.clone());
        Ok(reference)
    }

    fn load(
        &self,
        reference: &CredentialReference,
    ) -> Result<StorageCredentials, CredentialStoreError> {
        if let Some(error) = self.take_failure(CredentialStoreOperation::Load) {
            return Err(error);
        }
        self.state
            .entries
            .lock()
            .map_err(|_| CredentialStoreError::Unavailable)?
            .get(reference)
            .cloned()
            .ok_or(CredentialStoreError::NotFound)
    }

    fn delete(&self, reference: &CredentialReference) -> Result<(), CredentialStoreError> {
        if let Some(error) = self.take_failure(CredentialStoreOperation::Delete) {
            return Err(error);
        }
        let removed = self
            .state
            .entries
            .lock()
            .map_err(|_| CredentialStoreError::Unavailable)?
            .remove(reference);
        if removed.is_some() {
            Ok(())
        } else {
            Err(CredentialStoreError::NotFound)
        }
    }
}

/// A filesystem-backed store of named, credential-protected storage
/// configurations.
///
/// Each record is a versioned MessagePack file containing only backend
/// settings and an opaque credential reference. Blocking filesystem work is
/// performed synchronously by this adapter; callers using an async executor
/// should invoke these methods from their blocking boundary.
#[derive(Clone)]
pub struct StorageConfigurationStore {
    state: Arc<StorageConfigurationStoreState>,
}

struct StorageConfigurationStoreState {
    directory: PathBuf,
    credential_store: CredentialStoreHandle,
    lock: Arc<Mutex<()>>,
    failures: Mutex<BTreeMap<StorageConfigurationOperation, VecDeque<StorageConfigurationError>>>,
}

impl StorageConfigurationStore {
    /// Creates a named-storage store below `directory`.
    pub fn new<C>(
        directory: impl AsRef<Path>,
        credential_store: C,
    ) -> StorageConfigurationResult<Self>
    where
        C: CredentialStore + 'static,
    {
        let directory = directory.as_ref().to_path_buf();
        validate_configuration_directory(&directory)?;
        fs::create_dir_all(&directory).map_err(map_io_error)?;
        ensure_directory_is_safe(&directory)?;
        let directory = fs::canonicalize(&directory).map_err(map_io_error)?;
        ensure_directory_is_safe(&directory)?;
        let lock = lock_for_directory(&directory)?;
        Ok(Self {
            state: Arc::new(StorageConfigurationStoreState {
                directory,
                credential_store: CredentialStoreHandle::new(credential_store),
                lock,
                failures: Mutex::new(BTreeMap::new()),
            }),
        })
    }

    /// Returns the canonical directory containing named-storage records.
    pub fn directory(&self) -> &Path {
        &self.state.directory
    }

    /// Returns the path for one validated storage name.
    pub fn record_path(&self, name: impl AsRef<str>) -> StorageConfigurationResult<PathBuf> {
        let name = StorageName::new(name.as_ref())?;
        ensure_directory_is_safe(&self.state.directory)?;
        Ok(self.path_for(&name))
    }

    /// Adds or atomically updates a named storage configuration.
    pub fn save(
        &self,
        name: impl AsRef<str>,
        configuration: StorageConfiguration,
    ) -> StorageConfigurationResult<()> {
        let name = StorageName::new(name.as_ref())?;
        configuration
            .backend()
            .validate()
            .map_err(|_| StorageConfigurationError::InvalidConfiguration)?;
        let path = self.path_for(&name);
        let _guard = self.acquire_lock()?;
        ensure_directory_is_safe(&self.state.directory)?;
        let previous = self.read_previous_record(&path)?;
        let previous_reference = previous
            .as_ref()
            .and_then(|(_, decoded)| decoded.credential_reference.as_deref())
            .map(CredentialReference::new)
            .transpose()
            .map_err(|_| StorageConfigurationError::Malformed)?;
        if let Some((_, decoded)) = &previous {
            backend_from_decoded(decoded.clone())?;
        }

        let new_reference = match configuration.credentials() {
            Some(credentials) => Some(
                self.state
                    .credential_store
                    .store(credentials)
                    .map_err(map_credential_store_error_for_write)?,
            ),
            None => None,
        };
        if new_reference.is_some() && new_reference == previous_reference {
            return Err(StorageConfigurationError::CredentialStoreFailure);
        }
        let encoded = match encode_storage_configuration(
            configuration.backend(),
            new_reference.as_ref().map(CredentialReference::as_str),
        ) {
            Ok(encoded) => encoded,
            Err(error) => {
                return Err(if self.delete_new_reference(&new_reference) {
                    StorageConfigurationError::Unavailable
                } else {
                    error.into()
                });
            }
        };
        let write_result = self.write_atomically(&path, &encoded);
        if let Err(error) = write_result {
            let cleanup_error = self.delete_new_reference(&new_reference);
            let error = self.restore_after_write_failure(&path, previous.as_ref(), error);
            return Err(if cleanup_error {
                StorageConfigurationError::Unavailable
            } else {
                error
            });
        }

        if previous_reference != new_reference
            && let Some(reference) = previous_reference.as_ref()
            && let Err(error) = self.state.credential_store.delete(reference)
            && error != CredentialStoreError::NotFound
        {
            let rollback = self.restore_previous(&path, previous.as_ref());
            let cleanup = self.delete_new_reference(&new_reference);
            if rollback.is_err() || cleanup {
                return Err(StorageConfigurationError::Unavailable);
            }
            return Err(StorageConfigurationError::CredentialStoreFailure);
        }
        Ok(())
    }

    /// Alias for [`Self::save`] using update terminology.
    pub fn update(
        &self,
        name: impl AsRef<str>,
        configuration: StorageConfiguration,
    ) -> StorageConfigurationResult<()> {
        self.save(name, configuration)
    }

    /// Loads a named configuration and resolves its credential reference.
    pub fn load(&self, name: impl AsRef<str>) -> StorageConfigurationResult<StorageConfiguration> {
        let name = StorageName::new(name.as_ref())?;
        let path = self.path_for(&name);
        let _guard = self.acquire_lock()?;
        ensure_directory_is_safe(&self.state.directory)?;
        let bytes = self.read_record_bytes(&path)?;
        self.resolve_record(bytes)
    }

    /// Enumerates valid record filenames in lexical order.
    pub fn enumerate(&self) -> StorageConfigurationResult<Vec<StorageName>> {
        let _guard = self.acquire_lock()?;
        ensure_directory_is_safe(&self.state.directory)?;
        let mut names = Vec::new();
        for entry in fs::read_dir(&self.state.directory).map_err(map_io_error)? {
            let entry = entry.map_err(map_io_error)?;
            let metadata = fs::symlink_metadata(entry.path()).map_err(map_io_error)?;
            let file_name = entry.file_name();
            let Some(file_name) = file_name.to_str() else {
                continue;
            };
            let Some(name) = file_name.strip_suffix(STORAGE_CONFIGURATION_FILE_SUFFIX) else {
                continue;
            };
            if is_link_like_metadata(&metadata) || !metadata.is_file() {
                return Err(StorageConfigurationError::InvalidPath);
            }
            names.push(StorageName::new(name)?);
        }
        names.sort();
        Ok(names)
    }

    /// Alias for [`Self::enumerate`].
    pub fn list(&self) -> StorageConfigurationResult<Vec<StorageName>> {
        self.enumerate()
    }

    /// Deletes a named configuration and its referenced credential.
    pub fn delete(&self, name: impl AsRef<str>) -> StorageConfigurationResult<()> {
        let name = StorageName::new(name.as_ref())?;
        let path = self.path_for(&name);
        let _guard = self.acquire_lock()?;
        ensure_directory_is_safe(&self.state.directory)?;
        let previous = self
            .read_previous_record(&path)?
            .ok_or(StorageConfigurationError::NotFound)?;
        backend_from_decoded(previous.1.clone())?;
        let reference = previous
            .1
            .credential_reference
            .as_deref()
            .map(CredentialReference::new)
            .transpose()
            .map_err(|_| StorageConfigurationError::Malformed)?;
        let remove_result = self.remove_atomically(&path);
        if let Err(error) = remove_result {
            let error = self.restore_after_remove_failure(&path, Some(&previous), error);
            return Err(error);
        }
        if let Some(reference) = reference
            && let Err(error) = self.state.credential_store.delete(&reference)
            && error != CredentialStoreError::NotFound
        {
            let rollback = self.restore_previous(&path, Some(&previous));
            if rollback.is_err() {
                return Err(StorageConfigurationError::Unavailable);
            }
            return Err(StorageConfigurationError::CredentialStoreFailure);
        }
        Ok(())
    }

    /// Alias for [`Self::delete`].
    pub fn remove(&self, name: impl AsRef<str>) -> StorageConfigurationResult<()> {
        self.delete(name)
    }

    /// Queues a filesystem failure for atomic-persistence tests.
    pub fn inject_failure(
        &self,
        operation: StorageConfigurationOperation,
        error: StorageConfigurationError,
    ) {
        if let Ok(mut failures) = self.state.failures.lock() {
            failures.entry(operation).or_default().push_back(error);
        }
    }

    /// Removes all queued filesystem test failures.
    pub fn clear_injected_failures(&self) {
        if let Ok(mut failures) = self.state.failures.lock() {
            failures.clear();
        }
    }

    fn path_for(&self, name: &StorageName) -> PathBuf {
        self.state.directory.join(format!(
            "{}{STORAGE_CONFIGURATION_FILE_SUFFIX}",
            name.as_str()
        ))
    }

    fn acquire_lock(&self) -> StorageConfigurationResult<std::sync::MutexGuard<'_, ()>> {
        self.state
            .lock
            .lock()
            .map_err(|_| StorageConfigurationError::Unavailable)
    }

    fn read_previous_record(
        &self,
        path: &Path,
    ) -> StorageConfigurationResult<Option<(Vec<u8>, DecodedStorageConfiguration)>> {
        let Some(bytes) = self.read_optional_record_bytes(path)? else {
            return Ok(None);
        };
        let decoded =
            decode_storage_configuration(&bytes).map_err(StorageConfigurationError::from)?;
        Ok(Some((bytes, decoded)))
    }

    fn resolve_record(&self, bytes: Vec<u8>) -> StorageConfigurationResult<StorageConfiguration> {
        let decoded =
            decode_storage_configuration(&bytes).map_err(StorageConfigurationError::from)?;
        let backend = backend_from_decoded(decoded.clone())?;
        let reference = decoded
            .credential_reference
            .as_deref()
            .map(CredentialReference::new)
            .transpose()
            .map_err(|_| StorageConfigurationError::Malformed)?;
        let credentials = match (&backend, reference.as_ref()) {
            (StorageBackend::Local(_), None) => None,
            (StorageBackend::Local(_), Some(_)) => {
                return Err(StorageConfigurationError::Malformed);
            }
            (StorageBackend::S3(_), Some(reference))
            | (StorageBackend::WebDav(_), Some(reference)) => {
                let credentials = self
                    .state
                    .credential_store
                    .load(reference)
                    .map_err(map_credential_store_error_for_load)?;
                if Some(credentials.kind()) != expected_credential_kind(&backend) {
                    return Err(StorageConfigurationError::InvalidConfiguration);
                }
                Some(credentials)
            }
            (StorageBackend::S3(_) | StorageBackend::WebDav(_), None) => {
                return Err(StorageConfigurationError::MissingCredentialReference);
            }
        };
        StorageConfiguration::new(backend, credentials)
            .map(|configuration| configuration.with_loaded_credential_reference(reference))
            .map_err(|error| match error {
                StorageConfigurationError::MissingCredentialReference => {
                    StorageConfigurationError::MissingCredentialReference
                }
                _ => StorageConfigurationError::InvalidConfiguration,
            })
    }

    fn read_optional_record_bytes(
        &self,
        path: &Path,
    ) -> StorageConfigurationResult<Option<Vec<u8>>> {
        match fs::symlink_metadata(path) {
            Ok(metadata) => {
                if is_link_like_metadata(&metadata) || !metadata.is_file() {
                    return Err(StorageConfigurationError::InvalidPath);
                }
                let length = usize::try_from(metadata.len())
                    .map_err(|_| StorageConfigurationError::TooLarge)?;
                let max_length = crate::application::ports::MAX_STORAGE_CONFIGURATION_BYTES;
                if length > max_length {
                    return Err(StorageConfigurationError::TooLarge);
                }
                let file = open_record_file(path).map_err(map_io_error)?;
                let opened_metadata = file.metadata().map_err(map_io_error)?;
                if is_link_like_metadata(&opened_metadata) || !opened_metadata.is_file() {
                    return Err(StorageConfigurationError::InvalidPath);
                }
                let mut bytes = Vec::with_capacity(length.min(max_length));
                let read_limit =
                    u64::try_from(max_length).map_err(|_| StorageConfigurationError::TooLarge)? + 1;
                file.take(read_limit)
                    .read_to_end(&mut bytes)
                    .map_err(map_io_error)?;
                if bytes.len() > max_length {
                    return Err(StorageConfigurationError::TooLarge);
                }
                Ok(Some(bytes))
            }
            Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(None),
            Err(error) => Err(map_io_error(error)),
        }
    }

    fn read_record_bytes(&self, path: &Path) -> StorageConfigurationResult<Vec<u8>> {
        self.read_optional_record_bytes(path)?
            .ok_or(StorageConfigurationError::NotFound)
    }

    fn write_atomically(&self, path: &Path, contents: &[u8]) -> Result<(), AtomicFileError> {
        if contents.len() > crate::application::ports::MAX_STORAGE_CONFIGURATION_BYTES {
            return Err(AtomicFileError::not_published(
                StorageConfigurationError::TooLarge,
            ));
        }
        if let Some(error) = self.take_failure(StorageConfigurationOperation::Write) {
            return Err(AtomicFileError::not_published(error));
        }
        let parent = path.parent().ok_or_else(|| {
            AtomicFileError::not_published(StorageConfigurationError::InvalidPath)
        })?;
        if let Err(error) = ensure_directory_is_safe(parent) {
            return Err(AtomicFileError::not_published(error));
        }
        let (temporary_path, mut file) = match create_temporary_file(path, parent) {
            Ok(file) => file,
            Err(error) => return Err(AtomicFileError::not_published(error)),
        };
        let write_result = if file.write_all(contents).is_err()
            || self
                .take_failure(StorageConfigurationOperation::Flush)
                .is_some()
            || file.flush().is_err()
            || file.sync_all().is_err()
        {
            Err(StorageConfigurationError::Io)
        } else {
            Ok(())
        };
        drop(file);
        if let Err(error) = write_result {
            let _ = fs::remove_file(&temporary_path);
            return Err(AtomicFileError::not_published(error));
        }
        if let Some(error) = self.take_failure(StorageConfigurationOperation::Rename) {
            let _ = fs::remove_file(&temporary_path);
            return Err(AtomicFileError::not_published(error));
        }
        if let Err(error) = replace_file(&temporary_path, path) {
            let _ = fs::remove_file(&temporary_path);
            return Err(AtomicFileError::not_published(map_io_error(error)));
        }
        if let Some(error) = self.take_failure(StorageConfigurationOperation::DirectorySync) {
            return Err(AtomicFileError::published(error));
        }
        if let Err(error) = sync_parent(parent) {
            return Err(AtomicFileError::published(map_io_error(error)));
        }
        Ok(())
    }

    fn remove_atomically(&self, path: &Path) -> Result<(), AtomicFileError> {
        if let Some(error) = self.take_failure(StorageConfigurationOperation::Remove) {
            return Err(AtomicFileError::not_published(error));
        }
        let metadata = match fs::symlink_metadata(path) {
            Ok(metadata) => metadata,
            Err(error) if error.kind() == io::ErrorKind::NotFound => {
                return Err(AtomicFileError::not_published(
                    StorageConfigurationError::NotFound,
                ));
            }
            Err(error) => return Err(AtomicFileError::not_published(map_io_error(error))),
        };
        if is_link_like_metadata(&metadata) || !metadata.is_file() {
            return Err(AtomicFileError::not_published(
                StorageConfigurationError::InvalidPath,
            ));
        }
        let parent = path.parent().ok_or_else(|| {
            AtomicFileError::not_published(StorageConfigurationError::InvalidPath)
        })?;
        if let Err(error) = ensure_directory_is_safe(parent) {
            return Err(AtomicFileError::not_published(error));
        }
        fs::remove_file(path)
            .map_err(|error| AtomicFileError::not_published(map_io_error(error)))?;
        if let Some(error) = self.take_failure(StorageConfigurationOperation::DirectorySync) {
            return Err(AtomicFileError::published(error));
        }
        sync_parent(parent).map_err(|error| AtomicFileError::published(map_io_error(error)))
    }

    fn restore_previous(
        &self,
        path: &Path,
        previous: Option<&(Vec<u8>, DecodedStorageConfiguration)>,
    ) -> StorageConfigurationResult<()> {
        match previous {
            Some((bytes, _)) => self
                .write_atomically(path, bytes)
                .map_err(|error| error.error),
            None => match self.remove_atomically(path) {
                Ok(()) => Ok(()),
                Err(error) if error.error == StorageConfigurationError::NotFound => Ok(()),
                Err(error) => Err(error.error),
            },
        }
    }

    fn restore_after_write_failure(
        &self,
        path: &Path,
        previous: Option<&(Vec<u8>, DecodedStorageConfiguration)>,
        error: AtomicFileError,
    ) -> StorageConfigurationError {
        if !error.published {
            return error.error;
        }
        self.restore_previous(path, previous)
            .map_or(StorageConfigurationError::Unavailable, |_| error.error)
    }

    fn restore_after_remove_failure(
        &self,
        path: &Path,
        previous: Option<&(Vec<u8>, DecodedStorageConfiguration)>,
        error: AtomicFileError,
    ) -> StorageConfigurationError {
        if !error.published {
            return error.error;
        }
        self.restore_previous(path, previous)
            .map_or(StorageConfigurationError::Unavailable, |_| error.error)
    }

    fn delete_new_reference(&self, reference: &Option<CredentialReference>) -> bool {
        let Some(reference) = reference.as_ref() else {
            return false;
        };
        match self.state.credential_store.delete(reference) {
            Ok(()) | Err(CredentialStoreError::NotFound) => false,
            Err(_) => true,
        }
    }

    fn take_failure(
        &self,
        operation: StorageConfigurationOperation,
    ) -> Option<StorageConfigurationError> {
        let mut failures = self.state.failures.lock().ok()?;
        let failure = failures.get_mut(&operation).and_then(VecDeque::pop_front);
        if failures.get(&operation).is_some_and(VecDeque::is_empty) {
            failures.remove(&operation);
        }
        failure
    }
}

/// Alias emphasizing that this store is filesystem-backed.
pub type LocalStorageConfiguration = StorageConfigurationStore;

impl std::fmt::Debug for StorageConfigurationStore {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("StorageConfigurationStore")
            .field("directory", &self.state.directory)
            .finish()
    }
}

struct AtomicFileError {
    error: StorageConfigurationError,
    published: bool,
}

impl AtomicFileError {
    fn not_published(error: StorageConfigurationError) -> Self {
        Self {
            error,
            published: false,
        }
    }

    fn published(error: StorageConfigurationError) -> Self {
        Self {
            error,
            published: true,
        }
    }
}

fn backend_from_decoded(
    decoded: DecodedStorageConfiguration,
) -> StorageConfigurationResult<StorageBackend> {
    match decoded.backend {
        PersistedStorageBackend::Local { root_path } => {
            crate::application::ports::LocalStorageSettings::new(root_path)
                .map(StorageBackend::Local)
        }
        PersistedStorageBackend::S3 {
            region,
            bucket,
            endpoint,
            force_path_style,
            multipart_threshold,
            multipart_part_size,
            max_concurrency,
            capability_cache_path,
        } => {
            let mut settings = crate::application::ports::S3StorageSettings::new(region, bucket)?;
            if let Some(endpoint) = endpoint {
                settings = settings.with_endpoint(endpoint)?;
            }
            settings = settings
                .with_force_path_style(force_path_style)
                .with_multipart_threshold(multipart_threshold)
                .with_multipart_part_size(multipart_part_size)
                .with_max_concurrency(max_concurrency);
            if let Some(path) = capability_cache_path {
                settings = settings.with_capability_cache_path(path);
            }
            let backend = StorageBackend::S3(settings);
            backend.validate()?;
            Ok(backend)
        }
        PersistedStorageBackend::WebDav {
            collection_url,
            allow_insecure_http,
            max_concurrency,
        } => {
            let settings = crate::application::ports::WebDavStorageSettings::new(collection_url)?
                .with_allow_insecure_http(allow_insecure_http)
                .with_max_concurrency(max_concurrency);
            let backend = StorageBackend::WebDav(settings);
            backend.validate()?;
            Ok(backend)
        }
    }
}

fn expected_credential_kind(
    backend: &StorageBackend,
) -> Option<crate::application::ports::StorageCredentialKind> {
    match backend {
        StorageBackend::S3(_) => Some(crate::application::ports::StorageCredentialKind::S3),
        StorageBackend::WebDav(_) => Some(crate::application::ports::StorageCredentialKind::WebDav),
        StorageBackend::Local(_) => None,
    }
}

fn map_credential_store_error_for_write(error: CredentialStoreError) -> StorageConfigurationError {
    match error {
        CredentialStoreError::Invalid => StorageConfigurationError::InvalidConfiguration,
        CredentialStoreError::NotFound
        | CredentialStoreError::PermissionDenied
        | CredentialStoreError::Io
        | CredentialStoreError::Unavailable => StorageConfigurationError::CredentialStoreFailure,
    }
}

fn map_credential_store_error_for_load(error: CredentialStoreError) -> StorageConfigurationError {
    match error {
        CredentialStoreError::NotFound => StorageConfigurationError::MissingCredentialReference,
        CredentialStoreError::Invalid => StorageConfigurationError::Malformed,
        CredentialStoreError::PermissionDenied
        | CredentialStoreError::Io
        | CredentialStoreError::Unavailable => StorageConfigurationError::CredentialStoreFailure,
    }
}

fn validate_configuration_directory(path: &Path) -> StorageConfigurationResult<()> {
    if path.as_os_str().is_empty()
        || path
            .components()
            .any(|component| component.as_os_str().to_string_lossy().contains('\0'))
    {
        return Err(StorageConfigurationError::InvalidPath);
    }
    Ok(())
}

fn ensure_directory_is_safe(path: &Path) -> StorageConfigurationResult<()> {
    let metadata = fs::symlink_metadata(path).map_err(map_io_error)?;
    if is_link_like_metadata(&metadata) || !metadata.is_dir() {
        return Err(StorageConfigurationError::InvalidPath);
    }
    Ok(())
}

fn is_link_like_metadata(metadata: &fs::Metadata) -> bool {
    #[cfg(windows)]
    {
        use std::os::windows::fs::MetadataExt;

        return metadata.file_type().is_symlink()
            || metadata.file_attributes() & FILE_ATTRIBUTE_REPARSE_POINT != 0;
    }
    #[cfg(not(windows))]
    {
        metadata.file_type().is_symlink()
    }
}

fn lock_for_directory(path: &Path) -> StorageConfigurationResult<Arc<Mutex<()>>> {
    let locks = STORAGE_CONFIGURATION_LOCKS.get_or_init(|| Mutex::new(BTreeMap::new()));
    let mut locks = locks
        .lock()
        .map_err(|_| StorageConfigurationError::Unavailable)?;
    locks.retain(|_, lock| lock.strong_count() != 0);
    if let Some(lock) = locks.get(path).and_then(Weak::upgrade) {
        return Ok(lock);
    }
    let lock = Arc::new(Mutex::new(()));
    locks.insert(path.to_path_buf(), Arc::downgrade(&lock));
    Ok(lock)
}

fn create_temporary_file(
    path: &Path,
    parent: &Path,
) -> StorageConfigurationResult<(PathBuf, File)> {
    let file_name = path
        .file_name()
        .and_then(|name| name.to_str())
        .ok_or(StorageConfigurationError::InvalidPath)?;
    for _ in 0..MAX_STORAGE_CONFIGURATION_TEMP_ATTEMPTS {
        let id = NEXT_STORAGE_CONFIGURATION_TEMP_ID.fetch_add(1, Ordering::Relaxed);
        let temporary = parent.join(format!(".{file_name}.gib-tmp-{}-{id}", std::process::id()));
        match open_private_file(&temporary) {
            Ok(file) => {
                #[cfg(unix)]
                {
                    use std::os::unix::fs::PermissionsExt;

                    if let Err(error) = file.set_permissions(fs::Permissions::from_mode(0o600)) {
                        drop(file);
                        let _ = fs::remove_file(&temporary);
                        return Err(map_io_error(error));
                    }
                }
                return Ok((temporary, file));
            }
            Err(error) if error.kind() == io::ErrorKind::AlreadyExists => continue,
            Err(error) => return Err(map_io_error(error)),
        }
    }
    Err(StorageConfigurationError::Unavailable)
}

fn open_private_file(path: &Path) -> io::Result<File> {
    let mut options = OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;

        options.mode(0o600);
    }
    options.open(path)
}

fn open_record_file(path: &Path) -> io::Result<File> {
    let mut options = OpenOptions::new();
    options.read(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;

        options.custom_flags(libc::O_NOFOLLOW);
    }
    #[cfg(windows)]
    {
        use std::os::windows::fs::OpenOptionsExt;

        options.custom_flags(FILE_FLAG_OPEN_REPARSE_POINT);
    }
    options.open(path)
}

fn sync_parent(path: &Path) -> io::Result<()> {
    #[cfg(unix)]
    {
        match File::open(path)?.sync_all() {
            Ok(()) => Ok(()),
            Err(error) if directory_sync_is_unsupported(&error) => Ok(()),
            Err(error) => Err(error),
        }
    }
    #[cfg(not(unix))]
    {
        let _ = path;
        Ok(())
    }
}

#[cfg(unix)]
fn directory_sync_is_unsupported(error: &io::Error) -> bool {
    matches!(error.kind(), io::ErrorKind::InvalidInput)
        || matches!(
            error.raw_os_error(),
            Some(code) if code == libc::EINVAL || code == libc::ENOTSUP || code == libc::EOPNOTSUPP
        )
}

fn map_io_error(error: io::Error) -> StorageConfigurationError {
    if is_link_error(&error) {
        return StorageConfigurationError::InvalidPath;
    }
    match error.kind() {
        io::ErrorKind::NotFound => StorageConfigurationError::NotFound,
        io::ErrorKind::PermissionDenied => StorageConfigurationError::InvalidPath,
        io::ErrorKind::AlreadyExists => StorageConfigurationError::Io,
        _ => StorageConfigurationError::Io,
    }
}

fn is_link_error(error: &io::Error) -> bool {
    #[cfg(unix)]
    {
        error.raw_os_error() == Some(libc::ELOOP)
    }
    #[cfg(not(unix))]
    {
        let _ = error;
        false
    }
}

#[cfg(not(windows))]
fn replace_file(from: &Path, to: &Path) -> io::Result<()> {
    fs::rename(from, to)
}

#[cfg(windows)]
fn replace_file(from: &Path, to: &Path) -> io::Result<()> {
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
    // SAFETY: both vectors are NUL-terminated UTF-16 paths alive for this
    // synchronous call. MoveFileExW does not retain either pointer.
    let result = unsafe {
        MoveFileExW(
            from.as_ptr(),
            to.as_ptr(),
            MOVEFILE_REPLACE_EXISTING | MOVEFILE_WRITE_THROUGH,
        )
    };
    if result == 0 {
        Err(io::Error::last_os_error())
    } else {
        Ok(())
    }
}

#[cfg(windows)]
const MOVEFILE_REPLACE_EXISTING: u32 = 0x0000_0001;

#[cfg(windows)]
const MOVEFILE_WRITE_THROUGH: u32 = 0x0000_0008;

#[cfg(windows)]
const FILE_ATTRIBUTE_REPARSE_POINT: u32 = 0x0000_0400;

#[cfg(windows)]
const FILE_FLAG_OPEN_REPARSE_POINT: u32 = 0x0020_0000;

#[cfg(windows)]
#[link(name = "kernel32")]
unsafe extern "system" {
    fn MoveFileExW(existing_file_name: *const u16, new_file_name: *const u16, flags: u32) -> i32;
}
