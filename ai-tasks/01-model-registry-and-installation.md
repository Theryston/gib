# Task 01 — Add the AI model registry and local model installation

## Roadmap position

This is the first implementation task. It is the prerequisite for every task that executes a local model. It must leave the repository in a state where a fresh installation can discover one supported model, download it safely, verify it, and identify it as the active model without requiring Ollama, LM Studio, a model server, or a manually prepared path.

## Objective

Create a versioned, local-first model-management subsystem. The subsystem must distinguish a model definition published by GIB from a model file that is present on disk, support resumable downloads, expose progress through the existing interactive and JSON output abstractions, verify the artifact cryptographically, install it atomically, retain license and provenance metadata, and persist the active-model selection.

The first built-in model entry must point to the GIB bucket object:

https://public.trygib.org/ai/models/Qwen3.5-4B-Q8_0.gguf

The URL is part of the product contract for this task. Do not replace it with a model hub URL or an inference server. Do not invent a SHA-256 value or a file size in source control. Before enabling automatic installation, publish or check in a GIB-controlled manifest containing the independently verified SHA-256 and expected byte size for that exact object. A manifest without a verified digest is not a security control.

## Current repository analysis

The repository has no AI module or model directory today. The most relevant existing building blocks are:

- Cargo.toml already includes reqwest with rustls TLS, tokio, sha2, serde, serde_json, toml, dirs, bytes, and futures. Avoid introducing a second HTTP or hashing stack.
- src/output.rs owns OutputMode, JSON event envelopes, progress events, warnings, errors, the JSON log sink, and the panic hook. Model installation must publish domain events through an adapter built on this module; it must not print directly to stdout.
- src/config/local.rs represents project-local gib.toml and intentionally models repository configuration. Global AI state belongs under the user-level GIB directory, not in every project configuration.
- src/config/resolve.rs demonstrates the project’s distinction between interactive prompting and JSON-mode validation. The installer must never prompt for a URL, checksum, or destination in JSON mode.
- src/fs contains an asynchronous FS trait, but its LocalFS implementation is tied to repository-local storage and the .gib-cas.lock convention. Do not silently use a repository key as the global model directory or reuse repository locks for model installation.
- The release workflow builds Linux, Windows, and macOS targets on both x86_64 and aarch64. Path handling, rename behavior, permissions, and native runtime assumptions must remain portable.

## Required design

Add a focused module tree, for example src/ai/model/ with registry, manifest, installer, storage, and error modules. Keep the public service independent of the eventual llama.cpp binding so that model management can be tested without loading native code.

The registry must expose:

- A stable model ID that is safe to use in paths, such as qwen3.5-4b-q8-0.
- Human-readable name, family, parameter class, quantization, format, and intended use.
- Exact download URL, source/provenance, license identifier and license text location, and a model metadata version.
- Expected size and SHA-256 from a GIB-controlled manifest.
- Compatibility hints such as minimum RAM and supported runtime features. These are advisory for selection; installation and runtime must still validate the file.
- A registry version and a schema version so that adding models or changing metadata is a migration rather than an accidental format change.

Keep the registry definition separate from installation state. Installation state should record model ID, manifest version, URL, downloaded size, verified digest, installation timestamp, local file path, and status. Store it under a dedicated global layout such as ~/.gib/ai/models, with one immutable final artifact and a sidecar metadata document per installed model. Store the active model ID in the global AI configuration described in Task 04, while allowing the installer to work before conversations exist.

The downloader must be a streaming implementation using reqwest response chunks and Sha256 updates as bytes arrive. It must:

1. Acquire an exclusive per-model installation lock before inspecting or changing the partial artifact.
2. Write to a same-directory partial file, for example model.gguf.part, never to the final .gguf path.
3. Persist enough resume state to detect a changed URL, manifest version, expected size, or digest. A partial file from a different manifest must not be appended blindly.
4. Attempt HTTP Range from the current partial length. Accept a valid 206 response with matching Content-Range. If the server returns 200 for a range request, safely restart from byte zero. Treat an invalid 206, an impossible Content-Range, or an unsatisfied range as a controlled failure or safe restart, never as a concatenation opportunity.
5. Handle an interrupted stream by preserving the partial state and returning a resumable error. A later invocation must continue without losing verified prefix bytes.
6. Use content length only as a progress hint. The final truth is the expected manifest size and SHA-256.
7. Hash the complete candidate, compare size and digest using constant-time-equivalent digest comparison, and reject mismatches.
8. Flush and sync the temporary file, rename it into the final model path on the same filesystem, and make the metadata sidecar durable. Where the platform permits it, sync the parent directory after the rename.
9. Mark the model installed only after both the final artifact and metadata are complete. There must be no state in which a consumer can mistake a partial file for a usable model.

The installer should use an event type such as ModelInstallEvent with model ID, phase, bytes received, optional total bytes, percentage, resumable flag, and final status. Interactive rendering may use indicatif or a simple existing progress adapter. JSON mode must emit valid event objects only, with no ANSI escape sequences, spinner output, progress-bar characters, or native-library logs mixed into stdout. Errors and warnings must follow the repository’s existing stderr/JSON error policy.

Add commands or service entry points only as needed by the later AI command. A useful internal API is resolve_model, ensure_installed, install, verify_installed, list_installed, and set_active_model. Keep command parsing out of the installer.

## Safety and failure behavior

Reject path traversal and symlinked model destinations. Create the AI directory with restrictive permissions where supported. Do not delete a mismatched artifact automatically; quarantine it with a diagnostic suffix or leave it as a clearly marked failed candidate so a user can recover evidence. Never overwrite a verified model in place. If a manifest changes, install into a new versioned path and update the active selection only after verification.

Do not trust a remote filename, Content-Disposition header, or server checksum over the signed or checked-in GIB manifest. A checksum endpoint may be added later, but it must be authenticated or otherwise integrity-protected. The exact bucket URL above must remain visible in the registry fixture and in the documentation for this task.

## Tests and acceptance criteria

Add unit and integration tests that use a local HTTP test server or a controllable reqwest test transport. Cover:

- registry lookup, unknown model IDs, schema validation, license/provenance metadata, and the exact Qwen URL;
- a full streamed download with digest and size verification;
- interruption followed by a valid Range resume;
- a server that answers 200 to a Range request;
- invalid Content-Range, 416, truncated content, changed manifest, wrong digest, and wrong size;
- concurrent ensure_installed calls for the same model, proving that only one final artifact is published;
- crash-like state with a .part file, missing sidecar, or sidecar referring to a different URL;
- JSON events that can be parsed line by line and contain no terminal control sequences;
- permission and path behavior on the supported operating systems where CI can exercise it.

The task is complete when a clean machine can resolve the built-in model, download it from the required GIB URL, resume an interrupted transfer, verify the configured digest, publish an atomic final file plus metadata/license provenance, and persist a selectable active model. No partial or unverifiable file may be reported as installed.

## References

- [reqwest Response documentation](https://docs.rs/reqwest/latest/reqwest/struct.Response.html) — streaming chunks, content length, and response status handling.
- [reqwest header documentation](https://docs.rs/reqwest/latest/reqwest/header/index.html) — Range and Content-Range support.
- [sha2 crate documentation](https://docs.rs/sha2/latest/sha2/) — incremental SHA-256 hashing.
- [Rust File::sync_all](https://doc.rust-lang.org/std/fs/struct.File.html#method.sync_all) and [Rust rename](https://doc.rust-lang.org/std/fs/fn.rename.html) — durability and atomic publication primitives.
- [GGUF format source](https://github.com/ggml-org/llama.cpp/blob/master/ggml/include/gguf.h) — why the downloaded artifact must remain an intact GGUF file.

