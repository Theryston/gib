use super::context::ContextError;
use crate::ai::session::{ArtifactError, BudgetError, EvidenceError, SessionError, StopReason};
use serde::{Deserialize, Serialize};
use std::fmt;

/// Errors at the orchestration boundary are deliberately stable and safe to
/// expose to either frontend. They never contain prompts, model output, or
/// native runtime diagnostics.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(tag = "code", content = "details", rename_all = "snake_case")]
pub(crate) enum OrchestratorError {
    InvalidWorkflow { reason: String },
    DuplicatePhase { phase: String },
    UnknownPrerequisite { phase: String, prerequisite: String },
    DependencyCycle,
    EmptyWorkflow,
    InvalidIdentifier { kind: String },
    UnsupportedIntent { workflow: String, intent: String },
    MissingCapability { capability: String },
    InvalidPhaseTransition { from: String, to: String },
    PhaseNotReady { phase: String },
    NoReadyPhase,
    DependencyFailed { phase: String },
    ActiveAttempt,
    InvalidPhaseOutput { reason: String },
    MissingReference { kind: String, id: String },
    UndeclaredArtifactKind { phase: String, kind: String },
    UnsafeSideEffect { phase: String },
    SideEffectReplayBlocked { phase: String },
    RetryLimitExceeded { phase: String },
    NoProgress { phase: String },
    AlreadyTerminal { reason: Option<StopReason> },
    WaitingForUser,
    Cancelled,
    StateConflict,
    MalformedState,
    UnsupportedStateVersion { version: u32 },
    IoError { operation: String },
    SerializationError { operation: String },
    Session(SessionError),
    Context(ContextError),
}

impl OrchestratorError {
    pub(crate) fn code(&self) -> &'static str {
        match self {
            Self::InvalidWorkflow { .. } => "invalid_workflow",
            Self::DuplicatePhase { .. } => "duplicate_phase",
            Self::UnknownPrerequisite { .. } => "unknown_prerequisite",
            Self::DependencyCycle => "dependency_cycle",
            Self::EmptyWorkflow => "empty_workflow",
            Self::InvalidIdentifier { .. } => "invalid_identifier",
            Self::UnsupportedIntent { .. } => "unsupported_intent",
            Self::MissingCapability { .. } => "missing_capability",
            Self::InvalidPhaseTransition { .. } => "invalid_phase_transition",
            Self::PhaseNotReady { .. } => "phase_not_ready",
            Self::NoReadyPhase => "no_ready_phase",
            Self::DependencyFailed { .. } => "dependency_failed",
            Self::ActiveAttempt => "active_attempt",
            Self::InvalidPhaseOutput { .. } => "invalid_phase_output",
            Self::MissingReference { .. } => "missing_reference",
            Self::UndeclaredArtifactKind { .. } => "undeclared_artifact_kind",
            Self::UnsafeSideEffect { .. } => "unsafe_side_effect",
            Self::SideEffectReplayBlocked { .. } => "side_effect_replay_blocked",
            Self::RetryLimitExceeded { .. } => "retry_limit_exceeded",
            Self::NoProgress { .. } => "no_progress",
            Self::AlreadyTerminal { .. } => "already_terminal",
            Self::WaitingForUser => "waiting_for_user",
            Self::Cancelled => "cancelled",
            Self::StateConflict => "state_conflict",
            Self::MalformedState => "malformed_state",
            Self::UnsupportedStateVersion { .. } => "unsupported_state_version",
            Self::IoError { .. } => "io_error",
            Self::SerializationError { .. } => "serialization_error",
            Self::Session(error) => error.code(),
            Self::Context(error) => error.code(),
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

impl fmt::Display for OrchestratorError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::InvalidWorkflow { reason } => write!(formatter, "workflow is invalid: {reason}"),
            Self::DuplicatePhase { phase } => {
                write!(formatter, "phase '{phase}' is declared more than once")
            }
            Self::UnknownPrerequisite {
                phase,
                prerequisite,
            } => write!(
                formatter,
                "phase '{phase}' depends on unknown phase '{prerequisite}'"
            ),
            Self::DependencyCycle => {
                formatter.write_str("workflow phase dependencies contain a cycle")
            }
            Self::EmptyWorkflow => formatter.write_str("workflow must declare at least one phase"),
            Self::InvalidIdentifier { kind } => {
                write!(formatter, "{kind} must be a safe identifier")
            }
            Self::UnsupportedIntent { workflow, intent } => {
                write!(
                    formatter,
                    "workflow '{workflow}' does not support intent '{intent}'"
                )
            }
            Self::MissingCapability { capability } => {
                write!(
                    formatter,
                    "workflow capability '{capability}' is unavailable"
                )
            }
            Self::InvalidPhaseTransition { from, to } => {
                write!(
                    formatter,
                    "workflow cannot transition from '{from}' to '{to}'"
                )
            }
            Self::PhaseNotReady { phase } => write!(formatter, "phase '{phase}' is not ready"),
            Self::NoReadyPhase => formatter.write_str("the workflow has no ready phase"),
            Self::DependencyFailed { phase } => {
                write!(formatter, "phase '{phase}' has a failed dependency")
            }
            Self::ActiveAttempt => formatter.write_str("the session already has an active attempt"),
            Self::InvalidPhaseOutput { reason } => {
                write!(formatter, "phase output is invalid: {reason}")
            }
            Self::MissingReference { kind, id } => {
                write!(formatter, "{kind} reference '{id}' is missing")
            }
            Self::UndeclaredArtifactKind { phase, kind } => write!(
                formatter,
                "phase '{phase}' returned an undeclared artifact kind '{kind}'"
            ),
            Self::UnsafeSideEffect { phase } => {
                write!(
                    formatter,
                    "phase '{phase}' requested an undeclared side effect"
                )
            }
            Self::SideEffectReplayBlocked { phase } => write!(
                formatter,
                "replaying the side effecting phase '{phase}' is not safe"
            ),
            Self::RetryLimitExceeded { phase } => {
                write!(formatter, "phase '{phase}' exceeded its retry budget")
            }
            Self::NoProgress { phase } => {
                write!(formatter, "phase '{phase}' made no bounded progress")
            }
            Self::AlreadyTerminal { reason } => {
                write!(formatter, "the session is already terminal ({reason:?})")
            }
            Self::WaitingForUser => formatter.write_str("the session is waiting for user input"),
            Self::Cancelled => formatter.write_str("the orchestration was cancelled"),
            Self::StateConflict => {
                formatter.write_str("orchestrator state conflicts with the session")
            }
            Self::MalformedState => formatter.write_str("orchestrator state is malformed"),
            Self::UnsupportedStateVersion { version } => write!(
                formatter,
                "orchestrator state schema version {version} is newer than this binary"
            ),
            Self::IoError { operation } => {
                write!(formatter, "orchestrator storage failed to {operation}")
            }
            Self::SerializationError { operation } => {
                write!(formatter, "orchestrator storage failed to {operation}")
            }
            Self::Session(error) => error.fmt(formatter),
            Self::Context(error) => error.fmt(formatter),
        }
    }
}

impl std::error::Error for OrchestratorError {}

impl From<SessionError> for OrchestratorError {
    fn from(error: SessionError) -> Self {
        Self::Session(error)
    }
}

impl From<BudgetError> for OrchestratorError {
    fn from(error: BudgetError) -> Self {
        Self::Session(SessionError::Budget(error))
    }
}

impl From<ArtifactError> for OrchestratorError {
    fn from(error: ArtifactError) -> Self {
        Self::Session(SessionError::Artifact(error))
    }
}

impl From<EvidenceError> for OrchestratorError {
    fn from(error: EvidenceError) -> Self {
        Self::Session(SessionError::Evidence(error))
    }
}

impl From<ContextError> for OrchestratorError {
    fn from(error: ContextError) -> Self {
        Self::Context(error)
    }
}
