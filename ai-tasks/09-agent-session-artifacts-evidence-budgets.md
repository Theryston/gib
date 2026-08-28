# Task 09 — Implement AgentSession, artifacts, evidence, and budgets

## Roadmap position

This task marks the transition from direct chat to a bounded operational harness. It provides the memory and accounting primitives that the deterministic orchestrator and tool gateway will consume. It must not yet make the language model an authority over state or safety.

## Objective

Create the core operational types and services described by the AI design: AgentSession, AgentPhase, ArtifactStore, EvidenceLedger, AttemptLog, and AgentBudget. They must preserve enough state to explain and resume one turn, keep artifacts and evidence typed and bounded, and make resource consumption auditable in both interactive and JSON modes.

## Current repository analysis

The repository currently has persistent catalog histories and restore metadata, but no agent session, artifact, evidence, or budget modules. src/core contains domain logic for catalogs, indexes, metadata, and restore; src/output.rs owns event emission; the conversation files from Task 04 hold durable user-visible dialogue. Keep AgentSession separate from ConversationStore: a conversation is a long-lived transcript, whereas a session is a workflow execution with attempts, phases, evidence, and stopping state.

The existing catalog is metadata-only and can become degraded or pending. Evidence and artifacts must retain that status rather than presenting a degraded query as definitive. Existing restore code returns structured stats, which can later be referenced as artifacts, but Task 09 must not change restore behavior yet.

## Domain model

Define stable, serializable identifiers and enums:

- AgentSessionId, TurnId, AttemptId, ArtifactId, EvidenceId, and TraceEventId;
- AgentPhase values such as classify, plan, search, analyze, explain, restore-preview, confirm, commit, verify, and complete;
- session status values running, waiting-for-user, completed, cancelled, failed, and budget-exhausted;
- evidence kind, source kind, confidence qualifier, and fact/inference distinction;
- artifact kind, storage status, size, content hash, and retention class.

AgentSession should contain the immutable request/turn identity, conversation ID, workflow ID/version, phase/status, created/updated times, resolved runtime profile, budget counters, artifact/evidence references, attempt summaries, and a stop reason. Avoid storing raw prompts, hidden reasoning, credentials, or unbounded tool output in the session.

AgentBudget must make each limit explicit. Include wall-clock deadline, maximum model calls, output tokens, tool calls, search actions, candidate count, context bytes/tokens, retry count, and artifact/evidence bytes. Represent consumed and remaining values separately and provide a single checked consume operation. Every operation that can spend a budget must go through that operation; a log field alone is not enforcement.

## ArtifactStore

Artifacts are bounded, addressable outputs such as a catalog page, normalized timeline, candidate set, restore preview, or compact explanation input. Store a typed header with artifact ID, kind, schema version, producing phase/attempt, content hash, byte size, created time, sensitivity, and truncation status. Keep small artifacts in memory for a turn and persist only the explicitly durable subset under a session directory if continuation requires it.

Use content-addressed or otherwise collision-resistant storage names, but do not treat content hashes as authorization. Enforce maximum size and count, canonical serialization, and redaction before persistence. If an artifact is truncated, mark it and preserve the reason; downstream code must not treat it as complete evidence. Keep references rather than copying the same large catalog result into every prompt.

## EvidenceLedger

Evidence records should identify the source and the exact claim they support. A record may reference a catalog entry ID, revision ID, backup ID/timestamp, content hash, normalized event, tool invocation, or verified restore result. Store:

- evidence ID and schema version;
- source type and stable source identifiers;
- fact or inference label;
- statement or structured payload;
- qualifying status such as observed, derived, unavailable, or degraded;
- timestamps and provenance;
- linked artifact/attempt IDs.

Do not equate a model-generated statement with evidence. Inferences must cite supporting evidence IDs and use qualified wording. A missing or degraded catalog must be represented as a limitation in the ledger and propagated to explanations.

## AttemptLog and trace integration

AttemptLog records one bounded operation attempt: phase, action type, canonical fingerprint, start/end, outcome, budget delta, artifact IDs, evidence IDs, and safe error code. It must support loop detection in Task 11 without retaining unrestricted input or output. Trace events should be append-only for a running session and consumable by the frontend, with monotonic sequence numbers and a terminal session event.

Define redaction rules before logging. Paths may be sensitive; credentials, storage keys, prompt bodies, full message content, and native diagnostics must never appear by default. Provide a safe diagnostic identifier and retain full detail only through an explicit debug facility with its own policy.

## Lifecycle and continuation

Expose a SessionService to create, load, transition phase, append artifact/evidence/attempt references, consume budget, cancel, and complete/fail. Transitions must be validated by a finite-state table. A session may wait for user confirmation but must not auto-advance through Task 19’s safety gate.

Persist only what is needed for process continuation, using the same atomic/lock/migration discipline as conversations. If the process crashes, recovery must show the last committed phase and mark in-flight attempts as interrupted rather than fabricating success. Session state must reference conversation and catalog IDs rather than duplicating their authoritative data.

## Tests and acceptance criteria

Test:

- valid and invalid phase/status transitions;
- budget consumption, exhaustion, deadline, concurrent consume, and exact remaining values;
- artifact size/count limits, hashes, schema versions, truncation, sensitivity, and persistence;
- evidence linking, fact versus inference labels, degraded-source propagation, and missing-source handling;
- attempt fingerprints and safe redaction;
- monotonic trace sequence and exactly one session terminal event;
- crash-like recovery of an interrupted attempt and migration of older session versions;
- JSON serialization with bounded fields and no hidden reasoning/secrets;
- fake sessions used by later orchestrator/tool tests.

The task is complete when one agent turn has a typed, bounded, recoverable operational memory independent from its conversation transcript, and every later model/tool operation can be charged, traced, and tied to evidence or an explicit limitation.

## References

- [Rust serde documentation](https://serde.rs/) — versioned typed serialization patterns.
- [sha2 crate documentation](https://docs.rs/sha2/latest/sha2/) — artifact and canonical-payload hashing.
- [GIB catalog model](../src/core/catalog/model.rs) — existing schema-versioned historical data and revision identifiers.
- [GIB output implementation](../src/output.rs) — event and error serialization conventions.

