mod builder;
mod client;
mod error;
mod event;
mod operation;
mod repository;

pub use builder::{ClientBuilder, DEFAULT_EVENT_BUFFER_CAPACITY};
pub use client::Client;
pub use error::{ErrorCode, ErrorSummary, Result, SdkError, SdkResult};
pub use event::{
    CancellationInfo, EVENT_SCHEMA_VERSION, EventConsumer, EventDelivery, EventDispatcher,
    EventEnvelope, EventKind, EventMessage, EventPayload, EventPhase, EventSubscription, Progress,
    RecoveryPoint,
};
pub use operation::{
    CancellationHandle, CancellationToken, OperationHandle, OperationId, OperationKind,
    OperationRequest, OperationResult, OperationStatus, Request,
};
pub use repository::{
    CURRENT_REPOSITORY_BOOTSTRAP_VERSION, CURRENT_REPOSITORY_DESCRIPTOR_VERSION,
    CURRENT_REPOSITORY_FORMAT_VERSION, FORMAT_OBJECT_KEY, InitializeRepositoryRequest,
    LocalStorage, MemoryStorage, OpenRepositoryRequest, REPOSITORY_BOOTSTRAP_VERSION,
    REPOSITORY_DESCRIPTOR_OBJECT_KEY, REPOSITORY_DESCRIPTOR_VERSION, REPOSITORY_FORMAT_VERSION,
    REPOSITORY_MAGIC, REQUIRED_REPOSITORY_FEATURE, Repository, RepositoryDescriptor,
    RepositoryFeature, RepositoryId, RepositoryIdentity, RepositoryInitRequest,
    RepositoryInitializationRequest, RepositoryKey, RepositoryObject, RepositoryOpenRequest,
    RepositoryRoots, RepositoryStorage, StorageError, StorageHandle, StorageResult,
    initialize_repository, open_repository,
};
