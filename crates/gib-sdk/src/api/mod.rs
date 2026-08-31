mod builder;
mod client;
mod configuration;
mod error;
mod event;
mod identity;
mod operation;
mod repository;

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
    BackupReference, ByteRange, CURRENT_REPOSITORY_BOOTSTRAP_VERSION,
    CURRENT_REPOSITORY_DESCRIPTOR_VERSION, CURRENT_REPOSITORY_FORMAT_VERSION,
    CURRENT_REPOSITORY_HEAD_VERSION, CURRENT_SNAPSHOT_HISTORY_VERSION,
    CURRENT_SNAPSHOT_SUMMARY_VERSION, CURRENT_SNAPSHOT_VERSION, DEFAULT_OBJECT_LIST_PAGE_SIZE,
    DEFAULT_SNAPSHOT_PAGE_SIZE, FORMAT_OBJECT_KEY, HEAD_OBJECT_KEY, Head, HeadPublication,
    HeadRead, HeadState, InitializeRepositoryRequest, LATEST_REF_OBJECT_KEY, LATEST_SNAPSHOT_ALIAS,
    ListCursor, LocalStorage, LocalStorageOperation, MAX_OBJECT_LIST_PAGE_SIZE,
    MAX_SNAPSHOT_AUTHOR_LENGTH, MAX_SNAPSHOT_CURSOR_LENGTH, MAX_SNAPSHOT_ID_LENGTH,
    MAX_SNAPSHOT_MESSAGE_LENGTH, MAX_SNAPSHOT_PAGE_SIZE, MemoryStorage, MemoryStorageOperation,
    ObjectCursor, ObjectKey, ObjectListPage, ObjectListRequest, ObjectMetadata, ObjectPrefix,
    ObjectRange, ObjectRead, ObjectReader, ObjectStorage, ObjectWriteOptions,
    OpenRepositoryRequest, REPOSITORY_BOOTSTRAP_VERSION, REPOSITORY_DESCRIPTOR_OBJECT_KEY,
    REPOSITORY_DESCRIPTOR_VERSION, REPOSITORY_FORMAT_VERSION, REPOSITORY_HEAD_KEY,
    REPOSITORY_HEAD_OBJECT_KEY, REPOSITORY_HEAD_VERSION, REPOSITORY_MAGIC,
    REQUIRED_REPOSITORY_FEATURE, Repository, RepositoryDescriptor, RepositoryFeature,
    RepositoryHead, RepositoryHeadRead, RepositoryHeadState, RepositoryId, RepositoryIdentity,
    RepositoryInitRequest, RepositoryInitializationRequest, RepositoryKey, RepositoryObject,
    RepositoryOpenRequest, RepositoryRoots, RepositoryStorage, SNAPSHOT_HISTORY_OBJECT_PREFIX,
    SNAPSHOT_OBJECT_PREFIX, STORAGE_TRANSFER_BUFFER_SIZE, Snapshot, SnapshotCursor,
    SnapshotHistoryPage, SnapshotHistoryRequest, SnapshotId, SnapshotListRequest, SnapshotPage,
    SnapshotPublication, SnapshotPublicationRequest, SnapshotRef, SnapshotReference,
    SnapshotReferenceInput, SnapshotReferenceSelector, SnapshotSelector, SnapshotSummary,
    SnapshotSummaryListRequest, SnapshotSummaryPage, StorageCapabilities, StorageCapability,
    StorageError, StorageHandle, StorageKey, StorageListPage, StorageListRequest, StorageMetadata,
    StoragePrefix, StorageRange, StorageReader, StorageResult, StorageVersion, StorageVersionToken,
    StorageWriteCondition, StorageWriteOptions, VersionToken, VersionedHead, VersionedObject,
    VersionedStorageObject, WriteCondition, decode_snapshot_object, encode_snapshot_object,
    initialize_repository, open_repository,
};
