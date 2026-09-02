use super::repository::{FormatError, decode_messagepack_with_limits};
use super::transform::{
    EncryptionContext, compress, decompress, decrypt_in_place, derive_encryption_context,
    encrypt_in_place, random_nonce,
};
use crate::domain::{
    ARGON2ID_MEMORY_COST_KIB, ARGON2ID_PARALLELISM, ARGON2ID_TIME_COST,
    CURRENT_OBJECT_ENVELOPE_VERSION, CURRENT_TRANSFORMED_OBJECT_ENVELOPE_VERSION, CompressionLevel,
    ImmutableObject, ImmutableObjectParts, MAX_IMMUTABLE_OBJECT_BYTES,
    MAX_IMMUTABLE_OBJECT_PAYLOAD_BYTES, MAX_IMMUTABLE_OBJECT_STORED_PAYLOAD_BYTES, ObjectCodec,
    ObjectEncryption, ObjectId, ObjectKind, ObjectTransformOptions, REPOSITORY_ENCRYPTION_KDF,
    REPOSITORY_ENCRYPTION_KEY_LENGTH, REPOSITORY_MAGIC, RepositorySalt,
    XCHACHA20_POLY1305_NONCE_LENGTH, XCHACHA20_POLY1305_TAG_LENGTH,
};
use rmp_serde::config::BytesMode;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::io::Read;
use zeroize::{Zeroize, Zeroizing};

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

#[derive(Serialize)]
struct ObjectEnvelopeV2UnsignedWire<'a> {
    envelope_version: u16,
    magic: &'a str,
    kind: &'a str,
    object_version: u16,
    codec: &'a str,
    compression_level: i32,
    encryption: &'a str,
    encryption_kdf: &'a str,
    kdf_memory_kib: u32,
    kdf_time_cost: u32,
    kdf_parallelism: u32,
    kdf_output_length: u32,
    encryption_salt: &'a [u8],
    encryption_nonce: &'a [u8],
    plaintext_length: u64,
    payload_length: u64,
    object_id: &'a [u8],
    payload_checksum: &'a [u8],
    payload: &'a [u8],
}

#[derive(Serialize)]
struct ObjectEnvelopeV2Wire<'a> {
    envelope_version: u16,
    magic: &'a str,
    kind: &'a str,
    object_version: u16,
    codec: &'a str,
    compression_level: i32,
    encryption: &'a str,
    encryption_kdf: &'a str,
    kdf_memory_kib: u32,
    kdf_time_cost: u32,
    kdf_parallelism: u32,
    kdf_output_length: u32,
    encryption_salt: &'a [u8],
    encryption_nonce: &'a [u8],
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

struct UnsignedEnvelopeV2<'a> {
    kind: ObjectKind,
    object_version: u16,
    codec: ObjectCodec,
    compression_level: i32,
    encryption: ObjectEncryption,
    encryption_kdf: &'a str,
    kdf_memory_kib: u32,
    kdf_time_cost: u32,
    kdf_parallelism: u32,
    kdf_output_length: u32,
    encryption_salt: &'a [u8],
    encryption_nonce: &'a [u8],
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

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct ObjectEnvelopeV2WireOwned {
    envelope_version: u16,
    magic: String,
    kind: String,
    object_version: u16,
    codec: String,
    compression_level: i32,
    encryption: String,
    encryption_kdf: String,
    kdf_memory_kib: u32,
    kdf_time_cost: u32,
    kdf_parallelism: u32,
    kdf_output_length: u32,
    encryption_salt: Vec<u8>,
    encryption_nonce: Vec<u8>,
    plaintext_length: u64,
    payload_length: u64,
    object_id: Vec<u8>,
    payload_checksum: Vec<u8>,
    payload: Vec<u8>,
    envelope_checksum: Vec<u8>,
}

#[derive(Deserialize)]
struct EnvelopeVersionMarker {
    envelope_version: u16,
}

struct ValidatedV2<'a> {
    kind: ObjectKind,
    object_version: u16,
    codec: ObjectCodec,
    compression_level: CompressionLevel,
    encryption: ObjectEncryption,
    encryption_kdf: &'a str,
    kdf_memory_kib: u32,
    kdf_time_cost: u32,
    kdf_parallelism: u32,
    kdf_output_length: u32,
    encryption_salt: Option<RepositorySalt>,
    encryption_nonce: Option<[u8; XCHACHA20_POLY1305_NONCE_LENGTH]>,
    plaintext_length: usize,
    payload_length: usize,
    object_id: [u8; DIGEST_LENGTH],
    payload: &'a [u8],
}

struct AssociatedDataFields<'a> {
    kind: ObjectKind,
    object_version: u16,
    codec: ObjectCodec,
    compression_level: i32,
    encryption: ObjectEncryption,
    encryption_kdf: &'a str,
    kdf_memory_kib: u32,
    kdf_time_cost: u32,
    kdf_parallelism: u32,
    kdf_output_length: u32,
    encryption_salt: &'a [u8],
    encryption_nonce: &'a [u8],
    plaintext_length: u64,
    payload_length: u64,
    object_id: &'a [u8; DIGEST_LENGTH],
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
    encode_object_envelope_with_options(
        kind,
        object_version,
        ObjectTransformOptions::new(codec, encryption),
        None,
        canonical_plaintext,
    )
}

pub(crate) fn encode_object_envelope_with_options(
    kind: ObjectKind,
    object_version: u16,
    options: ObjectTransformOptions,
    encryption_context: Option<&EncryptionContext>,
    canonical_plaintext: &[u8],
) -> Result<Vec<u8>, FormatError> {
    encode_object_envelope_with_options_and_nonce(
        kind,
        object_version,
        options,
        encryption_context,
        canonical_plaintext,
        None,
    )
}

pub(crate) fn encode_object_envelope_with_encryption(
    kind: ObjectKind,
    object_version: u16,
    options: ObjectTransformOptions,
    encryption_context: &EncryptionContext,
    canonical_plaintext: &[u8],
) -> Result<Vec<u8>, FormatError> {
    encode_object_envelope_with_options(
        kind,
        object_version,
        options,
        Some(encryption_context),
        canonical_plaintext,
    )
}

pub(crate) fn encode_object_envelope_with_password(
    kind: ObjectKind,
    object_version: u16,
    options: ObjectTransformOptions,
    password: &[u8],
    salt: RepositorySalt,
    canonical_plaintext: &[u8],
) -> Result<Vec<u8>, FormatError> {
    if options.encryption() == ObjectEncryption::None {
        return encode_object_envelope_with_options(
            kind,
            object_version,
            options,
            None,
            canonical_plaintext,
        );
    }
    let encryption_context = derive_encryption_context(password, salt)?;
    encode_object_envelope_with_encryption(
        kind,
        object_version,
        options,
        &encryption_context,
        canonical_plaintext,
    )
}

fn encode_object_envelope_with_options_and_nonce(
    kind: ObjectKind,
    object_version: u16,
    options: ObjectTransformOptions,
    encryption_context: Option<&EncryptionContext>,
    canonical_plaintext: &[u8],
    nonce_override: Option<[u8; XCHACHA20_POLY1305_NONCE_LENGTH]>,
) -> Result<Vec<u8>, FormatError> {
    validate_kind_version(kind, object_version)?;
    if canonical_plaintext.len() > MAX_IMMUTABLE_OBJECT_PAYLOAD_BYTES {
        return Err(FormatError::InputTooLarge);
    }

    let codec = options.codec();
    let encryption = options.encryption();
    if codec == ObjectCodec::None && encryption == ObjectEncryption::None {
        return encode_object_envelope_v1(kind, object_version, canonical_plaintext);
    }
    if encryption == ObjectEncryption::XChaCha20Poly1305 && encryption_context.is_none() {
        return Err(FormatError::EncryptionKeyRequired);
    }

    let compression_level = match codec {
        ObjectCodec::None => 0,
        ObjectCodec::Zstd => options.compression_level().value(),
    };
    let object_id = calculate_object_id(kind, object_version, canonical_plaintext);
    let tag_length = match encryption {
        ObjectEncryption::None => 0,
        ObjectEncryption::XChaCha20Poly1305 => XCHACHA20_POLY1305_TAG_LENGTH,
    };
    let max_transformed_payload_length = MAX_IMMUTABLE_OBJECT_STORED_PAYLOAD_BYTES
        .checked_sub(tag_length)
        .ok_or(FormatError::InputTooLarge)?;
    let mut payload = Zeroizing::new(match codec {
        ObjectCodec::None => canonical_plaintext.to_vec(),
        ObjectCodec::Zstd => compress(
            canonical_plaintext,
            compression_level,
            max_transformed_payload_length,
        )?,
    });
    if payload
        .len()
        .checked_add(tag_length)
        .is_none_or(|length| length > MAX_IMMUTABLE_OBJECT_STORED_PAYLOAD_BYTES)
    {
        return Err(FormatError::InputTooLarge);
    }

    let nonce = match encryption {
        ObjectEncryption::None => None,
        ObjectEncryption::XChaCha20Poly1305 => Some(match nonce_override {
            Some(nonce) => nonce,
            None => random_nonce()?,
        }),
    };
    let encryption_salt_value = match encryption {
        ObjectEncryption::None => None,
        ObjectEncryption::XChaCha20Poly1305 => Some(
            encryption_context
                .ok_or(FormatError::EncryptionKeyRequired)?
                .salt(),
        ),
    };
    let encryption_salt = match encryption_salt_value.as_ref() {
        Some(salt) => salt.as_bytes().as_slice(),
        None => &[][..],
    };
    let (encryption_kdf, kdf_memory_kib, kdf_time_cost, kdf_parallelism, kdf_output_length) =
        match encryption {
            ObjectEncryption::None => ("none", 0, 0, 0, 0),
            ObjectEncryption::XChaCha20Poly1305 => (
                REPOSITORY_ENCRYPTION_KDF,
                ARGON2ID_MEMORY_COST_KIB,
                ARGON2ID_TIME_COST,
                ARGON2ID_PARALLELISM,
                u32::try_from(REPOSITORY_ENCRYPTION_KEY_LENGTH)
                    .map_err(|_| FormatError::InvalidTransformMetadata)?,
            ),
        };
    let encryption_nonce = match nonce.as_ref() {
        Some(nonce) => nonce.as_slice(),
        None => &[][..],
    };
    let payload_length = payload
        .len()
        .checked_add(tag_length)
        .and_then(|length| u64::try_from(length).ok())
        .ok_or(FormatError::InputTooLarge)?;
    let plaintext_length =
        u64::try_from(canonical_plaintext.len()).map_err(|_| FormatError::InputTooLarge)?;
    let associated_data = encode_associated_data(&AssociatedDataFields {
        kind,
        object_version,
        codec,
        compression_level,
        encryption,
        encryption_kdf,
        kdf_memory_kib,
        kdf_time_cost,
        kdf_parallelism,
        kdf_output_length,
        encryption_salt,
        encryption_nonce,
        plaintext_length,
        payload_length,
        object_id: object_id.as_digest(),
    })?;
    if encryption == ObjectEncryption::XChaCha20Poly1305 {
        let context = encryption_context.ok_or(FormatError::EncryptionKeyRequired)?;
        let nonce = nonce.as_ref().ok_or(FormatError::InvalidNonce)?;
        encrypt_in_place(context.key(), nonce, &associated_data, &mut payload)?;
    }
    let payload_checksum = Sha256::digest(&payload);
    let unsigned = encode_unsigned_v2(&UnsignedEnvelopeV2 {
        kind,
        object_version,
        codec,
        compression_level,
        encryption,
        encryption_kdf,
        kdf_memory_kib,
        kdf_time_cost,
        kdf_parallelism,
        kdf_output_length,
        encryption_salt,
        encryption_nonce,
        plaintext_length,
        payload_length,
        object_id: object_id.as_digest(),
        payload_checksum: &payload_checksum,
        payload: &payload,
    })?;
    let envelope_checksum = Sha256::digest(&unsigned);
    let bytes = encode_wire(&ObjectEnvelopeV2Wire {
        envelope_version: CURRENT_TRANSFORMED_OBJECT_ENVELOPE_VERSION,
        magic: REPOSITORY_MAGIC,
        kind: kind.as_str(),
        object_version,
        codec: codec.as_str(),
        compression_level,
        encryption: encryption.as_str(),
        encryption_kdf,
        kdf_memory_kib,
        kdf_time_cost,
        kdf_parallelism,
        kdf_output_length,
        encryption_salt,
        encryption_nonce,
        plaintext_length,
        payload_length,
        object_id: object_id.as_digest(),
        payload_checksum: &payload_checksum,
        payload: &payload,
        envelope_checksum: &envelope_checksum,
    })?;
    if bytes.len() > MAX_IMMUTABLE_OBJECT_BYTES {
        return Err(FormatError::InputTooLarge);
    }
    Ok(bytes)
}

fn encode_object_envelope_v1(
    kind: ObjectKind,
    object_version: u16,
    canonical_plaintext: &[u8],
) -> Result<Vec<u8>, FormatError> {
    let object_id = calculate_object_id(kind, object_version, canonical_plaintext);
    let payload_checksum = Sha256::digest(canonical_plaintext);
    let payload_length =
        u64::try_from(canonical_plaintext.len()).map_err(|_| FormatError::InputTooLarge)?;
    let unsigned = encode_unsigned(&UnsignedEnvelope {
        kind,
        object_version,
        codec: ObjectCodec::None,
        encryption: ObjectEncryption::None,
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
        codec: ObjectCodec::None.as_str(),
        encryption: ObjectEncryption::None.as_str(),
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
    decode_object_envelope_with_context(bytes, None)
}

pub(crate) fn decode_object_envelope_with_encryption(
    bytes: &[u8],
    encryption_context: &EncryptionContext,
) -> Result<ImmutableObject, FormatError> {
    decode_object_envelope_with_context(bytes, Some(encryption_context))
}

pub(crate) fn decode_object_envelope_with_password(
    bytes: &[u8],
    password: &[u8],
) -> Result<ImmutableObject, FormatError> {
    let version = decode_envelope_version(bytes)?;
    match version {
        CURRENT_OBJECT_ENVELOPE_VERSION => decode_object_envelope_v1(bytes),
        CURRENT_TRANSFORMED_OBJECT_ENVELOPE_VERSION => {
            let wire = decode_object_envelope_v2_wire(bytes)?;
            let validated = validate_v2_header(&wire)?;
            let context = match validated.encryption {
                ObjectEncryption::None => None,
                ObjectEncryption::XChaCha20Poly1305 => {
                    let salt = validated
                        .encryption_salt
                        .ok_or(FormatError::InvalidTransformMetadata)?;
                    Some(derive_encryption_context(password, salt)?)
                }
            };
            decode_validated_v2(validated, context.as_ref())
        }
        version => Err(FormatError::UnsupportedVersion { version }),
    }
}

pub(crate) fn decode_object_envelope_from_reader<R: Read>(
    reader: &mut R,
) -> Result<ImmutableObject, FormatError> {
    let bytes = read_bounded(reader, MAX_IMMUTABLE_OBJECT_BYTES)?;
    decode_object_envelope(&bytes)
}

pub(crate) fn decode_object_envelope_from_reader_with_encryption<R: Read>(
    reader: &mut R,
    encryption_context: &EncryptionContext,
) -> Result<ImmutableObject, FormatError> {
    let bytes = read_bounded(reader, MAX_IMMUTABLE_OBJECT_BYTES)?;
    decode_object_envelope_with_encryption(&bytes, encryption_context)
}

pub(crate) fn decode_object_envelope_from_reader_with_password<R: Read>(
    reader: &mut R,
    password: &[u8],
) -> Result<ImmutableObject, FormatError> {
    let bytes = read_bounded(reader, MAX_IMMUTABLE_OBJECT_BYTES)?;
    decode_object_envelope_with_password(&bytes, password)
}

fn decode_object_envelope_with_context(
    bytes: &[u8],
    encryption_context: Option<&EncryptionContext>,
) -> Result<ImmutableObject, FormatError> {
    let version = decode_envelope_version(bytes)?;
    match version {
        CURRENT_OBJECT_ENVELOPE_VERSION => decode_object_envelope_v1(bytes),
        CURRENT_TRANSFORMED_OBJECT_ENVELOPE_VERSION => {
            let wire = decode_object_envelope_v2_wire(bytes)?;
            let validated = validate_v2_header(&wire)?;
            decode_validated_v2(validated, encryption_context)
        }
        version => Err(FormatError::UnsupportedVersion { version }),
    }
}

fn decode_envelope_version(bytes: &[u8]) -> Result<u16, FormatError> {
    let marker: EnvelopeVersionMarker = decode_messagepack_with_limits(
        bytes,
        MAX_IMMUTABLE_OBJECT_BYTES,
        MAX_ENVELOPE_STRING_BYTES,
        u32::try_from(MAX_IMMUTABLE_OBJECT_STORED_PAYLOAD_BYTES)
            .map_err(|_| FormatError::InputTooLarge)?,
        MAX_ENVELOPE_COLLECTION_ITEMS,
        MAX_ENVELOPE_DEPTH,
    )?;
    Ok(marker.envelope_version)
}

fn decode_object_envelope_v1(bytes: &[u8]) -> Result<ImmutableObject, FormatError> {
    let wire: ObjectEnvelopeWireOwned = decode_messagepack_with_limits(
        bytes,
        MAX_IMMUTABLE_OBJECT_BYTES,
        MAX_ENVELOPE_STRING_BYTES,
        u32::try_from(MAX_IMMUTABLE_OBJECT_STORED_PAYLOAD_BYTES)
            .map_err(|_| FormatError::InputTooLarge)?,
        MAX_ENVELOPE_COLLECTION_ITEMS,
        MAX_ENVELOPE_DEPTH,
    )?;
    if encode_wire(&wire)? != bytes {
        return Err(FormatError::InvalidEncoding);
    }
    validate_envelope(&wire)
}

fn decode_object_envelope_v2_wire(bytes: &[u8]) -> Result<ObjectEnvelopeV2WireOwned, FormatError> {
    let wire = decode_messagepack_with_limits(
        bytes,
        MAX_IMMUTABLE_OBJECT_BYTES,
        MAX_ENVELOPE_STRING_BYTES,
        u32::try_from(MAX_IMMUTABLE_OBJECT_STORED_PAYLOAD_BYTES)
            .map_err(|_| FormatError::InputTooLarge)?,
        MAX_ENVELOPE_COLLECTION_ITEMS,
        MAX_ENVELOPE_DEPTH,
    )?;
    if encode_wire(&wire)? != bytes {
        return Err(FormatError::InvalidEncoding);
    }
    Ok(wire)
}

fn validate_v2_header<'a>(
    wire: &'a ObjectEnvelopeV2WireOwned,
) -> Result<ValidatedV2<'a>, FormatError> {
    if wire.envelope_version != CURRENT_TRANSFORMED_OBJECT_ENVELOPE_VERSION {
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
    let compression_level = match codec {
        ObjectCodec::None => {
            if wire.compression_level != 0 {
                return Err(FormatError::InvalidCompressionLevel);
            }
            CompressionLevel::DEFAULT
        }
        ObjectCodec::Zstd => CompressionLevel::new(wire.compression_level)
            .map_err(|_| FormatError::InvalidCompressionLevel)?,
    };
    let encryption =
        ObjectEncryption::parse(&wire.encryption).ok_or(FormatError::InvalidEncryption)?;
    let (encryption_salt, encryption_nonce) = match encryption {
        ObjectEncryption::None => {
            if wire.encryption_kdf != "none"
                || wire.kdf_memory_kib != 0
                || wire.kdf_time_cost != 0
                || wire.kdf_parallelism != 0
                || wire.kdf_output_length != 0
                || !wire.encryption_salt.is_empty()
                || !wire.encryption_nonce.is_empty()
            {
                return Err(FormatError::InvalidTransformMetadata);
            }
            (None, None)
        }
        ObjectEncryption::XChaCha20Poly1305 => {
            if wire.encryption_kdf != REPOSITORY_ENCRYPTION_KDF
                || wire.kdf_memory_kib != ARGON2ID_MEMORY_COST_KIB
                || wire.kdf_time_cost != ARGON2ID_TIME_COST
                || wire.kdf_parallelism != ARGON2ID_PARALLELISM
                || wire.kdf_output_length
                    != u32::try_from(REPOSITORY_ENCRYPTION_KEY_LENGTH)
                        .map_err(|_| FormatError::InvalidTransformMetadata)?
            {
                return Err(FormatError::InvalidTransformMetadata);
            }
            let salt = RepositorySalt::from_slice(&wire.encryption_salt)
                .map_err(|_| FormatError::InvalidTransformMetadata)?;
            let nonce: [u8; XCHACHA20_POLY1305_NONCE_LENGTH] = wire
                .encryption_nonce
                .as_slice()
                .try_into()
                .map_err(|_| FormatError::InvalidNonce)?;
            if wire.payload.len() < XCHACHA20_POLY1305_TAG_LENGTH {
                return Err(FormatError::InvalidLength);
            }
            (Some(salt), Some(nonce))
        }
    };
    if wire.payload_length > MAX_IMMUTABLE_OBJECT_STORED_PAYLOAD_BYTES as u64
        || wire.plaintext_length > MAX_IMMUTABLE_OBJECT_PAYLOAD_BYTES as u64
        || wire.payload_length != wire.payload.len() as u64
    {
        return Err(FormatError::InvalidLength);
    }
    let plaintext_length =
        usize::try_from(wire.plaintext_length).map_err(|_| FormatError::InvalidLength)?;
    let payload_length =
        usize::try_from(wire.payload_length).map_err(|_| FormatError::InvalidLength)?;
    let object_id = digest_from_bytes(&wire.object_id)?;
    let payload_checksum = digest_from_bytes(&wire.payload_checksum)?;
    let envelope_checksum = digest_from_bytes(&wire.envelope_checksum)?;
    let unsigned = encode_unsigned_v2(&UnsignedEnvelopeV2 {
        kind,
        object_version: wire.object_version,
        codec,
        compression_level: match codec {
            ObjectCodec::None => 0,
            ObjectCodec::Zstd => compression_level.value(),
        },
        encryption,
        encryption_kdf: &wire.encryption_kdf,
        kdf_memory_kib: wire.kdf_memory_kib,
        kdf_time_cost: wire.kdf_time_cost,
        kdf_parallelism: wire.kdf_parallelism,
        kdf_output_length: wire.kdf_output_length,
        encryption_salt: &wire.encryption_salt,
        encryption_nonce: &wire.encryption_nonce,
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
    Ok(ValidatedV2 {
        kind,
        object_version: wire.object_version,
        codec,
        compression_level,
        encryption,
        encryption_kdf: &wire.encryption_kdf,
        kdf_memory_kib: wire.kdf_memory_kib,
        kdf_time_cost: wire.kdf_time_cost,
        kdf_parallelism: wire.kdf_parallelism,
        kdf_output_length: wire.kdf_output_length,
        encryption_salt,
        encryption_nonce,
        plaintext_length,
        payload_length,
        object_id,
        payload: &wire.payload,
    })
}

fn decode_validated_v2(
    header: ValidatedV2<'_>,
    encryption_context: Option<&EncryptionContext>,
) -> Result<ImmutableObject, FormatError> {
    let mut stored_payload = Zeroizing::new(header.payload.to_vec());
    if header.encryption == ObjectEncryption::XChaCha20Poly1305 {
        let context = encryption_context.ok_or(FormatError::EncryptionKeyRequired)?;
        let salt = header
            .encryption_salt
            .ok_or(FormatError::InvalidTransformMetadata)?;
        if context.salt() != salt {
            return Err(FormatError::EncryptionKeyMismatch);
        }
        let nonce = header.encryption_nonce.ok_or(FormatError::InvalidNonce)?;
        let associated_data = encode_associated_data(&AssociatedDataFields {
            kind: header.kind,
            object_version: header.object_version,
            codec: header.codec,
            compression_level: match header.codec {
                ObjectCodec::None => 0,
                ObjectCodec::Zstd => header.compression_level.value(),
            },
            encryption: header.encryption,
            encryption_kdf: header.encryption_kdf,
            kdf_memory_kib: header.kdf_memory_kib,
            kdf_time_cost: header.kdf_time_cost,
            kdf_parallelism: header.kdf_parallelism,
            kdf_output_length: header.kdf_output_length,
            encryption_salt: salt.as_bytes(),
            encryption_nonce: &nonce,
            plaintext_length: u64::try_from(header.plaintext_length)
                .map_err(|_| FormatError::InvalidLength)?,
            payload_length: u64::try_from(header.payload_length)
                .map_err(|_| FormatError::InvalidLength)?,
            object_id: &header.object_id,
        })?;
        decrypt_in_place(context.key(), &nonce, &associated_data, &mut stored_payload)?;
    }

    let mut canonical_plaintext = match header.codec {
        ObjectCodec::None => Zeroizing::new(std::mem::take(&mut *stored_payload)),
        ObjectCodec::Zstd => {
            let decoded = decompress(&stored_payload, header.plaintext_length);
            stored_payload.zeroize();
            Zeroizing::new(decoded?)
        }
    };
    if canonical_plaintext.len() != header.plaintext_length {
        return Err(FormatError::InvalidLength);
    }
    let expected_id = calculate_object_id(header.kind, header.object_version, &canonical_plaintext);
    if expected_id.as_digest() != &header.object_id {
        return Err(FormatError::InvalidObjectId);
    }
    Ok(ImmutableObject::from_validated_parts(
        ImmutableObjectParts {
            kind: header.kind,
            version: header.object_version,
            codec: header.codec,
            encryption: header.encryption,
            plaintext_length: header.plaintext_length as u64,
            payload_length: header.payload_length as u64,
            object_id: expected_id,
            payload: std::mem::take(&mut *canonical_plaintext),
        },
    ))
}

fn encode_associated_data(fields: &AssociatedDataFields<'_>) -> Result<Vec<u8>, FormatError> {
    let mut associated_data = Vec::with_capacity(192);
    associated_data.extend_from_slice(b"GIB immutable object transform aad\0");
    associated_data.extend_from_slice(&CURRENT_TRANSFORMED_OBJECT_ENVELOPE_VERSION.to_be_bytes());
    append_length_prefixed(&mut associated_data, REPOSITORY_MAGIC.as_bytes())?;
    append_length_prefixed(&mut associated_data, fields.kind.as_str().as_bytes())?;
    associated_data.extend_from_slice(&fields.object_version.to_be_bytes());
    append_length_prefixed(&mut associated_data, fields.codec.as_str().as_bytes())?;
    associated_data.extend_from_slice(&fields.compression_level.to_be_bytes());
    append_length_prefixed(&mut associated_data, fields.encryption.as_str().as_bytes())?;
    append_length_prefixed(&mut associated_data, fields.encryption_kdf.as_bytes())?;
    associated_data.extend_from_slice(&fields.kdf_memory_kib.to_be_bytes());
    associated_data.extend_from_slice(&fields.kdf_time_cost.to_be_bytes());
    associated_data.extend_from_slice(&fields.kdf_parallelism.to_be_bytes());
    associated_data.extend_from_slice(&fields.kdf_output_length.to_be_bytes());
    append_length_prefixed(&mut associated_data, fields.encryption_salt)?;
    append_length_prefixed(&mut associated_data, fields.encryption_nonce)?;
    associated_data.extend_from_slice(&fields.plaintext_length.to_be_bytes());
    associated_data.extend_from_slice(&fields.payload_length.to_be_bytes());
    associated_data.extend_from_slice(fields.object_id);
    Ok(associated_data)
}

fn append_length_prefixed(output: &mut Vec<u8>, value: &[u8]) -> Result<(), FormatError> {
    let length = u32::try_from(value.len()).map_err(|_| FormatError::InvalidLength)?;
    output.extend_from_slice(&length.to_be_bytes());
    output.extend_from_slice(value);
    Ok(())
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

fn encode_unsigned_v2(envelope: &UnsignedEnvelopeV2<'_>) -> Result<Vec<u8>, FormatError> {
    encode_wire(&ObjectEnvelopeV2UnsignedWire {
        envelope_version: CURRENT_TRANSFORMED_OBJECT_ENVELOPE_VERSION,
        magic: REPOSITORY_MAGIC,
        kind: envelope.kind.as_str(),
        object_version: envelope.object_version,
        codec: envelope.codec.as_str(),
        compression_level: envelope.compression_level,
        encryption: envelope.encryption.as_str(),
        encryption_kdf: envelope.encryption_kdf,
        kdf_memory_kib: envelope.kdf_memory_kib,
        kdf_time_cost: envelope.kdf_time_cost,
        kdf_parallelism: envelope.kdf_parallelism,
        kdf_output_length: envelope.kdf_output_length,
        encryption_salt: envelope.encryption_salt,
        encryption_nonce: envelope.encryption_nonce,
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
    let result = (|| {
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
    })();
    buffer.zeroize();
    result
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

    fn encode_owned_v2_fixture(wire: &ObjectEnvelopeV2WireOwned) -> Vec<u8> {
        encode_wire(wire).unwrap_or_default()
    }

    fn decode_owned_v2_fixture(bytes: &[u8]) -> ObjectEnvelopeV2WireOwned {
        rmp_serde::from_slice(bytes).unwrap_or_else(|_| ObjectEnvelopeV2WireOwned {
            envelope_version: 0,
            magic: String::new(),
            kind: String::new(),
            object_version: 0,
            codec: String::new(),
            compression_level: 0,
            encryption: String::new(),
            encryption_kdf: String::new(),
            kdf_memory_kib: 0,
            kdf_time_cost: 0,
            kdf_parallelism: 0,
            kdf_output_length: 0,
            encryption_salt: Vec::new(),
            encryption_nonce: Vec::new(),
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

    fn refresh_v2_checksums(wire: &mut ObjectEnvelopeV2WireOwned, refresh_payload: bool) {
        let Some(kind) = ObjectKind::parse(&wire.kind) else {
            return;
        };
        let Some(codec) = ObjectCodec::parse(&wire.codec) else {
            return;
        };
        let Some(encryption) = ObjectEncryption::parse(&wire.encryption) else {
            return;
        };
        if refresh_payload {
            wire.payload_checksum = Sha256::digest(&wire.payload).to_vec();
        }
        let Ok(unsigned) = encode_unsigned_v2(&UnsignedEnvelopeV2 {
            kind,
            object_version: wire.object_version,
            codec,
            compression_level: wire.compression_level,
            encryption,
            encryption_kdf: &wire.encryption_kdf,
            kdf_memory_kib: wire.kdf_memory_kib,
            kdf_time_cost: wire.kdf_time_cost,
            kdf_parallelism: wire.kdf_parallelism,
            kdf_output_length: wire.kdf_output_length,
            encryption_salt: &wire.encryption_salt,
            encryption_nonce: &wire.encryption_nonce,
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

    fn deterministic_encrypted_object() -> (Vec<u8>, EncryptionContext) {
        let context = derive_encryption_context(
            b"envelope-test-password",
            RepositorySalt::from_bytes([0xabu8; 16]),
        )
        .expect("test key should derive");
        let options =
            ObjectTransformOptions::new(ObjectCodec::Zstd, ObjectEncryption::XChaCha20Poly1305);
        let bytes = encode_object_envelope_with_options_and_nonce(
            ObjectKind::Tree,
            1,
            options,
            Some(&context),
            b"deterministic transformed envelope payload",
            Some([0x11u8; XCHACHA20_POLY1305_NONCE_LENGTH]),
        )
        .expect("test object should encode");
        (bytes, context)
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
    fn unsupported_kind_versions_and_missing_encryption_keys_do_not_fallback() {
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
        let compressed = encode_object_envelope(
            ObjectKind::Tree,
            1,
            ObjectCodec::Zstd,
            ObjectEncryption::None,
            b"payload",
        )
        .expect("zstd is supported by the version-2 decoder");
        assert_eq!(
            decode_object_envelope(&compressed)
                .expect("zstd object should decode")
                .payload(),
            b"payload"
        );
        assert_eq!(
            encode_object_envelope(
                ObjectKind::Tree,
                1,
                ObjectCodec::None,
                ObjectEncryption::XChaCha20Poly1305,
                b"payload",
            ),
            Err(FormatError::EncryptionKeyRequired)
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
    fn transformed_header_payload_and_authentication_mutations_are_rejected() {
        let (valid, context) = deterministic_encrypted_object();
        let decoded = decode_object_envelope_with_encryption(&valid, &context);
        assert!(decoded.is_ok());

        let mut mutated = decode_owned_v2_fixture(&valid);
        mutated.kdf_memory_kib += 1;
        refresh_v2_checksums(&mut mutated, false);
        assert_eq!(
            decode_object_envelope_with_encryption(&encode_owned_v2_fixture(&mutated), &context),
            Err(FormatError::InvalidTransformMetadata)
        );

        let mut mutated = decode_owned_v2_fixture(&valid);
        mutated.encryption_nonce[0] ^= 1;
        refresh_v2_checksums(&mut mutated, false);
        assert_eq!(
            decode_object_envelope_with_encryption(&encode_owned_v2_fixture(&mutated), &context),
            Err(FormatError::AuthenticationFailure)
        );

        let mut mutated = decode_owned_v2_fixture(&valid);
        mutated.payload[0] ^= 1;
        refresh_v2_checksums(&mut mutated, true);
        assert_eq!(
            decode_object_envelope_with_encryption(&encode_owned_v2_fixture(&mutated), &context),
            Err(FormatError::AuthenticationFailure)
        );

        let mut mutated = decode_owned_v2_fixture(&valid);
        mutated.payload_checksum[0] ^= 1;
        refresh_v2_checksums(&mut mutated, false);
        assert_eq!(
            decode_object_envelope_with_encryption(&encode_owned_v2_fixture(&mutated), &context),
            Err(FormatError::InvalidPayloadChecksum)
        );

        let mut mutated = decode_owned_v2_fixture(&valid);
        mutated.payload_length += 1;
        assert_eq!(
            decode_object_envelope_with_encryption(&encode_owned_v2_fixture(&mutated), &context),
            Err(FormatError::InvalidLength)
        );

        let mut mutated = decode_owned_v2_fixture(&valid);
        mutated.kind = "future".to_owned();
        assert_eq!(
            decode_object_envelope_with_encryption(&encode_owned_v2_fixture(&mutated), &context),
            Err(FormatError::InvalidObjectKind)
        );

        let mut mutated = decode_owned_v2_fixture(&valid);
        mutated.object_version = 99;
        assert_eq!(
            decode_object_envelope_with_encryption(&encode_owned_v2_fixture(&mutated), &context),
            Err(FormatError::UnsupportedObjectVersion { version: 99 })
        );

        let mut mutated = decode_owned_v2_fixture(&valid);
        mutated.envelope_version = 99;
        assert_eq!(
            decode_object_envelope(&encode_owned_v2_fixture(&mutated)),
            Err(FormatError::UnsupportedVersion { version: 99 })
        );

        assert_eq!(
            decode_object_envelope(&valid),
            Err(FormatError::EncryptionKeyRequired)
        );
    }

    #[test]
    fn truncated_compressed_stream_is_rejected_after_header_validation() {
        let options = ObjectTransformOptions::new(ObjectCodec::Zstd, ObjectEncryption::None);
        let bytes = encode_object_envelope_with_options(
            ObjectKind::Pack,
            1,
            options,
            None,
            b"compressed stream that must not be accepted when truncated",
        )
        .expect("test object should encode");
        let mut wire = decode_owned_v2_fixture(&bytes);
        wire.payload.truncate(wire.payload.len() / 2);
        wire.payload_length = wire.payload.len() as u64;
        refresh_v2_checksums(&mut wire, true);
        assert_eq!(
            decode_object_envelope(&encode_owned_v2_fixture(&wire)),
            Err(FormatError::DecompressionFailure)
        );
    }

    #[test]
    fn transformed_envelopes_reject_unknown_critical_fields() {
        #[derive(Serialize)]
        struct UnknownV2FieldWire<'a> {
            envelope_version: u16,
            magic: &'a str,
            kind: &'a str,
            object_version: u16,
            codec: &'a str,
            compression_level: i32,
            encryption: &'a str,
            encryption_kdf: &'a str,
            kdf_memory_kib: u32,
            kdf_time_cost: u32,
            kdf_parallelism: u32,
            kdf_output_length: u32,
            encryption_salt: &'a [u8],
            encryption_nonce: &'a [u8],
            plaintext_length: u64,
            payload_length: u64,
            object_id: &'a [u8],
            payload_checksum: &'a [u8],
            payload: &'a [u8],
            envelope_checksum: &'a [u8],
            critical_future_field: &'a str,
        }

        let (bytes, _) = deterministic_encrypted_object();
        let wire = decode_owned_v2_fixture(&bytes);
        let unknown = encode_wire(&UnknownV2FieldWire {
            envelope_version: wire.envelope_version,
            magic: &wire.magic,
            kind: &wire.kind,
            object_version: wire.object_version,
            codec: &wire.codec,
            compression_level: wire.compression_level,
            encryption: &wire.encryption,
            encryption_kdf: &wire.encryption_kdf,
            kdf_memory_kib: wire.kdf_memory_kib,
            kdf_time_cost: wire.kdf_time_cost,
            kdf_parallelism: wire.kdf_parallelism,
            kdf_output_length: wire.kdf_output_length,
            encryption_salt: &wire.encryption_salt,
            encryption_nonce: &wire.encryption_nonce,
            plaintext_length: wire.plaintext_length,
            payload_length: wire.payload_length,
            object_id: &wire.object_id,
            payload_checksum: &wire.payload_checksum,
            payload: &wire.payload,
            envelope_checksum: &wire.envelope_checksum,
            critical_future_field: "reject-me",
        })
        .expect("unknown field fixture should encode");
        assert_eq!(
            decode_object_envelope(&unknown),
            Err(FormatError::InvalidEncoding)
        );
    }

    #[test]
    fn transformed_envelope_has_stable_canonical_bytes() {
        let (bytes, _) = deterministic_encrypted_object();
        assert_eq!(
            hex_encode_bytes(&bytes),
            "de0014b0656e76656c6f70655f76657273696f6e02a56d61676963a3474942a46b696e64a474726565ae6f626a6563745f76657273696f6e01a5636f646563a47a737464b1636f6d7072657373696f6e5f6c6576656c03aa656e6372797074696f6eb27863686163686132302d706f6c7931333035ae656e6372797074696f6e5f6b6466ab6172676f6e3269642d7631ae6b64665f6d656d6f72795f6b6962ce00010000ad6b64665f74696d655f636f737403af6b64665f706172616c6c656c69736d01b16b64665f6f75747075745f6c656e67746820af656e6372797074696f6e5f73616c74c410ababababababababababababababababb0656e6372797074696f6e5f6e6f6e6365c418111111111111111111111111111111111111111111111111b0706c61696e746578745f6c656e6774682aae7061796c6f61645f6c656e67746847a96f626a6563745f6964c420aef9698053b9277c8cf763374e66b0f2b0c3dad807fa6f3a95f20155518ca1f6b07061796c6f61645f636865636b73756dc4205b4e3086f9ce3e5df598108e0136370c068bb1d1683dab242725577fe185b1e1a77061796c6f6164c4471d8ac23d587202d8513bb98e9eab0934533fb27ff8a2b6200d671ee669f9446db8f861f6958206ce822cde9f8e882b507ad5d7764765308328127430780df62b83e1409191fa46b1656e76656c6f70655f636865636b73756dc4200535f3cdf94f6dc61f1f16d50bbf72701e4a04da1b4ca5927911290c6dcbd5e8"
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
