
# Repository Guidelines

## Scope and precedence

These instructions apply to the entire repository. More specific `AGENTS.md`
files may add rules for a subtree, but they must not weaken the architectural,
safety, compatibility, testing, or validation requirements below.

All code, comments, documentation, commit messages, CLI text, errors, events,
and configuration examples must be written in English.

Do not treat existing code as proof that a pattern is acceptable. The project
is migrating toward the standards in this document. Apply them to all new and
changed code. Fix small, safe problems directly adjacent to the work, include
refactoring required for a correct implementation, and report larger or
unrelated debt instead of silently expanding the task.

## Current architecture

GIB is one Cargo package with a public Rust library and a CLI binary.

- `src/lib.rs` is the library crate root and exposes the curated public API.
- `src/main.rs` is the binary crate root and must remain a thin entry point.
- `src/api/` owns the silent public facade, request/response DTOs, stable errors,
  and typed events.
- `src/cli/` owns argument definitions, input resolution, prompts, TUI behavior,
  output rendering, stdout/stderr routing, signal handling, and exit codes.
- `src/core/` owns internal backup, restore, catalog, reconciliation, crypto,
  metadata, and repository algorithms.
- `src/config/` owns configuration models, loading, validation, migration, and
  deterministic value resolution. It must not prompt or render output.
- `src/storage/` owns the `FS` boundary and storage backend implementations.
- `src/utils.rs` is only for genuinely cross-domain helpers. Do not move domain
  rules there to avoid choosing the correct owner.
- `scripts/check-architecture.sh` enforces part of the dependency boundary and
  must evolve when the architecture evolves.

The dependency direction is strictly:

```text
src/main.rs -> src/cli -> public gib API -> core/config/storage
```

Reverse dependencies are forbidden. In particular:

- The library must never declare, import, or depend on `cli`.
- CLI code must consume the library through its public API as an external
  consumer would. It must not reach into private `core`, `config`, or `storage`
  implementation details.
- `api`, `core`, `config`, and `storage` must not depend on Clap, Dialoguer,
  Indicatif, Crossterm, terminal renderers, or CLI output types.
- Only the CLI may prompt, inspect terminal capabilities, install process-wide
  hooks, handle Ctrl+C directly, choose exit codes, or terminate the process.
- Library behavior must never depend on an interactive/JSON mode or mutable
  global output state.

Every user-facing capability must be implemented through the public, silent,
programmatic API first. The CLI is a thin adapter that resolves input, calls
that API, and renders typed events and results. A feature is incomplete if its
core behavior is only available through the CLI.

## Public library contract

The library is silent by default. Library code must never print to stdout or
stderr, render progress, prompt, call `std::process::exit`, or install a global
panic hook. Operations receive explicit typed values, return typed results, and
may emit typed events through an optional callback.

- The returned `Result<T, GibError>` is the authoritative final outcome.
  Callbacks are for streaming events, not for returning the only copy of a
  result.
- Keep public request/response DTOs separate from persisted wire models.
  Never expose an internal serialized model merely for convenience.
- Treat everything re-exported by `src/lib.rs` or `src/api/mod.rs` as a public
  compatibility contract.
- Preserve source compatibility unless the task explicitly authorizes a
  breaking change. A breaking change requires migration guidance,
  documentation, tests, changelog/release treatment, and an appropriate version
  change.
- Prefer constructors and builders for extensible requests. Avoid designs where
  adding a field forces downstream users to update struct literals.
- Use `#[non_exhaustive]` where external matching or construction should remain
  forward-compatible, while considering the ergonomics cost deliberately.
- Avoid public boolean combinations that permit invalid states. Prefer enums,
  validated newtypes, or builders that make invalid combinations impossible.
- Public types must not expose credentials or secret-bearing values through
  `Debug`, `Display`, serialization, events, or errors.

All new or modified public items require complete rustdoc. Document purpose,
invariants, defaults, errors, cancellation behavior, platform limitations, and
examples where useful. Enable `missing_docs` in CI and treat it as an error for
the public API. Public examples must compile as doctests when practical.

## CLI and output contracts

Interactive output and JSONL output are two renderings of the same typed API
events and results. Do not implement separate business behavior for each mode.

- Interactive mode owns human-readable messages, styling, tables, spinners,
  progress bars, prompts, and TUI state.
- JSON mode is a stable public machine interface. Every emitted line must be one
  complete valid JSON value using the documented envelope. Never mix plain text,
  ANSI sequences, progress-bar control bytes, or debug output into JSON streams.
- Route stdout and stderr intentionally and test both. Errors written to stderr
  in JSON mode must still use the JSON contract.
- Treat JSON envelope names, event types, field names, value types, omission
  rules, and error codes as compatibility-sensitive. Do not rename or remove
  them without an explicitly authorized breaking change.
- Help/version output may be special-cased only in the CLI adapter and must keep
  the documented mode behavior.

Each operation receives a unique `operation_id`. Include it in every event and
preserve monotonic event order within that operation. Events from concurrent
operations may interleave, but consumers must always be able to correlate them.

User callbacks must run through a dedicated bounded dispatcher, never while an
internal lock is held and never on a critical I/O or compute worker. Isolate a
consumer panic so it cannot unwind the GIB operation. When a slow consumer fills
the queue, repetitive progress updates may be coalesced; warnings, errors,
state transitions, and terminal events must not be silently discarded. Preserve
event order after coalescing.

Add contract tests for success, progress, warnings, cancellation, and errors in
both interactive and JSON modes whenever relevant behavior changes.

## Errors and failure handling

Use typed errors by domain and explicit conversions at layer boundaries.

- Internal operations must not use `Result<_, String>` as their domain error
  model in new or changed code.
- Never infer an error code by searching or parsing human-readable error text.
- Map domain errors explicitly to stable `GibError` codes and structured,
  non-secret context.
- Preserve underlying causes through `source` where possible. Add context at the
  boundary where an operation becomes meaningful; do not repeatedly stringify
  and wrap the same error.
- Human-readable messages may improve without changing machine-readable codes.
- Do not expose provider responses verbatim until they have been sanitized.
- Partial per-file failures belong in typed result entries only when the
  operation is intentionally allowed to continue. A fatal invariant or
  repository-level failure must return `Err`.

`unwrap`, `expect`, `panic!`, `todo!`, and `unimplemented!` are forbidden in
production code unless an invariant makes failure impossible and the call is
accompanied by a precise comment proving that invariant. Prefer returning a
typed error. Tests may use descriptive `expect` messages.

## Async, concurrency, and cancellation

Never perform blocking filesystem calls, directory walking, blocking sleeps,
compression, hashing, encryption, or other CPU-heavy work directly on an async
runtime worker.

- Put blocking filesystem and CPU work behind `spawn_blocking` or a dedicated,
  bounded worker pool.
- Keep network backends truly asynchronous.
- Do not hold a synchronous or asynchronous lock across `.await`, user callback
  execution, slow I/O, compression, hashing, or encryption.
- Do not spawn unbounded tasks, create unbounded queues, or collect an unbounded
  number of futures/buffers before awaiting them.
- Use explicit concurrency limits and backpressure for file, chunk, catalog, and
  backend operations. Make limits configurable when consumers have a meaningful
  reason to tune them.
- Calculate the worst-case resource cost as concurrency multiplied by per-task
  buffers/chunk sizes. Keep memory use bounded and explain non-obvious budgets.
- Preserve deterministic final results even when worker completion order varies.

All long-running operations must be cancellation-safe. Check cancellation at
natural phase boundaries, release locks and resources through RAII, clean up
temporary files, and never publish partially constructed repository state.
Preserve resumable state when an operation supports resume. Cancellation must
return a typed `Cancelled` error or documented partial result instead of looking
like an internal failure.

## Performance and large-file behavior

Backup, restore, hashing, chunking, compression, encryption, catalog updates,
and storage transfers are performance-critical paths.

- Process large files as streams with bounded memory. Do not use `read_to_end`
  or build a full in-memory copy when the operation can work incrementally.
- Hash, compress, encrypt, upload, download, and restore in bounded chunks while
  preserving the repository format and deduplication semantics.
- Avoid unnecessary allocation, cloning of file/chunk buffers, repeated full
  scans, repeated serialization, and avoidable network round trips.
- Prefer clear ownership and borrowing over speculative micro-optimizations.
- Do not claim an optimization without evidence.

Changes to a critical path require a reproducible before/after benchmark that
measures the relevant dimensions, such as elapsed time, throughput, peak memory,
requests, or bytes transferred. Use representative small-file and large-file
datasets when relevant. Record the command, dataset, build profile, and results
in the PR or task summary. An unmeasured structural improvement is acceptable
only when the benefit is direct and the claim remains appropriately narrow.

Performance improvements must not weaken correctness, durability, security,
compatibility, cancellation, or output contracts.

## Persistence, schemas, and durability

Treat every on-disk or remote repository representation as a versioned contract,
including config, storage records, manifests, indexes, catalog shards, live
state, credential envelopes, and lock/state files.

- Give persisted schemas an explicit version.
- Maintain backward readers and safe migrations for supported older versions.
- Test migrations with committed fixtures produced by real earlier formats.
- Reject unknown future versions with a typed error; do not guess their shape.
- Do not overwrite the old representation until migration and validation have
  completed successfully.
- Keep persistence models private and convert them explicitly to public DTOs.
- Preserve deterministic serialization where object hashes or comparisons
  depend on bytes.

All mutable local state must be written atomically. Write a temporary file in
the same directory, flush/sync at the durability boundary appropriate to the
data, validate when relevant, and atomically rename it into place. On failure,
leave the previous valid file untouched and clean up temporary files. Account
for platform-specific rename semantics and test interruption/failure paths.

Compare-and-swap operations must be genuinely atomic for each backend. A
default implementation that performs separate read and write calls must not be
described as atomic. Backends must document their consistency and conditional
write guarantees, and shared backend contract tests must verify them.

## Security, credentials, and paths

Persisted credentials must be stored in a GIB-encrypted, versioned credential
file, never as plaintext fields in ordinary configuration or storage records.
Reuse the project's reviewed cryptographic primitives and authenticated
encryption pipeline; never invent a cipher, KDF, nonce scheme, or secret format.
If a task needs a key source or unlock policy that has not been specified, stop
and request a product decision. Migrations from legacy plaintext records must
be explicit, atomic, and tested.

Secrets must never appear in logs, JSON, interactive output, events, errors,
panic messages, filenames, `Debug`, or test snapshots. Redact at the earliest
boundary and use obviously fake credentials in tests.

Treat all repository paths and remote object keys as untrusted input.

- Reject absolute paths, parent traversal, empty unsafe segments, alternate
  separators, and encoded traversal before joining with a root.
- Prove containment at the filesystem operation boundary; lexical validation
  alone is insufficient when links or platform path features are involved.
- Never allow restore, prune, live reconciliation, or deletion to escape the
  configured root.
- Protect `.git` according to the documented product behavior in every code
  path, not only the primary restore path.
- Preserve symlinks, junctions, and equivalent link objects as metadata without
  following them during backup. Validate them during restore and refuse or warn
  on links that could escape the restore root.
- Destructive actions require explicit targets, safe defaults, and tests proving
  that unrelated data is preserved.

## Storage backends and dependencies

The `FS` abstraction is a public injection boundary. Keep backend-specific
behavior behind it and maintain shared contract tests for local, memory, S3,
WebDAV, and future implementations. Contract tests should cover missing objects,
listing semantics, overwrite behavior, conditional writes, deletion, path
normalization, and representative errors.

Organize heavyweight dependencies and backends behind optional Cargo features.
The official CLI binary may enable the complete supported set, while library
consumers must be able to compile only the capabilities they use.

- Avoid `tokio`'s `full` feature when a smaller explicit feature set is enough.
- Keep optional backend dependencies out of `--no-default-features --lib`.
- Do not add a dependency when a small, well-tested standard-library solution is
  clearer and safer.
- Before adding or upgrading a dependency, check maintenance, licensing,
  platform support, security posture, feature cost, and impact on compile time
  and binary size.
- Keep `Cargo.lock` updated for the CLI/application build.

## Portability and platform code

Keep `core` and the public API platform-neutral wherever possible. Isolate OS
behavior in small platform modules behind `cfg` and expose a common typed
interface. Unsupported capabilities must compile successfully and return a
typed `Unsupported` error.

CI must validate Linux, Windows, and macOS. Changes involving paths, atomic
rename, permissions, autostart, credentials, signals, or FFI require tests on
every affected platform. Do not assume UTF-8 paths, Unix permissions, `/` as the
native separator, case-sensitive filesystems, or identical rename semantics.

`unsafe` is forbidden unless it is unavoidable for an OS or FFI boundary. Keep
it in the smallest possible module, place a specific `// SAFETY:` justification
on every unsafe block, validate pointers/lengths/lifetimes, wrap owned resources
with RAII, and provide platform tests. Never spread unsafe invariants into safe
callers.

## Code organization and style

Follow Rust 2024 and default rustfmt. Use `snake_case` for modules, functions,
variables, and CLI-internal identifiers; `UpperCamelCase` for types and traits;
and `SCREAMING_SNAKE_CASE` for constants. Keep CLI flags in kebab-case and
consistent with existing Clap patterns.

- A module should own one cohesive responsibility. When touching a large module,
  extract the responsibility being changed when that can be done safely; do not
  add another unrelated section to an already oversized file.
- Split by domain responsibility, not by arbitrary line counts. Avoid tiny files
  that only obscure navigation.
- Keep functions focused and make side effects visible in their signatures.
- Prefer domain-specific names over generic names such as `data`, `handler`,
  `manager`, or `process` when a more precise name exists.
- Put shared defaults, limits, validations, and protocol names in one domain-owned
  source of truth. The CLI may resolve precedence but must not duplicate domain
  rules.
- Prefer exhaustive typed state machines and enums over string discriminators in
  internal code. Convert to stable wire strings only at serialization boundaries.
- Avoid premature abstraction. Extract shared behavior when it represents the
  same domain rule or invariant, not merely because two code fragments look
  similar.
- Avoid comments as much as possible. Prefer clear names, focused functions,
  expressive types, and code structure that explains itself. Add a comment only
  when the code cannot clearly communicate a necessary invariant, safety proof,
  compatibility constraint, non-obvious trade-off, or reason behind an unusual
  decision. Never add comments that merely narrate what the next line or block
  does. Required rustdoc and `// SAFETY:` justifications remain exceptions.
- No dead code, commented-out implementations, debug prints, or blanket lint
  allowances. A narrowly scoped lint allowance requires a justification.

## Testing strategy

Tests follow the architecture:

- Unit tests cover pure algorithms, validation, state transitions, error mapping,
  path normalization, and serialization details.
- API integration tests exercise public operations with `MemoryFS` or controlled
  temporary storage and verify typed results/events.
- Storage contract tests run the same behavior suite against every backend where
  practical. Network tests use controlled local test servers or explicitly
  separated opt-in integration environments.
- CLI end-to-end tests verify argument parsing, exit codes, stdout, stderr,
  interactive behavior, and JSONL contracts through the compiled binary.
- Migration tests read fixtures from earlier persisted schemas.
- Platform tests cover every affected OS-specific implementation.

Every bug fix must add a regression test that fails before the fix and passes
after it. New features require success, relevant boundary, and failure cases.
Tests must be deterministic, isolated, parallel-safe, and must clean up through
RAII-managed temporary resources. Do not depend on wall-clock sleeps, random
global paths, test order, real user configuration, or real external services.

Test observable behavior and invariants rather than private implementation
details. Do not weaken an assertion or delete a test merely to make a change
pass unless the task explicitly changes that contract.

## Required validation

Do not consider a code change complete until the full suite below passes from a
clean working tree state relevant to the task:

```bash
cargo fmt --all -- --check
cargo check --all-targets --all-features
cargo check --no-default-features --lib
cargo clippy --all-targets --all-features -- -D warnings
cargo test --all-features
bash scripts/check-architecture.sh
```

Run `cargo fmt --all` before the final check when formatting changed. Also run
targeted tests during development and any relevant benchmarks, doctests,
platform checks, or release builds required by the changed area.

Fix every failure before claiming completion. Do not hide a failure with a broad
lint allowance, disabled test, reduced feature set, or ignored exit code. If the
environment cannot run a required command, report the exact command and blocker;
do not imply that it passed.

## Commit, release, and change discipline

Keep commits focused on one coherent behavior change. Use Conventional Commit
subjects consistent with repository history, such as `feat:`, `fix:`, `refactor:`,
`perf:`, `test:`, `docs:`, or `chore:`. Use an imperative, concise subject and a
body for non-trivial changes explaining why, invariants, compatibility impact,
migrations, and benchmark results.

Do not mix unrelated formatting, cleanup, dependency upgrades, or generated
files into a functional commit. Preserve user changes already present in the
working tree.

The current release flow is driven by pushes to `main`: release-plz determines
the version/changelog update, the workflow creates the tag, builds release
binaries for supported targets, and creates the GitHub Release. Crate publishing
is triggered by the published GitHub Release. Do not manually bump versions,
edit generated release notes, create tags, or publish artifacts unless the task
explicitly requests it.

Before a release-related change, additionally validate the release binary and
package when applicable:

```bash
cargo build --release --bin gib
cargo doc --lib --no-deps
cargo publish --dry-run
```

## Completion report

When handing off work, state:

- What behavior changed and why.
- Which architectural boundaries or public contracts were affected.
- Tests added or updated.
- Exact validation commands run and their results.
- Relevant benchmark before/after results for critical paths.
- Compatibility, migration, security, cancellation, and platform considerations.
- Remaining larger or unrelated debt discovered but intentionally not changed.

Never claim success based only on code inspection when required validation was
not executed.