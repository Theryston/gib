use super::error::OrchestratorError;
use super::model::{
    AllowedSideEffects, BudgetClass, IntentKind, PhaseDefinition, WorkflowDefinition, WorkflowId,
};
use crate::ai::session::{AgentPhase, ArtifactKind};
use std::collections::BTreeMap;

/// Versioned workflow definitions are registered before a session starts and
/// are immutable for the lifetime of that session.
#[derive(Debug, Clone, Default)]
pub(crate) struct WorkflowRegistry {
    definitions: BTreeMap<(WorkflowId, String), WorkflowDefinition>,
}

impl WorkflowRegistry {
    pub(crate) fn new() -> Self {
        Self::default()
    }

    pub(crate) fn with_builtins() -> Result<Self, OrchestratorError> {
        let mut registry = Self::new();
        registry.register(locate_workflow())?;
        registry.register(explain_history_workflow())?;
        registry.register(confirmation_restore_workflow())?;
        Ok(registry)
    }

    pub(crate) fn register(
        &mut self,
        definition: WorkflowDefinition,
    ) -> Result<(), OrchestratorError> {
        definition.validate()?;
        let key = (definition.workflow_id.clone(), definition.version.clone());
        if self.definitions.contains_key(&key) {
            return Err(OrchestratorError::InvalidWorkflow {
                reason: "workflow version is already registered".to_string(),
            });
        }
        self.definitions.insert(key, definition);
        Ok(())
    }

    pub(crate) fn get(
        &self,
        workflow_id: &WorkflowId,
        version: &str,
    ) -> Option<&WorkflowDefinition> {
        self.definitions
            .get(&(workflow_id.clone(), version.to_string()))
    }

    pub(crate) fn require(
        &self,
        workflow_id: &WorkflowId,
        version: &str,
    ) -> Result<&WorkflowDefinition, OrchestratorError> {
        self.get(workflow_id, version)
            .ok_or_else(|| OrchestratorError::InvalidWorkflow {
                reason: "requested workflow version is not registered".to_string(),
            })
    }

    pub(crate) fn definitions(&self) -> Vec<WorkflowDefinition> {
        self.definitions.values().cloned().collect()
    }
}

pub(crate) fn locate_workflow() -> WorkflowDefinition {
    let workflow_id = WorkflowId::new("locate").expect("built-in workflow ID is valid");
    WorkflowDefinition::new(
        workflow_id,
        "1",
        vec![IntentKind::Locate],
        vec![
            PhaseDefinition::new(AgentPhase::Classify)
                .with_budget(BudgetClass::Model)
                .with_artifacts(Vec::new(), vec![ArtifactKind::ExplanationContext]),
            PhaseDefinition::new(AgentPhase::Plan)
                .with_prerequisites(vec![AgentPhase::Classify])
                .with_budget(BudgetClass::Model),
            PhaseDefinition::new(AgentPhase::Search)
                .with_prerequisites(vec![AgentPhase::Plan])
                .with_budget(BudgetClass::Search)
                .with_artifacts(Vec::new(), vec![ArtifactKind::CandidateSet]),
            PhaseDefinition::new(AgentPhase::Analyze)
                .with_prerequisites(vec![AgentPhase::Search])
                .with_budget(BudgetClass::Model),
            PhaseDefinition::new(AgentPhase::Explain)
                .with_prerequisites(vec![AgentPhase::Analyze])
                .with_budget(BudgetClass::Model)
                .with_artifacts(Vec::new(), vec![ArtifactKind::ExplanationContext]),
        ],
    )
}

pub(crate) fn explain_history_workflow() -> WorkflowDefinition {
    let workflow_id = WorkflowId::new("explain-history").expect("built-in workflow ID is valid");
    WorkflowDefinition::new(
        workflow_id,
        "1",
        vec![IntentKind::ExplainHistory],
        vec![
            PhaseDefinition::new(AgentPhase::Classify).with_budget(BudgetClass::Model),
            PhaseDefinition::new(AgentPhase::Plan)
                .with_prerequisites(vec![AgentPhase::Classify])
                .with_budget(BudgetClass::Model),
            PhaseDefinition::new(AgentPhase::Search)
                .with_prerequisites(vec![AgentPhase::Plan])
                .with_budget(BudgetClass::Search)
                .with_artifacts(Vec::new(), vec![ArtifactKind::NormalizedTimeline]),
            PhaseDefinition::new(AgentPhase::Analyze)
                .with_prerequisites(vec![AgentPhase::Search])
                .with_budget(BudgetClass::Model)
                .with_artifacts(Vec::new(), vec![ArtifactKind::ExplanationContext]),
            PhaseDefinition::new(AgentPhase::Explain)
                .with_prerequisites(vec![AgentPhase::Analyze])
                .with_budget(BudgetClass::Model)
                .with_artifacts(Vec::new(), vec![ArtifactKind::ExplanationContext]),
        ],
    )
}

#[allow(dead_code)]
pub(crate) fn confirmation_restore_workflow() -> WorkflowDefinition {
    let workflow_id = WorkflowId::new("restore").expect("built-in workflow ID is valid");
    WorkflowDefinition::new(
        workflow_id,
        "1",
        vec![IntentKind::Restore],
        vec![
            PhaseDefinition::new(AgentPhase::Classify).with_budget(BudgetClass::Model),
            PhaseDefinition::new(AgentPhase::Plan)
                .with_prerequisites(vec![AgentPhase::Classify])
                .with_budget(BudgetClass::Model),
            PhaseDefinition::new(AgentPhase::RestorePreview)
                .with_prerequisites(vec![AgentPhase::Plan])
                .with_budget(BudgetClass::Tool)
                .with_side_effects(AllowedSideEffects::Preview)
                .with_artifacts(Vec::new(), vec![ArtifactKind::RestorePreview]),
            PhaseDefinition::new(AgentPhase::Confirm)
                .with_prerequisites(vec![AgentPhase::RestorePreview])
                .with_side_effects(AllowedSideEffects::ConfirmationRequired),
            PhaseDefinition::new(AgentPhase::Commit)
                .with_prerequisites(vec![AgentPhase::Confirm])
                .with_side_effects(AllowedSideEffects::Commit),
            PhaseDefinition::new(AgentPhase::Verify)
                .with_prerequisites(vec![AgentPhase::Commit])
                .with_budget(BudgetClass::Tool),
        ],
    )
}
