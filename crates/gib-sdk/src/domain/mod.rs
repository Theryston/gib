mod author;
mod chunk;
mod configuration;
mod filesystem;
mod object;
mod pack;
mod pack_index;
mod policy;
mod repository;
mod snapshot;
mod tree;

pub use author::{AuthorIdentity, MAX_AUTHOR_IDENTITY_LENGTH};
#[cfg(feature = "async")]
pub use chunk::{AsyncChunkStream, AsyncChunker, async_chunk_reader};
pub use chunk::{
    BUZHASH_TABLE_SEED, BUZHASH_WINDOW_SIZE, CHUNK_BUFFER_POOL_CAPACITY, CHUNKING_READ_BUFFER_SIZE,
    CONTENT_DEFINED_CHUNKING_ALGORITHM, CURRENT_CHUNKING_VERSION, Chunk, ChunkBoundary, ChunkId,
    ChunkIdError, ChunkStream, Chunker, ChunkingConfiguration, ChunkingConfigurationError,
    ChunkingError, ChunkingResult, DEFAULT_MAX_CHUNK_SIZE_BYTES, DEFAULT_MIN_CHUNK_SIZE_BYTES,
    DEFAULT_TARGET_CHUNK_SIZE_BYTES, MAX_CONTENT_DEFINED_CHUNK_SIZE_BYTES, chunk_reader,
    chunk_reader_with_cancellation,
};
pub(crate) use configuration::{
    BackupConfigurationInput, CURRENT_CONFIGURATION_VERSION, ChunkingConfigurationInput,
    ConfigurationInput, ConfigurationValidationError, LiveConfigurationInput,
    MAX_BACKUP_CONCURRENCY, MAX_CHUNK_SIZE_BYTES, MAX_COMPRESSION_LEVEL, MAX_LIVE_INTERVAL_MS,
    MIN_COMPRESSION_LEVEL, RepositoryConfigurationInput, RestoreConfigurationInput,
    ValidatedConfiguration, validate_configuration,
};
pub(crate) use object::ImmutableObjectParts;
pub use object::{
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
pub(crate) use pack::PackMetadataParts;
pub use pack::{
    CURRENT_PACK_FORMAT_VERSION, DEFAULT_PACK_MAX_SIZE_BYTES, DEFAULT_PACK_TARGET_SIZE_BYTES,
    MAX_PACK_SIZE_BYTES, PACK_ALIGNMENT, PACK_ENTRY_HEADER_LENGTH, PACK_FOOTER_LENGTH,
    PACK_HEADER_LENGTH, PackConfiguration, PackConfigurationError, PackEntryError, PackEntryInput,
    PackEntryLocation, PackId, PackIdError, PackMetadata, SealedPack,
};
pub(crate) use pack_index::entry_belongs_to_shard;
pub use pack_index::{
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
pub use policy::{
    DEFAULT_IGNORE_GIT, IgnoreDecision, IgnoreMatch, IgnorePathError, IgnorePattern,
    IgnorePatternError, IgnorePolicy, IgnoreReason, IgnoreRule, IgnoreRuleError,
    MAX_IGNORE_RULE_LENGTH, MAX_IGNORE_RULES, is_git_path,
};
pub use repository::{
    CURRENT_REPOSITORY_BOOTSTRAP_VERSION, CURRENT_REPOSITORY_DESCRIPTOR_VERSION,
    CURRENT_REPOSITORY_FORMAT_VERSION, CURRENT_REPOSITORY_HEAD_VERSION, DomainError,
    FORMAT_OBJECT_KEY, HEAD_OBJECT_KEY, Head, HeadPublication, LATEST_REF_OBJECT_KEY,
    REPOSITORY_DESCRIPTOR_OBJECT_KEY, REPOSITORY_HEAD_KEY, REPOSITORY_HEAD_OBJECT_KEY,
    REPOSITORY_HEAD_VERSION, REPOSITORY_MAGIC, REQUIRED_REPOSITORY_FEATURE, RepositoryDescriptor,
    RepositoryFeature, RepositoryHead, RepositoryId, RepositoryIdentity, RepositoryKey,
    RepositoryObject, RepositoryRoots, SnapshotPublication, SnapshotPublicationRequest,
    SnapshotReference,
};

pub use filesystem::{
    FilesystemChangePhase, FilesystemChangeReason, FilesystemEntry, FilesystemEntryError,
    FilesystemEntryKind, FilesystemErrorKind, FilesystemIdentity, FilesystemMetadata,
    FilesystemOperation, FilesystemPermissionPolicy, FilesystemScanError,
    MAX_FILESYSTEM_SCAN_OPEN_DIRECTORIES, PermissionErrorPolicy,
};
pub use snapshot::{
    BackupReference, CURRENT_SNAPSHOT_HISTORY_VERSION, CURRENT_SNAPSHOT_SUMMARY_VERSION,
    CURRENT_SNAPSHOT_VERSION, DEFAULT_SNAPSHOT_PAGE_SIZE, LATEST_SNAPSHOT_ALIAS,
    MAX_SNAPSHOT_AUTHOR_LENGTH, MAX_SNAPSHOT_CURSOR_LENGTH, MAX_SNAPSHOT_ID_LENGTH,
    MAX_SNAPSHOT_MESSAGE_LENGTH, MAX_SNAPSHOT_PAGE_SIZE, SNAPSHOT_HISTORY_OBJECT_PREFIX,
    SNAPSHOT_OBJECT_PREFIX, Snapshot, SnapshotCursor, SnapshotHistoryPage, SnapshotHistoryRequest,
    SnapshotId, SnapshotListRequest, SnapshotPage, SnapshotRef, SnapshotReferenceInput,
    SnapshotReferenceSelector, SnapshotSelector, SnapshotSummary, SnapshotSummaryListRequest,
    SnapshotSummaryPage,
};
pub use tree::{
    CURRENT_TREE_METADATA_VERSION, CURRENT_TREE_NODE_VERSION, ChunkReference, ChunkReferenceError,
    DirectoryEntry, DirectoryNode, EntryName, EntryNameError, FileChunkReference, FileNode,
    FilePermissions, LazyTree, MAX_FILE_CHUNK_REFERENCES, MAX_METADATA_EXTENSION_BYTES,
    MAX_METADATA_EXTENSIONS, MAX_METADATA_NAMESPACE_BYTES, MAX_SYMLINK_TARGET_BYTES,
    MAX_TREE_ENTRIES, MAX_TREE_NAME_BYTES, MAX_TREE_PATH_BYTES, MetadataError, MetadataExtension,
    MetadataNamespace, MetadataNamespaceError, Name, NodeKind, NodeReference,
    NormalizedRelativePath, PermissionError, PortableMetadata, RegularFileNode, RelativePath,
    RelativePathError, SymbolicLinkNode, SymlinkNode, SymlinkTarget, SymlinkTargetError, TreeEntry,
    TreeNode, TreeNodeId, TreeNodeKind, TreeNodeReference, TreeNodeStore, TreeTraversalError,
    TreeValidationError, TreeWalkEntry, TreeWalker, ValidatedName,
};
