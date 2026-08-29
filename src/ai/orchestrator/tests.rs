use super::*;
use crate::ai::conversation::{
    Conversation, ConversationMessage, ConversationMessageRole, DurableContext,
};
use crate::ai::session::{
    AgentBudget, AgentBudgetLimits, AgentPhase, AgentSessionId, ArtifactId, ArtifactKind,
    ArtifactWriteOptions, BudgetError, EvidenceKind, EvidenceRecord, EvidenceSource,
    EvidenceSourceKind, EvidenceStatus, SessionService, SessionSpec, SessionStore, StopReason,
};
use serde_json::json;
use std::fs;
use std::path::PathBuf;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{SystemTime, UNIX_EPOCH};

fn temporary_root(name: &str) -> PathBuf {
    let stamp = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("the system clock should be after the Unix epoch")
        .as_nanos();
    std::env::temp_dir().join(format!(
        "gib-orchestrator-{name}-{}-{stamp}",
        std::process::id()
    ))
}

fn budget() -> AgentBudget {
    AgentBudget::new(AgentBudgetLimits {
        wall_clock_deadline: None,
        max_model_calls: 32,
        max_output_tokens: 256,
        max_tool_calls: 32,
        max_search_actions: 32,
        max_candidates: 256,
        max_context_bytes: 32 * 1024,
        max_context_tokens: 8 * 1024,
        max_retries: 4,
        max_artifact_bytes: 1024 * 1024,
        max_evidence_bytes: 1024 * 1024,
    })
    .expect("test budget should be valid")
}

fn spec(session_id: &str, workflow_id: &str) -> SessionSpec {
    SessionSpec {
        session_id: Some(
            AgentSessionId::from_string(session_id).expect("test session ID should be valid"),
        ),
        turn_id: crate::ai::session::TurnId::from_string("turn-orchestrator")
            .expect("test turn ID should be valid"),
        conversation_id: "conversation-orchestrator".to_string(),
        workflow_id: workflow_id.to_string(),
        workflow_version: "1".to_string(),
        runtime_profile: crate::ai::profiles::RuntimeProfile::Balanced,
        budget: budget(),
    }
}

fn service(name: &str, registry: WorkflowRegistry) -> (OrchestratorService, PathBuf) {
    let root = temporary_root(name);
    let sessions = SessionService::new(SessionStore::from_root(&root));
    let service = OrchestratorService::new(registry, sessions, ContextBuilder::default());
    (service, root)
}

#[test]
fn workflow_graph_rejects_cycles_and_orders_ready_phases_by_definition() {
    let cycle = WorkflowDefinition::new(
        WorkflowId::new("cycle").expect("workflow ID should be valid"),
        "1",
        vec![IntentKind::Locate],
        vec![
            PhaseDefinition::new(AgentPhase::Classify).with_prerequisites(vec![AgentPhase::Plan]),
            PhaseDefinition::new(AgentPhase::Plan).with_prerequisites(vec![AgentPhase::Classify]),
        ],
    );
    assert!(matches!(
        cycle.validate(),
        Err(OrchestratorError::DependencyCycle)
    ));

    let workflow = WorkflowDefinition::new(
        WorkflowId::new("ordering").expect("workflow ID should be valid"),
        "1",
        vec![IntentKind::Locate],
        vec![
            PhaseDefinition::new(AgentPhase::Classify),
            PhaseDefinition::new(AgentPhase::Explain),
            PhaseDefinition::new(AgentPhase::Plan),
        ],
    );
    workflow
        .validate()
        .expect("acyclic workflow should validate");
    let mut state = workflow.initial_state(
        AgentSessionId::from_string("session-ordering").expect("session ID should be valid"),
    );
    workflow.refresh_ready(&mut state);
    assert_eq!(
        workflow.ready_phases(&state),
        vec![AgentPhase::Classify, AgentPhase::Explain, AgentPhase::Plan]
    );
}

#[test]
fn context_builder_is_deterministic_bounded_and_trust_aware() {
    let conversation = Conversation {
        schema_version: 1,
        conversation_id: "conversation-context".to_string(),
        title: "Context test".to_string(),
        created_at: "2026-01-01T00:00:00Z".to_string(),
        updated_at: "2026-01-01T00:00:00Z".to_string(),
        revision: 1,
        model: None,
        prompt: None,
        messages: vec![
            ConversationMessage::new(
                "message-user".to_string(),
                ConversationMessageRole::User,
                "2026-01-01T00:00:00Z".to_string(),
                "find the older report".to_string(),
            ),
            ConversationMessage::new(
                "message-assistant".to_string(),
                ConversationMessageRole::Assistant,
                "2026-01-01T00:00:01Z".to_string(),
                "hidden reasoning: I will inspect verified catalog entries".to_string(),
            ),
        ],
        durable_context: DurableContext {
            summary: Some("The user prefers concise answers".to_string()),
            user_preferences: [("format".to_string(), "concise".to_string())]
                .into_iter()
                .collect(),
            artifact_refs: vec!["artifact-context".to_string()],
            evidence_refs: vec!["evidence-context".to_string()],
            facts: vec!["catalog status is retained".to_string()],
        },
        archived: false,
    };
    let evidence = EvidenceRecord::fact(
        EvidenceKind::CatalogEntry,
        EvidenceSource::new(EvidenceSourceKind::Catalog, "catalog-context"),
        "the catalog contains a verified report",
        EvidenceStatus::Observed,
        crate::ai::session::ConfidenceQualifier::High,
    )
    .expect("evidence should be created");
    let inputs = ContextInputs {
        conversation: Some(conversation),
        current_request: Some("find the older report".to_string()),
        normalized_goal: Some("older report".to_string()),
        available_capabilities: vec!["catalog-read".to_string(), "catalog-read".to_string()],
        hypotheses: vec!["the report is in the latest indexed backup".to_string()],
        evidence: vec![evidence],
        limitations: vec!["one backup is pending indexing".to_string()],
        ..ContextInputs::default()
    };
    let builder = ContextBuilder::new(ContextLimits {
        max_bytes: 600,
        max_tokens: 150,
        max_items: 8,
        max_item_bytes: 220,
        max_messages: 2,
    })
    .expect("context limits should be valid");
    let first = builder
        .build(ContextRole::HistoryExplanation, &inputs)
        .expect("context should build");
    let second = builder
        .build(ContextRole::HistoryExplanation, &inputs)
        .expect("context should build deterministically");
    assert_eq!(first, second);
    assert!(first.byte_size <= 600);
    assert!(first.token_estimate <= 150);
    assert!(first.omitted_item_count > 0 || first.truncated);
    assert!(
        first
            .items
            .iter()
            .any(|item| item.trust == TrustClass::Authoritative)
    );
    assert!(
        first
            .warnings
            .iter()
            .all(|warning| !warning.message.contains("hidden"))
    );
    assert!(
        !serde_json::to_string(&first)
            .expect("context should serialize")
            .contains("hidden reasoning")
    );
}

#[test]
fn locate_workflow_runs_to_completion_with_fake_executor() {
    let registry = WorkflowRegistry::with_builtins().expect("built-in workflows should validate");
    let (service, root) = service("locate", registry);
    let session = service
        .start(
            spec("session-locate", "locate"),
            IntentKind::Locate,
            Vec::new(),
        )
        .expect("locate session should start");
    let executor = ScriptedPhaseExecutor::new();
    let completed = service
        .run_to_completion(&session.session_id, ContextInputs::default(), &executor)
        .expect("locate workflow should complete");
    assert_eq!(
        completed.status,
        crate::ai::session::AgentSessionStatus::Completed
    );
    assert_eq!(completed.phase, AgentPhase::Complete);
    let events = service
        .events(&session.session_id)
        .expect("events should load");
    assert!(
        events
            .iter()
            .any(|event| event.kind == OrchestratorEventKind::PhaseStarted)
    );
    assert!(events.last().is_some_and(|event| event.terminal));
    assert!(
        events
            .windows(2)
            .all(|events| events[0].sequence < events[1].sequence)
    );
    let state_file = root.join("orchestrator").join("session-locate.json");
    let encoded = fs::read_to_string(state_file).expect("orchestrator state should persist");
    assert!(!encoded.contains("find the older report"));
    let _ = fs::remove_dir_all(root);
}

#[test]
fn explain_history_workflow_runs_with_the_same_deterministic_executor_contract() {
    let registry = WorkflowRegistry::with_builtins().expect("built-in workflows should validate");
    let (service, root) = service("explain-history", registry);
    let session = service
        .start(
            spec("session-explain-history", "explain-history"),
            IntentKind::ExplainHistory,
            Vec::new(),
        )
        .expect("history session should start");
    let completed = service
        .run_to_completion(
            &session.session_id,
            ContextInputs {
                current_request: Some("what changed in the report?".to_string()),
                normalized_goal: Some("explain report history".to_string()),
                ..ContextInputs::default()
            },
            &ScriptedPhaseExecutor::new(),
        )
        .expect("history workflow should complete");
    assert_eq!(
        completed.status,
        crate::ai::session::AgentSessionStatus::Completed
    );
    assert_eq!(completed.phase, AgentPhase::Complete);
    assert!(
        service
            .events(&session.session_id)
            .expect("events should load")
            .iter()
            .any(|event| event.kind == OrchestratorEventKind::SessionCompleted)
    );
    let _ = fs::remove_dir_all(root);
}

#[test]
fn output_references_are_committed_without_interrupting_the_active_attempt() {
    let mut registry = WorkflowRegistry::new();
    registry
        .register(WorkflowDefinition::new(
            WorkflowId::new("artifact-output").expect("workflow ID should be valid"),
            "1",
            vec![IntentKind::Locate],
            vec![
                PhaseDefinition::new(AgentPhase::Classify)
                    .with_artifacts(Vec::new(), vec![ArtifactKind::ExplanationContext]),
            ],
        ))
        .expect("workflow should register");
    let (service, root) = service("artifact-output", registry);
    let session = service
        .start(
            spec("session-artifact-output", "artifact-output"),
            IntentKind::Locate,
            Vec::new(),
        )
        .expect("session should start");
    let artifact = service
        .sessions()
        .artifact_store()
        .create(
            ArtifactKind::ExplanationContext,
            AgentPhase::Classify,
            None,
            &json!({"verified": true}),
            ArtifactWriteOptions::default(),
        )
        .expect("artifact should be created");
    let artifact_id = artifact.header.artifact_id;
    let output_artifact_id = artifact_id.clone();
    let executor = move |request: &PhaseRequest| {
        Ok(PhaseResult {
            phase_id: request.phase.phase_id,
            status: PhaseStatus::Succeeded,
            summary: "verified output".to_string(),
            artifact_refs: vec![output_artifact_id.clone()],
            evidence_refs: Vec::new(),
            progress: Some(ProgressSignal::NewVerifiedFact),
            stop_reason: None,
            error_code: None,
            retryable: false,
            idempotent: true,
        })
    };
    let step = service
        .step(&session.session_id, ContextInputs::default(), &executor)
        .expect("phase output should be accepted");
    assert_eq!(
        step.session.status,
        crate::ai::session::AgentSessionStatus::Completed
    );
    assert!(step.session.artifact_refs.contains(&artifact_id));
    assert!(
        step.session
            .attempts
            .iter()
            .any(|attempt| attempt.outcome == crate::ai::session::AttemptOutcome::Succeeded)
    );
    let _ = fs::remove_dir_all(root);
}

#[test]
fn phase_failure_retries_only_within_the_declared_budget() {
    let mut registry = WorkflowRegistry::new();
    let workflow = WorkflowDefinition::new(
        WorkflowId::new("retry").expect("workflow ID should be valid"),
        "1",
        vec![IntentKind::Locate],
        vec![PhaseDefinition::new(AgentPhase::Classify)],
    );
    registry
        .register(workflow)
        .expect("workflow should register");
    let (service, root) = service("retry", registry);
    let session = service
        .start(
            spec("session-retry", "retry"),
            IntentKind::Locate,
            Vec::new(),
        )
        .expect("retry session should start");
    let calls = Arc::new(AtomicUsize::new(0));
    let calls_for_executor = Arc::clone(&calls);
    let executor = move |request: &PhaseRequest| {
        if calls_for_executor.fetch_add(1, Ordering::SeqCst) == 0 {
            Err(PhaseExecutionError::new("temporary_catalog_failure", true))
        } else {
            Ok(PhaseResult::succeeded(
                request.phase.phase_id,
                "verified classification",
                Some(ProgressSignal::NewVerifiedFact),
            ))
        }
    };
    let first = service
        .step(&session.session_id, ContextInputs::default(), &executor)
        .expect("retryable failure should be represented as a step");
    assert_eq!(first.phase_status, PhaseStatus::Ready);
    let second = service
        .step(&session.session_id, ContextInputs::default(), &executor)
        .expect("retry should succeed");
    assert!(second.next_phase.is_none());
    assert_eq!(
        second.session.status,
        crate::ai::session::AgentSessionStatus::Completed
    );
    assert_eq!(calls.load(Ordering::SeqCst), 2);
    let _ = fs::remove_dir_all(root);
}

#[test]
fn phase_status_transition_table_covers_terminal_and_recovery_edges() {
    assert!(PhaseStatus::Pending.can_transition_to(PhaseStatus::Ready));
    assert!(PhaseStatus::Ready.can_transition_to(PhaseStatus::Running));
    assert!(PhaseStatus::Running.can_transition_to(PhaseStatus::Succeeded));
    assert!(PhaseStatus::Running.can_transition_to(PhaseStatus::Ready));
    assert!(PhaseStatus::Running.can_transition_to(PhaseStatus::Waiting));
    assert!(PhaseStatus::Waiting.can_transition_to(PhaseStatus::Ready));
    assert!(PhaseStatus::Ready.can_transition_to(PhaseStatus::Cancelled));
    assert!(PhaseStatus::Ready.can_transition_to(PhaseStatus::Failed));
    assert!(PhaseStatus::Running.can_transition_to(PhaseStatus::Exhausted));
    assert!(!PhaseStatus::Succeeded.can_transition_to(PhaseStatus::Ready));
    assert!(!PhaseStatus::Failed.can_transition_to(PhaseStatus::Succeeded));
}

#[test]
fn waiting_confirmation_resumes_at_a_safe_boundary() {
    let mut registry = WorkflowRegistry::new();
    let workflow = WorkflowDefinition::new(
        WorkflowId::new("waiting").expect("workflow ID should be valid"),
        "1",
        vec![IntentKind::Locate],
        vec![PhaseDefinition::new(AgentPhase::Classify)],
    );
    registry
        .register(workflow)
        .expect("workflow should register");
    let (service, root) = service("waiting", registry);
    let session = service
        .start(
            spec("session-waiting", "waiting"),
            IntentKind::Locate,
            Vec::new(),
        )
        .expect("waiting session should start");
    let executor = ScriptedPhaseExecutor::new();
    executor.push_result(PhaseResult::waiting(
        AgentPhase::Classify,
        "confirmation is required",
        StopReason::SafetyConfirmationRequired,
    ));
    let waiting = service
        .step(&session.session_id, ContextInputs::default(), &executor)
        .expect("phase should wait");
    assert!(waiting.summary.waiting);
    assert_eq!(
        waiting.session.status,
        crate::ai::session::AgentSessionStatus::WaitingForUser
    );
    let resumed = service
        .resume(&session.session_id, waiting.session.revision)
        .expect("session should resume");
    let completed = service
        .run_to_completion(&session.session_id, ContextInputs::default(), &executor)
        .expect("resumed phase should complete");
    assert_eq!(
        resumed.status,
        crate::ai::session::AgentSessionStatus::Running
    );
    assert_eq!(
        completed.status,
        crate::ai::session::AgentSessionStatus::Completed
    );
    let _ = fs::remove_dir_all(root);
}

#[test]
fn expired_deadline_stops_before_invoking_the_executor() {
    let mut registry = WorkflowRegistry::new();
    registry
        .register(WorkflowDefinition::new(
            WorkflowId::new("deadline").expect("workflow ID should be valid"),
            "1",
            vec![IntentKind::Locate],
            vec![PhaseDefinition::new(AgentPhase::Classify)],
        ))
        .expect("workflow should register");
    let root = temporary_root("deadline");
    let sessions = SessionService::new(SessionStore::from_root(&root));
    let service = OrchestratorService::new(registry, sessions, ContextBuilder::default());
    let mut session_spec = spec("session-deadline", "deadline");
    session_spec.budget = AgentBudget::new(AgentBudgetLimits {
        wall_clock_deadline: Some("1970-01-01T00:00:00Z".to_string()),
        ..session_spec.budget.limits()
    })
    .expect("expired deadline should be representable");
    let session = service
        .start(session_spec, IntentKind::Locate, Vec::new())
        .expect("session should start before the operation boundary");
    let invoked = Arc::new(AtomicUsize::new(0));
    let invoked_for_executor = Arc::clone(&invoked);
    let executor = move |_request: &PhaseRequest| {
        invoked_for_executor.fetch_add(1, Ordering::SeqCst);
        Ok(PhaseResult::succeeded(
            AgentPhase::Classify,
            "should not run",
            Some(ProgressSignal::NewVerifiedFact),
        ))
    };
    let step = service
        .step(&session.session_id, ContextInputs::default(), &executor)
        .expect("deadline should become a structured terminal step");
    assert_eq!(step.phase_status, PhaseStatus::Exhausted);
    assert_eq!(invoked.load(Ordering::SeqCst), 0);
    assert_eq!(step.session.stop_reason, Some(StopReason::BudgetExhausted));
    let _ = fs::remove_dir_all(root);
}

#[test]
fn event_sink_receives_bounded_ordered_events() {
    let registry = WorkflowRegistry::with_builtins().expect("built-in workflows should validate");
    let root = temporary_root("events");
    let sessions = SessionService::new(SessionStore::from_root(&root));
    let seen = Arc::new(Mutex::new(Vec::new()));
    let seen_for_sink = Arc::clone(&seen);
    let service = OrchestratorService::new(registry, sessions, ContextBuilder::default())
        .with_event_sink(Arc::new(move |event| {
            seen_for_sink
                .lock()
                .expect("event lock should work")
                .push(event.clone());
        }));
    let session = service
        .start(
            spec("session-events", "locate"),
            IntentKind::Locate,
            Vec::new(),
        )
        .expect("session should start");
    let executor = ScriptedPhaseExecutor::new();
    service
        .run_to_completion(&session.session_id, ContextInputs::default(), &executor)
        .expect("workflow should complete");
    let delivered = seen.lock().expect("event lock should work").clone();
    let stored = service
        .events(&session.session_id)
        .expect("events should load");
    assert_eq!(delivered, stored);
    assert!(
        delivered
            .windows(2)
            .all(|events| events[0].sequence < events[1].sequence)
    );
    assert!(delivered.iter().all(|event| event.summary.len() <= 512));
    let _ = fs::remove_dir_all(root);
}

#[test]
fn invalid_typed_output_is_rejected_without_marking_success() {
    let mut registry = WorkflowRegistry::new();
    registry
        .register(WorkflowDefinition::new(
            WorkflowId::new("invalid-output").expect("workflow ID should be valid"),
            "1",
            vec![IntentKind::Locate],
            vec![PhaseDefinition::new(AgentPhase::Classify)],
        ))
        .expect("workflow should register");
    let (service, root) = service("invalid-output", registry);
    let session = service
        .start(
            spec("session-invalid-output", "invalid-output"),
            IntentKind::Locate,
            Vec::new(),
        )
        .expect("session should start");
    let executor = |_request: &PhaseRequest| {
        Ok(PhaseResult::succeeded(
            AgentPhase::Plan,
            "wrong phase",
            Some(ProgressSignal::NewVerifiedFact),
        ))
    };
    let step = service
        .step(&session.session_id, ContextInputs::default(), &executor)
        .expect("invalid output should be represented as a terminal failure");
    assert_eq!(
        step.session.status,
        crate::ai::session::AgentSessionStatus::Failed
    );
    assert_eq!(step.phase_status, PhaseStatus::Failed);
    assert!(
        service
            .events(&session.session_id)
            .expect("events should load")
            .iter()
            .any(|event| event.error_code.as_deref() == Some("invalid_phase_output"))
    );
    let _ = fs::remove_dir_all(root);
}

#[test]
fn stopped_phase_preserves_the_declared_stop_reason() {
    let mut registry = WorkflowRegistry::new();
    registry
        .register(WorkflowDefinition::new(
            WorkflowId::new("stop-reason").expect("workflow ID should be valid"),
            "1",
            vec![IntentKind::Locate],
            vec![PhaseDefinition::new(AgentPhase::Classify)],
        ))
        .expect("workflow should register");
    let (service, root) = service("stop-reason", registry);
    let session = service
        .start(
            spec("session-stop-reason", "stop-reason"),
            IntentKind::Locate,
            Vec::new(),
        )
        .expect("session should start");
    let executor = |_request: &PhaseRequest| {
        Ok(PhaseResult::failed_with_reason(
            AgentPhase::Classify,
            "available evidence is insufficient",
            "evidence_insufficient",
            false,
            StopReason::EvidenceInsufficient,
        ))
    };
    let step = service
        .step(&session.session_id, ContextInputs::default(), &executor)
        .expect("stop reason should be represented as a terminal step");
    assert_eq!(
        step.session.stop_reason,
        Some(StopReason::EvidenceInsufficient)
    );
    assert!(
        service
            .events(&session.session_id)
            .expect("events should load")
            .iter()
            .any(|event| event.evidence_limitation)
    );
    let _ = fs::remove_dir_all(root);
}

#[test]
fn repeated_no_progress_stops_before_a_false_success() {
    let mut registry = WorkflowRegistry::new();
    registry
        .register(WorkflowDefinition::new(
            WorkflowId::new("no-progress").expect("workflow ID should be valid"),
            "1",
            vec![IntentKind::Locate],
            vec![
                PhaseDefinition::new(AgentPhase::Classify),
                PhaseDefinition::new(AgentPhase::Plan)
                    .with_prerequisites(vec![AgentPhase::Classify]),
                PhaseDefinition::new(AgentPhase::Analyze)
                    .with_prerequisites(vec![AgentPhase::Plan]),
            ],
        ))
        .expect("workflow should register");
    let (service, root) = service("no-progress", registry);
    let session = service
        .start(
            spec("session-no-progress", "no-progress"),
            IntentKind::Locate,
            Vec::new(),
        )
        .expect("session should start");
    let executor = |request: &PhaseRequest| {
        Ok(PhaseResult::succeeded(
            request.phase.phase_id,
            "same bounded result",
            None,
        ))
    };
    let completed = service
        .run_to_completion(&session.session_id, ContextInputs::default(), &executor)
        .expect("no-progress stopping should be represented as a terminal session");
    assert_eq!(
        completed.status,
        crate::ai::session::AgentSessionStatus::Failed
    );
    assert_eq!(completed.stop_reason, Some(StopReason::InternalError));
    assert!(
        service
            .events(&session.session_id)
            .expect("events should load")
            .iter()
            .any(|event| event.kind == OrchestratorEventKind::NoProgress)
    );
    let _ = fs::remove_dir_all(root);
}

#[test]
fn interrupted_attempts_resume_and_missing_artifacts_are_rejected() {
    let mut registry = WorkflowRegistry::new();
    registry
        .register(WorkflowDefinition::new(
            WorkflowId::new("resume").expect("workflow ID should be valid"),
            "1",
            vec![IntentKind::Locate],
            vec![PhaseDefinition::new(AgentPhase::Classify)],
        ))
        .expect("workflow should register");
    let (service, root) = service("resume", registry);
    let session = service
        .start(
            spec("session-resume", "resume"),
            IntentKind::Locate,
            Vec::new(),
        )
        .expect("session should start");
    let current = service
        .sessions()
        .load(session.session_id.as_str())
        .expect("session should load");
    service
        .sessions()
        .begin_attempt(
            current.session_id.as_str(),
            current.revision,
            "phase.classify",
            &json!({ "workflow": "resume" }),
        )
        .expect("simulated process should start an attempt");
    let executor = ScriptedPhaseExecutor::new();
    let resumed = service
        .run_to_completion(&session.session_id, ContextInputs::default(), &executor)
        .expect("an interrupted read-only attempt should resume");
    assert_eq!(
        resumed.status,
        crate::ai::session::AgentSessionStatus::Completed
    );
    assert!(
        resumed
            .attempts
            .iter()
            .any(|attempt| attempt.outcome == crate::ai::session::AttemptOutcome::Interrupted)
    );

    let missing_session = service
        .start(
            spec("session-missing-artifact", "resume"),
            IntentKind::Locate,
            Vec::new(),
        )
        .expect("second session should start");
    let mut state = service
        .load_state(&missing_session.session_id)
        .expect("state should load");
    state
        .phase_mut(AgentPhase::Classify)
        .expect("classify phase should exist")
        .artifact_refs
        .push(ArtifactId::from_string("artifact-missing").expect("artifact ID should be valid"));
    service
        .persist_state_for_test(&state)
        .expect("test state should persist");
    let error = service
        .step(
            &missing_session.session_id,
            ContextInputs::default(),
            &ScriptedPhaseExecutor::new(),
        )
        .expect_err("missing referenced artifact must stop execution");
    assert!(matches!(error, OrchestratorError::MissingReference { .. }));
    let _ = fs::remove_dir_all(root);
}

#[test]
fn budget_error_type_remains_structured() {
    let error = BudgetError::DeadlineExceeded;
    let encoded = serde_json::to_value(error).expect("budget error should serialize");
    assert_eq!(encoded["code"], json!("deadline_exceeded"));
}
