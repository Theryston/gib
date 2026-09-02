# Immutable repository objects

This document specifies the released version-1 immutable-object format and the
additive version-2 transform envelope used by snapshots, trees, packs, and
pack indexes. Repository bootstrap, descriptor, and HEAD records are control
records and retain their released version-1 schemas; they are not immutable
content objects and are not wrapped in this envelope.

## Version-1 envelope

An object is encoded as a named MessagePack map with these fields, in this
canonical order:

| Field | Type | Meaning |
| --- | --- | --- |
| `envelope_version` | unsigned 16-bit integer | Common envelope schema version. Value: `1`. |
| `magic` | UTF-8 string | Repository marker, exactly `GIB`. |
| `kind` | UTF-8 string | One of `snapshot`, `tree`, `pack`, or `index`. |
| `object_version` | unsigned 16-bit integer | Payload schema selected by `kind`. Current value is `1` for every kind. |
| `codec` | UTF-8 string | Payload transport codec. Value: `none`. |
| `encryption` | UTF-8 string | Payload transport encryption. Value: `none`. |
| `plaintext_length` | unsigned 64-bit integer | Length of the canonical plaintext payload. |
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

Version 1 has a 64 MiB canonical payload limit. The version-2 reader accepts a
64 MiB canonical payload, at most 65 MiB of stored payload, and a complete
object no larger than `MAX_IMMUTABLE_OBJECT_BYTES`. Reader-based decoding takes
at most one byte beyond that complete-object limit, rejects oversized input
before MessagePack decoding, and scans string, binary, collection, nesting,
and trailing-byte boundaries before deserializing the private wire model.

## Version-2 transform envelope

Version 2 is selected by `envelope_version = 2`; a version-1 decoder never
interprets a version-2 map. A transformed object is a named MessagePack map
with these fields, in this canonical order:

| Field | Type | Meaning |
| --- | --- | --- |
| `envelope_version` | unsigned 16-bit integer | Value: `2`. |
| `magic` | UTF-8 string | Repository marker, exactly `GIB`. |
| `kind` | UTF-8 string | One of `snapshot`, `tree`, `pack`, or `index`. |
| `object_version` | unsigned 16-bit integer | Payload schema selected by `kind`. |
| `codec` | UTF-8 string | `none` or `zstd`. |
| `compression_level` | signed 32-bit integer | Zstandard level `1..=22`; `0` when `codec` is `none`. |
| `encryption` | UTF-8 string | `none` or `xchacha20-poly1305`. |
| `encryption_kdf` | UTF-8 string | `none` or `argon2id-v1`. |
| `kdf_memory_kib` | unsigned 32-bit integer | Argon2 memory cost; zero for `none`. |
| `kdf_time_cost` | unsigned 32-bit integer | Argon2 pass count; zero for `none`. |
| `kdf_parallelism` | unsigned 32-bit integer | Argon2 parallelism; zero for `none`. |
| `kdf_output_length` | unsigned 32-bit integer | Derived key length; zero for `none`, otherwise `32`. |
| `encryption_salt` | MessagePack binary | The 16-byte repository salt; empty for `none`. |
| `encryption_nonce` | MessagePack binary | The unique 24-byte XChaCha20 nonce; empty for `none`. |
| `plaintext_length` | unsigned 64-bit integer | Length of canonical plaintext before transforms. |
| `payload_length` | unsigned 64-bit integer | Number of stored payload bytes, including the 16-byte AEAD tag. |
| `object_id` | MessagePack binary, 32 bytes | SHA-256 identity digest of canonical plaintext. |
| `payload_checksum` | MessagePack binary, 32 bytes | SHA-256 digest of the stored payload. |
| `payload` | MessagePack binary | Transformed payload. With encryption, it is `ciphertext || tag`. |
| `envelope_checksum` | MessagePack binary, 32 bytes | SHA-256 digest of the unsigned version-2 envelope. |

The encoder uses MessagePack named-map encoding with the field order above and
forces byte slices to MessagePack binary values. Every field is critical;
unknown fields, duplicate fields, wrong types, missing fields, invalid lengths,
and trailing bytes are rejected.

The transform pipeline is fixed and ordered:

```text
canonical plaintext
    -> Zstandard single frame (when codec = zstd)
    -> XChaCha20-Poly1305 in place (when encryption is enabled)
    -> append the 16-byte authentication tag
```

Zstandard uses the recorded level, constrains its encoder window to 2^26 bytes,
and includes its frame checksum. The decoder enforces the same maximum window
and writes through a bounded output; the declared plaintext length must be
reached exactly. The stored-payload checksum is checked before decompression or
decryption, and the outer envelope checksum is checked before either transform.

### Repository key derivation

`argon2id-v1` means Argon2id, version `0x13`, with these fixed parameters:

```text
m = 65536 KiB (64 MiB)
t = 3 passes
p = 1 lane
output = 32 bytes
salt = 16 bytes, generated once per repository
```

The same salt is recorded in every encrypted object from a repository. The
derived key and password are never persisted. Changing any KDF parameter is a
new KDF identifier and requires a new decoder; an existing identifier is never
reinterpreted.

### Associated data

XChaCha20-Poly1305 authenticates the transform metadata in addition to the
payload. The exact associated-data byte sequence is:

```text
GIB immutable object transform aad\0
|| envelope_version as u16 big-endian
|| length(u32 big-endian) || magic
|| length(u32 big-endian) || kind
|| object_version as u16 big-endian
|| length(u32 big-endian) || codec
|| compression_level as i32 big-endian
|| length(u32 big-endian) || encryption
|| length(u32 big-endian) || encryption_kdf
|| kdf_memory_kib as u32 big-endian
|| kdf_time_cost as u32 big-endian
|| kdf_parallelism as u32 big-endian
|| kdf_output_length as u32 big-endian
|| length(u32 big-endian) || encryption_salt
|| length(u32 big-endian) || encryption_nonce
|| plaintext_length as u64 big-endian
|| payload_length as u64 big-endian
|| object_id
```

`payload_checksum` and `envelope_checksum` are integrity fields but are not
part of the object identity or associated data. A nonce is generated from the
OS CSPRNG for every encrypted encoding; it is never reused for a second
encoding under the same repository key.

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

For the version-1 uncompressed and unencrypted representation,
`plaintext_length == payload_length == len(P)`. The payload checksum is
`SHA-256(payload)`. The envelope checksum is calculated over the canonical
unsigned envelope with `envelope_checksum` omitted. Decoding validates lengths,
digest sizes, the envelope checksum, the payload checksum, and finally the
object ID before returning a domain object or decoding its kind-specific
payload. For version 2, the object ID and envelope checksum rules are the
same, while `payload` is first authenticated and then decoded back to `P`;
`ImmutableObject::payload()` always returns canonical plaintext.

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
2. Read the envelope version marker and select exactly the matching private
   wire model; no version fallback is attempted.
3. Validate envelope version, magic, kind, object version, transform metadata,
   lengths, digests, and both checksums.
4. Select the transform decoder by the recorded codec, encryption, and KDF
   identifiers. Missing keys, authentication failures, and transform failures
   are explicit errors; they never fall back to plaintext.
5. Select the payload decoder by the validated `(kind, object_version)` pair
   and verify the canonical plaintext length and object ID.

The version-1 snapshot decoder also re-encodes the validated snapshot fields
and rejects a payload whose bytes are not the canonical version-1 encoding.
Opaque tree, pack, and index payloads must be canonicalized by their owning
payload implementation before being passed to the common envelope API.

An unknown envelope version returns an unsupported-version error. An unknown
object kind or metadata value is malformed; a known but not-yet-implemented
codec, encryption scheme, or KDF returns an incompatibility error. There is no
fallback to a different version, kind, codec, encryption mode, or password.

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

The immutable binary pack framing, size policy, pack ID boundary, entry
locations, and validation order are specified separately in
[`docs/packs.md`](packs.md). The common object envelope may wrap a pack payload,
but transport metadata in that envelope does not change the pack's own entry
identity or framing rules.

The versioned pack-index shard framing, chunk-ID prefix policy, publication
keys, and bounded range lookup are specified separately in
[`docs/pack-indexes.md`](pack-indexes.md).

## Fixtures

The repository contains exact hexadecimal byte fixtures under
`tests/fixtures/repository/v1/objects/`:

- `tree-envelope.hex` is the canonical tree envelope and golden ID fixture.
- `snapshot-envelope.hex` is a current enveloped snapshot.
- `snapshot-legacy.hex` is the released standalone snapshot representation.

The fixtures are text-wrapped only to keep the bytes reviewable in source
control; tests decode the hexadecimal contents before passing the original
bytes to the SDK.

The deterministic version-2 encrypted envelope fixture is
`tests/fixtures/repository/v2/objects/tree-encrypted-envelope.hex`. Its test
password is supplied only by the fixture test; passwords and derived keys are
not persisted in repository objects.

The historical version-1 pack-index fixture is
`tests/fixtures/pack-indexes/v1/basic.index.hex`.
