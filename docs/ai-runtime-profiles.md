# AI hardware detection and runtime profiles

Task 08 adds a single hardware-aware runtime decision at the beginning of
`gib ai`. The decision is read-only and is made once per process. The selected
values are passed to llama.cpp as one validated `AiRuntimeOptions` value; the
decode loop does not re-detect the host or read environment variables.

## What is detected

`HardwareSnapshot` records:

- total and available physical memory in bytes;
- the UTC RFC 3339 sampling timestamp and detector version;
- logical and physical CPU counts when the platform reports them;
- target architecture and operating system;
- native llama.cpp capabilities for CPU, GPU offload, memory mapping, and
  memory locking;
- native accelerator device names and reported total/free device memory when
  llama.cpp exposes them;
- compile-time accelerator backends and whether llama.cpp reports the runtime
  capability as usable;
- process limits when the platform exposes the targeted values, otherwise an
  explicit `unavailable` list.

Linux memory and process-limit values are read from `/proc` without invoking a
shell. Windows uses the read-only `GlobalMemoryStatusEx` API. macOS reads
`hw.memsize` and explicitly reports fields for which this implementation does
not have a portable observation. `num_cpus` supplies CPU counts. A GPU shown by
the operating system is not treated as usable unless the current llama.cpp
binary reports usable offload support.
When device memory is available, HighQuality all-layer offload also requires
the reported free accelerator memory to be at least the registered GGUF size;
otherwise it stays on CPU while retaining its larger quality budget.

The snapshot is observation only. It never installs drivers, changes system
settings, writes outside GIB's own state, or authorizes a tool or restore
operation.

## Profiles and defaults

The persisted profile names are `low_memory`, `balanced`, and `high_quality`.
The CLI accepts `low-memory`, `balanced`, and `high-quality`. Balanced is the
default when no profile has been configured.

The current built-in policies are:

| Profile | Context | Batch | Max output | Threads | GPU | Warm model |
| --- | ---: | ---: | ---: | --- | --- | --- |
| LowMemory | 2,048 | 128 | 128 | up to 4, capped by logical CPUs | off | no |
| Balanced | 4,096 | 512 | 256 | up to 8, capped by logical CPUs | off by default | yes |
| HighQuality | 8,192 | 512 | 512 | up to 16, capped by logical CPUs | all layers only when native offload is usable | yes |

In a resolved JSON/runtime value, `gpu_layers = 4294967295` is GIB's
llama.cpp all-layers sentinel. It is selected only after native offload
support is reported and the conservative host-memory estimate fits.

These values are policy defaults for the current Qwen3.5 4B Q8_0 manifest, not
claims that all future models have the same architecture. The resolver estimates
memory from the registered GGUF byte size, context-dependent KV-cache growth,
batch working memory, a 20% allocator headroom, and one concurrent request. It
does not estimate from the model's marketing parameter count.

The default safety budget is 80% of available memory. If available memory is
not reported, the resolver falls back to total memory when possible. If neither
is available, it selects LowMemory defaults and records the reason. If a higher
profile does not fit, it deterministically tries the next lower profile. If
LowMemory is still above the conservative budget, the command continues with
LowMemory defaults and clearly warns that performance may be degraded. This
keeps the command usable on constrained machines while making the trade-off
visible to the user.

An explicit resource override is stricter: an estimate above the safe budget
fails with a typed error rather than silently downgrading the user's request.
Unsupported GPU offload, zero or excessive thread counts, invalid batch sizes,
and a context size that is not greater than the output budget also fail before
llama.cpp starts.

LowMemory releases the model after each interactive turn and reloads it before
the next turn. Balanced and HighQuality keep the model warm for the lifetime of
the command. The profile does not change path validation, catalog truth,
confirmation requirements, restore verification, or any other safety rule.

## Invocation overrides

All runtime flags apply to the current invocation and take precedence over the
same values loaded from `~/.gib/ai/config.toml`:

```text
--profile low-memory|balanced|high-quality
--threads COUNT
--context-size TOKENS
--batch-size TOKENS
--gpu-layers COUNT
--gpu-offload auto|on|off
--max-output-tokens TOKENS
--agent-budget UNITS
--search-budget UNITS
--memory-budget-percent PERCENT
```

The agent and search budgets are optional fields carried forward for the future
agent harness. They do not add tools or change the direct-chat behavior in this
task. A force option is intentionally not present; if one is introduced later,
it must remain separate from restore safety gates.

## Persisted configuration

AI runtime preferences live in the existing user-global AI config. Existing
Task 04/06 fields remain valid:

```toml
schema_version = 1
active_conversation_id = "conv-example"

[model]
active = "qwen3.5-4b-q8-0"

[runtime]
profile = "balanced"

[runtime.overrides]
threads = 8
context_size = 4096
max_output_tokens = 256
```

Only explicit override fields are serialized. The current available-memory
sample is always ephemeral and is never written to config. Unknown runtime
keys and invalid values are rejected before execution. Invocation flags are
not written back automatically, so automation can use a temporary profile
without changing the user's global preference; persistent preferences can be
updated through the `AiConfigStore` API or by editing the user config with the
documented schema.

## JSON status event

JSON mode emits structured runtime detection and resolution records before
model loading. There are no platform-specific human logs on stdout:

```json
{"type":"ai_runtime","data":{"status":"detecting","model_id":"qwen3.5-4b-q8-0","message":"Detecting hardware and selecting an AI runtime profile for 'qwen3.5-4b-q8-0'"}}
{"type":"ai_runtime","data":{"status":"resolved","model_id":"qwen3.5-4b-q8-0","runtime":{"schema_version":1,"profile":"balanced","threads":8,"context_size":4096,"batch_size":512,"max_output_tokens":256}}}
```

The actual resolved record also includes the complete serializable hardware
snapshot, memory estimate, final llama.cpp settings, optional budgets, warm
model policy, and any deterministic downgrade reason. Human-readable progress
is shown only by the interactive spinner.

## Benchmark scenarios

Profile changes should be measured independently from model download:

1. startup hardware detection and profile resolution with an installed model;
2. first model load for each profile;
3. warm generation for Balanced and HighQuality;
4. LowMemory generation including release and reload between turns;
5. repeated profile resolutions with the same fixed snapshot and manifest.

Record wall time, peak resident memory, selected context/batch/GPU settings,
and whether a downgrade occurred. The fifth scenario must produce identical
settings and downgrade reasons for identical inputs.
