# Content-defined chunking

Gib uses content-defined chunking for changed regular-file content. The
chunker consumes a bounded `Read` source, or a futures-compatible `AsyncRead`
when the SDK `async` feature is enabled. It never needs the complete file in
memory. The caller should consume and release each emitted `Chunk` before
requesting unbounded output retention; the chunker itself keeps one active
chunk, one bounded read buffer, and at most two reusable chunk buffers.

## BuzHash v1

The released policy is identified by version `1` and algorithm name `buzhash`.
Its parameters are fixed and are part of the policy metadata:

| Parameter | Value |
| --- | ---: |
| Rolling window | 64 bytes |
| Table seed | `0x4752_4942_4344_4331` |
| Read buffer | 64 KiB |
| Reusable chunk buffers | 2 |
| Default minimum | 256 KiB |
| Default target | 1 MiB |
| Default maximum | 4 MiB |

For table entry `i` in `0..256`, let `x` start at
`seed + i + 0x9e37_79b9_7f4a_7c15` modulo `2^64`, then apply the SplitMix64
finalizer in this exact order:

```text
x <- (x XOR (x >> 30)) * 0xbf58_476d_1ce4_e5b9
x <- (x XOR (x >> 27)) * 0x94d0_49bb_1331_11eb
T[i] <- x XOR (x >> 31)
```

All arithmetic is modulo `2^64`. For each input byte, the rolling state is
updated as follows, where `T` is that table and `W` is 64:

```text
H <- rotate_left(H, 1) XOR T[incoming]
                         XOR rotate_left(T[outgoing], W)
```

Before the window is full, the outgoing term is omitted. The window continues
across chunk boundaries, so boundaries depend only on the byte sequence and
policy, not on source read sizes or buffer boundaries.

The target is converted to a low-bit mask using the smallest power of two
greater than or equal to the target. A fingerprint boundary is eligible when
the current length is at least the minimum and:

```text
H AND (2^ceil(log2(target)) - 1) == 0
```

The maximum boundary wins when the maximum is reached. Consequently every
non-final chunk is in `[minimum, maximum]`; a final chunk may be smaller than
the minimum. Empty input emits no chunk. A non-empty input smaller than the
minimum emits one final chunk.

## Identity and policy metadata

Chunk IDs are independent of boundaries and transport metadata. For plaintext
chunk bytes `P`, the identity input is:

```text
GIB chunk content\0 || P
```

The SHA-256 result is the 32-byte `ChunkId`. The policy itself has canonical
metadata bytes:

```text
GIB chunking policy\0
|| algorithm length as big-endian u16
|| UTF-8 algorithm
|| version as big-endian u16
|| window size as big-endian u16
|| table seed as big-endian u64
|| minimum, target, and maximum as big-endian u64 values
```

The SHA-256 digest of those bytes is available through
`ChunkingConfiguration::policy_digest`. A repository or snapshot that records
chunk references must persist this policy version and metadata. Changing any
boundary, window, table, or boundary-mask rule requires a new chunking policy
version and an additive decoder; version 1 must not be reinterpreted.

The project configuration may provide the same fields explicitly:

```toml
[backup.chunking]
version = 1
algorithm = "buzhash"
min_size = "256 KiB"
target_size = "1 MiB"
max_size = "4 MiB"
```

Omitted fields use the current v1 defaults. The legacy `backup.chunk_size`
field remains accepted for released configuration compatibility; it is not
used as a content-defined policy.

## Streaming and cancellation

`Chunker` reads no more than 64 KiB per source call and checks the cancellation
callback before and after each read and while scanning the bounded buffer. The
async stream has the same bounds and checks. Cancellation is cooperative: a
callback cannot interrupt a source implementation already blocked in its
current read. Dropping an async `next_chunk` future stops the chunker from
polling the source again.

The bounded buffer pool is an implementation resource limit, not a limit on
how many output chunks an application may retain. Retaining every output chunk
necessarily retains every payload; streaming consumers should process or move
each chunk before requesting the next one.

Chunking does not choose filesystem boundaries, persist chunks, or group them
into packs. Those policies belong to the backup and repository layers.
