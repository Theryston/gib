# Task 11 — Implement the AI Tool Gateway, validation, tracing, and anti-loop system

## Roadmap position

This task gives the harness a controlled way to call deterministic GIB capabilities. It is the security boundary between semantic model decisions and repository state. It must be implemented before the model can plan investigative searches or restore operations.

## Objective

Create a typed ToolGateway that exposes only approved capabilities, validates every request, classifies risk, creates evidence, emits trace events, and detects repeated or unproductive actions. The gateway must be usable by the orchestrator and both frontends, while keeping all filesystem, catalog, and restore truth in Rust services.

## Current repository analysis

The repository has command handlers for search, explore, backup, and restore, plus core catalog/query and restore services. The current handlers are CLI-oriented and some restore paths can mutate files directly. Do not let the model invoke command handlers, spawn a shell, construct arbitrary paths, or bypass core validation. Add a service-level gateway that calls typed core APIs.

src/core/catalog/query.rs already returns typed entry summaries, histories, pagination, and catalog status. src/commands/search.rs adds CLI-specific token parsing and ranking. src/commands/explore.rs adds navigation and a restore bridge. src/core/restore.rs performs the actual writes. The AI gateway should wrap reusable core operations and return bounded DTOs, not parse human CLI output.

Task 09 supplies AttemptLog, EvidenceLedger, and budgets; Task 10 supplies phase permissions and the orchestrator. Task 19 will add a safe restore plan and commit service. The gateway must allow only the read-only and preview operations available at each phase until those services exist.

## Tool contracts and capability policy

Define:

- ToolName and tool-version identifiers;
- ToolRequest with request ID, session/phase/attempt IDs, tool name, typed arguments, and caller skill/capability;
- ToolResult with typed data, evidence references, artifact references, warnings, and a bounded status;
- ToolFailure with stable code, retryability, safe message, and validation location;
- ToolPermission containing allowed tools, argument limits, and side-effect policy;
- RiskClass such as read-only, local-analysis, preview, confirmation-required, and commit.

Each tool must have a registered schema, version, maximum output size, required permission, risk class, and implementation pointer. Use typed Rust request/response DTOs and the structured-generation schema infrastructure where the model chooses arguments. The model must never receive an unbounded generic JSON object or an arbitrary tool name.

The initial read-only gateway should include catalog scan/history/summary/content-hash queries, deterministic temporal resolution, and candidate ranking inputs as they become available. Restore preview may be registered later. Restore commit must never be an ordinary model-callable tool: it requires the immutable plan ID and the explicit confirmation flow from Task 19.

## Validation

Validate before any core call:

- tool exists, is enabled for the current skill and phase, and is supported by the current workflow version;
- JSON shape and schema version;
- IDs resolve to the expected catalog/session/conversation scope;
- paths pass the existing catalog normalization rules and cannot be absolute, traversing, or outside an approved target root;
- time intervals are valid, bounded, and normalized;
- extensions/content types/limits/counts are within allowlisted bounds;
- requested artifact and response sizes fit the AgentBudget;
- side-effecting requests have a proper preview/plan/confirmation state.

Validation errors must identify a machine-readable field path and never call the underlying operation. Use a single canonical validation layer so interactive and JSON mode cannot disagree. Revalidate authoritative state after a model response; a schema-valid ID may still be stale or unavailable.

## Fingerprints and anti-loop behavior

Compute a canonical fingerprint from tool version, normalized operation name, and canonical arguments. Canonicalization must define whitespace, case behavior for case-insensitive fields, path separators, sorted set-like arrays, default omission, and timestamp representation. Do not sort semantically ordered arrays. Hash the canonical representation with SHA-256 and record the fingerprint in AttemptLog.

Before execution, compare the fingerprint with committed successful, failed, and in-flight attempts according to policy. An exact duplicate should be rejected or returned from a cached immutable result; it must not consume unlimited search attempts. Near-duplicates should be detected through normalized dimensions and reported as repeated progress, not guessed with vague string similarity.

Track progress signals such as new entries, new evidence IDs, new temporal coverage, a new search dimension, or a reduced hypothesis set. A sequence of valid calls that produces no new signal must transition to a loop/no-progress outcome within the workflow budget. The gateway should expose a compact anti-loop diagnostic to the planner, including prior tool names/fingerprints and unexplored dimensions, without dumping full payloads.

## Tracing, evidence, and output

Emit ToolStarted, ToolValidated, ToolCompleted, ToolRejected, ToolFailed, and ToolDeduplicated events with monotonic sequence numbers. Link each completed result to an ArtifactStore record and each authoritative observation to EvidenceLedger. A tool result must state whether the catalog was ready, degraded, pending, or unavailable.

Redact credentials, storage keys, prompt content, native logs, and sensitive file data. Return only bounded fields and explicit truncation markers. Paths may be sensitive; use the repository’s existing path policy and a safe display representation where appropriate. JSON mode receives structured events; interactive mode may render concise activity text from them. No gateway code may print directly to stdout.

## Tests and acceptance criteria

Cover:

- registration and version validation;
- permission denial by skill, phase, workflow, and risk class;
- malformed JSON, unknown fields, invalid IDs/paths/time ranges, oversized limits, and stale references;
- canonical fingerprints for defaults, paths, case, set-like arrays, and ordered arrays;
- exact duplicate rejection/cache behavior and near-duplicate/no-progress detection;
- budget consumption and output truncation;
- evidence/artifact links and degraded catalog propagation;
- safe error redaction;
- trace sequence, terminal events, JSON parseability, and interactive/JSON parity;
- proof that restore commit, shell execution, arbitrary filesystem access, and network calls are unavailable unless explicitly registered by a future service.

The task is complete when every model-selected operation passes through one typed, permissioned, budgeted, fingerprinted gateway, produces evidence or a documented limitation, and cannot silently repeat forever or perform an undeclared side effect.

## References

- [GIB catalog query API](../src/core/catalog/query.rs) — existing typed historical data and status semantics.
- [GIB restore implementation](../src/core/restore.rs) — the mutation boundary that must not be exposed directly to a model.
- [sha2 crate documentation](https://docs.rs/sha2/latest/sha2/) — canonical request fingerprints.
- [serde documentation](https://serde.rs/) — typed request and response serialization.

