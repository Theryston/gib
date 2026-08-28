# Task 08 — Add hardware detection and AI runtime profiles

## Roadmap position

This task follows the usable chat frontend and prepares the runtime for predictable behavior across laptops, servers, and constrained environments. It must inform performance and resource limits without weakening any safety rule or allowing the model to choose its own execution budget.

## Objective

Detect relevant hardware facts, expose LowMemory, Balanced, and HighQuality runtime profiles, and translate them into deterministic model/runtime settings. Capture RAM, CPU count and architecture, supported acceleration, threads, context size, batch settings, GPU offload, and optional generation/search budgets. Allow explicit configuration overrides while retaining safe automatic defaults.

## Current repository analysis

Cargo.toml already includes num_cpus, but it has no general hardware capability abstraction. There is no AI configuration or runtime profile module. Task 01 stores model metadata and Task 02 consumes context/thread/offload settings. The release workflow targets Linux, Windows, and macOS on x86_64 and aarch64, so the detector must compile and degrade gracefully on all of them.

The repository’s existing configuration split places project settings in src/config/local.rs. AI profiles are user-global and belong in ~/.gib/ai/config.toml from Task 04. The existing output layer requires hardware detection and profile selection diagnostics to be representable in JSON without logging platform-specific noise.

## Hardware snapshot

Define a serializable HardwareSnapshot with:

- total and available memory, using a clearly documented unit and sampling time;
- logical and optionally physical CPU counts;
- target architecture and operating system;
- detected accelerator capabilities and the source of each capability claim;
- process limits or unavailable fields where the platform cannot report them;
- detector version.

sysinfo is a possible dependency for memory and CPU refreshes, but use only the fields needed and avoid an expensive broad refresh on every turn. Native llama.cpp capability queries should be treated as the runtime source of truth for compiled backends and usable offload; a GPU visible to the operating system is not proof that the current binary can use it. Keep detection read-only and never install drivers, run arbitrary shell commands, or mutate system settings.

Separate observed capability from selected configuration. For example, a snapshot may say that Metal is available while the selected CPU profile deliberately uses no offload. Report both.

## Profiles and selection

Define a RuntimeProfile enum and a resolved RuntimeConfig. The built-in policies should be documented rather than hidden constants:

- LowMemory favors a small context, conservative batch size, limited output, fewer retained conversations, and CPU-safe thread counts.
- Balanced is the default and chooses a context and batch that fit the model’s compatibility hints plus available memory.
- HighQuality uses a larger context/output budget and available acceleration only when capability and memory checks pass.

The exact numeric defaults must be benchmarked and checked against the Qwen model metadata. Never select settings solely from a marketing parameter count. Account for GGUF file size, KV-cache growth with context, batch working memory, allocator overhead, and concurrent requests. Refuse or downgrade a profile when a conservative estimate exceeds a configurable fraction of available memory. State the downgrade reason.

Expose overrides for profile, threads, context size, batch size, GPU layers/offload, maximum output tokens, and optional agent/search budgets. Validate relationships such as context greater than output, positive thread counts, bounded batch values, and offload values supported by the compiled backend. An explicit unsafe resource override should fail with a clear error rather than silently overcommit; if a force option is later added, it must remain separate from restore safety.

Persist user-selected profile and explicit overrides in AI config, but resolve ephemeral available memory at startup. Include model ID, profile, hardware snapshot summary, and final resolved settings in trace metadata and JSON status responses.

## Runtime integration

Task 02 must receive a resolved RuntimeConfig instead of reading environment variables or re-detecting hardware inside the decode loop. Load/unload decisions should be profile-aware: a low-memory profile may unload between turns, while Balanced may keep one model warm if the budget allows. Task 09 and later tasks must consume the same budget fields rather than inventing independent token or tool limits.

Do not let profile selection change deterministic safety behavior, catalog truth, path validation, confirmation requirements, or restore verification. HighQuality may improve answer quality but cannot authorize additional tools or destructive actions.

## Tests and acceptance criteria

Test:

- fixed HardwareSnapshot fixtures for low, normal, high, and unavailable memory;
- architecture/platform serialization and missing capability handling;
- profile selection and deterministic downgrade reasons;
- model-size/context/memory estimates near the refusal threshold;
- invalid overrides, unsupported GPU offload, excessive threads, and context/output conflicts;
- config round trips and explicit override precedence;
- backend feature combinations for CPU and optional accelerators;
- JSON status output with no platform-specific unstructured logs;
- no system mutation or shell execution during detection.

Add benchmark scenarios for startup detection, first load, warm generation, and profile changes. The task is complete when the same machine repeatedly resolves the same profile/settings for the same observed inputs, resource overcommit is prevented or explicitly rejected, and llama.cpp receives one validated runtime configuration.

## References

- [sysinfo documentation](https://docs.rs/sysinfo/latest/sysinfo/) — targeted system and memory refreshes.
- [llama-cpp-2 context parameters](https://docs.rs/llama-cpp-2/latest/llama_cpp_2/context/struct.LlamaContextParams.html) — mapping context, batch, and thread settings.
- [llama.cpp repository](https://github.com/ggml-org/llama.cpp) — backend and acceleration build capabilities.

