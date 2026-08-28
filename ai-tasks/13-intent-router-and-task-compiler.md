# Task 13 — Implement the Intent Router and deterministic Task Compiler

## Roadmap position

This task converts natural-language requests into a small set of supported semantic intents and then into Rust-owned workflows. It is the first layer that understands what the user wants, but the model must not be allowed to invent execution graphs or safety permissions.

## Objective

Implement routing for Locate, ExplainLoss, Restore, TimeTravel, and ExplainHistory, including compound requests. Compile each validated intent into a deterministic workflow with typed inputs, dependencies, required capabilities, stopping conditions, and output contract.

## Current repository analysis

There is no AI router or compiler today. Existing commands expose independent search, explore, and restore behaviors. The catalog APIs in src/core/catalog/query.rs now provide the data primitives needed for routing targets and histories; Task 03 provides structured generation; Task 09 provides sessions/budgets; Task 10 provides workflow execution; Task 11 provides the capability gateway.

The current CLI parser is deterministic and strict, while existing human search uses free text only for path-token lookup. Do not route by matching a few keywords in the command handler. Build a typed router service whose model output is schema-constrained and then validated by deterministic Rust rules. Keep unsupported administrative, destructive, or content-understanding requests explicit rather than guessing.

## Intent model

Define an IntentKind enum with the five initial values and an IntentRequest containing:

- original user text and a redacted/hashable request ID;
- target references, optional path/name/content hints, and optional content type;
- temporal constraints from Task 14;
- desired operation such as locate-only, explain, preview, or restore;
- conversation/session context references;
- ambiguity and missing-information fields.

Define compound intents as a validated ordered graph or list of sub-intents with explicit dependency edges. Examples:

- “Find the PDF I deleted last week and restore it” compiles to Locate/ExplainLoss prerequisites, candidate disambiguation, RestorePreview, ConfirmationRequired, Commit, and Verify.
- “What happened to the project archive, then restore the last intact copy” compiles to ExplainHistory/Loss, candidate resolution, RestorePreview, and a gated restore path.
- “Show the latest version before Tuesday” compiles to TimeTravel plus deterministic candidate selection, not a generic search followed by an arbitrary model answer.

The router output must include a confidence/ambiguity status only as a routing aid. It is not permission to act. If multiple intent kinds are plausible or required target information is missing, return a structured clarification state. Do not silently convert Restore into Locate or TimeTravel into current-state search.

## Prompt and validation flow

Use a versioned routing prompt and a closed JSON Schema enum for intent kinds, supported operation modes, and field shapes. Tell the model the supported capabilities and non-goals. Include only compact conversation context and trusted prior evidence. The schema must reject unknown intent kinds and unknown fields.

After parsing, deterministic validation must:

- normalize paths and IDs using existing core rules;
- reject conflicting fields, invalid temporal expressions, impossible operation combinations, and unbounded limits;
- verify that requested capabilities exist in the current build;
- classify whether user confirmation will be required later;
- assign a stable router version and canonical request fingerprint.

Model output must never include a workflow DAG, tool name, filesystem operation, or restore destination that bypasses the compiler. If a future intent needs a new capability, update the enum, schema, compiler, and tests together.

## Task Compiler

Implement a pure compiler from IntentRequest to WorkflowPlan. A plan should contain workflow ID/version, ordered phases, dependency edges, typed phase inputs, required tools/capabilities, budget class, stopping conditions, and final response shape. The compiler must be deterministic for the same normalized request, catalog/runtime capabilities, and workflow version.

Use explicit compilation rules:

- Locate: normalize target → scan/search → rank candidates → judge ambiguity → return candidates.
- ExplainLoss: locate/resolve target → fetch history → build timeline/change summary → explain facts and inferences.
- TimeTravel: resolve temporal constraint → select revision deterministically → return snapshot/candidate.
- ExplainHistory: resolve scope/time → obtain history/timeline → summarize changes → explain with evidence.
- Restore: resolve target/revision → validate restorable source and destination → create preview plan → wait for confirmation → commit plan ID → verify.

Compound plans must share compatible evidence/artifacts rather than repeating searches. Dependencies must express data requirements, and a phase cannot execute if its prerequisite failed or is ambiguous. A plan with an unresolved ambiguity should stop at a user-facing clarification/confirmation state.

The compiler should preserve the user’s intent in a typed summary but never promise unsupported content analysis, causal certainty, or an automatic destructive action. Store the plan and router version in AgentSession for reproducibility.

## Tests and acceptance criteria

Create a fixture corpus with short and ambiguous requests. Test:

- all five simple intents, unsupported requests, and malformed structured output;
- compound intent dependency graphs and shared artifacts;
- conflicting target/time/action fields;
- missing target, missing timezone, ambiguous candidate, and unavailable capability behavior;
- deterministic plan serialization/fingerprints across repeated runs;
- schema version and router version changes;
- budgets and safety flags attached to restore plans;
- interactive and JSON routing responses with no raw model text or prompts;
- regression against existing explicit CLI commands.

The task is complete when natural language is reduced to one validated intent or a clear clarification state, and every accepted intent compiles to a deterministic Rust-owned workflow. No model response may directly schedule an undeclared tool or bypass the restore confirmation boundary.

## References

- [schemars documentation](https://docs.rs/schemars/latest/schemars/) — typed router and compiler DTO schemas.
- [jsonschema documentation](https://docs.rs/jsonschema/latest/jsonschema/) — runtime validation of model output.
- [GIB catalog query implementation](../src/core/catalog/query.rs) — deterministic target/history data.
- [GIB restore command](../src/commands/restore.rs) — existing behavior to preserve behind a safer future service.

