#![allow(dead_code)]

mod context;
mod error;
mod model;
mod service;
mod workflow;

#[allow(unused_imports)]
pub(crate) use context::{
    CONTEXT_SCHEMA_VERSION, CatalogContextRecord, ContextBuildResult, ContextBuilder, ContextError,
    ContextInputs, ContextItem, ContextLimits, ContextRole, ContextSourceType, ContextWarning,
    TrustClass,
};
#[allow(unused_imports)]
pub(crate) use error::OrchestratorError;
#[allow(unused_imports)]
pub(crate) use model::{
    AllowedSideEffects, BudgetClass, IntentKind, MAX_EVENT_REFERENCES, MAX_NO_PROGRESS_STREAK,
    MAX_WORKFLOW_PARALLELISM, ORCHESTRATOR_EVENT_SCHEMA_VERSION, ORCHESTRATOR_STATE_SCHEMA_VERSION,
    OrchestratorEvent, OrchestratorEventKind, OrchestratorStep, OrchestratorStepSummary,
    PhaseDefinition, PhaseId, PhaseRequest, PhaseResult, PhaseState, PhaseStatus, ProgressSignal,
    WORKFLOW_SCHEMA_VERSION, WorkflowDefinition, WorkflowId, WorkflowState,
};
#[allow(unused_imports)]
pub(crate) use service::{
    OrchestratorEventSink, OrchestratorService, PhaseExecutionError, PhaseExecutor,
    ScriptedPhaseExecutor,
};
#[allow(unused_imports)]
pub(crate) use workflow::{
    WorkflowRegistry, confirmation_restore_workflow, explain_history_workflow, locate_workflow,
};

use std::sync::Arc;

/// Both CLI frontends consume the same structured event object. The
/// interactive adapter only changes presentation; it does not change event
/// contents or ordering.
pub(crate) fn output_event_sink() -> OrchestratorEventSink {
    Arc::new(|event| crate::output::emit_orchestrator_event(event))
}

#[cfg(test)]
mod tests;
