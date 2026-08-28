# Task 19 — Implement safe intent-based AI restore in interactive and JSON modes

## Roadmap position

This task is the first AI workflow allowed to change user files. It must reuse the same intent, search, history, session, and frontend services while placing an immutable, deterministic safety boundary between language understanding and filesystem mutation.

## Objective

Implement RestorePlan, preview, SafetyGate, plan ID confirmation/commit, and post-restore verification. Provide two UX adapters over one core service:

- interactive mode presents a clear preview and prompts for confirmation;
- JSON mode returns a structured confirmation_required response and never blocks waiting for input.

The model may help identify intent and candidates, but it must never directly choose an unchecked path, invoke a write, or bypass a plan.

## Current repository analysis

src/core/restore.rs currently restores files concurrently, skips identical local files, writes other files through File::create, sets permissions, and returns RestoreStats. src/commands/restore.rs resolves a backup and selected paths, emits progress, and can optionally prune local files. src/commands/explore.rs bridges selected historical revisions to that command path. These APIs are useful but are not an AI safety layer: writes are not represented by an immutable plan, there is no plan ID, and direct overwrite/prune behavior must not be exposed to the model.

src/core/catalog/query.rs, src/core/catalog/model.rs, and Task 18 provide stable entry/revision/backup/content-hash history. Task 11 provides risk classification and permissions; Task 13 compiles Restore intent; Task 09/10 provide session/budget/state; Task 07 provides the interactive confirmation abstraction. Implement a reusable core restore service rather than invoking a CLI handler or shelling out.

## RestorePlan contract

Define an immutable, versioned plan containing:

- plan ID and schema/version;
- session/turn/conversation IDs and originating intent fingerprint;
- creation time, expiry, and repository/local target identity;
- exact source backup ID/hash, entry ID, revision ID, content hash, size, permissions, and chunk references for every item;
- normalized destination path for every item;
- current local precondition: missing, identical, or existing with content hash/metadata;
- overwrite count/bytes, create count, skipped count, risk class, and required confirmation reasons;
- catalog/source completeness and warnings;
- verification policy and expected postcondition.

The plan must be serializable and persisted before any mutation. A plan ID is an opaque identifier; it is not a permission by itself. The commit service must reload the plan, verify its schema, expiry, owner/scope, source references, and current preconditions, then require the exact plan ID plus an explicit approval token/result. Reconstructing an equivalent plan from user text must not count as confirmation.

## Preview and safety gate

Preview is strictly read-only. Resolve target candidates and temporal constraints deterministically, require a unique candidate or return ambiguity, verify that each source revision is restorable, normalize destination paths, and calculate all file operations. Never allow model-supplied absolute paths, traversal, symlink escapes, arbitrary target roots, or a path outside the explicit user-approved scope.

SafetyGate must be pure with respect to filesystem mutation. It classifies:

- create-only restores;
- overwrites of changed files;
- multiple files or large byte counts;
- sensitive or protected paths;
- ambiguous/non-restorable sources;
- target changes since preview;
- prune/deletion requests.

Default AI restore should refuse or separately confirm prune-local behavior; it must not infer deletion of extra local files from a restore request. Confirmation text must list affected paths/counts, source backup/timestamps, overwrite/skipped status, warnings, and verification policy. If a plan is stale, changed, incomplete, or ambiguous, return a new preview/clarification state rather than asking for a blind approval.

## Commit and atomic restoration

Expose commit_plan(plan_id, approval) as the only AI write entry point. Revalidate all plan preconditions while holding the appropriate local/plan lock. A changed local file, missing source chunk, changed catalog reference, expired plan, or mismatched approval must abort before mutation or report per-file results under a documented transactional policy.

Improve the underlying restore operation as needed to publish each file atomically: create a same-directory temporary file, stream/decompress verified chunks into it, hash and size-check the result, flush/sync it, then rename it into the destination. Preserve permissions only after content verification. Handle existing files, symlinks, directory creation, platform rename behavior, cancellation, and partial failures explicitly. Do not claim an all-files transaction unless the implementation truly rolls back or provides a durable per-file journal.

After commit, VerificationService must check existence, regular-file/type expectations, byte size, SHA-256 content hash, and permissions where supported. Return restored, skipped-identical, failed, and verification-failed statuses with evidence. If verification fails, do not silently retry with a different source or declare success.

## Interactive and JSON parity

Both adapters call preview, render the same plan DTO, and call commit_plan only after the appropriate approval. Interactive mode may accept a yes/no response through the shared confirmation abstraction. JSON mode returns a stable object such as status confirmation_required, plan_id, summary, required_confirmation_reasons, and next_action; it exits without reading stdin. A later automation call must provide the plan ID and explicit approval in a documented form.

Progress events must be structured and bounded. JSON stdout must contain no prompt, spinner, ANSI, native logs, or raw secret/path diagnostics beyond the approved plan. Interactive cancellation must leave the plan uncommitted and restore terminal state.

## Tests and acceptance criteria

Test:

- unique create-only restore, identical skip, changed-file overwrite, multi-file preview, and ambiguous candidate;
- plan serialization, stable IDs, expiry, ownership/scope, and tamper detection;
- traversal, absolute path, symlink, protected-root, source-not-restorable, missing-chunk, and changed-precondition rejection;
- JSON confirmation_required with no stdin blocking and interactive confirmation parity;
- commit requires exact plan ID and approval, rejects stale/expired/replayed plans, and cannot be triggered by free-form model text;
- atomic temp-file/rename behavior, interruption/crash-like states, concurrent local changes, partial failure, and cancellation;
- content hash/size/permissions verification and failure reporting;
- prune-local remains explicit and separately gated;
- existing non-AI restore command regression tests continue to pass;
- evidence and trace records identify preview, approval, commit, and verification without exposing secrets.

The task is complete when an AI restore request produces an inspectable immutable plan, cannot mutate before explicit approval, behaves identically in interactive and JSON modes, commits only by plan ID after revalidation, and reports verified outcomes. No language-model output may be treated as filesystem authorization.

## References

- [GIB restore implementation](../src/core/restore.rs) — existing restore mechanics and gaps the safe service must address.
- [GIB restore command](../src/commands/restore.rs) — current CLI selection/progress behavior to preserve behind reusable services.
- [Rust File::sync_all](https://doc.rust-lang.org/std/fs/struct.File.html#method.sync_all) and [Rust rename](https://doc.rust-lang.org/std/fs/fn.rename.html) — durable atomic file publication.
- [GIB catalog model](../src/core/catalog/model.rs) — source revision/content-hash data for exact plans.

