use super::attempt::AttemptLog;
use super::budget::AgentBudget;
use super::error::SessionError;
use super::redaction::{hash_bytes, is_safe_identifier};
use super::trace::{TraceEventKind, TraceLog};
use crate::ai::profiles::RuntimeProfile;
use chrono::{DateTime, SecondsFormat, Utc};
use rand_core::TryRngCore;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::fmt;
use std::str::FromStr;

pub(crate) const SESSION_SCHEMA_VERSION: u32 = 1;

macro_rules! stable_id {
    ($name:ident, $prefix:literal) => {
        #[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
        #[serde(transparent)]
        pub(crate) struct $name(pub(crate) String);

        impl $name {
            pub(crate) fn new() -> Result<Self, SessionError> {
                let mut random = [0_u8; 16];
                rand_core::OsRng
                    .try_fill_bytes(&mut random)
                    .map_err(|_| SessionError::io("generate identifier"))?;
                let mut value = String::with_capacity($prefix.len() + 33);
                value.push_str($prefix);
                value.push('-');
                for byte in random {
                    value.push_str(&format!("{byte:02x}"));
                }
                Self::from_string(value)
            }

            pub(crate) fn from_string(value: impl Into<String>) -> Result<Self, SessionError> {
                let value = value.into();
                if !is_safe_identifier(&value) || !value.starts_with(concat!($prefix, "-")) {
                    return Err(SessionError::InvalidIdentifier {
                        kind: stringify!($name).to_ascii_lowercase(),
                    });
                }
                Ok(Self(value))
            }

            pub(crate) fn as_str(&self) -> &str {
                &self.0
            }
        }

        impl AsRef<str> for $name {
            fn as_ref(&self) -> &str {
                self.as_str()
            }
        }

        impl fmt::Display for $name {
            fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
                formatter.write_str(self.as_str())
            }
        }

        impl FromStr for $name {
            type Err = SessionError;

            fn from_str(value: &str) -> Result<Self, Self::Err> {
                Self::from_string(value)
            }
        }
    };
}

stable_id!(AgentSessionId, "session");
stable_id!(TurnId, "turn");
stable_id!(AttemptId, "attempt");
stable_id!(ArtifactId, "artifact");
stable_id!(EvidenceId, "evidence");
stable_id!(TraceEventId, "trace");

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq, PartialOrd, Ord, Hash)]
#[serde(rename_all = "kebab-case")]
pub(crate) enum AgentPhase {
    Classify,
    Plan,
    Search,
    Analyze,
    Explain,
    #[serde(alias = "restore_preview")]
    RestorePreview,
    Confirm,
    Commit,
    Verify,
    Complete,
}

impl AgentPhase {
    pub(crate) fn as_str(self) -> &'static str {
        match self {
            Self::Classify => "classify",
            Self::Plan => "plan",
            Self::Search => "search",
            Self::Analyze => "analyze",
            Self::Explain => "explain",
            Self::RestorePreview => "restore-preview",
            Self::Confirm => "confirm",
            Self::Commit => "commit",
            Self::Verify => "verify",
            Self::Complete => "complete",
        }
    }

    /// Explicit finite-state transition table used by `SessionService`.
    pub(crate) fn can_transition_to(self, next: Self) -> bool {
        if self == next {
            return self != Self::Complete;
        }
        match self {
            Self::Classify => matches!(next, Self::Plan | Self::Explain | Self::Complete),
            Self::Plan => matches!(
                next,
                Self::Search
                    | Self::Analyze
                    | Self::Explain
                    | Self::RestorePreview
                    | Self::Confirm
                    | Self::Complete
            ),
            Self::Search => matches!(
                next,
                Self::Search | Self::Analyze | Self::Explain | Self::Plan
            ),
            Self::Analyze => matches!(
                next,
                Self::Search | Self::Explain | Self::RestorePreview | Self::Plan | Self::Complete
            ),
            Self::Explain => matches!(next, Self::Complete | Self::Plan),
            Self::RestorePreview => matches!(next, Self::Confirm | Self::Explain),
            Self::Confirm => matches!(next, Self::Commit | Self::Explain),
            Self::Commit => next == Self::Verify,
            Self::Verify => next == Self::Complete,
            Self::Complete => false,
        }
    }
}

impl fmt::Display for AgentPhase {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(self.as_str())
    }
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq, PartialOrd, Ord, Hash)]
#[serde(rename_all = "kebab-case")]
pub(crate) enum AgentSessionStatus {
    Running,
    #[serde(alias = "waiting_for_user")]
    WaitingForUser,
    Completed,
    Cancelled,
    Failed,
    #[serde(alias = "budget_exhausted")]
    BudgetExhausted,
}

impl AgentSessionStatus {
    pub(crate) fn is_terminal(self) -> bool {
        matches!(
            self,
            Self::Completed | Self::Cancelled | Self::Failed | Self::BudgetExhausted
        )
    }

    pub(crate) fn can_transition_to(self, next: Self) -> bool {
        if self == next {
            return !self.is_terminal();
        }
        match self {
            Self::Running => matches!(
                next,
                Self::WaitingForUser
                    | Self::Completed
                    | Self::Cancelled
                    | Self::Failed
                    | Self::BudgetExhausted
            ),
            Self::WaitingForUser => matches!(
                next,
                Self::Running | Self::Cancelled | Self::Failed | Self::BudgetExhausted
            ),
            Self::Completed | Self::Cancelled | Self::Failed | Self::BudgetExhausted => false,
        }
    }
}

impl fmt::Display for AgentSessionStatus {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(match self {
            Self::Running => "running",
            Self::WaitingForUser => "waiting-for-user",
            Self::Completed => "completed",
            Self::Cancelled => "cancelled",
            Self::Failed => "failed",
            Self::BudgetExhausted => "budget-exhausted",
        })
    }
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "kebab-case")]
pub(crate) enum StopReason {
    GoalSatisfied,
    NoCandidate,
    Ambiguous,
    EvidenceInsufficient,
    BudgetExhausted,
    UserCancelled,
    SafetyConfirmationRequired,
    DependencyFailed,
    InternalError,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct SessionLimits {
    pub(crate) max_id_bytes: usize,
    pub(crate) max_workflow_bytes: usize,
    pub(crate) max_references: usize,
    pub(crate) max_attempts: usize,
    pub(crate) max_trace_events: usize,
    pub(crate) max_session_file_bytes: usize,
}

impl Default for SessionLimits {
    fn default() -> Self {
        Self {
            max_id_bytes: 128,
            max_workflow_bytes: 128,
            max_references: 4_096,
            max_attempts: 1_024,
            max_trace_events: 4_096,
            max_session_file_bytes: 8 * 1024 * 1024,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub(crate) struct SessionSpec {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) session_id: Option<AgentSessionId>,
    pub(crate) turn_id: TurnId,
    pub(crate) conversation_id: String,
    pub(crate) workflow_id: String,
    pub(crate) workflow_version: String,
    pub(crate) runtime_profile: RuntimeProfile,
    pub(crate) budget: AgentBudget,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub(crate) struct AgentSession {
    pub(crate) schema_version: u32,
    pub(crate) session_id: AgentSessionId,
    pub(crate) turn_id: TurnId,
    pub(crate) conversation_id: String,
    /// A fingerprint of the request identity, never the raw request body.
    pub(crate) request_fingerprint: String,
    pub(crate) workflow_id: String,
    pub(crate) workflow_version: String,
    pub(crate) phase: AgentPhase,
    pub(crate) status: AgentSessionStatus,
    pub(crate) created_at: String,
    pub(crate) updated_at: String,
    pub(crate) runtime_profile: RuntimeProfile,
    pub(crate) budget: AgentBudget,
    #[serde(default)]
    pub(crate) artifact_refs: Vec<ArtifactId>,
    #[serde(default)]
    pub(crate) evidence_refs: Vec<EvidenceId>,
    #[serde(default)]
    pub(crate) attempts: Vec<AttemptLog>,
    #[serde(default)]
    pub(crate) trace: TraceLog,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) stop_reason: Option<StopReason>,
    #[serde(default)]
    pub(crate) revision: u64,
}

impl AgentSession {
    #[allow(clippy::too_many_arguments)]
    pub(crate) fn new(
        session_id: AgentSessionId,
        turn_id: TurnId,
        conversation_id: impl Into<String>,
        workflow_id: impl Into<String>,
        workflow_version: impl Into<String>,
        runtime_profile: RuntimeProfile,
        budget: AgentBudget,
    ) -> Result<Self, SessionError> {
        let turn_id_string = turn_id.to_string();
        let conversation_id = conversation_id.into();
        let workflow_id = workflow_id.into();
        let workflow_version = workflow_version.into();
        if !is_safe_identifier(&conversation_id)
            || !is_safe_identifier(&workflow_id)
            || workflow_id.len() > 128
            || !is_safe_identifier(&workflow_version)
            || workflow_version.len() > 128
        {
            return Err(SessionError::InvalidWorkflow);
        }
        let request_fingerprint = hash_bytes(
            "sha256:",
            format!("{conversation_id}\n{turn_id_string}\n{workflow_id}\n{workflow_version}")
                .as_bytes(),
        );
        let timestamp = current_timestamp();
        let mut session = Self {
            schema_version: SESSION_SCHEMA_VERSION,
            session_id,
            turn_id,
            conversation_id,
            request_fingerprint,
            workflow_id,
            workflow_version,
            phase: AgentPhase::Classify,
            status: AgentSessionStatus::Running,
            created_at: timestamp.clone(),
            updated_at: timestamp,
            runtime_profile,
            budget,
            artifact_refs: Vec::new(),
            evidence_refs: Vec::new(),
            attempts: Vec::new(),
            trace: TraceLog::new(),
            stop_reason: None,
            revision: 0,
        };
        session.record_trace(
            TraceEventKind::SessionStarted,
            "agent session started",
            None,
            Vec::new(),
            Vec::new(),
            None,
            false,
        )?;
        session.validate(SessionLimits::default())?;
        Ok(session)
    }

    pub(crate) fn for_turn(
        conversation_id: impl Into<String>,
        turn_id: TurnId,
        workflow_id: impl Into<String>,
        workflow_version: impl Into<String>,
        runtime_profile: RuntimeProfile,
        budget: AgentBudget,
    ) -> Result<Self, SessionError> {
        Self::new(
            AgentSessionId::new()?,
            turn_id,
            conversation_id,
            workflow_id,
            workflow_version,
            runtime_profile,
            budget,
        )
    }

    pub(crate) fn fake() -> Self {
        Self::for_turn(
            "conversation-test",
            TurnId::from_string("turn-test").expect("fake turn ID should be valid"),
            "fake-workflow",
            "1",
            RuntimeProfile::Balanced,
            AgentBudget::default_budget(),
        )
        .expect("fake session should be valid")
    }

    pub(crate) fn validate(&self, limits: SessionLimits) -> Result<(), SessionError> {
        if self.schema_version != SESSION_SCHEMA_VERSION {
            return Err(SessionError::UnsupportedSchemaVersion {
                id: self.session_id.to_string(),
                version: self.schema_version,
            });
        }
        for (kind, value) in [
            ("session ID", self.session_id.as_str()),
            ("turn ID", self.turn_id.as_str()),
        ] {
            if value.len() > limits.max_id_bytes || !is_safe_identifier(value) {
                return Err(SessionError::InvalidIdentifier {
                    kind: kind.to_string(),
                });
            }
        }
        if !is_safe_identifier(&self.conversation_id)
            || !is_safe_identifier(&self.workflow_id)
            || self.workflow_id.len() > limits.max_workflow_bytes
            || !is_safe_identifier(&self.workflow_version)
            || self.workflow_version.len() > limits.max_workflow_bytes
            || !is_sha256_hash(&self.request_fingerprint)
        {
            return Err(SessionError::InvalidWorkflow);
        }
        let created = parse_timestamp(&self.created_at)?;
        let updated = parse_timestamp(&self.updated_at)?;
        if updated < created {
            return Err(SessionError::InvalidTimestamp);
        }
        self.budget.validate()?;
        if self.artifact_refs.len() > limits.max_references
            || self.evidence_refs.len() > limits.max_references
            || self.attempts.len() > limits.max_attempts
            || self.trace.events.len() > limits.max_trace_events
        {
            return Err(SessionError::LimitExceeded {
                resource: "session references, attempts, or trace events".to_string(),
                limit: limits
                    .max_references
                    .max(limits.max_attempts)
                    .max(limits.max_trace_events),
                actual: self
                    .artifact_refs
                    .len()
                    .max(self.evidence_refs.len())
                    .max(self.attempts.len())
                    .max(self.trace.events.len()),
            });
        }
        if has_duplicates(&self.artifact_refs) || has_duplicates(&self.evidence_refs) {
            return Err(SessionError::InvalidWorkflow);
        }
        let mut attempt_ids = Vec::with_capacity(self.attempts.len());
        for attempt in &self.attempts {
            attempt.validate()?;
            if attempt.artifact_refs.len() > limits.max_references
                || attempt.evidence_refs.len() > limits.max_references
            {
                return Err(SessionError::LimitExceeded {
                    resource: "attempt references".to_string(),
                    limit: limits.max_references,
                    actual: attempt.artifact_refs.len().max(attempt.evidence_refs.len()),
                });
            }
            attempt_ids.push(attempt.attempt_id.clone());
        }
        if has_duplicates(&attempt_ids) {
            return Err(SessionError::InvalidAttempt);
        }
        self.trace.validate(&self.session_id)?;
        let terminal_events = self
            .trace
            .events
            .iter()
            .filter(|event| event.terminal)
            .count();
        if (self.status.is_terminal() && terminal_events != 1)
            || (!self.status.is_terminal() && terminal_events != 0)
        {
            return Err(SessionError::InvalidWorkflow);
        }
        if self.status == AgentSessionStatus::Completed && self.phase != AgentPhase::Complete {
            return Err(SessionError::InvalidStatusTransition {
                from: self.status,
                to: AgentSessionStatus::Completed,
            });
        }
        if self.status.is_terminal() && self.stop_reason.is_none() {
            return Err(SessionError::InvalidWorkflow);
        }
        Ok(())
    }

    pub(crate) fn is_terminal(&self) -> bool {
        self.status.is_terminal()
    }

    pub(crate) fn record_trace(
        &mut self,
        kind: TraceEventKind,
        summary: impl Into<String>,
        attempt_id: Option<AttemptId>,
        artifact_refs: Vec<ArtifactId>,
        evidence_refs: Vec<EvidenceId>,
        progress: Option<super::trace::TraceProgress>,
        terminal: bool,
    ) -> Result<(), SessionError> {
        self.trace.append_new(
            &self.session_id,
            kind,
            self.phase,
            self.status,
            summary,
            attempt_id,
            artifact_refs,
            evidence_refs,
            progress,
            terminal,
        )?;
        Ok(())
    }

    pub(crate) fn touch(&mut self) -> Result<(), SessionError> {
        self.updated_at = current_timestamp();
        self.revision = self
            .revision
            .checked_add(1)
            .ok_or(SessionError::InvalidWorkflow)?;
        Ok(())
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub(crate) struct SessionSummary {
    pub(crate) session_id: AgentSessionId,
    pub(crate) turn_id: TurnId,
    pub(crate) conversation_id: String,
    pub(crate) workflow_id: String,
    pub(crate) phase: AgentPhase,
    pub(crate) status: AgentSessionStatus,
    pub(crate) created_at: String,
    pub(crate) updated_at: String,
    pub(crate) revision: u64,
    pub(crate) artifact_count: usize,
    pub(crate) evidence_count: usize,
    pub(crate) attempt_count: usize,
    pub(crate) stop_reason: Option<StopReason>,
}

impl AgentSession {
    pub(crate) fn summary(&self) -> SessionSummary {
        SessionSummary {
            session_id: self.session_id.clone(),
            turn_id: self.turn_id.clone(),
            conversation_id: self.conversation_id.clone(),
            workflow_id: self.workflow_id.clone(),
            phase: self.phase,
            status: self.status,
            created_at: self.created_at.clone(),
            updated_at: self.updated_at.clone(),
            revision: self.revision,
            artifact_count: self.artifact_refs.len(),
            evidence_count: self.evidence_refs.len(),
            attempt_count: self.attempts.len(),
            stop_reason: self.stop_reason,
        }
    }
}

fn current_timestamp() -> String {
    Utc::now().to_rfc3339_opts(SecondsFormat::Millis, true)
}

fn parse_timestamp(value: &str) -> Result<DateTime<Utc>, SessionError> {
    DateTime::parse_from_rfc3339(value)
        .map(|value| value.with_timezone(&Utc))
        .map_err(|_| SessionError::InvalidTimestamp)
}

fn has_duplicates<T: Ord + Clone>(values: &[T]) -> bool {
    let mut sorted = values.to_vec();
    sorted.sort();
    sorted.windows(2).any(|values| values[0] == values[1])
}

fn is_sha256_hash(value: &str) -> bool {
    let Some(digest) = value.strip_prefix("sha256:") else {
        return false;
    };
    digest.len() == 64 && digest.bytes().all(|byte| byte.is_ascii_hexdigit())
}

#[allow(dead_code)]
fn _stable_hash(value: &str) -> String {
    format!("sha256:{:x}", Sha256::digest(value.as_bytes()))
}
