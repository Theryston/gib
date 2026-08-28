# Task 05 — Introduce the initial gib ai command

## Roadmap position

This is the first user-facing AI task and should land immediately after Tasks 01–04. It intentionally provides a narrow direct-chat workflow: one service, one active conversation, one local backend, and identical persistence semantics in interactive and JSON modes. It must not prematurely depend on the full agent harness.

## Objective

Deliver these commands:

    gib ai
    gib ai --mode json --message "hello"

Both paths must use the same ConversationService, resolve the same active conversation by default, invoke the same AiBackend, and persist user and assistant messages. The interactive path may remain a simple streaming chat loop at this stage. JSON mode must be automation-safe and must never wait for terminal input.

## Current repository analysis

src/main.rs currently detects the output mode from raw arguments, initializes the global OutputMode, installs a JSON-aware panic hook, and dispatches existing command handlers. Add an explicit ai subcommand and a src/commands/ai.rs handler without creating a second top-level argument parser. Preserve the existing global --mode behavior and ensure the requested invocation parses whether --mode appears before or after the ai command according to the project’s established clap configuration.

src/output.rs emits JSON event envelopes through emit_event and sends errors/warnings to the appropriate stream. It also owns progress behavior. The AI command must not write a human sentence directly to stdout in JSON mode. Define an AI response/event contract on top of the existing output mechanism and make the interactive renderer a separate adapter over the same internal turn events.

The conversation store from Task 04 is global state under ~/.gib/ai. The command must work from a directory with no gib.toml, no repository, and no configured storage. Model resolution and installation come from Task 01; inference comes from Task 02; plain text is sufficient, so Task 03’s structured generator is optional for this first command but the command should use the common prompt/message boundary.

## CLI contract

Add an Ai command with:

- optional --message for a single non-interactive turn;
- optional --conversation ID, which selects a conversation for this invocation without changing the global active ID;
- the existing --mode flag, with Json requiring --message unless a future explicit input protocol is added;
- a default interactive mode when no message is supplied and stdout/stderr are TTY-compatible.

For "gib ai":

1. Resolve the explicit conversation or active conversation.
2. Create a first default conversation only if the product policy says fresh chat should do so; make the policy deterministic and test it.
3. Ensure the active model is installed and loadable.
4. Start an interactive turn loop or one-turn session, showing streamed assistant text.
5. Append the user message before generation using a pending/committed status or equivalent.
6. Stream the assistant response.
7. Append the complete assistant message only after a successful terminal event. Record cancellation or failure as a bounded status/diagnostic without pretending an answer was completed.

For "gib ai --mode json --message hello":

- perform exactly one turn and exit;
- emit structured started/progress/delta/finished events only if the documented JSON contract includes them;
- emit one final response object containing conversation ID, turn/message IDs, model ID, assistant text, finish reason, and usage when available;
- never emit ANSI, a spinner, a prompt, a progress bar, native runtime logs, or an unstructured greeting to stdout;
- on failure, emit the repository-compatible structured error and a non-zero exit status;
- never block waiting for confirmation, a password, or another line of input.

Choose and document whether JSON output is line-delimited event JSON or one final JSON document. It must be stable for automation and compatible with existing output.rs conventions. If shared progress events are used, callers must be able to distinguish them from the final response without parsing human text.

## Shared service flow

Create an AiTurnService that accepts a conversation selector, user message, backend, prompt policy, cancellation source, and output sink. Both adapters call it. The service owns message persistence and turn lifecycle; the terminal UI and JSON serializer only consume events. Do not implement one persistence path in the interactive loop and another in the JSON branch.

Use a request ID and idempotency guard so a retry caused by a transport or output failure cannot append the same user message twice. Handle a conversation revision conflict by reloading before generation if no model output has begun; once generation starts, fail safely rather than appending to a different revision.

Keep the initial turn simple. Do not let the model invoke tools, perform restore, invent historical answers, or select a different conversation. A direct-chat assistant may state that repository investigation capabilities are not yet available.

## Tests and acceptance criteria

Add CLI and service tests for:

- argument parsing for both commands, --conversation, missing JSON message, and no repository;
- interactive and JSON adapters calling the same fake turn service;
- user/assistant persistence on success, cancellation, backend failure, and process restart;
- active conversation use and explicit override without changing active state;
- streaming reconstruction and one terminal event;
- missing/uninstalled model, corrupted model metadata, and backend errors;
- JSON stdout parseability, absence of ANSI/prompts/native logs, structured stderr/error behavior, and non-zero failures;
- two sequential process invocations continuing the same conversation.

The task is complete when both documented invocations work against the local model, share all conversation and runtime services, persist messages durably, and remain safe for scripts. The command may be visually basic, but it must establish the stable service boundaries used by Tasks 06 and 07.

## References

- [clap derive tutorial](https://docs.rs/clap/latest/clap/_derive/_tutorial/index.html) — subcommand and option modeling.
- [GIB output implementation](../src/output.rs) — repository JSON event and progress conventions.
- [GIB command dispatch](../src/main.rs) — the integration point for the new command.

