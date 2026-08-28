# Task 16 — Implement the Search Gap Analyzer and recovery behavior

## Roadmap position

This task makes Investigative Search recover intelligently when an initial plan stops producing information. It builds on SearchGoal, hypotheses, the escalation ladder, the ToolGateway, AgentBudget, and the deterministic catalog APIs.

## Objective

Detect missing progress, repeated searches, unexplored dimensions, invalid or weak hypotheses, and exhausted data sources. Produce a bounded recovery decision that either expands the search intelligently, abandons a bad hypothesis, requests clarification, or stops with an honest completeness limitation.

## Current repository analysis

Task 15 introduces SearchPlan, hypotheses, SearchDimension, SearchAction, beam pruning, and candidate artifacts. Task 11 records canonical tool fingerprints and anti-loop signals. Task 09 provides AttemptLog, EvidenceLedger, and budgets. Task 12 provides metadata-only scans, histories, content-hash lookup, and catalog status. The existing src/commands/search.rs performs deterministic lexical search but has no concept of search gaps, repeated attempts, or recovery.

Do not infer a gap by comparing free-form model prose. The analyzer must consume typed attempt records, candidate/evidence deltas, known catalog capabilities, and the explicit SearchGoal. It must also distinguish “no matching entry” from “the catalog was incomplete,” “the tool failed,” and “the requested dimension is unsupported.”

## Gap model

Define a SearchCoverage record containing:

- dimensions attempted and their normalized argument summaries;
- hypotheses explored, supported, contradicted, abandoned, or unresolved;
- candidate IDs and evidence IDs discovered per attempt;
- temporal/path/type/history ranges covered;
- duplicate and near-duplicate fingerprints;
- catalog indexed-through status and warnings;
- remaining action, model, token, wall-clock, and candidate budgets.

Define ProgressSignal values such as new candidate, new revision, new content-hash link, new evidence, narrowed temporal range, eliminated hypothesis, new dimension, source recovery, and no-progress. Make the signal calculation deterministic and based on stable IDs, not model confidence.

Define GapKind values:

- unexplored dimension;
- overly narrow constraint;
- unsupported synonym/normalization variant;
- insufficient historical scope;
- candidate ambiguity;
- degraded or incomplete source;
- transient tool failure;
- repeated action;
- hypothesis contradicted;
- budget/deadline exhaustion.

The analyzer should return a GapAnalysis with gap kind, evidence/attempt references, safe next dimensions, blocked dimensions, recommended action count, and a stop/clarification reason. It must never return an arbitrary query string or filesystem operation.

## Recovery behavior

After every search action or bounded batch, compute a progress delta. If the result is new, update the beam and continue only when a declared stopping condition has not been met. If there is no new signal, inspect the remaining dimensions and canonical fingerprints:

1. Reject exact duplicates before execution.
2. Mark a dimension exhausted after its documented normalized variants and bounded pages have been tried.
3. Penalize or abandon a hypothesis with repeated contradictions, no candidate support, or invalid target assumptions.
4. Prefer a genuinely unexplored dimension that is permitted by the SearchGoal and budget.
5. Retry transient infrastructure failures with bounded backoff, but never classify a failure as an empty result.
6. Ask for clarification when multiple viable hypotheses remain or when a required constraint cannot be inferred safely.
7. Stop as no-match, incomplete, or exhausted only with the corresponding source/budget evidence.

The model may propose a gap explanation or one next dimension through a structured prompt. Rust must validate that the proposed dimension is available, new or deliberately retryable, within budget, and consistent with the goal. The model may not re-enable an exhausted dimension by changing capitalization or whitespace.

Use configurable ceilings for no-progress iterations, repeated attempts, synonym count, path branches, time expansions, and total hypotheses. Make all ceilings part of the search plan version and include them in evidence for completeness.

## Failure and continuation

Persist enough SearchCoverage and AttemptLog state to resume after a process boundary. A resumed search must not repeat a successful side-effect-free action unless its result artifact is missing or invalid. Revalidate artifact hashes and catalog revision/status before continuing. If the catalog changes, invalidate cursors and state which coverage facts remain valid.

A gap due to degraded catalog must be propagated to ExplainLoss and ExplainHistory. A gap due to candidate ambiguity should transition to the router/session’s clarification state, not trigger broader destructive actions. JSON mode returns the structured gap/clarification response; interactive mode renders it and may collect a new constraint through the shared input abstraction.

## Tests and acceptance criteria

Test:

- progress signals for new and duplicate candidates, revisions, hashes, hypotheses, and dimensions;
- exact and near-duplicate search actions with canonical fingerprints;
- dimension exhaustion and capitalization/format variants;
- hypothesis abandonment after contradiction or repeated no-progress;
- transient versus permanent tool failures and bounded retries;
- degraded, pending, empty, and changing catalogs;
- budget/deadline/no-progress limits and deterministic stop reasons;
- process continuation with valid, missing, and stale artifacts;
- clarification responses for unresolved ambiguity;
- identical structured events and results in interactive and JSON modes.

The task is complete when an investigative search either finds new information, deliberately changes strategy, asks for a useful missing constraint, or stops with a supported limitation. It must not loop on semantically identical searches or report false completeness after an infrastructure/data gap.

## References

- [GIB search command](../src/commands/search.rs) — existing lexical search behavior and limits.
- [GIB catalog query API](../src/core/catalog/query.rs) — deterministic query/status primitives.
- [GIB AI search planner](15-investigative-search-planning.md) — search dimensions, hypotheses, and beam contract.
- [sha2 crate documentation](https://docs.rs/sha2/latest/sha2/) — stable action fingerprints.

