use crate::application::ports::{
    ObjectKey, ObjectListPage, ObjectListRequest, ObjectMetadata, ObjectRange, ObjectRead,
    ObjectWriteOptions, RepositoryStorage, StorageCapabilities, StorageError, StorageResult,
    StorageVersion, StorageWriteCondition, VersionedObject, read_stream_to_vec,
};
use std::collections::{BTreeMap, VecDeque};
use std::fmt;
use std::io::{Cursor, Read};
use std::sync::{Arc, Mutex};

/// A storage operation that can be failed by [`MemoryStorage`] for contract
/// and fault-injection tests.
#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub enum MemoryStorageOperation {
    /// Whole-object reads.
    Read,
    /// Streaming writes, including unconditional writes.
    Write,
    /// Metadata reads.
    Metadata,
    /// Prefix listing.
    List,
    /// Object deletion.
    Delete,
    /// Range reads.
    Range,
    /// Conditional writes.
    ConditionalWrite,
}

/// An in-memory object-storage conformance backend for tests and embedded
/// callers.
///
/// Clones share one object map. Payloads are retained by the backend, but
/// reads hand out shared, bounded readers instead of cloning the complete
/// object. Conditional writes serialize their version check and publication.
#[derive(Clone, Default)]
pub struct MemoryStorage {
    state: Arc<Mutex<MemoryStorageState>>,
}

#[derive(Default)]
struct MemoryStorageState {
    objects: BTreeMap<String, StoredObject>,
    next_version: u64,
    failures: BTreeMap<MemoryStorageOperation, VecDeque<StorageError>>,
}

struct StoredObject {
    contents: Arc<[u8]>,
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
        let key = ObjectKey::new(object_key)?;
        let mut state = self.state.lock().map_err(|_| StorageError::Unavailable)?;
        let version = next_version(&mut state)?;
        state.objects.insert(
            key.into_string(),
            StoredObject {
                contents: Arc::from(contents.as_ref().to_vec()),
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
        let key = ObjectKey::new(object_key)?;
        let mut state = self.state.lock().map_err(|_| StorageError::Unavailable)?;
        Ok(state.objects.remove(key.as_str()).is_some())
    }

    /// Queues a provider-neutral failure for the next operation of `operation`.
    pub fn inject_failure(&self, operation: MemoryStorageOperation, error: StorageError) {
        if let Ok(mut state) = self.state.lock() {
            state
                .failures
                .entry(operation)
                .or_default()
                .push_back(error);
        }
    }

    /// Alias for [`Self::inject_failure`].
    pub fn fail_next(&self, operation: MemoryStorageOperation, error: StorageError) {
        self.inject_failure(operation, error);
    }

    /// Removes all queued injected failures.
    pub fn clear_injected_failures(&self) {
        if let Ok(mut state) = self.state.lock() {
            state.failures.clear();
        }
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
        let key = ObjectKey::new(object_key)?;
        let mut source = Cursor::new(contents);
        self.write_stream(&key, &mut source, ObjectWriteOptions::if_absent())
            .map(|_| ())
    }

    fn read(&self, object_key: &str) -> StorageResult<Vec<u8>> {
        let key = ObjectKey::new(object_key)?;
        let mut state = self.state.lock().map_err(|_| StorageError::Unavailable)?;
        if let Some(error) = take_failure(&mut state, MemoryStorageOperation::Read) {
            return Err(error);
        }
        state
            .objects
            .get(key.as_str())
            .map(|object| object.contents.as_ref().to_vec())
            .ok_or(StorageError::NotFound)
    }

    fn capabilities(&self) -> StorageCapabilities {
        StorageCapabilities::ALL
    }

    fn read_stream(&self, object_key: &ObjectKey) -> StorageResult<ObjectRead> {
        let mut state = self.state.lock().map_err(|_| StorageError::Unavailable)?;
        if let Some(error) = take_failure(&mut state, MemoryStorageOperation::Read) {
            return Err(error);
        }
        let object = state
            .objects
            .get(object_key.as_str())
            .ok_or(StorageError::NotFound)?;
        let metadata = metadata_for(object_key.clone(), object);
        let reader = SharedMemoryReader::new(object.contents.clone(), 0, object.contents.len());
        Ok(ObjectRead::new(metadata, reader))
    }

    fn read_range(&self, object_key: &ObjectKey, range: ObjectRange) -> StorageResult<ObjectRead> {
        let mut state = self.state.lock().map_err(|_| StorageError::Unavailable)?;
        if let Some(error) = take_failure(&mut state, MemoryStorageOperation::Range) {
            return Err(error);
        }
        let object = state
            .objects
            .get(object_key.as_str())
            .ok_or(StorageError::NotFound)?;
        let size = u64::try_from(object.contents.len()).map_err(|_| StorageError::InvalidRange)?;
        if range.end() > size {
            return Err(StorageError::InvalidRange);
        }
        let start = usize::try_from(range.start()).map_err(|_| StorageError::InvalidRange)?;
        let end = usize::try_from(range.end()).map_err(|_| StorageError::InvalidRange)?;
        let metadata = metadata_for(object_key.clone(), object);
        let reader = SharedMemoryReader::new(object.contents.clone(), start, end);
        Ok(ObjectRead::new(metadata, reader))
    }

    fn metadata(&self, object_key: &ObjectKey) -> StorageResult<ObjectMetadata> {
        let mut state = self.state.lock().map_err(|_| StorageError::Unavailable)?;
        if let Some(error) = take_failure(&mut state, MemoryStorageOperation::Metadata) {
            return Err(error);
        }
        state
            .objects
            .get(object_key.as_str())
            .map(|object| metadata_for(object_key.clone(), object))
            .ok_or(StorageError::NotFound)
    }

    fn write_stream(
        &self,
        object_key: &ObjectKey,
        source: &mut dyn Read,
        options: ObjectWriteOptions,
    ) -> StorageResult<ObjectMetadata> {
        let operation = match options.condition() {
            StorageWriteCondition::Any => MemoryStorageOperation::Write,
            StorageWriteCondition::IfAbsent | StorageWriteCondition::IfVersion(_) => {
                MemoryStorageOperation::ConditionalWrite
            }
        };
        {
            let mut state = self.state.lock().map_err(|_| StorageError::Unavailable)?;
            if let Some(error) = take_failure(&mut state, operation) {
                return Err(error);
            }
        }

        let contents = read_stream_to_vec(source, options.expected_size())?;
        let mut state = self.state.lock().map_err(|_| StorageError::Unavailable)?;
        let current = state.objects.get(object_key.as_str());
        match options.condition() {
            StorageWriteCondition::Any => {}
            StorageWriteCondition::IfAbsent if current.is_some() => {
                return Err(StorageError::AlreadyExists);
            }
            StorageWriteCondition::IfAbsent => {}
            StorageWriteCondition::IfVersion(expected)
                if current.is_some_and(|object| object.version == *expected) => {}
            StorageWriteCondition::IfVersion(_) => return Err(StorageError::Conflict),
        }
        let version = next_version(&mut state)?;
        let metadata = ObjectMetadata::new(
            object_key.clone(),
            u64::try_from(contents.len()).map_err(|_| StorageError::InvalidRequest)?,
            Some(version.clone()),
        );
        state.objects.insert(
            object_key.as_str().to_owned(),
            StoredObject {
                contents: Arc::from(contents),
                version,
            },
        );
        Ok(metadata)
    }

    fn delete(&self, object_key: &ObjectKey) -> StorageResult<()> {
        let mut state = self.state.lock().map_err(|_| StorageError::Unavailable)?;
        if let Some(error) = take_failure(&mut state, MemoryStorageOperation::Delete) {
            return Err(error);
        }
        state
            .objects
            .remove(object_key.as_str())
            .map(|_| ())
            .ok_or(StorageError::NotFound)
    }

    fn list_page(&self, request: &ObjectListRequest) -> StorageResult<ObjectListPage> {
        request.validate()?;
        let mut state = self.state.lock().map_err(|_| StorageError::Unavailable)?;
        if let Some(error) = take_failure(&mut state, MemoryStorageOperation::List) {
            return Err(error);
        }
        let prefix = request.prefix().as_str();
        let cursor = request.cursor().map(|value| value.as_str());
        let mut objects: Vec<ObjectMetadata> = Vec::with_capacity(request.limit());
        let mut next_cursor = None;
        for (key, object) in state.objects.iter() {
            if !matches_prefix(key, prefix) || cursor.is_some_and(|value| key.as_str() <= value) {
                continue;
            }
            if objects.len() == request.limit() {
                let last_key = objects
                    .last()
                    .map(|object| object.key().as_str().to_owned())
                    .ok_or(StorageError::Unavailable)?;
                next_cursor = Some(crate::application::ports::ObjectCursor::new(last_key)?);
                break;
            }
            objects.push(metadata_for(ObjectKey::new(key.clone())?, object));
        }
        Ok(ObjectListPage::new(objects, next_cursor))
    }

    fn read_with_version(&self, object_key: &str) -> StorageResult<VersionedObject> {
        let key = ObjectKey::new(object_key)?;
        let state = self.state.lock().map_err(|_| StorageError::Unavailable)?;
        let object = state
            .objects
            .get(key.as_str())
            .ok_or(StorageError::NotFound)?;
        Ok(VersionedObject::new(
            object.contents.as_ref().to_vec(),
            object.version.clone(),
        ))
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

fn metadata_for(key: ObjectKey, object: &StoredObject) -> ObjectMetadata {
    ObjectMetadata::new(
        key,
        u64::try_from(object.contents.len()).unwrap_or(u64::MAX),
        Some(object.version.clone()),
    )
}

fn matches_prefix(key: &str, prefix: &str) -> bool {
    prefix.is_empty()
        || key == prefix
        || key
            .strip_prefix(prefix)
            .is_some_and(|suffix| suffix.starts_with('/'))
}

fn next_version(state: &mut MemoryStorageState) -> StorageResult<StorageVersion> {
    let next = state
        .next_version
        .checked_add(1)
        .ok_or(StorageError::Unavailable)?;
    state.next_version = next;
    StorageVersion::from_bytes(next.to_be_bytes())
}

fn take_failure(
    state: &mut MemoryStorageState,
    operation: MemoryStorageOperation,
) -> Option<StorageError> {
    let failure = state
        .failures
        .get_mut(&operation)
        .and_then(VecDeque::pop_front);
    if state
        .failures
        .get(&operation)
        .is_some_and(VecDeque::is_empty)
    {
        state.failures.remove(&operation);
    }
    failure
}

struct SharedMemoryReader {
    contents: Arc<[u8]>,
    position: usize,
    end: usize,
}

impl SharedMemoryReader {
    fn new(contents: Arc<[u8]>, start: usize, end: usize) -> Self {
        Self {
            contents,
            position: start,
            end,
        }
    }
}

impl Read for SharedMemoryReader {
    fn read(&mut self, buffer: &mut [u8]) -> std::io::Result<usize> {
        if self.position >= self.end || buffer.is_empty() {
            return Ok(0);
        }
        let available = self.end - self.position;
        let amount = available.min(buffer.len());
        buffer[..amount].copy_from_slice(&self.contents[self.position..self.position + amount]);
        self.position += amount;
        Ok(amount)
    }
}
