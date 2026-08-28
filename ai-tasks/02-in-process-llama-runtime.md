# Task 02 — Integrate llama.cpp as the in-process Rust inference runtime

## Roadmap position

This task follows the model installer and makes one verified local GGUF usable by GIB. It is the first runtime boundary and must be designed so later structured generation, conversations, hardware profiles, and the agent harness do not depend directly on llama.cpp types.

## Objective

Integrate the Rust llama.cpp bindings as an in-process backend. Load the installed GGUF locally, perform bounded text generation, expose token streaming, support explicit load and unload, and provide an AiBackend abstraction that can later be replaced by a fake backend in tests or another local runtime without changing the command, conversation, or orchestrator layers.

The initial implementation should use the utilityai llama-cpp-rs family, specifically the llama-cpp-2 crate and its matching sys crate, subject to a pinned version or commit chosen during implementation. The upstream project tracks llama.cpp closely and does not provide a stable semver promise, so the dependency version and native build configuration must be deliberately pinned and documented.

## Current repository analysis

There is currently no src/ai module, no native inference dependency, and no model loader. Cargo.toml already provides tokio, futures, serde, reqwest, and output infrastructure, but no llama.cpp wrapper, JSON Schema library, or terminal UI framework. src/main.rs dispatches all commands and sets a global output mode before invoking command handlers. src/output.rs is the only valid route for user-visible progress and errors.

The release workflow builds six platform/architecture combinations. A native dependency that happens to compile on the developer’s machine is insufficient. Feature flags must allow CPU builds for every supported target, while Metal, CUDA, Vulkan, or other accelerators are opt-in and must not make the default build fail. The exact llama.cpp feature matrix and compiler requirements must be recorded in the repository rather than hidden in a local setup.

## Required architecture

Define an object-safe boundary that does not expose llama-cpp-2 structs:

- AiBackend: load or acquire a model, create a generation request, stream generation events, cancel a request, and unload or release the model.
- AiBackendFactory or AiRuntime: resolve a model installation and runtime profile, then construct the backend.
- AiGenerationRequest: model ID, system/developer/user messages, sampling settings, context limit, stop sequences, optional grammar, and a request ID.
- AiStreamEvent: started, token/text delta, usage/update, finished, cancelled, or failed.
- AiGenerationResult: final text, finish reason, token counts if available, and timing data that is safe to expose.
- AiBackendError: model-not-installed, invalid-GGUF, load failure, context exhaustion, cancelled, native failure, and unsupported feature.

Use a worker or actor that owns the llama context and sampler. The llama-cpp-2 documentation states that the backend must be initialized before models and that LlamaBackend must outlive the model and context. It also exposes non-thread-safe context/sampler types. Therefore, do not move a context or sampler into arbitrary async tasks and do not promise that AiBackend is Send or Sync unless the wrapper actually enforces that guarantee. A dedicated blocking worker with bounded channels is the preferred design. The async-facing service sends a request to that worker, receives stream events, and sends cancellation through a separate channel or atomic flag that the decode loop checks between tokens.

Initialize the global llama backend exactly once for the process. Keep initialization and native logging behind a runtime module so that native diagnostics can be routed or suppressed consistently. Native stdout/stderr must not corrupt JSON mode. If the binding exposes a logging callback, route it to structured diagnostics; otherwise redirect or disable it using the supported API and document the limitation.

Load only a path that the model installer marked verified. Validate that the file exists, is a regular file, matches installation metadata, and has the expected GGUF identity before passing it to llama.cpp. Use the model’s chat-template support when available, but keep message-to-prompt formatting in a prompt layer rather than letting arbitrary callers assemble strings. Tokenize, decode, sample, and stop on end-of-generation or configured stop sequences. Enforce maximum output tokens and context size at the backend boundary.

Translate runtime-profile values into the binding’s context parameters: context size, batch and micro-batch size, generation and batch thread counts, sequence count, and accelerator/offload options. Unsupported accelerator settings must become a clear capability error or a deterministic CPU fallback according to the profile policy; they must not silently claim GPU offload.

Load/unload semantics must be explicit. Loading the same verified model twice should reuse or reject deterministically rather than leaking contexts. Unload must stop new requests, allow or cancel the active request according to a documented policy, release the context, and report completion. Do not unload implicitly on every turn if the selected profile is intended to keep the model warm.

## Generation and streaming behavior

The first generation path may be plain text only. It must nevertheless define a stable event sequence:

1. started with request and model IDs;
2. zero or more token/text deltas;
3. exactly one finished, cancelled, or failed terminal event.

A consumer must be able to reconstruct the final text from deltas. Backpressure must prevent an unbounded stream queue. Cancellation must be observable and must not append a partial assistant message unless the conversation layer explicitly records a cancelled turn.

Do not add tool calling, autonomous loops, hidden reasoning, or restore actions here. Those belong to later tasks and would make the runtime boundary unstable.

## Build and test requirements

Pin the binding version and, if needed, a llama.cpp revision. Add documented Cargo features for CPU and each supported accelerator. Verify cargo check and cargo build --release for the existing target matrix or clearly mark optional accelerator jobs. Native build logs must be captured in CI diagnostics without appearing in normal JSON command output.

## Tests and acceptance criteria

Use a FakeAiBackend implementing the same trait for all higher-level tests. Runtime tests should include:

- invalid and missing model paths;
- successful GGUF load and unload using a small test fixture where licensing permits;
- request construction, stop sequences, context/output limits, and terminal event invariants;
- streaming reconstruction and cancellation;
- two concurrent requests, proving the chosen serialization or concurrency policy;
- backend initialization exactly once;
- native error conversion without leaking raw paths or secrets;
- CPU feature compilation on all release targets and opt-in accelerator compilation when available.

An optional ignored smoke test may load the real Qwen model from Task 01, but the standard test suite must never download a multi-gigabyte artifact or require a GPU. The task is complete when one local verified GGUF can be loaded, generate a bounded response, stream deltas through AiBackend, cancel safely, and unload without callers importing llama.cpp types.

## References

- [llama-cpp-rs repository](https://github.com/utilityai/llama-cpp-rs) — binding scope, native build relationship, and fast-moving upstream compatibility.
- [llama-cpp-2 API documentation](https://docs.rs/llama-cpp-2/latest/llama_cpp_2/) — backend, model, context, tokenization, and sampling wrappers.
- [LlamaContextParams documentation](https://docs.rs/llama-cpp-2/latest/llama_cpp_2/context/struct.LlamaContextParams.html) — context, batch, and thread configuration.
- [llama.cpp repository](https://github.com/ggml-org/llama.cpp) — supported model/runtime backends and build guidance.
