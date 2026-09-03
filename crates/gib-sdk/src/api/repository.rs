use super::error::{SdkError, SdkResult};
use super::operation::CancellationToken;
use crate::application::ports::read_stream_to_vec;
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
use crate::format::{
    EncryptionContext, FormatError, PackBuilder as PackBuilderFormat, PackFormatError,
    PackIndexFormatError, PackIndexShardBuilder as PackIndexShardBuilderFormat, VerifiedPack,
    VerifiedPackIndexShard, calculate_object_id, decode_object_envelope,
    decode_object_envelope_from_reader, decode_object_envelope_from_reader_with_encryption,
    decode_object_envelope_from_reader_with_password, decode_object_envelope_with_encryption,
    decode_object_envelope_with_password, decode_snapshot, derive_encryption_context,
    encode_object_envelope, encode_object_envelope_with_encryption,
    encode_object_envelope_with_options, encode_object_envelope_with_password, encode_snapshot,
    generate_encryption_context, snapshot_object_id,
};
use std::collections::HashMap;
use std::fmt;
use std::io::{Cursor, Read, Write};
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
    ARGON2ID_MEMORY_COST_KIB, ARGON2ID_PARALLELISM, ARGON2ID_TIME_COST,
    CURRENT_INDEX_OBJECT_VERSION, CURRENT_OBJECT_ENVELOPE_VERSION, CURRENT_PACK_OBJECT_VERSION,
    CURRENT_TRANSFORMED_OBJECT_ENVELOPE_VERSION, CURRENT_TREE_OBJECT_VERSION, CompressionLevel,
    CompressionLevelError, DEFAULT_ZSTD_COMPRESSION_LEVEL, ImmutableObject,
    MAX_IMMUTABLE_OBJECT_BYTES, MAX_IMMUTABLE_OBJECT_PAYLOAD_BYTES,
    MAX_IMMUTABLE_OBJECT_STORED_PAYLOAD_BYTES, OBJECT_ID_HEX_LENGTH, ObjectCodec, ObjectEncryption,
    ObjectId, ObjectKind, ObjectTransformOptions, REPOSITORY_ENCRYPTION_KDF,
    REPOSITORY_ENCRYPTION_KEY_LENGTH, REPOSITORY_ENCRYPTION_SALT_LENGTH, RepositorySalt,
    RepositorySaltError, XCHACHA20_POLY1305_NONCE_LENGTH, XCHACHA20_POLY1305_TAG_LENGTH,
};
#[cfg(feature = "async")]
pub use crate::domain::{AsyncChunkStream, AsyncChunker, async_chunk_reader};
pub use crate::domain::{
    BUZHASH_TABLE_SEED, BUZHASH_WINDOW_SIZE, CHUNK_BUFFER_POOL_CAPACITY, CHUNKING_READ_BUFFER_SIZE,
    CONTENT_DEFINED_CHUNKING_ALGORITHM, CURRENT_CHUNKING_VERSION, Chunk, ChunkBoundary, ChunkId,
    ChunkIdError, ChunkStream, Chunker, ChunkingConfiguration, ChunkingConfigurationError,
    ChunkingError, ChunkingResult, DEFAULT_MAX_CHUNK_SIZE_BYTES, DEFAULT_MIN_CHUNK_SIZE_BYTES,
    DEFAULT_TARGET_CHUNK_SIZE_BYTES, MAX_CONTENT_DEFINED_CHUNK_SIZE_BYTES, chunk_reader,
    chunk_reader_with_cancellation,
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
    CURRENT_PACK_FORMAT_VERSION, DEFAULT_PACK_MAX_SIZE_BYTES, DEFAULT_PACK_TARGET_SIZE_BYTES,
    MAX_PACK_SIZE_BYTES, PACK_ALIGNMENT, PACK_ENTRY_HEADER_LENGTH, PACK_FOOTER_LENGTH,
    PACK_HEADER_LENGTH, PackConfiguration, PackConfigurationError, PackEntryError, PackEntryInput,
    PackEntryLocation, PackId, PackIdError, PackMetadata, SealedPack,
};
pub use crate::domain::{
    CURRENT_PACK_INDEX_FORMAT_VERSION, DEFAULT_PACK_INDEX_CACHE_MAX_BYTES,
    DEFAULT_PACK_INDEX_CACHE_MAX_SHARDS, DEFAULT_PACK_INDEX_MAX_SHARD_BYTES,
    MAX_PACK_INDEX_CACHE_BYTES, MAX_PACK_INDEX_SHARD_BYTES, MIN_PACK_INDEX_SHARD_BYTES,
    PACK_INDEX_ALIGNMENT, PACK_INDEX_FOOTER_LENGTH, PACK_INDEX_HEADER_LENGTH,
    PACK_INDEX_RECORD_LENGTH, PACK_INDEX_SHARD_COUNT, PACK_INDEX_SHARD_PREFIX_BYTES,
    PACK_INDEX_STORAGE_PREFIX, PackIndexCacheConfiguration, PackIndexCacheConfigurationError,
    PackIndexConfiguration, PackIndexConfigurationError, PackIndexEntry, PackIndexEntryError,
    PackIndexId, PackIndexIdError, PackIndexRange, PackIndexRangeError, PackIndexShardId,
    PackIndexShardMetadata, PackIndexTransform, PackIndexTransformError, SealedPackIndexShard,
    pack_index_object_key, pack_index_storage_key,
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
    DEFAULT_S3_CAPABILITY_CACHE_FILE_NAME, DEFAULT_S3_CAPABILITY_CACHE_TTL_SECONDS,
    DEFAULT_S3_MAX_CONCURRENCY, DEFAULT_S3_MULTIPART_PART_SIZE, DEFAULT_S3_MULTIPART_THRESHOLD,
    MAX_S3_MULTIPART_PART_SIZE, MAX_S3_MULTIPART_THRESHOLD, MAX_S3_MULTIPART_UPLOAD_PARTS,
    MIN_S3_MULTIPART_PART_SIZE, S3ConditionalWriteCapabilities, S3ConditionalWriteStatus,
    S3Storage, S3StorageConfig,
};
#[cfg(feature = "webdav")]
pub use crate::infrastructure::storage::{
    DEFAULT_WEBDAV_MAX_CONCURRENCY, DEFAULT_WEBDAV_REQUEST_TIMEOUT,
    DEFAULT_WEBDAV_TRANSFER_BUFFER_SIZE, MAX_WEBDAV_MAX_CONCURRENCY, WebDavStorage,
    WebDavStorageConfig,
};
pub use crate::infrastructure::storage::{
    LocalStorage, LocalStorageOperation, MemoryStorage, MemoryStorageOperation,
};

/// Repository encryption material derived from a password and a persistent
/// per-repository salt.
///
/// The derived key is kept private, is redacted from `Debug`, and is zeroized
/// when this context is dropped. The salt is copied into transformed object
/// envelopes so a password-only decoder can select the recorded KDF inputs.
#[derive(Clone)]
pub struct RepositoryEncryption {
    context: EncryptionContext,
}

impl RepositoryEncryption {
    /// Derives repository encryption material using the fixed Argon2id v1
    /// parameters documented by the repository format.
    pub fn from_password(password: &[u8], salt: RepositorySalt) -> SdkResult<Self> {
        if password.is_empty() {
            return Err(SdkError::InvalidRequest {
                field: "password",
                reason: "must not be empty",
            });
        }
        derive_encryption_context(password, salt)
            .map(|context| Self { context })
            .map_err(map_object_format_error)
    }

    /// Generates a fresh repository salt and derives encryption material from
    /// the supplied password.
    pub fn generate(password: &[u8]) -> SdkResult<Self> {
        if password.is_empty() {
            return Err(SdkError::InvalidRequest {
                field: "password",
                reason: "must not be empty",
            });
        }
        generate_encryption_context(password)
            .map(|context| Self { context })
            .map_err(map_object_format_error)
    }

    /// Returns the per-repository salt that must be retained with the
    /// repository's encryption configuration.
    pub const fn salt(&self) -> RepositorySalt {
        self.context.salt()
    }

    pub(crate) fn context(&self) -> &EncryptionContext {
        &self.context
    }
}

impl fmt::Debug for RepositoryEncryption {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("RepositoryEncryption")
            .field("salt", &"<redacted>")
            .field("key", &"<redacted>")
            .finish()
    }
}

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

    pub(crate) fn as_arc(&self) -> Arc<dyn RepositoryStorage> {
        Arc::clone(&self.inner)
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

#[cfg(feature = "webdav")]
impl From<&WebDavStorage> for StorageHandle {
    fn from(storage: &WebDavStorage) -> Self {
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
            DomainError::InvalidObjectId { reason } => SdkError::InvalidRequest {
                field: "object_id",
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

/// Publishes one sealed pack atomically.
///
/// The publisher is called only after the pack footer and pack ID have been
/// completed. A publisher should write the bytes to its durable backend and
/// make the object visible only after the write has succeeded. Completed packs
/// are not retained by [`PackBuilder`].
pub trait PackPublisher {
    /// Publishes one immutable pack or returns a stable SDK error.
    fn publish(&mut self, pack: &SealedPack) -> SdkResult<()>;
}

impl<F> PackPublisher for F
where
    F: FnMut(&SealedPack) -> SdkResult<()>,
{
    fn publish(&mut self, pack: &SealedPack) -> SdkResult<()> {
        self(pack)
    }
}

/// Builds size-bounded immutable packs from transformed chunk payloads.
///
/// The builder owns at most the current pack. Use [`Self::add_to`] or
/// [`Self::add_stream`] to publish completed packs immediately and keep memory
/// bounded across a large backup. Entries are written in the order supplied;
/// identical inputs and configuration therefore produce identical bytes and
/// IDs.
pub struct PackBuilder {
    inner: PackBuilderFormat,
}

impl PackBuilder {
    /// Creates an empty builder with a validated pack configuration.
    pub const fn new(configuration: PackConfiguration) -> Self {
        Self {
            inner: PackBuilderFormat::new(configuration),
        }
    }

    /// Returns the configuration used by this builder.
    pub const fn configuration(&self) -> PackConfiguration {
        self.inner.configuration()
    }

    /// Adds one transformed chunk.
    ///
    /// When adding the entry seals the current pack, that pack is returned and
    /// must be consumed or published by the caller before continuing. The
    /// builder retains only the new current pack.
    pub fn add(&mut self, entry: PackEntryInput) -> SdkResult<Option<SealedPack>> {
        self.inner.add(entry).map_err(map_pack_format_error)
    }

    /// Adds one transformed chunk after checking a cooperative cancellation
    /// token.
    pub fn add_with_cancellation(
        &mut self,
        entry: PackEntryInput,
        cancellation: &CancellationToken,
    ) -> SdkResult<Option<SealedPack>> {
        if cancellation.is_cancelled() {
            self.inner.abort();
            return Err(SdkError::OperationCancelled { operation_id: None });
        }
        self.add(entry)
    }

    /// Seals and returns the current pack, if it contains entries.
    pub fn finish(&mut self) -> SdkResult<Option<SealedPack>> {
        self.inner.finish().map_err(map_pack_format_error)
    }

    /// Publishes a completed pack as soon as the next entry crosses a pack
    /// boundary.
    pub fn add_to<P: PackPublisher + ?Sized>(
        &mut self,
        entry: PackEntryInput,
        publisher: &mut P,
    ) -> SdkResult<()> {
        let sealed = self.inner.add(entry).map_err(map_pack_format_error)?;
        if let Some(pack) = sealed
            && let Err(error) = publisher.publish(&pack)
        {
            self.inner.abort();
            return Err(error);
        }
        Ok(())
    }

    /// Publishes a completed pack after checking a cooperative cancellation
    /// token.
    pub fn add_to_with_cancellation<P: PackPublisher + ?Sized>(
        &mut self,
        entry: PackEntryInput,
        cancellation: &CancellationToken,
        publisher: &mut P,
    ) -> SdkResult<()> {
        if cancellation.is_cancelled() {
            self.inner.abort();
            return Err(SdkError::OperationCancelled { operation_id: None });
        }
        self.add_to(entry, publisher)
    }

    /// Consumes a streaming entry iterator, publishes each completed pack,
    /// and seals the final pack.
    ///
    /// The iterator is advanced one entry at a time. The caller remains
    /// responsible for producing transformed payloads with bounded memory;
    /// this method never collects the iterator or completed packs.
    pub fn add_stream<I, P>(
        &mut self,
        entries: I,
        publisher: &mut P,
        cancellation: &CancellationToken,
    ) -> SdkResult<u64>
    where
        I: IntoIterator<Item = PackEntryInput>,
        P: PackPublisher + ?Sized,
    {
        let mut count = 0_u64;
        for entry in entries {
            self.add_to_with_cancellation(entry, cancellation, publisher)?;
            count = count.checked_add(1).ok_or(SdkError::InvalidRequest {
                field: "pack.entry_count",
                reason: "entry count exceeds the supported range",
            })?;
        }
        if cancellation.is_cancelled() {
            self.inner.abort();
            return Err(SdkError::OperationCancelled { operation_id: None });
        }
        self.finish_to(publisher)?;
        Ok(count)
    }

    /// Seals and publishes the current pack, if any.
    pub fn finish_to<P: PackPublisher + ?Sized>(&mut self, publisher: &mut P) -> SdkResult<()> {
        let sealed = self.inner.finish().map_err(map_pack_format_error)?;
        if let Some(pack) = sealed
            && let Err(error) = publisher.publish(&pack)
        {
            self.inner.abort();
            return Err(error);
        }
        Ok(())
    }

    /// Aborts construction and discards the current unsealed pack.
    ///
    /// Packs already accepted by a publisher cannot be rolled back by this
    /// method; publication implementations must provide their own atomicity.
    pub fn abort(&mut self) {
        self.inner.abort();
    }
}

impl fmt::Debug for PackBuilder {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("PackBuilder")
            .field("configuration", &self.configuration())
            .finish()
    }
}

impl SealedPack {
    /// Writes this sealed pack to a writer and returns the number of bytes
    /// accepted by the API.
    pub fn write_to<W: Write>(&self, writer: &mut W) -> SdkResult<u64> {
        writer
            .write_all(self.as_bytes())
            .map_err(|_| SdkError::RepositoryPackWriteFailed)?;
        u64::try_from(self.len()).map_err(|_| SdkError::RepositoryPackWriteFailed)
    }
}

/// Publishes one sealed immutable pack-index shard.
pub trait PackIndexPublisher {
    /// Publishes one shard after its footer and index ID are complete.
    fn publish(&mut self, shard: &SealedPackIndexShard) -> SdkResult<()>;
}

impl<F> PackIndexPublisher for F
where
    F: FnMut(&SealedPackIndexShard) -> SdkResult<()>,
{
    fn publish(&mut self, shard: &SealedPackIndexShard) -> SdkResult<()> {
        self(shard)
    }
}

/// An immutable-storage publisher for pack-index shards.
pub struct PackIndexStoragePublisher {
    storage: StorageHandle,
}

impl PackIndexStoragePublisher {
    /// Wraps a storage backend for create-if-absent index publication.
    pub fn new<S>(storage: S) -> Self
    where
        S: Into<StorageHandle>,
    {
        Self {
            storage: storage.into(),
        }
    }

    /// Returns the storage handle retained by this publisher.
    pub fn storage(&self) -> StorageHandle {
        self.storage.clone()
    }
}

impl PackIndexPublisher for PackIndexStoragePublisher {
    fn publish(&mut self, shard: &SealedPackIndexShard) -> SdkResult<()> {
        let key = ObjectKey::new(pack_index_object_key(shard.id())).map_err(|_| {
            SdkError::InvalidRequest {
                field: "pack_index_key",
                reason: "derived pack-index storage key is invalid",
            }
        })?;
        let mut source = Cursor::new(shard.as_bytes());
        let expected_size =
            u64::try_from(shard.len()).map_err(|_| SdkError::RepositoryPackIndexWriteFailed)?;
        self.storage
            .as_storage()
            .write_stream(
                &key,
                &mut source,
                ObjectWriteOptions::if_absent().with_expected_size(expected_size),
            )
            .map(|_| ())
            .map_err(|error| match error {
                StorageError::AlreadyExists => SdkError::RepositoryPublicationConflict,
                StorageError::UnsupportedCapability => SdkError::StorageCapabilityUnsupported,
                StorageError::Cancelled => SdkError::OperationCancelled { operation_id: None },
                _ => SdkError::StorageFailure {
                    operation: "publish_pack_index",
                },
            })
    }
}

/// Builds one sorted, immutable pack-index shard without retaining other
/// shards.
pub struct PackIndexShardBuilder {
    inner: PackIndexShardBuilderFormat,
}

impl PackIndexShardBuilder {
    /// Creates a builder for one validated chunk-ID prefix.
    pub fn new(
        configuration: PackIndexConfiguration,
        shard_id: PackIndexShardId,
    ) -> SdkResult<Self> {
        Ok(Self {
            inner: PackIndexShardBuilderFormat::new(configuration, shard_id)
                .map_err(map_pack_index_format_error)?,
        })
    }

    /// Returns the shard policy used by this builder.
    pub const fn configuration(&self) -> PackIndexConfiguration {
        self.inner.configuration()
    }

    /// Returns the chunk-ID prefix assigned to this builder.
    pub const fn shard_id(&self) -> PackIndexShardId {
        self.inner.shard_id()
    }

    /// Adds one validated location and its transform descriptor.
    pub fn add(&mut self, entry: PackIndexEntry) -> SdkResult<()> {
        self.inner.add(entry).map_err(map_pack_index_format_error)
    }

    /// Adds all entries from a sealed pack using one transform descriptor.
    ///
    /// Callers that use different transforms for different entries can call
    /// [`Self::add`] for each location instead.
    pub fn add_pack(&mut self, pack: &SealedPack, transform: PackIndexTransform) -> SdkResult<u64> {
        let mut added = 0_u64;
        for location in pack.entries() {
            let entry = PackIndexEntry::from_location(*location, transform)
                .map_err(map_pack_index_entry_error)?;
            self.add(entry)?;
            added = added.checked_add(1).ok_or(SdkError::InvalidRequest {
                field: "pack_index.entry_count",
                reason: "entry count exceeds the supported range",
            })?;
        }
        Ok(added)
    }

    /// Seals the sorted shard and returns its complete immutable bytes.
    pub fn finish(&mut self) -> SdkResult<SealedPackIndexShard> {
        self.inner.finish().map_err(map_pack_index_format_error)
    }

    /// Seals and publishes the shard after all bytes have been verified.
    pub fn finish_to<P: PackIndexPublisher + ?Sized>(
        &mut self,
        publisher: &mut P,
    ) -> SdkResult<()> {
        let shard = self.finish()?;
        if let Err(error) = publisher.publish(&shard) {
            self.inner.abort();
            return Err(error);
        }
        Ok(())
    }

    /// Aborts this builder and discards all unsealed records.
    pub fn abort(&mut self) {
        self.inner.abort();
    }
}

impl fmt::Debug for PackIndexShardBuilder {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("PackIndexShardBuilder")
            .field("configuration", &self.configuration())
            .field("shard_id", &self.shard_id())
            .finish()
    }
}

impl SealedPackIndexShard {
    /// Writes this sealed shard to a writer.
    pub fn write_to<W: Write>(&self, writer: &mut W) -> SdkResult<u64> {
        writer
            .write_all(self.as_bytes())
            .map_err(|_| SdkError::RepositoryPackIndexWriteFailed)?;
        u64::try_from(self.len()).map_err(|_| SdkError::RepositoryPackIndexWriteFailed)
    }
}

/// A verified index-shard view with binary-search lookup.
pub struct PackIndexReader {
    inner: VerifiedPackIndexShard,
}

impl PackIndexReader {
    /// Validates the complete shard before exposing any record.
    pub fn new(bytes: &[u8]) -> SdkResult<Self> {
        Ok(Self {
            inner: VerifiedPackIndexShard::new(bytes).map_err(map_pack_index_format_error)?,
        })
    }

    /// Returns verified shard metadata.
    pub const fn metadata(&self) -> PackIndexShardMetadata {
        self.inner.metadata()
    }

    /// Returns records in canonical full chunk-ID order.
    pub fn entries(&self) -> &[PackIndexEntry] {
        self.inner.entries()
    }

    /// Finds one chunk using binary search, or returns `None` if absent.
    pub fn lookup(&self, chunk_id: ChunkId) -> Option<PackIndexEntry> {
        self.inner.lookup(chunk_id)
    }
}

/// Verifies an immutable pack-index shard and returns its metadata.
pub fn verify_pack_index(bytes: &[u8]) -> SdkResult<PackIndexShardMetadata> {
    PackIndexReader::new(bytes).map(|reader| reader.metadata())
}

#[derive(Clone, Debug, Eq, Hash, PartialEq)]
struct PackIndexCacheKey {
    storage_key: String,
    shard_id: PackIndexShardId,
}

struct CachedPackIndexShard {
    shard: Arc<VerifiedPackIndexShard>,
    weight: usize,
    last_used: u64,
}

/// A bounded least-recently-used cache of verified index shards.
pub struct PackIndexCache {
    configuration: PackIndexCacheConfiguration,
    shards: HashMap<PackIndexCacheKey, CachedPackIndexShard>,
    resident_bytes: usize,
    clock: u64,
}

impl PackIndexCache {
    /// Creates an empty cache with an explicit resident-memory policy.
    pub fn new(configuration: PackIndexCacheConfiguration) -> Self {
        Self {
            configuration,
            shards: HashMap::new(),
            resident_bytes: 0,
            clock: 0,
        }
    }

    /// Returns the cache's validated memory and shard limits.
    pub const fn configuration(&self) -> PackIndexCacheConfiguration {
        self.configuration
    }

    /// Returns the number of resident verified shards.
    pub fn len(&self) -> usize {
        self.shards.len()
    }

    /// Returns whether no shard is resident.
    pub fn is_empty(&self) -> bool {
        self.shards.is_empty()
    }

    /// Returns the estimated resident memory used by this cache.
    pub const fn resident_bytes(&self) -> usize {
        self.resident_bytes
    }

    /// Removes every cached shard.
    pub fn clear(&mut self) {
        self.shards.clear();
        self.resident_bytes = 0;
    }

    /// Looks up a chunk in the default static shard layout.
    pub fn lookup(&mut self, chunk_id: ChunkId) -> Option<PackIndexEntry> {
        let shard_id = PackIndexShardId::from_chunk_id(chunk_id);
        let storage_key = pack_index_storage_key(shard_id);
        self.lookup_at(&storage_key, shard_id, chunk_id)
    }

    /// Returns whether a default-layout shard is resident.
    pub fn contains_shard(&self, shard_id: PackIndexShardId) -> bool {
        let storage_key = pack_index_storage_key(shard_id);
        self.contains_at(&storage_key, shard_id)
    }

    /// Inserts a verified shard into the default static layout.
    ///
    /// A valid shard larger than the cache budget is intentionally used for
    /// the current lookup and then discarded; it is never allowed to exceed
    /// the configured resident-memory limit.
    pub fn insert(&mut self, shard: &SealedPackIndexShard) -> SdkResult<()> {
        let storage_key = pack_index_storage_key(shard.metadata().shard_id());
        self.insert_at(&storage_key, shard)
    }

    /// Inserts a verified shard under an immutable publication key.
    pub fn insert_at(&mut self, storage_key: &str, shard: &SealedPackIndexShard) -> SdkResult<()> {
        let verified =
            VerifiedPackIndexShard::new(shard.as_bytes()).map_err(map_pack_index_format_error)?;
        self.insert_verified(storage_key, verified);
        Ok(())
    }

    fn contains_at(&self, storage_key: &str, shard_id: PackIndexShardId) -> bool {
        self.shards.contains_key(&PackIndexCacheKey {
            storage_key: storage_key.to_owned(),
            shard_id,
        })
    }

    fn lookup_at(
        &mut self,
        storage_key: &str,
        shard_id: PackIndexShardId,
        chunk_id: ChunkId,
    ) -> Option<PackIndexEntry> {
        let key = PackIndexCacheKey {
            storage_key: storage_key.to_owned(),
            shard_id,
        };
        let last_used = self.next_clock();
        let cached = self.shards.get_mut(&key)?;
        cached.last_used = last_used;
        cached.shard.lookup(chunk_id)
    }

    fn insert_verified(&mut self, storage_key: &str, shard: VerifiedPackIndexShard) {
        let shard_id = shard.metadata().shard_id();
        let key = PackIndexCacheKey {
            storage_key: storage_key.to_owned(),
            shard_id,
        };
        let key_weight = key.storage_key.len();
        let Some(weight) = shard.estimated_memory().checked_add(key_weight) else {
            return;
        };
        if weight > self.configuration.max_bytes() {
            return;
        }
        if let Some(previous) = self.shards.remove(&key) {
            self.resident_bytes = self.resident_bytes.saturating_sub(previous.weight);
        }
        while self.resident_bytes.saturating_add(weight) > self.configuration.max_bytes()
            || self.shards.len() >= self.configuration.max_shards()
        {
            if !self.evict_oldest() {
                return;
            }
        }
        let last_used = self.next_clock();
        self.resident_bytes = self.resident_bytes.saturating_add(weight);
        self.shards.insert(
            key,
            CachedPackIndexShard {
                shard: Arc::new(shard),
                weight,
                last_used,
            },
        );
    }

    fn evict_oldest(&mut self) -> bool {
        let Some(key) = self
            .shards
            .iter()
            .min_by_key(|(_, cached)| cached.last_used)
            .map(|(key, _)| key.clone())
        else {
            return false;
        };
        if let Some(removed) = self.shards.remove(&key) {
            self.resident_bytes = self.resident_bytes.saturating_sub(removed.weight);
            true
        } else {
            false
        }
    }

    fn next_clock(&mut self) -> u64 {
        self.clock = self.clock.saturating_add(1);
        self.clock
    }
}

impl fmt::Debug for PackIndexCache {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("PackIndexCache")
            .field("configuration", &self.configuration)
            .field("shards", &self.shards.len())
            .field("resident_bytes", &self.resident_bytes)
            .finish()
    }
}

/// A bounded range read returned by pack-index lookup.
pub struct PackChunkRead {
    entry: PackIndexEntry,
    range: PackIndexRange,
    object: ObjectRead,
}

impl PackChunkRead {
    fn new(entry: PackIndexEntry, range: PackIndexRange, object: ObjectRead) -> Self {
        Self {
            entry,
            range,
            object,
        }
    }

    /// Returns the verified index record used for this read.
    pub const fn entry(&self) -> PackIndexEntry {
        self.entry
    }

    /// Returns the exact payload range requested from the pack.
    pub const fn range(&self) -> PackIndexRange {
        self.range
    }

    /// Returns metadata returned by the pack storage backend.
    pub const fn metadata(&self) -> &ObjectMetadata {
        self.object.metadata()
    }

    /// Returns the bounded payload reader.
    pub fn reader(&mut self) -> &mut dyn Read {
        self.object.reader()
    }

    /// Consumes the result and returns its bounded payload reader.
    pub fn into_reader(self) -> crate::application::ports::StorageReader {
        self.object.into_reader()
    }
}

impl Read for PackChunkRead {
    fn read(&mut self, buffer: &mut [u8]) -> std::io::Result<usize> {
        self.object.read(buffer)
    }
}

impl fmt::Debug for PackChunkRead {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("PackChunkRead")
            .field("entry", &self.entry)
            .field("range", &self.range)
            .field("metadata", &self.object.metadata())
            .finish_non_exhaustive()
    }
}

/// Storage-backed bounded lookup for pack-index shards and pack payloads.
pub struct PackIndexLookup {
    storage: StorageHandle,
    configuration: PackIndexConfiguration,
    cache: PackIndexCache,
}

impl PackIndexLookup {
    /// Creates a lookup using the default index-shard format policy.
    pub fn new<S>(storage: S, cache_configuration: PackIndexCacheConfiguration) -> Self
    where
        S: Into<StorageHandle>,
    {
        Self::with_configuration(
            storage,
            PackIndexConfiguration::default_policy(),
            cache_configuration,
        )
    }

    /// Creates a lookup with explicit index and cache policies.
    pub fn with_configuration<S>(
        storage: S,
        configuration: PackIndexConfiguration,
        cache_configuration: PackIndexCacheConfiguration,
    ) -> Self
    where
        S: Into<StorageHandle>,
    {
        Self {
            storage: storage.into(),
            configuration,
            cache: PackIndexCache::new(cache_configuration),
        }
    }

    /// Creates a lookup with the SDK's default index and cache policies.
    pub fn with_defaults<S>(storage: S) -> Self
    where
        S: Into<StorageHandle>,
    {
        Self::new(storage, PackIndexCacheConfiguration::default_policy())
    }

    /// Returns the index format policy used for reads.
    pub const fn configuration(&self) -> PackIndexConfiguration {
        self.configuration
    }

    /// Returns the storage handle used by this lookup.
    pub fn storage(&self) -> StorageHandle {
        self.storage.clone()
    }

    /// Returns the mutable bounded shard cache.
    pub fn cache(&mut self) -> &mut PackIndexCache {
        &mut self.cache
    }

    /// Looks up a chunk in the conventional static shard key.
    pub fn lookup(&mut self, chunk_id: ChunkId) -> SdkResult<Option<PackIndexEntry>> {
        let shard_id = PackIndexShardId::from_chunk_id(chunk_id);
        let key = ObjectKey::new(pack_index_storage_key(shard_id)).map_err(|_| {
            SdkError::InvalidRequest {
                field: "pack_index_key",
                reason: "derived pack-index storage key is invalid",
            }
        })?;
        self.lookup_at(chunk_id, &key)
    }

    /// Looks up a chunk in an explicitly selected immutable shard publication.
    ///
    /// The selected key may identify any immutable generation. The decoded
    /// shard must still contain the requested one-byte prefix.
    pub fn lookup_at(
        &mut self,
        chunk_id: ChunkId,
        index_key: &ObjectKey,
    ) -> SdkResult<Option<PackIndexEntry>> {
        let shard_id = PackIndexShardId::from_chunk_id(chunk_id);
        if self.cache.contains_at(index_key.as_str(), shard_id) {
            return Ok(self.cache.lookup_at(index_key.as_str(), shard_id, chunk_id));
        }
        let mut object = match self.storage.as_storage().read_stream(index_key) {
            Ok(object) => object,
            Err(StorageError::NotFound) => return Ok(None),
            Err(error) => return Err(map_pack_index_storage_error(error, "read_pack_index")),
        };
        let size = object.metadata().size();
        if size > self.configuration.max_shard_bytes() {
            return Err(SdkError::RepositoryMalformed {
                reason: "pack-index shard exceeds its configured size limit",
            });
        }
        let bytes = read_stream_to_vec(object.reader(), Some(size))
            .map_err(|error| map_pack_index_storage_error(error, "read_pack_index"))?;
        let shard = VerifiedPackIndexShard::new(&bytes).map_err(map_pack_index_format_error)?;
        if shard.metadata().shard_id() != shard_id {
            return Err(SdkError::RepositoryMalformed {
                reason: "pack-index shard prefix does not match the requested chunk",
            });
        }
        let result = shard.lookup(chunk_id);
        self.cache.insert_verified(index_key.as_str(), shard);
        Ok(result)
    }

    /// Alias for [`Self::lookup`].
    pub fn locate(&mut self, chunk_id: ChunkId) -> SdkResult<Option<PackIndexEntry>> {
        self.lookup(chunk_id)
    }

    /// Reads exactly the transformed payload range for a chunk.
    pub fn read_chunk(&mut self, chunk_id: ChunkId) -> SdkResult<Option<PackChunkRead>> {
        let shard_id = PackIndexShardId::from_chunk_id(chunk_id);
        let key = ObjectKey::new(pack_index_storage_key(shard_id)).map_err(|_| {
            SdkError::InvalidRequest {
                field: "pack_index_key",
                reason: "derived pack-index storage key is invalid",
            }
        })?;
        self.read_chunk_at(chunk_id, &key)
    }

    /// Reads exactly the transformed payload using an explicit shard key.
    pub fn read_chunk_at(
        &mut self,
        chunk_id: ChunkId,
        index_key: &ObjectKey,
    ) -> SdkResult<Option<PackChunkRead>> {
        let Some(entry) = self.lookup_at(chunk_id, index_key)? else {
            return Ok(None);
        };
        let pack_key =
            ObjectKey::new(format!("packs/{}", entry.pack_id().as_hex())).map_err(|_| {
                SdkError::InvalidRequest {
                    field: "pack_key",
                    reason: "derived pack storage key is invalid",
                }
            })?;
        let metadata = match self.storage.as_storage().metadata(&pack_key) {
            Ok(metadata) => metadata,
            Err(StorageError::NotFound) => return Err(SdkError::RepositoryRequiredObjectMissing),
            Err(error) => return Err(map_pack_index_storage_error(error, "read_pack_metadata")),
        };
        let range = entry
            .validate_against_pack_length(metadata.size())
            .map_err(map_pack_index_range_error)?;
        let storage_range = ObjectRange::new(range.offset(), range.length()).map_err(|_| {
            SdkError::RepositoryMalformed {
                reason: "pack-index payload range is invalid",
            }
        })?;
        let object = match self
            .storage
            .as_storage()
            .read_range(&pack_key, storage_range)
        {
            Ok(object) => object,
            Err(StorageError::NotFound) => return Err(SdkError::RepositoryRequiredObjectMissing),
            Err(error) => return Err(map_pack_index_storage_error(error, "read_pack_range")),
        };
        Ok(Some(PackChunkRead::new(entry, range, object)))
    }

    /// Reads one transformed payload into a bounded vector.
    pub fn read_chunk_payload(&mut self, chunk_id: ChunkId) -> SdkResult<Option<Vec<u8>>> {
        let Some(read) = self.read_chunk(chunk_id)? else {
            return Ok(None);
        };
        let expected_size = read.entry().stored_length();
        let mut reader = read.into_reader();
        read_stream_to_vec(reader.as_mut(), Some(expected_size))
            .map(Some)
            .map_err(|error| map_pack_index_storage_error(error, "read_pack_range"))
    }
}

impl fmt::Debug for PackIndexLookup {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("PackIndexLookup")
            .field("configuration", &self.configuration)
            .field("cache", &self.cache)
            .finish()
    }
}

/// A verified, zero-copy view over an immutable pack.
pub struct PackReader<'a> {
    inner: VerifiedPack<'a>,
}

impl<'a> PackReader<'a> {
    /// Validates the complete pack before exposing any entry payload.
    pub fn new(bytes: &'a [u8]) -> SdkResult<Self> {
        Ok(Self {
            inner: VerifiedPack::new(bytes).map_err(map_pack_format_error)?,
        })
    }

    /// Returns verified pack metadata.
    pub const fn metadata(&self) -> PackMetadata {
        self.inner.metadata()
    }

    /// Returns verified entry locations in file order.
    pub fn entries(&self) -> &[PackEntryLocation] {
        self.inner.entries()
    }

    /// Returns one transformed payload without copying it.
    pub fn payload(&self, location: &PackEntryLocation) -> SdkResult<&'a [u8]> {
        self.inner.payload(location).map_err(map_pack_format_error)
    }
}

/// Verifies an immutable pack and returns its metadata.
pub fn verify_pack(bytes: &[u8]) -> SdkResult<PackMetadata> {
    PackReader::new(bytes).map(|reader| reader.metadata())
}

fn map_pack_format_error(error: PackFormatError) -> SdkError {
    match error {
        PackFormatError::UnsupportedVersion { version } => {
            SdkError::RepositoryUnsupportedVersion { version }
        }
        PackFormatError::BuilderFinished => SdkError::InvalidRequest {
            field: "pack_builder",
            reason: "pack builder is already finished",
        },
        PackFormatError::BuilderAborted => SdkError::InvalidRequest {
            field: "pack_builder",
            reason: "pack builder is already aborted",
        },
        PackFormatError::PackTooLarge => SdkError::InvalidRequest {
            field: "pack",
            reason: "pack exceeds the configured or SDK size limit",
        },
        PackFormatError::InvalidLocation => SdkError::InvalidRequest {
            field: "pack_entry_location",
            reason: "entry location does not belong to this verified pack",
        },
        PackFormatError::InvalidMagic => SdkError::RepositoryMalformed {
            reason: "immutable pack magic is invalid",
        },
        PackFormatError::InvalidField => SdkError::RepositoryMalformed {
            reason: "immutable pack contains an invalid field",
        },
        PackFormatError::InvalidLength => SdkError::RepositoryMalformed {
            reason: "immutable pack contains an invalid length or offset",
        },
        PackFormatError::InvalidChecksum => SdkError::RepositoryMalformed {
            reason: "immutable pack integrity check failed",
        },
        PackFormatError::Truncated => SdkError::RepositoryMalformed {
            reason: "immutable pack is truncated",
        },
        PackFormatError::TrailingData => SdkError::RepositoryMalformed {
            reason: "immutable pack contains trailing data",
        },
        PackFormatError::AllocationFailure => SdkError::RepositoryMalformed {
            reason: "immutable pack exceeds the available allocation limit",
        },
    }
}

fn map_pack_index_format_error(error: PackIndexFormatError) -> SdkError {
    match error {
        PackIndexFormatError::UnsupportedVersion { version } => {
            SdkError::RepositoryUnsupportedVersion { version }
        }
        PackIndexFormatError::UnsupportedCodec | PackIndexFormatError::UnsupportedEncryption => {
            SdkError::RepositoryIncompatible {
                reason: "pack-index transform metadata is not supported",
            }
        }
        PackIndexFormatError::BuilderFinished => SdkError::InvalidRequest {
            field: "pack_index_builder",
            reason: "pack-index builder is already finished",
        },
        PackIndexFormatError::BuilderAborted => SdkError::InvalidRequest {
            field: "pack_index_builder",
            reason: "pack-index builder is already aborted",
        },
        PackIndexFormatError::WrongShard => SdkError::InvalidRequest {
            field: "pack_index_shard",
            reason: "entry does not belong to the selected chunk-ID shard",
        },
        PackIndexFormatError::ShardTooLarge => SdkError::InvalidRequest {
            field: "pack_index_shard",
            reason: "pack-index shard exceeds the configured size limit",
        },
        PackIndexFormatError::DuplicateChunkId => SdkError::RepositoryMalformed {
            reason: "pack-index shard contains a duplicate chunk ID",
        },
        PackIndexFormatError::InvalidMagic => SdkError::RepositoryMalformed {
            reason: "pack-index shard magic is invalid",
        },
        PackIndexFormatError::InvalidField => SdkError::RepositoryMalformed {
            reason: "pack-index shard contains an invalid field",
        },
        PackIndexFormatError::InvalidLength => SdkError::RepositoryMalformed {
            reason: "pack-index shard contains an invalid length or offset",
        },
        PackIndexFormatError::InvalidChecksum => SdkError::RepositoryMalformed {
            reason: "pack-index shard integrity check failed",
        },
        PackIndexFormatError::Truncated => SdkError::RepositoryMalformed {
            reason: "pack-index shard is truncated",
        },
        PackIndexFormatError::TrailingData => SdkError::RepositoryMalformed {
            reason: "pack-index shard contains trailing data",
        },
        PackIndexFormatError::AllocationFailure => SdkError::RepositoryMalformed {
            reason: "pack-index shard exceeds the available allocation limit",
        },
    }
}

fn map_pack_index_entry_error(error: PackIndexEntryError) -> SdkError {
    let _ = error;
    SdkError::InvalidRequest {
        field: "pack_index_entry",
        reason: "pack-index entry coordinates are invalid",
    }
}

fn map_pack_index_range_error(error: PackIndexRangeError) -> SdkError {
    let _ = error;
    SdkError::RepositoryMalformed {
        reason: "pack-index range exceeds the containing pack",
    }
}

fn map_pack_index_storage_error(error: StorageError, operation: &'static str) -> SdkError {
    match error {
        StorageError::UnsupportedCapability => SdkError::StorageCapabilityUnsupported,
        StorageError::Cancelled => SdkError::OperationCancelled { operation_id: None },
        _ => SdkError::StorageFailure { operation },
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

    /// Returns the content ID calculated from this snapshot's canonical
    /// plaintext payload.
    pub fn object_id(&self) -> SdkResult<ObjectId> {
        snapshot_object_id(self).map_err(map_snapshot_format_error)
    }

    /// Alias for [`Self::object_id`].
    pub fn content_id(&self) -> SdkResult<ObjectId> {
        self.object_id()
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

/// Calculates an immutable object ID from a kind, payload version, and
/// canonical plaintext payload. The hash includes only the kind, version, and
/// payload (with the documented domain separator); codec, encryption, length,
/// and checksum metadata do not affect the result.
pub fn object_id_for_content(
    kind: ObjectKind,
    object_version: u16,
    canonical_plaintext: &[u8],
) -> ObjectId {
    calculate_object_id(kind, object_version, canonical_plaintext)
}

/// Alias for [`object_id_for_content`].
pub fn calculate_object_id_for_content(
    kind: ObjectKind,
    object_version: u16,
    canonical_plaintext: &[u8],
) -> ObjectId {
    object_id_for_content(kind, object_version, canonical_plaintext)
}

/// Encodes canonical plaintext in the common immutable-object envelope.
///
/// Uncompressed and unencrypted objects retain the released version-1 bytes.
/// Zstandard objects use the default level and a version-2 envelope. An
/// encrypted request fails with [`SdkError::RepositoryEncryptionKeyRequired`]
/// unless [`encode_immutable_object_with_options`] is used with an
/// explicit repository encryption context.
pub fn encode_immutable_object(
    kind: ObjectKind,
    object_version: u16,
    codec: ObjectCodec,
    encryption: ObjectEncryption,
    canonical_plaintext: &[u8],
) -> SdkResult<Vec<u8>> {
    encode_object_envelope(kind, object_version, codec, encryption, canonical_plaintext)
        .map_err(map_object_format_error)
}

/// Encodes canonical plaintext with validated transport options.
///
/// The optional encryption context is required exactly when
/// `options.encryption()` is [`ObjectEncryption::XChaCha20Poly1305`]. The
/// context's salt is persisted in the envelope; its derived key is not.
pub fn encode_immutable_object_with_options(
    kind: ObjectKind,
    object_version: u16,
    options: ObjectTransformOptions,
    encryption_context: Option<&RepositoryEncryption>,
    canonical_plaintext: &[u8],
) -> SdkResult<Vec<u8>> {
    encode_object_envelope_with_options(
        kind,
        object_version,
        options,
        encryption_context.map(RepositoryEncryption::context),
        canonical_plaintext,
    )
    .map_err(map_object_format_error)
}

/// Encodes canonical plaintext using an explicit repository encryption
/// context.
pub fn encode_immutable_object_with_encryption(
    kind: ObjectKind,
    object_version: u16,
    options: ObjectTransformOptions,
    encryption_context: &RepositoryEncryption,
    canonical_plaintext: &[u8],
) -> SdkResult<Vec<u8>> {
    encode_object_envelope_with_encryption(
        kind,
        object_version,
        options,
        encryption_context.context(),
        canonical_plaintext,
    )
    .map_err(map_object_format_error)
}

/// Encodes canonical plaintext after deriving a repository key from a
/// password and the supplied per-repository salt.
pub fn encode_immutable_object_with_password(
    kind: ObjectKind,
    object_version: u16,
    codec: ObjectCodec,
    encryption: ObjectEncryption,
    password: &[u8],
    salt: RepositorySalt,
    canonical_plaintext: &[u8],
) -> SdkResult<Vec<u8>> {
    encode_immutable_object_with_password_and_options(
        kind,
        object_version,
        ObjectTransformOptions::new(codec, encryption),
        password,
        salt,
        canonical_plaintext,
    )
}

/// Encodes canonical plaintext after deriving a repository key with explicit
/// compression options.
pub fn encode_immutable_object_with_password_and_options(
    kind: ObjectKind,
    object_version: u16,
    options: ObjectTransformOptions,
    password: &[u8],
    salt: RepositorySalt,
    canonical_plaintext: &[u8],
) -> SdkResult<Vec<u8>> {
    if password.is_empty() {
        return Err(SdkError::InvalidRequest {
            field: "password",
            reason: "must not be empty",
        });
    }
    encode_object_envelope_with_password(
        kind,
        object_version,
        options,
        password,
        salt,
        canonical_plaintext,
    )
    .map_err(map_object_format_error)
}

/// Encodes canonical plaintext using the current uncompressed, unencrypted
/// object representation.
pub fn encode_object(
    kind: ObjectKind,
    object_version: u16,
    canonical_plaintext: &[u8],
) -> SdkResult<Vec<u8>> {
    encode_immutable_object(
        kind,
        object_version,
        ObjectCodec::None,
        ObjectEncryption::None,
        canonical_plaintext,
    )
}

/// Decodes and authenticates a common immutable-object envelope.
pub fn decode_immutable_object(bytes: &[u8]) -> SdkResult<ImmutableObject> {
    decode_object_envelope(bytes).map_err(map_object_format_error)
}

/// Decodes an object using an explicit repository encryption context.
pub fn decode_immutable_object_with_encryption(
    bytes: &[u8],
    encryption_context: &RepositoryEncryption,
) -> SdkResult<ImmutableObject> {
    decode_object_envelope_with_encryption(bytes, encryption_context.context())
        .map_err(map_object_format_error)
}

/// Decodes an object by deriving its key from the password and the salt
/// recorded in the validated envelope.
pub fn decode_immutable_object_with_password(
    bytes: &[u8],
    password: &[u8],
) -> SdkResult<ImmutableObject> {
    if password.is_empty() {
        return Err(SdkError::InvalidRequest {
            field: "password",
            reason: "must not be empty",
        });
    }
    decode_object_envelope_with_password(bytes, password).map_err(map_object_format_error)
}

/// Alias for [`decode_immutable_object`].
pub fn decode_object(bytes: &[u8]) -> SdkResult<ImmutableObject> {
    decode_immutable_object(bytes)
}

/// Reads, bounds, decodes, and authenticates an immutable object.
pub fn decode_immutable_object_from_reader<R: Read>(mut reader: R) -> SdkResult<ImmutableObject> {
    decode_object_envelope_from_reader(&mut reader).map_err(map_object_format_error)
}

/// Reads, bounds, and decodes an object using an explicit encryption context.
pub fn decode_immutable_object_from_reader_with_encryption<R: Read>(
    mut reader: R,
    encryption_context: &RepositoryEncryption,
) -> SdkResult<ImmutableObject> {
    decode_object_envelope_from_reader_with_encryption(&mut reader, encryption_context.context())
        .map_err(map_object_format_error)
}

/// Reads, bounds, and decodes an object using a password.
pub fn decode_immutable_object_from_reader_with_password<R: Read>(
    mut reader: R,
    password: &[u8],
) -> SdkResult<ImmutableObject> {
    if password.is_empty() {
        return Err(SdkError::InvalidRequest {
            field: "password",
            reason: "must not be empty",
        });
    }
    decode_object_envelope_from_reader_with_password(&mut reader, password)
        .map_err(map_object_format_error)
}

fn map_snapshot_format_error(error: FormatError) -> SdkError {
    match error {
        FormatError::UnsupportedVersion { version }
        | FormatError::UnsupportedObjectVersion { version } => {
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
        | FormatError::Serialization
        | FormatError::InvalidObjectKind
        | FormatError::InvalidCodec
        | FormatError::InvalidEncryption
        | FormatError::InvalidLength
        | FormatError::InvalidDigestLength
        | FormatError::InvalidObjectId
        | FormatError::InvalidPayloadChecksum
        | FormatError::InvalidEnvelopeChecksum
        | FormatError::InvalidCompressionLevel
        | FormatError::InvalidTransformMetadata
        | FormatError::InvalidNonce => SdkError::RepositoryMalformed {
            reason: "snapshot object contains an invalid field",
        },
        FormatError::EncryptionKeyRequired => SdkError::RepositoryEncryptionKeyRequired,
        FormatError::EncryptionKeyMismatch | FormatError::AuthenticationFailure => {
            SdkError::RepositoryAuthenticationFailed
        }
        FormatError::KdfFailure => SdkError::RepositoryTransformFailed {
            reason: "repository encryption key derivation failed",
        },
        FormatError::CompressionFailure => SdkError::RepositoryTransformFailed {
            reason: "repository object compression failed",
        },
        FormatError::DecompressionFailure => SdkError::RepositoryTransformFailed {
            reason: "repository object decompression failed",
        },
        FormatError::RandomnessFailure => SdkError::RepositoryTransformFailed {
            reason: "secure random generation failed",
        },
        FormatError::UnsupportedCodec | FormatError::UnsupportedEncryption => {
            SdkError::RepositoryIncompatible {
                reason: "snapshot object transport metadata is not supported",
            }
        }
    }
}

pub(crate) fn map_object_format_error(error: FormatError) -> SdkError {
    match error {
        FormatError::UnsupportedVersion { version }
        | FormatError::UnsupportedObjectVersion { version } => {
            SdkError::RepositoryUnsupportedVersion { version }
        }
        FormatError::UnsupportedCodec | FormatError::UnsupportedEncryption => {
            SdkError::RepositoryIncompatible {
                reason: "immutable object transport metadata is not supported",
            }
        }
        FormatError::InvalidEncoding => SdkError::RepositoryMalformed {
            reason: "immutable object is not valid MessagePack",
        },
        FormatError::InputTooLarge => SdkError::RepositoryMalformed {
            reason: "immutable object exceeds the supported size limit",
        },
        FormatError::TrailingBytes => SdkError::RepositoryMalformed {
            reason: "immutable object contains trailing bytes",
        },
        FormatError::InvalidMagic => SdkError::RepositoryMalformed {
            reason: "immutable object magic is invalid",
        },
        FormatError::InvalidObjectKind => SdkError::RepositoryMalformed {
            reason: "immutable object kind is invalid",
        },
        FormatError::InvalidCodec | FormatError::InvalidEncryption => {
            SdkError::RepositoryMalformed {
                reason: "immutable object transport metadata is invalid",
            }
        }
        FormatError::InvalidLength => SdkError::RepositoryMalformed {
            reason: "immutable object length is invalid",
        },
        FormatError::InvalidDigestLength => SdkError::RepositoryMalformed {
            reason: "immutable object digest length is invalid",
        },
        FormatError::InvalidObjectId => SdkError::RepositoryMalformed {
            reason: "immutable object identity check failed",
        },
        FormatError::InvalidPayloadChecksum | FormatError::InvalidEnvelopeChecksum => {
            SdkError::RepositoryMalformed {
                reason: "immutable object integrity check failed",
            }
        }
        FormatError::EncryptionKeyRequired => SdkError::RepositoryEncryptionKeyRequired,
        FormatError::EncryptionKeyMismatch | FormatError::AuthenticationFailure => {
            SdkError::RepositoryAuthenticationFailed
        }
        FormatError::InvalidCompressionLevel | FormatError::InvalidTransformMetadata => {
            SdkError::RepositoryTransformFailed {
                reason: "immutable object transform metadata is invalid",
            }
        }
        FormatError::InvalidNonce => SdkError::RepositoryTransformFailed {
            reason: "immutable object encryption nonce is invalid",
        },
        FormatError::KdfFailure => SdkError::RepositoryTransformFailed {
            reason: "repository encryption key derivation failed",
        },
        FormatError::CompressionFailure => SdkError::RepositoryTransformFailed {
            reason: "repository object compression failed",
        },
        FormatError::DecompressionFailure => SdkError::RepositoryTransformFailed {
            reason: "repository object decompression failed",
        },
        FormatError::RandomnessFailure => SdkError::RepositoryTransformFailed {
            reason: "secure random generation failed",
        },
        FormatError::InvalidRootReference
        | FormatError::MissingRequiredFeature
        | FormatError::UnsupportedRequiredFeature
        | FormatError::VersionMismatch
        | FormatError::InvalidChecksum
        | FormatError::InvalidField
        | FormatError::Serialization => SdkError::RepositoryMalformed {
            reason: "immutable object contains an invalid field",
        },
    }
}

/// Alias for the current repository format version.
pub const REPOSITORY_FORMAT_VERSION: u16 = CURRENT_REPOSITORY_FORMAT_VERSION;

/// Alias for the current repository bootstrap schema version.
pub const REPOSITORY_BOOTSTRAP_VERSION: u16 = CURRENT_REPOSITORY_BOOTSTRAP_VERSION;

/// Alias for the current repository descriptor version.
pub const REPOSITORY_DESCRIPTOR_VERSION: u16 = CURRENT_REPOSITORY_DESCRIPTOR_VERSION;
