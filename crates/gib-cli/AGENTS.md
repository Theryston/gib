# Gib CLI Guidelines

## Scope

This file applies to `crates/gib-cli/` and extends the workspace-level rules.
The CLI is a thin adapter around the public `gib-sdk` API. It must not become an
alternative implementation of Gib behavior.

The package name is `gib-cli`; the produced binary name is `gib`.

## CLI responsibilities

The CLI may:

- define commands, flags, argument relationships, and help text;
- read configuration intended specifically for CLI invocation;
- collect interactive input and confirmations;
- convert validated command input into public SDK requests;
- subscribe to public SDK events;
- render the same events/results in Interactive or JSON mode;
- map public errors to stable process exit codes;
- configure tracing/log destinations without changing SDK behavior.

The CLI must not:

- implement backup, restore, search, explore, live, prune, or repository rules;
- access Local, S3, WebDAV, SQLite, chunks, packs, trees, snapshots, or
  repository refs directly;
- call private/internal SDK modules;
- duplicate SDK defaults or validation rules;
- perform compression, encryption, hashing, chunking, or filesystem scanning;
- make SDK behavior conditional on terminal rendering concerns;
- parse SDK error messages to decide behavior.

If required behavior is missing, add it to the public SDK first and then adapt
it in the CLI.

## Argument parsing

Use `clap`, preferably its derive API, for all commands, subcommands, options,
positional arguments, help, and version parsing. Do not hand-roll argument
parsing or help text. Keep CLI syntax and argument relationships in Clap, map
the parsed values to public SDK requests, and leave domain validation to the
SDK.

## Comments

Never add comments to CLI source unless they are genuinely necessary to explain
a non-obvious invariant, security or compatibility constraint, or behavior that
cannot be expressed in code or Clap metadata. Use the smallest possible number
of comments and the shortest wording; remove comments that merely restate the
code. User-facing help belongs in Clap attributes, not comments.

## Required source structure

```text
src/
├── main.rs
├── app.rs
├── commands/
│   ├── mod.rs
│   ├── backup.rs
│   ├── restore.rs
│   ├── search.rs
│   ├── explore.rs
│   ├── live.rs
│   ├── storage.rs
│   └── maintenance.rs
├── input/
│   ├── mod.rs
│   ├── arguments.rs
│   ├── config.rs
│   └── prompt.rs
└── output/
    ├── mod.rs
    ├── renderer.rs
    ├── interactive.rs
    ├── json.rs
    └── terminal.rs
```

Add command-domain modules as features grow; do not place every command in one
controller. Shared CLI parsing belongs in `input/`; shared presentation belongs
in `output/`. Do not create a generic CLI utility dumping ground.

## Entry point

`main.rs` must remain minimal. It may initialize the runtime, call the CLI app,
render a bootstrap failure when normal rendering cannot start, and exit with the
mapped code. Argument interpretation and command dispatch belong elsewhere.

The desired shape is conceptually:

```rust
#[tokio::main]
async fn main() -> ExitCode {
    app::run().await
}
```

Do not initialize repository backends or execute use cases in `main.rs`.

## Command flow

Every command follows the same direction:

```text
arguments/config/prompt
        -> validated CLI input
        -> public SDK request or builder
        -> SDK operation/events/result
        -> selected renderer
        -> exit code
```

Command handlers should be small. A handler may combine CLI input sources and
choose a renderer, but domain validation remains owned by SDK validated types.
CLI validation is limited to syntax and argument relationships that cannot be
expressed after parsing.

Never restate numeric defaults in Clap declarations when they can be obtained
from the SDK's public policy/default API. Help text and effective values must
not drift from library behavior.

## Output modes

Interactive and JSON are two renderers of the same public SDK events and
results. They must not execute different use cases or have different domain
semantics.

### JSON mode

JSON mode is a stable public automation protocol.

- Stdout contains JSON only, preferably one complete versioned object per line.
- Never write progress bars, ANSI control sequences, tracing text, prompts,
  plain warnings, or human errors to JSON stdout.
- Serialize the SDK's public wire DTOs or an explicitly versioned CLI envelope;
  do not serialize arbitrary internal or Clap types.
- Preserve SDK `operation_id`, sequence, event kind, phase, payload, and stable
  error code.
- Every started operation must produce exactly one completed, failed, or
  cancelled terminal event.
- Do not drop warnings, conflicts, recovery events, or errors.
- Event order must remain monotonic within each operation.
- Human message wording is not a machine contract; scripts use typed fields and
  stable codes.
- Define and test stderr behavior. Once an operation protocol has started,
  represent operation failure in the JSON stream. Reserve plain stderr for
  failures that occur before protocol initialization, and keep those cases
  documented and stable.
- JSON mode must never prompt. Missing required input returns a structured
  validation/bootstrap failure.

Adding/removing/renaming fields or changing their meaning requires protocol
compatibility review and fixtures. Additive optional fields are preferred.

### Interactive mode

Interactive mode may use colors, progress bars, spinners, prompts, tables, and
terminal-width adaptation.

- Rendering must consume structured events rather than infer progress from
  strings.
- Handle redirected/non-TTY output gracefully and disable unsupported terminal
  control behavior.
- Progress rendering must not delay or block SDK work; consume events through
  the SDK's bounded dispatcher contract.
- Clear/finish progress UI before printing final errors or summaries.
- Never leak secrets through prompts, progress labels, debug output, or terminal
  history. Use hidden input for passphrases and credentials.

### Mode parity

For every command and behavior, test both modes. A feature is incomplete when it
works only interactively or produces incomplete JSON.

## Errors and exit codes

Use typed SDK errors and stable CLI bootstrap/input errors. Maintain one
documented mapping from error categories to process exit codes.

Exit-code behavior must distinguish at least:

- success;
- CLI syntax/validation failure;
- authentication/credential failure;
- repository/storage unavailable;
- conflict or stale operation;
- corruption/incompatible format;
- cancellation;
- partial/recovery-required outcome;
- unexpected internal failure.

Do not assign codes by matching message text. Preserve the SDK error code in
JSON and render a concise actionable English message interactively. Avoid
printing full sensitive local paths unless necessary and explicitly safe.

## Configuration and input

- CLI configuration maps into SDK configuration/request types.
- Do not define a second storage, backup, restore, or resource policy model when
  the SDK already owns it.
- Establish precedence explicitly, for example flags over local config over
  global config over SDK defaults. Test this order.
- Prompt only in Interactive mode and only for information unavailable through
  non-interactive input.
- Do not accept secrets as ordinary command-line flags when they would be
  exposed in process lists or shell history. Prefer hidden prompts, stdin/file
  descriptors with explicit policy, OS secure storage, or the SDK credential
  vault flow.
- Autostart/non-interactive commands must fail actionably when an encrypted
  vault cannot be unlocked safely; never introduce a plaintext fallback.

## Command design

- Use kebab-case flags and consistent names across commands.
- Prefer shared argument groups only for truly identical syntax, not shared
  domain behavior.
- Keep destructive commands plan-first and confirmation-aware in Interactive
  mode. JSON mode requires an explicit non-interactive authorization field/flag
  and never prompts.
- Pass opaque destructive plans back to the SDK; never inspect or modify their
  internal targets.
- Long commands expose cancellation through Ctrl+C by cancelling the SDK
  operation and awaiting bounded graceful shutdown. Do not simply drop the
  operation future.
- `live` reuses the public SDK live/backup API and renders the resulting event
  stream; it does not implement its own watcher reconciliation logic.

## Library boundary enforcement

The CLI depends only on public exports from the `gib` library. Never make an
internal SDK module public merely to let the CLI take a shortcut. If an API is
generally useful, design and document a proper public capability. If it is only
presentation-specific, keep it in the CLI.

The SDK must compile and pass functional tests without compiling the CLI. CLI
dependencies such as Clap and terminal/progress libraries belong only to
`gib-cli`.

## Testing

Add tests for:

- argument parsing and conflicts;
- flag/config/default precedence;
- each command's request mapping;
- every stable exit-code category;
- Interactive behavior with TTY and redirected output;
- JSON stdout, stderr, event order, lifecycle, cancellation, and error fixtures;
- secret redaction;
- Ctrl+C graceful cancellation;
- terminal resize and progress cleanup where relevant;
- command parity with the public SDK feature set.

End-to-end tests must execute the built `gib` binary using isolated temporary
configuration and deterministic SDK fixtures/test backends. They must never read
or alter a developer's real Gib configuration, repository, credentials, or
home-directory data.

For every JSON scenario:

- parse every stdout line as JSON;
- assert no ANSI escape/control output;
- assert exact stable event types/codes and terminal lifecycle;
- assert expected stderr and process exit code;
- keep a reviewed compatibility fixture.

## Performance

CLI overhead must remain negligible compared with SDK work.

- Do not buffer complete event streams, file listings, search results, or
  explore trees unless an explicit bounded result contract requires it.
- Render or serialize incrementally.
- Apply backpressure through the SDK event API.
- Coalesce only presentation-level progress updates; never remove semantic
  events from JSON.
- Avoid repeated formatting/serialization of unchanged progress state.

Benchmark CLI startup and high-volume JSON/event rendering when changes affect
those paths. Domain performance belongs to SDK benchmarks.

## CLI-specific validation

In addition to the workspace suite, run:

```bash
cargo test -p gib-cli --all-features --no-fail-fast
cargo clippy -p gib-cli --all-targets --all-features -- -D warnings
cargo run -p gib-cli -- --help
```

Run end-to-end Interactive and JSON fixtures for every affected command. When
the SDK public API changes, verify the CLI uses only the new documented surface
and does not duplicate migration or compatibility logic.
