# Repository Guidelines

## Scope and precedence

This file defines the global rules for the Gib workspace. More specific
`AGENTS.md` files may add or strengthen rules for their subtree. When rules
conflict, follow the file closest to the code being changed.

Treat these instructions as mandatory. Inspect the relevant code, tests, and
documentation before editing. Implement the requested work completely without
expanding into unrelated changes.

## Project mission

Gib is a backup system designed for very large repositories, multiple storage
backends, a public Rust SDK, and a thin CLI. Correctness, recoverability,
bounded resource usage, and stable contracts take priority over convenience or
short-term implementation speed.

The architecture must remain open for new features and backends while keeping
established domain behavior closed to accidental modification. Add capabilities
through stable interfaces and typed extension points instead of inserting
backend- or UI-specific branches into existing use cases.

## Workspace structure

- `crates/gib-sdk/` contains the public library, domain, use cases, repository
  format, and infrastructure adapters.
- `crates/gib-cli/` contains only command-line parsing, input collection, and
  Interactive/JSON presentation.
- `tests/fixtures/` contains committed compatibility, corruption, JSON, and
  repository fixtures.
- `tests/scenarios/` contains workspace-level end-to-end scenarios.
- `benches/` contains reproducible critical-path benchmarks and deterministic
  dataset generators.
- `docs/` contains architecture, repository format, protocol, security, and
  compatibility documentation.
- `scripts/` contains repeatable project checks; scripts must work from the
  workspace root.

The required dependency direction is:

```text
gib-cli -> gib-sdk public API
gib-sdk API -> application -> domain and application ports
gib-sdk infrastructure -> application ports, domain, and repository format
```

The domain must never depend on the CLI, concrete storage backends, SQLite,
Tokio, terminal libraries, environment variables, or platform APIs.

## Global engineering rules

- Use Rust 2024 edition and default `rustfmt` formatting.
- Write all code, identifiers, documentation, tests, errors, logs, event values,
  commit messages, and persisted text in English.
- Use `snake_case` for modules, functions, and variables; `CamelCase` for types
  and traits; and `SCREAMING_SNAKE_CASE` for constants.
- Keep CLI flags in kebab-case.
- Prefer explicit domain types and validated constructors over strings, tuples,
  booleans with unclear meaning, or bags of optional fields.
- Keep one authoritative definition for every shared rule, default, limit, and
  serialized value.
- Do not create generic dumping grounds such as `utils.rs`, `helpers.rs`,
  `common.rs`, or `misc.rs`. Shared behavior belongs to the domain or adapter
  that owns it.
- Keep modules cohesive. When touching a module that has accumulated multiple
  responsibilities, extract the responsibility relevant to the change.
- Avoid comments that narrate obvious code. Comments are reserved for
  non-obvious invariants, security reasoning, format/protocol decisions,
  platform constraints, compatibility workarounds, and required `SAFETY`
  explanations. Public Rustdoc is mandatory and is not restricted by this rule.
- Do not use production `unwrap`, `expect`, or `panic`, except for a truly
  unavoidable invariant documented at the call site and covered by tests.
- Unsafe code is allowed only when unavoidable, isolated behind a safe API,
  narrowly scoped, and accompanied by a precise `SAFETY` explanation.

## Change discipline

- Preserve public API, JSON protocol, and persisted-format compatibility unless
  the task explicitly authorizes a breaking release.
- Add new public behavior to `gib-sdk` first; the CLI then adapts it.
- Never repair corruption by silently treating invalid data as valid data.
- Never weaken atomicity, cancellation, validation, encryption, or resource
  limits to make an implementation simpler.
- Fix small, safe, directly adjacent debt when it is clearly understood and
  testable. Report larger or unrelated debt separately.
- Preserve user changes in a dirty worktree and avoid unrelated formatting,
  renaming, dependency updates, or cleanup.
- Every fixed defect requires a regression test at the lowest useful layer.
- Performance changes to backup, restore, chunking, codec, encryption, storage,
  indexing, search, or explore require reproducible before/after measurements.

## Contracts and compatibility

- Public SDK types must be designed for additive evolution. Prefer builders,
  private fields, validated newtypes, opaque handles, and `#[non_exhaustive]`
  where external exhaustive construction or matching is not intentional.
- Persisted wire models are separate from mutable domain models.
- Every persisted format and public JSON envelope has an explicit version.
- Maintain migration code and committed fixtures for all supported historical
  formats.
- Unknown future versions must fail with a typed unsupported-version error.
- Feature flags must be additive. Backend and capability dependencies must be
  optional when they are not required by the selected build.

## Security and portability

- Treat storage paths, restored paths, serialized plans, repository objects,
  credentials, and remote responses as untrusted input.
- All filesystem access must remain confined to an opened/configured root and
  must account for symlinks, junctions, reparse points, and races.
- Gib-managed credentials must be encrypted at rest and must never appear in
  `Debug`, errors, events, JSON, terminal output, temporary files, or fixtures.
- Persistent mutations must be atomic and recoverable. Multi-object operations
  require versioned journals and idempotent recovery.
- The portable core must support Linux, Windows, and macOS. Platform behavior
  must be isolated and tested on the native platform.

## Required validation

After any code change, run the relevant focused tests and then the complete
workspace suite:

```bash
cargo fmt --all -- --check
cargo check --workspace --all-targets --all-features
cargo clippy --workspace --all-targets --all-features -- -D warnings
cargo test --workspace --all-features --no-fail-fast
RUSTDOCFLAGS="-D warnings" cargo doc --workspace --all-features --no-deps
cargo check -p gib-sdk --no-default-features
```

Also run architecture, feature-matrix, fixture, platform, fault-injection, and
benchmark checks relevant to the change. Fix failures caused or exposed by the
change. If the environment cannot complete a command, report the exact command,
failure, and remaining unverified behavior; never describe an inconclusive run
as passing.

## Completion report

At handoff, report:

- behavior and contracts changed;
- tests and validation commands run with their results;
- compatibility, migration, performance, security, or platform implications;
- safe adjacent debt fixed;
- remaining adjacent findings or explicit blockers.

Keep commits focused on one behavior change. Use a short conventional title such
as `feat:`, `fix:`, `refactor:`, `perf:`, `test:`, or `docs:` followed by a
detailed explanatory body when committing is requested.
