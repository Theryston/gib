# In-process AI runtime

GIB's first local inference runtime is built on the utilityai `llama-cpp-2`
and matching `llama-cpp-sys-2` crates. Both crates are pinned to exactly
`0.1.154` in `Cargo.toml`. The binding follows llama.cpp closely and is not a
stable-semver API, so updating either crate is an intentional runtime change:
review the upstream API and native llama.cpp revision before changing the
pin.

## Native build requirements

The sys crate builds llama.cpp and generates Rust FFI bindings with bindgen.
Every build host therefore needs:

- a C and C++ compiler;
- CMake;
- libclang's shared C API, normally provided by `libclang-dev` or
  `libclang1`/`libclang` for the host platform.

The default `ai-cpu` feature builds the CPU backend without requiring an
accelerator SDK. `ai-openmp` is an additional opt-in CPU performance feature.
Accelerators are opt-in and must be available both at native build time and
at runtime:

```bash
cargo build --release                         # portable CPU backend
cargo build --release --features ai-openmp     # CPU + OpenMP
cargo build --release --features ai-cuda       # CUDA
cargo build --release --features ai-metal      # Metal
cargo build --release --features ai-vulkan     # Vulkan
cargo build --release --features ai-opencl     # OpenCL
cargo build --release --features ai-rocm       # ROCm/HIP
cargo build --release --features ai-mkl        # Intel oneMKL
```

Selecting an accelerator feature does not silently promise that a device is
available. The runtime checks llama.cpp's reported capability before loading a
model and returns a capability error if GPU layers were requested but no usable
GPU backend exists. A CPU-only profile uses zero GPU layers explicitly.

On Linux, a local development machine can install the prerequisites with:

```bash
sudo apt-get update
sudo apt-get install -y build-essential cmake clang libclang-dev
```

The release workflow installs the corresponding bindgen prerequisites on each
host family before building the existing six-target matrix. Cross compilation
uses the host libclang to generate bindings and the target C/C++ toolchain to
build llama.cpp.

## Runtime boundary

Callers depend on `AiBackend`, `AiGenerationRequest`, `AiStreamEvent`,
`AiGenerationResult`, and `AiBackendError` from `src/ai/runtime/api.rs` and
`src/ai/runtime/error.rs`; no llama-cpp type crosses that boundary.

`AiRuntime` owns one dedicated blocking worker. The worker is the only owner of
llama contexts and samplers, so concurrent callers are serialized at the
native boundary. Tokio channels are bounded: the command queue applies
backpressure before work is accepted and each generation stream has a bounded
event queue. Dropping a stream or calling `AiBackend::cancel` sets an atomic
flag checked between prompt batches and generated tokens.

The lifecycle is explicit:

1. `load_model(id)` resolves the model through the Task 01 registry, verifies
   its installation metadata, checks that the artifact is a regular file with
   the GGUF magic, and loads it into the warm worker.
2. `generate(request)` creates a fresh context and sampler for one bounded
   turn. The model remains loaded between turns.
3. `unload_model(None)` cancels active work, waits for the worker to finish the
   current operation, and releases the model. Supplying an ID additionally
   checks that the requested model is the one loaded.

The native backend is initialized once per process and intentionally kept
alive until process exit so it outlives all models and contexts. Native logs
are routed through llama-cpp's tracing hook and suppressed by default. Setting
`GIB_LLAMA_NATIVE_LOGS=1` enables them only when the process is in interactive
mode; JSON mode always suppresses native diagnostics so stdout remains valid
structured output.

## Event contract

For a consumed generation stream, the worker emits exactly one `started`
event, zero or more `text_delta` and `usage` events, and exactly one terminal
`finished`, `cancelled`, or `failed` event. Concatenating all text deltas
reconstructs the successful result. Stop-sequence suffixes are held back until
they are disambiguated, so a configured stop sequence is not included in the
successful final text.

The initial runtime supports plain text generation and model-provided chat
templates. The `grammar` request field is reserved at the boundary but is
rejected until Task 03 defines structured-generation semantics. Tool calling,
reasoning, agent loops, and persistence are intentionally outside this layer.

## Verification

The standard suite never downloads a multi-gigabyte model. It tests request
validation, prompt fallback, bounded event behavior, cancellation primitives,
safe native errors, and missing verified installations. A real model smoke test
should be opt-in and run only against an already installed artifact, for
example the verified Qwen3.5 model registered by Task 01.
