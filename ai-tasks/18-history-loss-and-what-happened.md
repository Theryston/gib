# Task 18 — Implement history analysis, File Loss Explanation, and What Happened?

## Roadmap position

This task uses the catalog’s revision history to explain changes and disappearance without inventing causes. It is the bridge between intelligent search and a safe, intent-based restore.

## Objective

Create a normalized timeline, disappearance-window analysis, change-burst detection, rename/move inference by content hash, and evidence-backed natural explanations. Expose workflows for ExplainLoss, ExplainHistory, and “What Happened?” with a strict separation between observed facts and qualified inferences.

## Current repository analysis

src/core/catalog/model.rs already stores EntryHistory, FileRevision, first/last seen backup/timestamps, presence intervals, content hashes, latest restorable backups, and last-change references. src/core/catalog/update.rs creates/closes revisions for present, changed, deleted, reappeared, full, and incremental snapshots. src/core/catalog/query.rs retrieves histories and reports catalog status. Backup summaries and timestamps are available through src/core/indexes.rs and metadata types.

The current commands can show history and restore selected files, but they do not expose a normalized event model or causal explanation. Existing restore writes can mutate local files directly; this task must remain read-only. Do not infer file content, user identity, process cause, or deletion mechanism from metadata alone.

## Timeline model

Define normalized event types:

- first_seen;
- revision_started;
- revision_ended or superseded;
- last_seen;
- disappeared;
- reappeared;
- path_changed or probable_rename_move;
- restorable_reference_changed;
- catalog_gap or source_degraded.

Every event must include stable entry/revision/backup IDs where available, UTC timestamp or timestamp interval, path, content hash/size/type when authoritative, source status, and evidence IDs. Use intervals when the underlying snapshots do not reveal an exact event time.

For each revision, normalize the half-open presence interval from present_from_backup/timestamp to present_until_backup/timestamp. A disappearance window begins after the last snapshot in which the entry was present and ends at the first indexed snapshot in which it is absent, if such a snapshot exists. Do not call the first absent snapshot the deletion time. If the catalog is not continuous or is degraded, widen or mark the window unknown and carry that limitation into the explanation.

## Change analysis

Implement a deterministic ChangeAnalyzer that accepts an entry/history scope and optional TemporalConstraint. It should produce:

- added/modified/deleted/reappeared counts;
- revision transitions with content-hash and size/type changes;
- affected path/directories;
- change bursts using a documented bucket/window threshold;
- gaps, incomplete coverage, and non-restorable references;
- candidate rename/move links.

For rename/move inference, match equal normalized content hashes across entries whose snapshot/backup timestamps are close or whose presence intervals overlap a transition. Require a configurable uniqueness rule or mark the relationship ambiguous when many paths share the same hash. Label the result “probable rename/move” or equivalent; a matching hash proves content continuity, not user intent or the exact filesystem operation.

Do not download chunks or inspect file contents. Metadata can establish that two revisions have the same content hash, but it cannot establish why a file changed or who changed it.

## Explanation contract

Define a structured ExplanationResult with:

- answer type and target;
- concise observed facts;
- qualified inferences;
- chronological timeline;
- disappearance/change windows;
- probable rename/move relationships;
- restorable options and limitations;
- evidence IDs and source completeness;
- a natural-language summary generated only after deterministic analysis.

Facts must be generated from catalog/backup/filesystem observations and retain source identifiers. Inferences must cite the supporting facts and use qualified language such as may, likely, or consistent with. Explicitly list what cannot be concluded, for example exact deletion time, application/user cause, or history beyond the indexed range.

The model may select salient facts or phrase the explanation through a versioned prompt. It must receive structured timeline/evidence and cannot add unsupported events, timestamps, causes, or candidates. Validate its output and fall back to a deterministic summary if generation fails. A user asking “what happened?” without a target should produce a routing clarification, not a scan of unlimited personal data.

## Workflow behavior

ExplainLoss should resolve a target/history, locate the last known present revision, calculate the disappearance window, search for content-hash/path continuity, and report restorable status. ExplainHistory should summarize all relevant changes for an entry or bounded scope. “What Happened?” may combine target resolution, temporal filtering, burst analysis, and a compact explanation.

A degraded/pending catalog must produce an incomplete answer with the indexed-through point. No-match, never-seen, and not-indexed are distinct outcomes. Preserve source status in JSON and render the distinction interactively.

## Tests and acceptance criteria

Use synthetic backup/catalog histories for:

- first appearance, multiple content revisions, deletion, reappearance, and later deletion;
- exact boundary timestamps and missing snapshots;
- full parentless and incremental indexes;
- same-hash path changes, duplicate-hash ambiguity, and non-restorable revisions;
- change bursts and threshold boundaries;
- degraded/pending/partial catalogs and indexed-through limits;
- facts versus inferences, unsupported causal questions, malformed explanation output, and deterministic fallback;
- JSON/interactive parity and proof that no file chunks are read or files mutated.

The task is complete when GIB can explain what its metadata proves, bound the period in which disappearance occurred, surface probable content-preserving path changes, and clearly separate facts, inferences, and unknowns. It must never present a guessed cause or exact deletion time as historical truth.

## References

- [GIB catalog model](../src/core/catalog/model.rs) — revision intervals, content hashes, and restorable references.
- [GIB catalog update logic](../src/core/catalog/update.rs) — present/deleted/reappeared revision transitions.
- [GIB catalog query API](../src/core/catalog/query.rs) — history retrieval and completeness status.
- [GIB backup metadata](../src/core/metadata.rs) — snapshot timestamps and file metadata.

