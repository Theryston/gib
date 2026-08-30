use crate::application::ports::{RepositoryStorage, StorageError, StorageResult};
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
    objects: Arc<Mutex<BTreeMap<String, Vec<u8>>>>,
}

impl MemoryStorage {
    /// Creates an empty in-memory storage.
    pub fn new() -> Self {
        Self::default()
    }

    /// Returns the logical object keys currently stored in sorted order.
    pub fn objects(&self) -> StorageResult<Vec<String>> {
        let objects = self.objects.lock().map_err(|_| StorageError::Unavailable)?;
        Ok(objects.keys().cloned().collect())
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
        let mut objects = self.objects.lock().map_err(|_| StorageError::Unavailable)?;
        objects.insert(object_key.to_owned(), contents.as_ref().to_vec());
        Ok(())
    }

    /// Alias for [`Self::replace_object`] useful in small test fixtures.
    pub fn put(&self, object_key: &str, contents: impl AsRef<[u8]>) -> StorageResult<()> {
        self.replace_object(object_key, contents)
    }

    /// Removes an object for corruption and missing-root tests.
    pub fn remove_object(&self, object_key: &str) -> StorageResult<bool> {
        validate_object_key(object_key)?;
        let mut objects = self.objects.lock().map_err(|_| StorageError::Unavailable)?;
        Ok(objects.remove(object_key).is_some())
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
        let mut objects = self.objects.lock().map_err(|_| StorageError::Unavailable)?;
        if objects.contains_key(object_key) {
            return Err(StorageError::AlreadyExists);
        }
        objects.insert(object_key.to_owned(), contents.to_vec());
        Ok(())
    }

    fn read(&self, object_key: &str) -> StorageResult<Vec<u8>> {
        validate_object_key(object_key)?;
        let objects = self.objects.lock().map_err(|_| StorageError::Unavailable)?;
        objects
            .get(object_key)
            .cloned()
            .ok_or(StorageError::NotFound)
    }
}

fn validate_object_key(object_key: &str) -> StorageResult<()> {
    RepositoryObject::new(object_key)
        .map(|_| ())
        .map_err(|_| StorageError::InvalidObjectKey)
}
