# Task 03 — Implement structured generation and prompt infrastructure

## Roadmap position

This task completes the first local-model foundation. It turns raw text generation into bounded, typed decisions that later conversation, routing, search, and restore workflows can validate without trusting free-form model output.

## Objective

Build a prompt and structured-output layer with versioned prompt assets, role-specific reasoning policy, JSON Schema validation, llama.cpp grammar-constrained decoding, bounded retries, and typed request/response interfaces. The layer must make invalid or ambiguous model output a normal controlled result rather than an unchecked string.

## Current repository analysis

The repository has no AI prompt assets, schema registry, or structured-generation abstraction. Cargo.toml has serde and serde_json but not schemars, jsonschema, or an AI runtime. Task 02 will introduce AiBackend and streaming; this task must depend on that abstraction rather than importing llama-cpp-2 directly into routing or command code. src/output.rs requires all diagnostics and progress to work in Interactive and Json modes, so validation failures must be domain events/errors rather than ad hoc prints.

The existing catalog and restore types are strongly typed Rust structures. The AI layer should follow that pattern, while keeping model-facing DTOs separate from core persistence types so a prompt change cannot silently alter repository data.

## Prompt architecture

Create a prompt registry under src/ai/prompts or an equivalent module. Every prompt must have:

- a stable prompt ID;
- an explicit semantic version;
- a role or capability name;
- a system/developer template and a short purpose statement;
- input and output schema IDs;
- a reasoning policy;
- tests with representative fixtures.

Use deterministic delimiters and clearly labeled sections for user request, trusted catalog evidence, prior tool results, and instructions. Escape or length-limit inserted data. Never allow a file name, catalog field, conversation message, or tool output to become an unlabeled instruction. Prompts must explicitly describe the required JSON shape because llama.cpp grammar constrains output but does not inject the schema explanation into the prompt.

Represent reasoning policy as an enum such as Disabled, InternalSummary, or AllowedForRole. Default user-facing chat and all safety decisions to no exposed chain-of-thought. If a workflow needs a rationale, request a concise evidence-linked explanation field, not hidden reasoning. The runtime must never stream hidden reasoning into the conversation viewport or persist it as an assistant message.

## Structured generation contract

Use schemars for Rust DTOs deriving JsonSchema and jsonschema for reusable validation. Keep a schema registry keyed by stable schema ID and version. At request construction:

1. Serialize the selected schema.
2. Reject unsupported size, depth, reference, or keyword patterns before invoking the model.
3. Convert the schema to a llama.cpp grammar using json_schema_to_grammar from llama-cpp-2, or use a checked-in GBNF grammar when conversion is not appropriate.
4. Include a natural-language format instruction in the versioned prompt.
5. Ask AiBackend for a bounded structured generation request.
6. Parse the complete output as JSON and validate it against the same schema.
7. Deserialize into the typed response only after validation.

Set additionalProperties to false unless a specific extension point is required. The grammar converter supports a JSON Schema subset, and unsupported constructs must be rejected or deliberately lowered by a documented compiler. Do not silently omit constraints such as enum, required, array bounds, or string limits. Disable external schema references and network resolution; all schemas must come from the local registry.

Define typed errors for schema-not-found, unsupported-schema, grammar-build failure, invalid-json, schema-validation-failed, deserialization-failed, output-limit, cancelled, and backend failure. Include machine-readable paths and error codes but cap the amount of model output copied into diagnostics.

## Retry behavior

Retry invalid structured output only a bounded number of times, normally two or three attempts configurable by the workflow budget. Each retry must provide a compact validation summary and preserve the original request semantics. Do not append invalid JSON to a conversation or treat a retry as a new user turn. Use a fresh sampling state or deterministic context reset so a failed decoder cannot contaminate the next attempt. If all attempts fail, return a typed failure that the orchestrator can handle; never coerce malformed output with a permissive parser.

Separate parser validation from semantic validation. JSON Schema can establish shape and primitive constraints, while later Rust code must validate IDs, paths, time ranges, permissions, and capability-specific invariants. A structurally valid restore request can still be unsafe and must not bypass Task 11 or Task 19.

## Interfaces and storage

Expose a PromptService, SchemaRegistry, StructuredGenerator, and typed GenerationRequest/GenerationOutcome. Keep prompt version, schema version, model ID, and attempt count in trace metadata. Do not persist full prompts by default because they can contain private conversation or catalog content; retain hashes and redacted excerpts for diagnostics.

Provide a test-only way to register a fake generator that returns scripted JSON, malformed JSON, valid-but-schema-invalid JSON, and valid typed objects. This will be used by Tasks 10–20 without loading a model.

## Tests and acceptance criteria

Cover:

- prompt lookup, version pinning, deterministic rendering, delimiter escaping, and role reasoning policy;
- schema serialization and registry lookup;
- grammar conversion for supported objects, arrays, enums, required fields, and additionalProperties false;
- rejection of external references, oversized/deep schemas, unsupported keywords, and invalid schema definitions;
- valid JSON parsing and typed deserialization;
- invalid JSON, schema errors with field paths, semantic validation after schema validation, and bounded retry counts;
- no invalid output persistence and no hidden reasoning in user-visible stream events;
- identical request semantics across interactive and JSON adapters;
- trace metadata containing model/prompt/schema versions without exposing secrets.

The task is complete when every model-generated structured decision is schema-constrained where supported, validated twice at the typed boundary, retried within an explicit budget, and represented by a versioned prompt contract. A caller must never receive an unvalidated generic JSON value when it requested a typed response.

## References

- [llama.cpp grammar documentation](https://github.com/ggml-org/llama.cpp/blob/master/grammars/README.md) — GBNF constraints, JSON Schema support, and prompt requirements.
- [llama.cpp JSON grammar](https://github.com/ggml-org/llama.cpp/blob/master/grammars/json.gbnf) — baseline grammar behavior.
- [llama-cpp-2 json_schema_to_grammar](https://docs.rs/llama-cpp-2/latest/llama_cpp_2/fn.json_schema_to_grammar.html) — Rust schema-to-grammar conversion.
- [schemars documentation](https://docs.rs/schemars/latest/schemars/) — deriving and producing JSON Schemas from Rust types.
- [jsonschema documentation](https://docs.rs/jsonschema/latest/jsonschema/) — reusable local validation and error iteration.

