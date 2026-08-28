# Task 10 — Implement the deterministic AI orchestrator and context builder

## Roadmap position

This task turns the session primitives into repeatable workflows. It is the point at which GIB begins replacing an unbounded chat loop with a Rust-owned state machine, while still using the local model only for narrow, typed semantic decisions.

## Objective

Implement a deterministic orchestrator for workflow phases, dependencies, progress, stopping conditions, cancellation, and continuation. Build compact role-specific context from the persistent conversation, session artifacts, evidence, and verified catalog data. The same event stream must drive interactive and JSON frontends.

## Current repository analysis

Task 04 owns persistent ConversationStore/Service; Task 09 owns AgentSession, artifacts, evidence, attempts, and budgets; Task 03 owns versioned prompts and structured generation. The repository’s catalog and restore layers are deterministic Rust services, while src/main.rs and src/output.rs provide the CLI and output boundaries. There is no existing workflow engine, so the implementation should be explicit and small rather than a generic scheduler.

The current commands often perform a single deterministic operation and return a typed response. Use that style for workflow phases. Do not move truth resolution or filesystem mutation into the model. The catalog may report ready, degraded, or pending; the orchestrator must carry those statuses into stopping decisions and explanations.

## Workflow model

Define:

- WorkflowId and WorkflowDefinition with version, supported intent kinds, required capabilities, and phase graph;
- PhaseDefinition with phase ID, prerequisites, input artifact kinds, output artifact kinds, budget class, and allowed side effects;
- PhaseStatus values pending, ready, running, waiting, succeeded, skipped, failed, cancelled, and exhausted;
- StopReason values goal-satisfied, no-candidate, ambiguous, evidence-insufficient, budget-exhausted, user-cancelled, safety-confirmation-required, dependency-failed, and internal-error;
- OrchestratorEvent with session/phase/attempt IDs, sequence, phase, safe summary, progress state, artifact/evidence refs, and terminal status.

Keep the dependency graph acyclic and validate it when a workflow is registered. At runtime, the Rust scheduler owns which phase may run next. A model response can propose a typed decision inside a phase, but it cannot add a phase, invoke an undeclared tool, bypass a prerequisite, or mark a phase successful without the phase validator.

Implement one active attempt per session unless a workflow explicitly declares bounded parallel work. Ready-phase selection must be deterministic, for example definition order followed by phase ID. Every transition must append a trace event and update AgentSession atomically enough that a crash leaves a recoverable last committed state.

## Context builder

Build compact contexts by role rather than sending the entire conversation or repository:

- conversation context: recent user-visible messages plus a durable summary and explicit preferences;
- routing context: current request, a small amount of prior turn context, and available capabilities;
- search context: normalized goal, hypothesis summaries, prior attempt fingerprints, candidate artifacts, and remaining budget;
- history/explanation context: selected evidence records, timeline artifacts, source-status limitations, and facts/inferences;
- restore context: exact user intent, validated selected revisions, RestorePlan preview, risk/confirmation state, and verification requirements.

Every context item should carry source/type, size, and trust classification. Use deterministic ordering, stable truncation, and explicit truncation markers. Never let a lower-trust model-generated summary replace authoritative catalog or filesystem values. Context builders should return a typed ContextBuildResult with token/byte estimates, omitted item counts, and warnings.

Summarization may use the model only through a versioned structured prompt and must be bounded. Prefer deterministic reduction first: deduplicate evidence, collapse repeated attempts, keep IDs and timestamps, and preserve unresolved limitations. A summary must retain links to the source artifacts it represents.

## Orchestration behavior

At the start of a turn, resolve conversation, model/profile, workflow, and budget. Emit a started event. For every phase:

1. Validate prerequisites and budget.
2. Build and validate the phase context.
3. Invoke the declared semantic operation or deterministic tool service.
4. Validate its typed output and create artifacts/evidence.
5. Record an AttemptLog and phase result.
6. Evaluate stopping conditions and select the next ready phase.

Progress must be evidence-based: new candidate, new time constraint, new verified fact, completed preview, or an explicit exhausted dimension. Repeated model text or a spinner tick is not logical progress. Cancellation must stop at a safe boundary and mark in-flight work interrupted/cancelled. A deadline or budget exhaustion must produce a structured stop reason, not an incomplete success.

The orchestrator should support process continuation from the last committed phase. On resume, reconcile in-flight attempts conservatively, revalidate referenced artifacts, and never repeat a side effect unless the operation is idempotent and its fingerprint is absent from the committed log.

## Frontend and error contract

Expose an event sink interface. Interactive mode renders events; JSON mode serializes the same events and final response envelope. Events must be bounded, ordered, and free of ANSI/native output. A phase failure must include a stable code, phase, retryability, and evidence limitation. Do not expose raw prompt or model internals.

Keep restore commit and other destructive actions behind declared future phases and a confirmation state. This task may model waiting-for-confirmation but must not approve or mutate files.

## Tests and acceptance criteria

Add:

- workflow graph validation and deterministic ready-phase ordering;
- transition tables for success, skip, failure, cancellation, dependency failure, and exhaustion;
- context-builder fixtures proving deterministic ordering, truncation, source trust, and no hidden reasoning;
- budget/deadline enforcement at phase and operation boundaries;
- repeated progress detection and stopping behavior;
- crash/resume fixtures with interrupted attempts and missing artifacts;
- identical event sequences consumed by interactive and JSON adapters;
- fake semantic generator/tool placeholders, including invalid typed output;
- end-to-end deterministic workflows for a simple Locate and ExplainHistory fixture, without a real model.

The task is complete when a workflow can be compiled into a validated DAG, execute with bounded retries and budgets, resume conservatively after a process boundary, and produce the same structured progress/result semantics in both output modes.

## References

- [serde documentation](https://serde.rs/) — typed workflow and session contracts.
- [GIB catalog query implementation](../src/core/catalog/query.rs) — authoritative source/status data for context building.
- [GIB output implementation](../src/output.rs) — shared event and JSON behavior.

