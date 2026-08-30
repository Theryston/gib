use crate::application::ports::{
    RepositoryStorage, StorageError, StorageResult, StorageVersion, VersionedObject,
};
use crate::domain::RepositoryObject;
use std::collections::BTreeMap;
use std::fmt;
use std::sync::{Arc, Mutex};

/// An in-memory repository storage backend for tests and embedded callers.
///
/// Clones share one object map. Its create-if-absent operation is serialized by
/// the backend lock, so concurrent initialization attempts have one winner.
#[derive(Clone, Default)]
pub struct MemoryStorage {
    state: Arc<Mutex<MemoryStorageState>>,
}

#[derive(Default)]
struct MemoryStorageState {
    objects: BTreeMap<String, StoredObject>,
    next_version: u64,
}

struct StoredObject {
    contents: Vec<u8>,
    version: StorageVersion,
}

impl MemoryStorage {
    /// Creates an empty in-memory storage.
    pub fn new() -> Self {
        Self::default()
    }

    /// Returns the logical object keys currently stored in sorted order.
    pub fn objects(&self) -> StorageResult<Vec<String>> {
        let state = self.state.lock().map_err(|_| StorageError::Unavailable)?;
        Ok(state.objects.keys().cloned().collect())
    }

    /// Reads an object directly for diagnostics and test setup.
    pub fn read_object(&self, object_key: &str) -> StorageResult<Vec<u8>> {
        self.read(object_key)
    }

    /// Replaces or inserts an object for corruption and compatibility tests.
    ///
    /// Repository lifecycle use cases never call this method; they use the
    /// immutable [`RepositoryStorage::create_if_absent`] contract instead.
    pub fn replace_object(
        &self,
        object_key: &str,
        contents: impl AsRef<[u8]>,
    ) -> StorageResult<()> {
        validate_object_key(object_key)?;
        let mut state = self.state.lock().map_err(|_| StorageError::Unavailable)?;
        let version = next_version(&mut state)?;
        state.objects.insert(
            object_key.to_owned(),
            StoredObject {
                contents: contents.as_ref().to_vec(),
                version,
            },
        );
        Ok(())
    }

    /// Alias for [`Self::replace_object`] useful in small test fixtures.
    pub fn put(&self, object_key: &str, contents: impl AsRef<[u8]>) -> StorageResult<()> {
        self.replace_object(object_key, contents)
    }

    /// Removes an object for corruption and missing-root tests.
    pub fn remove_object(&self, object_key: &str) -> StorageResult<bool> {
        validate_object_key(object_key)?;
        let mut state = self.state.lock().map_err(|_| StorageError::Unavailable)?;
        Ok(state.objects.remove(object_key).is_some())
    }
}

impl fmt::Debug for MemoryStorage {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        let object_count = self.objects().map_or(0, |objects| objects.len());
        formatter
            .debug_struct("MemoryStorage")
            .field("object_count", &object_count)
            .finish()
    }
}

impl RepositoryStorage for MemoryStorage {
    fn create_if_absent(&self, object_key: &str, contents: &[u8]) -> StorageResult<()> {
        validate_object_key(object_key)?;
        let mut state = self.state.lock().map_err(|_| StorageError::Unavailable)?;
        if state.objects.contains_key(object_key) {
            return Err(StorageError::AlreadyExists);
        }
        let version = next_version(&mut state)?;
        state.objects.insert(
            object_key.to_owned(),
            StoredObject {
                contents: contents.to_vec(),
                version,
            },
        );
        Ok(())
    }

    fn read(&self, object_key: &str) -> StorageResult<Vec<u8>> {
        validate_object_key(object_key)?;
        let state = self.state.lock().map_err(|_| StorageError::Unavailable)?;
        state
            .objects
            .get(object_key)
            .map(|object| object.contents.clone())
            .ok_or(StorageError::NotFound)
    }

    fn read_with_version(&self, object_key: &str) -> StorageResult<VersionedObject> {
        validate_object_key(object_key)?;
        let state = self.state.lock().map_err(|_| StorageError::Unavailable)?;
        let object = state
            .objects
            .get(object_key)
            .ok_or(StorageError::NotFound)?;
        Ok(VersionedObject::new(
            object.contents.clone(),
            object.version.clone(),
        ))
    }

    fn compare_and_swap(
        &self,
        object_key: &str,
        expected: Option<&StorageVersion>,
        contents: &[u8],
    ) -> StorageResult<StorageVersion> {
        validate_object_key(object_key)?;
        let mut state = self.state.lock().map_err(|_| StorageError::Unavailable)?;
        let matches = match (state.objects.get(object_key), expected) {
            (None, None) => true,
            (Some(object), Some(expected)) => object.version == *expected,
            (None, Some(_)) | (Some(_), None) => false,
        };
        if !matches {
            return Err(StorageError::ConditionNotMet);
        }

        let version = next_version(&mut state)?;
        state.objects.insert(
            object_key.to_owned(),
            StoredObject {
                contents: contents.to_vec(),
                version: version.clone(),
            },
        );
        Ok(version)
    }
}

fn next_version(state: &mut MemoryStorageState) -> StorageResult<StorageVersion> {
    let next = state
        .next_version
        .checked_add(1)
        .ok_or(StorageError::Unavailable)?;
    state.next_version = next;
    StorageVersion::from_bytes(next.to_be_bytes())
}

fn validate_object_key(object_key: &str) -> StorageResult<()> {
    RepositoryObject::new(object_key)
        .map(|_| ())
        .map_err(|_| StorageError::InvalidObjectKey)
}
