# Persistent AI Conversations

Task 04 stores AI conversation state globally, independently of the current
working directory and independently of a GIB repository.

## Layout

The default layout is:

~~~text
~/.gib/ai/
├── config.toml
└── conversations/
    ├── conv-<opaque-id>.json
    ├── .conv-<opaque-id>.lock
    └── .creation.lock
~~~

The existing AI model configuration shares config.toml. Its current serialized
key is schema_version; the previous version key is accepted as a compatibility
alias and is written back in the current form. Conversation state adds
active_conversation_id without changing repository-local gib.toml.

Conversation file names contain only validated opaque IDs. Titles never
participate in path construction, so renaming a conversation does not move or
replace its file.

## Conversation document

Each JSON document contains:

- schema_version, conversation_id, title, UTC created_at and updated_at;
- a monotonically increasing revision;
- optional model and prompt identity metadata;
- user-visible messages with stable IDs, user or assistant roles, UTC
  timestamps, text content, and completion status. Status can be `complete`,
  `interrupted`, or `pending`; pending is reserved for a user message whose
  generation was interrupted before a terminal result was durably recorded.
  Messages may also carry an opaque turn ID for retry idempotency; this ID is
  operational metadata and is never included in a model prompt;
- bounded durable_context containing only an explicit summary, user
  preferences, artifact references, evidence references, and facts;
- an archived flag reserved for lifecycle metadata.

There is no persisted system/tool role, hidden-reasoning field, raw tool
payload, prompt expansion, native diagnostic, or token log. Conversation
callers must provide visible text only.

## Mutation and concurrency

ConversationService exposes create, list, load, append, rename, context
replacement, delete, active selection, and explicit/active resolution.
Append, rename, and context replacement require the caller's expected
revision. The store acquires a per-conversation lock, reloads the document
inside that lock, compares the on-disk revision, and returns a structured
revision conflict instead of silently merging concurrent assistant responses.
Turn finalization marks the pending user message and appends the assistant
message inside the same locked mutation. A later invocation can therefore
resume a pending turn left by a crashed process without duplicating its user
message.

The store uses a separate configuration lock for config.toml. File
replacements are written to a same-directory temporary file, flushed with
sync_all, renamed over the destination, and followed by a directory sync on
Unix. Temporary files are ignored by listing and do not replace a valid
conversation after an interrupted write.

Locks are ownership-token files with a fifteen-minute lease. A crashed process
can leave a lock behind; a later operation may reclaim it after the lease
expires. A lock owner checks its token before cleanup, so reclaiming a stale
lock cannot cause the old owner to delete a newer lock.

Conversation service methods run blocking file and lock work through
tokio::task::spawn_blocking. This keeps lock polling and synchronous disk
operations off the async executor.

## Migrations and limits

Reads accept the known version-zero legacy shape and migrate it in memory.
Legacy IDs, message IDs, timestamps, statuses, and durable context names are
normalized without changing the source file. The next successful mutation
writes only the current schema. Unknown future versions are reported and are
never overwritten.

The store bounds IDs, titles, individual messages, message count, durable
context, and complete document size. Objects are protected with restrictive
user-only permissions where the platform supports them. Listing returns valid
summaries and structured warnings for malformed files rather than failing the
entire directory; direct loading keeps the actionable error.

## Management commands

GIB keeps AI conversations in the user-level `~/.gib/ai` state directory. The
conversation commands do not require a repository, a project `gib.toml`, an
installed model, or llama.cpp to be available.

## Commands

```text
gib ai conversation new [TITLE]
gib ai conversation list
gib ai conversation select <ID>
gib ai conversation show <ID>
gib ai conversation rename <ID> <TITLE>
gib ai conversation delete <ID> [--yes]
```

`new` creates a stable opaque ID and selects the new conversation as the
global active conversation. When no title is supplied, GIB uses `New
conversation`. Titles are trimmed, must be non-empty, and are bounded by the
conversation storage limits.

`list` is ordered by most recently updated conversation first, with the stable
ID as the deterministic tie-breaker. It reports the ID, title, timestamps,
message count, last message role, and active marker. A malformed or newer
conversation document is isolated as a warning instead of hiding valid
conversations.

`select` changes only the global `active_conversation_id` in the AI config.
`rename` changes only conversation metadata. Both operations require the
conversation to exist and leave message content untouched.

`show` returns messages in their persisted chronological order. To keep a
large conversation from flooding a terminal or script, the default response
contains at most 128 messages and 128 KiB of serialized message data. The
limits can be lowered or raised up to 4,096 messages and 1 MiB:

```text
gib ai conversation show <ID> --limit 40 --max-bytes 65536
```

The response reports `truncated: true` when either bound stops the output.
Message DTOs contain only user-visible fields: message ID, role, timestamp,
content, and terminal status. Operational turn IDs, hidden reasoning, raw
prompts, tool payloads, lock paths, and absolute storage paths are not exposed.

## Active-conversation policy

`delete` uses a two-mode confirmation policy:

- interactive mode asks for confirmation unless `--yes` is supplied;
- JSON mode never reads stdin. Without `--yes`, it returns a structured
  `confirmation_required: true` response and does not change state.

Deleting the active conversation removes its document and clears
`active_conversation_id`. GIB does not silently choose another conversation.
The next direct chat follows the Task 05 fresh-install policy and creates a
new default conversation if no active conversation exists. `new` always
selects the conversation because selection is the explicit purpose of that
command.

## Invocation-scoped chat selection

The chat command accepts an explicit conversation without changing global
state:

```text
gib ai --conversation <ID>
gib ai --mode json --conversation <ID> --message "continue there"
```

An explicit ID is validated before model installation and before any message
write. In JSON mode, turn events and the final response include the resolved
conversation ID.

## JSONL response contract

Management commands emit one shared output-layer event on stdout:

```json
{
  "type": "ai_conversation",
  "data": {
    "schema_version": 1,
    "operation": "list",
    "conversations": [],
    "active_conversation_id": null
  }
}
```

Each conversation-management DTO has a stable `schema_version`, and warnings
are included in the response data. Errors are emitted through the standard
JSON error envelope on stderr with stable codes such as
`conversation_not_found`, `invalid_conversation_id`, `invalid_title`,
`active_selection_conflict`, `malformed_conversation`, `newer_schema`,
`locked`, and `persistence_failure`.
