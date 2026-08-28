# Structured generation and prompt infrastructure

Task 03 adds the typed boundary between the local llama.cpp runtime and the
future router, search, history, and restore workflows.

## Components

`SchemaRegistry` stores immutable `SchemaDefinition` values keyed by a stable
identifier and a full SemVer version. A registration compiles three artifacts
once:

1. compact JSON Schema for the prompt format contract;
2. a reusable `jsonschema` validator for the post-generation boundary;
3. a llama.cpp GBNF grammar produced by `json_schema_to_grammar`.

Schemas are local-only. External references, unresolved local references,
unknown grammar keywords, oversized documents, and excessively deep or complex
documents are rejected before a model call. Object contracts default to
`additionalProperties: false`. Schemars integer-width and floating-point
`format` annotations are deliberately removed because Rust's typed
deserialization preserves those semantics while the llama.cpp grammar compiler
does not understand those annotations. Other unsupported formats are rejected.

`PromptService` stores prompt assets as versioned `PromptDefinition` values.
Each definition records its capability, purpose, system/developer templates,
input and output schema keys, and `ReasoningPolicy`. Rendering always emits
labeled sections for the user request, trusted catalog evidence, prior tool
results, retry feedback, and the output contract. Dynamic data is
UTF-8-safely bounded and escapes ampersands and delimiter characters before it
is inserted. The complete rendered prompt is hashed for tracing; the prompt
content itself is not persisted by this layer.

`StructuredGenerator` takes a version-pinned `GenerationRequest` and returns a
`GenerationOutcome<T>`. Every attempt sends the same semantic request with a
fresh runtime request ID, a fresh sampler/context, and a schema grammar. The
complete response is processed in this order:

1. enforce the output byte limit;
2. parse exactly one JSON value;
3. validate that value with the same registered JSON Schema;
4. deserialize it into the requested Rust type;
5. run optional domain-specific semantic validation.

Only failures in those output-validation stages are retryable. Retries are
bounded to three by default and eight at the absolute service limit. A retry
receives a compact validation summary; the invalid model response is never
added to a conversation and never appears in a user-visible stream. Backend
failures, cancellation, prompt errors, and schema compiler errors return
immediately.

Reasoning policies never enable hidden chain-of-thought persistence. The
default `Disabled` policy explicitly requests only fields in the output
contract. `InternalSummary` and `AllowedForRole` permit a concise rationale
only when the typed schema declares such a field. The structured layer
consumes runtime deltas internally and exposes only the validated typed value.

## Native integration

The llama.cpp runtime now accepts the `AiGrammar` already present at the
`AiBackend` boundary. The worker creates the grammar sampler from the
loaded model and places it before the final greedy or distribution sampler. Grammar
initialization failures are represented as safe typed backend errors without
including native diagnostics or prompt content.

The `jsonschema` dependency is built without its HTTP and file-resolution
features. This is defense in depth: the registry also rejects every external
reference explicitly, so schema compilation cannot turn a model request into a
network or filesystem fetch.

## Test strategy

The unit suite uses `ScriptedAiBackend`, a test-only backend that returns
malformed JSON, schema-invalid JSON, or valid scripted values without loading
the multi-gigabyte Qwen model. Tests cover deterministic prompt hashes,
delimiter escaping, reasoning policy text, SemVer pinning, schema grammar
conversion, closed objects, array and enum constraints, external reference
rejection, resource limits, parser/schema/deserialization/semantic errors, and
bounded retries.
