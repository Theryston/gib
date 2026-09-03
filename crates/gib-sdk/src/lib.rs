//! Public SDK contracts for Gib.
//!
//! The crate root intentionally exposes only stable API types. Repository
//! lifecycle operations use validated domain values and injectable storage
//! backends. Repository metadata and the atomically published HEAD are
//! persisted as versioned MessagePack bytes; the SDK does not write JSON.
//!
//! ```
//! use gib::{Client, EventEnvelope, OperationKind};
//!
//! # fn main() -> gib::SdkResult<()> {
//! let client = Client::builder().event_buffer_capacity(8).build()?;
//! let subscription = client.register_event_consumer(|_event: EventEnvelope| {})?;
//! let operation = client.create_operation(OperationKind::Backup)?;
//! let _result = operation.cancel()?;
//! drop(subscription);
//! # Ok(())
//! # }
//! ```

#![warn(missing_docs)]
#![deny(rustdoc::broken_intra_doc_links)]

mod api;
mod application;
mod domain;
mod format;
mod infrastructure;

pub use api::{
    ARGON2ID_MEMORY_COST_KIB, ARGON2ID_PARALLELISM, ARGON2ID_TIME_COST, AddStorageRequest, Author,
    AuthorIdentity, BUZHASH_TABLE_SEED, BUZHASH_WINDOW_SIZE, BackupReference, ByteRange,
    CHUNK_BUFFER_POOL_CAPACITY, CHUNKING_READ_BUFFER_SIZE, CONTENT_DEFINED_CHUNKING_ALGORITHM,
    CURRENT_CHUNKING_VERSION, CURRENT_IDENTITY_CONFIGURATION_VERSION, CURRENT_INDEX_OBJECT_VERSION,
    CURRENT_OBJECT_ENVELOPE_VERSION, CURRENT_PACK_OBJECT_VERSION, CURRENT_SNAPSHOT_HISTORY_VERSION,
    CURRENT_SNAPSHOT_SUMMARY_VERSION, CURRENT_SNAPSHOT_VERSION, CURRENT_STORAGE_BACKEND_VERSION,
    CURRENT_STORAGE_CONFIGURATION_VERSION, CURRENT_TRANSFORMED_OBJECT_ENVELOPE_VERSION,
    CURRENT_TREE_OBJECT_VERSION, CancellationHandle, CancellationInfo, CancellationToken, Chunk,
    ChunkBoundary, ChunkId, ChunkIdError, ChunkStream, Chunker, ChunkingConfiguration,
    ChunkingConfigurationError, ChunkingError, ChunkingResult, Client, ClientBuilder,
    CompressionLevel, CompressionLevelError, ConfigurationError, ConfigurationHandle,
    ConfigurationResult, ConfigurationStorage, CredentialReference, CredentialStore,
    CredentialStoreError, CredentialStoreHandle, CredentialStoreOperation, CredentialStoreResult,
    DEFAULT_EVENT_BUFFER_CAPACITY, DEFAULT_MAX_CHUNK_SIZE_BYTES, DEFAULT_MIN_CHUNK_SIZE_BYTES,
    DEFAULT_OBJECT_LIST_PAGE_SIZE, DEFAULT_SNAPSHOT_PAGE_SIZE, DEFAULT_TARGET_CHUNK_SIZE_BYTES,
    DEFAULT_ZSTD_COMPRESSION_LEVEL, DefaultStorageConnectivity, EVENT_SCHEMA_VERSION, ErrorCode,
    ErrorSummary, EventConsumer, EventDelivery, EventDispatcher, EventEnvelope, EventKind,
    EventMessage, EventPayload, EventPhase, EventSubscription, GLOBAL_CONFIG_DIRECTORY,
    GLOBAL_CONFIG_FILE_NAME, GLOBAL_CONFIGURATION_DIRECTORY, GlobalConfiguration, Head,
    HeadPublication, HeadRead, HeadState, IDENTITY_CONFIGURATION_FILE_NAME,
    IdentityConfigurationStorage, IdentityStorageResult, ImmutableObject,
    InitializeRepositoryRequest, LATEST_SNAPSHOT_ALIAS, ListCursor, ListStorageRequest,
    LocalConfiguration, LocalIdentityConfiguration, LocalStorage, LocalStorageConfiguration,
    LocalStorageOperation, LocalStorageSettings, MAX_AUTHOR_IDENTITY_LENGTH, MAX_AUTHOR_LENGTH,
    MAX_CONTENT_DEFINED_CHUNK_SIZE_BYTES, MAX_IMMUTABLE_OBJECT_BYTES,
    MAX_IMMUTABLE_OBJECT_PAYLOAD_BYTES, MAX_IMMUTABLE_OBJECT_STORED_PAYLOAD_BYTES,
    MAX_OBJECT_LIST_PAGE_SIZE, MAX_SNAPSHOT_AUTHOR_LENGTH, MAX_SNAPSHOT_CURSOR_LENGTH,
    MAX_SNAPSHOT_ID_LENGTH, MAX_SNAPSHOT_MESSAGE_LENGTH, MAX_SNAPSHOT_PAGE_SIZE,
    MAX_STORAGE_CONFIGURATION_BYTES, MAX_STORAGE_CREDENTIAL_LENGTH, MAX_STORAGE_NAME_LENGTH,
    MAX_STORAGE_SETTING_LENGTH, MemoryConfiguration, MemoryCredentialStore,
    MemoryIdentityConfiguration, MemoryStorage, MemoryStorageOperation, OBJECT_ID_HEX_LENGTH,
    ObjectCodec, ObjectCursor, ObjectEncryption, ObjectId, ObjectKey, ObjectKind, ObjectListPage,
    ObjectListRequest, ObjectMetadata, ObjectPrefix, ObjectRange, ObjectRead, ObjectReader,
    ObjectStorage, ObjectTransformOptions, ObjectWriteOptions, OpenRepositoryRequest,
    OperationHandle, OperationId, OperationKind, OperationRequest, OperationResult,
    OperationStatus, PlatformCredentialStore, Progress, REPOSITORY_BOOTSTRAP_VERSION,
    REPOSITORY_DESCRIPTOR_VERSION, REPOSITORY_ENCRYPTION_KDF, REPOSITORY_ENCRYPTION_KEY_LENGTH,
    REPOSITORY_ENCRYPTION_SALT_LENGTH, REPOSITORY_FORMAT_VERSION, RecoveryPoint,
    RemoveStorageRequest, Repository, RepositoryDescriptor, RepositoryEncryption,
    RepositoryFeature, RepositoryHead, RepositoryHeadRead, RepositoryHeadState, RepositoryId,
    RepositoryIdentity, RepositoryInitRequest, RepositoryInitializationRequest, RepositoryKey,
    RepositoryObject, RepositoryOpenRequest, RepositoryRoots, RepositorySalt, RepositorySaltError,
    RepositoryStorage, Request, Result, S3StorageCredentials, S3StorageSettings,
    SNAPSHOT_HISTORY_OBJECT_PREFIX, SNAPSHOT_OBJECT_PREFIX, STORAGE_CONFIGURATION_DIRECTORY_NAME,
    STORAGE_CONFIGURATION_FILE_SUFFIX, STORAGE_TRANSFER_BUFFER_SIZE, SdkError, SdkResult,
    SetAuthorRequest, SetIdentityRequest, Snapshot, SnapshotCursor, SnapshotHistoryPage,
    SnapshotHistoryRequest, SnapshotId, SnapshotListRequest, SnapshotPage, SnapshotPublication,
    SnapshotPublicationRequest, SnapshotRef, SnapshotReference, SnapshotReferenceInput,
    SnapshotReferenceSelector, SnapshotSelector, SnapshotSummary, SnapshotSummaryListRequest,
    SnapshotSummaryPage, StorageAddRequest, StorageAddResult, StorageBackend, StorageBackendKind,
    StorageCapabilities, StorageCapability, StorageConfiguration, StorageConfigurationError,
    StorageConfigurationListRequest, StorageConfigurationManager, StorageConfigurationMetadata,
    StorageConfigurationOperation, StorageConfigurationRepository, StorageConfigurationResult,
    StorageConfigurationStore, StorageConnectivity, StorageCredentialKind, StorageCredentials,
    StorageEntry, StorageError, StorageHandle, StorageHealth, StorageInfo, StorageKey,
    StorageListPage, StorageListRequest, StorageListResult, StorageManager, StorageMetadata,
    StorageName, StoragePrefix, StorageProbe, StorageRange, StorageReader, StorageRemoveRequest,
    StorageRemoveResult, StorageResult, StorageVersion, StorageVersionToken, StorageWriteCondition,
    StorageWriteOptions, UserIdentity, VersionToken, VersionedHead, VersionedObject,
    VersionedStorageObject, WebDavStorageCredentials, WebDavStorageSettings, WriteCondition,
    XCHACHA20_POLY1305_NONCE_LENGTH, XCHACHA20_POLY1305_TAG_LENGTH, add_storage,
    calculate_object_id_for_content, chunk_reader, chunk_reader_with_cancellation,
    decode_immutable_object, decode_immutable_object_from_reader,
    decode_immutable_object_from_reader_with_encryption,
    decode_immutable_object_from_reader_with_password, decode_immutable_object_with_encryption,
    decode_immutable_object_with_password, decode_object, decode_snapshot_object,
    encode_immutable_object, encode_immutable_object_with_encryption,
    encode_immutable_object_with_options, encode_immutable_object_with_password,
    encode_immutable_object_with_password_and_options, encode_object, encode_snapshot_object,
    get_author, get_global_identity, get_identity, initialize_repository, list_storages,
    object_id_for_content, open_repository, read_identity, remove_storage, set_author,
    set_global_identity, set_identity,
};

pub use api::{
    BuiltTree, CURRENT_TREE_METADATA_VERSION, CURRENT_TREE_NODE_VERSION, ChunkReference,
    ChunkReferenceError, DirectoryEntry, DirectoryNode, EncodedTreeNode, EntryName, EntryNameError,
    FileChunkReference, FileNode, FilePermissions, IncrementalTreeBuilder, LazyTree,
    MAX_FILE_CHUNK_REFERENCES, MAX_METADATA_EXTENSION_BYTES, MAX_METADATA_EXTENSIONS,
    MAX_METADATA_NAMESPACE_BYTES, MAX_SYMLINK_TARGET_BYTES, MAX_TREE_ENTRIES, MAX_TREE_NAME_BYTES,
    MAX_TREE_OBJECT_BYTES, MAX_TREE_PATH_BYTES, MetadataError, MetadataExtension,
    MetadataNamespace, MetadataNamespaceError, Name, NodeKind, NodeReference,
    NormalizedRelativePath, PermissionError, PortableMetadata, RegularFileNode, RelativePath,
    RelativePathError, RepositoryTreeStore, SnapshotTree, SymbolicLinkNode, SymlinkNode,
    SymlinkTarget, SymlinkTargetError, TreeBuilder, TreeEntry, TreeNode, TreeNodeId, TreeNodeKind,
    TreeNodeReference, TreeNodeStore, TreeObject, TreeObjectPublisher, TreeTraversalError,
    TreeValidationError, TreeWalkEntry, TreeWalker, ValidatedName, decode_tree,
    decode_tree_node_object, encode_tree, encode_tree_node_object, tree_node_id_for_content,
    tree_node_object_id,
};

pub use api::{
    Filesystem, FilesystemChangePhase, FilesystemChangeReason, FilesystemClock,
    FilesystemDirectory, FilesystemDirectoryEntry, FilesystemEntry, FilesystemEntryError,
    FilesystemEntryKind, FilesystemErrorKind, FilesystemFile, FilesystemIdentity,
    FilesystemMetadata, FilesystemOperation, FilesystemPermissionPolicy, FilesystemScan,
    FilesystemScanError, FilesystemScanOptions, FilesystemScanner, LocalFilesystem,
    LocalFilesystemScanner, MAX_FILESYSTEM_SCAN_OPEN_DIRECTORIES, PermissionErrorPolicy,
    SystemClock, VerifiedFileReader, local_filesystem_scanner,
};

#[cfg(feature = "async")]
pub use api::AsyncFilesystemScan;

pub use api::{
    CURRENT_PACK_FORMAT_VERSION, DEFAULT_PACK_MAX_SIZE_BYTES, DEFAULT_PACK_TARGET_SIZE_BYTES,
    MAX_PACK_SIZE_BYTES, PACK_ALIGNMENT, PACK_ENTRY_HEADER_LENGTH, PACK_FOOTER_LENGTH,
    PACK_HEADER_LENGTH, PackBuilder, PackConfiguration, PackConfigurationError, PackEntryError,
    PackEntryInput, PackEntryLocation, PackId, PackIdError, PackMetadata, PackPublisher,
    PackReader, SealedPack, verify_pack,
};

pub use api::{
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

#[cfg(feature = "async")]
pub use api::{AsyncChunkStream, AsyncChunker, async_chunk_reader};

#[cfg(feature = "s3")]
pub use api::{
    DEFAULT_S3_CAPABILITY_CACHE_FILE_NAME, DEFAULT_S3_CAPABILITY_CACHE_TTL_SECONDS,
    DEFAULT_S3_MAX_CONCURRENCY, DEFAULT_S3_MULTIPART_PART_SIZE, DEFAULT_S3_MULTIPART_THRESHOLD,
    MAX_S3_MULTIPART_PART_SIZE, MAX_S3_MULTIPART_THRESHOLD, MAX_S3_MULTIPART_UPLOAD_PARTS,
    MIN_S3_MULTIPART_PART_SIZE, S3ConditionalWriteCapabilities, S3ConditionalWriteStatus,
    S3Storage, S3StorageConfig,
};

#[cfg(feature = "webdav")]
pub use api::{
    DEFAULT_WEBDAV_MAX_CONCURRENCY, DEFAULT_WEBDAV_REQUEST_TIMEOUT,
    DEFAULT_WEBDAV_TRANSFER_BUFFER_SIZE, MAX_WEBDAV_MAX_CONCURRENCY, WebDavStorage,
    WebDavStorageConfig,
};

pub use api::{
    CURRENT_REPOSITORY_BOOTSTRAP_VERSION, CURRENT_REPOSITORY_DESCRIPTOR_VERSION,
    CURRENT_REPOSITORY_FORMAT_VERSION, CURRENT_REPOSITORY_HEAD_VERSION, FORMAT_OBJECT_KEY,
    HEAD_OBJECT_KEY, LATEST_REF_OBJECT_KEY, REPOSITORY_DESCRIPTOR_OBJECT_KEY, REPOSITORY_HEAD_KEY,
    REPOSITORY_HEAD_OBJECT_KEY, REPOSITORY_HEAD_VERSION, REPOSITORY_MAGIC,
    REQUIRED_REPOSITORY_FEATURE,
};

pub use api::{
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

pub use domain::DomainError;

/// Compatibility name for the repository storage abstraction.
pub use api::RepositoryStorage as Storage;
