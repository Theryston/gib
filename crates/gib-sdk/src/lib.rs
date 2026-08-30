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
    CancellationHandle, CancellationInfo, CancellationToken, Client, ClientBuilder,
    DEFAULT_EVENT_BUFFER_CAPACITY, EVENT_SCHEMA_VERSION, ErrorCode, ErrorSummary, EventConsumer,
    EventDelivery, EventDispatcher, EventEnvelope, EventKind, EventMessage, EventPayload,
    EventPhase, EventSubscription, Head, HeadPublication, HeadRead, HeadState,
    InitializeRepositoryRequest, LocalStorage, MemoryStorage, OpenRepositoryRequest,
    OperationHandle, OperationId, OperationKind, OperationRequest, OperationResult,
    OperationStatus, Progress, REPOSITORY_BOOTSTRAP_VERSION, REPOSITORY_DESCRIPTOR_VERSION,
    REPOSITORY_FORMAT_VERSION, RecoveryPoint, Repository, RepositoryDescriptor, RepositoryFeature,
    RepositoryHead, RepositoryHeadRead, RepositoryHeadState, RepositoryId, RepositoryIdentity,
    RepositoryInitRequest, RepositoryInitializationRequest, RepositoryKey, RepositoryObject,
    RepositoryOpenRequest, RepositoryRoots, RepositoryStorage, Request, Result, SdkError,
    SdkResult, SnapshotPublication, SnapshotPublicationRequest, SnapshotReference, StorageError,
    StorageHandle, StorageResult, StorageVersion, StorageVersionToken, VersionToken, VersionedHead,
    VersionedObject, VersionedStorageObject, initialize_repository, open_repository,
};

pub use api::{
    CURRENT_REPOSITORY_BOOTSTRAP_VERSION, CURRENT_REPOSITORY_DESCRIPTOR_VERSION,
    CURRENT_REPOSITORY_FORMAT_VERSION, CURRENT_REPOSITORY_HEAD_VERSION, FORMAT_OBJECT_KEY,
    HEAD_OBJECT_KEY, LATEST_REF_OBJECT_KEY, REPOSITORY_DESCRIPTOR_OBJECT_KEY, REPOSITORY_HEAD_KEY,
    REPOSITORY_HEAD_OBJECT_KEY, REPOSITORY_HEAD_VERSION, REPOSITORY_MAGIC,
    REQUIRED_REPOSITORY_FEATURE,
};

pub use domain::DomainError;

/// Compatibility name for the repository storage abstraction.
pub use api::RepositoryStorage as Storage;
