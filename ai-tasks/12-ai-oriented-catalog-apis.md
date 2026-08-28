# Task 12 — Add AI-oriented historical catalog query APIs

## Roadmap position

This task gives the AI harness structured historical information without forcing it to parse CLI output or download file contents. It is the data foundation for temporal reasoning, investigative search, loss explanations, candidate ranking, and safe restore planning.

## Objective

Extend the catalog with internal, deterministic, metadata-only query primitives equivalent to:

    scan_catalog(...)
    get_ai_entry_history(...)
    summarize_changes(...)
    find_entries_by_content_hash(...)

The API must support queries with no textual search term, such as “all PDFs that existed last week,” and must return enough provenance for evidence and explanations. It must preserve the existing catalog’s current/all-history semantics, pagination, degraded-state reporting, and restorable-backup rules.

## Current repository analysis

src/core/catalog/model.rs defines schema version 1, EntryHistory, FileRevision, backup timestamps, content hashes, presence intervals, restorable-backup references, and catalog status. src/core/catalog/query.rs currently supports current/all-history scopes, directory children, token-based lookup with AND semantics, deterministic path ordering, path cursors, and latest parentless snapshot correction. It does not yet offer a general filter object, content-hash lookup, change summaries, or no-query scans.

src/core/catalog/storage.rs stores sharded entries, children, and token objects under the repository catalog root, encoded with msgpack/zstd and optionally encrypted. src/core/catalog/update.rs maintains revisions for present, changed, deleted, incremental, and parentless/full snapshot updates. src/commands/search.rs and src/commands/explore.rs add CLI-specific filtering/ranking/navigation on top of these APIs. Keep those commands backward compatible while adding reusable core query functions.

Repository snapshots use Unix-second timestamps and backup indexes resolve latest, prefixes, and full references. Catalog answers must distinguish indexed history from authoritative completeness: a degraded or pending catalog cannot be reported as a definitive absence.

## Query contracts

Define AI-facing DTOs independent of CLI response types:

- CatalogScanRequest with optional path prefix, normalized name tokens, extension set, content type set, size bounds, time constraint, state constraint, content hash, scope, cursor, and limit;
- CatalogScanResult with entries, next cursor, total/returned counts when known, catalog status, indexed-through backup/timestamp, truncation, and warnings;
- AiEntryHistory with stable entry ID, all relevant paths/revisions, first/last seen data, presence intervals, restorable references, and source-status metadata;
- ChangeSummary with added/modified/deleted/reappeared counts, content-hash continuity, affected paths/directories, time buckets, and evidence references;
- ContentHashMatch with hash, entry IDs, paths, revisions, backup/timestamp, restorable status, and deterministic ordering.

scan_catalog must allow all filters to be absent. An empty filter means a bounded scan, not “return nothing.” Require a caller-provided maximum page size or use a safe default. Apply filters in Rust, not in the model. Define whether a time predicate means overlap with an interval, presence at an instant, or change within an interval; use separate typed fields rather than one ambiguous string.

For “all PDFs that existed last week,” combine content type/extension and a typed existence interval. An entry matches if its presence interval overlaps the requested interval according to documented half-open interval rules. A file deleted before the interval must not match; a file present at any point should match an “existed during” query, while a “present at” query should use the revision active at the instant.

get_ai_entry_history must retrieve by stable entry ID or a validated path reference and return every revision needed to explain changes. Do not read file chunks. find_entries_by_content_hash must search the content-hash index or bounded revision scan and return path history, allowing rename/move discovery even when names differ. Normalize hashes to one format and reject malformed lengths.

summarize_changes should operate on catalog metadata and backup summaries. It should report intervals and categories, not assert a human cause. If detecting bursts, define a deterministic bucket/window algorithm and include the input range and threshold in the result. When a catalog is incomplete, include an explicit completeness qualifier.

## Implementation requirements

Prefer adding indexes for content hashes, timestamp buckets, extensions, or content types only when measurements show that scanning all entries is insufficient. Any index must have a schema version, deterministic rebuild path, update/delete handling, and degraded fallback. Do not weaken the existing sharded storage or encryption model.

Use the latest parentless snapshot correction already available, but do not make a query appear complete merely because that in-memory correction found a newer manifest. Return indexed-through metadata and correction warnings. Restorable status must be based on the catalog’s reference plus the existing chunk/index availability checks when the caller requests a restorable-only result.

Pagination must be stable under a fixed catalog revision. Prefer a cursor containing the last deterministic sort key and query fingerprint; reject a cursor for a different filter or catalog version. Sort results by a documented tuple, such as path/entry ID for scans and timestamp/revision ID for histories. Bound all allocations and prevent a no-query request from loading an unbounded entire catalog into memory.

Keep password/key handling at the existing catalog storage boundary. AI APIs receive the resolved repository access context; they must not expose keys in DTOs, logs, evidence, or prompts.

## Tests and acceptance criteria

Build synthetic catalog histories covering:

- present, modified, deleted, reappeared, renamed, and moved entries;
- full parentless snapshots and incremental changed-path indexing;
- exact interval boundaries, no-query scans, extension/content-type filters, path prefixes, size limits, and combined predicates;
- content-hash matches across multiple paths and backups;
- missing chunks and non-restorable revisions;
- pagination/cursor mismatch and deterministic order;
- ready, pending, degraded, empty, and encrypted catalogs;
- malformed hashes, invalid paths, excessive limits, and bounded output;
- unchanged behavior of existing search/explore commands.

The task is complete when the agent can answer metadata-only historical queries with no textual query, obtain complete entry histories and content-hash continuity, summarize changes with explicit completeness, and cite stable catalog/backup/revision IDs as evidence. No query in this task may download file contents or mutate the repository.

## References

- [GIB catalog data model](../src/core/catalog/model.rs) — existing history and revision fields.
- [GIB catalog query implementation](../src/core/catalog/query.rs) — pagination, scopes, status, and path-token behavior.
- [GIB catalog update implementation](../src/core/catalog/update.rs) — revision and deletion semantics.
- [GIB backup indexes](../src/core/indexes.rs) — backup references and Unix-second timestamps.

