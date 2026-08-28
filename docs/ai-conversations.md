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
