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
    Author, AuthorIdentity, BackupReference, CURRENT_IDENTITY_CONFIGURATION_VERSION,
    CURRENT_SNAPSHOT_HISTORY_VERSION, CURRENT_SNAPSHOT_SUMMARY_VERSION, CURRENT_SNAPSHOT_VERSION,
    CancellationHandle, CancellationInfo, CancellationToken, Client, ClientBuilder,
    ConfigurationError, ConfigurationHandle, ConfigurationResult, ConfigurationStorage,
    DEFAULT_EVENT_BUFFER_CAPACITY, DEFAULT_SNAPSHOT_PAGE_SIZE, EVENT_SCHEMA_VERSION, ErrorCode,
    ErrorSummary, EventConsumer, EventDelivery, EventDispatcher, EventEnvelope, EventKind,
    EventMessage, EventPayload, EventPhase, EventSubscription, GLOBAL_CONFIG_DIRECTORY,
    GLOBAL_CONFIG_FILE_NAME, GLOBAL_CONFIGURATION_DIRECTORY, GlobalConfiguration, Head,
    HeadPublication, HeadRead, HeadState, IDENTITY_CONFIGURATION_FILE_NAME,
    IdentityConfigurationStorage, IdentityStorageResult, InitializeRepositoryRequest,
    LATEST_SNAPSHOT_ALIAS, LocalConfiguration, LocalIdentityConfiguration, LocalStorage,
    MAX_AUTHOR_IDENTITY_LENGTH, MAX_AUTHOR_LENGTH, MAX_SNAPSHOT_AUTHOR_LENGTH,
    MAX_SNAPSHOT_CURSOR_LENGTH, MAX_SNAPSHOT_ID_LENGTH, MAX_SNAPSHOT_MESSAGE_LENGTH,
    MAX_SNAPSHOT_PAGE_SIZE, MemoryConfiguration, MemoryIdentityConfiguration, MemoryStorage,
    OpenRepositoryRequest, OperationHandle, OperationId, OperationKind, OperationRequest,
    OperationResult, OperationStatus, Progress, REPOSITORY_BOOTSTRAP_VERSION,
    REPOSITORY_DESCRIPTOR_VERSION, REPOSITORY_FORMAT_VERSION, RecoveryPoint, Repository,
    RepositoryDescriptor, RepositoryFeature, RepositoryHead, RepositoryHeadRead,
    RepositoryHeadState, RepositoryId, RepositoryIdentity, RepositoryInitRequest,
    RepositoryInitializationRequest, RepositoryKey, RepositoryObject, RepositoryOpenRequest,
    RepositoryRoots, RepositoryStorage, Request, Result, SNAPSHOT_HISTORY_OBJECT_PREFIX,
    SNAPSHOT_OBJECT_PREFIX, SdkError, SdkResult, SetAuthorRequest, SetIdentityRequest, Snapshot,
    SnapshotCursor, SnapshotHistoryPage, SnapshotHistoryRequest, SnapshotId, SnapshotListRequest,
    SnapshotPage, SnapshotPublication, SnapshotPublicationRequest, SnapshotRef, SnapshotReference,
    SnapshotReferenceInput, SnapshotReferenceSelector, SnapshotSelector, SnapshotSummary,
    SnapshotSummaryListRequest, SnapshotSummaryPage, StorageError, StorageHandle, StorageResult,
    StorageVersion, StorageVersionToken, UserIdentity, VersionToken, VersionedHead,
    VersionedObject, VersionedStorageObject, decode_snapshot_object, encode_snapshot_object,
    get_author, get_global_identity, get_identity, initialize_repository, open_repository,
    read_identity, set_author, set_global_identity, set_identity,
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
    Configuration, ConfigurationFileError, ConfigurationParseError, GIB_CONFIGURATION_FILE_NAME,
    GibConfig, GibConfiguration, GibConfigurationError, GibConfigurationErrorKind,
    LOCAL_CONFIGURATION_FILE_NAME, LiveConfig, LiveConfiguration, LocalConfig,
    MAX_BACKUP_CONCURRENCY, MAX_CHUNK_SIZE_BYTES, MAX_COMPRESSION_LEVEL, MAX_CONFIGURATION_BYTES,
    MAX_LIVE_INTERVAL_MS, MIN_COMPRESSION_LEVEL, PROJECT_CONFIGURATION_FILE_NAME, ProjectConfig,
    ProjectConfigError, ProjectConfiguration, ProjectConfigurationError,
    ProjectConfigurationErrorKind, ProjectRepositoryConfiguration, RepositoryConfig,
    RepositoryConfiguration, RestoreConfig, RestoreConfiguration, load_configuration,
    parse_configuration,
};

pub use domain::DomainError;

/// Compatibility name for the repository storage abstraction.
pub use api::RepositoryStorage as Storage;
