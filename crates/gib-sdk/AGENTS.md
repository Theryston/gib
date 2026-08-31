# Gib SDK Guidelines

## Scope

This file applies to `crates/gib-sdk/` and extends the workspace-level rules.
The SDK is the product implementation. Every capability available through the
CLI must be available programmatically through this crate without terminal input
or direct process output.

The crates.io package name is `gib-sdk`; the Rust library name is `gib`.

## Binary output and persistence policy

The SDK produces no JSON. Every SDK-owned repository object, metadata record,
event payload, operation artifact, and other durable output must use a binary
representation; never add JSON serialization, JSON repository fixtures, or a
`serde_json` dependency to this crate. The standard binary representation for
structured information is versioned, deterministic MessagePack using explicit
wire structs. Chunk, pack, encryption, and codec outputs must use their own
documented binary formats.

JSON may exist only in an outer CLI or integration adapter that translates the
public SDK contract. It must never be written to repository storage or emitted
by an SDK persistence or infrastructure path.

## Crate responsibilities

The SDK owns:

- public requests, results, errors, events, and operation handles;
- backup, restore, search, explore, live, repository, and maintenance use cases;
- pure repository and backup domain rules;
- storage, filesystem, index, crypto, codec, runtime, and platform adapters;
- repository-format encoding, decoding, validation, and migration.

The SDK must not:

- parse CLI arguments;
- prompt users;
- render progress bars;
- print to stdout or stderr;
- depend on CLI output modes;
- expose internal wire models or backend SDK types as public contracts.

## Required source structure

```text
src/
├── lib.rs
├── api/
│   ├── client.rs
│   ├── builder.rs
│   ├── error.rs
│   ├── event.rs
│   └── <feature>.rs
├── application/
│   ├── backup/
│   ├── restore/
│   ├── search/
│   ├── explore/
│   ├── live/
│   ├── maintenance/
│   └── ports/
├── domain/
│   ├── repository.rs
│   ├── snapshot.rs
│   ├── tree.rs
│   ├── entry.rs
│   ├── chunk.rs
│   ├── pack.rs
│   ├── path.rs
│   ├── delta.rs
│   ├── operation.rs
│   ├── policy.rs
│   └── error.rs
├── format/
│   ├── envelope.rs
│   ├── encoder.rs
│   ├── decoder.rs
│   ├── migration.rs
│   └── v1/
└── infrastructure/
    ├── storage/
    ├── index/
    ├── crypto/
    ├── codec/
    ├── filesystem/
    ├── runtime/
    └── platform/
```

Do not add a layer merely to forward calls. Each module must own a real policy,
use case, model, port, format, or adapter responsibility.

## Dependency rules

### API

`api/` is the only intentional public surface. It may call application use cases
and compose configured adapters. It must not contain backup algorithms,
filesystem traversal, storage protocol logic, or persistence encoding.

`lib.rs` must remain small. Keep internal modules private and re-export only
reviewed public API items.

### Application

`application/` coordinates use cases. It depends on domain types and traits in
`application/ports/`. It must not depend on concrete Local, S3, WebDAV, SQLite,
crypto, terminal, or platform implementations.

Each use case accepts validated inputs and explicit dependencies. Replace long
argument lists with cohesive input/context types; do not introduce generic
service locators.

### Domain

`domain/` contains pure rules and valid states. It must remain independent of
Tokio, Serde wire formats, storage SDKs, SQLite, operating systems, clocks,
random-number generators, and environment access.

Use private fields and validated constructors. If a value can be invalid, it
must not be represented by an unrestricted primitive internally.

### Ports and infrastructure

Ports describe capabilities required by use cases. Infrastructure adapters
implement them. Add a new backend by implementing ports and capability traits,
not by adding backend branches to application logic.

Do not force every backend to pretend it supports a capability. Capabilities
such as range read, multipart upload, atomic compare-and-swap, conditional
create, and batch existence checks must be explicit. Correctness-critical code
must fail before mutation when a required capability is unavailable.

### Repository format

`format/` owns immutable wire models. A type such as `format::v1::Snapshot` is
not the same type as `domain::Snapshot`.

The required flow is:

```text
bytes -> envelope validation -> versioned wire model -> migration -> domain
domain -> current wire model -> envelope -> atomic publication
```

Never deserialize persistent bytes directly into current domain or public API
types.

## Repository storage model

The repository's durable data model is content-addressed, append-oriented, and
backend-neutral. The intended logical layout is:

```text
repository/
├── format
├── config/
│   └── repository
├── keys/
│   └── <key-id>
├── refs/
│   └── latest
├── snapshots/
│   └── <prefix>/<snapshot-id>
├── packs/
│   └── <prefix>/<pack-id>
├── pack-indexes/
│   └── <prefix>/<index-id>
├── path-deltas/
│   └── <prefix>/<snapshot-id>
├── checkpoints/
│   └── <checkpoint-id>
├── operations/
│   └── <operation-id>
└── locks/
    └── <lease-id>
```

Backend adapters may map these logical keys to files, S3 objects, or WebDAV
resources, but must preserve their semantics.

### Source of truth

Snapshots and their referenced immutable objects are authoritative. Search
indexes, explore caches, and local databases are derived and disposable. A
backup must remain fully restorable when every local cache has been deleted.

Never make a derived catalog necessary for restore or repository integrity.

### Immutable objects

- Chunks, trees, packs, pack indexes, snapshots, path deltas, and checkpoints
  are immutable after publication.
- IDs are derived from authenticated canonical content or generated according to
  the documented format rule.
- Existing immutable content must never be overwritten in place.
- Small chunks are grouped into packs to avoid millions of remote objects and
  filesystem entries.
- Pack indexes map blob IDs to pack IDs, offsets, lengths, types, and format
  metadata. They are generated while packs are written, not by a later rescan.

### Snapshots and trees

A snapshot references:

- its parent snapshot when one exists;
- one root tree;
- its path delta;
- creation/source metadata;
- repository-format version and summary statistics.

Trees contain sorted entries. Directory entries reference child tree IDs;
regular files reference ordered chunk IDs; symlinks store their link target
without following it. Unchanged subtrees are reused between snapshots.

### Mutable references

Keep mutable repository state minimal. `refs/latest` must be updated only after
all referenced objects have been durably published. Update it with true atomic
compare-and-swap semantics. A read-then-write implementation is not CAS.

If a backend cannot provide safe ref publication, reject the operation or use a
documented backend-specific transactional protocol. Never silently fall back to
last-writer-wins for repository heads.

## Backup pipeline

Backup is a bounded streaming pipeline, not a sequence of full-memory phases.
The conceptual stages are:

1. Open the configured source root safely.
2. Discover filesystem entries without following symlinks.
3. Compare entries against the parent tree and validated local cache hints.
4. Reuse unchanged file metadata, chunks, and subtrees.
5. Stream changed files through hashing and content-defined chunking.
6. Deduplicate chunks through metadata/existence indexes without downloading
   existing chunk payloads.
7. Compress and encrypt chunks in bounded CPU workers.
8. Accumulate chunks into bounded immutable packs.
9. Upload packs and generate pack indexes concurrently.
10. Build sorted directory trees bottom-up.
11. Emit the path delta during the same scan; never rescan the completed backup
    to build search/explore catalogs.
12. Write and authenticate the snapshot object.
13. Atomically compare-and-swap `refs/latest`.
14. Commit the local query-cache transaction.

Once the ref update succeeds, the backup is valid. A failed local cache commit
must not invalidate the snapshot; the cache catches up later from the snapshot's
path delta.

### Pipeline invariants

- Peak memory is bounded by configured chunk, pack, queue, and concurrency
  limits, never by the largest input file or total backup size.
- Discovery stops admitting work when downstream queues are full.
- CPU, blocking filesystem, network, open-file, buffered-byte, and event-queue
  usage have explicit global and per-operation limits.
- Results are deterministic regardless of worker completion order.
- Final tree entries and metadata are canonically ordered.
- Cancellation is checked between bounded units and before publication.
- Incomplete packs, trees, and staging files are unreachable until commit and
  can be recovered or removed safely.

Do not use `read_to_end` for user files, clone complete payloads, spawn one task
per file without a bound, or perform hashing/compression/encryption directly on
async runtime worker threads.

## Restore behavior

Restore resolves data through:

```text
snapshot -> tree -> file entry -> chunk IDs -> pack index -> pack ranges
```

It must not depend on the search database.

- Read only required trees and pack ranges where the backend supports ranges.
- Stream decode, decrypt, verify, and write through bounded buffers.
- Write regular files to unique sibling staging files.
- Verify size and content hash before atomically publishing the final path.
- Preserve the previous destination until replacement succeeds.
- Restore symlinks last and never allow a restored symlink to become a parent
  for a later write.
- Cancellation leaves either the original destination or a complete verified
  replacement, never a partial final file.

## Search and explore

### Local query cache

Use a repository-scoped SQLite database as a derived local cache. It may store:

- snapshots already applied;
- current path state;
- additions, changes, deletions, and last-seen snapshot data;
- searchable names and paths through FTS;
- frequently used pack-location information.

The cache must use versioned schema migrations and atomic transactions. It must
be safe to delete and rebuild.

### Path deltas and checkpoints

Each backup emits an immutable path delta as part of its normal scan. New or
stale caches apply only missing deltas. Periodic immutable checkpoints shorten
cold rebuild time. Checkpoint compaction is maintenance work and must not block
every backup's critical completion path.

### Explore

Explore can navigate snapshot trees directly and may use the local cache as an
optimization. Loading a directory must not require materializing the entire
snapshot.

### Search

Search normally uses the local SQLite/FTS cache. If the cache is stale, apply
the latest checkpoint and missing deltas before querying. Search results must
refer only to content that remains restorable according to repository state.

## Live mode

Live mode reuses the same backup domain and publication pipeline. It may batch
filesystem events, coalesce repeated changes, and maintain a long-lived cache,
but must not implement a second backup format or bypass backup invariants.

Filesystem events are hints, not authoritative truth. Overflow, missed events,
or watcher restart requires safe reconciliation against the source filesystem
and the latest committed tree.

## Async, blocking work, and resources

- Async methods must not execute blocking filesystem traversal, blocking sleep,
  compression, hashing, encryption, or other sustained CPU work inline.
- Use async I/O for network operations.
- Isolate short blocking operations with `spawn_blocking` and sustained CPU work
  in a dedicated bounded pool.
- A started blocking task cannot be assumed abortable; workers must check a
  cooperative cancellation signal between bounded units.
- Use bounded channels and semaphores. Unbounded queues are prohibited in data,
  work, and event pipelines.
- Define one domain-owned resource policy. Do not scatter independent constants
  such as restore `100`, WebDAV `8`, or catalog `16` across modules.
- Budget buffered bytes, not only task counts.

## Atomicity, recovery, and cancellation

Single-file persistence must use:

1. a unique temporary sibling created with create-new semantics;
2. bounded write;
3. flush and file synchronization;
4. verification when applicable;
5. atomic platform-appropriate replacement;
6. parent-directory synchronization where supported.

Never delete a valid destination before replacement, including on Windows.

Multi-object mutations such as prune, delete, migration, encryption, and
compaction require a versioned operation journal. Recovery is idempotent and
runs before accepting conflicting mutations.

Every long-running public operation accepts cooperative cancellation. Every
started operation reaches exactly one completed, failed, or cancelled state.
Cancellation events state whether work is resumable and identify safe recovery
state without exposing secrets.

## Errors

- Use typed non-exhaustive errors per layer: storage, repository, config,
  format, codec, crypto, index, application, and public API.
- Preserve `Error::source` chains.
- Map errors explicitly at layer boundaries.
- Public errors expose stable machine codes separately from human messages.
- Never parse error strings to control behavior.
- `NotFound`, conflict, unsupported capability, cancellation, corruption, and
  retryability are typed states.
- Never convert codec, decryption, authentication, or deserialization failure
  into successful raw/default data.

## Events and callbacks

Every public long operation uses a versioned event envelope containing at least:

- schema version;
- operation ID;
- monotonic sequence number per operation;
- typed operation and event kinds;
- typed phase;
- structured payload;
- optional stable error object.

The event model is public and independent of CLI presentation.

Callbacks run through a bounded isolated dispatcher. User callback latency must
not execute in storage/core producer code. Isolate callback panics. Coalesce
high-frequency progress events when required, but never drop lifecycle, warning,
error, conflict, recovery, or terminal events.

Do not hold internal locks while invoking user code. Document callback thread,
ordering, reentrancy, backpressure, and shutdown behavior.

## Security

### Paths and links

- Use validated relative path types internally.
- Reject absolute paths, parent traversal, Windows prefixes/device paths, NULs,
  and ambiguous cross-platform forms.
- Use directory-handle-relative/rooted filesystem operations; lexical join and
  canonicalize-then-reopen are insufficient for destructive paths.
- Preserve symlinks as links without following them during backup.
- Account for Windows junctions and reparse points.
- Destructive plans must be opaque, bound to repository/storage identity and
  source generation, and rejected when forged, altered, stale, or replayed.

### Cryptography and credentials

- Use reviewed authenticated encryption and versioned envelopes; never invent a
  cipher or protocol.
- Generate nonces, salts, IDs, and keys from an OS CSPRNG where security depends
  on unpredictability.
- Store S3/WebDAV and future backend credentials in a Gib-encrypted vault.
- Store only opaque credential references in ordinary config and autostart
  records.
- Prefer OS secure key storage to wrap/unlock the vault DEK for unattended
  desktop use; use an explicit passphrase-derived wrapping mode for headless
  environments. Never store an unwrapped key beside encrypted data.
- Zeroize sensitive buffers where practical.

## Public API evolution

- Requests use builders or validated constructors with private fields.
- Do not expose structs that require external struct literals when fields will
  need additive evolution.
- Use opaque operation/destructive-plan handles.
- Mark extensible enums and types `#[non_exhaustive]` when appropriate.
- Keep public DTOs separate from internal and persisted models.
- Run semantic API compatibility checks against the latest released `gib-sdk`
  before publishing.
- Breaking changes require an explicit release decision, migration guidance, and
  release notes.

## Feature organization

Heavy backends and capabilities must be optional and additive, for example:

- `local`;
- `s3`;
- `webdav`;
- `autostart` or platform integrations when owned by the SDK;
- a documented convenience feature set for the official CLI.

`cargo check -p gib-sdk --no-default-features` must not compile unused AWS,
WebDAV, CLI, or terminal dependency trees. Avoid Tokio's `full` feature; enable
only required capabilities.

## Documentation and tests

- Enable `#![warn(missing_docs)]` and make documentation warnings fail CI.
- Document every public item, errors, cancellation, atomicity, resource limits,
  callback behavior, examples, and required features.
- Keep public Rustdoc concise and focused on public behavior and contracts. Do
  not repeat an item's signature or implementation; aliases should use a
  single-line reference. Comments outside required Rustdoc are forbidden unless
  they explain a non-obvious invariant, security or compatibility constraint
  that cannot be expressed more clearly in code.
- Use doctests with `?`, not `unwrap`.
- Unit-test pure domain rules.
- Apply one shared contract suite to Memory, Local, S3, and WebDAV adapters.
- Add API integration, compatibility fixture, corruption, cancellation,
  recovery, race, and fault-injection tests.
- Use RAII temporary resources, deterministic clocks/IDs/seeds, and no real user
  configuration or credentials.
- Add a regression test before or with every defect fix.

## Performance validation

Maintain reproducible benchmarks for:

- many small files;
- very large files;
- incremental backup with no changes and partial changes;
- restore and targeted restore;
- chunking, hashing, compression, encryption, and pack building;
- catalog delta application, search, and explore;
- local and deterministic remote-backend profiles;
- concurrency 1 and bounded parallel configurations.

Record throughput, wall time, peak memory, requests, transferred bytes, open
files, and effective concurrency where relevant. Performance claims require a
baseline and comparison from the same environment.

## SDK-specific validation

In addition to the workspace suite, run configurations affected by the change:

```bash
cargo test -p gib-sdk --all-features --no-fail-fast
cargo check -p gib-sdk --no-default-features
cargo clippy -p gib-sdk --all-targets --all-features -- -D warnings
RUSTDOCFLAGS="-D warnings" cargo doc -p gib-sdk --all-features --no-deps
```

Run individual backend feature checks and the shared backend contract suite when
storage code changes. Run compatibility fixtures whenever public events, API
types, or repository formats change.
