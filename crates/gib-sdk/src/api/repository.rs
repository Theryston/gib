use super::error::{SdkError, SdkResult};
use super::operation::CancellationToken;
use crate::application::repository::{
    HeadRead as ApplicationHeadRead, RepositoryError, RepositoryOpenExpectations,
    initialize_repository as initialize_use_case,
    list_snapshot_summaries as list_snapshot_summaries_use_case, open_repository as open_use_case,
    publish_head as publish_head_use_case, read_head as read_head_use_case,
    read_snapshot_summary as read_snapshot_summary_use_case,
    rebuild_snapshot_summaries as rebuild_snapshot_summaries_use_case,
    resolve_snapshot_reference as resolve_snapshot_reference_use_case,
};
use crate::domain::DomainError;
use crate::format::{FormatError, decode_snapshot, encode_snapshot};
use std::fmt;
use std::sync::Arc;

pub use crate::application::ports::{
    ByteRange, DEFAULT_OBJECT_LIST_PAGE_SIZE, ListCursor, MAX_OBJECT_LIST_PAGE_SIZE, ObjectCursor,
    ObjectKey, ObjectListPage, ObjectListRequest, ObjectMetadata, ObjectPrefix, ObjectRange,
    ObjectRead, ObjectReader, ObjectStorage, ObjectWriteOptions, RepositoryStorage,
    STORAGE_TRANSFER_BUFFER_SIZE, StorageCapabilities, StorageCapability, StorageError, StorageKey,
    StorageListPage, StorageListRequest, StorageMetadata, StoragePrefix, StorageRange,
    StorageReader, StorageResult, StorageVersion, StorageVersionToken, StorageWriteCondition,
    StorageWriteOptions, VersionToken, VersionedObject, VersionedStorageObject, WriteCondition,
};
pub use crate::domain::{
    BackupReference, CURRENT_SNAPSHOT_HISTORY_VERSION, CURRENT_SNAPSHOT_SUMMARY_VERSION,
    CURRENT_SNAPSHOT_VERSION, DEFAULT_SNAPSHOT_PAGE_SIZE, LATEST_SNAPSHOT_ALIAS,
    MAX_SNAPSHOT_AUTHOR_LENGTH, MAX_SNAPSHOT_CURSOR_LENGTH, MAX_SNAPSHOT_ID_LENGTH,
    MAX_SNAPSHOT_MESSAGE_LENGTH, MAX_SNAPSHOT_PAGE_SIZE, SNAPSHOT_HISTORY_OBJECT_PREFIX,
    SNAPSHOT_OBJECT_PREFIX, Snapshot, SnapshotCursor, SnapshotHistoryPage, SnapshotHistoryRequest,
    SnapshotId, SnapshotListRequest, SnapshotPage, SnapshotRef, SnapshotReferenceInput,
    SnapshotReferenceSelector, SnapshotSelector, SnapshotSummary, SnapshotSummaryListRequest,
    SnapshotSummaryPage,
};
pub use crate::domain::{
    CURRENT_REPOSITORY_BOOTSTRAP_VERSION, CURRENT_REPOSITORY_DESCRIPTOR_VERSION,
    CURRENT_REPOSITORY_FORMAT_VERSION, CURRENT_REPOSITORY_HEAD_VERSION, FORMAT_OBJECT_KEY,
    HEAD_OBJECT_KEY, LATEST_REF_OBJECT_KEY, REPOSITORY_DESCRIPTOR_OBJECT_KEY, REPOSITORY_HEAD_KEY,
    REPOSITORY_HEAD_OBJECT_KEY, REPOSITORY_HEAD_VERSION, REPOSITORY_MAGIC,
    REQUIRED_REPOSITORY_FEATURE,
};
pub use crate::domain::{
    Head, HeadPublication, RepositoryDescriptor, RepositoryFeature, RepositoryHead, RepositoryId,
    RepositoryIdentity, RepositoryKey, RepositoryObject, RepositoryRoots, SnapshotPublication,
    SnapshotPublicationRequest, SnapshotReference,
};
#[cfg(feature = "s3")]
pub use crate::infrastructure::storage::{
    DEFAULT_S3_MAX_CONCURRENCY, DEFAULT_S3_MULTIPART_PART_SIZE, DEFAULT_S3_MULTIPART_THRESHOLD,
    MAX_S3_MULTIPART_PART_SIZE, MAX_S3_MULTIPART_THRESHOLD, MAX_S3_MULTIPART_UPLOAD_PARTS,
    MIN_S3_MULTIPART_PART_SIZE, S3Storage, S3StorageConfig,
};
pub use crate::infrastructure::storage::{
    LocalStorage, LocalStorageOperation, MemoryStorage, MemoryStorageOperation,
};

/// A cloneable type-erased handle for a repository storage backend.
#[derive(Clone)]
pub struct StorageHandle {
    inner: Arc<dyn RepositoryStorage>,
}

impl StorageHandle {
    /// Wraps a thread-safe storage backend in a shareable handle.
    pub fn new<S>(storage: S) -> Self
    where
        S: RepositoryStorage + 'static,
    {
        Self {
            inner: Arc::new(storage),
        }
    }

    /// Wraps an existing type-erased storage backend.
    pub fn from_arc(storage: Arc<dyn RepositoryStorage>) -> Self {
        Self { inner: storage }
    }

    /// Returns the backend object used by the repository handle.
    pub fn as_storage(&self) -> &dyn RepositoryStorage {
        self.inner.as_ref()
    }
}

impl<S> From<S> for StorageHandle
where
    S: RepositoryStorage + 'static,
{
    fn from(storage: S) -> Self {
        Self::new(storage)
    }
}

impl From<&MemoryStorage> for StorageHandle {
    fn from(storage: &MemoryStorage) -> Self {
        Self::new(storage.clone())
    }
}

impl From<&LocalStorage> for StorageHandle {
    fn from(storage: &LocalStorage) -> Self {
        Self::new(storage.clone())
    }
}

#[cfg(feature = "s3")]
impl From<&S3Storage> for StorageHandle {
    fn from(storage: &S3Storage) -> Self {
        Self::new(storage.clone())
    }
}

impl fmt::Debug for StorageHandle {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("StorageHandle(..)")
    }
}

/// Validated input for repository initialization.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct RepositoryInitRequest {
    identity: RepositoryIdentity,
    repository_key: RepositoryKey,
}

impl RepositoryInitRequest {
    /// Creates an initialization request from validated domain values.
    pub const fn new(identity: RepositoryIdentity, repository_key: RepositoryKey) -> Self {
        Self {
            identity,
            repository_key,
        }
    }

    /// Creates an initialization request by validating string values.
    pub fn from_values(
        identity: impl Into<String>,
        repository_key: impl Into<String>,
    ) -> SdkResult<Self> {
        Ok(Self::new(
            RepositoryIdentity::new(identity)?,
            RepositoryKey::new(repository_key)?,
        ))
    }

    /// Returns the identity to persist in the new repository.
    pub const fn identity(&self) -> &RepositoryIdentity {
        &self.identity
    }

    /// Alias for [`Self::identity`] using the repository-ID terminology.
    pub const fn repository_id(&self) -> &RepositoryIdentity {
        self.identity()
    }

    /// Returns the namespace key to persist in the new repository.
    pub const fn repository_key(&self) -> &RepositoryKey {
        &self.repository_key
    }

    /// Consumes the request and returns its validated values.
    pub fn into_parts(self) -> (RepositoryIdentity, RepositoryKey) {
        (self.identity, self.repository_key)
    }
}

/// Compatibility alias for callers that use the longer request name.
pub type RepositoryInitializationRequest = RepositoryInitRequest;

/// Compatibility alias for callers that use an imperative request name.
pub type InitializeRepositoryRequest = RepositoryInitRequest;

/// Validated optional expectations used when opening a repository.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct RepositoryOpenRequest {
    expected_identity: Option<RepositoryIdentity>,
    expected_repository_key: Option<RepositoryKey>,
}

impl RepositoryOpenRequest {
    /// Creates an open request that accepts any identity and repository key
    /// after the persisted descriptor has been validated.
    pub const fn new() -> Self {
        Self {
            expected_identity: None,
            expected_repository_key: None,
        }
    }

    /// Creates an open request that verifies the persisted repository identity.
    pub fn for_identity(identity: RepositoryIdentity) -> Self {
        Self {
            expected_identity: Some(identity),
            expected_repository_key: None,
        }
    }

    /// Creates an open request that verifies both persisted identity values.
    pub fn for_repository(identity: RepositoryIdentity, repository_key: RepositoryKey) -> Self {
        Self {
            expected_identity: Some(identity),
            expected_repository_key: Some(repository_key),
        }
    }

    /// Sets an expected identity for the open operation.
    pub fn with_identity(mut self, identity: RepositoryIdentity) -> Self {
        self.expected_identity = Some(identity);
        self
    }

    /// Sets an expected key for the open operation.
    pub fn with_repository_key(mut self, repository_key: RepositoryKey) -> Self {
        self.expected_repository_key = Some(repository_key);
        self
    }

    /// Alias for [`Self::with_identity`].
    pub fn identity(self, identity: RepositoryIdentity) -> Self {
        self.with_identity(identity)
    }

    /// Alias for [`Self::with_repository_key`].
    pub fn repository_key(self, repository_key: RepositoryKey) -> Self {
        self.with_repository_key(repository_key)
    }

    /// Returns the expected identity, when one was configured.
    pub const fn expected_identity(&self) -> Option<&RepositoryIdentity> {
        self.expected_identity.as_ref()
    }

    /// Returns the expected key, when one was configured.
    pub const fn expected_repository_key(&self) -> Option<&RepositoryKey> {
        self.expected_repository_key.as_ref()
    }
}

impl Default for RepositoryOpenRequest {
    fn default() -> Self {
        Self::new()
    }
}

/// Compatibility alias for callers that use an imperative request name.
pub type OpenRepositoryRequest = RepositoryOpenRequest;

/// A repository HEAD value together with the storage token observed with it.
///
/// The token is part of the publication precondition. Callers should retain
/// this value and pass the same instance to a publication attempt; a later
/// attempt can explicitly read a fresh value after receiving a conflict.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct HeadState {
    head: RepositoryHead,
    version: Option<StorageVersion>,
}

impl HeadState {
    fn from_read(read: ApplicationHeadRead) -> Self {
        Self {
            head: read.head,
            version: read.version,
        }
    }

    /// Returns the validated domain HEAD value.
    pub fn head(&self) -> &RepositoryHead {
        &self.head
    }

    /// Alias for [`Self::head`].
    pub fn value(&self) -> &RepositoryHead {
        self.head()
    }

    /// Returns the monotonically increasing publication generation.
    pub fn generation(&self) -> u64 {
        self.head.generation()
    }

    /// Returns the current snapshot reference, if one has been published.
    pub fn snapshot(&self) -> Option<&SnapshotReference> {
        self.head.snapshot()
    }

    /// Alias for [`Self::snapshot`] using the persisted-reference terminology.
    pub fn snapshot_reference(&self) -> Option<&SnapshotReference> {
        self.snapshot()
    }

    /// Alias for [`Self::snapshot`] using the short reference terminology.
    pub fn snapshot_ref(&self) -> Option<&SnapshotReference> {
        self.snapshot()
    }

    /// Returns whether this read names a published snapshot.
    pub fn has_snapshot(&self) -> bool {
        self.head.has_snapshot()
    }

    /// Returns the storage version token observed with this HEAD read.
    pub fn version(&self) -> Option<&StorageVersion> {
        self.version.as_ref()
    }

    /// Alias for [`Self::version`] using conditional-write terminology.
    pub fn version_token(&self) -> Option<&StorageVersion> {
        self.version()
    }

    /// Alias for [`Self::version`] using storage-version terminology.
    pub fn storage_version(&self) -> Option<&StorageVersion> {
        self.version()
    }

    /// Returns whether this read contains no published snapshot.
    pub fn is_empty(&self) -> bool {
        self.head.is_empty()
    }

    /// Consumes the read and returns the HEAD and its optional token.
    pub fn into_parts(self) -> (RepositoryHead, Option<StorageVersion>) {
        (self.head, self.version)
    }
}

/// Compatibility name for [`HeadState`].
pub type RepositoryHeadState = HeadState;

/// Compatibility name for [`HeadState`] emphasizing the versioned read.
pub type VersionedHead = HeadState;

/// Compatibility name for [`HeadState`] emphasizing the read operation.
pub type HeadRead = HeadState;

/// Compatibility name for [`HeadState`] using the full repository terminology.
pub type RepositoryHeadRead = HeadState;

/// A usable handle for a repository whose root objects have been validated.
#[derive(Clone)]
pub struct Repository {
    descriptor: RepositoryDescriptor,
    storage: StorageHandle,
}

impl Repository {
    /// Initializes the minimum valid repository using validated domain values.
    pub fn initialize<S>(
        storage: S,
        identity: RepositoryIdentity,
        repository_key: RepositoryKey,
    ) -> SdkResult<Self>
    where
        S: Into<StorageHandle>,
    {
        Self::initialize_with_request(
            storage,
            RepositoryInitRequest::new(identity, repository_key),
        )
    }

    /// Initializes the minimum valid repository from a typed request.
    pub fn initialize_with_request<S>(storage: S, request: RepositoryInitRequest) -> SdkResult<Self>
    where
        S: Into<StorageHandle>,
    {
        let storage = storage.into();
        let (identity, repository_key) = request.into_parts();
        let descriptor = initialize_use_case(storage.as_storage(), identity, repository_key)
            .map_err(SdkError::from)?;
        Ok(Self {
            descriptor,
            storage,
        })
    }

    /// Opens a repository after validating its persisted root objects.
    pub fn open<S>(storage: S) -> SdkResult<Self>
    where
        S: Into<StorageHandle>,
    {
        Self::open_with_request(storage, RepositoryOpenRequest::new())
    }

    /// Opens a repository using optional identity expectations.
    pub fn open_with_request<S>(storage: S, request: RepositoryOpenRequest) -> SdkResult<Self>
    where
        S: Into<StorageHandle>,
    {
        let storage = storage.into();
        let descriptor = open_use_case(
            storage.as_storage(),
            RepositoryOpenExpectations {
                identity: request.expected_identity(),
                repository_key: request.expected_repository_key(),
            },
        )
        .map_err(SdkError::from)?;
        Ok(Self {
            descriptor,
            storage,
        })
    }

    /// Opens a repository and verifies its persisted identity and key.
    pub fn open_for_repository<S>(
        storage: S,
        identity: RepositoryIdentity,
        repository_key: RepositoryKey,
    ) -> SdkResult<Self>
    where
        S: Into<StorageHandle>,
    {
        Self::open_with_request(
            storage,
            RepositoryOpenRequest::for_repository(identity, repository_key),
        )
    }

    /// Alias for [`Self::open_for_repository`].
    pub fn open_with_identity<S>(
        storage: S,
        identity: RepositoryIdentity,
        repository_key: RepositoryKey,
    ) -> SdkResult<Self>
    where
        S: Into<StorageHandle>,
    {
        Self::open_for_repository(storage, identity, repository_key)
    }

    /// Returns the validated repository descriptor.
    pub const fn descriptor(&self) -> &RepositoryDescriptor {
        &self.descriptor
    }

    /// Returns the repository identity reported by the descriptor.
    pub fn identity(&self) -> &RepositoryIdentity {
        self.descriptor.identity()
    }

    /// Returns the repository ID reported by the descriptor.
    pub fn repository_id(&self) -> &RepositoryIdentity {
        self.identity()
    }

    /// Returns the namespace key reported by the descriptor.
    pub fn repository_key(&self) -> &RepositoryKey {
        self.descriptor.repository_key()
    }

    /// Returns the repository format version.
    pub const fn format_version(&self) -> u16 {
        self.descriptor.format_version()
    }

    /// Returns the descriptor schema version.
    pub const fn descriptor_version(&self) -> u16 {
        self.descriptor.descriptor_version()
    }

    /// Returns the required root object references.
    pub fn roots(&self) -> &RepositoryRoots {
        self.descriptor.roots()
    }

    /// Returns the storage handle retained by this repository.
    pub fn storage(&self) -> StorageHandle {
        self.storage.clone()
    }

    /// Reads the current repository HEAD and its storage version token.
    ///
    /// An absent HEAD is the valid empty repository state and is returned as
    /// generation zero with no snapshot and no token. A present but malformed
    /// HEAD is an error; it is never replaced with an empty fallback.
    pub fn read_head(&self) -> SdkResult<HeadState> {
        read_head_use_case(self.storage.as_storage())
            .map(HeadState::from_read)
            .map_err(SdkError::from)
    }

    /// Alias for [`Self::read_head`].
    pub fn head(&self) -> SdkResult<HeadState> {
        self.read_head()
    }

    /// Alias for [`Self::read_head`] emphasizing that the returned value is
    /// paired with a backend version token.
    pub fn read_head_with_version(&self) -> SdkResult<HeadState> {
        self.read_head()
    }

    /// Publishes a snapshot after validating all listed immutable objects.
    ///
    /// The supplied HEAD read is the compare-and-swap precondition. Validation
    /// and encoding happen before the final conditional write, so a missing or
    /// malformed snapshot cannot advance HEAD.
    pub fn publish_head<P>(&self, expected: &HeadState, publication: P) -> SdkResult<HeadState>
    where
        P: Into<SnapshotPublication>,
    {
        self.publish_head_with_cancellation(expected, publication, None)
    }

    /// Publishes a snapshot through the repository HEAD.
    pub fn publish_snapshot<P>(&self, expected: &HeadState, publication: P) -> SdkResult<HeadState>
    where
        P: Into<SnapshotPublication>,
    {
        self.publish_head(expected, publication)
    }

    /// Alias for [`Self::publish_head`] using the short repository verb.
    pub fn publish<P>(&self, expected: &HeadState, publication: P) -> SdkResult<HeadState>
    where
        P: Into<SnapshotPublication>,
    {
        self.publish_head(expected, publication)
    }

    /// Publishes a snapshot and explicitly supplies its required objects.
    pub fn publish_snapshot_with_required_objects<I>(
        &self,
        expected: &HeadState,
        snapshot: SnapshotReference,
        required_objects: impl IntoIterator<Item = I>,
    ) -> SdkResult<HeadState>
    where
        I: Into<RepositoryObject>,
    {
        self.publish_snapshot(
            expected,
            SnapshotPublication::with_required_objects(snapshot, required_objects),
        )
    }

    /// Publishes a snapshot while observing a cooperative cancellation token.
    ///
    /// Cancellation is checked before object validation and immediately before
    /// the storage CAS. A cancellation observed after the CAS has begun cannot
    /// undo an already atomic publication; callers can read the resulting HEAD
    /// and decide whether to continue subsequent work.
    pub fn publish_head_with_cancellation<P>(
        &self,
        expected: &HeadState,
        publication: P,
        cancellation: Option<&CancellationToken>,
    ) -> SdkResult<HeadState>
    where
        P: Into<SnapshotPublication>,
    {
        let publication = publication.into();
        let is_cancelled = || cancellation.is_some_and(CancellationToken::is_cancelled);
        publish_head_use_case(
            self.storage.as_storage(),
            &ApplicationHeadRead {
                head: expected.head.clone(),
                version: expected.version.clone(),
            },
            &publication,
            Some(&is_cancelled),
        )
        .map(HeadState::from_read)
        .map_err(SdkError::from)
    }

    /// Alias for [`Self::publish_head_with_cancellation`].
    pub fn publish_snapshot_with_cancellation<P>(
        &self,
        expected: &HeadState,
        publication: P,
        cancellation: Option<&CancellationToken>,
    ) -> SdkResult<HeadState>
    where
        P: Into<SnapshotPublication>,
    {
        self.publish_head_with_cancellation(expected, publication, cancellation)
    }

    /// Returns whether a valid published snapshot currently exists.
    pub fn has_published_snapshot(&self) -> bool {
        self.read_head().is_ok_and(|head| head.has_snapshot())
    }

    /// Returns whether a valid published snapshot currently exists, preserving
    /// a typed error when HEAD cannot be read or decoded.
    pub fn try_has_published_snapshot(&self) -> SdkResult<bool> {
        self.read_head().map(|head| head.has_snapshot())
    }

    /// Lists compact snapshot summaries in deterministic newest-first order.
    ///
    /// The operation reads history records or compact authoritative snapshot
    /// headers only. It never follows root-tree or path-delta references.
    pub fn list_snapshot_summaries(
        &self,
        request: impl Into<SnapshotListRequest>,
    ) -> SdkResult<SnapshotSummaryPage> {
        let request = request.into();
        list_snapshot_summaries_use_case(self.storage.as_storage(), &request)
            .map_err(SdkError::from)
    }

    /// Alias for [`Self::list_snapshot_summaries`] using history terminology.
    pub fn list_history(
        &self,
        request: impl Into<SnapshotListRequest>,
    ) -> SdkResult<SnapshotHistoryPage> {
        self.list_snapshot_summaries(request)
    }

    /// Alias for [`Self::list_snapshot_summaries`] using snapshot terminology.
    pub fn list_snapshots(
        &self,
        request: impl Into<SnapshotListRequest>,
    ) -> SdkResult<SnapshotPage> {
        self.list_snapshot_summaries(request)
    }

    /// Rebuilds summaries directly from immutable snapshot objects.
    ///
    /// This is a read-only recovery path for a missing or incomplete derived
    /// history index. The returned values are not written back automatically.
    pub fn rebuild_snapshot_summaries(&self) -> SdkResult<Vec<SnapshotSummary>> {
        rebuild_snapshot_summaries_use_case(self.storage.as_storage()).map_err(SdkError::from)
    }

    /// Resolves a full snapshot ID, a unique ID prefix, or `latest` to its
    /// immutable repository object reference.
    pub fn resolve_snapshot_reference(
        &self,
        reference: impl AsRef<str>,
    ) -> SdkResult<SnapshotReference> {
        resolve_snapshot_reference_use_case(self.storage.as_storage(), reference.as_ref())
            .map_err(SdkError::from)
    }

    /// Alias for [`Self::resolve_snapshot_reference`].
    pub fn resolve_snapshot(&self, reference: impl AsRef<str>) -> SdkResult<SnapshotReference> {
        self.resolve_snapshot_reference(reference)
    }

    /// Resolves a full ID, unique prefix, or `latest` and returns its ID.
    pub fn resolve_snapshot_id(&self, reference: impl AsRef<str>) -> SdkResult<SnapshotId> {
        self.resolve_snapshot_reference(reference)
            .and_then(|reference| reference.snapshot_id().map_err(SdkError::from))
    }

    /// Reads one compact summary after resolving its reference.
    pub fn snapshot_summary(&self, reference: impl AsRef<str>) -> SdkResult<SnapshotSummary> {
        read_snapshot_summary_use_case(self.storage.as_storage(), reference.as_ref())
            .map_err(SdkError::from)
    }
}

impl PartialEq for Repository {
    fn eq(&self, other: &Self) -> bool {
        self.descriptor == other.descriptor
    }
}

impl Eq for Repository {}

impl fmt::Debug for Repository {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("Repository")
            .field("descriptor", &self.descriptor)
            .finish()
    }
}

/// Initializes a repository through the storage abstraction.
pub fn initialize_repository<S>(storage: S, request: RepositoryInitRequest) -> SdkResult<Repository>
where
    S: Into<StorageHandle>,
{
    Repository::initialize_with_request(storage, request)
}

/// Opens a repository through the storage abstraction.
pub fn open_repository<S>(storage: S, request: RepositoryOpenRequest) -> SdkResult<Repository>
where
    S: Into<StorageHandle>,
{
    Repository::open_with_request(storage, request)
}

impl From<DomainError> for SdkError {
    fn from(error: DomainError) -> Self {
        match error {
            DomainError::InvalidRepositoryIdentity { reason } => SdkError::InvalidRequest {
                field: "repository_identity",
                reason,
            },
            DomainError::InvalidAuthorIdentity { reason } => SdkError::InvalidRequest {
                field: "author",
                reason,
            },
            DomainError::InvalidRepositoryKey { reason } => SdkError::InvalidRequest {
                field: "repository_key",
                reason,
            },
            DomainError::InvalidRepositoryObject { reason } => SdkError::InvalidRequest {
                field: "repository_object",
                reason,
            },
            DomainError::InvalidSnapshotReference { reason } => SdkError::InvalidRequest {
                field: "snapshot_reference",
                reason,
            },
            DomainError::InvalidSnapshotId { reason } => SdkError::InvalidRequest {
                field: "snapshot_id",
                reason,
            },
            DomainError::InvalidSnapshotSelector { reason } => SdkError::InvalidRequest {
                field: "snapshot_reference",
                reason,
            },
            DomainError::InvalidSnapshotMetadata { reason } => SdkError::InvalidRequest {
                field: "snapshot_metadata",
                reason,
            },
            DomainError::InvalidRepositoryHead { reason } => SdkError::InvalidRequest {
                field: "repository_head",
                reason,
            },
        }
    }
}

impl From<RepositoryError> for SdkError {
    fn from(error: RepositoryError) -> Self {
        match error {
            RepositoryError::AlreadyExists => Self::RepositoryAlreadyExists,
            RepositoryError::Missing => Self::RepositoryMissing,
            RepositoryError::Malformed { reason } => Self::RepositoryMalformed { reason },
            RepositoryError::UnsupportedVersion { version } => {
                Self::RepositoryUnsupportedVersion { version }
            }
            RepositoryError::Incompatible { reason } => Self::RepositoryIncompatible { reason },
            RepositoryError::PublicationConflict => Self::RepositoryPublicationConflict,
            RepositoryError::SnapshotMissing => Self::RepositorySnapshotMissing,
            RepositoryError::RequiredObjectMissing => Self::RepositoryRequiredObjectMissing,
            RepositoryError::InvalidPublication { reason } => Self::InvalidRequest {
                field: "snapshot_publication",
                reason,
            },
            RepositoryError::GenerationExhausted => Self::RepositoryGenerationExhausted,
            RepositoryError::UnsupportedCapability => Self::StorageCapabilityUnsupported,
            RepositoryError::Cancelled => Self::OperationCancelled { operation_id: None },
            RepositoryError::NoSnapshots => Self::RepositoryNoSnapshots,
            RepositoryError::SnapshotReferenceEmpty => Self::SnapshotReferenceEmpty,
            RepositoryError::SnapshotReferenceMalformed => Self::SnapshotReferenceMalformed,
            RepositoryError::SnapshotReferenceNotFound => Self::SnapshotReferenceNotFound,
            RepositoryError::SnapshotReferenceAmbiguous => Self::SnapshotReferenceAmbiguous,
            RepositoryError::SnapshotHistoryRequestInvalid => Self::InvalidRequest {
                field: "snapshot_history",
                reason: "history page size is outside the supported range",
            },
            RepositoryError::SnapshotHistoryCursorInvalid => Self::InvalidRequest {
                field: "snapshot_history_cursor",
                reason: "history cursor is not present in the current snapshot history",
            },
            RepositoryError::Storage { operation } => Self::StorageFailure { operation },
        }
    }
}

impl Snapshot {
    /// Encodes this compact authoritative snapshot header as versioned binary
    /// MessagePack suitable for an immutable snapshot object.
    pub fn to_bytes(&self) -> SdkResult<Vec<u8>> {
        encode_snapshot(self).map_err(map_snapshot_format_error)
    }

    /// Alias for [`Self::to_bytes`].
    pub fn encode(&self) -> SdkResult<Vec<u8>> {
        self.to_bytes()
    }

    /// Decodes and validates a compact authoritative snapshot header.
    pub fn from_bytes(bytes: &[u8]) -> SdkResult<Self> {
        decode_snapshot(bytes).map_err(map_snapshot_format_error)
    }
}

/// Encodes one compact authoritative snapshot header.
pub fn encode_snapshot_object(snapshot: &Snapshot) -> SdkResult<Vec<u8>> {
    snapshot.to_bytes()
}

/// Decodes one compact authoritative snapshot header.
pub fn decode_snapshot_object(bytes: &[u8]) -> SdkResult<Snapshot> {
    Snapshot::from_bytes(bytes)
}

fn map_snapshot_format_error(error: FormatError) -> SdkError {
    match error {
        FormatError::UnsupportedVersion { version } => {
            SdkError::RepositoryUnsupportedVersion { version }
        }
        FormatError::InvalidEncoding => SdkError::RepositoryMalformed {
            reason: "snapshot object is not valid MessagePack",
        },
        FormatError::InputTooLarge => SdkError::RepositoryMalformed {
            reason: "snapshot object exceeds the supported size limit",
        },
        FormatError::TrailingBytes => SdkError::RepositoryMalformed {
            reason: "snapshot object contains trailing MessagePack bytes",
        },
        FormatError::InvalidMagic => SdkError::RepositoryMalformed {
            reason: "snapshot object magic is invalid",
        },
        FormatError::InvalidChecksum => SdkError::RepositoryMalformed {
            reason: "snapshot object integrity check failed",
        },
        FormatError::InvalidField
        | FormatError::InvalidRootReference
        | FormatError::MissingRequiredFeature
        | FormatError::UnsupportedRequiredFeature
        | FormatError::VersionMismatch
        | FormatError::Serialization => SdkError::RepositoryMalformed {
            reason: "snapshot object contains an invalid field",
        },
    }
}

/// Alias for the current repository format version.
pub const REPOSITORY_FORMAT_VERSION: u16 = CURRENT_REPOSITORY_FORMAT_VERSION;

/// Alias for the current repository bootstrap schema version.
pub const REPOSITORY_BOOTSTRAP_VERSION: u16 = CURRENT_REPOSITORY_BOOTSTRAP_VERSION;

/// Alias for the current repository descriptor version.
pub const REPOSITORY_DESCRIPTOR_VERSION: u16 = CURRENT_REPOSITORY_DESCRIPTOR_VERSION;
