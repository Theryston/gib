use super::*;
use serde_json::json;
use std::fs;
use std::path::PathBuf;
use std::sync::Arc;
use std::thread;
use std::time::{SystemTime, UNIX_EPOCH};

fn temporary_root(name: &str) -> PathBuf {
    let stamp = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("the system clock should be after the Unix epoch")
        .as_nanos();
    let root = std::env::temp_dir().join(format!(
        "gib-agent-session-{name}-{}-{stamp}",
        std::process::id()
    ));
    let _ = fs::remove_dir_all(&root);
    root
}

fn small_limits() -> AgentBudgetLimits {
    AgentBudgetLimits {
        wall_clock_deadline: None,
        max_model_calls: 8,
        max_output_tokens: 64,
        max_tool_calls: 8,
        max_search_actions: 8,
        max_candidates: 16,
        max_context_bytes: 1_024,
        max_context_tokens: 128,
        max_retries: 8,
        max_artifact_bytes: 4_096,
        max_evidence_bytes: 4_096,
    }
}

fn session_service(name: &str) -> (SessionService, PathBuf) {
    let root = temporary_root(name);
    let store = SessionStore::from_root(&root);
    (SessionService::new(store), root)
}

fn session_spec(session_id: &str) -> SessionSpec {
    SessionSpec {
        session_id: Some(
            AgentSessionId::from_string(session_id).expect("test session ID should be valid"),
        ),
        turn_id: TurnId::from_string("turn-test").expect("test turn ID should be valid"),
        conversation_id: "conversation-test".to_string(),
        workflow_id: "locate".to_string(),
        workflow_version: "1".to_string(),
        runtime_profile: crate::ai::profiles::RuntimeProfile::Balanced,
        budget: AgentBudget::new(small_limits()).expect("test budget should be valid"),
    }
}

#[test]
fn budget_consumption_is_exact_and_atomic_under_concurrency() {
    let mut limits = small_limits();
    limits.max_model_calls = 10;
    limits.max_output_tokens = 10;
    let budget = Arc::new(AgentBudget::new(limits).expect("budget should be valid"));
    let handles = (0..20)
        .map(|_| {
            let budget = Arc::clone(&budget);
            thread::spawn(move || budget.consume(BudgetCost::model_call(1)).is_ok())
        })
        .collect::<Vec<_>>();
    let accepted = handles
        .into_iter()
        .map(|handle| handle.join().expect("budget worker should finish"))
        .filter(|accepted| *accepted)
        .count();
    let snapshot = budget.snapshot().expect("budget snapshot should work");
    assert_eq!(accepted, 10);
    assert_eq!(snapshot.consumed.model_calls, 10);
    assert_eq!(snapshot.remaining.model_calls, 0);
    assert_eq!(snapshot.consumed.output_tokens, 10);
    assert_eq!(snapshot.remaining.output_tokens, 0);

    let before = snapshot;
    let error = budget
        .consume(BudgetCost {
            tool_calls: 1,
            output_tokens: 1,
            ..BudgetCost::default()
        })
        .expect_err("a zero-remaining output budget should reject the full charge");
    assert!(matches!(error, BudgetError::Exhausted { .. }));
    assert_eq!(
        budget.snapshot().expect("snapshot should work").consumed,
        before.consumed
    );
}

#[test]
fn budget_deadline_is_checked_at_the_operation_boundary() {
    let limits = small_limits().with_deadline("1970-01-01T00:00:00Z");
    let budget = AgentBudget::new(limits).expect("an expired deadline is still a valid budget");
    assert!(matches!(
        budget.consume(BudgetCost::tool_call()),
        Err(BudgetError::DeadlineExceeded)
    ));
}

#[test]
fn artifact_store_redacts_truncates_hashes_and_persists_durable_records() {
    let root = temporary_root("artifacts");
    let limits = ArtifactLimits {
        max_bytes: 180,
        max_count: 4,
        max_file_bytes: 4_096,
    };
    let store = ArtifactStore::from_root(&root).with_limits(limits);
    let header = ArtifactHeader::new(
        ArtifactKind::CatalogPage,
        AgentPhase::Search,
        None,
        ArtifactSensitivity::Internal,
        RetentionClass::Durable,
    )
    .expect("artifact header should be valid");
    let record = store
        .put_json(
            header,
            json!({
                "path": "/home/user/private.txt",
                "prompt_body": "do not persist this prompt",
                "description": "x".repeat(2_000)
            }),
        )
        .expect("artifact should be bounded");
    let encoded = serde_json::to_string(&record).expect("artifact should serialize");
    assert!(!encoded.contains("do not persist this prompt"));
    assert!(!encoded.contains("/home/user/private.txt"));
    assert!(record.header.truncation.truncated);
    assert!(record.header.content_hash.starts_with("sha256:"));
    assert!(record.header.byte_size <= limits.max_bytes as u64);
    assert_eq!(
        record.header.storage_status,
        ArtifactStorageStatus::Persisted
    );

    let reloaded = ArtifactStore::from_root(&root).with_limits(limits);
    let loaded = reloaded
        .load(record.header.artifact_id.as_str())
        .expect("durable artifact should be recoverable");
    assert_eq!(loaded.header.content_hash, record.header.content_hash);
    assert_eq!(reloaded.count().expect("artifact count should work"), 1);
    let _ = fs::remove_dir_all(root);
}

#[test]
fn evidence_requires_support_and_propagates_degraded_sources() {
    let ledger = EvidenceLedger::new(EvidenceLimits::default());
    let source = EvidenceSource::new(EvidenceSourceKind::Catalog, "catalog-1");
    let fact = ledger
        .append(
            EvidenceRecord::fact(
                EvidenceKind::CatalogEntry,
                source.clone(),
                "the catalog contains an entry",
                EvidenceStatus::Observed,
                ConfidenceQualifier::High,
            )
            .expect("fact should be constructed"),
        )
        .expect("fact should be appended");
    let degraded = ledger
        .append(
            EvidenceRecord::fact(
                EvidenceKind::CatalogEntry,
                source,
                "the catalog is missing some shards",
                EvidenceStatus::Degraded,
                ConfidenceQualifier::Low,
            )
            .expect("limitation fact should be constructed"),
        )
        .expect("degraded evidence should be appended");
    let inference = ledger
        .append(
            EvidenceRecord::inference(
                EvidenceKind::NormalizedEvent,
                EvidenceSource::new(EvidenceSourceKind::Tool, "tool-1"),
                "the entry probably changed during the interval",
                ConfidenceQualifier::Low,
                vec![fact.evidence_id.clone(), degraded.evidence_id.clone()],
            )
            .expect("inference should be constructed"),
        )
        .expect("inference should be appended");
    assert_eq!(inference.status, EvidenceStatus::Degraded);
    assert!(
        ledger
            .has_limitation()
            .expect("limitation query should work")
    );

    let missing = EvidenceId::from_string("evidence-missing").expect("test evidence ID");
    let error = ledger
        .append(
            EvidenceRecord::inference(
                EvidenceKind::NormalizedEvent,
                EvidenceSource::new(EvidenceSourceKind::Tool, "tool-2"),
                "unsupported inference",
                ConfidenceQualifier::Unknown,
                vec![missing],
            )
            .expect("inference should be constructed"),
        )
        .expect_err("an inference cannot cite missing support");
    assert!(matches!(
        error,
        EvidenceError::MissingSupportingEvidence { .. }
    ));
}

#[test]
fn fingerprints_are_stable_without_retaining_sensitive_arguments() {
    let first = json!({
        "path": "./folder\\file.txt",
        "extensions": ["TXT", "md", "txt"],
        "ordered": [2, 1],
        "password": "first-secret"
    });
    let second = json!({
        "password": "second-secret",
        "ordered": [2, 1],
        "extensions": ["md", "txt"],
        "path": "folder/file.txt"
    });
    let third = json!({"path": "folder/other.txt", "ordered": [2, 1]});
    assert_eq!(
        canonical_fingerprint(" Search ", &first),
        canonical_fingerprint("search", &second)
    );
    assert_ne!(
        canonical_fingerprint("search", &first),
        canonical_fingerprint("search", &third)
    );
    let attempt =
        AttemptLog::begin(AgentPhase::Search, "Search", &first).expect("attempt should be created");
    let encoded = serde_json::to_string(&attempt).expect("attempt should serialize");
    assert!(!encoded.contains("first-secret"));
    assert!(!encoded.contains("folder/file.txt"));
    assert!(!encoded.contains("prompt"));
    assert!(attempt.canonical_fingerprint.starts_with("sha256:"));
}

#[test]
fn service_validates_transitions_and_emits_one_terminal_event() {
    let (service, root) = session_service("lifecycle");
    let session = service
        .create(session_spec("session-lifecycle"))
        .expect("session should be created");
    let session = service
        .transition_phase(
            session.session_id.as_str(),
            session.revision,
            AgentPhase::Plan,
        )
        .expect("classify should transition to plan");
    assert!(matches!(
        service.transition_phase(
            session.session_id.as_str(),
            session.revision,
            AgentPhase::Classify
        ),
        Err(SessionError::InvalidPhaseTransition { .. })
    ));
    let session = service
        .wait_for_user(
            session.session_id.as_str(),
            session.revision,
            StopReason::SafetyConfirmationRequired,
        )
        .expect("session should wait for the user");
    let session = service
        .resume(session.session_id.as_str(), session.revision)
        .expect("session should resume");
    let session = service
        .complete(session.session_id.as_str(), session.revision)
        .expect("session should complete");
    assert_eq!(session.status, AgentSessionStatus::Completed);
    assert_eq!(
        session
            .trace
            .events
            .iter()
            .filter(|event| event.terminal)
            .count(),
        1
    );
    assert!(matches!(
        service.cancel(session.session_id.as_str(), session.revision),
        Err(SessionError::SessionTerminal)
    ));
    let encoded = fs::read_to_string(root.join("sessions").join("session-lifecycle.json"))
        .expect("session file should exist");
    assert!(!encoded.contains("hidden reasoning"));
    let _ = fs::remove_dir_all(root);
}

#[test]
fn in_flight_attempts_are_interrupted_on_recovery() {
    let (service, root) = session_service("recovery");
    let session = service
        .create(session_spec("session-recovery"))
        .expect("session should be created");
    let attempt = AttemptLog::begin(
        AgentPhase::Classify,
        "classify",
        &json!({"message": "not persisted"}),
    )
    .expect("attempt should start");
    let service_session = service
        .append_attempt(session.session_id.as_str(), session.revision, attempt)
        .expect("attempt should be recorded");
    let reloaded = SessionService::new(SessionStore::from_root(&root))
        .load(service_session.session_id.as_str())
        .expect("session should recover");
    assert_eq!(reloaded.attempts[0].outcome, AttemptOutcome::Interrupted);
    assert_eq!(
        reloaded.attempts[0].safe_error_code.as_deref(),
        Some("interrupted")
    );
    assert!(
        reloaded
            .trace
            .events
            .iter()
            .any(|event| event.kind == TraceEventKind::AttemptInterrupted)
    );
    let revision = reloaded.revision;
    let reloaded_again = SessionService::new(SessionStore::from_root(&root))
        .load(reloaded.session_id.as_str())
        .expect("recovered session should remain stable");
    assert_eq!(reloaded_again.revision, revision);
    let _ = fs::remove_dir_all(root);
}

#[test]
fn older_session_documents_are_migrated_on_the_next_write() {
    let root = temporary_root("migration");
    fs::create_dir_all(root.join("sessions")).expect("session directory should exist");
    fs::write(
        root.join("sessions").join("session-legacy.json"),
        serde_json::to_vec(&json!({
            "version": 0,
            "id": "session-legacy",
            "turn": "turn-legacy"
        }))
        .expect("legacy session should encode"),
    )
    .expect("legacy session should be written");
    let store = SessionStore::from_root(&root);
    let loaded = store
        .load_blocking("session-legacy")
        .expect("legacy session should migrate in memory");
    assert_eq!(loaded.schema_version, SESSION_SCHEMA_VERSION);
    store
        .mutate_blocking("session-legacy", loaded.revision, |_| Ok(()))
        .expect("the migrated session should be writable");
    let encoded = fs::read_to_string(root.join("sessions").join("session-legacy.json"))
        .expect("migrated session should be readable");
    assert!(encoded.contains("\"schema_version\": 1"));
    let _ = fs::remove_dir_all(root);
}

#[test]
fn phase_and_status_contract_uses_safe_gate_transitions() {
    assert_eq!(
        serde_json::to_value(AgentPhase::RestorePreview).expect("phase should serialize"),
        json!("restore-preview")
    );
    assert_eq!(
        serde_json::to_value(AgentSessionStatus::WaitingForUser).expect("status should serialize"),
        json!("waiting-for-user")
    );
    assert!(AgentPhase::RestorePreview.can_transition_to(AgentPhase::Confirm));
    assert!(!AgentPhase::RestorePreview.can_transition_to(AgentPhase::Complete));
    assert!(AgentPhase::Confirm.can_transition_to(AgentPhase::Commit));
    assert!(!AgentPhase::Confirm.can_transition_to(AgentPhase::Complete));
    assert!(AgentPhase::Commit.can_transition_to(AgentPhase::Verify));
    assert!(AgentPhase::Verify.can_transition_to(AgentPhase::Complete));
    assert!(AgentSessionStatus::WaitingForUser.can_transition_to(AgentSessionStatus::Running));
    assert!(!AgentSessionStatus::Completed.can_transition_to(AgentSessionStatus::Running));
}

#[test]
fn trace_is_monotonic_and_has_one_terminal_event() {
    let session_id = AgentSessionId::from_string("session-trace").expect("session ID should work");
    let mut trace = TraceLog::new();
    trace
        .append_new(
            &session_id,
            TraceEventKind::SessionStarted,
            AgentPhase::Classify,
            AgentSessionStatus::Running,
            "session started",
            None,
            Vec::new(),
            Vec::new(),
            None,
            false,
        )
        .expect("first event should append");
    trace
        .append_new(
            &session_id,
            TraceEventKind::PhaseChanged,
            AgentPhase::Plan,
            AgentSessionStatus::Running,
            "phase changed",
            None,
            Vec::new(),
            Vec::new(),
            None,
            false,
        )
        .expect("second event should append");
    trace
        .append_new(
            &session_id,
            TraceEventKind::SessionCancelled,
            AgentPhase::Plan,
            AgentSessionStatus::Cancelled,
            "cancelled",
            None,
            Vec::new(),
            Vec::new(),
            None,
            true,
        )
        .expect("terminal event should append");
    assert_eq!(
        trace
            .events
            .iter()
            .map(|event| event.sequence)
            .collect::<Vec<_>>(),
        vec![1, 2, 3]
    );
    trace.validate(&session_id).expect("trace should validate");
    assert!(matches!(
        trace.append_new(
            &session_id,
            TraceEventKind::PhaseChanged,
            AgentPhase::Plan,
            AgentSessionStatus::Cancelled,
            "after terminal",
            None,
            Vec::new(),
            Vec::new(),
            None,
            false,
        ),
        Err(TraceError::EventAfterTerminal)
    ));
}

#[test]
fn durable_evidence_is_enumerable_and_preserves_limitations() {
    let root = temporary_root("evidence-persistence");
    let ledger = EvidenceLedger::from_root(&root);
    let fact = ledger
        .append(
            EvidenceRecord::fact(
                EvidenceKind::CatalogEntry,
                EvidenceSource::new(EvidenceSourceKind::Catalog, "catalog-persisted"),
                "the catalog entry was observed",
                EvidenceStatus::Observed,
                ConfidenceQualifier::High,
            )
            .expect("fact should be constructed"),
        )
        .expect("fact should persist");
    let missing = ledger
        .append_missing_source(
            EvidenceKind::CatalogRevision,
            EvidenceSource::new(EvidenceSourceKind::Catalog, "/private/catalog"),
            "catalog revision was unavailable",
        )
        .expect("limitation should persist");
    let reloaded = EvidenceLedger::from_root(&root);
    let records = reloaded.all().expect("durable records should enumerate");
    assert_eq!(records.len(), 2);
    assert_eq!(
        reloaded
            .status_for(&[fact.evidence_id, missing.evidence_id])
            .expect("status should resolve"),
        EvidenceStatus::Unavailable
    );
    assert!(
        reloaded
            .has_limitation()
            .expect("limitation should resolve")
    );
    let encoded = serde_json::to_string(&records).expect("records should serialize");
    assert!(!encoded.contains("/private/catalog"));
    let _ = fs::remove_dir_all(root);
}

#[test]
fn artifact_and_evidence_count_limits_are_enforced() {
    let artifact_store = ArtifactStore::new(ArtifactLimits {
        max_bytes: 1_024,
        max_count: 1,
        max_file_bytes: 2_048,
    });
    artifact_store
        .create(
            ArtifactKind::CatalogSummary,
            AgentPhase::Analyze,
            None,
            &json!({"entry": "one"}),
            ArtifactWriteOptions::default(),
        )
        .expect("first artifact should fit");
    let artifact_error = artifact_store
        .create(
            ArtifactKind::CatalogSummary,
            AgentPhase::Analyze,
            None,
            &json!({"entry": "two"}),
            ArtifactWriteOptions::default(),
        )
        .expect_err("second artifact should exceed the count limit");
    assert!(matches!(
        artifact_error,
        ArtifactError::ArtifactCountExceeded { limit: 1 }
    ));

    let evidence_ledger = EvidenceLedger::new(EvidenceLimits {
        max_record_bytes: 4_096,
        max_count: 1,
        max_statement_bytes: 512,
        max_file_bytes: 8_192,
    });
    evidence_ledger
        .append(
            EvidenceRecord::fact(
                EvidenceKind::Timestamp,
                EvidenceSource::new(EvidenceSourceKind::Filesystem, "file-1"),
                "one timestamp",
                EvidenceStatus::Observed,
                ConfidenceQualifier::Medium,
            )
            .expect("first evidence should be constructed"),
        )
        .expect("first evidence should fit");
    let evidence_error = evidence_ledger
        .append(
            EvidenceRecord::fact(
                EvidenceKind::Timestamp,
                EvidenceSource::new(EvidenceSourceKind::Filesystem, "file-2"),
                "two timestamps",
                EvidenceStatus::Observed,
                ConfidenceQualifier::Medium,
            )
            .expect("second evidence should be constructed"),
        )
        .expect_err("second evidence should exceed the count limit");
    assert!(matches!(
        evidence_error,
        EvidenceError::EvidenceCountExceeded { limit: 1 }
    ));
}

#[test]
fn service_links_records_and_charges_artifact_and_evidence_bytes() {
    let (service, root) = session_service("links-and-budgets");
    let session = service
        .create(session_spec("session-links"))
        .expect("session should be created");
    let (session, artifact) = service
        .create_artifact(
            session.session_id.as_str(),
            session.revision,
            ArtifactKind::CatalogSummary,
            AgentPhase::Classify,
            None,
            &json!({"entry": "one"}),
            ArtifactWriteOptions::default(),
        )
        .expect("artifact should be linked");
    assert_eq!(
        session.artifact_refs,
        vec![artifact.header.artifact_id.clone()]
    );
    assert_eq!(
        session
            .budget
            .consumed()
            .expect("budget should be readable")
            .artifact_bytes,
        artifact.header.byte_size
    );
    let evidence = EvidenceRecord::fact(
        EvidenceKind::CatalogEntry,
        EvidenceSource::new(EvidenceSourceKind::Catalog, "catalog-links"),
        "the linked entry exists",
        EvidenceStatus::Observed,
        ConfidenceQualifier::High,
    )
    .expect("evidence should be constructed");
    let (session, evidence) = service
        .store_evidence(session.session_id.as_str(), session.revision, evidence)
        .expect("evidence should be linked");
    assert_eq!(session.evidence_refs, vec![evidence.evidence_id]);
    assert!(
        session
            .budget
            .consumed()
            .expect("budget should be readable")
            .evidence_bytes
            > 0
    );
    let encoded = fs::read_to_string(root.join("sessions").join("session-links.json"))
        .expect("session should be persisted");
    assert!(!encoded.contains("the linked entry exists"));
    let _ = fs::remove_dir_all(root);
}
