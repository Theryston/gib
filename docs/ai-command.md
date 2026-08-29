# Initial `gib ai` command

Task 05 adds a narrow local direct-chat command on top of the model registry,
in-process runtime, and persistent conversation service.

## Invocations

The interactive command is:

```text
gib ai
```

It creates and selects the first conversation when no active conversation
exists, then reads one message per line until Ctrl+D. Each assistant response
is streamed to the terminal. Blank lines are ignored. Ctrl+C during generation
cancels the turn and saves an interrupted assistant message.

A single turn can be run without terminal input:

```text
gib ai --mode json --message "hello"
```

`--conversation <ID>` selects a conversation for that invocation only. It does
not change `active_conversation_id`:

```text
gib ai --mode json --conversation conv-example --message "continue there"
```

AI state is global under `~/.gib/ai`; the command does not require a repository,
`gib.toml`, storage configuration, or a particular working directory.

## JSONL contract

JSON mode writes newline-delimited JSON objects to stdout. Every line follows
the repository envelope used by `src/output.rs`:

```json
{"type":"<event-type>","data":{}}
```

The model installer may emit `ai_model_install` events while the verified local
model is being prepared. These are structured progress records and never
contain a spinner, ANSI escape sequence, native llama.cpp log, or human-only
text.
The command also emits `ai_model_load` events while llama.cpp maps and loads
the verified GGUF into memory.

The turn service emits lifecycle records as `ai_turn` events. A successful
turn has one `started` event, zero or more `progress` and `delta` events, and
one `ai_response` event. `delta` text concatenates to the final `text` value.
For example:

```json
{"type":"ai_turn","data":{"event":"started","conversation_id":"conv-...","turn_id":"turn-...","user_message_id":"msg-...","model_id":"qwen3.5-4b-q8-0"}}
{"type":"ai_turn","data":{"event":"delta","conversation_id":"conv-...","turn_id":"turn-...","text":"Hello"}}
{"type":"ai_turn","data":{"event":"progress","conversation_id":"conv-...","turn_id":"turn-...","usage":{"prompt_tokens":12,"completion_tokens":1,"total_tokens":13}}}
{"type":"ai_response","data":{"conversation_id":"conv-...","turn_id":"turn-...","user_message_id":"msg-...","assistant_message_id":"msg-...","model_id":"qwen3.5-4b-q8-0","text":"Hello","finish_reason":"end_of_generation","usage":{"prompt_tokens":12,"completion_tokens":1,"total_tokens":13}}}
```

The response object contains the stable conversation, turn, user-message, and
assistant-message IDs, the model ID, the reconstructed assistant text, the
runtime finish reason, and aggregate usage. Runtime duration is intentionally
not part of the persistence or command contract.

When a turn is cancelled or fails after the user message is persisted, the
service stores a bounded assistant message with `status = "interrupted"` and
emits a terminal `cancelled` or `failed` `ai_turn` event. The command then emits
one structured error object to stderr and exits non-zero. A JSON invocation
never asks for another line, a password, or a confirmation.

## Persistence and retries

The user message is committed with `status = "pending"` before generation.
After a successful terminal runtime event, one atomic conversation mutation
marks that user message complete and appends the complete assistant message.
Cancellation and failure use the same atomic mutation to mark the user message
complete and append one bounded assistant message with
`status = "interrupted"`.

If a process dies after the pending user message is written, a later
invocation with the same message resumes that persisted turn and does not
append a second user message. A different message is rejected while that turn
is pending, so the conversation cannot silently reorder work. An explicitly
reused opaque `turn_id` is also rejected after a terminal turn with
`turn_already_recorded`. The ID is operational metadata, is not sent to the
model, and prevents duplicate persistence for request retries.

A revision conflict is retried once before generation. Once output has
started, the service never reloads and merges another process's revision.

The direct-chat prompt contains a fixed local-assistant system instruction and
all persisted user-visible messages mapped through the shared `AiMessage`
boundary. It has no tools, restore capability, agent loop, hidden reasoning,
or structured-output requirement. Those capabilities belong to later tasks.

## Model preparation

The first command invocation calls the Task 01 model manager. If the active
model is not installed and verified, it downloads the built-in Qwen3.5 model
from the GIB bucket, resumes a valid partial download, verifies the registered
size, and publishes it atomically before llama.cpp loads it. The
built-in manifest is:

```text
https://public.trygib.org/ai/models/Qwen3.5-4B-Q8_0.gguf
```

The raw artifact is not considered installed until its sidecar metadata and
size check also pass. Model installation intentionally does not hash the
multi-gigabyte artifact during startup.

## Interactive terminal frontend

Hardware detection and runtime profile selection are documented in
[`ai-runtime-profiles.md`](ai-runtime-profiles.md). The selected profile and
resolved context/thread/batch/GPU settings are shown in the interactive header
and in the `ai_runtime` JSON status event.

The interactive form uses a full-screen alternate terminal with an explicit
conversation viewport, a multiline composer, streaming assistant output, and a
status footer. It consumes the same `AiTurnService` events as JSON mode; the
frontend does not create a second persistence or generation path.

```text
gib ai
```

Input behavior:

- `Enter` submits a non-empty message.
- `Ctrl+J` inserts a newline without submitting, so multiline input works in
  terminals that do not provide a portable `Shift+Enter` sequence.
- Multiline terminal paste preserves newlines while ignoring carriage returns
  and other control characters.
- Arrow keys, `Home`, `End`, `Backspace`, and `Delete` edit the composer.
- `Up` and `Down` navigate the local input history when the composer is empty;
  otherwise they move the cursor between multiline rows.
- `PageUp` and `PageDown` scroll the transcript. New output follows the newest
  content again after scrolling back to the bottom.
- `Ctrl+C` cancels an active generation. When idle it clears a non-empty draft;
  with an empty draft it exits the frontend.
- `Ctrl+D` deletes the character under the cursor when a draft exists and exits
  only when the composer is empty and no confirmation is pending.
- Typing `/` opens a local command palette. The list is filtered as the command
  prefix grows; `↑`/`↓` changes the highlighted option and `Tab` inserts the
  selected command. `Enter` runs a complete command, or completes an incomplete
  command first.

The header identifies the conversation and model. The status footer shows the
generation/cancellation state, token usage when available, viewport-following
state, and the available slash-command hint. Long messages, code, paths, and
very narrow terminals are wrapped to the current viewport width. Visible text
is sanitized before rendering, and hidden model reasoning is never displayed.

Slash commands are handled locally and do not consume a model turn:

```text
/help
/new [title]
/list
/select <id>
/switch <id>
/rename <id> <title>
/clear
/status
/exit
```

`/new` and `/select` update the global active conversation through
`ConversationService`, matching the conversation-management command. The
`--conversation <ID>` option remains an invocation-scoped override when the
interactive command is started and does not itself change global state.

The frontend enters raw mode only after model preparation succeeds and restores
raw mode, the alternate screen, and cursor visibility through a terminal guard
on normal exit and errors. If standard input/output/error are not TTYs,
interactive mode refuses to enter raw mode and asks the caller to use a JSON
message invocation instead. JSON mode never initializes this frontend, emits
terminal escape sequences, or waits for interactive confirmation.

## Manual verification matrix

Use a built binary and an installed local model for the interactive checks:

```bash
cargo build
./target/debug/gib ai
```

On Linux, macOS, and Windows terminals, verify the following sequence:

1. The alternate screen opens with a header, prompt, status indicator, and
   slash-command hint.
2. Send a normal message and confirm that assistant text appears incrementally
   without duplicated final output.
3. Press `Ctrl+J` between two lines, then `Enter`, and confirm the complete
   multiline message is one persisted user message.
4. Use `PageUp`/`PageDown`, resize the terminal, and confirm the draft and
   transcript remain intact.
5. During generation press `Ctrl+C`; confirm cancellation returns to a usable
   prompt. Press `Ctrl+C` with a draft to clear it, then with an empty draft to
   exit.
6. Re-enter with `gib ai` and verify the conversation remains available. Try
   `/help`, `/status`, `/list`, `/new Manual test`, `/rename <id> Renamed`,
   `/select <id>`, `/clear`, and `/exit`.
7. After every exit path, type a shell command and confirm the cursor is
   visible, the shell is not in raw mode, and the alternate screen was left.

For redirected or dumb terminals, verify that the command exits with a clear
non-interactive error rather than enabling raw mode:

```bash
printf '' | ./target/debug/gib ai
```

Finally, verify that JSON mode never opens the full-screen frontend or emits
ANSI sequences:

```bash
./target/debug/gib --mode json ai conversation list
./target/debug/gib --mode json ai --message "hello"
```

To verify Task 08 runtime selection without changing the persistent profile,
run a one-shot override and inspect the `ai_runtime` records before the model
load record:

```bash
./target/debug/gib --mode json ai \
  --profile low-memory \
  --threads 2 \
  --context-size 2048 \
  --max-output-tokens 128 \
  --message "report your runtime status"
```

The resolved record should contain the hardware snapshot, memory estimate,
selected settings, and any downgrade reason. For a persistent preference,
update the `[runtime]` section in `~/.gib/ai/config.toml` as documented in
[`ai-runtime-profiles.md`](ai-runtime-profiles.md), then start `gib ai` and
confirm the profile/settings appear in the header. On a LowMemory run, wait
for the first response, send a second message, and confirm the footer reports
that the model was released and then reloaded for the second turn.
