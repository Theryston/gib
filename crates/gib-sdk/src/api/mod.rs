mod backup;
mod builder;
mod client;
mod configuration;
mod error;
mod event;
mod filesystem;
mod identity;
mod operation;
mod repository;
mod storage_configuration;
mod tree;

pub use crate::domain::{
    BackupBudgetError, BackupBudgets, BackupMetrics, BackupResource, BackupStage,
    DEFAULT_BACKUP_CPU_WORKERS, DEFAULT_BACKUP_FILE_DESCRIPTORS, DEFAULT_BACKUP_MEMORY_BYTES,
    DEFAULT_BACKUP_NETWORK_REQUESTS, DEFAULT_BACKUP_QUEUE_CAPACITY, MAX_BACKUP_QUEUE_CAPACITY,
    MIN_BACKUP_CPU_WORKERS, MIN_BACKUP_FILE_DESCRIPTORS, MIN_BACKUP_NETWORK_REQUESTS,
};
pub use backup::{BackupHandle, BackupPipeline, BackupRequest, BackupResult};
pub use builder::{ClientBuilder, DEFAULT_EVENT_BUFFER_CAPACITY};
pub use client::Client;
pub use configuration::{
    BackupConfig, BackupConfiguration, ByteSize, CONFIGURATION_FILE_NAME,
    CURRENT_CONFIGURATION_VERSION, CURRENT_GIB_CONFIGURATION_VERSION, ConfigError, ConfigErrorKind,
    Configuration, ConfigurationFileError, ConfigurationFileMetadata, ConfigurationFileSystem,
    ConfigurationOverrides, ConfigurationParseError, ConfigurationResolution,
    ConfigurationResolutionRequest, ConfigurationResolver, ConfigurationSelection,
    ConfigurationSource, ConfigurationSourceEvent, GIB_CONFIGURATION_FILE_NAME, GibConfig,
    GibConfiguration, GibConfigurationError, GibConfigurationErrorKind,
    LOCAL_CONFIGURATION_FILE_NAME, LiveConfig, LiveConfiguration, LocalConfig,
    LocalConfigurationFileSystem, MAX_BACKUP_CONCURRENCY, MAX_CHUNK_SIZE_BYTES,
    MAX_COMPRESSION_LEVEL, MAX_CONFIGURATION_BYTES, MAX_LIVE_INTERVAL_MS, MIN_COMPRESSION_LEVEL,
    OsConfigurationFileSystem, PROJECT_CONFIGURATION_FILE_NAME, ProjectConfig, ProjectConfigError,
    ProjectConfiguration, ProjectConfigurationError, ProjectConfigurationErrorKind,
    ProjectConfigurationFileMetadata, ProjectConfigurationFileSystem,
    ProjectRepositoryConfiguration, RepositoryConfig, RepositoryConfiguration,
    ResolvedConfiguration, RestoreConfig, RestoreConfiguration, discover_configuration,
    discover_configuration_with_file_system, load_configuration, merge_ignore_patterns,
    merge_ignore_rules, parse_configuration, resolve_configuration,
    resolve_configuration_with_file_system,
};
pub use error::{ErrorCode, ErrorSummary, Result, SdkError, SdkResult};
pub use event::{
    CancellationInfo, EVENT_SCHEMA_VERSION, EventConsumer, EventDelivery, EventDispatcher,
    EventEnvelope, EventKind, EventMessage, EventPayload, EventPhase, EventSubscription, Progress,
    RecoveryPoint,
};
pub use filesystem::*;
pub use identity::{
    Author, AuthorIdentity, CURRENT_IDENTITY_CONFIGURATION_VERSION, ConfigurationError,
    ConfigurationHandle, ConfigurationResult, ConfigurationStorage, GLOBAL_CONFIG_DIRECTORY,
    GLOBAL_CONFIG_FILE_NAME, GLOBAL_CONFIGURATION_DIRECTORY, GlobalConfiguration,
    IDENTITY_CONFIGURATION_FILE_NAME, IdentityConfigurationStorage, IdentityStorageResult,
    LocalConfiguration, LocalIdentityConfiguration, MAX_AUTHOR_IDENTITY_LENGTH, MAX_AUTHOR_LENGTH,
    MemoryConfiguration, MemoryIdentityConfiguration, SetAuthorRequest, SetIdentityRequest,
    UserIdentity, get_author, get_global_identity, get_identity, read_identity, set_author,
    set_global_identity, set_identity,
};
pub use operation::{
    CancellationHandle, CancellationToken, OperationHandle, OperationId, OperationKind,
    OperationRequest, OperationResult, OperationStatus, Request,
};
pub use repository::{
    ARGON2ID_MEMORY_COST_KIB, ARGON2ID_PARALLELISM, ARGON2ID_TIME_COST, BackupReference, ByteRange,
    CURRENT_INDEX_OBJECT_VERSION, CURRENT_OBJECT_ENVELOPE_VERSION, CURRENT_PACK_OBJECT_VERSION,
    CURRENT_REPOSITORY_BOOTSTRAP_VERSION, CURRENT_REPOSITORY_DESCRIPTOR_VERSION,
    CURRENT_REPOSITORY_FORMAT_VERSION, CURRENT_REPOSITORY_HEAD_VERSION,
    CURRENT_SNAPSHOT_HISTORY_VERSION, CURRENT_SNAPSHOT_SUMMARY_VERSION, CURRENT_SNAPSHOT_VERSION,
    CURRENT_TRANSFORMED_OBJECT_ENVELOPE_VERSION, CURRENT_TREE_OBJECT_VERSION, CompressionLevel,
    CompressionLevelError, DEFAULT_OBJECT_LIST_PAGE_SIZE, DEFAULT_SNAPSHOT_PAGE_SIZE,
    DEFAULT_ZSTD_COMPRESSION_LEVEL, FORMAT_OBJECT_KEY, HEAD_OBJECT_KEY, Head, HeadPublication,
    HeadRead, HeadState, ImmutableObject, InitializeRepositoryRequest, LATEST_REF_OBJECT_KEY,
    LATEST_SNAPSHOT_ALIAS, ListCursor, LocalStorage, LocalStorageOperation,
    MAX_IMMUTABLE_OBJECT_BYTES, MAX_IMMUTABLE_OBJECT_PAYLOAD_BYTES,
    MAX_IMMUTABLE_OBJECT_STORED_PAYLOAD_BYTES, MAX_OBJECT_LIST_PAGE_SIZE,
    MAX_SNAPSHOT_AUTHOR_LENGTH, MAX_SNAPSHOT_CURSOR_LENGTH, MAX_SNAPSHOT_ID_LENGTH,
    MAX_SNAPSHOT_MESSAGE_LENGTH, MAX_SNAPSHOT_PAGE_SIZE, MemoryStorage, MemoryStorageOperation,
    OBJECT_ID_HEX_LENGTH, ObjectCodec, ObjectCursor, ObjectEncryption, ObjectId, ObjectKey,
    ObjectKind, ObjectListPage, ObjectListRequest, ObjectMetadata, ObjectPrefix, ObjectRange,
    ObjectRead, ObjectReader, ObjectStorage, ObjectTransformOptions, ObjectWriteOptions,
    OpenRepositoryRequest, REPOSITORY_BOOTSTRAP_VERSION, REPOSITORY_DESCRIPTOR_OBJECT_KEY,
    REPOSITORY_DESCRIPTOR_VERSION, REPOSITORY_ENCRYPTION_KDF, REPOSITORY_ENCRYPTION_KEY_LENGTH,
    REPOSITORY_ENCRYPTION_SALT_LENGTH, REPOSITORY_FORMAT_VERSION, REPOSITORY_HEAD_KEY,
    REPOSITORY_HEAD_OBJECT_KEY, REPOSITORY_HEAD_VERSION, REPOSITORY_MAGIC,
    REQUIRED_REPOSITORY_FEATURE, Repository, RepositoryDescriptor, RepositoryEncryption,
    RepositoryFeature, RepositoryHead, RepositoryHeadRead, RepositoryHeadState, RepositoryId,
    RepositoryIdentity, RepositoryInitRequest, RepositoryInitializationRequest, RepositoryKey,
    RepositoryObject, RepositoryOpenRequest, RepositoryRoots, RepositorySalt, RepositorySaltError,
    RepositoryStorage, SNAPSHOT_HISTORY_OBJECT_PREFIX, SNAPSHOT_OBJECT_PREFIX,
    STORAGE_TRANSFER_BUFFER_SIZE, Snapshot, SnapshotCursor, SnapshotHistoryPage,
    SnapshotHistoryRequest, SnapshotId, SnapshotListRequest, SnapshotPage, SnapshotPublication,
    SnapshotPublicationRequest, SnapshotRef, SnapshotReference, SnapshotReferenceInput,
    SnapshotReferenceSelector, SnapshotSelector, SnapshotSummary, SnapshotSummaryListRequest,
    SnapshotSummaryPage, StorageCapabilities, StorageCapability, StorageError, StorageHandle,
    StorageKey, StorageListPage, StorageListRequest, StorageMetadata, StoragePrefix, StorageRange,
    StorageReader, StorageResult, StorageVersion, StorageVersionToken, StorageWriteCondition,
    StorageWriteOptions, VersionToken, VersionedHead, VersionedObject, VersionedStorageObject,
    WriteCondition, XCHACHA20_POLY1305_NONCE_LENGTH, XCHACHA20_POLY1305_TAG_LENGTH,
    calculate_object_id_for_content, decode_immutable_object, decode_immutable_object_from_reader,
    decode_immutable_object_from_reader_with_encryption,
    decode_immutable_object_from_reader_with_password, decode_immutable_object_with_encryption,
    decode_immutable_object_with_password, decode_object, decode_snapshot_object,
    encode_immutable_object, encode_immutable_object_with_encryption,
    encode_immutable_object_with_options, encode_immutable_object_with_password,
    encode_immutable_object_with_password_and_options, encode_object, encode_snapshot_object,
    initialize_repository, object_id_for_content, open_repository,
};
#[cfg(feature = "async")]
pub use repository::{AsyncChunkStream, AsyncChunker, async_chunk_reader};
pub use repository::{
    BUZHASH_TABLE_SEED, BUZHASH_WINDOW_SIZE, CHUNK_BUFFER_POOL_CAPACITY, CHUNKING_READ_BUFFER_SIZE,
    CONTENT_DEFINED_CHUNKING_ALGORITHM, CURRENT_CHUNKING_VERSION, Chunk, ChunkBoundary, ChunkId,
    ChunkIdError, ChunkStream, Chunker, ChunkingConfiguration, ChunkingConfigurationError,
    ChunkingError, ChunkingResult, DEFAULT_MAX_CHUNK_SIZE_BYTES, DEFAULT_MIN_CHUNK_SIZE_BYTES,
    DEFAULT_TARGET_CHUNK_SIZE_BYTES, MAX_CONTENT_DEFINED_CHUNK_SIZE_BYTES, chunk_reader,
    chunk_reader_with_cancellation,
};
pub use repository::{
    CURRENT_PACK_FORMAT_VERSION, DEFAULT_PACK_MAX_SIZE_BYTES, DEFAULT_PACK_TARGET_SIZE_BYTES,
    MAX_PACK_SIZE_BYTES, PACK_ALIGNMENT, PACK_ENTRY_HEADER_LENGTH, PACK_FOOTER_LENGTH,
    PACK_HEADER_LENGTH, PackBuilder, PackConfiguration, PackConfigurationError, PackEntryError,
    PackEntryInput, PackEntryLocation, PackId, PackIdError, PackMetadata, PackPublisher,
    PackReader, SealedPack, verify_pack,
};
pub use repository::{
    CURRENT_PACK_INDEX_FORMAT_VERSION, DEFAULT_PACK_INDEX_CACHE_MAX_BYTES,
    DEFAULT_PACK_INDEX_CACHE_MAX_SHARDS, DEFAULT_PACK_INDEX_MAX_SHARD_BYTES,
    MAX_PACK_INDEX_CACHE_BYTES, MAX_PACK_INDEX_SHARD_BYTES, MIN_PACK_INDEX_SHARD_BYTES,
    PACK_INDEX_ALIGNMENT, PACK_INDEX_FOOTER_LENGTH, PACK_INDEX_HEADER_LENGTH,
    PACK_INDEX_RECORD_LENGTH, PACK_INDEX_SHARD_COUNT, PACK_INDEX_SHARD_PREFIX_BYTES,
    PACK_INDEX_STORAGE_PREFIX, PackChunkRead, PackIndexCache, PackIndexCacheConfiguration,
    PackIndexCacheConfigurationError, PackIndexConfiguration, PackIndexConfigurationError,
    PackIndexEntry, PackIndexEntryError, PackIndexId, PackIndexIdError, PackIndexLookup,
    PackIndexPublisher, PackIndexRange, PackIndexRangeError, PackIndexReader,
    PackIndexShardBuilder, PackIndexShardId, PackIndexShardMetadata, PackIndexStoragePublisher,
    PackIndexTransform, PackIndexTransformError, SealedPackIndexShard, pack_index_object_key,
    pack_index_storage_key, verify_pack_index,
};
#[cfg(feature = "s3")]
pub use repository::{
    DEFAULT_S3_CAPABILITY_CACHE_FILE_NAME, DEFAULT_S3_CAPABILITY_CACHE_TTL_SECONDS,
    DEFAULT_S3_MAX_CONCURRENCY, DEFAULT_S3_MULTIPART_PART_SIZE, DEFAULT_S3_MULTIPART_THRESHOLD,
    MAX_S3_MULTIPART_PART_SIZE, MAX_S3_MULTIPART_THRESHOLD, MAX_S3_MULTIPART_UPLOAD_PARTS,
    MIN_S3_MULTIPART_PART_SIZE, S3ConditionalWriteCapabilities, S3ConditionalWriteStatus,
    S3Storage, S3StorageConfig,
};
#[cfg(feature = "webdav")]
pub use repository::{
    DEFAULT_WEBDAV_MAX_CONCURRENCY, DEFAULT_WEBDAV_REQUEST_TIMEOUT,
    DEFAULT_WEBDAV_TRANSFER_BUFFER_SIZE, MAX_WEBDAV_MAX_CONCURRENCY, WebDavStorage,
    WebDavStorageConfig,
};
pub use storage_configuration::*;
pub use tree::*;
