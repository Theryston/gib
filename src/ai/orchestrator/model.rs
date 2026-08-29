use super::error::OrchestratorError;
use crate::ai::session::{
    AgentPhase, AgentSession, AgentSessionId, AgentSessionStatus, ArtifactId, ArtifactKind,
    AttemptId, EvidenceId, StopReason, TraceEventId,
};
use crate::ai::session::{canonical_bytes, hash_bytes, redact_text};
use chrono::DateTime;
use serde::{Deserialize, Serialize};
use std::fmt;
use std::str::FromStr;

pub(crate) const WORKFLOW_SCHEMA_VERSION: u32 = 1;
pub(crate) const ORCHESTRATOR_EVENT_SCHEMA_VERSION: u32 = 1;
pub(crate) const ORCHESTRATOR_STATE_SCHEMA_VERSION: u32 = 1;
pub(crate) const MAX_WORKFLOW_PHASES: usize = 64;
pub(crate) const MAX_WORKFLOW_PARALLELISM: usize = 8;
pub(crate) const MAX_EVENT_REFERENCES: usize = 512;
pub(crate) const MAX_NO_PROGRESS_STREAK: u32 = 3;

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub(crate) struct WorkflowId(String);

impl WorkflowId {
    pub(crate) fn new(value: impl Into<String>) -> Result<Self, OrchestratorError> {
        Self::from_string(value.into())
    }

    pub(crate) fn from_string(value: String) -> Result<Self, OrchestratorError> {
        if !is_safe_identifier(&value) || value.len() > 128 {
            return Err(OrchestratorError::InvalidIdentifier {
                kind: "workflow ID".to_string(),
            });
        }
        Ok(Self(value))
    }

    pub(crate) fn as_str(&self) -> &str {
        &self.0
    }

    fn validate(&self) -> Result<(), OrchestratorError> {
        Self::from_string(self.0.clone()).map(|_| ())
    }
}

impl AsRef<str> for WorkflowId {
    fn as_ref(&self) -> &str {
        self.as_str()
    }
}

impl fmt::Display for WorkflowId {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(self.as_str())
    }
}

impl FromStr for WorkflowId {
    type Err = OrchestratorError;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        Self::from_string(value.to_string())
    }
}

/// The phase identifier remains the finite, session-owned phase enum. This
/// prevents a workflow from inventing a phase that the session service cannot
/// trace or transition.
pub(crate) type PhaseId = AgentPhase;

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub(crate) enum IntentKind {
    Locate,
    ExplainHistory,
    Restore,
    #[serde(rename = "custom")]
    Custom(String),
}

impl IntentKind {
    pub(crate) fn as_str(&self) -> &str {
        match self {
            Self::Locate => "locate",
            Self::ExplainHistory => "explain-history",
            Self::Restore => "restore",
            Self::Custom(value) => value.as_str(),
        }
    }

    pub(crate) fn validate(&self) -> Result<(), OrchestratorError> {
        if self.as_str().is_empty() || !is_safe_identifier(self.as_str()) {
            return Err(OrchestratorError::InvalidIdentifier {
                kind: "intent".to_string(),
            });
        }
        Ok(())
    }
}

impl fmt::Display for IntentKind {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(self.as_str())
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub(crate) enum BudgetClass {
    Free,
    Model,
    Tool,
    Search,
    Expensive,
}

impl Default for BudgetClass {
    fn default() -> Self {
        Self::Free
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub(crate) enum AllowedSideEffects {
    None,
    ReadOnly,
    LocalAnalysis,
    Preview,
    ConfirmationRequired,
    Commit,
}

impl Default for AllowedSideEffects {
    fn default() -> Self {
        Self::None
    }
}

impl AllowedSideEffects {
    pub(crate) fn is_side_effecting(self) -> bool {
        matches!(self, Self::ConfirmationRequired | Self::Commit)
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub(crate) struct PhaseDefinition {
    pub(crate) phase_id: PhaseId,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub(crate) prerequisites: Vec<PhaseId>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub(crate) input_artifact_kinds: Vec<ArtifactKind>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub(crate) output_artifact_kinds: Vec<ArtifactKind>,
    #[serde(default)]
    pub(crate) budget_class: BudgetClass,
    #[serde(default)]
    pub(crate) allowed_side_effects: AllowedSideEffects,
}

impl PhaseDefinition {
    pub(crate) fn new(phase_id: PhaseId) -> Self {
        Self {
            phase_id,
            prerequisites: Vec::new(),
            input_artifact_kinds: Vec::new(),
            output_artifact_kinds: Vec::new(),
            budget_class: BudgetClass::Free,
            allowed_side_effects: AllowedSideEffects::None,
        }
    }

    pub(crate) fn with_prerequisites(mut self, prerequisites: Vec<PhaseId>) -> Self {
        self.prerequisites = prerequisites;
        self
    }

    pub(crate) fn with_artifacts(
        mut self,
        inputs: Vec<ArtifactKind>,
        outputs: Vec<ArtifactKind>,
    ) -> Self {
        self.input_artifact_kinds = inputs;
        self.output_artifact_kinds = outputs;
        self
    }

    pub(crate) fn with_budget(mut self, budget_class: BudgetClass) -> Self {
        self.budget_class = budget_class;
        self
    }

    pub(crate) fn with_side_effects(mut self, allowed_side_effects: AllowedSideEffects) -> Self {
        self.allowed_side_effects = allowed_side_effects;
        self
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub(crate) struct WorkflowDefinition {
    pub(crate) schema_version: u32,
    pub(crate) workflow_id: WorkflowId,
    pub(crate) version: String,
    pub(crate) supported_intents: Vec<IntentKind>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub(crate) required_capabilities: Vec<String>,
    pub(crate) phases: Vec<PhaseDefinition>,
    #[serde(default = "default_parallelism")]
    pub(crate) max_parallelism: usize,
}

impl WorkflowDefinition {
    pub(crate) fn new(
        workflow_id: WorkflowId,
        version: impl Into<String>,
        supported_intents: Vec<IntentKind>,
        phases: Vec<PhaseDefinition>,
    ) -> Self {
        Self {
            schema_version: WORKFLOW_SCHEMA_VERSION,
            workflow_id,
            version: version.into(),
            supported_intents,
            required_capabilities: Vec::new(),
            phases,
            max_parallelism: 1,
        }
    }

    pub(crate) fn with_required_capabilities(mut self, capabilities: Vec<String>) -> Self {
        self.required_capabilities = capabilities;
        self
    }

    pub(crate) fn with_max_parallelism(mut self, max_parallelism: usize) -> Self {
        self.max_parallelism = max_parallelism;
        self
    }

    pub(crate) fn validate(&self) -> Result<(), OrchestratorError> {
        if self.schema_version != WORKFLOW_SCHEMA_VERSION {
            return Err(OrchestratorError::InvalidWorkflow {
                reason: "unsupported workflow schema version".to_string(),
            });
        }
        self.workflow_id.validate()?;
        if self.version.is_empty() || self.version.len() > 128 || !is_safe_identifier(&self.version)
        {
            return Err(OrchestratorError::InvalidIdentifier {
                kind: "workflow version".to_string(),
            });
        }
        if self.supported_intents.is_empty() {
            return Err(OrchestratorError::InvalidWorkflow {
                reason: "workflow has no supported intent".to_string(),
            });
        }
        for intent in &self.supported_intents {
            intent.validate()?;
        }
        if has_duplicates(&self.supported_intents) {
            return Err(OrchestratorError::InvalidWorkflow {
                reason: "workflow lists an intent more than once".to_string(),
            });
        }
        if has_duplicates(&self.required_capabilities)
            || self
                .required_capabilities
                .iter()
                .any(|capability| !is_safe_identifier(capability))
        {
            return Err(OrchestratorError::InvalidWorkflow {
                reason: "workflow capabilities must be unique safe identifiers".to_string(),
            });
        }
        if self.phases.is_empty() {
            return Err(OrchestratorError::EmptyWorkflow);
        }
        if self.phases.len() > MAX_WORKFLOW_PHASES {
            return Err(OrchestratorError::InvalidWorkflow {
                reason: "workflow has too many phases".to_string(),
            });
        }
        if self.max_parallelism == 0 || self.max_parallelism > MAX_WORKFLOW_PARALLELISM {
            return Err(OrchestratorError::InvalidWorkflow {
                reason: "workflow parallelism exceeds the bounded scheduler limit".to_string(),
            });
        }

        let mut positions = std::collections::BTreeMap::new();
        for (position, definition) in self.phases.iter().enumerate() {
            if positions.insert(definition.phase_id, position).is_some() {
                return Err(OrchestratorError::DuplicatePhase {
                    phase: definition.phase_id.to_string(),
                });
            }
            if definition.allowed_side_effects == AllowedSideEffects::Commit
                && definition.phase_id != AgentPhase::Commit
            {
                return Err(OrchestratorError::InvalidWorkflow {
                    reason: "commit side effects must belong to the commit phase".to_string(),
                });
            }
            if definition.allowed_side_effects == AllowedSideEffects::ConfirmationRequired
                && definition.phase_id != AgentPhase::Confirm
            {
                return Err(OrchestratorError::InvalidWorkflow {
                    reason: "confirmation side effects must belong to the confirm phase"
                        .to_string(),
                });
            }
            if definition.phase_id == AgentPhase::Commit
                && definition.allowed_side_effects != AllowedSideEffects::Commit
            {
                return Err(OrchestratorError::InvalidWorkflow {
                    reason: "commit phase must declare commit side effects".to_string(),
                });
            }
            if definition.phase_id == AgentPhase::Complete {
                return Err(OrchestratorError::InvalidWorkflow {
                    reason: "complete is owned by the session terminal transition".to_string(),
                });
            }
            if definition.phase_id == AgentPhase::Commit
                && !definition.prerequisites.contains(&AgentPhase::Confirm)
            {
                return Err(OrchestratorError::InvalidWorkflow {
                    reason: "commit phase requires confirmation".to_string(),
                });
            }
        }
        for definition in &self.phases {
            let mut prerequisites = definition.prerequisites.clone();
            prerequisites.sort();
            if prerequisites
                .windows(2)
                .any(|values| values[0] == values[1])
            {
                return Err(OrchestratorError::InvalidWorkflow {
                    reason: "a phase lists a prerequisite more than once".to_string(),
                });
            }
            for prerequisite in &definition.prerequisites {
                if !positions.contains_key(prerequisite) {
                    return Err(OrchestratorError::UnknownPrerequisite {
                        phase: definition.phase_id.to_string(),
                        prerequisite: prerequisite.to_string(),
                    });
                }
                if *prerequisite == definition.phase_id {
                    return Err(OrchestratorError::DependencyCycle);
                }
            }
            validate_unique_artifact_kinds(&definition.input_artifact_kinds)?;
            validate_unique_artifact_kinds(&definition.output_artifact_kinds)?;
        }

        let mut marks = vec![VisitMark::Unvisited; self.phases.len()];
        for index in 0..self.phases.len() {
            visit(index, self, &positions, &mut marks)?;
        }
        Ok(())
    }

    pub(crate) fn phase(&self, phase: PhaseId) -> Option<&PhaseDefinition> {
        self.phases
            .iter()
            .find(|definition| definition.phase_id == phase)
    }

    pub(crate) fn supports_intent(&self, intent: &IntentKind) -> bool {
        self.supported_intents
            .iter()
            .any(|candidate| candidate == intent)
    }

    pub(crate) fn initial_state(&self, session_id: AgentSessionId) -> WorkflowState {
        WorkflowState {
            schema_version: ORCHESTRATOR_STATE_SCHEMA_VERSION,
            session_id,
            workflow_id: self.workflow_id.clone(),
            workflow_version: self.version.clone(),
            phase_states: self
                .phases
                .iter()
                .map(|definition| PhaseState::new(definition.phase_id))
                .collect(),
            active_attempt: None,
            no_progress_streak: 0,
            last_progress_fingerprint: None,
            next_event_sequence: 1,
            events: Vec::new(),
        }
    }

    pub(crate) fn refresh_ready(&self, state: &mut WorkflowState) {
        for definition in &self.phases {
            let Some(current_status) = state.phase(definition.phase_id).map(|value| value.status)
            else {
                continue;
            };
            if current_status != PhaseStatus::Pending {
                continue;
            }
            let dependency_failed = definition.prerequisites.iter().any(|prerequisite| {
                state
                    .phase(*prerequisite)
                    .is_some_and(|state| state.status.is_dependency_failure())
            });
            if dependency_failed {
                continue;
            }
            let prerequisites_succeeded = definition.prerequisites.iter().all(|prerequisite| {
                state.phase(*prerequisite).is_some_and(|state| {
                    matches!(state.status, PhaseStatus::Succeeded | PhaseStatus::Skipped)
                })
            });
            if prerequisites_succeeded
                && let Some(phase_state) = state.phase_mut(definition.phase_id)
            {
                phase_state.status = PhaseStatus::Ready;
            }
        }
    }

    pub(crate) fn ready_phases(&self, state: &WorkflowState) -> Vec<PhaseId> {
        self.phases
            .iter()
            .filter_map(|definition| {
                state.phase(definition.phase_id).and_then(|phase_state| {
                    (phase_state.status == PhaseStatus::Ready).then_some(definition.phase_id)
                })
            })
            .collect()
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum VisitMark {
    Unvisited,
    Visiting,
    Visited,
}

fn visit(
    index: usize,
    workflow: &WorkflowDefinition,
    positions: &std::collections::BTreeMap<PhaseId, usize>,
    marks: &mut [VisitMark],
) -> Result<(), OrchestratorError> {
    match marks[index] {
        VisitMark::Visiting => return Err(OrchestratorError::DependencyCycle),
        VisitMark::Visited => return Ok(()),
        VisitMark::Unvisited => marks[index] = VisitMark::Visiting,
    }
    for prerequisite in &workflow.phases[index].prerequisites {
        let Some(prerequisite_index) = positions.get(prerequisite).copied() else {
            return Err(OrchestratorError::UnknownPrerequisite {
                phase: workflow.phases[index].phase_id.to_string(),
                prerequisite: prerequisite.to_string(),
            });
        };
        visit(prerequisite_index, workflow, positions, marks)?;
    }
    marks[index] = VisitMark::Visited;
    Ok(())
}

fn validate_unique_artifact_kinds(kinds: &[ArtifactKind]) -> Result<(), OrchestratorError> {
    let mut sorted = kinds.to_vec();
    sorted.sort_by_key(|kind| format!("{kind:?}"));
    if sorted.windows(2).any(|values| values[0] == values[1]) {
        return Err(OrchestratorError::InvalidWorkflow {
            reason: "a phase lists an artifact kind more than once".to_string(),
        });
    }
    Ok(())
}

fn default_parallelism() -> usize {
    1
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "kebab-case")]
pub(crate) enum PhaseStatus {
    Pending,
    Ready,
    Running,
    Waiting,
    Succeeded,
    Skipped,
    Failed,
    Cancelled,
    Exhausted,
}

impl PhaseStatus {
    pub(crate) fn is_terminal(self) -> bool {
        matches!(
            self,
            Self::Succeeded | Self::Skipped | Self::Failed | Self::Cancelled | Self::Exhausted
        )
    }

    pub(crate) fn is_dependency_failure(self) -> bool {
        matches!(self, Self::Failed | Self::Cancelled | Self::Exhausted)
    }

    pub(crate) fn can_transition_to(self, next: Self) -> bool {
        if self == next {
            return !self.is_terminal();
        }
        match self {
            Self::Pending => matches!(
                next,
                Self::Ready | Self::Skipped | Self::Failed | Self::Cancelled
            ),
            Self::Ready => matches!(
                next,
                Self::Running | Self::Skipped | Self::Failed | Self::Cancelled
            ),
            Self::Running => matches!(
                next,
                Self::Ready
                    | Self::Waiting
                    | Self::Succeeded
                    | Self::Failed
                    | Self::Cancelled
                    | Self::Exhausted
            ),
            Self::Waiting => matches!(
                next,
                Self::Ready | Self::Succeeded | Self::Failed | Self::Cancelled
            ),
            Self::Succeeded | Self::Skipped | Self::Failed | Self::Cancelled | Self::Exhausted => {
                false
            }
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub(crate) struct PhaseState {
    pub(crate) phase_id: PhaseId,
    pub(crate) status: PhaseStatus,
    pub(crate) attempt_count: u32,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) last_error_code: Option<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub(crate) artifact_refs: Vec<ArtifactId>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub(crate) evidence_refs: Vec<EvidenceId>,
}

impl PhaseState {
    fn new(phase_id: PhaseId) -> Self {
        Self {
            phase_id,
            status: PhaseStatus::Pending,
            attempt_count: 0,
            last_error_code: None,
            artifact_refs: Vec::new(),
            evidence_refs: Vec::new(),
        }
    }
}

/// Persisted orchestration state contains only identifiers, statuses,
/// fingerprints, and bounded safe events. Context and prompts are ephemeral.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub(crate) struct WorkflowState {
    pub(crate) schema_version: u32,
    pub(crate) session_id: AgentSessionId,
    pub(crate) workflow_id: WorkflowId,
    pub(crate) workflow_version: String,
    pub(crate) phase_states: Vec<PhaseState>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) active_attempt: Option<AttemptId>,
    #[serde(default)]
    pub(crate) no_progress_streak: u32,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) last_progress_fingerprint: Option<String>,
    pub(crate) next_event_sequence: u64,
    #[serde(default)]
    pub(crate) events: Vec<OrchestratorEvent>,
}

impl WorkflowState {
    pub(crate) fn phase(&self, phase: PhaseId) -> Option<&PhaseState> {
        self.phase_states
            .iter()
            .find(|state| state.phase_id == phase)
    }

    pub(crate) fn phase_mut(&mut self, phase: PhaseId) -> Option<&mut PhaseState> {
        self.phase_states
            .iter_mut()
            .find(|state| state.phase_id == phase)
    }

    pub(crate) fn validate(&self, workflow: &WorkflowDefinition) -> Result<(), OrchestratorError> {
        if self.schema_version != ORCHESTRATOR_STATE_SCHEMA_VERSION
            || self.workflow_id != workflow.workflow_id
            || self.workflow_version != workflow.version
            || self.phase_states.len() != workflow.phases.len()
            || self.next_event_sequence == 0
            || self.no_progress_streak > MAX_NO_PROGRESS_STREAK
        {
            return Err(OrchestratorError::StateConflict);
        }
        for (expected, actual) in workflow.phases.iter().zip(&self.phase_states) {
            if expected.phase_id != actual.phase_id
                || actual.attempt_count > 1_024
                || actual.artifact_refs.len() > MAX_EVENT_REFERENCES
                || actual.evidence_refs.len() > MAX_EVENT_REFERENCES
                || duplicate_ids(&actual.artifact_refs)
                || duplicate_ids(&actual.evidence_refs)
                || actual
                    .last_error_code
                    .as_deref()
                    .is_some_and(|code| safe_error_code(code) != code)
            {
                return Err(OrchestratorError::MalformedState);
            }
        }
        if self.events.len() > 4_096
            || self.events.iter().enumerate().any(|(index, event)| {
                event.session_id != self.session_id || event.sequence != index as u64 + 1
            })
            || self.next_event_sequence != self.events.len() as u64 + 1
        {
            return Err(OrchestratorError::MalformedState);
        }
        for event in &self.events {
            event.validate()?;
        }
        Ok(())
    }
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq, PartialOrd, Ord, Hash)]
#[serde(rename_all = "kebab-case")]
pub(crate) enum ProgressSignal {
    NewCandidate,
    NewTimeConstraint,
    NewVerifiedFact,
    CompletedPreview,
    ExhaustedDimension,
    NewEvidence,
    ReducedHypotheses,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub(crate) struct PhaseResult {
    pub(crate) phase_id: PhaseId,
    pub(crate) status: PhaseStatus,
    pub(crate) summary: String,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub(crate) artifact_refs: Vec<ArtifactId>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub(crate) evidence_refs: Vec<EvidenceId>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) progress: Option<ProgressSignal>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) stop_reason: Option<StopReason>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) error_code: Option<String>,
    #[serde(default)]
    pub(crate) retryable: bool,
    #[serde(default)]
    pub(crate) idempotent: bool,
}

impl PhaseResult {
    pub(crate) fn succeeded(
        phase_id: PhaseId,
        summary: impl Into<String>,
        progress: Option<ProgressSignal>,
    ) -> Self {
        Self {
            phase_id,
            status: PhaseStatus::Succeeded,
            summary: safe_summary(summary.into()),
            artifact_refs: Vec::new(),
            evidence_refs: Vec::new(),
            progress,
            stop_reason: None,
            error_code: None,
            retryable: false,
            idempotent: true,
        }
    }

    pub(crate) fn skipped(phase_id: PhaseId, summary: impl Into<String>) -> Self {
        Self {
            phase_id,
            status: PhaseStatus::Skipped,
            summary: safe_summary(summary.into()),
            artifact_refs: Vec::new(),
            evidence_refs: Vec::new(),
            progress: None,
            stop_reason: None,
            error_code: None,
            retryable: false,
            idempotent: true,
        }
    }

    pub(crate) fn waiting(
        phase_id: PhaseId,
        summary: impl Into<String>,
        reason: StopReason,
    ) -> Self {
        Self {
            phase_id,
            status: PhaseStatus::Waiting,
            summary: safe_summary(summary.into()),
            artifact_refs: Vec::new(),
            evidence_refs: Vec::new(),
            progress: None,
            stop_reason: Some(reason),
            error_code: None,
            retryable: false,
            idempotent: true,
        }
    }

    pub(crate) fn failed(
        phase_id: PhaseId,
        summary: impl Into<String>,
        error_code: impl Into<String>,
        retryable: bool,
    ) -> Self {
        Self::failed_with_reason(
            phase_id,
            summary,
            error_code,
            retryable,
            StopReason::InternalError,
        )
    }

    pub(crate) fn failed_with_reason(
        phase_id: PhaseId,
        summary: impl Into<String>,
        error_code: impl Into<String>,
        retryable: bool,
        stop_reason: StopReason,
    ) -> Self {
        Self {
            phase_id,
            status: PhaseStatus::Failed,
            summary: safe_summary(summary.into()),
            artifact_refs: Vec::new(),
            evidence_refs: Vec::new(),
            progress: None,
            stop_reason: Some(stop_reason),
            error_code: Some(safe_error_code(&error_code.into())),
            retryable,
            idempotent: true,
        }
    }

    pub(crate) fn cancelled(phase_id: PhaseId, summary: impl Into<String>) -> Self {
        Self {
            phase_id,
            status: PhaseStatus::Cancelled,
            summary: safe_summary(summary.into()),
            artifact_refs: Vec::new(),
            evidence_refs: Vec::new(),
            progress: None,
            stop_reason: Some(StopReason::UserCancelled),
            error_code: Some("cancelled".to_string()),
            retryable: false,
            idempotent: true,
        }
    }

    pub(crate) fn exhausted(phase_id: PhaseId, summary: impl Into<String>) -> Self {
        Self {
            phase_id,
            status: PhaseStatus::Exhausted,
            summary: safe_summary(summary.into()),
            artifact_refs: Vec::new(),
            evidence_refs: Vec::new(),
            progress: None,
            stop_reason: Some(StopReason::BudgetExhausted),
            error_code: Some("budget_exhausted".to_string()),
            retryable: false,
            idempotent: true,
        }
    }

    pub(crate) fn validate(&self, definition: &PhaseDefinition) -> Result<(), OrchestratorError> {
        if self.phase_id != definition.phase_id
            || self.summary.is_empty()
            || self.summary.len() > 512
            || self.summary.chars().any(char::is_control)
            || safe_summary(self.summary.clone()) != self.summary
        {
            return Err(OrchestratorError::InvalidPhaseOutput {
                reason: "phase identity or summary is invalid".to_string(),
            });
        }
        if !matches!(
            self.status,
            PhaseStatus::Succeeded
                | PhaseStatus::Skipped
                | PhaseStatus::Waiting
                | PhaseStatus::Failed
                | PhaseStatus::Cancelled
                | PhaseStatus::Exhausted
        ) {
            return Err(OrchestratorError::InvalidPhaseOutput {
                reason: "phase output must be terminal or waiting".to_string(),
            });
        }
        if matches!(
            self.status,
            PhaseStatus::Waiting
                | PhaseStatus::Failed
                | PhaseStatus::Cancelled
                | PhaseStatus::Exhausted
        ) && self.stop_reason.is_none()
        {
            return Err(OrchestratorError::InvalidPhaseOutput {
                reason: "stopped phase output must include a stop reason".to_string(),
            });
        }
        if matches!(self.status, PhaseStatus::Succeeded | PhaseStatus::Skipped)
            && self.stop_reason.is_some()
        {
            return Err(OrchestratorError::InvalidPhaseOutput {
                reason: "successful phase output cannot include a stop reason".to_string(),
            });
        }
        if self.status == PhaseStatus::Failed && self.error_code.is_none() {
            return Err(OrchestratorError::InvalidPhaseOutput {
                reason: "failed phase output must include a stable error code".to_string(),
            });
        }
        if self.status != PhaseStatus::Failed && self.retryable {
            return Err(OrchestratorError::InvalidPhaseOutput {
                reason: "only failed phase output may be retryable".to_string(),
            });
        }
        if self
            .error_code
            .as_deref()
            .is_some_and(|code| safe_error_code(code) != code)
        {
            return Err(OrchestratorError::InvalidPhaseOutput {
                reason: "phase error code is not safe".to_string(),
            });
        }
        if self.artifact_refs.len() > MAX_EVENT_REFERENCES
            || self.evidence_refs.len() > MAX_EVENT_REFERENCES
            || duplicate_ids(&self.artifact_refs)
            || duplicate_ids(&self.evidence_refs)
        {
            return Err(OrchestratorError::InvalidPhaseOutput {
                reason: "phase output contains duplicate references".to_string(),
            });
        }
        if definition.output_artifact_kinds.is_empty() && !self.artifact_refs.is_empty() {
            return Err(OrchestratorError::UndeclaredArtifactKind {
                phase: definition.phase_id.to_string(),
                kind: "unspecified".to_string(),
            });
        }
        Ok(())
    }
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub(crate) enum OrchestratorEventKind {
    Started,
    PhaseReady,
    PhaseStarted,
    PhaseCompleted,
    PhaseSkipped,
    WaitingForUser,
    PhaseFailed,
    PhaseCancelled,
    PhaseExhausted,
    NoProgress,
    Resumed,
    SessionCompleted,
    SessionCancelled,
    SessionFailed,
    SessionBudgetExhausted,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub(crate) struct OrchestratorEvent {
    pub(crate) schema_version: u32,
    pub(crate) event_id: TraceEventId,
    pub(crate) session_id: AgentSessionId,
    pub(crate) sequence: u64,
    pub(crate) timestamp: String,
    pub(crate) kind: OrchestratorEventKind,
    pub(crate) phase: PhaseId,
    pub(crate) status: PhaseStatus,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) attempt_id: Option<AttemptId>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub(crate) artifact_refs: Vec<ArtifactId>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub(crate) evidence_refs: Vec<EvidenceId>,
    pub(crate) summary: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) progress: Option<ProgressSignal>,
    #[serde(default)]
    pub(crate) terminal: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) terminal_status: Option<AgentSessionStatus>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) error_code: Option<String>,
    #[serde(default)]
    pub(crate) retryable: bool,
    #[serde(default)]
    pub(crate) evidence_limitation: bool,
}

impl OrchestratorEvent {
    pub(crate) fn interactive_summary(&self) -> String {
        let suffix = if self.terminal { " (terminal)" } else { "" };
        format!("[{}] {}{}", self.phase, self.summary, suffix)
    }

    pub(crate) fn validate(&self) -> Result<(), OrchestratorError> {
        if self.schema_version != ORCHESTRATOR_EVENT_SCHEMA_VERSION
            || self.sequence == 0
            || self.timestamp.is_empty()
            || DateTime::parse_from_rfc3339(&self.timestamp).is_err()
            || self.summary.is_empty()
            || self.summary.len() > 512
            || self.summary.chars().any(char::is_control)
            || redact_text(&self.summary) != self.summary
            || self.terminal && self.terminal_status.is_none()
            || self.artifact_refs.len() > MAX_EVENT_REFERENCES
            || self.evidence_refs.len() > MAX_EVENT_REFERENCES
            || duplicate_ids(&self.artifact_refs)
            || duplicate_ids(&self.evidence_refs)
            || self
                .error_code
                .as_deref()
                .is_some_and(|code| safe_error_code(code) != code)
        {
            return Err(OrchestratorError::MalformedState);
        }
        if self.terminal && self.status == PhaseStatus::Running {
            return Err(OrchestratorError::MalformedState);
        }
        Ok(())
    }
}

#[derive(Debug, Clone)]
pub(crate) struct PhaseRequest {
    pub(crate) session: AgentSession,
    pub(crate) workflow: WorkflowDefinition,
    pub(crate) phase: PhaseDefinition,
    pub(crate) phase_state: PhaseState,
    pub(crate) attempt_id: AttemptId,
    pub(crate) context: super::context::ContextBuildResult,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct OrchestratorStepSummary {
    pub(crate) terminal: bool,
    pub(crate) waiting: bool,
    pub(crate) progress: bool,
}

#[derive(Debug, Clone)]
pub(crate) struct OrchestratorStep {
    pub(crate) session: AgentSession,
    pub(crate) phase: PhaseId,
    pub(crate) phase_status: PhaseStatus,
    pub(crate) result: Option<PhaseResult>,
    pub(crate) context: Option<super::context::ContextBuildResult>,
    pub(crate) next_phase: Option<PhaseId>,
    pub(crate) summary: OrchestratorStepSummary,
}

impl OrchestratorStep {
    pub(crate) fn is_terminal(&self) -> bool {
        self.summary.terminal
    }
}

fn duplicate_ids<T: Ord + Clone>(values: &[T]) -> bool {
    let mut values = values.to_vec();
    values.sort();
    values.windows(2).any(|window| window[0] == window[1])
}

fn safe_summary(value: String) -> String {
    let value = redact_text(&value);
    let mut result = value.chars().take(512).collect::<String>();
    if result.is_empty() {
        result = "phase completed".to_string();
    }
    result
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

pub(crate) fn progress_fingerprint(phase: PhaseId, result: &PhaseResult) -> Option<String> {
    result.progress.map(|progress| {
        let value = serde_json::json!({
            "phase": phase.as_str(),
            "progress": progress,
            "artifacts": result.artifact_refs,
            "evidence": result.evidence_refs,
        });
        hash_bytes("sha256:", &canonical_bytes(&value))
    })
}

fn is_safe_identifier(value: &str) -> bool {
    !value.is_empty()
        && value.len() <= 128
        && value != "."
        && value != ".."
        && value.bytes().all(|byte| {
            byte.is_ascii_lowercase() || byte.is_ascii_digit() || matches!(byte, b'-' | b'_' | b'.')
        })
        && value.as_bytes()[0].is_ascii_alphanumeric()
}

fn has_duplicates<T: Ord + Clone>(values: &[T]) -> bool {
    let mut values = values.to_vec();
    values.sort();
    values.windows(2).any(|window| window[0] == window[1])
}
