use super::operation::{OperationId, OperationStatus};
use std::fmt;

/// Stable machine-readable categories for [`SdkError`].
#[non_exhaustive]
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub enum ErrorCode {
    /// A client or dispatcher configuration value is invalid.
    InvalidConfiguration,
    /// A request or event contains an invalid value.
    InvalidRequest,
    /// No global author identity has been configured.
    IdentityNotConfigured,
    /// The persisted global identity configuration is malformed.
    ConfigurationMalformed,
    /// The persisted global identity configuration version is unsupported.
    ConfigurationUnsupportedVersion,
    /// The global identity configuration could not be read or written.
    ConfigurationFailure,
    /// No more operation identifiers are available in this process.
    OperationIdExhausted,
    /// An operation method was called after an incompatible terminal state.
    OperationStateConflict,
    /// An operation has been cancelled.
    OperationCancelled,
    /// The event dispatcher has been closed.
    EventDispatcherClosed,
    /// An event consumer could not be registered.
    EventConsumerRegistration,
    /// An operation exhausted its event sequence range.
    OperationSequenceExhausted,
    /// A repository initialization found an existing root object.
    RepositoryAlreadyExists,
    /// A required repository root object is missing.
    RepositoryMissing,
    /// A repository root object or descriptor is malformed.
    RepositoryMalformed,
    /// A repository format or descriptor version is not supported.
    RepositoryUnsupportedVersion,
    /// A repository is validly encoded but incompatible with this SDK.
    RepositoryIncompatible,
    /// A repository HEAD publication lost its compare-and-swap race.
    RepositoryPublicationConflict,
    /// A requested snapshot object is missing.
    RepositorySnapshotMissing,
    /// An immutable object required by a requested snapshot is missing.
    RepositoryRequiredObjectMissing,
    /// The storage backend cannot perform conditional repository publication.
    StorageCapabilityUnsupported,
    /// The repository publication generation cannot be incremented.
    RepositoryGenerationExhausted,
    /// The configured storage backend failed a lifecycle operation.
    StorageFailure,
    /// A repository contains no published snapshots for the `latest` alias.
    RepositoryNoSnapshots,
    /// A snapshot reference was empty.
    SnapshotReferenceEmpty,
    /// A snapshot reference has invalid syntax.
    SnapshotReferenceMalformed,
    /// No snapshot matches a requested reference.
    SnapshotReferenceNotFound,
    /// More than one snapshot matches a requested prefix.
    SnapshotReferenceAmbiguous,
}

impl ErrorCode {
    /// Returns the stable lowercase code used by automation clients.
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::InvalidConfiguration => "invalid_configuration",
            Self::InvalidRequest => "invalid_request",
            Self::IdentityNotConfigured => "identity_not_configured",
            Self::ConfigurationMalformed => "configuration_malformed",
            Self::ConfigurationUnsupportedVersion => "configuration_unsupported_version",
            Self::ConfigurationFailure => "configuration_failure",
            Self::OperationIdExhausted => "operation_id_exhausted",
            Self::OperationStateConflict => "operation_state_conflict",
            Self::OperationCancelled => "operation_cancelled",
            Self::EventDispatcherClosed => "event_dispatcher_closed",
            Self::EventConsumerRegistration => "event_consumer_registration",
            Self::OperationSequenceExhausted => "operation_sequence_exhausted",
            Self::RepositoryAlreadyExists => "repository_already_exists",
            Self::RepositoryMissing => "repository_missing",
            Self::RepositoryMalformed => "repository_malformed",
            Self::RepositoryUnsupportedVersion => "repository_unsupported_version",
            Self::RepositoryIncompatible => "repository_incompatible",
            Self::RepositoryPublicationConflict => "repository_publication_conflict",
            Self::RepositorySnapshotMissing => "repository_snapshot_missing",
            Self::RepositoryRequiredObjectMissing => "repository_required_object_missing",
            Self::StorageCapabilityUnsupported => "storage_capability_unsupported",
            Self::RepositoryGenerationExhausted => "repository_generation_exhausted",
            Self::StorageFailure => "storage_failure",
            Self::RepositoryNoSnapshots => "repository_no_snapshots",
            Self::SnapshotReferenceEmpty => "snapshot_reference_empty",
            Self::SnapshotReferenceMalformed => "snapshot_reference_malformed",
            Self::SnapshotReferenceNotFound => "snapshot_reference_not_found",
            Self::SnapshotReferenceAmbiguous => "snapshot_reference_ambiguous",
        }
    }
}

impl fmt::Display for ErrorCode {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(self.as_str())
    }
}

/// Typed top-level error returned by the public SDK.
///
/// Error variants carry stable categories and intentionally avoid embedding
/// credentials, full local paths, or backend-specific error objects. Lower
/// layers can map their details to these variants without changing the public
/// contract. The enum is non-exhaustive so new categories can be added
/// compatibly.
#[non_exhaustive]
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum SdkError {
    /// A client or dispatcher configuration field is invalid.
    InvalidConfiguration {
        /// The stable configuration field name.
        field: &'static str,
        /// The stable reason for rejection.
        reason: &'static str,
    },
    /// A public request or event contains an invalid value.
    InvalidRequest {
        /// The stable request field name.
        field: &'static str,
        /// The stable reason for rejection.
        reason: &'static str,
    },
    /// No global author identity has been configured.
    IdentityNotConfigured,
    /// The persisted global identity configuration is malformed.
    ConfigurationMalformed {
        /// A stable explanation of the malformed configuration.
        reason: &'static str,
    },
    /// The persisted global identity configuration version is unsupported.
    ConfigurationUnsupportedVersion {
        /// The version found in the persisted configuration.
        version: u16,
    },
    /// A global identity configuration storage operation failed.
    ConfigurationFailure {
        /// The configuration operation that failed.
        operation: &'static str,
    },
    /// The process-wide operation identifier sequence is exhausted.
    OperationIdExhausted,
    /// An operation's event sequence is exhausted.
    OperationSequenceExhausted {
        /// The operation whose sequence cannot advance.
        operation_id: OperationId,
    },
    /// Initialization cannot proceed because a repository root object exists.
    RepositoryAlreadyExists,
    /// A required repository root object is missing.
    RepositoryMissing,
    /// A repository object is present but malformed.
    RepositoryMalformed {
        /// A stable explanation of the malformed condition.
        reason: &'static str,
    },
    /// A repository format or descriptor version is unsupported.
    RepositoryUnsupportedVersion {
        /// The version found in the persisted object.
        version: u16,
    },
    /// The repository is validly encoded but not compatible with this SDK.
    RepositoryIncompatible {
        /// A stable explanation of the incompatibility.
        reason: &'static str,
    },
    /// Another publisher changed HEAD after the supplied versioned read.
    RepositoryPublicationConflict,
    /// The requested snapshot object is missing.
    RepositorySnapshotMissing,
    /// An immutable object required by the requested snapshot is missing.
    RepositoryRequiredObjectMissing,
    /// The storage backend does not provide conditional HEAD publication.
    StorageCapabilityUnsupported,
    /// The repository HEAD generation cannot be incremented safely.
    RepositoryGenerationExhausted,
    /// A storage backend failed without exposing backend-specific details.
    StorageFailure {
        /// The lifecycle operation that failed.
        operation: &'static str,
    },
    /// The repository has no published snapshot for a `latest` request.
    RepositoryNoSnapshots,
    /// A snapshot reference is empty.
    SnapshotReferenceEmpty,
    /// A snapshot reference is malformed.
    SnapshotReferenceMalformed,
    /// No published snapshot matches a requested full ID or prefix.
    SnapshotReferenceNotFound,
    /// Multiple published snapshots match a requested prefix.
    SnapshotReferenceAmbiguous,
    /// An operation method conflicts with its current state.
    OperationStateConflict {
        /// The operation involved in the conflict.
        operation_id: OperationId,
        /// The state observed when the method was called.
        status: OperationStatus,
    },
    /// Cooperative cancellation was requested or observed.
    OperationCancelled {
        /// The operation involved, when the error came from an operation.
        operation_id: Option<OperationId>,
    },
    /// The dispatcher has been closed and cannot accept consumers.
    EventDispatcherClosed,
    /// A dedicated callback worker could not be started.
    EventConsumerRegistration,
}

impl SdkError {
    /// Returns the stable machine-readable category for this error.
    pub const fn code(&self) -> ErrorCode {
        match self {
            Self::InvalidConfiguration { .. } => ErrorCode::InvalidConfiguration,
            Self::InvalidRequest { .. } => ErrorCode::InvalidRequest,
            Self::IdentityNotConfigured => ErrorCode::IdentityNotConfigured,
            Self::ConfigurationMalformed { .. } => ErrorCode::ConfigurationMalformed,
            Self::ConfigurationUnsupportedVersion { .. } => {
                ErrorCode::ConfigurationUnsupportedVersion
            }
            Self::ConfigurationFailure { .. } => ErrorCode::ConfigurationFailure,
            Self::OperationIdExhausted => ErrorCode::OperationIdExhausted,
            Self::OperationSequenceExhausted { .. } => ErrorCode::OperationSequenceExhausted,
            Self::OperationStateConflict { .. } => ErrorCode::OperationStateConflict,
            Self::OperationCancelled { .. } => ErrorCode::OperationCancelled,
            Self::EventDispatcherClosed => ErrorCode::EventDispatcherClosed,
            Self::EventConsumerRegistration => ErrorCode::EventConsumerRegistration,
            Self::RepositoryAlreadyExists => ErrorCode::RepositoryAlreadyExists,
            Self::RepositoryMissing => ErrorCode::RepositoryMissing,
            Self::RepositoryMalformed { .. } => ErrorCode::RepositoryMalformed,
            Self::RepositoryUnsupportedVersion { .. } => ErrorCode::RepositoryUnsupportedVersion,
            Self::RepositoryIncompatible { .. } => ErrorCode::RepositoryIncompatible,
            Self::RepositoryPublicationConflict => ErrorCode::RepositoryPublicationConflict,
            Self::RepositorySnapshotMissing => ErrorCode::RepositorySnapshotMissing,
            Self::RepositoryRequiredObjectMissing => ErrorCode::RepositoryRequiredObjectMissing,
            Self::StorageCapabilityUnsupported => ErrorCode::StorageCapabilityUnsupported,
            Self::RepositoryGenerationExhausted => ErrorCode::RepositoryGenerationExhausted,
            Self::StorageFailure { .. } => ErrorCode::StorageFailure,
            Self::RepositoryNoSnapshots => ErrorCode::RepositoryNoSnapshots,
            Self::SnapshotReferenceEmpty => ErrorCode::SnapshotReferenceEmpty,
            Self::SnapshotReferenceMalformed => ErrorCode::SnapshotReferenceMalformed,
            Self::SnapshotReferenceNotFound => ErrorCode::SnapshotReferenceNotFound,
            Self::SnapshotReferenceAmbiguous => ErrorCode::SnapshotReferenceAmbiguous,
        }
    }

    /// Returns the operation identifier associated with this error, if any.
    pub const fn operation_id(&self) -> Option<OperationId> {
        match self {
            Self::OperationSequenceExhausted { operation_id }
            | Self::OperationStateConflict { operation_id, .. } => Some(*operation_id),
            Self::OperationCancelled { operation_id } => *operation_id,
            Self::InvalidConfiguration { .. }
            | Self::InvalidRequest { .. }
            | Self::IdentityNotConfigured
            | Self::ConfigurationMalformed { .. }
            | Self::ConfigurationUnsupportedVersion { .. }
            | Self::ConfigurationFailure { .. }
            | Self::OperationIdExhausted
            | Self::EventDispatcherClosed
            | Self::EventConsumerRegistration
            | Self::RepositoryAlreadyExists
            | Self::RepositoryMissing
            | Self::RepositoryMalformed { .. }
            | Self::RepositoryUnsupportedVersion { .. }
            | Self::RepositoryIncompatible { .. }
            | Self::RepositoryPublicationConflict
            | Self::RepositorySnapshotMissing
            | Self::RepositoryRequiredObjectMissing
            | Self::StorageCapabilityUnsupported
            | Self::RepositoryGenerationExhausted
            | Self::StorageFailure { .. }
            | Self::RepositoryNoSnapshots
            | Self::SnapshotReferenceEmpty
            | Self::SnapshotReferenceMalformed
            | Self::SnapshotReferenceNotFound
            | Self::SnapshotReferenceAmbiguous => None,
        }
    }

    /// Returns whether retrying the same request may succeed without changing
    /// its inputs or configuration.
    pub const fn is_retryable(&self) -> bool {
        false
    }
}

impl fmt::Display for SdkError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::InvalidConfiguration { field, reason } => {
                write!(formatter, "invalid configuration for {field}: {reason}")
            }
            Self::InvalidRequest { field, reason } => {
                write!(formatter, "invalid request field {field}: {reason}")
            }
            Self::IdentityNotConfigured => {
                formatter.write_str("global author identity is not configured")
            }
            Self::ConfigurationMalformed { reason } => {
                write!(
                    formatter,
                    "global identity configuration is malformed: {reason}"
                )
            }
            Self::ConfigurationUnsupportedVersion { version } => {
                write!(
                    formatter,
                    "global identity configuration version {version} is unsupported"
                )
            }
            Self::ConfigurationFailure { operation } => {
                write!(
                    formatter,
                    "global identity configuration {operation} operation failed"
                )
            }
            Self::OperationIdExhausted => {
                formatter.write_str("operation identifier space exhausted")
            }
            Self::OperationSequenceExhausted { operation_id } => {
                write!(
                    formatter,
                    "event sequence exhausted for operation {operation_id}"
                )
            }
            Self::OperationStateConflict {
                operation_id,
                status,
            } => write!(formatter, "operation {operation_id} is already {status}"),
            Self::OperationCancelled {
                operation_id: Some(operation_id),
            } => write!(formatter, "operation {operation_id} was cancelled"),
            Self::OperationCancelled { operation_id: None } => {
                formatter.write_str("operation was cancelled")
            }
            Self::EventDispatcherClosed => formatter.write_str("event dispatcher is closed"),
            Self::EventConsumerRegistration => {
                formatter.write_str("event consumer could not be registered")
            }
            Self::RepositoryAlreadyExists => formatter.write_str("repository already exists"),
            Self::RepositoryMissing => {
                formatter.write_str("repository is missing a required root object")
            }
            Self::RepositoryMalformed { reason } => {
                write!(formatter, "repository is malformed: {reason}")
            }
            Self::RepositoryUnsupportedVersion { version } => {
                write!(
                    formatter,
                    "repository format version {version} is unsupported"
                )
            }
            Self::RepositoryIncompatible { reason } => {
                write!(formatter, "repository is incompatible: {reason}")
            }
            Self::RepositoryPublicationConflict => {
                formatter.write_str("repository HEAD publication conflicted with another publisher")
            }
            Self::RepositorySnapshotMissing => {
                formatter.write_str("the requested snapshot object is missing")
            }
            Self::RepositoryRequiredObjectMissing => {
                formatter.write_str("a required snapshot object is missing")
            }
            Self::StorageCapabilityUnsupported => {
                formatter.write_str("storage does not support conditional HEAD publication")
            }
            Self::RepositoryGenerationExhausted => {
                formatter.write_str("repository HEAD publication generation is exhausted")
            }
            Self::StorageFailure { operation } => {
                write!(formatter, "repository storage {operation} operation failed")
            }
            Self::RepositoryNoSnapshots => {
                formatter.write_str("repository contains no published snapshots")
            }
            Self::SnapshotReferenceEmpty => {
                formatter.write_str("snapshot reference must not be empty")
            }
            Self::SnapshotReferenceMalformed => {
                formatter.write_str("snapshot reference is malformed")
            }
            Self::SnapshotReferenceNotFound => {
                formatter.write_str("no snapshot matches the requested reference")
            }
            Self::SnapshotReferenceAmbiguous => {
                formatter.write_str("snapshot reference is ambiguous; provide a longer reference")
            }
        }
    }
}

impl std::error::Error for SdkError {}

/// Result alias used by all public SDK operations.
pub type SdkResult<T> = std::result::Result<T, SdkError>;

/// Conventional result name for applications that prefer a shorter alias.
pub type Result<T> = SdkResult<T>;

/// A redacted, structured error suitable for an event payload.
///
/// It contains the stable code and retryability only. Human-readable errors
/// remain available through [`SdkError`] and are not copied into event streams
/// by default.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct ErrorSummary {
    code: ErrorCode,
    retryable: bool,
}

impl ErrorSummary {
    /// Creates an error summary from a stable code and retryability decision.
    pub const fn new(code: ErrorCode, retryable: bool) -> Self {
        Self { code, retryable }
    }

    /// Returns the stable error category.
    pub const fn code(self) -> ErrorCode {
        self.code
    }

    /// Returns whether the failed operation may be retried.
    pub const fn is_retryable(self) -> bool {
        self.retryable
    }
}

impl From<&SdkError> for ErrorSummary {
    fn from(error: &SdkError) -> Self {
        Self::new(error.code(), error.is_retryable())
    }
}
