//! Public SDK contracts for Gib.
//!
//! The crate root intentionally exposes only stable API types. Repository
//! lifecycle operations use validated domain values and injectable storage
//! backends. Repository metadata is persisted as versioned MessagePack bytes;
//! the SDK does not write JSON. Snapshot and backup operations will build on
//! these contracts.
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
    EventPhase, EventSubscription, InitializeRepositoryRequest, LocalStorage, MemoryStorage,
    OpenRepositoryRequest, OperationHandle, OperationId, OperationKind, OperationRequest,
    OperationResult, OperationStatus, Progress, REPOSITORY_BOOTSTRAP_VERSION,
    REPOSITORY_DESCRIPTOR_VERSION, REPOSITORY_FORMAT_VERSION, RecoveryPoint, Repository,
    RepositoryDescriptor, RepositoryFeature, RepositoryId, RepositoryIdentity,
    RepositoryInitRequest, RepositoryInitializationRequest, RepositoryKey, RepositoryObject,
    RepositoryOpenRequest, RepositoryRoots, RepositoryStorage, Request, Result, SdkError,
    SdkResult, StorageError, StorageHandle, StorageResult, initialize_repository, open_repository,
};

pub use api::{
    CURRENT_REPOSITORY_BOOTSTRAP_VERSION, CURRENT_REPOSITORY_DESCRIPTOR_VERSION,
    CURRENT_REPOSITORY_FORMAT_VERSION, FORMAT_OBJECT_KEY, REPOSITORY_DESCRIPTOR_OBJECT_KEY,
    REPOSITORY_MAGIC, REQUIRED_REPOSITORY_FEATURE,
};

pub use domain::DomainError;

/// Compatibility name for the repository storage abstraction.
pub use api::RepositoryStorage as Storage;
