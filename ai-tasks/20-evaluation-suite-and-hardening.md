# Task 20 — Build the full GIB AI evaluation suite and harden the product

## Roadmap position

This is the final roadmap task and an ongoing quality gate for every future AI change. It must measure the complete system rather than only whether a model produces plausible prose.

## Objective

Create a deterministic evaluation corpus, executable workflow tests, mode-parity tests, failure-injection scenarios, concurrency/process-continuation coverage, performance measurements, and release hardening for the full GIB AI stack. Establish regression thresholds and make the safe behavior observable in CI.

## Current repository analysis

The repository already has tests around catalog indexing/querying, search, explore, restore, encryption, and output behavior. Its Cargo manifest uses tokio and async services, while the release workflow targets Linux, Windows, and macOS on x86_64 and aarch64. The AI work adds native llama.cpp, user-level persistence, terminal rendering, structured model output, and filesystem safety, so a test suite that depends on one developer machine or one real model is insufficient.

Tasks 01–19 define model installation, AiBackend, structured prompts, conversations, UI, profiles, sessions, orchestration, tools, catalog APIs, routing, temporal resolution, investigative search, history explanation, and restore plans. Every component must remain testable with fakes and synthetic repositories. The real Qwen model at the GIB bucket URL is appropriate for opt-in smoke/performance runs, not mandatory for ordinary unit or pull-request tests.

## Evaluation corpus

Build reusable fixtures with a small LocalFS/repository generator:

- empty and newly initialized repositories;
- files added, modified, deleted, reappeared, renamed, moved, and duplicated by content hash;
- full parentless and incremental snapshots;
- exact backup/timezone boundary cases;
- encrypted and non-encrypted catalogs;
- degraded, pending, stale, missing-chunk, and partially indexed states;
- large directories, many revisions, ambiguous candidates, and no-query metadata searches;
- local files missing, identical, changed, symlinked, protected, and concurrently modified for restore tests.

Each fixture should have an expected truth manifest containing entry/revision/backup IDs, presence intervals, restorable status, catalog completeness, and allowed answers. Do not use natural-language expected answers as the sole oracle.

## Test layers

Add the following layers:

1. Unit tests for normalization, time arithmetic, schema/grammar compilation, prompt versioning, IDs, fingerprints, budgets, state transitions, profile resolution, ranking, evidence, and path safety.
2. Service tests with FakeAiBackend and scripted structured outputs covering valid, malformed, schema-invalid, semantically invalid, contradictory, and cancelled responses.
3. Workflow tests for each intent and compound intent, including search escalation/gap recovery, history/loss explanation, and restore preview/confirmation/commit/verify.
4. Adapter tests proving interactive and JSON modes consume the same event/result source. Parse JSON stdout strictly and assert no ANSI, prompt, spinner, native log, secret, or unstructured text leakage.
5. Process-level tests that invoke the binary in separate processes, continue a conversation/session, select explicit versus active conversations, and exercise concurrent writers/locks.
6. Failure-injection tests for interrupted downloads, partial atomic writes, stale locks, corrupted metadata, missing artifacts, model load failure, backend cancellation, catalog degradation, changed restore preconditions, and verification failure.
7. Optional real-model smoke tests that are explicitly enabled, record model URL/ID/digest/version, cap prompts/output, and never mutate user files. The test harness must not download the model implicitly in ordinary CI.

## Evaluation dimensions and metrics

Track machine-readable results for:

- intent routing accuracy and compound dependency correctness;
- temporal boundary and timezone accuracy;
- search recall/precision, candidate ambiguity handling, escalation efficiency, and completeness verdicts;
- loss/history factuality, disappearance-window correctness, fact/inference separation, and unsupported-cause rate;
- restore safety: false-positive restore, pre-confirmation writes, stale-plan acceptance, path escape, verification failure, and JSON blocking;
- conversation durability, active/explicit selection, schema migrations, concurrent updates, and continuation;
- structured-output validity, retry counts, invalid-output persistence, and prompt/schema version coverage;
- tool permission violations, duplicate/loop prevention, evidence linkage, and budget enforcement;
- interactive/JSON parity and error/exit-code consistency;
- startup/load/warm-generation latency, tokens per second when available, peak memory, context usage, tool/model call counts, and disk footprint.

Use deterministic pass/fail gates for safety and correctness. Quality metrics may have target thresholds, but a semantic score must never override a hard safety failure. Store evaluation metadata, fixture version, binary commit, runtime profile, model ID/digest, and interpreter/workflow versions.

## Performance and portability

Add benchmarks for model download/resume/hash verification, catalog scans/history/content-hash queries, context building, routing/search workflows with a fake backend, restore preview/verification, and real-model first-load/warm-turn when opted in. Record LowMemory/Balanced/HighQuality profile behavior and ensure memory limits are enforced.

Run cargo fmt --check, cargo check, cargo test, and cargo build --release in CI. Compile the CPU/default feature set for every existing release target and compile accelerator features in matching jobs where toolchains exist. Keep native llama.cpp logs out of command JSON. Add a documented way to skip optional hardware/model tests without skipping deterministic safety tests.

Run security-focused tests for path traversal, symlink races, permissions, secret/log redaction, untrusted catalog/message content, schema external references, malformed GGUF/metadata, lock recovery, and plan tampering. Review dependency licenses and native build provenance before release.

## Tests and acceptance criteria

The task is complete when:

- every roadmap intent has a fixture-backed deterministic evaluation;
- the real model is optional and all core correctness tests use fakes;
- interactive and JSON workflows are proven to share service/event semantics;
- process continuation and concurrent access are tested rather than assumed;
- restore safety has hard negative tests that fail the build on any pre-confirmation write or stale-plan commit;
- budgets, anti-loop, evidence, completeness, and degraded-source behavior have measurable assertions;
- performance and memory regressions have a baseline and an explicit review threshold;
- release-target builds and documented optional accelerator paths are checked in CI;
- failures produce actionable, structured diagnostics without leaking private content.

## References

- [GIB output implementation](../src/output.rs) — existing JSON, progress, and panic behavior to test.
- [GIB catalog tests and query code](../src/core/catalog/query.rs) — existing deterministic historical test surface.
- [GIB restore implementation](../src/core/restore.rs) — mutation and verification cases for regression coverage.
- [llama-cpp-rs repository](https://github.com/utilityai/llama-cpp-rs) — native runtime version/build drift to monitor.
- [reqwest Response documentation](https://docs.rs/reqwest/latest/reqwest/struct.Response.html) — download-stream failure/resume cases.
