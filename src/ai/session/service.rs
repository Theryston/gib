use super::artifact::{ArtifactHeader, ArtifactRecord, ArtifactStore, ArtifactWriteOptions};
use super::attempt::{AttemptLog, AttemptOutcome};
use super::budget::{AgentBudget, BudgetCost, BudgetSnapshot, BudgetUsage};
use super::error::SessionError;
use super::evidence::{EvidenceLedger, EvidenceRecord};
use super::model::{
    AgentPhase, AgentSession, AgentSessionId, AgentSessionStatus, AttemptId, EvidenceId,
    SessionSpec, StopReason,
};
use super::trace::{TraceEvent, TraceEventKind, TraceProgress};
use serde_json::Value;
use std::sync::Arc;

use super::store::{SessionList, SessionStore};

pub(crate) type SessionEventSink = Arc<dyn Fn(&TraceEvent) + Send + Sync>;

/// User-facing operations over one recoverable agent-session store.
#[derive(Clone)]
pub(crate) struct SessionService {
    store: SessionStore,
    artifacts: ArtifactStore,
    evidence: EvidenceLedger,
    event_sink: Option<SessionEventSink>,
}

impl std::fmt::Debug for SessionService {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("SessionService")
            .field("store", &self.store)
            .field("artifacts", &self.artifacts)
            .field("evidence", &self.evidence)
            .field("event_sink_configured", &self.event_sink.is_some())
            .finish()
    }
}

impl SessionService {
    pub(crate) fn new(store: SessionStore) -> Self {
        let root = store.paths().root().to_path_buf();
        Self {
            store,
            artifacts: ArtifactStore::from_root(&root),
            evidence: EvidenceLedger::from_root(root),
            event_sink: None,
        }
    }

    pub(crate) fn default_store() -> Result<Self, SessionError> {
        Ok(Self::new(SessionStore::new()?))
    }

    pub(crate) fn with_artifact_store(mut self, artifacts: ArtifactStore) -> Self {
        self.artifacts = artifacts;
        self
    }

    pub(crate) fn with_evidence_ledger(mut self, evidence: EvidenceLedger) -> Self {
        self.evidence = evidence;
        self
    }

    pub(crate) fn with_event_sink(mut self, event_sink: SessionEventSink) -> Self {
        self.event_sink = Some(event_sink);
        self
    }

    pub(crate) fn store(&self) -> &SessionStore {
        &self.store
    }

    pub(crate) fn artifact_store(&self) -> &ArtifactStore {
        &self.artifacts
    }

    pub(crate) fn evidence_ledger(&self) -> &EvidenceLedger {
        &self.evidence
    }

    pub(crate) fn create(&self, spec: SessionSpec) -> Result<AgentSession, SessionError> {
        let session_id = match spec.session_id {
            Some(session_id) => session_id,
            None => AgentSessionId::new()?,
        };
        let session = AgentSession::new(
            session_id,
            spec.turn_id,
            spec.conversation_id,
            spec.workflow_id,
            spec.workflow_version,
            spec.runtime_profile,
            spec.budget,
        )?;
        let created = self.store.create_blocking(&session)?;
        self.emit_last_event(&created);
        Ok(created)
    }

    pub(crate) fn create_for_turn(
        &self,
        conversation_id: impl Into<String>,
        turn_id: super::model::TurnId,
        workflow_id: impl Into<String>,
        workflow_version: impl Into<String>,
        runtime_profile: crate::ai::profiles::RuntimeProfile,
        budget: AgentBudget,
    ) -> Result<AgentSession, SessionError> {
        self.create(SessionSpec {
            session_id: None,
            turn_id,
            conversation_id: conversation_id.into(),
            workflow_id: workflow_id.into(),
            workflow_version: workflow_version.into(),
            runtime_profile,
            budget,
        })
    }

    pub(crate) fn load(&self, session_id: impl AsRef<str>) -> Result<AgentSession, SessionError> {
        self.store.load_blocking(session_id)
    }

    pub(crate) fn list(&self) -> Result<SessionList, SessionError> {
        self.store.list_blocking()
    }

    pub(crate) fn transition_phase(
        &self,
        session_id: impl AsRef<str>,
        expected_revision: u64,
        next: AgentPhase,
    ) -> Result<AgentSession, SessionError> {
        self.mutate(session_id, expected_revision, |session| {
            require_running(session)?;
            if !session.phase.can_transition_to(next) {
                return Err(SessionError::InvalidPhaseTransition {
                    from: session.phase,
                    to: next,
                });
            }
            session.phase = next;
            if next == AgentPhase::Complete {
                transition_status(session, AgentSessionStatus::Completed)?;
                session.stop_reason = Some(StopReason::GoalSatisfied);
                session.record_trace(
                    TraceEventKind::SessionCompleted,
                    "agent session completed",
                    None,
                    Vec::new(),
                    Vec::new(),
                    None,
                    true,
                )?;
            } else {
                session.record_trace(
                    TraceEventKind::PhaseChanged,
                    format!("phase changed to {next}"),
                    None,
                    Vec::new(),
                    Vec::new(),
                    None,
                    false,
                )?;
            }
            Ok(())
        })
    }

    pub(crate) fn wait_for_user(
        &self,
        session_id: impl AsRef<str>,
        expected_revision: u64,
        reason: StopReason,
    ) -> Result<AgentSession, SessionError> {
        self.mutate(session_id, expected_revision, |session| {
            if session.is_terminal() {
                return Err(SessionError::SessionTerminal);
            }
            transition_status(session, AgentSessionStatus::WaitingForUser)?;
            session.stop_reason = Some(reason);
            session.record_trace(
                TraceEventKind::SessionWaitingForUser,
                "session is waiting for user input",
                None,
                Vec::new(),
                Vec::new(),
                None,
                false,
            )?;
            Ok(())
        })
    }

    pub(crate) fn resume(
        &self,
        session_id: impl AsRef<str>,
        expected_revision: u64,
    ) -> Result<AgentSession, SessionError> {
        self.mutate(session_id, expected_revision, |session| {
            if session.is_terminal() {
                return Err(SessionError::SessionTerminal);
            }
            if session.status != AgentSessionStatus::WaitingForUser {
                return Err(SessionError::InvalidStatusTransition {
                    from: session.status,
                    to: AgentSessionStatus::Running,
                });
            }
            transition_status(session, AgentSessionStatus::Running)?;
            session.stop_reason = None;
            session.record_trace(
                TraceEventKind::SessionResumed,
                "session resumed after user input",
                None,
                Vec::new(),
                Vec::new(),
                None,
                false,
            )?;
            Ok(())
        })
    }

    pub(crate) fn cancel(
        &self,
        session_id: impl AsRef<str>,
        expected_revision: u64,
    ) -> Result<AgentSession, SessionError> {
        self.mutate(session_id, expected_revision, |session| {
            if session.is_terminal() {
                return Err(SessionError::SessionTerminal);
            }
            transition_status(session, AgentSessionStatus::Cancelled)?;
            interrupt_in_flight(session)?;
            session.stop_reason = Some(StopReason::UserCancelled);
            session.record_trace(
                TraceEventKind::SessionCancelled,
                "agent session cancelled",
                None,
                Vec::new(),
                Vec::new(),
                None,
                true,
            )?;
            Ok(())
        })
    }

    pub(crate) fn complete(
        &self,
        session_id: impl AsRef<str>,
        expected_revision: u64,
    ) -> Result<AgentSession, SessionError> {
        self.transition_phase(session_id, expected_revision, AgentPhase::Complete)
    }

    pub(crate) fn fail(
        &self,
        session_id: impl AsRef<str>,
        expected_revision: u64,
        error_code: impl Into<String>,
    ) -> Result<AgentSession, SessionError> {
        self.fail_with_reason(
            session_id,
            expected_revision,
            error_code,
            StopReason::InternalError,
        )
    }

    pub(crate) fn fail_with_reason(
        &self,
        session_id: impl AsRef<str>,
        expected_revision: u64,
        error_code: impl Into<String>,
        reason: StopReason,
    ) -> Result<AgentSession, SessionError> {
        let error_code = super::attempt::sanitize_safe_error_code(&error_code.into());
        self.mutate(session_id, expected_revision, |session| {
            if session.is_terminal() {
                return Err(SessionError::SessionTerminal);
            }
            transition_status(session, AgentSessionStatus::Failed)?;
            interrupt_in_flight(session)?;
            session.stop_reason = Some(reason);
            session.record_trace(
                TraceEventKind::SessionFailed,
                format!("agent session failed ({error_code})"),
                None,
                Vec::new(),
                Vec::new(),
                None,
                true,
            )?;
            Ok(())
        })
    }

    pub(crate) fn mark_budget_exhausted(
        &self,
        session_id: impl AsRef<str>,
        expected_revision: u64,
    ) -> Result<AgentSession, SessionError> {
        self.mutate(session_id, expected_revision, |session| {
            if session.is_terminal() {
                return Err(SessionError::SessionTerminal);
            }
            transition_status(session, AgentSessionStatus::BudgetExhausted)?;
            interrupt_in_flight(session)?;
            session.stop_reason = Some(StopReason::BudgetExhausted);
            session.record_trace(
                TraceEventKind::SessionBudgetExhausted,
                "agent session budget exhausted",
                None,
                Vec::new(),
                Vec::new(),
                None,
                true,
            )?;
            Ok(())
        })
    }

    pub(crate) fn consume_budget(
        &self,
        session_id: impl AsRef<str>,
        expected_revision: u64,
        cost: BudgetCost,
    ) -> Result<(AgentSession, BudgetSnapshot), SessionError> {
        self.consume_budget_with_recovery(session_id, expected_revision, cost, true)
    }

    pub(crate) fn consume_budget_preserving_in_flight(
        &self,
        session_id: impl AsRef<str>,
        expected_revision: u64,
        cost: BudgetCost,
    ) -> Result<(AgentSession, BudgetSnapshot), SessionError> {
        self.consume_budget_with_recovery(session_id, expected_revision, cost, false)
    }

    fn consume_budget_with_recovery(
        &self,
        session_id: impl AsRef<str>,
        expected_revision: u64,
        cost: BudgetCost,
        recover_running_attempts: bool,
    ) -> Result<(AgentSession, BudgetSnapshot), SessionError> {
        let session_id = session_id.as_ref().to_string();
        let result = self.mutate_with_recovery(
            &session_id,
            expected_revision,
            recover_running_attempts,
            |session| {
                require_running(session)?;
                let snapshot = session.budget.checked_consume(cost)?;
                session.record_trace(
                    TraceEventKind::BudgetConsumed,
                    "session budget consumed",
                    None,
                    Vec::new(),
                    Vec::new(),
                    Some(TraceProgress {
                        completed: snapshot.consumed.model_calls,
                        total: snapshot
                            .consumed
                            .model_calls
                            .saturating_add(snapshot.remaining.model_calls),
                        signal: Some("budget_charge".to_string()),
                    }),
                    false,
                )?;
                Ok(())
            },
        );
        match result {
            Ok(session) => {
                let snapshot = session.budget.snapshot()?;
                Ok((session, snapshot))
            }
            Err(error @ SessionError::Budget(_)) => {
                let current = self.load(&session_id)?;
                if !current.is_terminal() {
                    let _ = self.mark_budget_exhausted(&session_id, current.revision);
                }
                Err(error)
            }
            Err(error) => Err(error),
        }
    }

    pub(crate) fn append_artifact_reference(
        &self,
        session_id: impl AsRef<str>,
        expected_revision: u64,
        artifact_id: super::model::ArtifactId,
    ) -> Result<AgentSession, SessionError> {
        self.append_artifact_reference_with_recovery(
            session_id,
            expected_revision,
            artifact_id,
            true,
        )
    }

    pub(crate) fn append_artifact_reference_preserving_in_flight(
        &self,
        session_id: impl AsRef<str>,
        expected_revision: u64,
        artifact_id: super::model::ArtifactId,
    ) -> Result<AgentSession, SessionError> {
        self.append_artifact_reference_with_recovery(
            session_id,
            expected_revision,
            artifact_id,
            false,
        )
    }

    fn append_artifact_reference_with_recovery(
        &self,
        session_id: impl AsRef<str>,
        expected_revision: u64,
        artifact_id: super::model::ArtifactId,
        recover_running_attempts: bool,
    ) -> Result<AgentSession, SessionError> {
        if self.artifacts.get(&artifact_id)?.is_none() {
            return Err(SessionError::MissingReference {
                kind: "artifact".to_string(),
                id: artifact_id.to_string(),
            });
        }
        self.mutate_with_recovery(
            session_id,
            expected_revision,
            recover_running_attempts,
            |session| {
                require_running(session)?;
                if session.artifact_refs.contains(&artifact_id) {
                    return Err(SessionError::DuplicateReference {
                        kind: "artifact".to_string(),
                        id: artifact_id.to_string(),
                    });
                }
                if session.artifact_refs.len() >= self.store.limits().max_references {
                    return Err(SessionError::LimitExceeded {
                        resource: "artifact references".to_string(),
                        limit: self.store.limits().max_references,
                        actual: session.artifact_refs.len() + 1,
                    });
                }
                session.artifact_refs.push(artifact_id.clone());
                session.record_trace(
                    TraceEventKind::ArtifactAdded,
                    "artifact reference added",
                    None,
                    vec![artifact_id],
                    Vec::new(),
                    None,
                    false,
                )?;
                Ok(())
            },
        )
    }

    pub(crate) fn append_evidence_reference(
        &self,
        session_id: impl AsRef<str>,
        expected_revision: u64,
        evidence_id: EvidenceId,
    ) -> Result<AgentSession, SessionError> {
        self.append_evidence_reference_with_recovery(
            session_id,
            expected_revision,
            evidence_id,
            true,
        )
    }

    pub(crate) fn append_evidence_reference_preserving_in_flight(
        &self,
        session_id: impl AsRef<str>,
        expected_revision: u64,
        evidence_id: EvidenceId,
    ) -> Result<AgentSession, SessionError> {
        self.append_evidence_reference_with_recovery(
            session_id,
            expected_revision,
            evidence_id,
            false,
        )
    }

    fn append_evidence_reference_with_recovery(
        &self,
        session_id: impl AsRef<str>,
        expected_revision: u64,
        evidence_id: EvidenceId,
        recover_running_attempts: bool,
    ) -> Result<AgentSession, SessionError> {
        if self.evidence.get(&evidence_id)?.is_none() {
            return Err(SessionError::MissingReference {
                kind: "evidence".to_string(),
                id: evidence_id.to_string(),
            });
        }
        self.mutate_with_recovery(
            session_id,
            expected_revision,
            recover_running_attempts,
            |session| {
                require_running(session)?;
                if session.evidence_refs.contains(&evidence_id) {
                    return Err(SessionError::DuplicateReference {
                        kind: "evidence".to_string(),
                        id: evidence_id.to_string(),
                    });
                }
                if session.evidence_refs.len() >= self.store.limits().max_references {
                    return Err(SessionError::LimitExceeded {
                        resource: "evidence references".to_string(),
                        limit: self.store.limits().max_references,
                        actual: session.evidence_refs.len() + 1,
                    });
                }
                session.evidence_refs.push(evidence_id.clone());
                session.record_trace(
                    TraceEventKind::EvidenceAdded,
                    "evidence reference added",
                    None,
                    Vec::new(),
                    vec![evidence_id],
                    None,
                    false,
                )?;
                Ok(())
            },
        )
    }

    pub(crate) fn begin_attempt(
        &self,
        session_id: impl AsRef<str>,
        expected_revision: u64,
        action_type: impl Into<String>,
        arguments: &Value,
    ) -> Result<(AgentSession, AttemptId), SessionError> {
        let current = self.load(session_id.as_ref())?;
        if current.revision != expected_revision {
            return Err(SessionError::RevisionConflict {
                id: current.session_id.to_string(),
                expected: expected_revision,
                actual: current.revision,
            });
        }
        require_running(&current)?;
        let attempt = AttemptLog::begin(current.phase, action_type, arguments)?;
        let attempt_id = attempt.attempt_id.clone();
        let session = self.append_attempt(session_id, expected_revision, attempt)?;
        Ok((session, attempt_id))
    }

    pub(crate) fn append_attempt(
        &self,
        session_id: impl AsRef<str>,
        expected_revision: u64,
        attempt: AttemptLog,
    ) -> Result<AgentSession, SessionError> {
        attempt.validate()?;
        self.mutate(session_id, expected_revision, |session| {
            require_running(session)?;
            if attempt.phase != session.phase {
                return Err(SessionError::InvalidAttempt);
            }
            if session
                .attempts
                .iter()
                .any(|current| current.attempt_id == attempt.attempt_id)
            {
                return Err(SessionError::InvalidAttempt);
            }
            if session.attempts.len() >= self.store.limits().max_attempts {
                return Err(SessionError::LimitExceeded {
                    resource: "attempts".to_string(),
                    limit: self.store.limits().max_attempts,
                    actual: session.attempts.len() + 1,
                });
            }
            let kind = match attempt.outcome {
                AttemptOutcome::Running => TraceEventKind::AttemptStarted,
                AttemptOutcome::Succeeded => TraceEventKind::AttemptCompleted,
                AttemptOutcome::Failed => TraceEventKind::AttemptFailed,
                AttemptOutcome::Interrupted => TraceEventKind::AttemptInterrupted,
                AttemptOutcome::Cancelled => TraceEventKind::AttemptCancelled,
                AttemptOutcome::Rejected => TraceEventKind::AttemptRejected,
                AttemptOutcome::Exhausted => TraceEventKind::AttemptFailed,
            };
            let summary = format!("attempt {} recorded", attempt.outcome_string());
            let attempt_id = attempt.attempt_id.clone();
            let artifact_refs = attempt.artifact_refs.clone();
            let evidence_refs = attempt.evidence_refs.clone();
            session.attempts.push(attempt);
            session.record_trace(
                kind,
                summary,
                Some(attempt_id),
                artifact_refs,
                evidence_refs,
                None,
                false,
            )?;
            Ok(())
        })
    }

    pub(crate) fn finish_attempt(
        &self,
        session_id: impl AsRef<str>,
        expected_revision: u64,
        attempt_id: AttemptId,
        outcome: AttemptOutcome,
        budget_delta: BudgetUsage,
        artifact_refs: Vec<super::model::ArtifactId>,
        evidence_refs: Vec<EvidenceId>,
        safe_error_code: Option<String>,
    ) -> Result<AgentSession, SessionError> {
        self.mutate_preserving_in_flight(session_id, expected_revision, |session| {
            {
                let Some(attempt) = session
                    .attempts
                    .iter_mut()
                    .find(|attempt| attempt.attempt_id == attempt_id)
                else {
                    return Err(SessionError::AttemptNotFound {
                        id: attempt_id.to_string(),
                    });
                };
                attempt.finish(
                    outcome,
                    budget_delta,
                    artifact_refs.clone(),
                    evidence_refs.clone(),
                    safe_error_code,
                )?;
            }
            let kind = match outcome {
                AttemptOutcome::Succeeded => TraceEventKind::AttemptCompleted,
                AttemptOutcome::Failed | AttemptOutcome::Exhausted => TraceEventKind::AttemptFailed,
                AttemptOutcome::Interrupted => TraceEventKind::AttemptInterrupted,
                AttemptOutcome::Cancelled => TraceEventKind::AttemptCancelled,
                AttemptOutcome::Rejected => TraceEventKind::AttemptRejected,
                AttemptOutcome::Running => TraceEventKind::AttemptStarted,
            };
            session.record_trace(
                kind,
                format!("attempt {outcome:?}"),
                Some(attempt_id),
                artifact_refs,
                evidence_refs,
                None,
                false,
            )?;
            Ok(())
        })
    }

    pub(crate) fn store_artifact(
        &self,
        session_id: impl AsRef<str>,
        expected_revision: u64,
        header: ArtifactHeader,
        payload: Value,
    ) -> Result<(AgentSession, ArtifactRecord), SessionError> {
        let artifact = self.artifacts.put_json(header, payload)?;
        let bytes = artifact.header.byte_size;
        let session = self.append_artifact_reference_preserving_in_flight(
            session_id.as_ref(),
            expected_revision,
            artifact.header.artifact_id.clone(),
        )?;
        let (session, _) = self.consume_budget_preserving_in_flight(
            session.session_id.as_str(),
            session.revision,
            BudgetCost::for_artifact(bytes),
        )?;
        Ok((session, artifact))
    }

    pub(crate) fn create_artifact<T: serde::Serialize>(
        &self,
        session_id: impl AsRef<str>,
        expected_revision: u64,
        kind: super::artifact::ArtifactKind,
        phase: AgentPhase,
        attempt_id: Option<AttemptId>,
        payload: &T,
        options: ArtifactWriteOptions,
    ) -> Result<(AgentSession, ArtifactRecord), SessionError> {
        let artifact = self
            .artifacts
            .create(kind, phase, attempt_id, payload, options)?;
        let session = self.append_artifact_reference_preserving_in_flight(
            session_id,
            expected_revision,
            artifact.header.artifact_id.clone(),
        )?;
        let (session, _) = self.consume_budget_preserving_in_flight(
            session.session_id.as_str(),
            session.revision,
            BudgetCost::for_artifact(artifact.header.byte_size),
        )?;
        Ok((session, artifact))
    }

    pub(crate) fn store_evidence(
        &self,
        session_id: impl AsRef<str>,
        expected_revision: u64,
        record: EvidenceRecord,
    ) -> Result<(AgentSession, EvidenceRecord), SessionError> {
        let evidence = self.evidence.append(record)?;
        let bytes = serde_json::to_vec(&evidence)
            .map_err(|_| SessionError::serialization("encode evidence charge"))?
            .len() as u64;
        let session = self.append_evidence_reference_preserving_in_flight(
            session_id.as_ref(),
            expected_revision,
            evidence.evidence_id.clone(),
        )?;
        let (session, _) = self.consume_budget_preserving_in_flight(
            session.session_id.as_str(),
            session.revision,
            BudgetCost::for_evidence(bytes),
        )?;
        Ok((session, evidence))
    }

    pub(crate) fn progress(
        &self,
        session_id: impl AsRef<str>,
        expected_revision: u64,
        progress: TraceProgress,
        summary: impl Into<String>,
    ) -> Result<AgentSession, SessionError> {
        self.mutate(session_id, expected_revision, |session| {
            require_running(session)?;
            session.record_trace(
                TraceEventKind::PhaseChanged,
                summary,
                None,
                Vec::new(),
                Vec::new(),
                Some(progress),
                false,
            )?;
            Ok(())
        })
    }

    fn mutate<F>(
        &self,
        session_id: impl AsRef<str>,
        expected_revision: u64,
        mutation: F,
    ) -> Result<AgentSession, SessionError>
    where
        F: FnOnce(&mut AgentSession) -> Result<(), SessionError>,
    {
        self.mutate_with_recovery(session_id, expected_revision, true, mutation)
    }

    fn mutate_preserving_in_flight<F>(
        &self,
        session_id: impl AsRef<str>,
        expected_revision: u64,
        mutation: F,
    ) -> Result<AgentSession, SessionError>
    where
        F: FnOnce(&mut AgentSession) -> Result<(), SessionError>,
    {
        self.mutate_with_recovery(session_id, expected_revision, false, mutation)
    }

    fn mutate_with_recovery<F>(
        &self,
        session_id: impl AsRef<str>,
        expected_revision: u64,
        recover_running_attempts: bool,
        mutation: F,
    ) -> Result<AgentSession, SessionError>
    where
        F: FnOnce(&mut AgentSession) -> Result<(), SessionError>,
    {
        let updated = if recover_running_attempts {
            self.store
                .mutate_blocking(session_id, expected_revision, mutation)?
        } else {
            self.store
                .mutate_preserving_in_flight(session_id, expected_revision, mutation)?
        };
        self.emit_last_event(&updated);
        Ok(updated)
    }

    fn emit_last_event(&self, session: &AgentSession) {
        if let Some(sink) = &self.event_sink
            && let Some(event) = session.trace.last()
        {
            sink(event);
        }
    }
}

fn transition_status(
    session: &mut AgentSession,
    next: AgentSessionStatus,
) -> Result<(), SessionError> {
    if !session.status.can_transition_to(next) {
        return Err(SessionError::InvalidStatusTransition {
            from: session.status,
            to: next,
        });
    }
    session.status = next;
    Ok(())
}

fn require_running(session: &AgentSession) -> Result<(), SessionError> {
    if session.is_terminal() {
        return Err(SessionError::SessionTerminal);
    }
    if session.status != AgentSessionStatus::Running {
        return Err(SessionError::InvalidStatusTransition {
            from: session.status,
            to: AgentSessionStatus::Running,
        });
    }
    Ok(())
}

fn interrupt_in_flight(session: &mut AgentSession) -> Result<(), SessionError> {
    let in_flight = session
        .attempts
        .iter()
        .filter(|attempt| attempt.is_in_flight())
        .map(|attempt| attempt.attempt_id.clone())
        .collect::<Vec<_>>();
    for attempt_id in in_flight {
        let (artifact_refs, evidence_refs) = {
            let Some(attempt) = session
                .attempts
                .iter_mut()
                .find(|attempt| attempt.attempt_id == attempt_id)
            else {
                continue;
            };
            let artifact_refs = attempt.artifact_refs.clone();
            let evidence_refs = attempt.evidence_refs.clone();
            attempt.finish(
                AttemptOutcome::Interrupted,
                attempt.budget_delta,
                artifact_refs.clone(),
                evidence_refs.clone(),
                Some("interrupted".to_string()),
            )?;
            (artifact_refs, evidence_refs)
        };
        session.record_trace(
            TraceEventKind::AttemptInterrupted,
            "in-flight attempt interrupted",
            Some(attempt_id),
            artifact_refs,
            evidence_refs,
            None,
            false,
        )?;
    }
    Ok(())
}
