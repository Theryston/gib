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

The user message is committed before generation. The assistant message is
committed only after the runtime sends its successful terminal event. Every
turn carries an opaque `turn_id` in persisted message metadata; it is not sent
to the model and makes a retry fail with `turn_already_recorded` instead of
duplicating the user or assistant messages. A revision conflict is retried
once before generation. Once output has started, the service never reloads and
merges another process's revision.

The direct-chat prompt contains a fixed local-assistant system instruction and
all persisted user-visible messages mapped through the shared `AiMessage`
boundary. It has no tools, restore capability, agent loop, hidden reasoning,
or structured-output requirement. Those capabilities belong to later tasks.

## Model preparation

The first command invocation calls the Task 01 model manager. If the active
model is not installed and verified, it downloads the built-in Qwen3.5 model
from the GIB bucket, resumes a valid partial download, verifies the registered
size and SHA-256, and publishes it atomically before llama.cpp loads it. The
built-in manifest is:

```text
https://public.trygib.org/ai/models/Qwen3.5-4B-Q8_0.gguf
```

The raw artifact is not considered installed until its sidecar metadata and
integrity checks also pass.
