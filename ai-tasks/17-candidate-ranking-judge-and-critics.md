# Task 17 — Implement deterministic candidate ranking, Candidate Judge, and search critics

## Roadmap position

This task completes Investigative Search end to end. It combines deterministic Rust ranking with narrowly scoped semantic judgment and a completeness critic, while preserving ambiguity instead of forcing a confident answer.

## Objective

Rank candidates deterministically, use a schema-constrained Candidate Judge only when lexical/metadata evidence cannot resolve the result, handle ambiguity and optional self-consistency, and run a Completeness Critic when the workflow requires a defensible search conclusion.

## Current repository analysis

src/commands/search.rs already has search_relevance_score based on exact/path/name/stem/token matches and orders results by score, newest timestamp, and path. src/core/catalog/query.rs supplies stable summaries and histories. Task 12 adds typed filters and content-hash/history data; Task 15 supplies hypotheses/actions; Task 16 supplies coverage and gap analysis. There is currently no semantic candidate judge or completeness critic.

Do not replace the existing deterministic score with an opaque model score. Preserve the CLI search contract and implement an AI ranking layer in core/ai services. The model must see bounded candidate summaries, not arbitrary repository contents, and must never be allowed to manufacture an entry ID or claim that unreturned data was searched.

## Ranking contract

Define CandidateFeatures and CandidateScore with explicit dimensions:

- exact normalized name/path match;
- token/name/stem overlap;
- path-prefix and directory fit;
- extension/content-type fit;
- size and other metadata constraints;
- temporal fit and revision-state fit;
- deleted/reappeared/history fit;
- content-hash continuity or rename/move support;
- hypothesis support/contradiction;
- source completeness/restorable status.

Every feature must have a deterministic value and documented weight/version. Combine features in Rust using bounded integer or fixed-point arithmetic where practical, then sort by score, exact tie-breakers, stable entry ID, revision ID, and path. Do not use locale-dependent string ordering or floating-point NaN behavior. Retain feature explanations in the candidate artifact.

Limit the candidate set passed to a model, normally a small top slice such as five to twelve candidates plus explicitly diverse alternatives. Include a coverage marker saying how many total candidates were considered and whether results were truncated. A judge that sees only a slice must not be asked to assert global completeness.

## Candidate Judge

Define a versioned CandidateJudgeRequest containing SearchGoal, compact hypotheses, candidate summaries/features, relevant evidence IDs, catalog completeness, and the requested resolution policy. Define a typed response containing:

- selected candidate IDs or no selection;
- resolution quality: strong, acceptable, ambiguous, or insufficient;
- a small set of competing candidates when ambiguous;
- field-level reasons linked to evidence IDs;
- missing information or clarification request;
- judge version and candidate coverage.

The response must be schema-constrained and validated. Rust must verify every returned ID belongs to the supplied candidate set, every evidence ID exists, and the reason does not assert an unsupported source. The judge may choose among candidates or say that none can be selected; it must not create a candidate, alter scores, resolve timestamps, or authorize restore.

Define ambiguity thresholds in Rust using score gaps, conflicting constraints, and judge output. A model saying “95 percent sure” is not a safety or selection threshold. When two candidates are close or evidence is contradictory, return both with a useful discriminator. Restore workflows must stop until the user or a deterministic policy resolves the ambiguity.

## Self-consistency and critics

Use self-consistency only for difficult/close cases and within AgentBudget. Run a small fixed number of independently seeded judge attempts, canonicalize their selected IDs, and accept only a documented agreement rule. If attempts disagree, return ambiguity; never majority-vote a destructive action. Record all attempt fingerprints/results in AttemptLog without persisting hidden reasoning.

The Completeness Critic receives SearchGoal, SearchCoverage, gap analysis, final candidates, catalog status, and search budget. It returns a typed verdict:

- complete enough for the requested answer;
- incomplete because a dimension/source/budget is missing;
- ambiguous and requiring clarification;
- no match under the covered constraints.

The critic must cite coverage and limitations, not merely repeat the judge’s conclusion. It cannot launch searches itself. The orchestrator decides whether to return, call Search Gap Analyzer once more, or ask the user.

## Output and evidence

Create evidence records for deterministic feature scores, judge selections, disagreements, critic verdicts, catalog status, and coverage. Keep facts separate from semantic interpretations. Interactive output may explain the top reasons; JSON output must expose IDs, scores/features, ambiguity, evidence references, and completeness fields in stable DTOs. No raw model text is required in the final response.

## Tests and acceptance criteria

Build fixtures for:

- exact single match, lexical ties, path/type/time disambiguation, deleted/reappeared files, and content-hash moves;
- deterministic ordering across repeated runs and platforms;
- judge selecting only supplied candidates, invalid IDs/evidence, malformed output, and bounded retries;
- close candidates, explicit ambiguity, self-consistency agreement/disagreement, and no false confidence;
- critic verdicts for complete, truncated, degraded, exhausted, and unsupported searches;
- candidate/evidence/trace budget limits and JSON/interactive parity;
- existing CLI search ranking regression tests remaining unchanged.

The task is complete when Investigative Search returns the best evidence-supported candidate or an actionable ambiguity/incompleteness result, with deterministic ranking and auditable semantic judgment. A model must never turn an incomplete or tied search into an unqualified unique answer.

## References

- [GIB search ranking implementation](../src/commands/search.rs) — existing deterministic score and tie-breakers.
- [GIB catalog query implementation](../src/core/catalog/query.rs) — stable candidate summaries and historical scope.
- [jsonschema documentation](https://docs.rs/jsonschema/latest/jsonschema/) — validating judge and critic responses.
- [llama.cpp grammar documentation](https://github.com/ggml-org/llama.cpp/blob/master/grammars/README.md) — constrained semantic output.

