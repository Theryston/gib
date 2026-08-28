# Task 04 — Add persistent AI conversations and active conversation state

## Roadmap position

This task establishes durable chat state before the first public gib ai command. It is independent of AgentSession: a conversation survives process exit and contains user-visible dialogue, while a later agent session contains the operational state of one turn.

## Objective

Implement ConversationStore and ConversationService with one file per conversation, stable IDs, metadata, durable context, a global active-conversation setting, atomic writes, locking, and migrations. The service must be safe when two GIB processes read or update conversations concurrently and must not depend on a project repository being available.

## Current repository analysis

The current configuration model in src/config/local.rs is project-local gib.toml, discovered relative to the working directory and validated with deny_unknown_fields. It stores repository, backup, live, and restore settings. Do not put global AI conversations in that file or make gib ai require a repository. src/config/resolve.rs handles repository storage and keys and may prompt in interactive mode; conversation persistence must not reuse repository encryption assumptions unless a separate feature is explicitly designed.

src/fs/fs.rs and src/fs/local.rs provide asynchronous repository file operations and a repository CAS lock. Those APIs are valuable references for atomic and versioned writes, but global conversation files need an explicit user-level path and a lock that covers the whole document update. The existing repository catalog uses schema versions and encoded objects; follow that migration discipline while choosing a human-debuggable conversation format.

## Storage layout and data contract

Use a global directory such as ~/.gib/ai. Keep config.toml for AI-wide settings and conversations/ for per-conversation documents. A conversation file may be JSON or another documented text format, but it must be stable, inspectable, and independently migratable. A suggested top-level contract is:

- schema_version;
- conversation_id;
- title;
- created_at and updated_at in UTC;
- revision, incremented on every successful mutation;
- optional model and prompt metadata;
- messages, each with stable message ID, role, timestamp, content, and status;
- durable_context, containing only explicit summaries, user preferences, artifact/evidence references, and bounded facts;
- optional archived/deleted metadata if deletion is soft internally.

Use opaque stable IDs generated once at creation. Do not derive an ID from a mutable title. Validate IDs before constructing paths and reject separators, traversal, control characters, and unexpected length. File names should use the ID, not user-controlled titles.

Conversation messages are user-visible text only. Do not persist hidden chain-of-thought, raw model scratchpads, unbounded prompt expansions, native logs, or entire tool payloads. Store references to evidence or artifacts and compact summaries where needed. Enforce configurable message and total-context limits so one conversation cannot consume unbounded memory.

The global config should contain schema_version, active_conversation_id, and AI/runtime settings that later tasks extend. A missing active conversation is valid on a fresh installation; the service may create a default conversation on first chat or report a clear no-active-conversation state according to the command contract.

## Service API

Implement a store that owns serialization, path validation, lock acquisition, atomic persistence, and migrations. Implement a service that owns user operations:

- create with optional title;
- list summaries in deterministic order;
- load by ID;
- append a user or assistant message with an expected revision;
- rename;
- delete;
- select active conversation;
- resolve an explicit conversation ID or the configured active ID without mutating the active selection.

An append must use optimistic revision checking inside the lock. If the on-disk revision differs from the caller’s expected revision, return a conflict and let the command decide whether to reload or retry. Do not merge two assistant responses silently. Listing should tolerate one malformed conversation by returning a structured warning and continuing with valid files; loading the malformed file must remain an actionable error.

## Atomicity, locks, and migrations

Write temporary files in the same directory, flush and sync them, rename over the target, and sync the directory where supported. Never truncate a live conversation file before a replacement is durable. Use an explicit per-conversation lock and a config lock. fs4 is a possible cross-platform dependency; if used, perform blocking lock calls on a dedicated blocking thread or otherwise avoid blocking the async executor. A lock file created with create_new is acceptable only if stale-lock recovery, ownership, and crash behavior are documented and tested.

Every read must migrate in memory from known older schema versions. Every write must emit the current schema version. Migrations must be pure, ordered, idempotent, and tested with fixtures. Unknown future versions must not be overwritten; report that the file is newer than this binary. Config migration must never select a deleted or malformed conversation without a deterministic repair policy.

Apply restrictive permissions to the directory and files where supported. Do not log message content, tokens, keys, or full paths at normal levels. If a user explicitly asks for show, return content through the command’s chosen output mode with structured JSON in JSON mode.

## Tests and acceptance criteria

Test:

- fresh directory creation, stable IDs, title changes, and deterministic listing;
- append, load, revision increments, stale revision conflicts, and concurrent writers;
- active selection, missing active conversation, explicit override, and deletion of the active conversation;
- malformed file isolation, unknown schema version, and every migration fixture;
- interrupted write with a temp file, missing sidecar/config, and recovery;
- path traversal, control characters, oversized titles/messages, and permissions;
- no persistence of hidden reasoning or raw tool output;
- JSON representations that remain parseable and interactive operations that never receive ANSI data from the store;
- two independently spawned processes appending safely to the same conversation.

The task is complete when conversation data survives process boundaries, a fresh user can select and resume one active conversation, concurrent updates cannot lose acknowledged messages, and all mutations are atomic and migratable. AgentSession must remain a separate future abstraction.

## References

- [fs4 file-lock documentation](https://docs.rs/fs4/latest/fs4/) — cross-platform advisory locking and blocking-call considerations.
- [Rust File::sync_all](https://doc.rust-lang.org/std/fs/struct.File.html#method.sync_all) and [Rust rename](https://doc.rust-lang.org/std/fs/fn.rename.html) — durable replacement writes.
- [toml crate documentation](https://docs.rs/toml/latest/toml/) — configuration serialization and parsing.

