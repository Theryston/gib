use super::repository::{FormatError, decode_messagepack_with_limits};
use crate::domain::{
    CURRENT_OBJECT_ENVELOPE_VERSION, ImmutableObject, ImmutableObjectParts,
    MAX_IMMUTABLE_OBJECT_BYTES, MAX_IMMUTABLE_OBJECT_PAYLOAD_BYTES, ObjectCodec, ObjectEncryption,
    ObjectId, ObjectKind, REPOSITORY_MAGIC,
};
use rmp_serde::config::BytesMode;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::io::Read;

/// The digest size used by the object identity and integrity fields.
const DIGEST_LENGTH: usize = 32;

const MAX_ENVELOPE_STRING_BYTES: u32 = 64;
const MAX_ENVELOPE_COLLECTION_ITEMS: u32 = 32;
const MAX_ENVELOPE_DEPTH: usize = 8;

#[derive(Serialize)]
struct ObjectEnvelopeUnsignedWire<'a> {
    envelope_version: u16,
    magic: &'a str,
    kind: &'a str,
    object_version: u16,
    codec: &'a str,
    encryption: &'a str,
    plaintext_length: u64,
    payload_length: u64,
    object_id: &'a [u8],
    payload_checksum: &'a [u8],
    payload: &'a [u8],
}

#[derive(Serialize)]
struct ObjectEnvelopeWire<'a> {
    envelope_version: u16,
    magic: &'a str,
    kind: &'a str,
    object_version: u16,
    codec: &'a str,
    encryption: &'a str,
    plaintext_length: u64,
    payload_length: u64,
    object_id: &'a [u8],
    payload_checksum: &'a [u8],
    payload: &'a [u8],
    envelope_checksum: &'a [u8],
}

struct UnsignedEnvelope<'a> {
    kind: ObjectKind,
    object_version: u16,
    codec: ObjectCodec,
    encryption: ObjectEncryption,
    plaintext_length: u64,
    payload_length: u64,
    object_id: &'a [u8],
    payload_checksum: &'a [u8],
    payload: &'a [u8],
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct ObjectEnvelopeWireOwned {
    envelope_version: u16,
    magic: String,
    kind: String,
    object_version: u16,
    codec: String,
    encryption: String,
    plaintext_length: u64,
    payload_length: u64,
    object_id: Vec<u8>,
    payload_checksum: Vec<u8>,
    payload: Vec<u8>,
    envelope_checksum: Vec<u8>,
}

pub(crate) fn calculate_object_id(
    kind: ObjectKind,
    object_version: u16,
    canonical_plaintext: &[u8],
) -> ObjectId {
    let mut hasher = Sha256::new();
    hasher.update(b"GIB immutable object identity\0");
    hasher.update(kind.as_str().as_bytes());
    hasher.update([0]);
    hasher.update(object_version.to_be_bytes());
    hasher.update(canonical_plaintext);
    ObjectId::from_digest(hasher.finalize().into())
}

pub(crate) fn encode_object_envelope(
    kind: ObjectKind,
    object_version: u16,
    codec: ObjectCodec,
    encryption: ObjectEncryption,
    canonical_plaintext: &[u8],
) -> Result<Vec<u8>, FormatError> {
    validate_kind_version(kind, object_version)?;
    if codec != ObjectCodec::None {
        return Err(FormatError::UnsupportedCodec);
    }
    if encryption != ObjectEncryption::None {
        return Err(FormatError::UnsupportedEncryption);
    }
    if canonical_plaintext.len() > MAX_IMMUTABLE_OBJECT_PAYLOAD_BYTES {
        return Err(FormatError::InputTooLarge);
    }

    let object_id = calculate_object_id(kind, object_version, canonical_plaintext);
    let payload_checksum = Sha256::digest(canonical_plaintext);
    let payload_length =
        u64::try_from(canonical_plaintext.len()).map_err(|_| FormatError::InputTooLarge)?;
    let unsigned = encode_unsigned(&UnsignedEnvelope {
        kind,
        object_version,
        codec,
        encryption,
        plaintext_length: payload_length,
        payload_length,
        object_id: object_id.as_digest(),
        payload_checksum: &payload_checksum,
        payload: canonical_plaintext,
    })?;
    let envelope_checksum = Sha256::digest(&unsigned);
    let bytes = encode_wire(&ObjectEnvelopeWire {
        envelope_version: CURRENT_OBJECT_ENVELOPE_VERSION,
        magic: REPOSITORY_MAGIC,
        kind: kind.as_str(),
        object_version,
        codec: codec.as_str(),
        encryption: encryption.as_str(),
        plaintext_length: payload_length,
        payload_length,
        object_id: object_id.as_digest(),
        payload_checksum: &payload_checksum,
        payload: canonical_plaintext,
        envelope_checksum: &envelope_checksum,
    })?;
    if bytes.len() > MAX_IMMUTABLE_OBJECT_BYTES {
        return Err(FormatError::InputTooLarge);
    }
    Ok(bytes)
}

pub(crate) fn decode_object_envelope(bytes: &[u8]) -> Result<ImmutableObject, FormatError> {
    let wire: ObjectEnvelopeWireOwned = decode_messagepack_with_limits(
        bytes,
        MAX_IMMUTABLE_OBJECT_BYTES,
        MAX_ENVELOPE_STRING_BYTES,
        u32::try_from(MAX_IMMUTABLE_OBJECT_PAYLOAD_BYTES)
            .map_err(|_| FormatError::InputTooLarge)?,
        MAX_ENVELOPE_COLLECTION_ITEMS,
        MAX_ENVELOPE_DEPTH,
    )?;
    validate_envelope(&wire)
}

pub(crate) fn decode_object_envelope_from_reader<R: Read>(
    reader: &mut R,
) -> Result<ImmutableObject, FormatError> {
    let bytes = read_bounded(reader, MAX_IMMUTABLE_OBJECT_BYTES)?;
    decode_object_envelope(&bytes)
}

fn validate_envelope(wire: &ObjectEnvelopeWireOwned) -> Result<ImmutableObject, FormatError> {
    if wire.envelope_version != CURRENT_OBJECT_ENVELOPE_VERSION {
        return Err(FormatError::UnsupportedVersion {
            version: wire.envelope_version,
        });
    }
    if wire.magic != REPOSITORY_MAGIC {
        return Err(FormatError::InvalidMagic);
    }
    let kind = ObjectKind::parse(&wire.kind).ok_or(FormatError::InvalidObjectKind)?;
    validate_kind_version(kind, wire.object_version)?;
    let codec = ObjectCodec::parse(&wire.codec).ok_or(FormatError::InvalidCodec)?;
    let encryption =
        ObjectEncryption::parse(&wire.encryption).ok_or(FormatError::InvalidEncryption)?;
    if codec != ObjectCodec::None {
        return Err(FormatError::UnsupportedCodec);
    }
    if encryption != ObjectEncryption::None {
        return Err(FormatError::UnsupportedEncryption);
    }
    if wire.payload_length > MAX_IMMUTABLE_OBJECT_PAYLOAD_BYTES as u64
        || wire.plaintext_length > MAX_IMMUTABLE_OBJECT_PAYLOAD_BYTES as u64
        || wire.payload_length != wire.payload.len() as u64
        || wire.plaintext_length != wire.payload.len() as u64
    {
        return Err(FormatError::InvalidLength);
    }
    let object_id = digest_from_bytes(&wire.object_id)?;
    let payload_checksum = digest_from_bytes(&wire.payload_checksum)?;
    let envelope_checksum = digest_from_bytes(&wire.envelope_checksum)?;
    let unsigned = encode_unsigned(&UnsignedEnvelope {
        kind,
        object_version: wire.object_version,
        codec,
        encryption,
        plaintext_length: wire.plaintext_length,
        payload_length: wire.payload_length,
        object_id: &wire.object_id,
        payload_checksum: &wire.payload_checksum,
        payload: &wire.payload,
    })?;
    if Sha256::digest(&unsigned).as_slice() != envelope_checksum {
        return Err(FormatError::InvalidEnvelopeChecksum);
    }
    if Sha256::digest(&wire.payload).as_slice() != payload_checksum {
        return Err(FormatError::InvalidPayloadChecksum);
    }
    let expected_id = calculate_object_id(kind, wire.object_version, &wire.payload);
    if expected_id.as_digest() != &object_id {
        return Err(FormatError::InvalidObjectId);
    }
    Ok(ImmutableObject::from_validated_parts(
        ImmutableObjectParts {
            kind,
            version: wire.object_version,
            codec,
            encryption,
            plaintext_length: wire.plaintext_length,
            payload_length: wire.payload_length,
            object_id: expected_id,
            payload: wire.payload.clone(),
        },
    ))
}

fn validate_kind_version(kind: ObjectKind, object_version: u16) -> Result<(), FormatError> {
    if object_version != kind.current_version() {
        return Err(FormatError::UnsupportedObjectVersion {
            version: object_version,
        });
    }
    Ok(())
}

fn digest_from_bytes(value: &[u8]) -> Result<[u8; DIGEST_LENGTH], FormatError> {
    value
        .try_into()
        .map_err(|_| FormatError::InvalidDigestLength)
}

fn encode_unsigned(envelope: &UnsignedEnvelope<'_>) -> Result<Vec<u8>, FormatError> {
    encode_wire(&ObjectEnvelopeUnsignedWire {
        envelope_version: CURRENT_OBJECT_ENVELOPE_VERSION,
        magic: REPOSITORY_MAGIC,
        kind: envelope.kind.as_str(),
        object_version: envelope.object_version,
        codec: envelope.codec.as_str(),
        encryption: envelope.encryption.as_str(),
        plaintext_length: envelope.plaintext_length,
        payload_length: envelope.payload_length,
        object_id: envelope.object_id,
        payload_checksum: envelope.payload_checksum,
        payload: envelope.payload,
    })
}

fn encode_wire<T: Serialize>(value: &T) -> Result<Vec<u8>, FormatError> {
    let mut bytes = Vec::new();
    let mut serializer = rmp_serde::Serializer::new(&mut bytes)
        .with_struct_map()
        .with_bytes(BytesMode::ForceAll);
    value
        .serialize(&mut serializer)
        .map_err(|_| FormatError::Serialization)?;
    Ok(bytes)
}

fn read_bounded<R: Read>(reader: &mut R, max_bytes: usize) -> Result<Vec<u8>, FormatError> {
    let read_limit = u64::try_from(max_bytes)
        .ok()
        .and_then(|value| value.checked_add(1))
        .ok_or(FormatError::InputTooLarge)?;
    let mut limited = reader.take(read_limit);
    let mut bytes = Vec::new();
    let mut buffer = [0u8; 64 * 1024];
    loop {
        let read = limited
            .read(&mut buffer)
            .map_err(|_| FormatError::InvalidEncoding)?;
        if read == 0 {
            break;
        }
        if read > buffer.len() {
            return Err(FormatError::InvalidEncoding);
        }
        if bytes
            .len()
            .checked_add(read)
            .is_none_or(|length| length > max_bytes)
        {
            return Err(FormatError::InputTooLarge);
        }
        bytes.extend_from_slice(&buffer[..read]);
    }
    Ok(bytes)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::{CURRENT_SNAPSHOT_VERSION, ObjectKind};
    use serde::Serialize;
    use std::io::Cursor;

    fn encode_fixture(wire: &ObjectEnvelopeWire<'_>) -> Vec<u8> {
        encode_wire(wire).unwrap_or_default()
    }

    fn encode_owned_fixture(wire: &ObjectEnvelopeWireOwned) -> Vec<u8> {
        encode_wire(wire).unwrap_or_default()
    }

    fn decode_owned_fixture(bytes: &[u8]) -> ObjectEnvelopeWireOwned {
        rmp_serde::from_slice(bytes).unwrap_or_else(|_| ObjectEnvelopeWireOwned {
            envelope_version: 0,
            magic: String::new(),
            kind: String::new(),
            object_version: 0,
            codec: String::new(),
            encryption: String::new(),
            plaintext_length: 0,
            payload_length: 0,
            object_id: Vec::new(),
            payload_checksum: Vec::new(),
            payload: Vec::new(),
            envelope_checksum: Vec::new(),
        })
    }

    fn refresh_envelope_checksum(wire: &mut ObjectEnvelopeWireOwned) {
        let Some(kind) = ObjectKind::parse(&wire.kind) else {
            return;
        };
        let Some(codec) = ObjectCodec::parse(&wire.codec) else {
            return;
        };
        let Some(encryption) = ObjectEncryption::parse(&wire.encryption) else {
            return;
        };
        let Ok(unsigned) = encode_unsigned(&UnsignedEnvelope {
            kind,
            object_version: wire.object_version,
            codec,
            encryption,
            plaintext_length: wire.plaintext_length,
            payload_length: wire.payload_length,
            object_id: &wire.object_id,
            payload_checksum: &wire.payload_checksum,
            payload: &wire.payload,
        }) else {
            return;
        };
        wire.envelope_checksum = Sha256::digest(&unsigned).to_vec();
    }

    fn hex_encode_bytes(bytes: &[u8]) -> String {
        const HEX: &[u8; 16] = b"0123456789abcdef";
        let mut output = String::with_capacity(bytes.len() * 2);
        for byte in bytes {
            output.push(HEX[usize::from(byte >> 4)] as char);
            output.push(HEX[usize::from(byte & 0x0f)] as char);
        }
        output
    }

    fn canonical_fixture() -> Vec<u8> {
        b"canonical tree payload".to_vec()
    }

    #[test]
    fn every_current_kind_round_trips_through_the_common_envelope() {
        for kind in [
            ObjectKind::Snapshot,
            ObjectKind::Tree,
            ObjectKind::Pack,
            ObjectKind::Index,
        ] {
            let payload = canonical_fixture();
            let bytes = encode_object_envelope(
                kind,
                kind.current_version(),
                ObjectCodec::None,
                ObjectEncryption::None,
                &payload,
            );
            assert!(bytes.is_ok());
            let decoded = decode_object_envelope(&bytes.unwrap_or_default());
            assert!(decoded.is_ok());
            let decoded = decoded.unwrap_or_else(|_| {
                ImmutableObject::from_validated_parts(ImmutableObjectParts {
                    kind,
                    version: kind.current_version(),
                    codec: ObjectCodec::None,
                    encryption: ObjectEncryption::None,
                    plaintext_length: 0,
                    payload_length: 0,
                    object_id: ObjectId::from_digest([0; 32]),
                    payload: Vec::new(),
                })
            });
            assert_eq!(decoded.kind(), kind);
            assert_eq!(decoded.version(), kind.current_version());
            assert_eq!(decoded.payload(), payload);
            assert_eq!(decoded.object_id(), &calculate_object_id(kind, 1, &payload));
        }
    }

    #[test]
    fn property_style_round_trip_corpus_covers_empty_binary_and_variable_payloads() {
        for kind in [
            ObjectKind::Snapshot,
            ObjectKind::Tree,
            ObjectKind::Pack,
            ObjectKind::Index,
        ] {
            for seed in 0u8..=31 {
                let length = (usize::from(seed) * 257) % 4_097;
                let payload: Vec<u8> = (0..length)
                    .map(|index| (index as u8).wrapping_mul(31) ^ seed)
                    .collect();
                let bytes = encode_object_envelope(
                    kind,
                    kind.current_version(),
                    ObjectCodec::None,
                    ObjectEncryption::None,
                    &payload,
                )
                .unwrap_or_default();
                let decoded = decode_object_envelope(&bytes);
                assert!(decoded.is_ok());
                let decoded = decoded.unwrap_or_else(|_| {
                    ImmutableObject::from_validated_parts(ImmutableObjectParts {
                        kind,
                        version: kind.current_version(),
                        codec: ObjectCodec::None,
                        encryption: ObjectEncryption::None,
                        plaintext_length: 0,
                        payload_length: 0,
                        object_id: ObjectId::from_digest([0; 32]),
                        payload: Vec::new(),
                    })
                });
                assert_eq!(decoded.payload(), payload);
                assert_eq!(decoded.plaintext_length(), length as u64);
                assert_eq!(decoded.payload_length(), length as u64);
            }
        }
    }

    #[test]
    fn reader_is_bounded_before_messagepack_decoding() {
        let bytes = vec![0u8; MAX_IMMUTABLE_OBJECT_BYTES + 1];
        assert_eq!(
            decode_object_envelope_from_reader(&mut Cursor::new(bytes)),
            Err(FormatError::InputTooLarge)
        );
    }

    #[test]
    fn unsupported_kind_versions_codecs_and_encryption_do_not_fallback() {
        assert_eq!(
            encode_object_envelope(
                ObjectKind::Tree,
                CURRENT_SNAPSHOT_VERSION + 1,
                ObjectCodec::None,
                ObjectEncryption::None,
                b"payload",
            ),
            Err(FormatError::UnsupportedObjectVersion {
                version: CURRENT_SNAPSHOT_VERSION + 1,
            })
        );
        assert_eq!(
            encode_object_envelope(
                ObjectKind::Tree,
                1,
                ObjectCodec::Zstd,
                ObjectEncryption::None,
                b"payload",
            ),
            Err(FormatError::UnsupportedCodec)
        );
        assert_eq!(
            encode_object_envelope(
                ObjectKind::Tree,
                1,
                ObjectCodec::None,
                ObjectEncryption::XChaCha20Poly1305,
                b"payload",
            ),
            Err(FormatError::UnsupportedEncryption)
        );
    }

    #[test]
    fn payload_and_header_mutations_are_rejected_deterministically() {
        let payload = canonical_fixture();
        let id = calculate_object_id(ObjectKind::Tree, 1, &payload);
        let payload_checksum = Sha256::digest(&payload);
        let unsigned = encode_unsigned(&UnsignedEnvelope {
            kind: ObjectKind::Tree,
            object_version: 1,
            codec: ObjectCodec::None,
            encryption: ObjectEncryption::None,
            plaintext_length: payload.len() as u64,
            payload_length: payload.len() as u64,
            object_id: id.as_digest(),
            payload_checksum: &payload_checksum,
            payload: &payload,
        })
        .unwrap_or_default();
        let envelope_checksum = Sha256::digest(&unsigned);
        let wire = ObjectEnvelopeWire {
            envelope_version: 1,
            magic: REPOSITORY_MAGIC,
            kind: "tree",
            object_version: 1,
            codec: "none",
            encryption: "none",
            plaintext_length: payload.len() as u64,
            payload_length: payload.len() as u64,
            object_id: id.as_digest(),
            payload_checksum: &payload_checksum,
            payload: &payload,
            envelope_checksum: &envelope_checksum,
        };
        let valid = encode_fixture(&wire);
        assert!(decode_object_envelope(&valid).is_ok());

        let mut truncated = valid.clone();
        truncated.pop();
        assert_eq!(
            decode_object_envelope(&truncated),
            Err(FormatError::InvalidEncoding)
        );
        let mut trailing = valid.clone();
        trailing.push(0);
        assert_eq!(
            decode_object_envelope(&trailing),
            Err(FormatError::TrailingBytes)
        );

        let mut corrupt_payload = payload.clone();
        corrupt_payload[0] ^= 1;
        let corrupt_wire = ObjectEnvelopeWire {
            payload: &corrupt_payload,
            ..wire
        };
        assert_eq!(
            decode_object_envelope(&encode_fixture(&corrupt_wire)),
            Err(FormatError::InvalidEnvelopeChecksum)
        );

        let mut mutated = decode_owned_fixture(&valid);
        mutated.magic = "BAD".to_owned();
        assert_eq!(
            decode_object_envelope(&encode_owned_fixture(&mutated)),
            Err(FormatError::InvalidMagic)
        );

        let mut mutated = decode_owned_fixture(&valid);
        mutated.kind = "future".to_owned();
        assert_eq!(
            decode_object_envelope(&encode_owned_fixture(&mutated)),
            Err(FormatError::InvalidObjectKind)
        );

        let mut mutated = decode_owned_fixture(&valid);
        mutated.envelope_version = 99;
        assert_eq!(
            decode_object_envelope(&encode_owned_fixture(&mutated)),
            Err(FormatError::UnsupportedVersion { version: 99 })
        );

        let mut mutated = decode_owned_fixture(&valid);
        mutated.object_version = 99;
        assert_eq!(
            decode_object_envelope(&encode_owned_fixture(&mutated)),
            Err(FormatError::UnsupportedObjectVersion { version: 99 })
        );

        let mut mutated = decode_owned_fixture(&valid);
        mutated.codec = "zstd".to_owned();
        assert_eq!(
            decode_object_envelope(&encode_owned_fixture(&mutated)),
            Err(FormatError::UnsupportedCodec)
        );

        let mut mutated = decode_owned_fixture(&valid);
        mutated.encryption = "xchacha20-poly1305".to_owned();
        assert_eq!(
            decode_object_envelope(&encode_owned_fixture(&mutated)),
            Err(FormatError::UnsupportedEncryption)
        );

        let mut mutated = decode_owned_fixture(&valid);
        mutated.payload_length += 1;
        assert_eq!(
            decode_object_envelope(&encode_owned_fixture(&mutated)),
            Err(FormatError::InvalidLength)
        );

        let mut mutated = decode_owned_fixture(&valid);
        mutated.plaintext_length += 1;
        assert_eq!(
            decode_object_envelope(&encode_owned_fixture(&mutated)),
            Err(FormatError::InvalidLength)
        );

        let mut mutated = decode_owned_fixture(&valid);
        mutated.payload_checksum[0] ^= 1;
        refresh_envelope_checksum(&mut mutated);
        assert_eq!(
            decode_object_envelope(&encode_owned_fixture(&mutated)),
            Err(FormatError::InvalidPayloadChecksum)
        );

        let mut mutated = decode_owned_fixture(&valid);
        mutated.object_id[0] ^= 1;
        refresh_envelope_checksum(&mut mutated);
        assert_eq!(
            decode_object_envelope(&encode_owned_fixture(&mutated)),
            Err(FormatError::InvalidObjectId)
        );

        let mut mutated = decode_owned_fixture(&valid);
        mutated.envelope_checksum[0] ^= 1;
        assert_eq!(
            decode_object_envelope(&encode_owned_fixture(&mutated)),
            Err(FormatError::InvalidEnvelopeChecksum)
        );

        let mut mutated = decode_owned_fixture(&valid);
        mutated.object_id.pop();
        assert_eq!(
            decode_object_envelope(&encode_owned_fixture(&mutated)),
            Err(FormatError::InvalidDigestLength)
        );
    }

    #[test]
    fn canonical_identity_ignores_transport_metadata() {
        let payload = canonical_fixture();
        let id = calculate_object_id(ObjectKind::Pack, 1, &payload);
        assert_eq!(id, calculate_object_id(ObjectKind::Pack, 1, &payload));
        assert_ne!(id, calculate_object_id(ObjectKind::Tree, 1, &payload));
        assert_ne!(id, calculate_object_id(ObjectKind::Pack, 2, &payload));
    }

    #[test]
    fn envelope_wire_is_named_and_uses_binary_integrity_fields() {
        let bytes = encode_object_envelope(
            ObjectKind::Tree,
            1,
            ObjectCodec::None,
            ObjectEncryption::None,
            &canonical_fixture(),
        )
        .unwrap_or_default();
        assert_eq!(bytes.first().copied(), Some(0x8c));
        assert!(bytes.windows(2).any(|window| window == [0xc4, 0x20]));
        assert_eq!(
            hex_encode_bytes(&bytes),
            "8cb0656e76656c6f70655f76657273696f6e01a56d61676963a3474942a46b696e64a474726565ae6f626a6563745f76657273696f6e01a5636f646563a46e6f6e65aa656e6372797074696f6ea46e6f6e65b0706c61696e746578745f6c656e67746816ae7061796c6f61645f6c656e67746816a96f626a6563745f6964c420c740fdadc5c20672e5a77b27f10e71b119606dee4f2cb45267e416df3d0e0063b07061796c6f61645f636865636b73756dc4206fae073d97b085a584948bdfadcc84f3b36ac64e2e4b8b7107344d12528ffb0aa77061796c6f6164c41663616e6f6e6963616c2074726565207061796c6f6164b1656e76656c6f70655f636865636b73756dc4205b91b387ca79207f20e8c0052ee5f0d269ad2be49034fee63a9793c2c33c864f"
        );
        assert_eq!(
            calculate_object_id(ObjectKind::Tree, 1, &canonical_fixture()).as_str(),
            "c740fdadc5c20672e5a77b27f10e71b119606dee4f2cb45267e416df3d0e0063"
        );
    }

    #[derive(Serialize)]
    struct UnknownFieldWire<'a> {
        envelope_version: u16,
        magic: &'a str,
        kind: &'a str,
        object_version: u16,
        codec: &'a str,
        encryption: &'a str,
        plaintext_length: u64,
        payload_length: u64,
        object_id: &'a [u8],
        payload_checksum: &'a [u8],
        payload: &'a [u8],
        envelope_checksum: &'a [u8],
        critical_future_field: &'a str,
    }

    #[test]
    fn unknown_critical_fields_are_rejected() {
        let payload = canonical_fixture();
        let bytes = encode_object_envelope(
            ObjectKind::Tree,
            1,
            ObjectCodec::None,
            ObjectEncryption::None,
            &payload,
        )
        .unwrap_or_default();
        let decoded = decode_owned_fixture(&bytes);
        let extra = encode_wire(&UnknownFieldWire {
            envelope_version: decoded.envelope_version,
            magic: &decoded.magic,
            kind: &decoded.kind,
            object_version: decoded.object_version,
            codec: &decoded.codec,
            encryption: &decoded.encryption,
            plaintext_length: decoded.plaintext_length,
            payload_length: decoded.payload_length,
            object_id: &decoded.object_id,
            payload_checksum: &decoded.payload_checksum,
            payload: &decoded.payload,
            envelope_checksum: &decoded.envelope_checksum,
            critical_future_field: "reject-me",
        })
        .unwrap_or_default();
        assert_eq!(
            decode_object_envelope(&extra),
            Err(FormatError::InvalidEncoding)
        );
    }
}
