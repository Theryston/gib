# Task 06 — Add AI conversation management

## Roadmap position

This task extends the persistent conversation service and the initial ai command. It must make conversations discoverable and scriptable without duplicating storage logic or changing the meaning of the active conversation.

## Objective

Deliver the following operations:

    gib ai conversation new
    gib ai conversation list
    gib ai conversation select <id>
    gib ai conversation show <id>
    gib ai conversation rename <id> <title>
    gib ai conversation delete <id>

Also support --conversation ID on AI operations that consume a conversation. The explicit selector is an invocation-scoped override; it must never change the global active conversation. Interactive and JSON modes must call the same ConversationService methods and expose the same state transitions.

## Current repository analysis

Task 04 introduces ConversationStore and ConversationService under the global ~/.gib/ai state directory. src/main.rs currently has one top-level clap command enum and dispatches commands in a single match. Add a nested AiConversation subcommand in the existing parser and keep command handlers thin. Do not make the CLI read or mutate conversation files directly.

src/output.rs provides JSON event envelopes and an error policy, but existing commands have different response shapes. Define stable AI management response types and serialize them through the shared output layer. The existing project configuration in src/config/local.rs is unrelated to conversation selection; selecting a conversation must remain valid outside a project and must not write project gib.toml.

## CLI behavior

Define exact argument semantics:

- conversation new accepts an optional title and returns the new stable ID, title, timestamps, and whether it became active;
- conversation list returns deterministic summaries including ID, title, created/updated times, message count, last role, and active status;
- conversation select requires an existing ID, updates only global AI config, and returns the selected summary;
- conversation show requires an existing ID and returns metadata plus user-visible messages in chronological order; it should support a bounded output policy so a very large conversation cannot flood a script;
- conversation rename validates a non-empty, bounded title and updates only metadata;
- conversation delete requires an existing ID and removes or tombstones the conversation according to the storage policy.

The default new-conversation policy must be explicit. A recommended policy is that new creates and selects the conversation only when the user explicitly asks for it, while the first direct chat may create a default conversation under Task 05’s documented fresh-install policy. If deleting the active conversation, either select the most recently updated remaining conversation deterministically or clear active_conversation_id; document and test the choice. Never silently point the active setting at a deleted ID.

The global --conversation option should be accepted by chat and future AI workflow commands. It resolves the target for one invocation and does not alter active_conversation_id. Reject an unknown ID before model loading or any message write. For automation, JSON output must make the resolved conversation ID explicit.

## JSON and interactive contracts

Interactive mode may display concise human-readable confirmations and use a confirmation abstraction for deletion. JSON mode must return structured data such as operation, conversation, active_conversation_id, and warnings. It must never prompt, read from stdin unexpectedly, or mix confirmation text with JSON stdout. For a destructive delete in JSON mode, either require an explicit --yes flag or return a confirmation_required object without changing state. Choose one policy and use it consistently with Task 19’s future safety gate.

Errors need stable codes for not-found, invalid-id, invalid-title, active-selection-conflict, malformed-conversation, newer-schema, locked, and persistence-failure. Keep message content out of normal diagnostics; show returns it intentionally and should still avoid hidden reasoning, raw prompts, native logs, and raw tool payloads.

## Implementation details

Reuse the store’s atomic writes, schema migrations, locks, revision checks, and permission rules. List should scan only the conversation directory, ignore known temporary files, and report malformed documents as warnings rather than crashing the entire list. Ordering must be deterministic, for example updated_at descending followed by conversation_id ascending; do not rely on filesystem directory order.

Use a dedicated command response DTO rather than serializing internal store structs directly. This allows future fields to be added without leaking lock paths, absolute local paths, or implementation-only revision data. Include schema/version fields in JSON responses where automation needs to distinguish changes.

Interactive deletion and rename should restore the terminal and report errors cleanly even if the command is interrupted. Conversation operations must not initialize llama.cpp or download a model. They should remain fast and usable when the model is unavailable.

## Tests and acceptance criteria

Add parser, service, and process-level tests for:

- every documented command and its missing/extra argument behavior;
- creation with and without a title, deterministic IDs, list ordering, active markers, and timestamps;
- selecting a conversation and proving that an explicit --conversation operation leaves the active setting unchanged;
- show ordering, bounded output, message role/status serialization, and absence of hidden fields;
- rename validation, Unicode title handling, and revision updates;
- active-conversation deletion policy, deleting the last conversation, and repeated deletion;
- malformed and newer-schema files in list/show;
- JSON mode with no stdin prompt, stable response/error codes, no ANSI sequences, and parseable stdout;
- concurrent select/rename/delete operations and lock/revision conflicts;
- operation behavior with no project repository and no installed model.

The task is complete when users and automation can manage conversations entirely through the documented subcommands, every operation uses the common service, the active conversation is explicit and recoverable, and an invocation-scoped selector never causes an accidental global state change.

## References

- [clap derive tutorial](https://docs.rs/clap/latest/clap/_derive/_tutorial/index.html) — nested subcommand modeling.
- [GIB conversation storage task](04-persistent-conversations-and-active-state.md) — the persistence and migration contract this task must reuse.
- [GIB output implementation](../src/output.rs) — JSON event and error conventions.

