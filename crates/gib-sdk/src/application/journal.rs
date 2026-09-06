use super::ports::{
    ObjectKey, ObjectListRequest, ObjectPrefix, ObjectWriteOptions, RepositoryStorage,
    StorageCapabilities, StorageError, StorageVersion, read_stream_to_vec,
};
use crate::domain::ObjectKind;
use crate::format::{
    FormatError, MAX_OPERATION_JOURNAL_BYTES, MAX_OPERATION_JOURNAL_COMPLETED_OBJECTS,
    OperationJournalCheckpoint, OperationJournalData, OperationJournalObject,
    OperationJournalState, decode_operation_journal, encode_operation_journal,
};
use sha2::{Digest, Sha256};
use std::io::Cursor;
use std::sync::{Arc, Mutex, MutexGuard};

/// The logical prefix containing resumable operation journals.
pub(crate) const OPERATION_JOURNAL_PREFIX: &str = "operations";

const OPERATION_JOURNAL_IDENTIFIER_PREFIX: &str = "op-";
const JOURNAL_RANDOM_SUFFIX_BYTES: usize = 16;
const JOURNAL_LIST_PAGE_SIZE: usize = 100;
const JOURNAL_READ_BUFFER_BYTES: usize = 64 * 1024;

/// A failure while reading, validating, or updating a resumable operation
/// journal. The type deliberately contains no backend error or path data.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum JournalError {
    NotFound,
    AlreadyExists,
    Malformed,
    UnsupportedVersion { version: u16 },
    Incompatible,
    Stale,
    AlreadyPublished,
    SourceChanged,
    TooLarge,
    EncryptionKeyRequired,
    Cancelled,
    Storage { operation: &'static str },
}

/// One decoded journal together with the backend version used for CAS writes.
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct LoadedOperationJournal {
    pub(crate) key: String,
    pub(crate) data: OperationJournalData,
    pub(crate) version: StorageVersion,
}

/// The result of one bounded pending-journal listing.
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct PendingOperationPage {
    pub(crate) operations: Vec<PendingOperationInfo>,
    pub(crate) next_cursor: Option<crate::application::ports::ObjectCursor>,
}

/// The parse status retained for a pending-list row.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum PendingOperationStatus {
    Active,
    Corrupt,
    UnsupportedVersion,
    Encrypted,
    TooLarge,
}

/// Safe metadata projected from one journal without exposing its payload.
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct PendingOperationInfo {
    pub(crate) identifier: String,
    pub(crate) status: PendingOperationStatus,
    pub(crate) object_size: u64,
    pub(crate) operation_id: Option<u64>,
    pub(crate) repository_format_version: Option<u16>,
    pub(crate) base_generation: Option<u64>,
    pub(crate) base_snapshot: Option<String>,
    pub(crate) source_root: Option<String>,
    pub(crate) checkpoint: Option<OperationJournalCheckpoint>,
    pub(crate) completed_objects: usize,
    pub(crate) target_snapshot: Option<String>,
    pub(crate) created_at: Option<u64>,
    pub(crate) updated_at: Option<u64>,
}

/// Creates a collision-resistant, non-secret journal identifier for an SDK
/// operation. The operation number remains visible for event correlation;
/// the random suffix prevents process-local operation counters from colliding
/// after a restart.
pub(crate) fn new_journal_identifier(operation_id: u64) -> Result<String, JournalError> {
    let mut suffix = [0_u8; JOURNAL_RANDOM_SUFFIX_BYTES];
    getrandom::getrandom(&mut suffix).map_err(|_| JournalError::Storage {
        operation: "generate_journal_identifier",
    })?;
    Ok(format!(
        "{OPERATION_JOURNAL_PREFIX}/{OPERATION_JOURNAL_IDENTIFIER_PREFIX}{operation_id}-{}",
        hex_encode(&suffix)
    ))
}

/// Creates and atomically publishes a new active journal.
pub(crate) fn create_journal(
    storage: Arc<dyn RepositoryStorage>,
    key: String,
    data: OperationJournalData,
    encryption: Option<crate::format::EncryptionContext>,
    is_cancelled: &dyn Fn() -> bool,
) -> Result<Arc<JournalRuntime>, JournalError> {
    if is_cancelled() {
        return Err(JournalError::Cancelled);
    }
    let key = validate_journal_key(&key)?;
    ensure_journal_capabilities(storage.as_ref())?;
    let encoded = encode_operation_journal(&data, encryption.as_ref()).map_err(map_format_error)?;
    let mut source = Cursor::new(&encoded);
    let metadata = storage
        .write_stream_with_cancellation(
            &key,
            &mut source,
            ObjectWriteOptions::if_absent().with_expected_size(encoded.len() as u64),
            is_cancelled,
        )
        .map_err(|error| map_storage_error(error, "create_journal"))?;
    let version = metadata.version().cloned().ok_or(JournalError::Storage {
        operation: "create_journal",
    })?;
    Ok(Arc::new(JournalRuntime {
        storage,
        key,
        encryption,
        state: Mutex::new(JournalRuntimeState { data, version }),
    }))
}

/// Loads one journal by its full identifier or by its visible `op-N` number.
pub(crate) fn load_journal(
    storage: Arc<dyn RepositoryStorage>,
    identifier: &str,
    encryption: Option<crate::format::EncryptionContext>,
) -> Result<LoadedOperationJournal, JournalError> {
    if let Some(key) = exact_journal_key(identifier) {
        return read_journal(&storage, key, encryption.as_ref());
    }
    let Some(operation_id) = parse_operation_number(identifier) else {
        return Err(JournalError::NotFound);
    };
    let prefix = ObjectPrefix::new(OPERATION_JOURNAL_PREFIX.to_owned())
        .map_err(|_| JournalError::Malformed)?;
    let mut request = ObjectListRequest::new(prefix).with_limit(JOURNAL_LIST_PAGE_SIZE);
    let mut found = None;
    loop {
        let page = storage
            .list_page(&request)
            .map_err(|error| map_storage_error(error, "list_journals"))?;
        let (objects, cursor) = page.into_parts();
        for object in objects {
            let key = object.key().as_str();
            if !key.starts_with("operations/op-") {
                continue;
            }
            let loaded =
                match read_journal(storage.as_ref(), object.key().clone(), encryption.as_ref()) {
                    Ok(loaded) => loaded,
                    Err(JournalError::NotFound) => continue,
                    Err(error) => return Err(error),
                };
            if loaded.data.operation_id != operation_id {
                continue;
            }
            if found.is_some() {
                return Err(JournalError::Incompatible);
            }
            found = Some(loaded);
        }
        let Some(cursor) = cursor else {
            break;
        };
        request = request.with_cursor(cursor);
    }
    found.ok_or(JournalError::NotFound)
}

/// Lists only the dedicated journal prefix. The caller can page the result
/// without enumerating immutable packs, indexes, trees, or snapshots.
pub(crate) fn list_pending_operations(
    storage: &dyn RepositoryStorage,
    request: &ObjectListRequest,
    encryption: Option<&crate::format::EncryptionContext>,
) -> Result<PendingOperationPage, JournalError> {
    if request.prefix().as_str() != OPERATION_JOURNAL_PREFIX {
        return Err(JournalError::Malformed);
    }
    if request.limit() > JOURNAL_LIST_PAGE_SIZE {
        return Err(JournalError::TooLarge);
    }
    let page = storage
        .list_page(request)
        .map_err(|error| map_storage_error(error, "list_journals"))?;
    let (objects, next_cursor) = page.into_parts();
    let mut operations = Vec::new();
    operations
        .try_reserve(objects.len())
        .map_err(|_| JournalError::TooLarge)?;
    for object in objects {
        let key = object.key().as_str();
        if !key.starts_with("operations/op-") {
            continue;
        }
        let identifier = key
            .strip_prefix("operations/")
            .ok_or(JournalError::Malformed)?
            .to_owned();
        if object.size() > MAX_OPERATION_JOURNAL_BYTES as u64 {
            operations.push(PendingOperationInfo {
                identifier,
                status: PendingOperationStatus::TooLarge,
                object_size: object.size(),
                operation_id: None,
                repository_format_version: None,
                base_generation: None,
                base_snapshot: None,
                source_root: None,
                checkpoint: None,
                completed_objects: 0,
                target_snapshot: None,
                created_at: None,
                updated_at: None,
            });
            continue;
        }
        let loaded = match read_journal(storage, object.key().clone(), encryption) {
            Ok(loaded) => loaded,
            Err(JournalError::NotFound) => continue,
            Err(JournalError::EncryptionKeyRequired) => {
                operations.push(PendingOperationInfo {
                    identifier,
                    status: PendingOperationStatus::Encrypted,
                    object_size: object.size(),
                    operation_id: None,
                    repository_format_version: None,
                    base_generation: None,
                    base_snapshot: None,
                    source_root: None,
                    checkpoint: None,
                    completed_objects: 0,
                    target_snapshot: None,
                    created_at: None,
                    updated_at: None,
                });
                continue;
            }
            Err(JournalError::UnsupportedVersion { .. }) => {
                operations.push(PendingOperationInfo {
                    identifier,
                    status: PendingOperationStatus::UnsupportedVersion,
                    object_size: object.size(),
                    operation_id: None,
                    repository_format_version: None,
                    base_generation: None,
                    base_snapshot: None,
                    source_root: None,
                    checkpoint: None,
                    completed_objects: 0,
                    target_snapshot: None,
                    created_at: None,
                    updated_at: None,
                });
                continue;
            }
            Err(JournalError::TooLarge) => {
                operations.push(PendingOperationInfo {
                    identifier,
                    status: PendingOperationStatus::TooLarge,
                    object_size: object.size(),
                    operation_id: None,
                    repository_format_version: None,
                    base_generation: None,
                    base_snapshot: None,
                    source_root: None,
                    checkpoint: None,
                    completed_objects: 0,
                    target_snapshot: None,
                    created_at: None,
                    updated_at: None,
                });
                continue;
            }
            Err(_) => {
                operations.push(PendingOperationInfo {
                    identifier,
                    status: PendingOperationStatus::Corrupt,
                    object_size: object.size(),
                    operation_id: None,
                    repository_format_version: None,
                    base_generation: None,
                    base_snapshot: None,
                    source_root: None,
                    checkpoint: None,
                    completed_objects: 0,
                    target_snapshot: None,
                    created_at: None,
                    updated_at: None,
                });
                continue;
            }
        };
        if loaded.data.state == OperationJournalState::Complete {
            continue;
        }
        operations.push(PendingOperationInfo {
            identifier,
            status: PendingOperationStatus::Active,
            object_size: object.size(),
            operation_id: Some(loaded.data.operation_id),
            repository_format_version: Some(loaded.data.repository_format_version),
            base_generation: Some(loaded.data.base_generation),
            base_snapshot: loaded.data.base_snapshot,
            source_root: Some(loaded.data.source_root),
            checkpoint: Some(loaded.data.checkpoint),
            completed_objects: loaded.data.completed_objects.len(),
            target_snapshot: loaded.data.target_snapshot,
            created_at: Some(loaded.data.created_at),
            updated_at: Some(loaded.data.updated_at),
        });
    }
    Ok(PendingOperationPage {
        operations,
        next_cursor,
    })
}

/// Runtime state shared by all upload workers for one backup.
pub(crate) struct JournalRuntime {
    storage: Arc<dyn RepositoryStorage>,
    key: ObjectKey,
    encryption: Option<crate::format::EncryptionContext>,
    state: Mutex<JournalRuntimeState>,
}

struct JournalRuntimeState {
    data: OperationJournalData,
    version: StorageVersion,
}

impl JournalRuntime {
    /// Creates a mutable runtime from a journal read before the worker starts.
    pub(crate) fn from_loaded(
        storage: Arc<dyn RepositoryStorage>,
        loaded: LoadedOperationJournal,
        encryption: Option<crate::format::EncryptionContext>,
    ) -> Result<Arc<Self>, JournalError> {
        let key = validate_journal_key(&loaded.key)?;
        ensure_journal_capabilities(storage.as_ref())?;
        Ok(Arc::new(Self {
            storage,
            key,
            encryption,
            state: Mutex::new(JournalRuntimeState {
                data: loaded.data,
                version: loaded.version,
            }),
        }))
    }

    /// Returns a clone of the current advisory state for resume decisions.
    pub(crate) fn data(&self) -> OperationJournalData {
        lock_or_recover(&self.state).data.clone()
    }

    /// Durably records the source assumption after the bounded preflight scan.
    /// A journal created before that scan has no completed objects and can be
    /// safely finalized by a later resume attempt.
    pub(crate) fn set_source_fingerprint(
        &self,
        source_root: &str,
        source_fingerprint: [u8; 32],
        is_cancelled: &dyn Fn() -> bool,
    ) -> Result<(), JournalError> {
        self.update(is_cancelled, |data| {
            if data.source_root != source_root || data.source_fingerprint_ready {
                return Err(JournalError::Incompatible);
            }
            if !data.completed_objects.is_empty() || data.target_snapshot.is_some() {
                return Err(JournalError::Malformed);
            }
            data.source_fingerprint = source_fingerprint;
            data.source_fingerprint_ready = true;
            data.checkpoint = OperationJournalCheckpoint {
                stage: 0,
                sequence: data.checkpoint.sequence,
            };
            Ok(())
        })
    }

    /// Revalidates every recorded object and drops records whose immutable
    /// object is absent. A present object with different bytes is corruption,
    /// never a candidate for silent replacement.
    pub(crate) fn verify_completed(
        &self,
        is_cancelled: &dyn Fn() -> bool,
    ) -> Result<(), JournalError> {
        let data = self.data();
        if !data.source_fingerprint_ready {
            return Err(JournalError::Incompatible);
        }
        let mut retained = Vec::new();
        retained
            .try_reserve(data.completed_objects.len())
            .map_err(|_| JournalError::TooLarge)?;
        let mut removed = false;
        for object in data.completed_objects {
            if is_cancelled() {
                return Err(JournalError::Cancelled);
            }
            match verify_stored_object(self.storage.as_ref(), &object, is_cancelled)? {
                StoredObjectState::Present => retained.push(object),
                StoredObjectState::Missing => removed = true,
            }
        }
        if removed {
            self.update(is_cancelled, |data| {
                data.completed_objects = retained;
                Ok(())
            })?;
        }
        Ok(())
    }

    /// Revalidates one recorded object before an uploader relies on the
    /// checkpoint. A missing immutable object is removed from the advisory
    /// set so the normal create-if-absent path can rebuild it.
    pub(crate) fn is_verified_completed(
        &self,
        key: &str,
        size: u64,
        digest: [u8; 32],
        is_cancelled: &dyn Fn() -> bool,
    ) -> Result<bool, JournalError> {
        let record = {
            let state = lock_or_recover(&self.state);
            state
                .data
                .completed_objects
                .iter()
                .find(|record| record.key == key)
                .cloned()
        };
        let Some(record) = record else {
            return Ok(false);
        };
        if record.size != size || record.digest != digest {
            return Err(JournalError::Malformed);
        }
        match verify_stored_object(self.storage.as_ref(), &record, is_cancelled)? {
            StoredObjectState::Present => Ok(true),
            StoredObjectState::Missing => {
                self.update(is_cancelled, |data| {
                    data.completed_objects.retain(|entry| entry.key != key);
                    Ok(())
                })?;
                Ok(false)
            }
        }
    }

    /// Records an immutable object only after its storage write has completed
    /// and its content digest is known.
    pub(crate) fn record_completed(
        &self,
        stage: u8,
        key: &str,
        size: u64,
        digest: [u8; 32],
        is_cancelled: &dyn Fn() -> bool,
    ) -> Result<(), JournalError> {
        let key = ObjectKey::new(key.to_owned()).map_err(|_| JournalError::Malformed)?;
        if !is_immutable_object_key(key.as_str()) {
            return Err(JournalError::Malformed);
        }
        self.update(is_cancelled, |data| {
            if let Some(existing) = data
                .completed_objects
                .iter()
                .find(|existing| existing.key == key.as_str())
            {
                if existing.size == size && existing.digest == digest {
                    return Ok(());
                }
                return Err(JournalError::Malformed);
            }
            if data.completed_objects.len() >= MAX_OPERATION_JOURNAL_COMPLETED_OBJECTS {
                return Err(JournalError::TooLarge);
            }
            data.completed_objects.push(OperationJournalObject {
                key: key.as_str().to_owned(),
                size,
                digest,
            });
            data.checkpoint = OperationJournalCheckpoint {
                stage,
                sequence: data.checkpoint.sequence.saturating_add(1),
            };
            Ok(())
        })
    }

    /// Stores the snapshot target before the final HEAD CAS.
    pub(crate) fn set_target_snapshot(
        &self,
        snapshot: &str,
        is_cancelled: &dyn Fn() -> bool,
    ) -> Result<(), JournalError> {
        if snapshot.is_empty() {
            return Err(JournalError::Malformed);
        }
        self.update(is_cancelled, |data| {
            if data
                .target_snapshot
                .as_deref()
                .is_some_and(|existing| existing != snapshot)
            {
                return Err(JournalError::Malformed);
            }
            data.target_snapshot = Some(snapshot.to_owned());
            data.checkpoint = OperationJournalCheckpoint {
                stage: 9,
                sequence: data.checkpoint.sequence.saturating_add(1),
            };
            Ok(())
        })
    }

    /// Marks the journal complete only after publication has succeeded. A
    /// failed delete leaves a terminal marker, which is excluded from active
    /// pending listings and is safe to retry or remove later.
    pub(crate) fn complete(&self) -> Result<(), JournalError> {
        // Publication is the commit boundary. Once HEAD has advanced, the
        // terminal journal update must not be skipped merely because the
        // caller requested cancellation in the same interval.
        self.update(&|| false, |data| {
            data.state = OperationJournalState::Complete;
            Ok(())
        })?;
        let _ = self.storage.delete(&self.key);
        Ok(())
    }

    fn update(
        &self,
        is_cancelled: &dyn Fn() -> bool,
        mutate: impl FnOnce(&mut OperationJournalData) -> Result<(), JournalError>,
    ) -> Result<(), JournalError> {
        if is_cancelled() {
            return Err(JournalError::Cancelled);
        }
        let mut state = lock_or_recover(&self.state);
        let mut data = state.data.clone();
        mutate(&mut data)?;
        data.updated_at = current_unix_seconds();
        let encoded =
            encode_operation_journal(&data, self.encryption.as_ref()).map_err(map_format_error)?;
        let mut source = Cursor::new(&encoded);
        let metadata = self
            .storage
            .write_stream_with_cancellation(
                &self.key,
                &mut source,
                ObjectWriteOptions::if_version(state.version.clone())
                    .with_expected_size(encoded.len() as u64),
                is_cancelled,
            )
            .map_err(|error| map_storage_error(error, "update_journal"))?;
        let version = metadata.version().cloned().ok_or(JournalError::Storage {
            operation: "update_journal",
        })?;
        state.data = data;
        state.version = version;
        Ok(())
    }
}

enum StoredObjectState {
    Present,
    Missing,
}

fn verify_stored_object(
    storage: &dyn RepositoryStorage,
    object: &OperationJournalObject,
    is_cancelled: &dyn Fn() -> bool,
) -> Result<StoredObjectState, JournalError> {
    if is_cancelled() {
        return Err(JournalError::Cancelled);
    }
    let key = ObjectKey::new(object.key.clone()).map_err(|_| JournalError::Malformed)?;
    let mut stored = match storage.read_stream(&key) {
        Ok(stored) => stored,
        Err(StorageError::NotFound) => return Ok(StoredObjectState::Missing),
        Err(error) => return Err(map_storage_error(error, "verify_journal_object")),
    };
    if stored.metadata().size() != object.size {
        return Err(JournalError::Malformed);
    }
    let mut digest = Sha256::new();
    let mut buffer = [0_u8; JOURNAL_READ_BUFFER_BYTES];
    loop {
        if is_cancelled() {
            return Err(JournalError::Cancelled);
        }
        let read = stored
            .reader()
            .read(&mut buffer)
            .map_err(|_| JournalError::Storage {
                operation: "verify_journal_object",
            })?;
        if read == 0 {
            break;
        }
        digest.update(&buffer[..read]);
    }
    let actual: [u8; 32] = digest.finalize().into();
    if actual != object.digest {
        return Err(JournalError::Malformed);
    }
    Ok(StoredObjectState::Present)
}

fn read_journal(
    storage: &dyn RepositoryStorage,
    key: ObjectKey,
    encryption: Option<&crate::format::EncryptionContext>,
) -> Result<LoadedOperationJournal, JournalError> {
    let key = validate_journal_key(key.as_str())?;
    let mut object = match storage.read_stream(&key) {
        Ok(object) => object,
        Err(error) => return Err(map_storage_error(error, "read_journal")),
    };
    let size = object.metadata().size();
    if size > MAX_OPERATION_JOURNAL_BYTES as u64 {
        return Err(JournalError::TooLarge);
    }
    let bytes = read_stream_to_vec(object.reader(), Some(size))
        .map_err(|error| map_storage_error(error, "read_journal"))?;
    let data = decode_operation_journal(&bytes, encryption).map_err(map_format_error)?;
    let identifier = key
        .as_str()
        .strip_prefix("operations/")
        .ok_or(JournalError::Malformed)?;
    if parse_operation_number(identifier) != Some(data.operation_id) {
        return Err(JournalError::Malformed);
    }
    let version = object
        .metadata()
        .version()
        .cloned()
        .ok_or(JournalError::Storage {
            operation: "read_journal",
        })?;
    Ok(LoadedOperationJournal {
        key: key.into_string(),
        data,
        version,
    })
}

fn exact_journal_key(identifier: &str) -> Option<ObjectKey> {
    let value = identifier.strip_prefix("operations/").unwrap_or(identifier);
    if !value.starts_with(OPERATION_JOURNAL_IDENTIFIER_PREFIX)
        || value.contains('/')
        || value.len() > ObjectKey::MAX_LENGTH
        || !value
            .strip_prefix(OPERATION_JOURNAL_IDENTIFIER_PREFIX)
            .is_some_and(|suffix| suffix.contains('-'))
    {
        return None;
    }
    ObjectKey::new(format!("{OPERATION_JOURNAL_PREFIX}/{value}")).ok()
}

fn parse_operation_number(identifier: &str) -> Option<u64> {
    let value = identifier.strip_prefix(OPERATION_JOURNAL_IDENTIFIER_PREFIX)?;
    let number = value.split('-').next()?;
    number.parse().ok()
}

fn validate_journal_key(value: &str) -> Result<ObjectKey, JournalError> {
    let key = ObjectKey::new(value.to_owned()).map_err(|_| JournalError::Malformed)?;
    let identifier = key
        .as_str()
        .strip_prefix("operations/")
        .ok_or(JournalError::Malformed)?;
    if exact_journal_key(identifier).as_ref() != Some(&key) {
        return Err(JournalError::Malformed);
    }
    Ok(key)
}

fn is_immutable_object_key(value: &str) -> bool {
    [
        ObjectKind::Snapshot.storage_prefix(),
        ObjectKind::Tree.storage_prefix(),
        ObjectKind::Pack.storage_prefix(),
        ObjectKind::Index.storage_prefix(),
    ]
    .iter()
    .any(|prefix| value.starts_with(&format!("{prefix}/")))
}

fn ensure_journal_capabilities(storage: &dyn RepositoryStorage) -> Result<(), JournalError> {
    let required = StorageCapabilities::STREAMING_WRITE | StorageCapabilities::CONDITIONAL_WRITE;
    if !storage.capabilities().contains(required) {
        return Err(JournalError::Incompatible);
    }
    Ok(())
}

fn map_storage_error(error: StorageError, operation: &'static str) -> JournalError {
    match error {
        StorageError::NotFound => JournalError::NotFound,
        StorageError::AlreadyExists => JournalError::AlreadyExists,
        StorageError::InvalidObjectKey | StorageError::InvalidVersion => JournalError::Malformed,
        StorageError::UnsupportedCapability => JournalError::Incompatible,
        StorageError::Cancelled => JournalError::Cancelled,
        StorageError::Conflict | StorageError::ConditionNotMet => {
            JournalError::Storage { operation }
        }
        _ => JournalError::Storage { operation },
    }
}

fn map_format_error(error: FormatError) -> JournalError {
    match error {
        FormatError::UnsupportedVersion { version }
        | FormatError::UnsupportedObjectVersion { version } => {
            JournalError::UnsupportedVersion { version }
        }
        FormatError::EncryptionKeyRequired => JournalError::EncryptionKeyRequired,
        FormatError::InputTooLarge => JournalError::TooLarge,
        FormatError::Serialization | FormatError::RandomnessFailure => JournalError::Storage {
            operation: "encode_journal",
        },
        _ => JournalError::Malformed,
    }
}

fn current_unix_seconds() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |duration| duration.as_secs())
}

fn hex_encode(bytes: &[u8]) -> String {
    const HEX: &[u8; 16] = b"0123456789abcdef";
    let mut output = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        output.push(char::from(HEX[usize::from(byte >> 4)]));
        output.push(char::from(HEX[usize::from(byte & 0x0f)]));
    }
    output
}

fn lock_or_recover<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    match mutex.lock() {
        Ok(guard) => guard,
        Err(poisoned) => poisoned.into_inner(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn operation_numbers_are_parsed_without_accepting_a_path() {
        assert_eq!(parse_operation_number("op-42"), Some(42));
        assert_eq!(parse_operation_number("op-42-deadbeef"), Some(42));
        assert_eq!(parse_operation_number("operations/op-42"), None);
        assert_eq!(parse_operation_number("other-42"), None);
    }

    #[test]
    fn immutable_completion_keys_exclude_journals() {
        assert!(is_immutable_object_key("packs/id"));
        assert!(is_immutable_object_key("indexes/id"));
        assert!(!is_immutable_object_key("operations/op-1"));
    }
}
