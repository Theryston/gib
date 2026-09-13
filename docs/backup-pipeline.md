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

Tree nodes are content-addressed and are written with an idempotent
create-if-absent operation. If a concurrent writer wins that operation, the
uploader compares the existing size and complete bytes with the candidate; an
existing object with different bytes is malformed and cannot be reused. The
snapshot publication lists its root tree as a required object, so a missing or
corrupted root cannot be committed.

`BackupMetrics` reports `logical_bytes` for source content,
`new_stored_bytes` for immutable object bytes newly accepted by this run, and
`reused_bytes` for logical chunk bytes served by existing or earlier-in-run
content. `uploaded_objects` counts only successful new immutable object
creates; conditional `AlreadyExists` results are not counted.

## Parent-based incremental snapshots

`BackupRequest::with_parent` accepts a validated full snapshot ID or selector.
`with_parent_latest` selects the current HEAD, while `without_parent` explicitly
requests a parentless snapshot. Textual selectors can be validated with
`with_parent_reference`, and the shared repository resolver accepts `latest`, a
full ID, or a unique ID prefix. Resolution and the parent snapshot's root
validation happen before the backup can publish a new snapshot.

The tree stage compares each source entry with the direct entry at the same
path in the selected parent. File and link nodes are reused only when their
content-addressed node reference, kind, and portable metadata match. Directory
nodes are rebuilt only when their canonical metadata or child references differ;
otherwise the old reference is retained. Parent directories are loaded lazily
along source paths, so parent-only deleted subtrees are not walked. A deleted
source entry is therefore absent from the new directory, while a rename is a
removed old name plus a new name that may still reuse the same leaf node. Type
changes always carry the new typed reference.

The selected parent ID is included in the new snapshot identity and recorded in
the snapshot header. The parent remains immutable. Final publication still
validates the complete new graph and its parent chain, so missing or corrupt
parent objects fail closed without advancing HEAD.

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
attempted only after all worker joins and immutable uploads have completed. The
publish step then decodes the new snapshot and walks its parent chain, Merkle
tree nodes, chunk references, pack-index records, packs, and exact payload
ranges. Snapshot statistics must agree with the walked tree. Only after this
reachability check does the repository advance HEAD with the expected
generation and storage version, so an unsuccessful run cannot publish a new
snapshot. Uploaded objects that are not reachable from the winning HEAD are
left for pruning rather than deleted in the publication transaction.

## Resumable operation journals

Every backup creates one journal below `operations/` after reading the current
HEAD and before source preflight. The journal is a versioned common-envelope
object with a canonical MessagePack payload. It records the operation number,
repository format and identity, base HEAD generation/snapshot/version, a
validated request fingerprint, source-root and source fingerprint, the last
pipeline stage and sequence checkpoint, snapshot creation time, timestamps,
the target snapshot, and a bounded set of immutable object keys with their
stored byte sizes and SHA-256 digests. Credentials, passwords, salts, and
derived encryption keys are never journal fields.

Journal creation and every checkpoint update use the storage port's atomic
conditional-write contract. The journal is authenticated by the common object
envelope checksum. When a pipeline has repository encryption material, journal
payloads use the same XChaCha20-Poly1305 envelope policy; listing without that
material reports the row as encrypted and cannot decode or resume it. Journal
loading is bounded to 8 MiB and 65,536 completion records. Pending listings
page only the `operations` prefix and project safe metadata, so they do not
scan immutable objects or retain a full completion set in the public result.

The completion set is advisory. Before resume, every recorded object is
re-read in a bounded stream and its size and digest are checked. Missing
objects are removed from the advisory set and rebuilt through the normal
create-if-absent path; a present object with different bytes is corruption and
fails closed. Encrypted transformed chunk envelopes use a deterministic nonce
derived from their immutable identity during backup, so rebuilding a pack has
the same bytes and can skip a verified completed pack without re-uploading it.

Resume compares the request fingerprint, repository format and identity, base
HEAD generation/snapshot/version, source root, and source fingerprint before
workers can publish. It rejects incompatible options, stale HEADs, changed
sources, malformed journals, unsupported versions, missing encryption material,
and already-published targets with distinct typed errors. A cancellation leaves
the last atomically committed checkpoint. The journal is marked complete and
then removed only after the HEAD CAS succeeds; a cleanup failure leaves a
terminal marker that is excluded from active pending listings and can be
recognized as already published on a later resume.

Progress is deliberately off the hot path. A separate progress reporter has a
small bounded queue and uses coalescing/drop semantics. The SDK event dispatcher
has its own bounded queue per consumer: progress can be coalesced or dropped,
while lifecycle, error, warning, conflict, recovery, and terminal events are
retained. A slow consumer can delay a terminal event, but cannot grow pipeline
queues or retain an unlimited number of payload buffers.

## Path deltas and checkpoints

After the tree result is known and before the snapshot is encoded, the
coordinator compares the parent and new Merkle trees and emits a sorted
immutable path delta: adds for new paths, modifies for changed nodes
(including rebuilt directories), and deletes for removed paths, with removed
subtrees collapsed to a single prefix delete. Identical subtrees are pruned
by node identity, so an incremental delta loads only tree nodes along the
changed frontier and never rescans the filesystem. A parentless snapshot
emits a full listing, which doubles as the genesis checkpoint. Every 16th
generation additionally publishes a full checkpoint listing. Both objects use
plain version-1 envelopes under `path-deltas/<shard>/<snapshot-id>` and
`checkpoints/<snapshot-id>`, are journaled like any other immutable upload
(so interrupted publishes resume and rebuild byte-identical bytes through
create-if-absent), and are listed as required publication objects. Delta
records reserve pipeline memory per path and fail closed on exhaustion;
decoding is bounded to 8 MiB and 1,048,576 records. Rebuilds start from the
nearest checkpoint and apply at most one interval of deltas; a missing or
corrupt checkpoint falls back to the delta chain, while a missing or corrupt
delta fails closed and is regenerated from the authoritative trees. Restore
never reads derived objects.

Measured on 256 files with 20 one-file-change rounds (release profile):
backups 8.84 s total, 20 deltas 23,372 bytes, one 257-path checkpoint
12,949 bytes, rebuild of the tip 0.28 ms over 4 deltas through the
checkpoint versus 1.68 ms for a full tree traversal. Small-change deltas
stay near one kilobyte; the amortized checkpoint cost is under one kilobyte
per generation at this shape.

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
Derived `refs/history/<generation>` records are written after the HEAD CAS and
can be rebuilt from authoritative snapshot objects if their update fails. The
The journal does not change snapshot publication rules: HEAD remains the only
commit boundary, and abandoned immutable objects are left for the existing
pruning policy.

## Validation and measurement

Focused pipeline tests cover authoritative metadata, complete graph traversal,
observed resource peaks, cancellation, typed injected storage failures before
and after upload, slow storage, slow events, idempotent retry after a HEAD
conflict, history-index failure, identical snapshots, duplicate files, shifted
content, explicit/latest/prefix and parentless selection, deletions, renames,
type changes, missing or incompatible parents, one-leaf tree edits, missing
packs, corrupt indexes, and a one-shard deduplication cache. Repository tests
also cover concurrent CAS publication and typed expected/current HEAD conflict
context. Resumable-backup tests cover encrypted and checksummed journals,
bounded pending listings, cancellation with retained checkpoints, verified
pack reuse, request/source mismatch, missing completed objects, corrupt
journals, and already-published targets. They also cover one-shot faults at
each immutable prefix (trees, packs, indexes, snapshots) plus journal creation,
cancellation before the first upload, a mismatch case for every fingerprinted
option (message, author, timestamp, chunking, pack, index, dedup, transforms,
budgets, parent), stale HEAD advances, tampered completion bytes failing
closed, oversized journals reported as too large, paged prefix-isolated
with and without repository material. Path-delta tests replay randomized
snapshot sequences against full tree traversal, pin rename to delete plus
add, cover type changes and branch parents, prove missing deltas regenerate
byte-identical, prove corrupt checkpoints fall back to deltas, and round-trip
a two-thousand-path listing sorted.
The one-million-entry stress test is opt-in because it creates a large
temporary dataset. The standalone benchmark performs and reports a cold first
backup and a parent-based incremental backup against the same repository per
run. Dataset size and run count are controlled by environment variables.

```bash
cargo test -p gib-sdk --test path_deltas -- --nocapture --test-threads=1
cargo bench -p gib-sdk --bench path_deltas
```

For a local manual run, use the `backup_pipeline_qa` example. Keep the source
and repository directories separate:

```bash
cargo run -p gib-examples --example backup_pipeline_qa -- \
  /path/to/source /tmp/gib-qa-repository \
  --memory-mib 64 --cpu 2 --fds 16 --network 1 --queue 1
```

The QA example accepts `--parent VALUE`; omitting `VALUE` selects `latest`:

```bash
cargo run -p gib-examples --example backup_pipeline_qa -- \
  /path/to/source /tmp/gib-qa-repository --parent latest
```

See the repository completion report for the exact full-workspace validation
commands and their results.
