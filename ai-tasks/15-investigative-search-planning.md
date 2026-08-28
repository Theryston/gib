# Task 15 — Implement Investigative Search planning and hypothesis exploration

## Roadmap position

This task begins the end-to-end intelligent search subsystem. It builds bounded search planning on top of the deterministic catalog APIs; it does not allow the model to issue arbitrary queries or search forever.

## Objective

Implement SearchGoal, hypotheses, a search escalation ladder, a bounded search beam, and a planner that deliberately changes search dimensions: exact name, normalized name, synonyms, path, extension/content type, time, and historical state. Each action must be typed, validated by the ToolGateway, budgeted, and recorded as evidence/attempt state.

## Current repository analysis

src/commands/search.rs currently performs token-based path/name search with AND semantics, extension/path filters, deterministic relevance ranking, all-history catalog scope, and a result limit. src/core/catalog/query.rs exposes token lookup, history/current scopes, pagination, and current snapshot correction. Task 12 adds no-query filters, histories, change summaries, and content-hash queries. There is no hypothesis or beam planner today.

The current CLI search behavior must remain predictable. Investigative Search should be a new internal service used by AI workflows, not a change that makes existing search commands depend on a model. The model can propose semantic variants, but Rust owns normalization, query execution, deduplication, ranking inputs, and stopping bounds.

## Search model

Define:

- SearchGoal with the user’s normalized target, intent, temporal/content constraints, desired answer type, and completeness requirement;
- Hypothesis with ID, label, target interpretation, supporting/contradicting evidence IDs, score, status, and last explored dimensions;
- SearchDimension enum: exact_name, normalized_name, synonym, path, extension, content_type, time, historical_state, content_hash, and move/rename history;
- SearchAction with one dimension, normalized arguments, source hypothesis, expected information gain, and budget cost;
- SearchResultSet/CandidateArtifact with stable entry/revision IDs, match features, source query/fingerprint, catalog status, and truncation;
- SearchPlan with escalation level, beam width, maximum actions, and stop policy.

The initial ladder should be explicit and finite:

1. exact normalized name/path;
2. path/name token variants and extension/type filters;
3. user-approved or model-proposed synonyms with clear provenance;
4. historical scope, deleted/reappeared state, and time constraints;
5. content-hash continuity and rename/move analysis;
6. a structured clarification request when the remaining ambiguity cannot be safely resolved.

Do not introduce web search, file-content indexing, OCR, or unbounded fuzzy matching in this task. Those are separate capabilities and should be reported as unavailable rather than simulated.

## Planner and beam behavior

At the start, Rust creates an initial SearchGoal and one or more hypotheses from the router output. The model may propose a small set of candidate hypotheses or synonyms through a schema-constrained prompt. Rust validates them, assigns IDs, caps their count, and removes duplicates.

The planner chooses at most one or a small bounded batch of actions per iteration. Each action must name the dimension it explores and the expected new information. Rust computes the canonical fingerprint, calls ToolGateway, records the outcome, merges candidates by stable entry/revision IDs, and updates hypothesis evidence. Beam width should be a small configured value such as three to five; candidate/action counts and levels are budgeted by AgentBudget.

Use deterministic beam pruning. Prefer hypotheses with new evidence, unexplored dimensions, temporal fit, and lower cost; use stable ID tie-breakers. Do not let a model self-report a probability that overrides deterministic evidence. Preserve abandoned hypotheses and the reason for abandonment so explanations can state what was tried.

Search actions must return bounded metadata-only candidates. They should include why an entry matched, source catalog revision/status, last relevant backup/timestamp, restorable status, and feature values for Task 17. Do not download file contents or restore during search.

## Planner prompts and failure handling

Provide versioned planning and hypothesis prompts. Give the model compact SearchGoal, candidate summaries, previous attempt fingerprints, remaining dimensions, and budget. Ask for typed actions/hypotheses only. If output is invalid, use the structured-generation retry policy; if a proposed dimension is already exhausted or unsupported, reject it and record a planner failure rather than executing a nearby action.

Tool failures should be classified as transient, permanent, unavailable, or degraded-data. A transient failure may consume a bounded retry; a permanent failure should mark the action and let the gap analyzer decide whether another dimension is useful. Never turn a network/storage error into “no results.”

## Tests and acceptance criteria

Create synthetic histories and fake model plans covering:

- exact-name success without escalation;
- synonym/path/extension/time/history escalation;
- deleted files and content-hash rename/move discovery;
- multiple hypotheses, beam pruning, duplicate actions, and deterministic ordering;
- no textual query with metadata filters;
- catalog degraded/pending results and tool failures;
- action, hypothesis, candidate, and token/search budgets;
- invalid/unsupported planner output and bounded retries;
- identical search evidence/traces in interactive and JSON modes.

The task is complete when Investigative Search can start from a vague but supported goal, explore a bounded set of explicit hypotheses and dimensions, produce deterministic candidate artifacts, and stop or escalate with a documented reason. It must never claim completeness solely because the model stopped generating suggestions.

## References

- [GIB search command](../src/commands/search.rs) — existing token search and deterministic score behavior.
- [GIB catalog query API](../src/core/catalog/query.rs) — current/all-history queries and pagination.
- [GIB AI catalog API](12-ai-oriented-catalog-apis.md) — the metadata-only filter/history primitives used by the planner.
- [llama.cpp grammar documentation](https://github.com/ggml-org/llama.cpp/blob/master/grammars/README.md) — constrained planner output.

