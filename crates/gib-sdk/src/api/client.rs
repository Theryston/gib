use super::builder::DEFAULT_EVENT_BUFFER_CAPACITY;
use super::error::{SdkError, SdkResult};
use super::event::{EventConsumer, EventDispatcher, EventSubscription};
use super::operation::{OperationHandle, OperationKind, OperationRequest};
use super::repository::{
    Repository, RepositoryIdentity, RepositoryInitRequest, RepositoryKey, RepositoryOpenRequest,
    StorageHandle,
};
use super::storage_configuration::{StorageConfigurationStore, StorageManager};
use std::fmt;

/// Entry point for programmatic use of the Gib SDK.
///
/// A client is cheap to clone and owns an event dispatcher configured by its
/// builder. It does not initialize a repository or perform I/O until a future
/// use-case API receives an explicit request. Clients are independent: each
/// has its own event consumers and bounded queues.
#[derive(Clone)]
pub struct Client {
    events: EventDispatcher,
}

impl Client {
    /// Returns a builder for a validated client.
    pub const fn builder() -> super::builder::ClientBuilder {
        super::builder::ClientBuilder::new()
    }

    /// Returns a clone of this client's event dispatcher.
    ///
    /// The dispatcher is a public composition point for future use cases and
    /// remains independent of terminal, Tokio, and CLI types.
    pub fn events(&self) -> EventDispatcher {
        self.events.clone()
    }

    /// Creates a storage manager with the supplied persistence adapter.
    pub fn storage_manager(&self, store: StorageConfigurationStore) -> StorageManager {
        StorageManager::new(store)
    }

    /// Registers a callback on a dedicated bounded event-consumer worker.
    ///
    /// Events are delivered in queue order on the worker thread, never while a
    /// producer holds a client or operation lock. Progress events may be
    /// coalesced or dropped when the queue is full; lifecycle, warning,
    /// conflict, recovery, error, and terminal events are preserved. Dropping
    /// the returned subscription stops new delivery after already queued events
    /// drain. A callback panic closes only that subscription.
    pub fn register_event_consumer<C>(&self, consumer: C) -> SdkResult<EventSubscription>
    where
        C: EventConsumer,
    {
        self.events.register_consumer(consumer)
    }

    /// Starts a typed operation and emits its initial lifecycle event.
    ///
    /// The returned handle is the operation's sole public control surface. It
    /// provides its opaque identifier, cooperative cancellation, progress
    /// reporting, and terminal transitions for the future application layer.
    pub fn start_operation(&self, request: OperationRequest) -> SdkResult<OperationHandle> {
        request.validate()?;
        if self.events.is_closed() {
            return Err(SdkError::EventDispatcherClosed);
        }
        OperationHandle::start(self.events.clone(), request)
    }

    /// Starts a generic operation of the supplied kind.
    pub fn create_operation(&self, kind: OperationKind) -> SdkResult<OperationHandle> {
        self.start_operation(OperationRequest::new(kind))
    }

    /// Returns the configured queue capacity per event consumer.
    pub fn event_buffer_capacity(&self) -> usize {
        self.events.capacity()
    }

    /// Initializes a repository through an injected storage backend.
    pub fn initialize_repository<S>(
        &self,
        storage: S,
        request: RepositoryInitRequest,
    ) -> SdkResult<Repository>
    where
        S: Into<StorageHandle>,
    {
        Repository::initialize_with_request(storage, request)
    }

    /// Opens and validates a repository through an injected storage backend.
    pub fn open_repository<S>(
        &self,
        storage: S,
        request: RepositoryOpenRequest,
    ) -> SdkResult<Repository>
    where
        S: Into<StorageHandle>,
    {
        Repository::open_with_request(storage, request)
    }

    /// Convenience form of [`Self::initialize_repository`] for validated
    /// identity and key values.
    pub fn initialize_repository_with_identity<S>(
        &self,
        storage: S,
        identity: RepositoryIdentity,
        repository_key: RepositoryKey,
    ) -> SdkResult<Repository>
    where
        S: Into<StorageHandle>,
    {
        self.initialize_repository(
            storage,
            RepositoryInitRequest::new(identity, repository_key),
        )
    }

    /// Convenience form of [`Self::open_repository`] that verifies identity
    /// and namespace key.
    pub fn open_repository_with_identity<S>(
        &self,
        storage: S,
        identity: RepositoryIdentity,
        repository_key: RepositoryKey,
    ) -> SdkResult<Repository>
    where
        S: Into<StorageHandle>,
    {
        self.open_repository(
            storage,
            RepositoryOpenRequest::for_repository(identity, repository_key),
        )
    }

    /// Alias for [`Self::initialize_repository`] for callers using the shorter
    /// lifecycle method name.
    pub fn initialize<S>(&self, storage: S, request: RepositoryInitRequest) -> SdkResult<Repository>
    where
        S: Into<StorageHandle>,
    {
        self.initialize_repository(storage, request)
    }

    /// Alias for [`Self::open_repository`] for callers using the shorter
    /// lifecycle method name.
    pub fn open<S>(&self, storage: S, request: RepositoryOpenRequest) -> SdkResult<Repository>
    where
        S: Into<StorageHandle>,
    {
        self.open_repository(storage, request)
    }
}

impl Default for Client {
    fn default() -> Self {
        Self::from_dispatcher(EventDispatcher::from_valid_capacity(
            DEFAULT_EVENT_BUFFER_CAPACITY,
        ))
    }
}

impl fmt::Debug for Client {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("Client")
            .field("event_buffer_capacity", &self.event_buffer_capacity())
            .field("event_consumer_count", &self.events.consumer_count())
            .finish()
    }
}

impl Client {
    pub(crate) fn from_dispatcher(events: EventDispatcher) -> Self {
        Self { events }
    }
}
