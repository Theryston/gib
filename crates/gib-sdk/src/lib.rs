//! Public SDK contracts for Gib.
//!
//! The crate root intentionally exposes only stable API types. Backup,
//! restore, repository, storage, and persistence implementations will be added
//! behind these contracts as the SDK grows.
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

pub use api::{
    CancellationHandle, CancellationInfo, CancellationToken, Client, ClientBuilder,
    DEFAULT_EVENT_BUFFER_CAPACITY, EVENT_SCHEMA_VERSION, ErrorCode, ErrorSummary, EventConsumer,
    EventDelivery, EventDispatcher, EventEnvelope, EventKind, EventMessage, EventPayload,
    EventPhase, EventSubscription, OperationHandle, OperationId, OperationKind, OperationRequest,
    OperationResult, OperationStatus, Progress, RecoveryPoint, Request, Result, SdkError,
    SdkResult,
};
