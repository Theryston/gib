# Bounded backup pipeline

The SDK backup use case is an end-to-end bounded pipeline. A request carries
one shared policy for resident memory, CPU permits, filesystem descriptors,
storage requests, and inter-stage queue capacity:

```text
scan -> read -> chunk -> hash -> dedup -> transform -> tree -> pack -> upload -> index -> publish
```

The scanner and reader use blocking filesystem adapters on dedicated threads.
Hashing, compression, authenticated encryption, tree encoding, pack building,
and index building also run on dedicated fixed worker threads. The pipeline
never creates a task for an individual file or chunk.

## Resource ownership

`BackupBudgets` is validated before the operation starts. Each channel is a
bounded synchronous channel with the request queue capacity. Owned payloads
carry a memory permit until the next stage, upload completion, or cancellation.
Scratch allocations and accumulators reserve permits before allocation and
release them when ownership ends. CPU, descriptor, and network permits are
held only around the corresponding operation.

Index records are partitioned into 256 temporary shard files as they arrive;
the spool keeps at most one file descriptor open at a time. Index shard
construction uses a conservative transient-memory multiplier for
the simultaneously live spool records, sorted entries, and encoded shard
envelope; this is included in request validation and the runtime reservation.

The worker plan is fixed for the request. Read, hash, and transform pools are
derived from the CPU and descriptor budgets; upload workers equal the network
request budget. CPU-heavy work also acquires a shared CPU permit, so a larger
pool cannot exceed the configured CPU ownership limit. The result exposes
observed peaks for memory, CPU, descriptors, and network requests, which makes
resource assertions possible without relying on scheduler timing.

## Content reuse

After hashing, the deduplication stage resolves unique chunk IDs in bounded
batches. It discovers immutable index objects through paged listings, groups
queries by their one-byte chunk shard, and retains decoded verified shards in
the request's configured LRU cache. Index hits are accepted only after the
referenced pack metadata proves that every recorded range fits in the pack;
missing packs and malformed indexes fail the backup instead of becoming new
content. Repeated chunks in the same source are also marked reused, so the
packer receives only transformed content that is not already available.

Tree nodes are content-addressed. The preflight catalog records existing file,
symlink, and directory node keys within an explicit bound, allowing unchanged
Merkle subtrees to skip immutable tree writes. Conditional object creation
remains the concurrency boundary for publishers that race the catalog view.
The snapshot publication lists its root tree as a required object, so a missing
root cannot be committed.

`BackupMetrics` reports `logical_bytes` for source content,
`new_stored_bytes` for immutable object bytes newly accepted by this run, and
`reused_bytes` for logical chunk bytes served by existing or earlier-in-run
content. `uploaded_objects` counts only successful new immutable object
creates; conditional `AlreadyExists` results are not counted.

The filesystem scanner retains one directory enumerator per active path
component. Because that adapter owns its private directory-frame collection,
the backup worker conservatively reserves its configured maximum directory
descriptors for the scan lifetime and leaves permits for readers, the index
spool, and at least one storage call. Storage calls also acquire a descriptor
permit while they hold a network permit. Therefore a very small descriptor
budget can reject a source whose directory depth is larger than the budget;
the failure is typed and fail-closed.

## Backpressure and fairness

Producers retry bounded sends with cancellation checks. A full downstream
channel therefore slows the owning stage and eventually the source scanner,
without accumulating payloads in an unbounded queue. Shared receiver locks
distribute work among the fixed workers in arrival order; the scheduler does
not promise equal service time between files of different sizes.

The tree boundary restores scanner sequence order. Directory nodes are emitted
post-order, and file chunks are restored by ordinal before they are forwarded
to the packer. Only the bounded out-of-order window and the file's compact
chunk-reference list remain resident until its tree node is created.
Independent immutable-object uploads may complete in any order. HEAD is
attempted only after all worker joins and immutable uploads have completed, so
an unsuccessful run cannot publish a new snapshot.

Progress is deliberately off the hot path. A separate progress reporter has a
small bounded queue and uses coalescing/drop semantics. The SDK event dispatcher
has its own bounded queue per consumer: progress can be coalesced or dropped,
while lifecycle, error, warning, conflict, recovery, and terminal events are
retained. A slow consumer can delay a terminal event, but cannot grow pipeline
queues or retain an unlimited number of payload buffers.

## Failure and cancellation

One shared control object records the first fatal typed error and wakes every
permit wait and bounded channel operation. Workers stop at cancellation
boundaries, release owned permits through normal drop paths, and are joined by
the coordinator. Blocking storage adapters receive a cancellation-aware
source reader; adapters with native cancellation can override the storage port
hook to interrupt an in-flight request as well.

The existing repository publication use case remains the commit boundary. An
immutable object that finishes just as cancellation is observed may be left as
an unpublished object, but no new HEAD is published by the failed operation.
The backup pipeline does not add resume journals or change snapshot publication
rules.

## Validation and measurement

Focused pipeline tests cover successful publication, observed resource peaks,
cancellation, typed injected storage failure, slow storage, slow events,
identical snapshots, duplicate files, shifted content, one-leaf tree edits,
missing packs, corrupt indexes, and a one-shard deduplication cache.
The one-million-entry stress test is opt-in because it creates a large
temporary dataset. The standalone benchmark performs and reports a cold first
backup and a warm unchanged incremental backup against the same repository per
run. Dataset size and run count are controlled by environment variables.

Useful commands from the workspace root:

```bash
cargo test -p gib-sdk --test backup_pipeline -- --nocapture --test-threads=1
GIB_BACKUP_STRESS_ENTRIES=1000000 cargo test -p gib-sdk --test backup_pipeline stress_opt_in_large_entry_count -- --ignored --nocapture
GIB_BACKUP_BENCH_FILES=1024 GIB_BACKUP_BENCH_FILE_KIB=256 GIB_BACKUP_BENCH_RUNS=3 cargo bench -p gib-sdk --bench backup_pipeline
```

For a local manual run, use the `backup_pipeline_qa` example. Keep the source
and repository directories separate:

```bash
cargo run -p gib-examples --example backup_pipeline_qa -- \
  /path/to/source /tmp/gib-qa-repository \
  --memory-mib 64 --cpu 2 --fds 16 --network 1 --queue 1
```

See the repository completion report for the exact full-workspace validation
commands and their results.
