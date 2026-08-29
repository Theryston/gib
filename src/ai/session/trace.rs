use super::model::{
    AgentPhase, AgentSessionId, AgentSessionStatus, ArtifactId, AttemptId, EvidenceId, TraceEventId,
};
use super::redaction::{hash_bytes, is_safe_identifier, redact_text, truncate_text};
use chrono::{DateTime, SecondsFormat, Utc};
use serde::{Deserialize, Serialize};
use std::fmt;

pub(crate) const TRACE_SCHEMA_VERSION: u32 = 1;
const MAX_TRACE_SUMMARY_BYTES: usize = 512;
const MAX_TRACE_REFS: usize = 512;

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub(crate) enum TraceEventKind {
    SessionStarted,
    PhaseChanged,
    SessionWaitingForUser,
    SessionResumed,
    SessionCancelled,
    SessionCompleted,
    SessionFailed,
    SessionBudgetExhausted,
    AttemptStarted,
    AttemptCompleted,
    AttemptFailed,
    AttemptInterrupted,
    AttemptCancelled,
    AttemptRejected,
    ArtifactAdded,
    EvidenceAdded,
    BudgetConsumed,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub(crate) struct TraceProgress {
    pub(crate) completed: u64,
    pub(crate) total: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) signal: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub(crate) struct TraceEvent {
    pub(crate) schema_version: u32,
    pub(crate) trace_event_id: TraceEventId,
    pub(crate) session_id: AgentSessionId,
    pub(crate) sequence: u64,
    pub(crate) timestamp: String,
    pub(crate) kind: TraceEventKind,
    pub(crate) phase: AgentPhase,
    pub(crate) status: AgentSessionStatus,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) attempt_id: Option<AttemptId>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub(crate) artifact_refs: Vec<ArtifactId>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub(crate) evidence_refs: Vec<EvidenceId>,
    pub(crate) summary: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) progress: Option<TraceProgress>,
    pub(crate) terminal: bool,
}

impl TraceEvent {
    pub(crate) fn is_terminal(&self) -> bool {
        self.terminal
    }

    pub(crate) fn interactive_summary(&self) -> String {
        format!("[{}] {}", self.phase, self.summary)
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(tag = "code", content = "details", rename_all = "snake_case")]
pub(crate) enum TraceError {
    SequenceMismatch { expected: u64, actual: u64 },
    TerminalEventAlreadyEmitted,
    EventAfterTerminal,
    InvalidTrace,
}

impl TraceError {
    pub(crate) fn code(&self) -> &'static str {
        match self {
            Self::SequenceMismatch { .. } => "trace_sequence_mismatch",
            Self::TerminalEventAlreadyEmitted => "terminal_event_already_emitted",
            Self::EventAfterTerminal => "trace_event_after_terminal",
            Self::InvalidTrace => "invalid_trace",
        }
    }
}

impl fmt::Display for TraceError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::SequenceMismatch { expected, actual } => {
                write!(
                    formatter,
                    "trace sequence expected {expected}, received {actual}"
                )
            }
            Self::TerminalEventAlreadyEmitted => {
                formatter.write_str("the session already has a terminal trace event")
            }
            Self::EventAfterTerminal => {
                formatter.write_str("a trace event cannot be appended after session termination")
            }
            Self::InvalidTrace => formatter.write_str("the session trace is invalid"),
        }
    }
}

impl std::error::Error for TraceError {}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub(crate) struct TraceLog {
    pub(crate) schema_version: u32,
    pub(crate) next_sequence: u64,
    pub(crate) terminal_event_emitted: bool,
    #[serde(default)]
    pub(crate) events: Vec<TraceEvent>,
}

impl Default for TraceLog {
    fn default() -> Self {
        Self::new()
    }
}

impl TraceLog {
    pub(crate) fn new() -> Self {
        Self {
            schema_version: TRACE_SCHEMA_VERSION,
            next_sequence: 1,
            terminal_event_emitted: false,
            events: Vec::new(),
        }
    }

    #[allow(clippy::too_many_arguments)]
    pub(crate) fn append_new(
        &mut self,
        session_id: &AgentSessionId,
        kind: TraceEventKind,
        phase: AgentPhase,
        status: AgentSessionStatus,
        summary: impl Into<String>,
        attempt_id: Option<AttemptId>,
        artifact_refs: Vec<ArtifactId>,
        evidence_refs: Vec<EvidenceId>,
        progress: Option<TraceProgress>,
        terminal: bool,
    ) -> Result<TraceEvent, TraceError> {
        if self.terminal_event_emitted {
            return Err(TraceError::EventAfterTerminal);
        }
        if terminal && self.events.iter().any(TraceEvent::is_terminal) {
            return Err(TraceError::TerminalEventAlreadyEmitted);
        }
        let sequence = self.next_sequence;
        if sequence == 0 {
            return Err(TraceError::SequenceMismatch {
                expected: 1,
                actual: sequence,
            });
        }
        let next_sequence = sequence.checked_add(1).ok_or(TraceError::InvalidTrace)?;
        let trace_event_id = TraceEventId::from_string(hash_bytes(
            "trace-",
            format!("{}\n{}", session_id, sequence).as_bytes(),
        ))
        .map_err(|_| TraceError::InvalidTrace)?;
        let progress = progress.map(|mut progress| {
            progress.signal = progress
                .signal
                .take()
                .map(|signal| truncate_text(&redact_text(&signal), MAX_TRACE_SUMMARY_BYTES));
            progress
        });
        let event = TraceEvent {
            schema_version: TRACE_SCHEMA_VERSION,
            trace_event_id,
            session_id: session_id.clone(),
            sequence,
            timestamp: Utc::now().to_rfc3339_opts(SecondsFormat::Millis, true),
            kind,
            phase,
            status,
            attempt_id,
            artifact_refs,
            evidence_refs,
            summary: truncate_text(&redact_text(&summary.into()), MAX_TRACE_SUMMARY_BYTES),
            progress,
            terminal,
        };
        validate_event(&event, session_id)?;
        self.events.push(event.clone());
        self.next_sequence = next_sequence;
        if terminal {
            self.terminal_event_emitted = true;
        }
        Ok(event)
    }

    pub(crate) fn append(&mut self, event: TraceEvent) -> Result<(), TraceError> {
        if self.terminal_event_emitted {
            return Err(TraceError::EventAfterTerminal);
        }
        if event.sequence != self.next_sequence {
            return Err(TraceError::SequenceMismatch {
                expected: self.next_sequence,
                actual: event.sequence,
            });
        }
        if event.terminal && self.events.iter().any(TraceEvent::is_terminal) {
            return Err(TraceError::TerminalEventAlreadyEmitted);
        }
        validate_event(&event, &event.session_id)?;
        let next_sequence = self
            .next_sequence
            .checked_add(1)
            .ok_or(TraceError::InvalidTrace)?;
        self.events.push(event);
        self.next_sequence = next_sequence;
        if self.events.last().is_some_and(TraceEvent::is_terminal) {
            self.terminal_event_emitted = true;
        }
        Ok(())
    }

    pub(crate) fn validate(&self, session_id: &AgentSessionId) -> Result<(), TraceError> {
        if self.schema_version != TRACE_SCHEMA_VERSION
            || self.next_sequence != self.events.len() as u64 + 1
            || self.terminal_event_emitted != self.events.iter().any(TraceEvent::is_terminal)
        {
            return Err(TraceError::InvalidTrace);
        }
        for (index, event) in self.events.iter().enumerate() {
            if event.schema_version != TRACE_SCHEMA_VERSION
                || event.sequence != index as u64 + 1
                || event.session_id != *session_id
                || event.summary.len() > MAX_TRACE_SUMMARY_BYTES
            {
                return Err(TraceError::InvalidTrace);
            }
            validate_event(event, session_id)?;
        }
        if self.events.iter().filter(|event| event.terminal).count() > 1 {
            return Err(TraceError::InvalidTrace);
        }
        Ok(())
    }

    pub(crate) fn last(&self) -> Option<&TraceEvent> {
        self.events.last()
    }
}

fn validate_event(event: &TraceEvent, session_id: &AgentSessionId) -> Result<(), TraceError> {
    if event.schema_version != TRACE_SCHEMA_VERSION
        || event.session_id != *session_id
        || !is_safe_identifier(event.trace_event_id.as_str())
        || !event.trace_event_id.as_str().starts_with("trace-")
        || event.summary.is_empty()
        || event.summary != truncate_text(&redact_text(&event.summary), MAX_TRACE_SUMMARY_BYTES)
        || event.artifact_refs.len() > MAX_TRACE_REFS
        || event.evidence_refs.len() > MAX_TRACE_REFS
        || DateTime::parse_from_rfc3339(&event.timestamp).is_err()
    {
        return Err(TraceError::InvalidTrace);
    }
    let expected_id = TraceEventId::from_string(hash_bytes(
        "trace-",
        format!("{}\n{}", session_id, event.sequence).as_bytes(),
    ))
    .map_err(|_| TraceError::InvalidTrace)?;
    if event.trace_event_id != expected_id {
        return Err(TraceError::InvalidTrace);
    }
    if let Some(progress) = &event.progress {
        if progress.completed > progress.total
            || progress.signal.as_ref().is_some_and(|signal| {
                signal != &truncate_text(&redact_text(signal), MAX_TRACE_SUMMARY_BYTES)
            })
        {
            return Err(TraceError::InvalidTrace);
        }
    }
    let terminal_kind = matches!(
        event.kind,
        TraceEventKind::SessionCancelled
            | TraceEventKind::SessionCompleted
            | TraceEventKind::SessionFailed
            | TraceEventKind::SessionBudgetExhausted
    );
    if event.terminal != terminal_kind || (event.terminal && !event.status.is_terminal()) {
        return Err(TraceError::InvalidTrace);
    }
    if !event.terminal && event.status.is_terminal() {
        return Err(TraceError::InvalidTrace);
    }
    Ok(())
}
