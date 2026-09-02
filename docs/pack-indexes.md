# Versioned pack indexes

Gib stores pack indexes as immutable, independently readable shards. A lookup
selects the shard from the first byte of the 32-byte `ChunkId`, reads that one
bounded shard, and then reads only the transformed payload range from the
containing pack. It never needs a repository-wide chunk map or a complete pack
read.

## Version and shard policy

The current binary shard format is version `1`, selected by the version in the
shard header. The decoder accepts exactly version 1; an unknown version is an
unsupported-version error and is never interpreted as version 1.

Version 1 uses one leading `ChunkId` byte as its shard key. There are therefore
256 independent shards, named `00` through `ff`. Records are sorted by the
complete 32-byte chunk ID, not only by the prefix. A shard can be built and
published independently, so adding new packs does not require retaining or
rewriting other shards in memory.

The simple current-layout key is `indexes/pack-v1/<prefix>`. Immutable
generation publication uses `indexes/<index-id>`, where the selected key is
recorded by a repository catalog or manifest owned by the caller. The lookup
API accepts that selected key with `lookup_at` and `read_chunk_at`; it verifies
that the decoded shard prefix matches the requested chunk. The SDK's storage
publisher uses the immutable index-ID key and create-if-absent publication.

The default maximum complete encoded shard is 16 MiB. The SDK absolute shard
limit is 64 MiB. A lookup checks the storage metadata size before buffering a
shard and rejects a larger object. The decoded shard cache has an explicit
default budget of 64 MiB and eight shards and evicts least-recently-used
shards. A shard larger than the cache budget may still be used for one lookup,
but is not retained.

## Binary representation

All integers are unsigned big-endian unless stated otherwise. Flags and
reserved bytes are critical: flags must be zero and every reserved byte must
be zero. The complete shard is exactly `header || records || footer`; trailing
bytes are rejected.

### Header

The header is exactly 64 bytes at offset zero:

| Offset | Length | Field | Value or meaning |
| ---: | ---: | --- | --- |
| 0 | 4 | magic | ASCII `GIXS` |
| 4 | 2 | version | Pack-index format version, currently `1` |
| 6 | 2 | flags | `0`; all bits are critical |
| 8 | 4 | header length | `64` |
| 12 | 4 | alignment | `8` |
| 16 | 1 | shard prefix bytes | `1` |
| 17 | 1 | shard ID | Leading chunk-ID byte |
| 18 | 2 | record length | `128` |
| 20 | 8 | entry count | Number of records |
| 28 | 8 | records offset | `64` |
| 36 | 8 | records length | `entry count * 128` |
| 44 | 8 | body length | `64 + records length` |
| 52 | 12 | reserved | All zero |

### Record

Every record is exactly 128 bytes. The records are adjacent and sorted:

| Relative offset | Length | Field | Value or meaning |
| ---: | ---: | --- | --- |
| 0 | 32 | chunk ID | Logical plaintext chunk identity |
| 32 | 32 | pack ID | Containing immutable pack |
| 64 | 8 | entry offset | Offset of the pack entry frame |
| 72 | 8 | payload offset | Offset of transformed payload bytes |
| 80 | 8 | entry length | Aligned complete entry-frame length |
| 88 | 8 | stored length | Transformed payload length |
| 96 | 8 | logical length | Plaintext chunk length |
| 104 | 2 | envelope version | Common immutable-object envelope version |
| 106 | 2 | object version | Kind-specific payload decoder version |
| 108 | 1 | codec | `0 = none`, `1 = zstd` |
| 109 | 1 | encryption | `0 = none`, `1 = xchacha20-poly1305` |
| 110 | 2 | reserved | All zero |
| 112 | 4 | compression level | Signed Zstandard level; `0` when codec is `none` |
| 116 | 12 | reserved | All zero |

The entry offset must be at or after the 64-byte pack header and aligned to
eight bytes. The payload offset must equal `entry offset + 96`; the entry
length must equal `align8(96 + stored length)`. Both the entry end and payload
end must fit within the absolute pack limit. Logical lengths are bounded by the
content-defined chunk limit. A record with a wrong prefix, invalid transform
tag, duplicate chunk ID, descending chunk ID, overflow, or impossible range is
rejected.

The transform descriptor is selection metadata, not a second identity. The
authenticated object payload still contains its nonce, salt, KDF parameters,
authentication data, and payload checksum. Those fields are intentionally not
duplicated in every index record.

### Footer

The footer is exactly 96 bytes and is the final bytes of the shard:

| Relative offset | Length | Field | Value or meaning |
| ---: | ---: | --- | --- |
| 0 | 4 | magic | ASCII `GIXF` |
| 4 | 2 | version | Must match the header, currently `1` |
| 6 | 2 | flags | `0`; all bits are critical |
| 8 | 4 | footer length | `96` |
| 12 | 8 | entry count | Must match the header and scanned records |
| 20 | 8 | body length | Must equal the footer start offset |
| 28 | 32 | body checksum | SHA-256 of the header and all records |
| 60 | 32 | index ID | SHA-256 identity of the versioned body |
| 92 | 4 | reserved | All zero |

The reader validates framing and counts, checks the body checksum and index ID,
then scans records and checks ordering and ranges before returning any record.
This ordering ensures that a corrupt or truncated shard is never used to issue
a pack read.

## Identity and range lookup

For version `V` and complete body bytes `B`, the index ID is the SHA-256 digest
of:

```text
GIB pack index identity\0
|| V as two big-endian bytes
|| B
```

The index ID is publication identity only. A chunk's logical ID remains the
chunk-content digest and does not change when pack placement, compression,
encryption, or index publication changes.

After a record is found, `PackIndexLookup` performs these checks in order:

1. Read and fully validate the selected bounded index shard.
2. Derive `packs/<pack-id>` from the validated pack ID.
3. Read pack metadata and verify `entry end <= pack length` and `payload end <= pack length` with checked arithmetic.
4. Issue one exact half-open range request for `[payload offset, payload offset + stored length)`.

The returned `PackChunkRead` retains the exact validated range and exposes the
storage backend's bounded reader. `read_chunk_payload` additionally checks the
reader produces exactly `stored length` bytes. A missing index record returns
`None`; a record whose pack is missing returns a required-object error. A
range outside the pack is rejected before `read_range` is called.

## Compatibility and publication

The persisted wire representation is private to `gib-sdk::format`. Public SDK
callers use `PackIndexEntry`, `PackIndexTransform`,
`PackIndexShardBuilder`, `PackIndexReader`, and `PackIndexLookup`; no wire
struct is part of the public API.

New decoders must be added alongside version 1. Existing version-1 bytes must
not be reinterpreted when a later format changes record fields, shard width,
alignment, or hashing boundaries. Immutable index generations are published by
ID with create-if-absent semantics. A catalog can atomically select a new set
of shard IDs while old generations remain readable for recovery.

The historical version-1 fixture is committed at
`tests/fixtures/pack-indexes/v1/basic.index.hex` and is exercised by the SDK
integration tests.
