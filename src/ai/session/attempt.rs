use super::budget::BudgetUsage;
use super::error::SessionError;
use super::model::{AgentPhase, ArtifactId, AttemptId, EvidenceId};
use super::redaction::{
    canonical_bytes, canonical_json, hash_bytes, is_safe_identifier, redact_text,
    safe_diagnostic_id,
};
use chrono::DateTime;
use serde::{Deserialize, Serialize};
use serde_json::Value;

pub(crate) const ATTEMPT_SCHEMA_VERSION: u32 = 1;
const MAX_ACTION_BYTES: usize = 128;
const MAX_FINGERPRINT_BYTES: usize = 128;
const MAX_ERROR_CODE_BYTES: usize = 96;
const MAX_ATTEMPT_REFS: usize = 512;

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub(crate) enum AttemptOutcome {
    Running,
    Succeeded,
    Failed,
    Interrupted,
    Cancelled,
    Rejected,
    Exhausted,
}

impl AttemptOutcome {
    pub(crate) fn is_terminal(self) -> bool {
        !matches!(self, Self::Running)
    }

    pub(crate) fn as_str(self) -> &'static str {
        match self {
            Self::Running => "running",
            Self::Succeeded => "succeeded",
            Self::Failed => "failed",
            Self::Interrupted => "interrupted",
            Self::Cancelled => "cancelled",
            Self::Rejected => "rejected",
            Self::Exhausted => "exhausted",
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub(crate) struct AttemptLog {
    pub(crate) schema_version: u32,
    pub(crate) attempt_id: AttemptId,
    pub(crate) phase: AgentPhase,
    pub(crate) action_type: String,
    pub(crate) canonical_fingerprint: String,
    pub(crate) started_at: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) ended_at: Option<String>,
    pub(crate) outcome: AttemptOutcome,
    pub(crate) budget_delta: BudgetUsage,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub(crate) artifact_refs: Vec<ArtifactId>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub(crate) evidence_refs: Vec<EvidenceId>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) safe_error_code: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) diagnostic_id: Option<String>,
}

impl AttemptLog {
    pub(crate) fn begin(
        phase: AgentPhase,
        action_type: impl Into<String>,
        arguments: &Value,
    ) -> Result<Self, SessionError> {
        Self::begin_with_id(AttemptId::new()?, phase, action_type, arguments)
    }

    pub(crate) fn begin_with_id(
        attempt_id: AttemptId,
        phase: AgentPhase,
        action_type: impl Into<String>,
        arguments: &Value,
    ) -> Result<Self, SessionError> {
        let action_type = normalize_action(action_type.into())?;
        Ok(Self {
            schema_version: ATTEMPT_SCHEMA_VERSION,
            attempt_id,
            phase,
            action_type: action_type.clone(),
            canonical_fingerprint: canonical_fingerprint(&action_type, arguments),
            started_at: timestamp(),
            ended_at: None,
            outcome: AttemptOutcome::Running,
            budget_delta: BudgetUsage::default(),
            artifact_refs: Vec::new(),
            evidence_refs: Vec::new(),
            safe_error_code: None,
            diagnostic_id: None,
        })
    }

    pub(crate) fn finish(
        &mut self,
        outcome: AttemptOutcome,
        budget_delta: BudgetUsage,
        artifact_refs: Vec<ArtifactId>,
        evidence_refs: Vec<EvidenceId>,
        safe_error_code: Option<String>,
    ) -> Result<(), SessionError> {
        if self.outcome != AttemptOutcome::Running || !outcome.is_terminal() {
            return Err(SessionError::InvalidAttempt);
        }
        let safe_error_code = safe_error_code.map(|code| sanitize_safe_error_code(&code));
        self.outcome = outcome;
        self.ended_at = Some(timestamp());
        self.budget_delta = budget_delta;
        self.artifact_refs = artifact_refs;
        self.evidence_refs = evidence_refs;
        self.safe_error_code = safe_error_code.clone();
        self.diagnostic_id = safe_error_code
            .as_deref()
            .map(|code| safe_diagnostic_id(code, &self.canonical_fingerprint));
        Ok(())
    }

    pub(crate) fn validate(&self) -> Result<(), SessionError> {
        if self.schema_version != ATTEMPT_SCHEMA_VERSION
            || self.action_type.is_empty()
            || self.action_type.len() > MAX_ACTION_BYTES
            || !is_sha256_hash(&self.canonical_fingerprint)
            || self.canonical_fingerprint.len() > MAX_FINGERPRINT_BYTES
            || !is_safe_identifier(self.attempt_id.as_str())
            || self.attempt_id.as_str().len() > 128
            || !self.attempt_id.as_str().starts_with("attempt-")
            || self.started_at.is_empty()
            || self.artifact_refs.len() > MAX_ATTEMPT_REFS
            || self.evidence_refs.len() > MAX_ATTEMPT_REFS
            || has_duplicates(&self.artifact_refs)
            || has_duplicates(&self.evidence_refs)
        {
            return Err(SessionError::InvalidAttempt);
        }
        let started_at = DateTime::parse_from_rfc3339(&self.started_at)
            .map_err(|_| SessionError::InvalidTimestamp)?;
        if let Some(ended_at) = &self.ended_at
            && DateTime::parse_from_rfc3339(ended_at).map_err(|_| SessionError::InvalidTimestamp)?
                < started_at
        {
            return Err(SessionError::InvalidTimestamp);
        }
        if self.outcome == AttemptOutcome::Running && self.ended_at.is_some() {
            return Err(SessionError::InvalidAttempt);
        }
        if self.outcome.is_terminal() && self.ended_at.is_none() {
            return Err(SessionError::InvalidAttempt);
        }
        if self.safe_error_code.as_ref().is_some_and(|code| {
            code.is_empty()
                || code.len() > MAX_ERROR_CODE_BYTES
                || code.chars().any(|character| {
                    !(character.is_ascii_lowercase()
                        || character.is_ascii_digit()
                        || matches!(character, '_' | '-'))
                })
        }) {
            return Err(SessionError::InvalidAttempt);
        }
        if self
            .diagnostic_id
            .as_ref()
            .is_some_and(|id| !is_safe_identifier(id) || !id.starts_with("diag-"))
        {
            return Err(SessionError::InvalidAttempt);
        }
        Ok(())
    }

    pub(crate) fn is_in_flight(&self) -> bool {
        self.outcome == AttemptOutcome::Running
    }

    pub(crate) fn outcome_string(&self) -> &'static str {
        self.outcome.as_str()
    }
}

pub(crate) type AttemptSummary = AttemptLog;

/// Hash only the normalized operation identity. Arguments never appear in an
/// attempt record, which keeps loop detection useful without retaining input
/// bodies or secrets.
pub(crate) fn canonical_fingerprint(action_type: &str, arguments: &Value) -> String {
    let action = redact_text(action_type).trim().to_ascii_lowercase();
    let canonical = canonical_json(arguments);
    let mut bytes = Vec::with_capacity(action.len() + 1 + canonical_bytes(&canonical).len());
    bytes.extend_from_slice(action.as_bytes());
    bytes.push(b'\n');
    bytes.extend_from_slice(&canonical_bytes(&canonical));
    hash_bytes("sha256:", &bytes)
}

/// The canonical value is also exposed for future gateways that need to
/// compare normalized dimensions without serializing arbitrary request JSON.
pub(crate) fn canonical_arguments(arguments: &Value) -> Value {
    canonical_json(arguments)
}

fn normalize_action(value: String) -> Result<String, SessionError> {
    let value = value.trim().to_ascii_lowercase();
    if value.is_empty() || value.len() > MAX_ACTION_BYTES || value.chars().any(char::is_control) {
        return Err(SessionError::InvalidAttempt);
    }
    Ok(value)
}

fn is_sha256_hash(value: &str) -> bool {
    let Some(digest) = value.strip_prefix("sha256:") else {
        return false;
    };
    digest.len() == 64 && digest.bytes().all(|byte| byte.is_ascii_hexdigit())
}

fn has_duplicates<T: Ord + Clone>(values: &[T]) -> bool {
    let mut sorted = values.to_vec();
    sorted.sort();
    sorted.windows(2).any(|values| values[0] == values[1])
}

pub(crate) fn sanitize_safe_error_code(value: &str) -> String {
    let value = value.trim().to_ascii_lowercase();
    if value.is_empty()
        || value.len() > MAX_ERROR_CODE_BYTES
        || value.chars().any(|character| {
            !(character.is_ascii_lowercase()
                || character.is_ascii_digit()
                || matches!(character, '_' | '-'))
        })
    {
        "operation_failed".to_string()
    } else {
        value
    }
}

fn timestamp() -> String {
    chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Millis, true)
}

pub(crate) fn timestamp_for_tests() -> String {
    timestamp()
}
