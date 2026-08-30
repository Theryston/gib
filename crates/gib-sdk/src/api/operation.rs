use super::error::{ErrorSummary, SdkError, SdkResult};
use super::event::{
    CancellationInfo, EventDelivery, EventDispatcher, EventEnvelope, EventKind, EventPayload,
    EventPhase, Progress, RecoveryPoint,
};
use std::fmt;
use std::num::NonZeroU64;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

static NEXT_OPERATION_ID: AtomicU64 = AtomicU64::new(1);

/// Opaque identifier shared by an operation and all of its events.
///
/// Identifiers are unique within the process while the finite `u64` sequence
/// remains available. They contain no repository path, credential, or user
/// data and are safe to log or correlate in an event consumer.
#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub struct OperationId(NonZeroU64);

impl OperationId {
    /// Allocates the next process-local operation identifier.
    pub fn new() -> SdkResult<Self> {
        let mut current = NEXT_OPERATION_ID.load(Ordering::Relaxed);
        loop {
            let Some(next) = current.checked_add(1) else {
                return Err(SdkError::OperationIdExhausted);
            };
            match NEXT_OPERATION_ID.compare_exchange_weak(
                current,
                next,
                Ordering::Relaxed,
                Ordering::Relaxed,
            ) {
                Ok(_) => {
                    return Self::from_u64(current).ok_or(SdkError::OperationIdExhausted);
                }
                Err(observed) => current = observed,
            }
        }
    }

    /// Creates an identifier from a non-zero raw value, useful when restoring
    /// an externally persisted operation reference.
    pub const fn from_u64(value: u64) -> Option<Self> {
        match NonZeroU64::new(value) {
            Some(value) => Some(Self(value)),
            None => None,
        }
    }

    /// Returns the raw non-zero identifier value.
    pub const fn as_u64(self) -> u64 {
        self.0.get()
    }
}

impl fmt::Display for OperationId {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(formatter, "op-{}", self.as_u64())
    }
}

/// Kind of work represented by an operation.
#[non_exhaustive]
#[derive(Clone, Debug, Eq, Hash, PartialEq)]
pub enum OperationKind {
    /// A caller-defined or not-yet-specialized operation.
    Generic,
    /// A backup operation.
    Backup,
    /// A restore operation.
    Restore,
    /// A search operation.
    Search,
    /// An explore operation.
    Explore,
    /// A live/watch operation.
    Live,
    /// Repository maintenance.
    Maintenance,
    /// An extension-defined operation with a bounded descriptive name.
    Custom(String),
}

impl OperationKind {
    /// Creates a custom kind after validating its descriptive name.
    pub fn custom(name: impl Into<String>) -> SdkResult<Self> {
        let name = name.into();
        if name.is_empty() || name.len() > 64 {
            return Err(SdkError::InvalidRequest {
                field: "operation_kind.name",
                reason: "must contain between 1 and 64 UTF-8 bytes",
            });
        }
        Ok(Self::Custom(name))
    }
}

impl fmt::Display for OperationKind {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Generic => formatter.write_str("generic"),
            Self::Backup => formatter.write_str("backup"),
            Self::Restore => formatter.write_str("restore"),
            Self::Search => formatter.write_str("search"),
            Self::Explore => formatter.write_str("explore"),
            Self::Live => formatter.write_str("live"),
            Self::Maintenance => formatter.write_str("maintenance"),
            Self::Custom(name) => formatter.write_str(name),
        }
    }
}

/// Validated request used to start an SDK operation.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct OperationRequest {
    kind: OperationKind,
}

impl OperationRequest {
    /// Creates a request for the supplied operation kind.
    pub const fn new(kind: OperationKind) -> Self {
        Self { kind }
    }

    /// Returns the requested operation kind.
    pub const fn kind(&self) -> &OperationKind {
        &self.kind
    }

    /// Consumes the request and returns its operation kind.
    pub fn into_kind(self) -> OperationKind {
        self.kind
    }

    pub(crate) fn validate(&self) -> SdkResult<()> {
        if let OperationKind::Custom(name) = &self.kind
            && (name.is_empty() || name.len() > 64)
        {
            return Err(SdkError::InvalidRequest {
                field: "operation_kind.name",
                reason: "must contain between 1 and 64 UTF-8 bytes",
            });
        }
        Ok(())
    }
}

impl Default for OperationRequest {
    fn default() -> Self {
        Self::new(OperationKind::Generic)
    }
}

/// Public request convention for future typed SDK use cases.
pub trait Request {
    /// Result type returned when the request is accepted.
    type Response;
}

impl Request for OperationRequest {
    type Response = OperationHandle;
}

/// Current lifecycle state of an operation.
#[non_exhaustive]
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub enum OperationStatus {
    /// The operation can report progress or reach a terminal state.
    Running,
    /// The operation completed successfully.
    Completed,
    /// The operation failed.
    Failed,
    /// The operation was cancelled.
    Cancelled,
}

impl OperationStatus {
    /// Returns whether this state cannot transition again.
    pub const fn is_terminal(self) -> bool {
        !matches!(self, Self::Running)
    }

    /// Returns whether this state represents successful completion.
    pub const fn is_success(self) -> bool {
        matches!(self, Self::Completed)
    }
}

impl fmt::Display for OperationStatus {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        let value = match self {
            Self::Running => "running",
            Self::Completed => "completed",
            Self::Failed => "failed",
            Self::Cancelled => "cancelled",
        };
        formatter.write_str(value)
    }
}

/// Final or current result metadata for an operation.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct OperationResult {
    operation_id: OperationId,
    status: OperationStatus,
    last_event_sequence: u64,
}

impl OperationResult {
    /// Returns the operation identifier.
    pub const fn operation_id(self) -> OperationId {
        self.operation_id
    }

    /// Returns the operation status represented by this result.
    pub const fn status(self) -> OperationStatus {
        self.status
    }

    /// Returns the last event sequence assigned by the operation.
    pub const fn last_event_sequence(self) -> u64 {
        self.last_event_sequence
    }

    /// Returns whether the operation completed successfully.
    pub const fn is_success(self) -> bool {
        self.status.is_success()
    }

    /// Returns whether the operation ended through cooperative cancellation.
    pub const fn is_cancelled(self) -> bool {
        matches!(self.status, OperationStatus::Cancelled)
    }
}

/// Cloneable cooperative cancellation source.
///
/// Calling [`CancellationHandle::cancel`] is idempotent and non-blocking. It
/// only records a request; an operation worker must check the handle between
/// bounded units and then transition its [`OperationHandle`] to `Cancelled` so
/// that the terminal event is emitted.
#[derive(Clone, Debug)]
pub struct CancellationHandle {
    state: Arc<CancellationState>,
}

#[derive(Debug)]
struct CancellationState {
    cancelled: AtomicBool,
}

impl CancellationHandle {
    /// Creates an independent cancellation source.
    pub fn new() -> Self {
        Self {
            state: Arc::new(CancellationState {
                cancelled: AtomicBool::new(false),
            }),
        }
    }

    /// Requests cancellation and returns `true` only for the first request.
    pub fn cancel(&self) -> bool {
        !self.state.cancelled.swap(true, Ordering::Release)
    }

    /// Returns whether cancellation has been requested.
    pub fn is_cancelled(&self) -> bool {
        self.state.cancelled.load(Ordering::Acquire)
    }

    /// Returns a typed cancellation error when a request is pending.
    pub fn check(&self) -> SdkResult<()> {
        if self.is_cancelled() {
            Err(SdkError::OperationCancelled { operation_id: None })
        } else {
            Ok(())
        }
    }
}

impl Default for CancellationHandle {
    fn default() -> Self {
        Self::new()
    }
}

/// Compatibility name for [`CancellationHandle`] used by async integrations.
pub type CancellationToken = CancellationHandle;

/// Opaque control handle for one started operation.
///
/// Handles are cloneable and thread-safe. Terminal transitions are idempotent
/// only for a repeated cancellation; attempting to complete or fail an already
/// terminal operation returns [`SdkError::OperationStateConflict`]. Every
/// started operation emits exactly one terminal event when one of the terminal
/// methods succeeds.
#[derive(Clone)]
pub struct OperationHandle {
    core: Arc<OperationCore>,
}

struct OperationCore {
    operation_id: OperationId,
    kind: OperationKind,
    dispatcher: EventDispatcher,
    cancellation: CancellationHandle,
    state: Mutex<OperationState>,
    publication: Mutex<()>,
}

struct OperationState {
    status: OperationStatus,
    next_event_sequence: u64,
    last_event_sequence: u64,
}

struct ReservedEvent {
    event: EventEnvelope,
    result: OperationResult,
}

impl OperationHandle {
    pub(crate) fn start(dispatcher: EventDispatcher, request: OperationRequest) -> SdkResult<Self> {
        let operation_id = OperationId::new()?;
        let kind = request.into_kind();
        let started = EventEnvelope::new(
            operation_id,
            1,
            EventKind::Started,
            EventPhase::Starting,
            EventPayload::Empty,
        )?;
        let core = Arc::new(OperationCore {
            operation_id,
            kind,
            dispatcher,
            cancellation: CancellationHandle::new(),
            state: Mutex::new(OperationState {
                status: OperationStatus::Running,
                next_event_sequence: 2,
                last_event_sequence: 1,
            }),
            publication: Mutex::new(()),
        });
        core.dispatcher.publish(started);
        Ok(Self { core })
    }

    /// Returns the operation's opaque identifier.
    pub fn id(&self) -> OperationId {
        self.core.operation_id
    }

    /// Returns the requested operation kind.
    pub fn kind(&self) -> &OperationKind {
        &self.core.kind
    }

    /// Returns the current lifecycle status.
    pub fn status(&self) -> OperationStatus {
        lock_or_recover(&self.core.state).status
    }

    /// Returns the current result metadata without changing operation state.
    pub fn result(&self) -> OperationResult {
        let state = lock_or_recover(&self.core.state);
        OperationResult {
            operation_id: self.id(),
            status: state.status,
            last_event_sequence: state.last_event_sequence,
        }
    }

    /// Returns a cloneable cancellation handle for worker code.
    pub fn cancellation_handle(&self) -> CancellationHandle {
        self.core.cancellation.clone()
    }

    /// Alias for [`Self::cancellation_handle`].
    pub fn cancellation_token(&self) -> CancellationToken {
        self.cancellation_handle()
    }

    /// Returns whether cancellation has been requested.
    pub fn is_cancelled(&self) -> bool {
        self.core.cancellation.is_cancelled()
    }

    /// Checks cancellation and returns a typed error if requested.
    pub fn check_cancelled(&self) -> SdkResult<()> {
        if self.is_cancelled() {
            Err(SdkError::OperationCancelled {
                operation_id: Some(self.id()),
            })
        } else {
            Ok(())
        }
    }

    /// Reports bounded progress for the running operation.
    pub fn report_progress(&self, progress: Progress) -> SdkResult<EventDelivery> {
        let _publication = lock_or_recover(&self.core.publication);
        let reserved = self.reserve_event(
            OperationStatus::Running,
            EventKind::Progress,
            EventPhase::Running,
            EventPayload::Progress(progress),
        )?;
        Ok(self.core.dispatcher.publish(reserved.event))
    }

    /// Requests cancellation and emits the operation's terminal event.
    pub fn cancel(&self) -> SdkResult<OperationResult> {
        self.cancel_with_info(CancellationInfo::new(
            true,
            RecoveryPoint::OperationBoundary,
        ))
    }

    /// Requests cancellation with explicit, redacted resumability metadata.
    pub fn cancel_with_info(&self, cancellation: CancellationInfo) -> SdkResult<OperationResult> {
        let _publication = lock_or_recover(&self.core.publication);
        let reserved = {
            let mut state = lock_or_recover(&self.core.state);
            match state.status {
                OperationStatus::Running => {
                    self.core.cancellation.cancel();
                    reserve_event_locked(
                        self.id(),
                        &mut state,
                        OperationStatus::Cancelled,
                        EventKind::Cancelled,
                        EventPhase::Cancelled,
                        EventPayload::Cancellation(cancellation),
                    )?
                }
                OperationStatus::Cancelled => return Ok(current_result(self.id(), &state)),
                status => {
                    return Err(SdkError::OperationStateConflict {
                        operation_id: self.id(),
                        status,
                    });
                }
            }
        };
        self.core.dispatcher.publish(reserved.event);
        Ok(reserved.result)
    }

    /// Marks the operation as successfully completed and emits its terminal event.
    pub fn complete(&self) -> SdkResult<OperationResult> {
        let _publication = lock_or_recover(&self.core.publication);
        let reserved = self.reserve_event(
            OperationStatus::Completed,
            EventKind::Completed,
            EventPhase::Completed,
            EventPayload::Empty,
        )?;
        self.core.dispatcher.publish(reserved.event);
        Ok(reserved.result)
    }

    /// Marks the operation as failed and emits a redacted error event payload.
    pub fn fail(&self, error: SdkError) -> SdkResult<OperationResult> {
        let _publication = lock_or_recover(&self.core.publication);
        let payload = EventPayload::Error(ErrorSummary::from(&error));
        let reserved = self.reserve_event(
            OperationStatus::Failed,
            EventKind::Failed,
            EventPhase::Failed,
            payload,
        )?;
        self.core.dispatcher.publish(reserved.event);
        Ok(reserved.result)
    }

    fn reserve_event(
        &self,
        status: OperationStatus,
        kind: EventKind,
        phase: EventPhase,
        payload: EventPayload,
    ) -> SdkResult<ReservedEvent> {
        let mut state = lock_or_recover(&self.core.state);
        if state.status != OperationStatus::Running {
            return Err(SdkError::OperationStateConflict {
                operation_id: self.id(),
                status: state.status,
            });
        }
        if self.core.cancellation.is_cancelled() {
            return Err(SdkError::OperationCancelled {
                operation_id: Some(self.id()),
            });
        }
        reserve_event_locked(self.id(), &mut state, status, kind, phase, payload)
    }
}

impl fmt::Debug for OperationHandle {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("OperationHandle")
            .field("operation_id", &self.id())
            .field("kind", self.kind())
            .field("status", &self.status())
            .finish()
    }
}

fn reserve_event_locked(
    operation_id: OperationId,
    state: &mut OperationState,
    status: OperationStatus,
    kind: EventKind,
    phase: EventPhase,
    payload: EventPayload,
) -> SdkResult<ReservedEvent> {
    let sequence = state.next_event_sequence;
    let Some(next_sequence) = sequence.checked_add(1) else {
        return Err(SdkError::OperationSequenceExhausted { operation_id });
    };
    let event = EventEnvelope::new(operation_id, sequence, kind, phase, payload)?;
    state.status = status;
    state.next_event_sequence = next_sequence;
    state.last_event_sequence = sequence;
    Ok(ReservedEvent {
        event,
        result: OperationResult {
            operation_id,
            status,
            last_event_sequence: sequence,
        },
    })
}

fn current_result(operation_id: OperationId, state: &OperationState) -> OperationResult {
    OperationResult {
        operation_id,
        status: state.status,
        last_event_sequence: state.last_event_sequence,
    }
}

fn lock_or_recover<T>(mutex: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    match mutex.lock() {
        Ok(guard) => guard,
        Err(poisoned) => poisoned.into_inner(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cancellation_handle_is_idempotent() {
        let cancellation = CancellationHandle::new();
        assert!(cancellation.cancel());
        assert!(!cancellation.cancel());
        assert!(cancellation.is_cancelled());
        assert!(matches!(
            cancellation.check(),
            Err(SdkError::OperationCancelled { operation_id: None })
        ));
    }
}
