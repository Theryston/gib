use super::context::{ContextBuildResult, ContextBuilder, ContextInputs, ContextRole};
use super::error::OrchestratorError;
use super::model::{
    AllowedSideEffects, BudgetClass, MAX_NO_PROGRESS_STREAK, ORCHESTRATOR_EVENT_SCHEMA_VERSION,
    OrchestratorEvent, OrchestratorEventKind, OrchestratorStep, OrchestratorStepSummary,
    PhaseDefinition, PhaseId, PhaseRequest, PhaseResult, PhaseStatus, ProgressSignal,
    WorkflowDefinition, WorkflowId, WorkflowState,
};
use super::workflow::WorkflowRegistry;
use crate::ai::session::{
    AgentPhase, AgentSession, AgentSessionId, AgentSessionStatus, AttemptOutcome, BudgetCost,
    BudgetUsage, SessionError, SessionService, SessionSpec, StopReason, TraceEventId,
    canonical_fingerprint, hash_bytes, redact_text,
};
use serde::{Deserialize, Serialize};
use serde_json::json;
use std::collections::BTreeMap;
use std::fs::{self, File, OpenOptions};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

const ORCHESTRATOR_DIRECTORY_NAME: &str = "orchestrator";
const MAX_STATE_FILE_BYTES: usize = 8 * 1024 * 1024;
static TEMP_SEQUENCE: AtomicU64 = AtomicU64::new(0);

pub(crate) type OrchestratorEventSink = Arc<dyn Fn(&OrchestratorEvent) + Send + Sync>;

/// A phase operation receives only a typed, bounded request. It cannot alter
/// the workflow graph or invoke undeclared tools through this interface.
pub(crate) trait PhaseExecutor {
    fn execute(&self, request: &PhaseRequest) -> Result<PhaseResult, PhaseExecutionError>;
}

impl<F> PhaseExecutor for F
where
    F: Fn(&PhaseRequest) -> Result<PhaseResult, PhaseExecutionError>,
{
    fn execute(&self, request: &PhaseRequest) -> Result<PhaseResult, PhaseExecutionError> {
        self(request)
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub(crate) struct PhaseExecutionError {
    pub(crate) error_code: String,
    pub(crate) retryable: bool,
    pub(crate) evidence_limitation: bool,
}

impl PhaseExecutionError {
    pub(crate) fn new(error_code: impl Into<String>, retryable: bool) -> Self {
        Self {
            error_code: safe_error_code(&error_code.into()),
            retryable,
            evidence_limitation: false,
        }
    }

    pub(crate) fn evidence_limitation(mut self) -> Self {
        self.evidence_limitation = true;
        self
    }
}

/// A deterministic fake executor is useful for tests and for callers that
/// need a bounded placeholder while the real catalog operation is wired in.
#[derive(Debug, Clone, Default)]
pub(crate) struct ScriptedPhaseExecutor {
    results: Arc<Mutex<BTreeMap<PhaseId, Vec<PhaseResult>>>>,
}

impl ScriptedPhaseExecutor {
    pub(crate) fn new() -> Self {
        Self::default()
    }

    pub(crate) fn push_result(&self, result: PhaseResult) {
        if let Ok(mut results) = self.results.lock() {
            results.entry(result.phase_id).or_default().push(result);
        }
    }
}

impl PhaseExecutor for ScriptedPhaseExecutor {
    fn execute(&self, request: &PhaseRequest) -> Result<PhaseResult, PhaseExecutionError> {
        let mut results = self
            .results
            .lock()
            .map_err(|_| PhaseExecutionError::new("executor_unavailable", false))?;
        if let Some(queue) = results.get_mut(&request.phase.phase_id)
            && !queue.is_empty()
        {
            return Ok(queue.remove(0));
        }
        Ok(PhaseResult::succeeded(
            request.phase.phase_id,
            format!("{} phase completed", request.phase.phase_id),
            Some(ProgressSignal::NewVerifiedFact),
        ))
    }
}

#[derive(Clone)]
pub(crate) struct OrchestratorService {
    registry: WorkflowRegistry,
    sessions: SessionService,
    context_builder: ContextBuilder,
    state_root: PathBuf,
    event_sink: Option<OrchestratorEventSink>,
}

impl std::fmt::Debug for OrchestratorService {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("OrchestratorService")
            .field("registry", &self.registry)
            .field("sessions", &self.sessions)
            .field("context_builder", &self.context_builder)
            .field("state_root", &self.state_root)
            .field("event_sink_configured", &self.event_sink.is_some())
            .finish()
    }
}

impl OrchestratorService {
    pub(crate) fn new(
        registry: WorkflowRegistry,
        sessions: SessionService,
        context_builder: ContextBuilder,
    ) -> Self {
        let state_root = sessions.store().paths().root().to_path_buf();
        Self {
            registry,
            sessions,
            context_builder,
            state_root,
            event_sink: None,
        }
    }

    pub(crate) fn with_default_registry(
        sessions: SessionService,
        context_builder: ContextBuilder,
    ) -> Result<Self, OrchestratorError> {
        Ok(Self::new(
            WorkflowRegistry::with_builtins()?,
            sessions,
            context_builder,
        ))
    }

    pub(crate) fn with_event_sink(mut self, event_sink: OrchestratorEventSink) -> Self {
        self.event_sink = Some(event_sink);
        self
    }

    pub(crate) fn registry(&self) -> &WorkflowRegistry {
        &self.registry
    }

    pub(crate) fn sessions(&self) -> &SessionService {
        &self.sessions
    }

    pub(crate) fn context_builder(&self) -> &ContextBuilder {
        &self.context_builder
    }

    /// Resolve and persist the workflow/session identity before any phase can
    /// run. The raw request remains outside this persisted contract.
    pub(crate) fn start(
        &self,
        spec: SessionSpec,
        intent: super::model::IntentKind,
        capabilities: Vec<String>,
    ) -> Result<AgentSession, OrchestratorError> {
        let workflow_id = WorkflowId::from_string(spec.workflow_id.clone())?;
        let workflow = self
            .registry
            .require(&workflow_id, &spec.workflow_version)?;
        intent.validate()?;
        if !workflow.supports_intent(&intent) {
            return Err(OrchestratorError::UnsupportedIntent {
                workflow: workflow_id.to_string(),
                intent: intent.to_string(),
            });
        }
        let available = capabilities
            .into_iter()
            .collect::<std::collections::BTreeSet<_>>();
        for capability in &available {
            if !is_safe_identifier(capability) {
                return Err(OrchestratorError::InvalidIdentifier {
                    kind: "capability".to_string(),
                });
            }
        }
        for required in &workflow.required_capabilities {
            if !available.contains(required) {
                return Err(OrchestratorError::MissingCapability {
                    capability: required.clone(),
                });
            }
        }
        let session = self.sessions.create(spec)?;
        let mut state = workflow.initial_state(session.session_id.clone());
        workflow.refresh_ready(&mut state);
        self.emit(
            &mut state,
            &session,
            OrchestratorEventKind::Started,
            session.phase,
            PhaseStatus::Pending,
            None,
            "agent orchestration started",
            None,
            Vec::new(),
            Vec::new(),
            false,
            Some(session.status),
            None,
            false,
        )?;
        self.emit_ready_events(&mut state, &session)?;
        self.persist_state(&state)?;
        Ok(session)
    }

    pub(crate) fn load_state(
        &self,
        session_id: &AgentSessionId,
    ) -> Result<WorkflowState, OrchestratorError> {
        let mut session = self.sessions.load(session_id.as_str())?;
        let workflow_id = WorkflowId::from_string(session.workflow_id.clone())?;
        let workflow = self
            .registry
            .require(&workflow_id, &session.workflow_version)?;
        let state = match self.read_state(session_id)? {
            Some(state) => state,
            None => self.rebuild_state(&session, workflow),
        };
        if state.session_id != *session_id {
            return Err(OrchestratorError::StateConflict);
        }
        let mut state = state;
        let before = state.clone();
        self.reconcile_state(&mut session, &mut state, workflow)?;
        if state != before {
            self.persist_state(&state)?;
        }
        Ok(state)
    }

    pub(crate) fn events(
        &self,
        session_id: &AgentSessionId,
    ) -> Result<Vec<OrchestratorEvent>, OrchestratorError> {
        Ok(self.load_state(session_id)?.events)
    }

    pub(crate) fn phase_status(
        &self,
        session_id: &AgentSessionId,
        phase: PhaseId,
    ) -> Result<PhaseStatus, OrchestratorError> {
        self.load_state(session_id)?
            .phase(phase)
            .map(|state| state.status)
            .ok_or_else(|| OrchestratorError::InvalidWorkflow {
                reason: "phase is not part of the session workflow".to_string(),
            })
    }

    pub(crate) fn ready_phases(
        &self,
        session_id: &AgentSessionId,
    ) -> Result<Vec<PhaseId>, OrchestratorError> {
        let session = self.sessions.load(session_id.as_str())?;
        let workflow_id = WorkflowId::from_string(session.workflow_id.clone())?;
        let workflow = self
            .registry
            .require(&workflow_id, &session.workflow_version)?;
        let mut state = self.load_state(session_id)?;
        workflow.refresh_ready(&mut state);
        Ok(workflow.ready_phases(&state))
    }

    /// Execute one and only one ready phase, or return a terminal/waiting
    /// snapshot. Repeated calls are safe at phase boundaries.
    pub(crate) fn step(
        &self,
        session_id: &AgentSessionId,
        mut inputs: ContextInputs,
        executor: &dyn PhaseExecutor,
    ) -> Result<OrchestratorStep, OrchestratorError> {
        let mut session = self.sessions.load(session_id.as_str())?;
        let workflow_id = WorkflowId::from_string(session.workflow_id.clone())?;
        let workflow = self
            .registry
            .require(&workflow_id, &session.workflow_version)?
            .clone();
        let mut state = self.load_or_rebuild_state(&session, &workflow)?;
        self.reconcile_state(&mut session, &mut state, &workflow)?;
        self.validate_references(&session, &state)?;

        if session.status.is_terminal() {
            self.persist_state(&state)?;
            return Ok(terminal_step(&session, session.phase, &state));
        }
        if session.status == AgentSessionStatus::WaitingForUser {
            self.persist_state(&state)?;
            return Ok(OrchestratorStep {
                session,
                phase: state
                    .phase_states
                    .iter()
                    .find(|phase| phase.status == PhaseStatus::Waiting)
                    .map(|phase| phase.phase_id)
                    .unwrap_or(AgentPhase::Classify),
                phase_status: PhaseStatus::Waiting,
                result: None,
                context: None,
                next_phase: None,
                summary: OrchestratorStepSummary {
                    terminal: false,
                    waiting: true,
                    progress: false,
                },
            });
        }

        workflow.refresh_ready(&mut state);
        self.emit_ready_events(&mut state, &session)?;
        let current_phase = session.phase;
        let definition = workflow
            .phase(current_phase)
            .cloned()
            .ok_or_else(|| OrchestratorError::StateConflict)?;
        let phase_state = state
            .phase(current_phase)
            .cloned()
            .ok_or_else(|| OrchestratorError::StateConflict)?;

        if phase_state.status.is_dependency_failure() {
            return self.fail_dependency(&mut session, &mut state, &workflow, current_phase);
        }
        if matches!(
            phase_state.status,
            PhaseStatus::Succeeded | PhaseStatus::Skipped
        ) {
            return self.advance_after_completed(
                &mut session,
                &mut state,
                &workflow,
                current_phase,
                None,
                false,
                None,
            );
        }
        if phase_state.status != PhaseStatus::Ready {
            return Err(OrchestratorError::PhaseNotReady {
                phase: current_phase.to_string(),
            });
        }
        if state.active_attempt.is_some()
            || session
                .attempts
                .iter()
                .any(|attempt| attempt.is_in_flight())
        {
            return Err(OrchestratorError::ActiveAttempt);
        }
        if definition.allowed_side_effects == AllowedSideEffects::Commit
            || current_phase == AgentPhase::Commit
        {
            return Err(OrchestratorError::UnsafeSideEffect {
                phase: current_phase.to_string(),
            });
        }
        if definition.allowed_side_effects == AllowedSideEffects::ConfirmationRequired
            && current_phase != AgentPhase::Confirm
        {
            return Err(OrchestratorError::UnsafeSideEffect {
                phase: current_phase.to_string(),
            });
        }
        self.validate_input_artifacts(&session, &definition)?;

        inputs.session = Some(session.clone());
        self.enrich_inputs(&mut inputs, &session)?;
        let role = role_for_phase(current_phase);
        let context = self.context_builder.build(role, &inputs)?;
        context.validate(self.context_builder.limits())?;
        let budget_cost =
            phase_budget_cost(definition.budget_class, &context, phase_state.attempt_count);
        let (mut session, _) = match self.sessions.consume_budget(
            session.session_id.as_str(),
            session.revision,
            budget_cost,
        ) {
            Ok(value) => value,
            Err(SessionError::Budget(_)) => {
                let current = self.sessions.load(session_id.as_str())?;
                if !current.is_terminal() {
                    let _ = self
                        .sessions
                        .mark_budget_exhausted(current.session_id.as_str(), current.revision);
                }
                session = self.sessions.load(session_id.as_str())?;
                if let Some(state) = state.phase_mut(current_phase) {
                    state.status = PhaseStatus::Exhausted;
                    state.last_error_code = Some("budget_exhausted".to_string());
                }
                state.active_attempt = None;
                self.emit(
                    &mut state,
                    &session,
                    OrchestratorEventKind::PhaseExhausted,
                    current_phase,
                    PhaseStatus::Exhausted,
                    None,
                    "phase budget exhausted",
                    None,
                    Vec::new(),
                    Vec::new(),
                    true,
                    Some(AgentSessionStatus::BudgetExhausted),
                    Some("budget_exhausted".to_string()),
                    false,
                )?;
                self.persist_state(&state)?;
                return Ok(OrchestratorStep {
                    session,
                    phase: current_phase,
                    phase_status: PhaseStatus::Exhausted,
                    result: Some(PhaseResult {
                        phase_id: current_phase,
                        status: PhaseStatus::Exhausted,
                        summary: "phase budget exhausted".to_string(),
                        artifact_refs: Vec::new(),
                        evidence_refs: Vec::new(),
                        progress: None,
                        stop_reason: Some(StopReason::BudgetExhausted),
                        error_code: Some("budget_exhausted".to_string()),
                        retryable: false,
                        idempotent: true,
                    }),
                    context: Some(context),
                    next_phase: None,
                    summary: OrchestratorStepSummary {
                        terminal: true,
                        waiting: false,
                        progress: false,
                    },
                });
            }
            Err(error) => return Err(error.into()),
        };

        let attempt_arguments = json!({
            "workflow_id": workflow.workflow_id,
            "workflow_version": workflow.version,
            "phase": current_phase,
        });
        let action_type = format!("phase.{}", current_phase.as_str());
        let action_fingerprint = canonical_fingerprint(&action_type, &attempt_arguments);
        if definition.allowed_side_effects.is_side_effecting()
            && session.attempts.iter().any(|attempt| {
                attempt.canonical_fingerprint == action_fingerprint
                    && attempt.outcome == AttemptOutcome::Interrupted
            })
        {
            return Err(OrchestratorError::SideEffectReplayBlocked {
                phase: current_phase.to_string(),
            });
        }
        let (session_after_attempt, attempt_id) = self.sessions.begin_attempt(
            session.session_id.as_str(),
            session.revision,
            action_type,
            &attempt_arguments,
        )?;
        session = session_after_attempt;
        if let Some(state) = state.phase_mut(current_phase) {
            state.status = PhaseStatus::Running;
            state.attempt_count = state.attempt_count.saturating_add(1);
        }
        state.active_attempt = Some(attempt_id.clone());
        self.persist_state(&state)?;
        self.emit(
            &mut state,
            &session,
            OrchestratorEventKind::PhaseStarted,
            current_phase,
            PhaseStatus::Running,
            Some(attempt_id.clone()),
            "phase started",
            None,
            Vec::new(),
            Vec::new(),
            false,
            Some(AgentSessionStatus::Running),
            None,
            false,
        )?;
        self.persist_state(&state)?;
        let phase_state = state
            .phase(current_phase)
            .cloned()
            .ok_or(OrchestratorError::StateConflict)?;
        let request = PhaseRequest {
            session: session.clone(),
            workflow: workflow.clone(),
            phase: definition.clone(),
            phase_state,
            attempt_id: attempt_id.clone(),
            context: context.clone(),
        };
        let execution = executor.execute(&request);
        let budget_delta = budget_usage(budget_cost);
        if matches!(
            session.budget.consume(BudgetCost::default()),
            Err(crate::ai::session::BudgetError::DeadlineExceeded)
        ) {
            return self.finish_deadline_exhausted(
                session,
                state,
                current_phase,
                attempt_id,
                budget_delta,
                context,
            );
        }
        let result = match execution {
            Ok(result) => result,
            Err(error) => {
                return self.finish_execution_error(
                    session,
                    state,
                    &workflow,
                    current_phase,
                    attempt_id,
                    budget_delta,
                    error,
                    context,
                );
            }
        };

        if validate_result(&result, &definition).is_err() {
            let rejected = PhaseExecutionError::new("invalid_phase_output", false);
            return self.finish_rejected(
                session,
                state,
                &workflow,
                current_phase,
                attempt_id,
                budget_delta,
                rejected,
                context,
            );
        }
        if let Err(error) = self.validate_output_references(&result, &definition) {
            let code = error.code().to_string();
            return self.finish_rejected(
                session,
                state,
                &workflow,
                current_phase,
                attempt_id,
                budget_delta,
                PhaseExecutionError::new(code, false),
                context,
            );
        }
        let (session, state) =
            self.commit_output_references(session, state, current_phase, &result)?;
        self.finish_valid_result(
            session,
            state,
            workflow,
            definition,
            attempt_id,
            budget_delta,
            result,
            context,
        )
    }

    pub(crate) fn run_to_completion(
        &self,
        session_id: &AgentSessionId,
        inputs: ContextInputs,
        executor: &dyn PhaseExecutor,
    ) -> Result<AgentSession, OrchestratorError> {
        let session = self.sessions.load(session_id.as_str())?;
        let workflow_id = WorkflowId::from_string(session.workflow_id.clone())?;
        let workflow = self
            .registry
            .require(&workflow_id, &session.workflow_version)?;
        let max_steps = workflow.phases.len().saturating_mul(16).max(1);
        for _ in 0..max_steps {
            let step = self.step(session_id, inputs.clone(), executor)?;
            if step.summary.terminal || step.summary.waiting {
                return Ok(step.session);
            }
        }
        let current = self.sessions.load(session_id.as_str())?;
        if current.is_terminal() {
            Ok(current)
        } else {
            let failed = self.sessions.fail(
                current.session_id.as_str(),
                current.revision,
                "orchestrator_step_limit",
            )?;
            Ok(failed)
        }
    }

    pub(crate) fn resume(
        &self,
        session_id: &AgentSessionId,
        expected_revision: u64,
    ) -> Result<AgentSession, OrchestratorError> {
        let mut session = self
            .sessions
            .resume(session_id.as_str(), expected_revision)?;
        let workflow_id = WorkflowId::from_string(session.workflow_id.clone())?;
        let workflow = self
            .registry
            .require(&workflow_id, &session.workflow_version)?;
        let mut state = self.load_or_rebuild_state(&session, workflow)?;
        self.reconcile_state(&mut session, &mut state, workflow)?;
        for phase in &mut state.phase_states {
            if phase.status == PhaseStatus::Waiting {
                phase.status = PhaseStatus::Ready;
            }
        }
        state.active_attempt = None;
        workflow.refresh_ready(&mut state);
        state.validate(workflow)?;
        let status = state
            .phase(session.phase)
            .map(|phase| phase.status)
            .unwrap_or(PhaseStatus::Ready);
        self.emit(
            &mut state,
            &session,
            OrchestratorEventKind::Resumed,
            session.phase,
            status,
            None,
            "orchestration resumed",
            None,
            Vec::new(),
            Vec::new(),
            false,
            Some(AgentSessionStatus::Running),
            None,
            false,
        )?;
        self.persist_state(&state)?;
        Ok(session)
    }

    pub(crate) fn cancel(
        &self,
        session_id: &AgentSessionId,
        expected_revision: u64,
    ) -> Result<AgentSession, OrchestratorError> {
        let mut state = self.load_state(session_id)?;
        let session = self
            .sessions
            .cancel(session_id.as_str(), expected_revision)?;
        if let Some(phase) = state.phase_mut(session.phase) {
            if !phase.status.is_terminal() {
                phase.status = PhaseStatus::Cancelled;
            }
        }
        state.active_attempt = None;
        let status = state
            .phase(session.phase)
            .map(|phase| phase.status)
            .unwrap_or(PhaseStatus::Cancelled);
        self.emit(
            &mut state,
            &session,
            OrchestratorEventKind::SessionCancelled,
            session.phase,
            status,
            None,
            "orchestration cancelled",
            None,
            Vec::new(),
            Vec::new(),
            true,
            Some(AgentSessionStatus::Cancelled),
            None,
            false,
        )?;
        self.persist_state(&state)?;
        Ok(session)
    }

    #[cfg(test)]
    pub(crate) fn persist_state_for_test(
        &self,
        state: &WorkflowState,
    ) -> Result<(), OrchestratorError> {
        self.persist_state(state)
    }

    fn finish_execution_error(
        &self,
        mut session: AgentSession,
        mut state: WorkflowState,
        workflow: &WorkflowDefinition,
        phase: PhaseId,
        attempt_id: crate::ai::session::AttemptId,
        budget_delta: BudgetUsage,
        error: PhaseExecutionError,
        context: ContextBuildResult,
    ) -> Result<OrchestratorStep, OrchestratorError> {
        let error_code = error.error_code.clone();
        let event_attempt_id = attempt_id.clone();
        session = self.sessions.finish_attempt(
            session.session_id.as_str(),
            session.revision,
            attempt_id.clone(),
            AttemptOutcome::Failed,
            budget_delta,
            Vec::new(),
            Vec::new(),
            Some(error_code.clone()),
        )?;
        state.active_attempt = None;
        let phase_state = state
            .phase_mut(phase)
            .ok_or(OrchestratorError::StateConflict)?;
        phase_state.last_error_code = Some(error_code.clone());
        let retry_limit = session.budget.limits().max_retries.saturating_add(1);
        if error.retryable && u64::from(phase_state.attempt_count) < retry_limit {
            phase_state.status = PhaseStatus::Ready;
            let retry_error_code = error_code.clone();
            self.emit(
                &mut state,
                &session,
                OrchestratorEventKind::PhaseFailed,
                phase,
                PhaseStatus::Ready,
                Some(event_attempt_id.clone()),
                "phase failed and may be retried",
                None,
                Vec::new(),
                Vec::new(),
                false,
                Some(AgentSessionStatus::Running),
                Some(error_code),
                error.evidence_limitation,
            )?;
            self.persist_state(&state)?;
            return Ok(OrchestratorStep {
                session,
                phase,
                phase_status: PhaseStatus::Ready,
                result: Some(PhaseResult::failed(
                    phase,
                    "phase failed and may be retried",
                    retry_error_code,
                    true,
                )),
                context: Some(context),
                next_phase: None,
                summary: OrchestratorStepSummary {
                    terminal: false,
                    waiting: false,
                    progress: false,
                },
            });
        }
        phase_state.status = PhaseStatus::Failed;
        let failed_dependents = mark_failed_dependents(workflow, &mut state, phase);
        self.emit_dependency_failures(&mut state, &session, failed_dependents)?;
        let session = self.sessions.fail(
            session.session_id.as_str(),
            session.revision,
            error_code.clone(),
        )?;
        self.emit(
            &mut state,
            &session,
            OrchestratorEventKind::PhaseFailed,
            phase,
            PhaseStatus::Failed,
            Some(event_attempt_id),
            "phase failed",
            None,
            Vec::new(),
            Vec::new(),
            true,
            Some(AgentSessionStatus::Failed),
            Some(error_code.clone()),
            error.evidence_limitation,
        )?;
        self.persist_state(&state)?;
        Ok(OrchestratorStep {
            session,
            phase,
            phase_status: PhaseStatus::Failed,
            result: Some(PhaseResult::failed(
                phase,
                "phase failed",
                error_code,
                false,
            )),
            context: Some(context),
            next_phase: None,
            summary: OrchestratorStepSummary {
                terminal: true,
                waiting: false,
                progress: false,
            },
        })
    }

    fn finish_deadline_exhausted(
        &self,
        mut session: AgentSession,
        mut state: WorkflowState,
        phase: PhaseId,
        attempt_id: crate::ai::session::AttemptId,
        budget_delta: BudgetUsage,
        context: ContextBuildResult,
    ) -> Result<OrchestratorStep, OrchestratorError> {
        session = self.sessions.finish_attempt(
            session.session_id.as_str(),
            session.revision,
            attempt_id.clone(),
            AttemptOutcome::Exhausted,
            budget_delta,
            Vec::new(),
            Vec::new(),
            Some("budget_deadline_exceeded".to_string()),
        )?;
        state.active_attempt = None;
        if let Some(phase_state) = state.phase_mut(phase) {
            phase_state.status = PhaseStatus::Exhausted;
            phase_state.last_error_code = Some("budget_deadline_exceeded".to_string());
        }
        session = self
            .sessions
            .mark_budget_exhausted(session.session_id.as_str(), session.revision)?;
        self.emit(
            &mut state,
            &session,
            OrchestratorEventKind::PhaseExhausted,
            phase,
            PhaseStatus::Exhausted,
            Some(attempt_id),
            "phase deadline exhausted during execution",
            None,
            Vec::new(),
            Vec::new(),
            true,
            Some(AgentSessionStatus::BudgetExhausted),
            Some("budget_deadline_exceeded".to_string()),
            false,
        )?;
        self.persist_state(&state)?;
        Ok(OrchestratorStep {
            session,
            phase,
            phase_status: PhaseStatus::Exhausted,
            result: Some(PhaseResult::exhausted(
                phase,
                "phase deadline exhausted during execution",
            )),
            context: Some(context),
            next_phase: None,
            summary: OrchestratorStepSummary {
                terminal: true,
                waiting: false,
                progress: false,
            },
        })
    }

    fn finish_rejected(
        &self,
        mut session: AgentSession,
        mut state: WorkflowState,
        workflow: &WorkflowDefinition,
        phase: PhaseId,
        attempt_id: crate::ai::session::AttemptId,
        budget_delta: BudgetUsage,
        error: PhaseExecutionError,
        context: ContextBuildResult,
    ) -> Result<OrchestratorStep, OrchestratorError> {
        let error_code = error.error_code.clone();
        let event_attempt_id = attempt_id.clone();
        session = self.sessions.finish_attempt(
            session.session_id.as_str(),
            session.revision,
            attempt_id,
            AttemptOutcome::Rejected,
            budget_delta,
            Vec::new(),
            Vec::new(),
            Some(error_code.clone()),
        )?;
        state.active_attempt = None;
        if let Some(phase_state) = state.phase_mut(phase) {
            phase_state.status = PhaseStatus::Failed;
            phase_state.last_error_code = Some(error_code.clone());
        }
        let failed_dependents = mark_failed_dependents(workflow, &mut state, phase);
        self.emit_dependency_failures(&mut state, &session, failed_dependents)?;
        session = self.sessions.fail(
            session.session_id.as_str(),
            session.revision,
            error_code.clone(),
        )?;
        self.emit(
            &mut state,
            &session,
            OrchestratorEventKind::PhaseFailed,
            phase,
            PhaseStatus::Failed,
            Some(event_attempt_id),
            "phase output was rejected",
            None,
            Vec::new(),
            Vec::new(),
            true,
            Some(AgentSessionStatus::Failed),
            Some(error_code.clone()),
            error.evidence_limitation,
        )?;
        self.persist_state(&state)?;
        Ok(OrchestratorStep {
            session,
            phase,
            phase_status: PhaseStatus::Failed,
            result: Some(PhaseResult::failed(
                phase,
                "phase output was rejected",
                error_code,
                false,
            )),
            context: Some(context),
            next_phase: None,
            summary: OrchestratorStepSummary {
                terminal: true,
                waiting: false,
                progress: false,
            },
        })
    }

    fn finish_valid_result(
        &self,
        mut session: AgentSession,
        mut state: WorkflowState,
        workflow: WorkflowDefinition,
        definition: PhaseDefinition,
        attempt_id: crate::ai::session::AttemptId,
        budget_delta: BudgetUsage,
        result: PhaseResult,
        context: ContextBuildResult,
    ) -> Result<OrchestratorStep, OrchestratorError> {
        let phase = definition.phase_id;
        let attempt_outcome = match result.status {
            PhaseStatus::Succeeded | PhaseStatus::Skipped | PhaseStatus::Waiting => {
                AttemptOutcome::Succeeded
            }
            PhaseStatus::Failed => AttemptOutcome::Failed,
            PhaseStatus::Cancelled => AttemptOutcome::Cancelled,
            PhaseStatus::Exhausted => AttemptOutcome::Exhausted,
            _ => {
                return Err(OrchestratorError::InvalidPhaseOutput {
                    reason: "phase result is not executable".to_string(),
                });
            }
        };
        session = self.sessions.finish_attempt(
            session.session_id.as_str(),
            session.revision,
            attempt_id.clone(),
            attempt_outcome,
            budget_delta,
            result.artifact_refs.clone(),
            result.evidence_refs.clone(),
            result.error_code.clone(),
        )?;
        state.active_attempt = None;
        if let Some(phase_state) = state.phase_mut(phase) {
            phase_state.status = result.status;
            phase_state.artifact_refs = result.artifact_refs.clone();
            phase_state.evidence_refs = result.evidence_refs.clone();
            phase_state.last_error_code = result.error_code.clone();
        }

        match result.status {
            PhaseStatus::Succeeded | PhaseStatus::Skipped => {
                let progress_fingerprint = super::model::progress_fingerprint(phase, &result);
                let progressed = match progress_fingerprint {
                    Some(fingerprint)
                        if state.last_progress_fingerprint.as_deref() != Some(&fingerprint) =>
                    {
                        state.last_progress_fingerprint = Some(fingerprint);
                        state.no_progress_streak = 0;
                        true
                    }
                    _ => {
                        state.no_progress_streak = state.no_progress_streak.saturating_add(1);
                        false
                    }
                };
                if state.no_progress_streak >= MAX_NO_PROGRESS_STREAK {
                    if let Some(phase_state) = state.phase_mut(phase) {
                        phase_state.status = PhaseStatus::Failed;
                        phase_state.last_error_code = Some("no_progress".to_string());
                    }
                    let failed_dependents = mark_failed_dependents(&workflow, &mut state, phase);
                    self.emit_dependency_failures(&mut state, &session, failed_dependents)?;
                    session = self.sessions.fail(
                        session.session_id.as_str(),
                        session.revision,
                        "no_progress",
                    )?;
                    self.emit(
                        &mut state,
                        &session,
                        OrchestratorEventKind::NoProgress,
                        phase,
                        PhaseStatus::Failed,
                        Some(attempt_id),
                        "phase stopped after repeated lack of evidence-based progress",
                        result.progress,
                        result.artifact_refs,
                        result.evidence_refs,
                        true,
                        Some(AgentSessionStatus::Failed),
                        Some("no_progress".to_string()),
                        false,
                    )?;
                    self.persist_state(&state)?;
                    return Ok(OrchestratorStep {
                        session,
                        phase,
                        phase_status: PhaseStatus::Failed,
                        result: Some(PhaseResult::failed(
                            phase,
                            "phase stopped after repeated lack of progress",
                            "no_progress",
                            false,
                        )),
                        context: Some(context),
                        next_phase: None,
                        summary: OrchestratorStepSummary {
                            terminal: true,
                            waiting: false,
                            progress: false,
                        },
                    });
                }
                self.advance_after_completed(
                    &mut session,
                    &mut state,
                    &workflow,
                    phase,
                    Some((result, attempt_id)),
                    progressed,
                    Some(context),
                )
            }
            PhaseStatus::Waiting => {
                let reason = result
                    .stop_reason
                    .unwrap_or(StopReason::SafetyConfirmationRequired);
                session = self.sessions.wait_for_user(
                    session.session_id.as_str(),
                    session.revision,
                    reason,
                )?;
                self.emit(
                    &mut state,
                    &session,
                    OrchestratorEventKind::WaitingForUser,
                    phase,
                    PhaseStatus::Waiting,
                    Some(attempt_id),
                    result.summary.clone(),
                    result.progress,
                    result.artifact_refs.clone(),
                    result.evidence_refs.clone(),
                    false,
                    Some(AgentSessionStatus::WaitingForUser),
                    result.error_code.clone(),
                    result_has_evidence_limitation(&result),
                )?;
                self.persist_state(&state)?;
                Ok(OrchestratorStep {
                    session,
                    phase,
                    phase_status: PhaseStatus::Waiting,
                    result: Some(result),
                    context: Some(context),
                    next_phase: None,
                    summary: OrchestratorStepSummary {
                        terminal: false,
                        waiting: true,
                        progress: false,
                    },
                })
            }
            PhaseStatus::Failed => {
                let retry_limit = session.budget.limits().max_retries.saturating_add(1);
                if result.retryable
                    && u64::from(state.phase(phase).map_or(0, |value| value.attempt_count))
                        < retry_limit
                {
                    if let Some(phase_state) = state.phase_mut(phase) {
                        phase_state.status = PhaseStatus::Ready;
                    }
                    let error_code = result
                        .error_code
                        .clone()
                        .unwrap_or_else(|| "phase_failed".to_string());
                    self.emit(
                        &mut state,
                        &session,
                        OrchestratorEventKind::PhaseFailed,
                        phase,
                        PhaseStatus::Ready,
                        Some(attempt_id),
                        "phase failed and may be retried",
                        None,
                        result.artifact_refs.clone(),
                        result.evidence_refs.clone(),
                        false,
                        Some(AgentSessionStatus::Running),
                        Some(error_code),
                        result_has_evidence_limitation(&result),
                    )?;
                    self.persist_state(&state)?;
                    Ok(OrchestratorStep {
                        session,
                        phase,
                        phase_status: PhaseStatus::Ready,
                        result: Some(result),
                        context: Some(context),
                        next_phase: None,
                        summary: OrchestratorStepSummary {
                            terminal: false,
                            waiting: false,
                            progress: false,
                        },
                    })
                } else {
                    let error_code = result
                        .error_code
                        .clone()
                        .unwrap_or_else(|| "phase_failed".to_string());
                    let failed_dependents = mark_failed_dependents(&workflow, &mut state, phase);
                    self.emit_dependency_failures(&mut state, &session, failed_dependents)?;
                    session = self.sessions.fail_with_reason(
                        session.session_id.as_str(),
                        session.revision,
                        error_code.clone(),
                        result.stop_reason.unwrap_or(StopReason::InternalError),
                    )?;
                    self.emit(
                        &mut state,
                        &session,
                        OrchestratorEventKind::PhaseFailed,
                        phase,
                        PhaseStatus::Failed,
                        Some(attempt_id),
                        result.summary.clone(),
                        result.progress,
                        result.artifact_refs.clone(),
                        result.evidence_refs.clone(),
                        true,
                        Some(AgentSessionStatus::Failed),
                        Some(error_code),
                        result_has_evidence_limitation(&result),
                    )?;
                    self.persist_state(&state)?;
                    Ok(OrchestratorStep {
                        session,
                        phase,
                        phase_status: PhaseStatus::Failed,
                        result: Some(result),
                        context: Some(context),
                        next_phase: None,
                        summary: OrchestratorStepSummary {
                            terminal: true,
                            waiting: false,
                            progress: false,
                        },
                    })
                }
            }
            PhaseStatus::Cancelled => {
                session = self
                    .sessions
                    .cancel(session.session_id.as_str(), session.revision)?;
                self.emit(
                    &mut state,
                    &session,
                    OrchestratorEventKind::PhaseCancelled,
                    phase,
                    PhaseStatus::Cancelled,
                    Some(attempt_id),
                    result.summary.clone(),
                    None,
                    result.artifact_refs.clone(),
                    result.evidence_refs.clone(),
                    true,
                    Some(AgentSessionStatus::Cancelled),
                    result.error_code.clone(),
                    false,
                )?;
                self.persist_state(&state)?;
                Ok(OrchestratorStep {
                    session,
                    phase,
                    phase_status: PhaseStatus::Cancelled,
                    result: Some(result),
                    context: Some(context),
                    next_phase: None,
                    summary: OrchestratorStepSummary {
                        terminal: true,
                        waiting: false,
                        progress: false,
                    },
                })
            }
            PhaseStatus::Exhausted => {
                session = self
                    .sessions
                    .mark_budget_exhausted(session.session_id.as_str(), session.revision)?;
                self.emit(
                    &mut state,
                    &session,
                    OrchestratorEventKind::PhaseExhausted,
                    phase,
                    PhaseStatus::Exhausted,
                    Some(attempt_id),
                    result.summary.clone(),
                    None,
                    result.artifact_refs.clone(),
                    result.evidence_refs.clone(),
                    true,
                    Some(AgentSessionStatus::BudgetExhausted),
                    result.error_code.clone(),
                    false,
                )?;
                self.persist_state(&state)?;
                Ok(OrchestratorStep {
                    session,
                    phase,
                    phase_status: PhaseStatus::Exhausted,
                    result: Some(result),
                    context: Some(context),
                    next_phase: None,
                    summary: OrchestratorStepSummary {
                        terminal: true,
                        waiting: false,
                        progress: false,
                    },
                })
            }
            _ => Err(OrchestratorError::InvalidPhaseOutput {
                reason: "unsupported phase status".to_string(),
            }),
        }
    }

    fn advance_after_completed(
        &self,
        session: &mut AgentSession,
        state: &mut WorkflowState,
        workflow: &WorkflowDefinition,
        phase: PhaseId,
        completed: Option<(PhaseResult, crate::ai::session::AttemptId)>,
        progressed: bool,
        context: Option<ContextBuildResult>,
    ) -> Result<OrchestratorStep, OrchestratorError> {
        workflow.refresh_ready(state);
        if let Some((result, attempt_id)) = &completed {
            self.emit(
                state,
                session,
                if result.status == PhaseStatus::Skipped {
                    OrchestratorEventKind::PhaseSkipped
                } else {
                    OrchestratorEventKind::PhaseCompleted
                },
                phase,
                result.status,
                Some(attempt_id.clone()),
                result.summary.clone(),
                result.progress,
                result.artifact_refs.clone(),
                result.evidence_refs.clone(),
                false,
                Some(AgentSessionStatus::Running),
                result.error_code.clone(),
                false,
            )?;
        }
        let next_phase = workflow.ready_phases(state).into_iter().next();
        if let Some(next_phase) = next_phase {
            if next_phase == phase {
                return Err(OrchestratorError::StateConflict);
            }
            if !phase.can_transition_to(next_phase) {
                return Err(OrchestratorError::InvalidPhaseTransition {
                    from: phase.to_string(),
                    to: next_phase.to_string(),
                });
            }
            *session = self.sessions.transition_phase(
                session.session_id.as_str(),
                session.revision,
                next_phase,
            )?;
            self.emit_ready_events(state, session)?;
            self.persist_state(state)?;
            return Ok(OrchestratorStep {
                session: session.clone(),
                phase,
                phase_status: state
                    .phase(phase)
                    .map_or(PhaseStatus::Succeeded, |value| value.status),
                result: completed.map(|value| value.0),
                context,
                next_phase: Some(next_phase),
                summary: OrchestratorStepSummary {
                    terminal: false,
                    waiting: false,
                    progress: progressed,
                },
            });
        }
        if state
            .phase_states
            .iter()
            .any(|phase_state| !phase_state.status.is_terminal())
        {
            return Err(OrchestratorError::NoReadyPhase);
        }
        *session = self
            .sessions
            .complete(session.session_id.as_str(), session.revision)?;
        self.emit(
            state,
            session,
            OrchestratorEventKind::SessionCompleted,
            phase,
            PhaseStatus::Succeeded,
            None,
            "agent orchestration completed",
            None,
            Vec::new(),
            Vec::new(),
            true,
            Some(AgentSessionStatus::Completed),
            None,
            false,
        )?;
        self.persist_state(state)?;
        Ok(OrchestratorStep {
            session: session.clone(),
            phase,
            phase_status: state
                .phase(phase)
                .map_or(PhaseStatus::Succeeded, |value| value.status),
            result: completed.map(|value| value.0),
            context,
            next_phase: None,
            summary: OrchestratorStepSummary {
                terminal: true,
                waiting: false,
                progress: progressed,
            },
        })
    }

    fn fail_dependency(
        &self,
        session: &mut AgentSession,
        state: &mut WorkflowState,
        workflow: &WorkflowDefinition,
        phase: PhaseId,
    ) -> Result<OrchestratorStep, OrchestratorError> {
        if let Some(phase_state) = state.phase_mut(phase) {
            phase_state.status = PhaseStatus::Failed;
            phase_state.last_error_code = Some("dependency_failed".to_string());
        }
        let failed_dependents = mark_failed_dependents(workflow, state, phase);
        self.emit_dependency_failures(state, session, failed_dependents)?;
        *session = self.sessions.fail_with_reason(
            session.session_id.as_str(),
            session.revision,
            "dependency_failed",
            StopReason::DependencyFailed,
        )?;
        self.emit(
            state,
            session,
            OrchestratorEventKind::PhaseFailed,
            phase,
            PhaseStatus::Failed,
            None,
            "phase dependency failed",
            None,
            Vec::new(),
            Vec::new(),
            true,
            Some(AgentSessionStatus::Failed),
            Some("dependency_failed".to_string()),
            false,
        )?;
        self.persist_state(state)?;
        Ok(OrchestratorStep {
            session: session.clone(),
            phase,
            phase_status: PhaseStatus::Failed,
            result: Some(PhaseResult::failed_with_reason(
                phase,
                "phase dependency failed",
                "dependency_failed",
                false,
                StopReason::DependencyFailed,
            )),
            context: None,
            next_phase: None,
            summary: OrchestratorStepSummary {
                terminal: true,
                waiting: false,
                progress: false,
            },
        })
    }

    fn emit_dependency_failures(
        &self,
        state: &mut WorkflowState,
        session: &AgentSession,
        phases: Vec<PhaseId>,
    ) -> Result<(), OrchestratorError> {
        for phase in phases {
            self.emit(
                state,
                session,
                OrchestratorEventKind::PhaseFailed,
                phase,
                PhaseStatus::Failed,
                None,
                "phase skipped because a dependency failed",
                None,
                Vec::new(),
                Vec::new(),
                false,
                Some(session.status),
                Some("dependency_failed".to_string()),
                false,
            )?;
        }
        Ok(())
    }

    fn enrich_inputs(
        &self,
        inputs: &mut ContextInputs,
        session: &AgentSession,
    ) -> Result<(), OrchestratorError> {
        for artifact in &inputs.artifacts {
            let Some(stored) = self
                .sessions
                .artifact_store()
                .get(&artifact.header.artifact_id)?
            else {
                return Err(OrchestratorError::MissingReference {
                    kind: "artifact".to_string(),
                    id: artifact.header.artifact_id.to_string(),
                });
            };
            if stored != *artifact {
                return Err(OrchestratorError::StateConflict);
            }
        }
        for evidence in &inputs.evidence {
            let Some(stored) = self.sessions.evidence_ledger().get(&evidence.evidence_id)? else {
                return Err(OrchestratorError::MissingReference {
                    kind: "evidence".to_string(),
                    id: evidence.evidence_id.to_string(),
                });
            };
            if stored != *evidence {
                return Err(OrchestratorError::StateConflict);
            }
        }
        if let Some(preview_id) = &inputs.restore_preview
            && self.sessions.artifact_store().get(preview_id)?.is_none()
        {
            return Err(OrchestratorError::MissingReference {
                kind: "artifact".to_string(),
                id: preview_id.to_string(),
            });
        }
        if inputs.attempts.is_empty() {
            inputs.attempts = session.attempts.clone();
        }
        if inputs.remaining_budget.is_none() {
            inputs.remaining_budget = Some(session.budget.snapshot()?);
        }
        if inputs.artifacts.is_empty() {
            for artifact_id in &session.artifact_refs {
                let Some(artifact) = self.sessions.artifact_store().get(artifact_id)? else {
                    return Err(OrchestratorError::MissingReference {
                        kind: "artifact".to_string(),
                        id: artifact_id.to_string(),
                    });
                };
                inputs.artifacts.push(artifact);
            }
        }
        if inputs.evidence.is_empty() {
            for evidence_id in &session.evidence_refs {
                let Some(evidence) = self.sessions.evidence_ledger().get(evidence_id)? else {
                    return Err(OrchestratorError::MissingReference {
                        kind: "evidence".to_string(),
                        id: evidence_id.to_string(),
                    });
                };
                inputs.evidence.push(evidence);
            }
        }
        Ok(())
    }

    fn validate_references(
        &self,
        session: &AgentSession,
        state: &WorkflowState,
    ) -> Result<(), OrchestratorError> {
        for artifact_id in &session.artifact_refs {
            if self.sessions.artifact_store().get(artifact_id)?.is_none() {
                return Err(OrchestratorError::MissingReference {
                    kind: "artifact".to_string(),
                    id: artifact_id.to_string(),
                });
            }
        }
        for evidence_id in &session.evidence_refs {
            if self.sessions.evidence_ledger().get(evidence_id)?.is_none() {
                return Err(OrchestratorError::MissingReference {
                    kind: "evidence".to_string(),
                    id: evidence_id.to_string(),
                });
            }
        }
        for phase in &state.phase_states {
            for artifact_id in &phase.artifact_refs {
                if self.sessions.artifact_store().get(artifact_id)?.is_none() {
                    return Err(OrchestratorError::MissingReference {
                        kind: "artifact".to_string(),
                        id: artifact_id.to_string(),
                    });
                }
            }
            for evidence_id in &phase.evidence_refs {
                if self.sessions.evidence_ledger().get(evidence_id)?.is_none() {
                    return Err(OrchestratorError::MissingReference {
                        kind: "evidence".to_string(),
                        id: evidence_id.to_string(),
                    });
                }
            }
        }
        Ok(())
    }

    fn validate_output_references(
        &self,
        result: &PhaseResult,
        definition: &PhaseDefinition,
    ) -> Result<(), OrchestratorError> {
        for artifact_id in &result.artifact_refs {
            let Some(artifact) = self.sessions.artifact_store().get(artifact_id)? else {
                return Err(OrchestratorError::MissingReference {
                    kind: "artifact".to_string(),
                    id: artifact_id.to_string(),
                });
            };
            if !definition
                .output_artifact_kinds
                .contains(&artifact.header.kind)
            {
                return Err(OrchestratorError::UndeclaredArtifactKind {
                    phase: definition.phase_id.to_string(),
                    kind: format!("{:?}", artifact.header.kind).to_ascii_lowercase(),
                });
            }
        }
        Ok(())
    }

    fn validate_input_artifacts(
        &self,
        session: &AgentSession,
        definition: &PhaseDefinition,
    ) -> Result<(), OrchestratorError> {
        for required_kind in &definition.input_artifact_kinds {
            let mut found = false;
            for artifact_id in &session.artifact_refs {
                let Some(artifact) = self.sessions.artifact_store().get(artifact_id)? else {
                    continue;
                };
                if artifact.header.kind == *required_kind {
                    found = true;
                    break;
                }
            }
            if !found {
                return Err(OrchestratorError::PhaseNotReady {
                    phase: definition.phase_id.to_string(),
                });
            }
        }
        Ok(())
    }

    fn commit_output_references(
        &self,
        mut session: AgentSession,
        mut state: WorkflowState,
        phase: PhaseId,
        result: &PhaseResult,
    ) -> Result<(AgentSession, WorkflowState), OrchestratorError> {
        for artifact_id in &result.artifact_refs {
            if !session.artifact_refs.contains(artifact_id) {
                session = self
                    .sessions
                    .append_artifact_reference_preserving_in_flight(
                        session.session_id.as_str(),
                        session.revision,
                        artifact_id.clone(),
                    )?;
            }
        }
        for evidence_id in &result.evidence_refs {
            if !session.evidence_refs.contains(evidence_id) {
                session = self
                    .sessions
                    .append_evidence_reference_preserving_in_flight(
                        session.session_id.as_str(),
                        session.revision,
                        evidence_id.clone(),
                    )?;
            }
        }
        if let Some(phase_state) = state.phase_mut(phase) {
            phase_state.artifact_refs = result.artifact_refs.clone();
            phase_state.evidence_refs = result.evidence_refs.clone();
        }
        Ok((session, state))
    }

    fn reconcile_state(
        &self,
        session: &mut AgentSession,
        state: &mut WorkflowState,
        workflow: &WorkflowDefinition,
    ) -> Result<(), OrchestratorError> {
        if state.active_attempt.is_some()
            && !session.attempts.iter().any(|attempt| {
                Some(&attempt.attempt_id) == state.active_attempt.as_ref() && attempt.is_in_flight()
            })
        {
            state.active_attempt = None;
        }
        for phase_state in &mut state.phase_states {
            let attempt_count = session
                .attempts
                .iter()
                .filter(|attempt| attempt.phase == phase_state.phase_id)
                .count();
            phase_state.attempt_count = phase_state
                .attempt_count
                .max(u32::try_from(attempt_count).unwrap_or(u32::MAX));
            let latest_attempt = session
                .attempts
                .iter()
                .rev()
                .find(|attempt| attempt.phase == phase_state.phase_id)
                .cloned();
            if phase_state.status == PhaseStatus::Running {
                match latest_attempt.as_ref().map(|attempt| attempt.outcome) {
                    Some(AttemptOutcome::Succeeded)
                        if session.status == AgentSessionStatus::WaitingForUser
                            && session.phase == phase_state.phase_id =>
                    {
                        phase_state.status = PhaseStatus::Waiting;
                        phase_state.artifact_refs = latest_attempt
                            .as_ref()
                            .map_or_else(Vec::new, |attempt| attempt.artifact_refs.clone());
                        phase_state.evidence_refs = latest_attempt
                            .as_ref()
                            .map_or_else(Vec::new, |attempt| attempt.evidence_refs.clone());
                    }
                    Some(AttemptOutcome::Succeeded) => {
                        phase_state.status = PhaseStatus::Succeeded;
                        phase_state.artifact_refs = latest_attempt
                            .as_ref()
                            .map_or_else(Vec::new, |attempt| attempt.artifact_refs.clone());
                        phase_state.evidence_refs = latest_attempt
                            .as_ref()
                            .map_or_else(Vec::new, |attempt| attempt.evidence_refs.clone());
                    }
                    Some(AttemptOutcome::Failed | AttemptOutcome::Rejected)
                    | Some(AttemptOutcome::Interrupted) => {
                        phase_state.status = if session.status == AgentSessionStatus::Failed {
                            PhaseStatus::Failed
                        } else {
                            PhaseStatus::Ready
                        };
                        phase_state.last_error_code = latest_attempt
                            .as_ref()
                            .and_then(|attempt| attempt.safe_error_code.clone());
                    }
                    Some(AttemptOutcome::Cancelled) => {
                        phase_state.status = PhaseStatus::Cancelled;
                        phase_state.last_error_code = latest_attempt
                            .as_ref()
                            .and_then(|attempt| attempt.safe_error_code.clone());
                    }
                    Some(AttemptOutcome::Exhausted) => {
                        phase_state.status = PhaseStatus::Exhausted;
                        phase_state.last_error_code = latest_attempt
                            .as_ref()
                            .and_then(|attempt| attempt.safe_error_code.clone());
                    }
                    Some(AttemptOutcome::Running) => {}
                    None => {
                        phase_state.status = PhaseStatus::Ready;
                        phase_state.last_error_code = Some("interrupted".to_string());
                    }
                }
            }
        }
        if session.status == AgentSessionStatus::Running {
            workflow.refresh_ready(state);
        }
        state.validate(workflow)?;
        Ok(())
    }

    fn load_or_rebuild_state(
        &self,
        session: &AgentSession,
        workflow: &WorkflowDefinition,
    ) -> Result<WorkflowState, OrchestratorError> {
        let state = match self.read_state(&session.session_id)? {
            Some(state) => state,
            None => self.rebuild_state(session, workflow),
        };
        if state.session_id != session.session_id {
            return Err(OrchestratorError::StateConflict);
        }
        state.validate(workflow)?;
        Ok(state)
    }

    fn rebuild_state(
        &self,
        session: &AgentSession,
        workflow: &WorkflowDefinition,
    ) -> WorkflowState {
        let mut state = workflow.initial_state(session.session_id.clone());
        for attempt in &session.attempts {
            if let Some(phase_state) = state.phase_mut(attempt.phase) {
                phase_state.attempt_count = phase_state.attempt_count.saturating_add(1);
                phase_state.artifact_refs = attempt.artifact_refs.clone();
                phase_state.evidence_refs = attempt.evidence_refs.clone();
                phase_state.last_error_code = attempt.safe_error_code.clone();
                phase_state.status = match attempt.outcome {
                    AttemptOutcome::Succeeded => PhaseStatus::Succeeded,
                    AttemptOutcome::Interrupted => PhaseStatus::Ready,
                    AttemptOutcome::Failed | AttemptOutcome::Rejected => {
                        if session.status.is_terminal() {
                            PhaseStatus::Failed
                        } else {
                            PhaseStatus::Ready
                        }
                    }
                    AttemptOutcome::Cancelled => PhaseStatus::Cancelled,
                    AttemptOutcome::Exhausted => PhaseStatus::Exhausted,
                    AttemptOutcome::Running => PhaseStatus::Running,
                };
            }
        }
        if let Some(phase_state) = state.phase_mut(session.phase)
            && !phase_state.status.is_terminal()
        {
            phase_state.status = if session.status == AgentSessionStatus::WaitingForUser {
                PhaseStatus::Waiting
            } else {
                PhaseStatus::Ready
            };
        }
        workflow.refresh_ready(&mut state);
        state
    }

    fn finish_event_summary(&self, value: &str) -> String {
        redact_text(value).chars().take(512).collect()
    }

    #[allow(clippy::too_many_arguments)]
    fn emit(
        &self,
        state: &mut WorkflowState,
        session: &AgentSession,
        kind: OrchestratorEventKind,
        phase: PhaseId,
        status: PhaseStatus,
        attempt_id: Option<crate::ai::session::AttemptId>,
        summary: impl Into<String>,
        progress: Option<ProgressSignal>,
        artifact_refs: Vec<crate::ai::session::ArtifactId>,
        evidence_refs: Vec<crate::ai::session::EvidenceId>,
        terminal: bool,
        terminal_status: Option<AgentSessionStatus>,
        error_code: Option<String>,
        evidence_limitation: bool,
    ) -> Result<(), OrchestratorError> {
        let sequence = state.next_event_sequence;
        if sequence == 0 || sequence > 4_096 {
            return Err(OrchestratorError::InvalidWorkflow {
                reason: "orchestrator event limit exceeded".to_string(),
            });
        }
        let event_id = TraceEventId::from_string(hash_bytes(
            "trace-",
            format!("{}\n{}\norchestrator", session.session_id, sequence).as_bytes(),
        ))?;
        let event = OrchestratorEvent {
            schema_version: ORCHESTRATOR_EVENT_SCHEMA_VERSION,
            event_id,
            session_id: session.session_id.clone(),
            sequence,
            timestamp: chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Millis, true),
            kind,
            phase,
            status,
            attempt_id,
            artifact_refs,
            evidence_refs,
            summary: self.finish_event_summary(&summary.into()),
            progress,
            terminal,
            terminal_status,
            error_code: error_code.map(|value| safe_error_code(&value)),
            retryable: matches!(kind, OrchestratorEventKind::PhaseFailed) && !terminal,
            evidence_limitation,
        };
        event.validate()?;
        state.events.push(event.clone());
        state.next_event_sequence = state.next_event_sequence.saturating_add(1);
        if let Some(sink) = &self.event_sink {
            sink(&event);
        }
        Ok(())
    }

    fn emit_ready_events(
        &self,
        state: &mut WorkflowState,
        session: &AgentSession,
    ) -> Result<(), OrchestratorError> {
        let ready = state
            .phase_states
            .iter()
            .filter(|phase| phase.status == PhaseStatus::Ready)
            .map(|phase| phase.phase_id)
            .collect::<Vec<_>>();
        for phase in ready {
            if state.events.iter().any(|event| {
                event.phase == phase
                    && event.kind == OrchestratorEventKind::PhaseReady
                    && !event.terminal
            }) {
                continue;
            }
            self.emit(
                state,
                session,
                OrchestratorEventKind::PhaseReady,
                phase,
                PhaseStatus::Ready,
                None,
                "phase is ready",
                None,
                Vec::new(),
                Vec::new(),
                false,
                Some(AgentSessionStatus::Running),
                None,
                false,
            )?;
        }
        Ok(())
    }

    fn state_path(&self, session_id: &AgentSessionId) -> Result<PathBuf, OrchestratorError> {
        let value = session_id.as_str();
        if !value.starts_with("session-") || value.contains("..") || value.contains('/') {
            return Err(OrchestratorError::InvalidIdentifier {
                kind: "session ID".to_string(),
            });
        }
        let directory = self.state_root.join(ORCHESTRATOR_DIRECTORY_NAME);
        ensure_directory(&directory)?;
        let path = directory.join(format!("{value}.json"));
        ensure_regular_or_missing(&path)?;
        Ok(path)
    }

    fn read_state(
        &self,
        session_id: &AgentSessionId,
    ) -> Result<Option<WorkflowState>, OrchestratorError> {
        let path = self.state_path(session_id)?;
        let bytes = match fs::read(&path) {
            Ok(bytes) => bytes,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
            Err(_) => return Err(OrchestratorError::io("read workflow state")),
        };
        if bytes.len() > MAX_STATE_FILE_BYTES {
            return Err(OrchestratorError::MalformedState);
        }
        let state = serde_json::from_slice::<WorkflowState>(&bytes)
            .map_err(|_| OrchestratorError::MalformedState)?;
        if state.schema_version != super::model::ORCHESTRATOR_STATE_SCHEMA_VERSION {
            return Err(OrchestratorError::UnsupportedStateVersion {
                version: state.schema_version,
            });
        }
        Ok(Some(state))
    }

    fn persist_state(&self, state: &WorkflowState) -> Result<(), OrchestratorError> {
        let path = self.state_path(&state.session_id)?;
        let bytes = serde_json::to_vec_pretty(state)
            .map_err(|_| OrchestratorError::serialization("encode workflow state"))?;
        if bytes.len() > MAX_STATE_FILE_BYTES {
            return Err(OrchestratorError::MalformedState);
        }
        let parent = path.parent().ok_or(OrchestratorError::StateConflict)?;
        let temporary = path.with_file_name(format!(
            ".{}.tmp-{}-{}",
            path.file_name()
                .and_then(|value| value.to_str())
                .unwrap_or("state.json"),
            std::process::id(),
            TEMP_SEQUENCE.fetch_add(1, Ordering::Relaxed)
        ));
        let result = (|| {
            let mut file = OpenOptions::new()
                .write(true)
                .create_new(true)
                .open(&temporary)
                .map_err(|_| OrchestratorError::io("create temporary workflow state"))?;
            set_file_mode(&file)?;
            file.write_all(&bytes)
                .map_err(|_| OrchestratorError::io("write temporary workflow state"))?;
            file.sync_all()
                .map_err(|_| OrchestratorError::io("sync temporary workflow state"))?;
            fs::rename(&temporary, &path)
                .map_err(|_| OrchestratorError::io("publish workflow state"))?;
            sync_directory(parent)
        })();
        if result.is_err() {
            let _ = fs::remove_file(&temporary);
        }
        result
    }
}

fn role_for_phase(phase: PhaseId) -> ContextRole {
    match phase {
        AgentPhase::Classify | AgentPhase::Plan => ContextRole::Routing,
        AgentPhase::Search | AgentPhase::Analyze => ContextRole::Search,
        AgentPhase::Explain => ContextRole::HistoryExplanation,
        AgentPhase::RestorePreview
        | AgentPhase::Confirm
        | AgentPhase::Commit
        | AgentPhase::Verify => ContextRole::Restore,
        AgentPhase::Complete => ContextRole::Conversation,
    }
}

fn phase_budget_cost(
    class: BudgetClass,
    context: &ContextBuildResult,
    attempt_count: u32,
) -> BudgetCost {
    let mut cost = BudgetCost {
        context_bytes: context.byte_size,
        context_tokens: context.token_estimate,
        ..BudgetCost::default()
    };
    match class {
        BudgetClass::Free => {}
        BudgetClass::Model => {
            cost.model_calls = 1;
            cost.output_tokens = 1;
        }
        BudgetClass::Tool => cost.tool_calls = 1,
        BudgetClass::Search => cost.search_actions = 1,
        BudgetClass::Expensive => {
            cost.model_calls = 1;
            cost.output_tokens = 1;
            cost.tool_calls = 1;
            cost.search_actions = 1;
        }
    }
    if attempt_count > 0 {
        cost.retries = 1;
    }
    cost
}

fn budget_usage(cost: BudgetCost) -> BudgetUsage {
    BudgetUsage {
        wall_clock_ms: cost.wall_clock_ms,
        model_calls: cost.model_calls,
        output_tokens: cost.output_tokens,
        tool_calls: cost.tool_calls,
        search_actions: cost.search_actions,
        candidates: cost.candidates,
        context_bytes: cost.context_bytes,
        context_tokens: cost.context_tokens,
        retries: cost.retries,
        artifact_bytes: cost.artifact_bytes,
        evidence_bytes: cost.evidence_bytes,
    }
}

fn validate_result(
    result: &PhaseResult,
    definition: &PhaseDefinition,
) -> Result<(), OrchestratorError> {
    result.validate(definition)
}

fn mark_failed_dependents(
    workflow: &WorkflowDefinition,
    state: &mut WorkflowState,
    failed_phase: PhaseId,
) -> Vec<PhaseId> {
    let mut failed_phases = Vec::new();
    let mut changed = true;
    while changed {
        changed = false;
        for definition in &workflow.phases {
            let blocked = definition.prerequisites.iter().any(|prerequisite| {
                *prerequisite == failed_phase
                    || state
                        .phase(*prerequisite)
                        .is_some_and(|phase| phase.status.is_dependency_failure())
            });
            if !blocked {
                continue;
            }
            if let Some(phase) = state.phase_mut(definition.phase_id)
                && matches!(phase.status, PhaseStatus::Pending | PhaseStatus::Ready)
            {
                phase.status = PhaseStatus::Failed;
                phase.last_error_code = Some("dependency_failed".to_string());
                failed_phases.push(phase.phase_id);
                changed = true;
            }
        }
    }
    failed_phases.sort();
    failed_phases
}

fn terminal_step(
    session: &AgentSession,
    phase: PhaseId,
    state: &WorkflowState,
) -> OrchestratorStep {
    let phase_status = state
        .phase(phase)
        .map_or(PhaseStatus::Succeeded, |value| value.status);
    OrchestratorStep {
        session: session.clone(),
        phase,
        phase_status,
        result: None,
        context: None,
        next_phase: None,
        summary: OrchestratorStepSummary {
            terminal: true,
            waiting: false,
            progress: false,
        },
    }
}

fn safe_error_code(value: &str) -> String {
    let value = value.trim().to_ascii_lowercase();
    if value.is_empty()
        || value.len() > 96
        || value.chars().any(|character| {
            !(character.is_ascii_lowercase()
                || character.is_ascii_digit()
                || matches!(character, '_' | '-'))
        })
    {
        "phase_failed".to_string()
    } else {
        value
    }
}

fn result_has_evidence_limitation(result: &PhaseResult) -> bool {
    result.stop_reason == Some(StopReason::EvidenceInsufficient)
}

fn is_safe_identifier(value: &str) -> bool {
    !value.is_empty()
        && value.len() <= 128
        && value != "."
        && value != ".."
        && value.chars().all(|character| {
            character.is_ascii_alphanumeric() || matches!(character, '-' | '_' | '.')
        })
}

fn ensure_directory(path: &Path) -> Result<(), OrchestratorError> {
    match fs::symlink_metadata(path) {
        Ok(metadata) if metadata.file_type().is_symlink() || !metadata.is_dir() => {
            Err(OrchestratorError::io("inspect workflow state directory"))
        }
        Ok(_) => Ok(()),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            fs::create_dir_all(path)
                .map_err(|_| OrchestratorError::io("create workflow state directory"))?;
            set_directory_mode(path)?;
            Ok(())
        }
        Err(_) => Err(OrchestratorError::io("inspect workflow state directory")),
    }
}

fn ensure_regular_or_missing(path: &Path) -> Result<(), OrchestratorError> {
    match fs::symlink_metadata(path) {
        Ok(metadata) if metadata.file_type().is_symlink() || !metadata.is_file() => {
            Err(OrchestratorError::io("inspect workflow state file"))
        }
        Ok(_) => Ok(()),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(_) => Err(OrchestratorError::io("inspect workflow state file")),
    }
}

fn set_file_mode(file: &File) -> Result<(), OrchestratorError> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        file.set_permissions(fs::Permissions::from_mode(0o600))
            .map_err(|_| OrchestratorError::io("protect workflow state file"))?;
    }
    Ok(())
}

fn set_directory_mode(path: &Path) -> Result<(), OrchestratorError> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(path, fs::Permissions::from_mode(0o700))
            .map_err(|_| OrchestratorError::io("protect workflow state directory"))?;
    }
    Ok(())
}

fn sync_directory(path: &Path) -> Result<(), OrchestratorError> {
    #[cfg(unix)]
    {
        File::open(path)
            .and_then(|file| file.sync_all())
            .map_err(|_| OrchestratorError::io("sync workflow state directory"))?;
    }
    #[cfg(not(unix))]
    {
        let _ = path;
    }
    Ok(())
}
