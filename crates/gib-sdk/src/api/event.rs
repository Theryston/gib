use super::error::{ErrorSummary, SdkError, SdkResult};
use super::operation::OperationId;
use std::collections::VecDeque;
use std::fmt;
use std::panic::{AssertUnwindSafe, catch_unwind};
use std::sync::{Arc, Condvar, Mutex, MutexGuard, Weak};
use std::thread;

/// Version of the public event envelope schema.
pub const EVENT_SCHEMA_VERSION: u16 = 1;

const MAX_EVENT_CODE_BYTES: usize = 64;
const MAX_EVENT_MESSAGE_BYTES: usize = 4 * 1024;

/// Typed kinds emitted by long-running SDK operations.
#[non_exhaustive]
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub enum EventKind {
    /// The operation has started.
    Started,
    /// Bounded work progress, eligible for coalescing.
    Progress,
    /// A non-fatal notice for the consumer.
    Warning,
    /// A conflict that needs caller attention.
    Conflict,
    /// A recovery or resumability update.
    Recovery,
    /// The operation completed successfully.
    Completed,
    /// The operation failed.
    Failed,
    /// The operation was cooperatively cancelled.
    Cancelled,
}

impl EventKind {
    /// Returns whether this event must not be dropped by a bounded dispatcher.
    pub const fn is_critical(self) -> bool {
        !matches!(self, Self::Progress)
    }

    /// Returns whether this event may be replaced by a newer progress event.
    pub const fn is_coalescible(self) -> bool {
        matches!(self, Self::Progress)
    }
}

impl fmt::Display for EventKind {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        let value = match self {
            Self::Started => "started",
            Self::Progress => "progress",
            Self::Warning => "warning",
            Self::Conflict => "conflict",
            Self::Recovery => "recovery",
            Self::Completed => "completed",
            Self::Failed => "failed",
            Self::Cancelled => "cancelled",
        };
        formatter.write_str(value)
    }
}

/// Typed phases shared by lifecycle and progress events.
#[non_exhaustive]
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub enum EventPhase {
    /// The operation is preparing its resources.
    Starting,
    /// The operation is doing bounded work.
    Running,
    /// The operation is publishing or cleaning up.
    Finalizing,
    /// The operation reached successful completion.
    Completed,
    /// The operation reached a failed terminal state.
    Failed,
    /// The operation reached a cancelled terminal state.
    Cancelled,
}

impl fmt::Display for EventPhase {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        let value = match self {
            Self::Starting => "starting",
            Self::Running => "running",
            Self::Finalizing => "finalizing",
            Self::Completed => "completed",
            Self::Failed => "failed",
            Self::Cancelled => "cancelled",
        };
        formatter.write_str(value)
    }
}

/// Validated progress data carried by an [`EventKind::Progress`] event.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct Progress {
    completed_units: u64,
    total_units: Option<u64>,
}

impl Progress {
    /// Creates progress with an optional total.
    ///
    /// When a total is supplied, completed units must not exceed it. Use
    /// [`Self::indeterminate`] when no total is known.
    pub fn new(completed_units: u64, total_units: Option<u64>) -> SdkResult<Self> {
        if total_units.is_some_and(|total| completed_units > total) {
            return Err(SdkError::InvalidRequest {
                field: "completed_units",
                reason: "must not exceed total_units",
            });
        }
        Ok(Self {
            completed_units,
            total_units,
        })
    }

    /// Creates progress without a known total.
    pub const fn indeterminate(completed_units: u64) -> Self {
        Self {
            completed_units,
            total_units: None,
        }
    }

    /// Returns the amount of completed work.
    pub const fn completed_units(self) -> u64 {
        self.completed_units
    }

    /// Returns the total work, when known.
    pub const fn total_units(self) -> Option<u64> {
        self.total_units
    }

    /// Returns a ratio in the inclusive range `0.0..=1.0`, when a total exists.
    pub fn fraction(self) -> Option<f64> {
        self.total_units.map(|total| {
            if total == 0 {
                1.0
            } else {
                self.completed_units as f64 / total as f64
            }
        })
    }
}

/// Safe cancellation metadata carried by a cancelled-operation event.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct CancellationInfo {
    resumable: bool,
    recovery_point: RecoveryPoint,
}

impl CancellationInfo {
    /// Creates cancellation metadata without a path or backend-specific state.
    pub const fn new(resumable: bool, recovery_point: RecoveryPoint) -> Self {
        Self {
            resumable,
            recovery_point,
        }
    }

    /// Returns whether the operation can be resumed from the reported point.
    pub const fn is_resumable(self) -> bool {
        self.resumable
    }

    /// Returns the safe recovery point known to the operation.
    pub const fn recovery_point(self) -> RecoveryPoint {
        self.recovery_point
    }
}

/// Redacted recovery state associated with cancellation.
#[non_exhaustive]
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub enum RecoveryPoint {
    /// Work stopped at a boundary safe for a later retry.
    OperationBoundary,
    /// A durable checkpoint can be used by a later retry.
    Checkpoint,
    /// No safe resumable point is known.
    Unknown,
}

/// A bounded, validated message for warning, conflict, or recovery events.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct EventMessage {
    code: String,
    message: String,
}

impl EventMessage {
    /// Creates a message with a short stable code and bounded human detail.
    pub fn new(code: impl Into<String>, message: impl Into<String>) -> SdkResult<Self> {
        let code = code.into();
        let message = message.into();
        if code.is_empty() || code.len() > MAX_EVENT_CODE_BYTES {
            return Err(SdkError::InvalidRequest {
                field: "event_message.code",
                reason: "must contain between 1 and 64 UTF-8 bytes",
            });
        }
        if message.len() > MAX_EVENT_MESSAGE_BYTES {
            return Err(SdkError::InvalidRequest {
                field: "event_message.message",
                reason: "must contain at most 4096 UTF-8 bytes",
            });
        }
        Ok(Self { code, message })
    }

    /// Returns the stable event-specific code.
    pub fn code(&self) -> &str {
        &self.code
    }

    /// Returns the bounded human-readable detail.
    pub fn message(&self) -> &str {
        &self.message
    }
}

/// Structured data carried by an event envelope.
#[non_exhaustive]
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum EventPayload {
    /// No additional data is needed for the event.
    Empty,
    /// Bounded unit progress.
    Progress(Progress),
    /// A warning, conflict, or recovery notice.
    Message(EventMessage),
    /// A redacted operation failure summary.
    Error(ErrorSummary),
    /// Safe resumability metadata for a cancelled operation.
    Cancellation(CancellationInfo),
}

/// Versioned event envelope delivered to public consumers.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct EventEnvelope {
    schema_version: u16,
    operation_id: OperationId,
    sequence: u64,
    kind: EventKind,
    phase: EventPhase,
    payload: EventPayload,
}

impl EventEnvelope {
    /// Creates a validated event envelope.
    ///
    /// Sequence numbers must be non-zero and are expected to increase
    /// monotonically for each operation. [`crate::OperationHandle`] assigns them
    /// automatically; direct publishers are responsible for maintaining that
    /// per-operation ordering rule.
    pub fn new(
        operation_id: OperationId,
        sequence: u64,
        kind: EventKind,
        phase: EventPhase,
        payload: EventPayload,
    ) -> SdkResult<Self> {
        if sequence == 0 {
            return Err(SdkError::InvalidRequest {
                field: "event.sequence",
                reason: "must be greater than zero",
            });
        }
        let valid_payload = match kind {
            EventKind::Started | EventKind::Completed => {
                matches!(payload, EventPayload::Empty)
            }
            EventKind::Cancelled => matches!(payload, EventPayload::Cancellation(_)),
            EventKind::Progress => matches!(payload, EventPayload::Progress(_)),
            EventKind::Warning | EventKind::Conflict | EventKind::Recovery => {
                matches!(payload, EventPayload::Message(_))
            }
            EventKind::Failed => matches!(payload, EventPayload::Error(_)),
        };
        if !valid_payload {
            return Err(SdkError::InvalidRequest {
                field: "event.payload",
                reason: "does not match event kind",
            });
        }
        let valid_phase = match kind {
            EventKind::Started => phase == EventPhase::Starting,
            EventKind::Progress => phase == EventPhase::Running,
            EventKind::Completed => phase == EventPhase::Completed,
            EventKind::Failed => phase == EventPhase::Failed,
            EventKind::Cancelled => phase == EventPhase::Cancelled,
            EventKind::Warning | EventKind::Conflict | EventKind::Recovery => matches!(
                phase,
                EventPhase::Starting | EventPhase::Running | EventPhase::Finalizing
            ),
        };
        if !valid_phase {
            return Err(SdkError::InvalidRequest {
                field: "event.phase",
                reason: "does not match event kind",
            });
        }
        Ok(Self {
            schema_version: EVENT_SCHEMA_VERSION,
            operation_id,
            sequence,
            kind,
            phase,
            payload,
        })
    }

    /// Returns the event schema version.
    pub const fn schema_version(&self) -> u16 {
        self.schema_version
    }

    /// Returns the operation identifier.
    pub const fn operation_id(&self) -> OperationId {
        self.operation_id
    }

    /// Returns the monotonic sequence number assigned by the operation.
    pub const fn sequence(&self) -> u64 {
        self.sequence
    }

    /// Returns the typed event kind.
    pub const fn kind(&self) -> EventKind {
        self.kind
    }

    /// Returns the typed operation phase.
    pub const fn phase(&self) -> EventPhase {
        self.phase
    }

    /// Returns the structured event payload.
    pub const fn payload(&self) -> &EventPayload {
        &self.payload
    }

    /// Returns whether the event must survive a full bounded queue.
    pub const fn is_critical(&self) -> bool {
        self.kind.is_critical()
    }
}

/// Consumer interface for structured SDK events.
///
/// Implementations run on a dedicated worker thread created by
/// [`EventDispatcher::register_consumer`]. They should keep their own work
/// bounded; slow consumers apply backpressure only when a critical event must
/// be retained. A panic is isolated and closes that subscription.
pub trait EventConsumer: Send + 'static {
    /// Handles one event outside all SDK internal locks.
    fn on_event(&mut self, event: EventEnvelope);
}

impl<F> EventConsumer for F
where
    F: FnMut(EventEnvelope) + Send + 'static,
{
    fn on_event(&mut self, event: EventEnvelope) {
        self(event);
    }
}

/// Outcome of publishing one event to the currently registered consumers.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct EventDelivery {
    consumer_count: usize,
    delivered_count: usize,
    coalesced_count: usize,
    dropped_progress_count: usize,
    evicted_progress_count: usize,
    dispatcher_closed: bool,
}

impl EventDelivery {
    /// Returns the number of consumers observed at publication time.
    pub const fn consumer_count(self) -> usize {
        self.consumer_count
    }

    /// Returns the number of consumers that accepted this event into a queue.
    pub const fn delivered_count(self) -> usize {
        self.delivered_count
    }

    /// Returns the number of consumers for which a queued progress event was
    /// replaced by this newer progress event.
    pub const fn coalesced_count(self) -> usize {
        self.coalesced_count
    }

    /// Returns the number of consumers that dropped this progress event because
    /// their full queues contained only critical events.
    pub const fn dropped_progress_count(self) -> usize {
        self.dropped_progress_count
    }

    /// Returns the number of consumers whose queued progress made room for this
    /// critical event.
    pub const fn evicted_progress_count(self) -> usize {
        self.evicted_progress_count
    }

    /// Returns whether the dispatcher had already been closed.
    pub const fn dispatcher_closed(self) -> bool {
        self.dispatcher_closed
    }

    /// Returns whether no consumer lost this event at publication time.
    pub const fn delivered_to_all(self) -> bool {
        self.dropped_progress_count == 0 && !self.dispatcher_closed
    }
}

/// Bounded event dispatcher independent of any runtime or presentation layer.
///
/// Each consumer owns a queue with the configured capacity. A producer never
/// calls consumer code directly. Progress events are coalesced or dropped when
/// possible; critical events evict queued progress and otherwise wait for room.
/// This preserves lifecycle and terminal events while keeping every queue
/// bounded. A producer can therefore block while a slow consumer makes room
/// for a critical event; consumers must not synchronously publish a critical
/// event to the same dispatcher from inside their own callback.
#[derive(Clone)]
pub struct EventDispatcher {
    inner: Arc<DispatcherInner>,
}

struct DispatcherInner {
    capacity: usize,
    state: Mutex<DispatcherState>,
}

struct DispatcherState {
    closed: bool,
    next_consumer_id: u64,
    consumers: Vec<ConsumerRegistration>,
}

struct ConsumerRegistration {
    id: u64,
    queue: Arc<ConsumerQueue>,
}

struct ConsumerQueue {
    capacity: usize,
    state: Mutex<QueueState>,
    changed: Condvar,
}

struct QueueState {
    closed: bool,
    events: VecDeque<EventEnvelope>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum QueueOutcome {
    Delivered,
    Coalesced,
    DroppedProgress,
    EvictedProgress,
    Closed,
}

impl EventDispatcher {
    /// Creates a dispatcher with a bounded queue capacity per consumer.
    pub fn new(capacity: usize) -> SdkResult<Self> {
        if capacity == 0 {
            return Err(SdkError::InvalidConfiguration {
                field: "event_buffer_capacity",
                reason: "must be greater than zero",
            });
        }
        Ok(Self::from_valid_capacity(capacity))
    }

    /// Returns the maximum number of queued events per consumer.
    pub fn capacity(&self) -> usize {
        self.inner.capacity
    }

    /// Registers a consumer and starts its isolated callback worker.
    pub fn register_consumer<C>(&self, consumer: C) -> SdkResult<EventSubscription>
    where
        C: EventConsumer,
    {
        let queue = Arc::new(ConsumerQueue::new(self.capacity()));
        let id = {
            let mut state = lock_or_recover(&self.inner.state);
            if state.closed {
                return Err(SdkError::EventDispatcherClosed);
            }
            let id = state.next_consumer_id;
            let Some(next_id) = id.checked_add(1) else {
                return Err(SdkError::EventConsumerRegistration);
            };
            state.next_consumer_id = next_id;
            state.consumers.push(ConsumerRegistration {
                id,
                queue: queue.clone(),
            });
            id
        };

        let dispatcher = Arc::downgrade(&self.inner);
        let worker_queue = queue.clone();
        let thread_result = thread::Builder::new()
            .name(format!("gib-event-consumer-{id}"))
            .spawn(move || run_consumer(worker_queue, dispatcher, id, consumer));
        if thread_result.is_err() {
            self.remove_consumer(id);
            queue.close();
            return Err(SdkError::EventConsumerRegistration);
        }

        Ok(EventSubscription {
            id,
            dispatcher: Arc::downgrade(&self.inner),
            queue,
        })
    }

    /// Alias for [`Self::register_consumer`].
    pub fn subscribe<C>(&self, consumer: C) -> SdkResult<EventSubscription>
    where
        C: EventConsumer,
    {
        self.register_consumer(consumer)
    }

    /// Publishes an event to all consumers without invoking user code inline.
    pub fn publish(&self, event: EventEnvelope) -> EventDelivery {
        let (queues, closed) = {
            let state = lock_or_recover(&self.inner.state);
            (
                state
                    .consumers
                    .iter()
                    .map(|consumer| consumer.queue.clone())
                    .collect::<Vec<_>>(),
                state.closed,
            )
        };

        if closed {
            return EventDelivery {
                consumer_count: 0,
                delivered_count: 0,
                coalesced_count: 0,
                dropped_progress_count: 0,
                evicted_progress_count: 0,
                dispatcher_closed: true,
            };
        }

        let mut delivery = EventDelivery {
            consumer_count: queues.len(),
            delivered_count: 0,
            coalesced_count: 0,
            dropped_progress_count: 0,
            evicted_progress_count: 0,
            dispatcher_closed: false,
        };
        for queue in queues {
            match queue.enqueue(event.clone()) {
                QueueOutcome::Delivered => delivery.delivered_count += 1,
                QueueOutcome::Coalesced => delivery.coalesced_count += 1,
                QueueOutcome::DroppedProgress => delivery.dropped_progress_count += 1,
                QueueOutcome::EvictedProgress => {
                    delivery.delivered_count += 1;
                    delivery.evicted_progress_count += 1;
                }
                QueueOutcome::Closed => {}
            }
        }
        delivery
    }

    /// Returns the number of registered consumers.
    pub fn consumer_count(&self) -> usize {
        lock_or_recover(&self.inner.state).consumers.len()
    }

    /// Returns whether this dispatcher will reject new consumers and report
    /// future publications as closed.
    pub fn is_closed(&self) -> bool {
        lock_or_recover(&self.inner.state).closed
    }

    /// Closes the dispatcher after allowing already queued events to drain.
    pub fn close(&self) {
        let queues = {
            let mut state = lock_or_recover(&self.inner.state);
            if state.closed {
                return;
            }
            state.closed = true;
            state
                .consumers
                .drain(..)
                .map(|consumer| consumer.queue)
                .collect::<Vec<_>>()
        };
        for queue in queues {
            queue.close();
        }
    }

    pub(crate) fn from_valid_capacity(capacity: usize) -> Self {
        Self {
            inner: Arc::new(DispatcherInner {
                capacity,
                state: Mutex::new(DispatcherState {
                    closed: false,
                    next_consumer_id: 1,
                    consumers: Vec::new(),
                }),
            }),
        }
    }

    fn remove_consumer(&self, id: u64) {
        self.inner.remove_consumer(id);
    }
}

impl fmt::Debug for EventDispatcher {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("EventDispatcher")
            .field("capacity", &self.capacity())
            .field("consumer_count", &self.consumer_count())
            .field("closed", &self.is_closed())
            .finish()
    }
}

impl DispatcherInner {
    fn remove_consumer(&self, id: u64) {
        let mut state = lock_or_recover(&self.state);
        state.consumers.retain(|consumer| consumer.id != id);
    }
}

impl ConsumerQueue {
    fn new(capacity: usize) -> Self {
        Self {
            capacity,
            state: Mutex::new(QueueState {
                closed: false,
                events: VecDeque::new(),
            }),
            changed: Condvar::new(),
        }
    }

    fn enqueue(&self, event: EventEnvelope) -> QueueOutcome {
        let mut state = lock_or_recover(&self.state);
        loop {
            if state.closed {
                return QueueOutcome::Closed;
            }
            if state.events.len() < self.capacity {
                state.events.push_back(event);
                self.changed.notify_one();
                return QueueOutcome::Delivered;
            }

            if event.kind().is_coalescible() {
                if state
                    .events
                    .back()
                    .is_some_and(|queued| queued.kind().is_coalescible())
                {
                    let Some(last) = state.events.back_mut() else {
                        return QueueOutcome::DroppedProgress;
                    };
                    *last = event;
                    self.changed.notify_one();
                    return QueueOutcome::Coalesced;
                }
                return QueueOutcome::DroppedProgress;
            }

            if let Some(index) = state
                .events
                .iter()
                .position(|queued| queued.kind().is_coalescible())
            {
                let _ = state.events.remove(index);
                state.events.push_back(event);
                self.changed.notify_one();
                return QueueOutcome::EvictedProgress;
            }

            state = wait_or_recover(&self.changed, state);
        }
    }

    fn next(&self) -> Option<EventEnvelope> {
        let mut state = lock_or_recover(&self.state);
        loop {
            if let Some(event) = state.events.pop_front() {
                self.changed.notify_all();
                return Some(event);
            }
            if state.closed {
                return None;
            }
            state = wait_or_recover(&self.changed, state);
        }
    }

    fn close(&self) {
        let mut state = lock_or_recover(&self.state);
        state.closed = true;
        self.changed.notify_all();
    }
}

/// Handle for one registered event consumer.
///
/// Dropping or explicitly closing a subscription removes it from future
/// publication. Events already in its bounded queue are delivered before its
/// worker exits, unless the callback panics.
pub struct EventSubscription {
    id: u64,
    dispatcher: Weak<DispatcherInner>,
    queue: Arc<ConsumerQueue>,
}

impl EventSubscription {
    /// Returns the opaque consumer identifier local to its dispatcher.
    pub const fn id(&self) -> u64 {
        self.id
    }

    /// Returns whether this subscription has stopped accepting new events.
    pub fn is_closed(&self) -> bool {
        lock_or_recover(&self.queue.state).closed
    }

    /// Stops this subscription; queued events are allowed to drain.
    pub fn close(&self) {
        self.queue.close();
        if let Some(dispatcher) = self.dispatcher.upgrade() {
            dispatcher.remove_consumer(self.id);
        }
    }
}

impl fmt::Debug for EventSubscription {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("EventSubscription")
            .field("id", &self.id)
            .field("closed", &self.is_closed())
            .finish()
    }
}

impl Drop for EventSubscription {
    fn drop(&mut self) {
        self.close();
    }
}

fn run_consumer<C>(
    queue: Arc<ConsumerQueue>,
    dispatcher: Weak<DispatcherInner>,
    id: u64,
    mut consumer: C,
) where
    C: EventConsumer,
{
    while let Some(event) = queue.next() {
        let callback_result = catch_unwind(AssertUnwindSafe(|| consumer.on_event(event)));
        if callback_result.is_err() {
            queue.close();
            if let Some(dispatcher) = dispatcher.upgrade() {
                dispatcher.remove_consumer(id);
            }
            return;
        }
    }
    if let Some(dispatcher) = dispatcher.upgrade() {
        dispatcher.remove_consumer(id);
    }
}

fn lock_or_recover<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    match mutex.lock() {
        Ok(guard) => guard,
        Err(poisoned) => poisoned.into_inner(),
    }
}

fn wait_or_recover<'a, T>(condvar: &Condvar, guard: MutexGuard<'a, T>) -> MutexGuard<'a, T> {
    match condvar.wait(guard) {
        Ok(guard) => guard,
        Err(poisoned) => poisoned.into_inner(),
    }
}
