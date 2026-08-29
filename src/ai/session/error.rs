use super::artifact::ArtifactError;
use super::budget::BudgetError;
use super::evidence::EvidenceError;
use super::model::{AgentPhase, AgentSessionStatus};
use super::trace::TraceError;
use serde::{Deserialize, Serialize};
use std::fmt;

/// Errors returned by the bounded agent-session subsystem.
///
/// Error messages intentionally contain only stable identifiers and safe
/// limits. Raw prompts, native diagnostics, credentials, and file contents do
/// not cross this boundary.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(tag = "code", content = "details", rename_all = "snake_case")]
pub(crate) enum SessionError {
    MissingHomeDirectory,
    InvalidIdentifier {
        kind: String,
    },
    InvalidWorkflow,
    InvalidTimestamp,
    SessionNotFound {
        id: String,
    },
    SessionAlreadyExists {
        id: String,
    },
    MalformedSession {
        id: String,
    },
    FutureSchemaVersion {
        id: String,
        version: u32,
    },
    UnsupportedSchemaVersion {
        id: String,
        version: u32,
    },
    RevisionConflict {
        id: String,
        expected: u64,
        actual: u64,
    },
    InvalidPhaseTransition {
        from: AgentPhase,
        to: AgentPhase,
    },
    InvalidStatusTransition {
        from: AgentSessionStatus,
        to: AgentSessionStatus,
    },
    SessionTerminal,
    InvalidAttempt,
    AttemptNotFound {
        id: String,
    },
    DuplicateReference {
        kind: String,
        id: String,
    },
    MissingReference {
        kind: String,
        id: String,
    },
    LimitExceeded {
        resource: String,
        limit: usize,
        actual: usize,
    },
    LockTimeout {
        scope: String,
    },
    UnsafePath,
    IoError {
        operation: String,
    },
    SerializationError {
        operation: String,
    },
    Trace(TraceError),
    Budget(BudgetError),
    Artifact(ArtifactError),
    Evidence(EvidenceError),
}

impl SessionError {
    pub(crate) fn code(&self) -> &'static str {
        match self {
            Self::MissingHomeDirectory => "missing_home_directory",
            Self::InvalidIdentifier { .. } => "invalid_identifier",
            Self::InvalidWorkflow => "invalid_workflow",
            Self::InvalidTimestamp => "invalid_timestamp",
            Self::SessionNotFound { .. } => "session_not_found",
            Self::SessionAlreadyExists { .. } => "session_already_exists",
            Self::MalformedSession { .. } => "malformed_session",
            Self::FutureSchemaVersion { .. } => "future_schema_version",
            Self::UnsupportedSchemaVersion { .. } => "unsupported_schema_version",
            Self::RevisionConflict { .. } => "revision_conflict",
            Self::InvalidPhaseTransition { .. } => "invalid_phase_transition",
            Self::InvalidStatusTransition { .. } => "invalid_status_transition",
            Self::SessionTerminal => "session_terminal",
            Self::InvalidAttempt => "invalid_attempt",
            Self::AttemptNotFound { .. } => "attempt_not_found",
            Self::DuplicateReference { .. } => "duplicate_reference",
            Self::MissingReference { .. } => "missing_reference",
            Self::LimitExceeded { .. } => "limit_exceeded",
            Self::LockTimeout { .. } => "lock_timeout",
            Self::UnsafePath => "unsafe_path",
            Self::IoError { .. } => "io_error",
            Self::SerializationError { .. } => "serialization_error",
            Self::Trace(error) => error.code(),
            Self::Budget(error) => error.code(),
            Self::Artifact(error) => error.code(),
            Self::Evidence(error) => error.code(),
        }
    }

    pub(crate) fn io(operation: impl Into<String>) -> Self {
        Self::IoError {
            operation: operation.into(),
        }
    }

    pub(crate) fn serialization(operation: impl Into<String>) -> Self {
        Self::SerializationError {
            operation: operation.into(),
        }
    }
}

impl fmt::Display for SessionError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::MissingHomeDirectory => {
                formatter.write_str("the user home directory could not be determined")
            }
            Self::InvalidIdentifier { kind } => {
                write!(formatter, "{kind} must be a safe opaque identifier")
            }
            Self::InvalidWorkflow => {
                formatter.write_str("the session workflow identity is invalid")
            }
            Self::InvalidTimestamp => {
                formatter.write_str("session timestamps must be valid UTC RFC 3339 values")
            }
            Self::SessionNotFound { id } => write!(formatter, "agent session '{id}' was not found"),
            Self::SessionAlreadyExists { id } => {
                write!(formatter, "agent session '{id}' already exists")
            }
            Self::MalformedSession { id } => {
                write!(
                    formatter,
                    "agent session '{id}' is malformed or cannot be decoded"
                )
            }
            Self::FutureSchemaVersion { id, version } => write!(
                formatter,
                "agent session '{id}' uses schema version {version}, which is newer than this binary"
            ),
            Self::UnsupportedSchemaVersion { id, version } => write!(
                formatter,
                "agent session '{id}' uses unsupported schema version {version}"
            ),
            Self::RevisionConflict {
                id,
                expected,
                actual,
            } => write!(
                formatter,
                "agent session '{id}' changed from expected revision {expected} to revision {actual}"
            ),
            Self::InvalidPhaseTransition { from, to } => {
                write!(
                    formatter,
                    "phase transition from {from} to {to} is not allowed"
                )
            }
            Self::InvalidStatusTransition { from, to } => {
                write!(
                    formatter,
                    "session status transition from {from} to {to} is not allowed"
                )
            }
            Self::SessionTerminal => formatter.write_str("the agent session is already terminal"),
            Self::InvalidAttempt => formatter.write_str("the bounded attempt record is invalid"),
            Self::AttemptNotFound { id } => write!(formatter, "attempt '{id}' was not found"),
            Self::DuplicateReference { kind, id } => {
                write!(formatter, "{kind} reference '{id}' is already recorded")
            }
            Self::MissingReference { kind, id } => {
                write!(formatter, "{kind} reference '{id}' does not exist")
            }
            Self::LimitExceeded {
                resource,
                limit,
                actual,
            } => write!(formatter, "{resource} is {actual}; the limit is {limit}"),
            Self::LockTimeout { scope } => {
                write!(formatter, "timed out waiting for the {scope} lock")
            }
            Self::UnsafePath => formatter.write_str("refusing to use an unsafe session path"),
            Self::IoError { operation } => {
                write!(formatter, "session storage failed to {operation}")
            }
            Self::SerializationError { operation } => {
                write!(formatter, "session storage failed to {operation}")
            }
            Self::Trace(error) => write!(formatter, "session trace error: {error}"),
            Self::Budget(error) => write!(formatter, "session budget error: {error}"),
            Self::Artifact(error) => write!(formatter, "session artifact error: {error}"),
            Self::Evidence(error) => write!(formatter, "session evidence error: {error}"),
        }
    }
}

impl std::error::Error for SessionError {}

impl From<TraceError> for SessionError {
    fn from(error: TraceError) -> Self {
        Self::Trace(error)
    }
}

impl From<BudgetError> for SessionError {
    fn from(error: BudgetError) -> Self {
        Self::Budget(error)
    }
}

impl From<ArtifactError> for SessionError {
    fn from(error: ArtifactError) -> Self {
        Self::Artifact(error)
    }
}

impl From<EvidenceError> for SessionError {
    fn from(error: EvidenceError) -> Self {
        Self::Evidence(error)
    }
}
