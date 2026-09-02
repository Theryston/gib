# Immutable packs

Gib groups already transformed chunk payloads into immutable pack files. Pack
construction is an SDK/domain concern: it does not choose chunk boundaries,
compress payloads, encrypt payloads, upload objects, or build an index. The
caller supplies entries in the order that should be stored and receives the
locations required by a pack index.

## Version and limits

The current pack file format is version `1`. This is separate from the common
immutable-object envelope's `object_version`: the envelope selects the pack
payload schema, while the pack header selects the binary container decoder. A
decoder selects a decoder by
the version in the pack header and rejects every other version. It never
interprets an unknown version as version 1.

The default policy targets 64 MiB and has a 128 MiB hard maximum. Both values
include all headers, entry padding, and the footer. The target is a soft
boundary: before adding to a non-empty current pack, the builder seals it when
the resulting pack would exceed the target. A first or single oversized entry
may therefore exceed the target. The maximum is hard for ordinary packs.

An entry whose own frame cannot fit under the configured maximum is emitted in
a pack containing exactly that one entry. This single-entry pack may exceed
the configured maximum, but the complete pack must remain at or below the SDK
absolute limit of 1 GiB plus 2 MiB. A second entry is never added to such a
pack. This exception prevents a single transformed chunk from being silently
rejected merely because pack framing is larger than the normal target.

The builder keeps one current pack and its entry metadata. `PackBuilder::add`
returns a completed pack at a boundary; `PackBuilder::add_stream` publishes
each completed pack immediately through a caller-provided publisher. The
publisher receives only sealed bytes, and the builder drops a completed pack
after the publisher returns. A failed publisher aborts the current build. An
already published pack cannot be rolled back by the builder.

## Binary representation

All integers are unsigned unless stated otherwise and are encoded in big-endian
order. There are no variable-length or serialized Rust structs in this
format. Unknown flags and non-zero reserved bytes are rejected as critical
format errors. Every pack body and footer boundary is aligned to 8 bytes.

### Header

The header is exactly 64 bytes and starts at offset 0:

| Offset | Length | Field | Value or meaning |
| ---: | ---: | --- | --- |
| 0 | 4 | magic | ASCII `GIBP` |
| 4 | 2 | version | Pack format version, currently `1` |
| 6 | 2 | flags | `0`; all bits are critical |
| 8 | 4 | header length | `64` |
| 12 | 4 | alignment | `8` |
| 16 | 8 | target size | Configured soft target, including framing |
| 24 | 8 | maximum size | Configured hard maximum, including framing |
| 32 | 8 | entries offset | `64` |
| 40 | 8 | entry count | Number of entry frames in the body |
| 48 | 8 | payload bytes | Sum of transformed payload lengths |
| 56 | 8 | reserved | All zero |

The header is written with zero counts and then finalized with the actual
counts before the body checksum and pack ID are calculated. A reader validates
all header fields before scanning entries.

### Entry frame

Entries are consecutive and begin at the header's `entries offset`. Each entry
header is exactly 96 bytes:

| Relative offset | Length | Field | Value or meaning |
| ---: | ---: | --- | --- |
| 0 | 4 | magic | ASCII `ENTR` |
| 4 | 2 | version | Pack format version, currently `1` |
| 6 | 2 | flags | `0`; all bits are critical |
| 8 | 8 | entry length | Header + payload + zero padding, aligned to 8 |
| 16 | 32 | chunk ID | Content ID of the logical plaintext chunk |
| 48 | 8 | plaintext length | Logical chunk length before transforms |
| 56 | 8 | payload length | Number of transformed payload bytes |
| 64 | 32 | payload checksum | SHA-256 of the transformed payload |

The transformed payload starts immediately after the entry header. Padding is
zero-filled until `entry length`. The entry length must equal
`align8(96 + payload length)`; a reader rejects an inconsistent length, an
overrun, non-zero padding, or a payload checksum mismatch.

The entry's `chunk ID` is the logical content identity. Compression,
encryption, and other transform metadata belong to the payload supplied by the
caller and do not change that ID. The pack builder does not decode or
reinterpret the payload.

### Footer

The footer is exactly 104 bytes and is the final bytes of the file. Its start
offset is the body length:

| Relative offset | Length | Field | Value or meaning |
| ---: | ---: | --- | --- |
| 0 | 4 | magic | ASCII `GIBF` |
| 4 | 2 | version | Pack format version, currently `1` |
| 6 | 2 | flags | `0`; all bits are critical |
| 8 | 4 | footer length | `104` |
| 12 | 8 | entry count | Must match the header and scanned entries |
| 20 | 8 | body length | Footer start offset |
| 28 | 8 | payload bytes | Must match the header and scanned entries |
| 36 | 32 | body checksum | SHA-256 of every byte before the footer |
| 68 | 32 | pack ID | SHA-256 identity of the versioned body |
| 100 | 4 | reserved | All zero |

The complete length is `body length + 104`. The footer itself is excluded from
the body checksum and pack ID, which avoids a circular identity calculation.
The body checksum covers the header, every entry header, every transformed
payload, and every padding byte.

## Identity and locations

For format version `V` and body bytes `B`, the pack ID is the SHA-256 digest of
the following exact byte sequence:

```text
GIB pack identity\0
|| V as two big-endian bytes
|| B
```

The body includes the configured target and maximum, entry order, entry
framing, transformed payload bytes, and finalized counts. Consequently the
same configuration and ordered logical inputs produce the same pack bytes and
ID in separate runs. The pack contract does not promise the same ID when
concurrent production schedules supply entries in a different order.

Each sealed pack returns an ordered `PackEntryLocation` for every supplied
entry. A location contains the pack ID, chunk ID, entry offset, payload offset,
aligned entry length, transformed payload length, and logical plaintext
length. `PackReader::new` verifies the complete file before exposing any
payload; `PackReader::payload` accepts only a location returned for that
verified pack and returns a zero-copy slice.

## Validation and compatibility

Pack validation is performed in this order:

1. Bound the complete input to the SDK absolute pack limit.
2. Validate header/footer framing, versions, flags, reserved bytes, alignment,
   configured limits, and body length.
3. Verify the body checksum and pack ID.
4. Scan every entry in order, checking frame lengths, non-overlap, payload
   ranges, padding, and payload checksums.
5. Compare scanned counts and lengths with both header and footer.

Truncated data, trailing data, unknown critical fields, unsupported versions,
invalid lengths or offsets, and any checksum mismatch fail with a typed SDK
error. No partially scanned pack is returned to callers. Future format changes
must add a new decoder and fixture; version 1 remains unchanged.

The persisted representation is private to `gib-sdk::format`. Public SDK
callers use `PackConfiguration`, `PackEntryInput`, `SealedPack`,
`PackEntryLocation`, and `PackReader`; no persisted wire model is part of the
public API.

The historical version-1 golden fixture is kept at
`tests/fixtures/packs/v1/basic.pack.hex` and is decoded as bytes by the pack
integration tests.

Versioned pack-index shards and bounded range lookup are specified in
[`pack-indexes.md`](pack-indexes.md).
