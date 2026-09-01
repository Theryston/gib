# Immutable repository objects

This document specifies the version-1 immutable-object format used by
snapshots, trees, packs, and pack indexes. Repository bootstrap, descriptor,
and HEAD records are control records and retain their released version-1
schemas; they are not immutable content objects and are not wrapped in this
envelope.

## Common envelope

An object is encoded as a named MessagePack map with these fields, in this
canonical order:

| Field | Type | Meaning |
| --- | --- | --- |
| `envelope_version` | unsigned 16-bit integer | Common envelope schema version. Current value: `1`. |
| `magic` | UTF-8 string | Repository marker, exactly `GIB`. |
| `kind` | UTF-8 string | One of `snapshot`, `tree`, `pack`, or `index`. |
| `object_version` | unsigned 16-bit integer | Payload schema selected by `kind`. Current value is `1` for every kind. |
| `codec` | UTF-8 string | Payload transport codec. Current value: `none`; `zstd` is reserved and rejected until implemented. |
| `encryption` | UTF-8 string | Payload transport encryption. Current value: `none`; `xchacha20-poly1305` is reserved and rejected until implemented. |
| `plaintext_length` | unsigned 64-bit integer | Length of the canonical plaintext payload after future transforms. |
| `payload_length` | unsigned 64-bit integer | Number of bytes stored in `payload`. |
| `object_id` | MessagePack binary, 32 bytes | SHA-256 identity digest. |
| `payload_checksum` | MessagePack binary, 32 bytes | SHA-256 digest of the stored payload. |
| `payload` | MessagePack binary | Canonical payload bytes. |
| `envelope_checksum` | MessagePack binary, 32 bytes | SHA-256 digest of the unsigned envelope containing all preceding fields. |

The encoder uses MessagePack named-map encoding with the field order above and
forces byte slices to MessagePack binary values. Strings are UTF-8. All
envelope fields are critical in version 1: unknown fields, duplicate fields,
wrong types, and missing fields are rejected. The decoder also rejects empty
input and any trailing MessagePack bytes.

The current limits are a 64 MiB canonical payload and a 64 MiB plus 4 KiB
complete object. Reader-based decoding takes at most one byte beyond the
complete-object limit, rejects oversized input before MessagePack decoding,
and scans string, binary, collection, nesting, and trailing-byte boundaries
before deserializing the private wire model.

## Identity and integrity

The object ID is calculated from canonical plaintext/domain content only. For
an object kind `K`, payload version `V`, and canonical payload bytes `P`, the
exact SHA-256 input is:

```text
GIB immutable object identity\0
|| UTF-8(K)
|| 0x00
|| V as two big-endian bytes
|| P
```

The result is the 32-byte object ID and is displayed as 64 lowercase
hexadecimal characters. The codec, encryption, plaintext length, payload
length, payload checksum, envelope checksum, and other transport metadata are
not part of the identity. Therefore changing transport metadata cannot change
the ID, while changing the kind, payload version, or canonical payload does.

For the current uncompressed and unencrypted representation,
`plaintext_length == payload_length == len(P)`. The payload checksum is
`SHA-256(payload)`. The envelope checksum is calculated over the canonical
unsigned envelope with `envelope_checksum` omitted. Decoding validates lengths,
digest sizes, the envelope checksum, the payload checksum, and finally the
object ID before returning a domain object or decoding its kind-specific
payload.

The conventional object keys are `snapshots/<id>`, `trees/<id>`,
`packs/<id>`, and `indexes/<id>`. These keys are derived from the validated
object ID and kind; a caller must not use a human label as an immutable object
key.

The existing `SnapshotId`/`SnapshotReference` API is retained for released
repositories whose snapshot paths use historical IDs. New content-addressed
callers should use the `ObjectId` returned by the envelope API when deriving a
snapshot key; the SDK does not silently rename an existing historical path.

## Decoder selection and compatibility

Decoding is explicit and additive:

1. Bound the input and validate its MessagePack framing.
2. Decode only the private envelope wire model.
3. Validate envelope version, magic, kind, object version, transport metadata,
   lengths, digests, checksums, and object ID.
4. Select the decoder by the validated `(kind, object_version)` pair.
5. Decode the canonical payload with that released payload decoder.

The version-1 snapshot decoder also re-encodes the validated snapshot fields
and rejects a payload whose bytes are not the canonical version-1 encoding.
Opaque tree, pack, and index payloads must be canonicalized by their owning
payload implementation before being passed to the common envelope API.

An unknown envelope version returns an unsupported-version error. An unknown
object kind or metadata value is malformed; a known but not-yet-implemented
codec or encryption scheme returns an incompatibility error. There is no
fallback to a different version or kind.

Snapshot version 1 previously existed as a standalone named MessagePack map
with a `checksum` field. The snapshot decoder keeps that released decoder
alongside the new envelope decoder so historical repositories remain readable.
New snapshot writes always emit the common envelope. The legacy decoder is
selected only when the new envelope is not present and the legacy version
marker is present; a malformed new object is never silently accepted as a
different version.

The persisted MessagePack structs live in the private `format` module. Public
SDK callers receive validated domain values such as `ImmutableObject`,
`ObjectId`, and `ObjectKind`, rather than the persisted wire representation.
Tree, pack, and index payload schemas can add their own versioned decoders
without changing this envelope or the existing snapshot decoder.

## Version-1 fixtures

The repository contains exact hexadecimal byte fixtures under
`tests/fixtures/repository/v1/objects/`:

- `tree-envelope.hex` is the canonical tree envelope and golden ID fixture.
- `snapshot-envelope.hex` is a current enveloped snapshot.
- `snapshot-legacy.hex` is the released standalone snapshot representation.

The fixtures are text-wrapped only to keep the bytes reviewable in source
control; tests decode the hexadecimal contents before passing the original
bytes to the SDK.
