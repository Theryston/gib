# GIB Library Migration Plan

## 1. Objective

Turn GIB into a single Cargo package containing both:

- The existing `gib` CLI binary, built from `src/main.rs` and released as precompiled binaries without breaking its current behavior.
- A public Rust library, built from `src/lib.rs` and published to crates.io, exposing programmatic equivalents for every CLI capability.

The library must be silent by default. It must never read terminal input, print to stdout or stderr, render progress bars, install process-wide panic hooks, or terminate the process. Operations receive explicit Rust request values, return typed Rust results, and optionally report typed events to a callback. Those events must be serializable to the same JSON/JSONL protocol used by the CLI.

This is one repository and one Cargo package, not a workspace. The library and binary share the same package name and version.

## 2. Non-negotiable architectural rules

The dependency direction is:

```text
src/main.rs
    -> src/cli/*
        -> public gib::api
            -> core + config + storage
```

The reverse direction is forbidden.

- `src/lib.rs` must not declare or import `cli`.
- `src/main.rs` is the binary crate root and is the only target that declares `mod cli`.
- CLI code must consume the library through `gib::api` or public root re-exports, just as an external Rust consumer would.
- `api`, `core`, `config`, and `storage` must not import `clap`, `dialoguer`, `indicatif`, `console`, `tabled`, `crossterm`, or any CLI output module.
- Library code must not call `println!`, `eprintln!`, `print!`, `std::process::exit`, or terminal prompt APIs.
- Library code must not use global output mode state.
- All user choices, prompts, confirmations, TUI behavior, output formatting, stdout/stderr routing, and process exit codes belong to `src/cli`.
- No persisted wire-format model should be exposed directly as a stable public API merely because it is convenient. Public request/response DTOs and internal persistence models must remain separate where future format evolution is likely.
- Credentials and secrets must never appear in `Debug`, errors, events, JSON output, or result DTOs intended for display.

## 3. Target source tree

```text
src/
|-- lib.rs
|-- main.rs
|-- api/
|   |-- mod.rs
|   |-- client.rs
|   |-- error.rs
|   |-- event.rs
|   |-- backup.rs
|   |-- restore.rs
|   |-- live.rs
|   |-- catalog.rs
|   |-- storage.rs
|   `-- autostart.rs
|-- core/
|   |-- mod.rs
|   |-- backup/
|   |   `-- mod.rs
|   |-- restore/
|   |   `-- mod.rs
|   |-- live/
|   |   `-- mod.rs
|   |-- catalog/
|   |   `-- mod.rs
|   |-- repository/
|   |   `-- mod.rs
|   |-- crypto/
|   |   `-- mod.rs
|   `-- storage_registry/
|       `-- mod.rs
|-- storage/
|   |-- mod.rs
|   |-- local.rs
|   |-- s3.rs
|   `-- webdav.rs
|-- config/
|   |-- mod.rs
|   |-- model.rs
|   |-- loader.rs
|   `-- resolver.rs
`-- cli/
    |-- mod.rs
    |-- definition.rs
    |-- controller/
    |   |-- mod.rs
    |   |-- backup.rs
    |   |-- restore.rs
    |   `-- ...
    |-- input/
    |   |-- mod.rs
    |   |-- prompts.rs
    |   `-- tui.rs
    `-- render/
        |-- mod.rs
        |-- interactive.rs
        `-- json.rs
```

Large domains may contain additional internal files below their specified directories. The tree describes ownership boundaries, not a requirement to put thousands of lines in each `mod.rs`.

The actual directory name is `storage_registry`, with a normal underscore.

## 4. Cargo target arrangement

Keep a single `Cargo.toml` and make the two targets explicit:

```toml
[lib]
name = "gib"
path = "src/lib.rs"

[[bin]]
name = "gib"
path = "src/main.rs"
```

The initial migration should keep dependency and feature changes conservative. First achieve correct layering and behavioral parity. CLI-only dependencies may later become optional behind a default `cli` feature if build-weight measurements justify it. Do not feature-gate prematurely while core code still imports CLI dependencies.

`src/lib.rs` should expose a deliberately curated public API and must not expose the CLI module. A representative shape is:

```rust
pub mod api;

mod config;
mod core;
mod storage;

pub use api::{Gib, GibBuilder, GibError, GibEvent};
```

`src/main.rs` should eventually be a thin binary adapter:

```rust
mod cli;

#[tokio::main]
async fn main() {
    cli::run().await;
}
```

## 5. Public API design

### 5.1 Facade

Expose a facade rather than requiring consumers to coordinate internal services:

```rust
let gib = Gib::builder()
    .data_dir("/custom/gib")
    .on_event(|event| {
        println!("{}", event.to_json_line());
    })
    .build()?;

let result = gib.backup(request).await?;
```

The facade should be cheap to clone when practical and safe to use from concurrent asynchronous tasks. Per-client or per-operation event handlers must not use global mutable state.

### 5.2 Explicit requests

Core operations must receive resolved values. For example, `BackupRequest` must carry the repository, source root, message, author, compression, chunk size, ignore rules, Git inclusion policy, concurrency, parent choice, and resume choice needed by the operation.

The core must not:

- Parse `ArgMatches`.
- Parse CLI-formatted strings when a typed value can be supplied.
- Prompt for missing values.
- Select a storage or backup interactively.
- Infer behavior from interactive versus JSON modes.
- Implicitly use the current working directory or home directory when these affect operation semantics.

An ergonomic constructor such as `Gib::from_default_environment()` may explicitly opt into default GIB directories and configuration discovery. It must still remain silent and non-interactive.

### 5.3 Typed results

Every operation returns `Result<T, GibError>`, where `T` is a public response type appropriate to the operation. Examples include `BackupResult`, `RestoreResult`, `SearchResponse`, `PruneResult`, and `StorageInfo`.

Final results must not be available only through callbacks. Callbacks are for streaming operation events; the authoritative final outcome is the returned `Result`.

### 5.4 Stable errors

Replace public `Result<_, String>` APIs with a typed error containing a stable code, a human-readable message, structured context where useful, and an error source where available.

Representative error codes include:

- `ConfigurationNotFound`
- `StorageNotFound`
- `InvalidStorageConfiguration`
- `PasswordRequired`
- `InvalidPassword`
- `BackupNotFound`
- `RepositoryConflict`
- `CatalogDegraded`
- `PermissionDenied`
- `Io`
- `Serialization`
- `Encryption`
- `Cancelled`

Only the CLI maps these errors to terminal styling, JSON error envelopes, stderr, and process exit codes.

## 6. Event and callback system

The primary event contract is typed, not a raw JSON string:

```rust
#[non_exhaustive]
pub enum GibEvent {
    OperationStarted(OperationStarted),
    Progress(ProgressEvent),
    Warning(WarningEvent),
    Backup(BackupEvent),
    Restore(RestoreEvent),
    Live(LiveEvent),
    Autostart(AutostartEvent),
}
```

Expose a callback or event-sink abstraction equivalent to:

```rust
pub type EventCallback = Arc<dyn Fn(GibEvent) + Send + Sync + 'static>;
```

Events should implement `Serialize` and provide convenient JSON conversion, including a JSON-line representation compatible with the CLI protocol.

A progress event should include at least:

- Operation kind.
- Operation phase.
- Processed count.
- Optional total.
- Optional percentage.
- Human-readable message.

Provide a no-op sink so calls without callbacks have negligible overhead and produce no output.

Events originating from parallel worker tasks should be serialized through an internal dispatcher before invoking a user callback. This avoids callback reentrancy, cross-operation mixing, and unstable ordering. Separate `Gib` instances and simultaneous operations must be able to use different callbacks safely.

The CLI renderers consume these same events:

- `InteractiveRenderer` turns them into text, tables, warnings, spinners, progress bars, and TUI updates.
- `JsonRenderer` turns them into the existing `{"type": ..., "data": ...}` JSONL protocol.
- The autostart JSON log writer consumes the same event stream.

There is no global `OutputMode::Lib`. Library mode is simply the library service with a callback sink or a no-op sink.

## 7. Configuration and input ownership

Separate the currently intertwined responsibilities:

1. Loading persisted global and local configuration.
2. Resolving explicit values, configuration values, and defaults.
3. Prompting the user for missing choices.

`config/model.rs`, `config/loader.rs`, and `config/resolver.rs` may implement the first two responsibilities without output or prompts. `cli/input` owns the third.

For CLI operations, the controller applies this precedence:

```text
explicit CLI flag -> gib.toml -> safe default -> interactive prompt
```

The controller then constructs a complete typed API request. JSON mode never prompts and reports missing required inputs as a CLI error rendered in the existing JSON format.

## 8. Storage design

Preserve and evolve the existing `FS` abstraction as an injectable backend boundary. Local, S3, and WebDAV implementations live in `src/storage`.

Represent storage configuration with valid-by-construction enums rather than a numeric type plus many unrelated optional fields:

```rust
pub enum StorageConfig {
    Local(LocalStorageConfig),
    S3(S3StorageConfig),
    WebDav(WebDavStorageConfig),
}
```

Maintain backward-compatible deserialization for existing MessagePack storage files. Migration of the in-memory model must not silently invalidate previously configured storages.

The public API should support custom storage injection if the existing `FS` abstraction can be exposed safely without locking persistence internals into the public contract.

## 9. Capability mapping

Every current CLI capability must have a programmatic equivalent:

| Current CLI capability | Public library capability |
| --- | --- |
| `config` | `set_identity()` |
| `whoami` | `get_identity()` |
| `setup` | `setup()` |
| `storage add` | `add_storage()` |
| `storage list` | `list_storages()` |
| `storage remove` | `remove_storage()` |
| `storage prune` | `plan_prune()` and `execute_prune()` |
| `backup` | `backup()` |
| `backup pending` | `list_pending_backups()` |
| `backup delete` | `delete_backup()` |
| `log` | `list_backups()` |
| `restore` | `restore()` |
| `encrypt` | `encrypt_repository()` |
| `search` | `search()` |
| `explore` | directory, search, file, history, selection, and restore APIs |
| `live` | `start_live()` returning a controllable handle |
| `autostart` | add, update, list, status, enable, disable, remove, run, and follow-event APIs |

Interactive-only UX, including backup selection, confirmation prompts, Explore TUI, restore selection TUI, paginated terminal tables, and follow-log rendering, remains in `src/cli` and consumes these APIs.

## 10. Long-running operations

Live must not wait directly on `tokio::signal::ctrl_c()` inside library code. Expose a handle similar to:

```rust
let handle = gib.start_live(request).await?;
handle.stop().await?;
handle.wait().await?;
```

The CLI listens for Ctrl+C and asks the handle to stop. Library consumers can stop it from application logic.

Live events include at least:

- Started.
- Change batch.
- Backup started.
- Backup completed.
- Remote synchronization completed.
- Conflict detected and resolved.
- Recoverable error.
- Stopped.

The conflict policy must be explicit in `LiveRequest`. Only the interactive CLI may ask the user to choose one.

Autostart execution must pass working roots explicitly rather than mutating the process-wide current directory. Registry persistence, platform integration, secrets, live execution, event logging, and interactive log rendering must be separate responsibilities.

## 11. Migration sequence

The migration must be incremental, compile after each step, and avoid a big-bang rewrite.

### Phase 1: Establish layer boundaries

- Add `src/lib.rs` and the target module roots.
- Move Clap definition to `cli/definition.rs`.
- Move current command handlers under `cli/controller` without initially changing behavior more than necessary.
- Make `main.rs` a thin dispatcher.
- Move shared models out of command modules.
- Move terminal/TUI code out of `core::only` and into `cli/input`.
- Split password prompting from core cryptography.
- Break dependencies from config, utilities, and autostart into command modules.
- Ensure the library target compiles independently.

Completion criterion: dependency direction is `cli -> library`, while CLI behavior remains unchanged.

### Phase 2: Freeze current behavior

- Add golden tests for current JSON envelopes and payloads.
- Cover `output`, `progress`, `warning`, `config`, `live`, `autostart`, `help`, `version`, and error events.
- Record stdout versus stderr behavior.
- Add persistence compatibility fixtures.
- Add `MemoryFS` or `FakeFS` test support.
- Cover empty repositories, missing passwords, missing storages, corrupted data, success paths, and partial failures.

### Phase 3: Introduce public contracts

- Implement `Gib`, `GibBuilder`, context paths, request/response DTOs, typed events, no-op and callback sinks, and typed errors.
- Add safe serialization and secret redaction.
- Use `#[non_exhaustive]` for public enums expected to evolve.

### Phase 4: Extract configuration, identity, and repository resolution

- Separate loading, resolution, and prompting.
- Make home, data, working, and explicit config paths part of a context.
- Remove environment and terminal assumptions from core resolution.

### Phase 5: Migrate Backup as the first vertical slice

- Turn current backup options/results into deliberate public requests/responses.
- Move orchestration into `core::backup`.
- Remove output mode checks, emit calls, progress bars, prints, and prompts from backup execution.
- Emit typed phases for metadata loading, generation, file/chunk processing, index persistence, catalog updates, HEAD publication, and cleanup.
- Return structured warnings for reused pending state, unencrypted chunks, and degraded catalogs.
- Keep parent, resume, deduplication, concurrency, incremental backup, and HEAD semantics compatible.

### Phase 6: Migrate read-only operations

- List backups.
- List pending backups.
- Search.
- Explore directory, search, file, and history APIs.
- Keep Explore TUI entirely in the CLI.

### Phase 7: Migrate Restore

- Accept explicit backup reference, target, file/revision selection, cleanup policy, and repository values.
- Upgrade restore progress from a zero-argument callback to typed events.
- Return skipped, restored, unavailable, failed, and locally deleted entries as structured result data.
- Keep interactive file selection in the CLI.

### Phase 8: Migrate destructive repository operations

- Extract backup deletion, prune, and repository encryption.
- Prefer plan/execute APIs for operations requiring confirmation, especially prune.
- Keep confirmation prompts in CLI controllers.
- Report individual failures and warnings structurally.

### Phase 9: Migrate setup, identity, and storage registry commands

- Add set/get identity APIs.
- Add setup APIs.
- Add storage validation, add, list, and remove APIs.
- Preserve existing persisted storage compatibility.

### Phase 10: Migrate Live

- Add explicit requests, typed events, cancellation, stop, and wait semantics.
- Remove direct Ctrl+C handling from library code.
- Preserve reconciliation, conflict, polling, debounce, cache, and incremental backup semantics.

### Phase 11: Migrate Autostart

- Separate platform registration, job registry, secrets, runner, logging, and rendering.
- Expose all current management capabilities programmatically.
- Consume the new Live API and event system.
- Eliminate process-wide current-directory mutation.

### Phase 12: Remove global output behavior

- Move `OutputMode` into CLI-only code.
- Replace global emit functions with CLI renderers consuming library events and returned results.
- Keep panic hooks and exit codes in the binary only.
- Make JSON log rotation an event consumer rather than a core side effect.
- Test simultaneous operations with isolated callbacks.

### Phase 13: Packaging, documentation, and release

- Add crate metadata required for crates.io.
- Confirm ownership or availability of the `gib` crate name.
- Add library documentation and runnable examples.
- Run `cargo package --list`, `cargo package`, and `cargo publish --dry-run`.
- Publish library and CLI with the same version from the same package.

## 12. CI and release design

### 12.1 Existing binary release CI

Keep the existing tag-driven multi-platform GitHub Release flow and preserve all current artifact names and CLI behavior. Prefer an explicit build command:

```bash
cargo build --release --bin gib --target "${TARGET}"
```

The resulting binary remains at the same target path and continues to be packaged for Linux, macOS, and Windows.

### 12.2 New crates.io publication CI

Add a separate workflow triggered only after the GitHub Release is successfully published. It should:

1. Check out the exact release tag.
2. Run formatting checks.
3. Check and test the library target.
4. Build library documentation.
5. Run `cargo publish --dry-run`.
6. Run `cargo publish` with `CARGO_REGISTRY_TOKEN`.

Using `release: published` ensures the crate is not published if binary builds or GitHub Release creation failed.

Keep `publish = false` in `release-plz.toml` if the dedicated workflow owns `cargo publish`; this prevents duplicate publication attempts. This setting is separate from Cargo's `[package].publish` manifest field.

The same package version applies to both CLI and library. The GitHub release distributes precompiled executables; crates.io distributes the Rust package sources used by `cargo add gib` and, because the package also has a binary target, potentially `cargo install gib`.

### 12.3 Required validation commands

During migration and in CI, run at least:

```bash
cargo fmt --check
cargo check --lib
cargo check --bin gib
cargo test --lib
cargo test --bin gib
cargo test
cargo doc --lib --no-deps
```

After every code change, follow the repository requirement to run `cargo check` and fix all errors.

Add an architectural check that fails if CLI dependencies or output/process primitives appear under `src/api`, `src/core`, `src/config`, or `src/storage`.

## 13. Compatibility and safety requirements

- Existing CLI commands, flags, defaults, prompts, interactive rendering, JSON payloads, stdout/stderr behavior, and exit statuses must remain compatible unless a change is explicitly documented and approved.
- Existing repositories, indexes, backup files, catalogs, storage configurations, local config files, live state, and autostart jobs must remain readable.
- Destructive operations must preserve confirmation semantics in the CLI.
- Core operations must report failures rather than silently ignoring them.
- Remove production `unwrap`/`expect` usage on environment values, external data, locks, serialization, filesystem access, network responses, and user-provided values where a recoverable error is possible.
- Do not create a nested Tokio runtime inside library operations. The asynchronous API runs on the caller's runtime; the CLI owns its `#[tokio::main]` runtime.
- Avoid changing algorithms and architecture simultaneously when they can be separated into distinct, testable commits.
- Preserve all unrelated user changes in the worktree.

## 14. Test strategy

The completed migration requires tests proving:

- Library APIs do not write to stdout or stderr.
- Library APIs never call `process::exit`.
- Calls without callbacks emit nothing.
- Callbacks receive expected typed events in a stable order.
- Concurrent operations do not mix callbacks.
- The JSON renderer remains compatible with current JSONL fixtures.
- External failures return typed errors rather than panicking.
- Requests do not read stdin.
- `FakeFS` can exercise backup, restore, deletion, prune, encryption, catalog, and search behavior without a real remote backend.
- Old persistence fixtures still deserialize.
- Credentials never appear in debug output, events, responses, or errors.
- Live cancellation shuts down watchers and background tasks.
- The library target compiles without declaring the CLI module.
- The CLI target uses the public library API rather than internal modules.
- All existing tests continue to pass.

## 15. Definition of done

The migration is complete only when:

- Every command currently dispatched by `src/main.rs` has an equivalent public Rust API.
- `src/lib.rs` never declares `cli`.
- `src/main.rs` and `src/cli` contain all terminal and process behavior.
- The CLI invokes only public library APIs for business operations.
- Every public operation returns `Result<T, GibError>`.
- Progress and long-running activity are observable through typed callbacks/events.
- Those events can reproduce the CLI JSONL protocol.
- The library is silent by default and safe for concurrent embedding.
- Live and Autostart can be controlled without process-global signals or working-directory changes.
- CLI interactive and JSON behavior remain compatible.
- Existing persisted data remains compatible.
- Binary GitHub releases still work for all current targets.
- The single Cargo package can be consumed with `cargo add gib` and published through the new crates.io CI.
- Formatting, checks, tests, documentation, packaging dry-run, and the repository's required `cargo check` all pass.

## 16. Implementation discipline

Implement the migration through focused commits or reviewable milestones corresponding to the phases above. At the end of each milestone:

- Run the relevant tests and `cargo check`.
- Confirm no new dependency inversion was introduced.
- Confirm the CLI still works in interactive and JSON modes.
- Update public documentation for any API completed in that milestone.
- Do not mark the migration complete while any CLI capability still bypasses the public library API.
