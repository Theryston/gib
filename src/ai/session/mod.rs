#![allow(dead_code)]

mod artifact;
mod attempt;
mod budget;
mod error;
mod evidence;
mod model;
mod redaction;
mod service;
mod store;
mod trace;

#[allow(unused_imports)]
pub(crate) use artifact::{
    ARTIFACT_SCHEMA_VERSION, ArtifactError, ArtifactHeader, ArtifactKind, ArtifactLimits,
    ArtifactRecord, ArtifactSensitivity, ArtifactStorageStatus, ArtifactStore, ArtifactTruncation,
    ArtifactWriteOptions, RetentionClass,
};
#[allow(unused_imports)]
pub(crate) use attempt::{
    ATTEMPT_SCHEMA_VERSION, AttemptLog, AttemptOutcome, AttemptSummary, canonical_arguments,
    canonical_fingerprint,
};
#[allow(unused_imports)]
pub(crate) use budget::{
    AgentBudget, AgentBudgetLimits, BUDGET_SCHEMA_VERSION, BudgetCost, BudgetDimension,
    BudgetError, BudgetSnapshot, BudgetUsage,
};
pub(crate) type BudgetLimits = AgentBudgetLimits;
#[allow(unused_imports)]
pub(crate) use error::SessionError;
#[allow(unused_imports)]
pub(crate) use evidence::{
    ConfidenceQualifier, EVIDENCE_SCHEMA_VERSION, EvidenceError, EvidenceFactKind, EvidenceKind,
    EvidenceLedger, EvidenceLimits, EvidenceNature, EvidenceRecord, EvidenceSource,
    EvidenceSourceKind, EvidenceStatus, EvidenceStore, FactOrInference,
};
#[allow(unused_imports)]
pub(crate) use model::{
    AgentPhase, AgentSession, AgentSessionId, AgentSessionStatus, ArtifactId, AttemptId,
    EvidenceId, SESSION_SCHEMA_VERSION, SessionLimits, SessionSpec, SessionSummary, StopReason,
    TraceEventId, TurnId,
};
pub(crate) type SessionPhase = AgentPhase;
pub(crate) type SessionStatus = AgentSessionStatus;
pub(crate) type ArtifactStatus = ArtifactStorageStatus;
pub(crate) type ArtifactRetentionClass = RetentionClass;
pub(crate) type EvidenceConfidence = ConfidenceQualifier;
#[allow(unused_imports)]
pub(crate) use redaction::{
    DebugDetailPolicy, DebugDetailStore, RedactionError, canonical_bytes, canonical_json,
    hash_bytes, redact_json, redact_text, safe_diagnostic_id,
};
#[allow(unused_imports)]
pub(crate) use service::{SessionEventSink, SessionService};
#[allow(unused_imports)]
pub(crate) use store::{SessionList, SessionPaths, SessionStore, SessionWarning};
#[allow(unused_imports)]
pub(crate) use trace::{
    TRACE_SCHEMA_VERSION, TraceError, TraceEvent, TraceEventKind, TraceLog, TraceProgress,
};

use std::sync::Arc;

/// Build the optional frontend adapter for the same trace events used by
/// interactive and JSON output. The service remains silent unless this sink
/// is explicitly installed by a command/orchestrator.
pub(crate) fn output_event_sink() -> SessionEventSink {
    Arc::new(|event| crate::output::emit_agent_trace_event(event))
}

#[cfg(test)]
mod tests;
