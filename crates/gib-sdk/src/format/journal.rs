use super::envelope::{
    decode_object_envelope, decode_object_envelope_with_encryption,
    encode_object_envelope_with_options,
};
use super::repository::{FormatError, decode_messagepack_with_limits};
use super::transform::EncryptionContext;
use crate::domain::{
    CURRENT_OPERATION_OBJECT_VERSION, ObjectCodec, ObjectEncryption, ObjectKind,
    ObjectTransformOptions, RepositoryIdentity, RepositoryObject, SnapshotReference,
};
use serde::{Deserialize, Serialize};
use std::collections::HashSet;

/// The version of the resumable operation journal payload.
pub const CURRENT_OPERATION_JOURNAL_VERSION: u16 = 1;

/// The largest complete journal object accepted by the journal loader.
pub(crate) const MAX_OPERATION_JOURNAL_BYTES: usize = 8 * 1024 * 1024;

/// The largest unencrypted canonical journal payload accepted by this release.
const MAX_OPERATION_JOURNAL_PAYLOAD_BYTES: usize = MAX_OPERATION_JOURNAL_BYTES - 1_024;

/// The maximum number of immutable-object completion records in one journal.
pub(crate) const MAX_OPERATION_JOURNAL_COMPLETED_OBJECTS: usize = 65_536;

const MAX_JOURNAL_STRING_BYTES: u32 = 4_096;
const MAX_JOURNAL_BINARY_BYTES: u32 = 256;
const MAX_JOURNAL_COLLECTION_ITEMS: u32 = 65_536;
const MAX_JOURNAL_DEPTH: usize = 8;
const MAX_JOURNAL_SOURCE_ROOT_BYTES: usize = 4_096;
const MAX_JOURNAL_OBJECT_KEY_BYTES: usize = 512;
const MAX_JOURNAL_SNAPSHOT_REFERENCE_BYTES: usize = 512;

/// Format data shared by the application journal writer and the public
/// pending-operation projection.
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct OperationJournalData {
    pub(crate) journal_version: u16,
    pub(crate) operation_id: u64,
    pub(crate) repository_format_version: u16,
    pub(crate) repository_identity: String,
    pub(crate) base_generation: u64,
    pub(crate) base_snapshot: Option<String>,
    pub(crate) base_storage_version: Option<Vec<u8>>,
    pub(crate) request_fingerprint: [u8; 32],
    pub(crate) source_fingerprint: [u8; 32],
    pub(crate) source_fingerprint_ready: bool,
    pub(crate) source_root: String,
    pub(crate) checkpoint: OperationJournalCheckpoint,
    pub(crate) completed_objects: Vec<OperationJournalObject>,
    pub(crate) target_snapshot: Option<String>,
    pub(crate) snapshot_created_at: u64,
    pub(crate) created_at: u64,
    pub(crate) updated_at: u64,
    pub(crate) state: OperationJournalState,
}

/// The last durable stage checkpoint in an operation journal.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct OperationJournalCheckpoint {
    pub(crate) stage: u8,
    pub(crate) sequence: u64,
}

/// One immutable object that was durably verified during a backup.
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct OperationJournalObject {
    pub(crate) key: String,
    pub(crate) size: u64,
    pub(crate) digest: [u8; 32],
}

/// Whether an operation journal is still active or has passed publication.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum OperationJournalState {
    Active,
    Complete,
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct OperationJournalWire {
    journal_version: u16,
    operation_id: u64,
    repository_format_version: u16,
    repository_identity: String,
    base_generation: u64,
    base_snapshot: Option<String>,
    base_storage_version: Option<Vec<u8>>,
    request_fingerprint: Vec<u8>,
    source_fingerprint: Vec<u8>,
    source_fingerprint_ready: bool,
    source_root: String,
    checkpoint: OperationJournalCheckpointWire,
    completed_objects: Vec<OperationJournalObjectWire>,
    target_snapshot: Option<String>,
    snapshot_created_at: u64,
    created_at: u64,
    updated_at: u64,
    state: u8,
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct OperationJournalCheckpointWire {
    stage: u8,
    sequence: u64,
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct OperationJournalObjectWire {
    key: String,
    size: u64,
    digest: Vec<u8>,
}

/// Encodes one journal as a checksummed, optionally authenticated object.
pub(crate) fn encode_operation_journal(
    data: &OperationJournalData,
    encryption: Option<&EncryptionContext>,
) -> Result<Vec<u8>, FormatError> {
    validate_data(data)?;
    let wire = to_wire(data);
    let payload = encode_wire(&wire)?;
    if payload.len() > MAX_OPERATION_JOURNAL_PAYLOAD_BYTES {
        return Err(FormatError::InputTooLarge);
    }
    let options = match encryption {
        Some(_) => {
            ObjectTransformOptions::new(ObjectCodec::Zstd, ObjectEncryption::XChaCha20Poly1305)
        }
        None => ObjectTransformOptions::new(ObjectCodec::None, ObjectEncryption::None),
    };
    let bytes = encode_object_envelope_with_options(
        ObjectKind::Operation,
        CURRENT_OPERATION_OBJECT_VERSION,
        options,
        encryption,
        &payload,
    )?;
    if bytes.len() > MAX_OPERATION_JOURNAL_BYTES {
        return Err(FormatError::InputTooLarge);
    }
    Ok(bytes)
}

/// Decodes and validates one bounded journal object.
pub(crate) fn decode_operation_journal(
    bytes: &[u8],
    encryption: Option<&EncryptionContext>,
) -> Result<OperationJournalData, FormatError> {
    if bytes.len() > MAX_OPERATION_JOURNAL_BYTES {
        return Err(FormatError::InputTooLarge);
    }
    let object = match encryption {
        Some(encryption) => decode_object_envelope_with_encryption(bytes, encryption)?,
        None => decode_object_envelope(bytes)?,
    };
    if object.kind() != ObjectKind::Operation {
        return Err(FormatError::InvalidObjectKind);
    }
    if object.version() != CURRENT_OPERATION_OBJECT_VERSION {
        return Err(FormatError::UnsupportedObjectVersion {
            version: object.version(),
        });
    }
    let payload = object.payload();
    if payload.len() > MAX_OPERATION_JOURNAL_PAYLOAD_BYTES {
        return Err(FormatError::InputTooLarge);
    }
    let wire: OperationJournalWire = decode_messagepack_with_limits(
        payload,
        MAX_OPERATION_JOURNAL_PAYLOAD_BYTES,
        MAX_JOURNAL_STRING_BYTES,
        MAX_JOURNAL_BINARY_BYTES,
        MAX_JOURNAL_COLLECTION_ITEMS,
        MAX_JOURNAL_DEPTH,
    )?;
    if encode_wire(&wire)? != payload {
        return Err(FormatError::InvalidEncoding);
    }
    let data = from_wire(wire)?;
    validate_data(&data)?;
    Ok(data)
}

fn to_wire(data: &OperationJournalData) -> OperationJournalWire {
    OperationJournalWire {
        journal_version: data.journal_version,
        operation_id: data.operation_id,
        repository_format_version: data.repository_format_version,
        repository_identity: data.repository_identity.clone(),
        base_generation: data.base_generation,
        base_snapshot: data.base_snapshot.clone(),
        base_storage_version: data.base_storage_version.clone(),
        request_fingerprint: data.request_fingerprint.to_vec(),
        source_fingerprint: data.source_fingerprint.to_vec(),
        source_fingerprint_ready: data.source_fingerprint_ready,
        source_root: data.source_root.clone(),
        checkpoint: OperationJournalCheckpointWire {
            stage: data.checkpoint.stage,
            sequence: data.checkpoint.sequence,
        },
        completed_objects: data
            .completed_objects
            .iter()
            .map(|object| OperationJournalObjectWire {
                key: object.key.clone(),
                size: object.size,
                digest: object.digest.to_vec(),
            })
            .collect(),
        target_snapshot: data.target_snapshot.clone(),
        snapshot_created_at: data.snapshot_created_at,
        created_at: data.created_at,
        updated_at: data.updated_at,
        state: match data.state {
            OperationJournalState::Active => 0,
            OperationJournalState::Complete => 1,
        },
    }
}

fn from_wire(wire: OperationJournalWire) -> Result<OperationJournalData, FormatError> {
    let request_fingerprint = digest_from_vec(wire.request_fingerprint)?;
    let source_fingerprint = digest_from_vec(wire.source_fingerprint)?;
    let state = match wire.state {
        0 => OperationJournalState::Active,
        1 => OperationJournalState::Complete,
        _ => return Err(FormatError::InvalidField),
    };
    let mut completed_objects = Vec::new();
    completed_objects
        .try_reserve(wire.completed_objects.len())
        .map_err(|_| FormatError::InputTooLarge)?;
    for object in wire.completed_objects {
        completed_objects.push(OperationJournalObject {
            key: object.key,
            size: object.size,
            digest: digest_from_vec(object.digest)?,
        });
    }
    Ok(OperationJournalData {
        journal_version: wire.journal_version,
        operation_id: wire.operation_id,
        repository_format_version: wire.repository_format_version,
        repository_identity: wire.repository_identity,
        base_generation: wire.base_generation,
        base_snapshot: wire.base_snapshot,
        base_storage_version: wire.base_storage_version,
        request_fingerprint,
        source_fingerprint,
        source_fingerprint_ready: wire.source_fingerprint_ready,
        source_root: wire.source_root,
        checkpoint: OperationJournalCheckpoint {
            stage: wire.checkpoint.stage,
            sequence: wire.checkpoint.sequence,
        },
        completed_objects,
        target_snapshot: wire.target_snapshot,
        snapshot_created_at: wire.snapshot_created_at,
        created_at: wire.created_at,
        updated_at: wire.updated_at,
        state,
    })
}

fn validate_data(data: &OperationJournalData) -> Result<(), FormatError> {
    if data.journal_version != CURRENT_OPERATION_JOURNAL_VERSION {
        return Err(FormatError::UnsupportedVersion {
            version: data.journal_version,
        });
    }
    if data.operation_id == 0 || data.repository_format_version == 0 {
        return Err(FormatError::InvalidField);
    }
    if RepositoryIdentity::new(data.repository_identity.clone()).is_err() {
        return Err(FormatError::InvalidField);
    }
    if data.source_root.is_empty() {
        return Err(FormatError::InvalidField);
    }
    if data.source_root.len() > MAX_JOURNAL_SOURCE_ROOT_BYTES {
        return Err(FormatError::InputTooLarge);
    }
    if data
        .base_storage_version
        .as_ref()
        .is_some_and(|version| version.is_empty() || version.len() > 256)
    {
        return Err(FormatError::InvalidField);
    }
    for reference in [&data.base_snapshot, &data.target_snapshot] {
        if reference.as_ref().is_some_and(|value| {
            value.is_empty() || value.len() > MAX_JOURNAL_SNAPSHOT_REFERENCE_BYTES
        }) {
            return Err(FormatError::InvalidField);
        }
    }
    if data.completed_objects.len() > MAX_OPERATION_JOURNAL_COMPLETED_OBJECTS {
        return Err(FormatError::InputTooLarge);
    }
    if !data.source_fingerprint_ready
        && (!data.completed_objects.is_empty()
            || data.target_snapshot.is_some()
            || data.state == OperationJournalState::Complete)
    {
        return Err(FormatError::InvalidField);
    }
    if data.state == OperationJournalState::Complete && data.target_snapshot.is_none() {
        return Err(FormatError::InvalidField);
    }
    if data.checkpoint.stage > 10 {
        return Err(FormatError::InvalidField);
    }
    for reference in [&data.base_snapshot, &data.target_snapshot]
        .into_iter()
        .flatten()
    {
        SnapshotReference::new(reference.clone()).map_err(|_| FormatError::InvalidField)?;
    }
    let mut object_keys = HashSet::new();
    object_keys
        .try_reserve(data.completed_objects.len())
        .map_err(|_| FormatError::InputTooLarge)?;
    for object in &data.completed_objects {
        if object.key.len() > MAX_JOURNAL_OBJECT_KEY_BYTES
            || RepositoryObject::new(object.key.clone()).is_err()
        {
            return Err(FormatError::InvalidField);
        }
        if !object_keys.insert(object.key.as_str()) {
            return Err(FormatError::InvalidField);
        }
        let Some((namespace, _)) = object.key.split_once('/') else {
            return Err(FormatError::InvalidField);
        };
        if !matches!(namespace, "snapshots" | "trees" | "packs" | "indexes") {
            return Err(FormatError::InvalidField);
        }
        if object.size == 0 {
            return Err(FormatError::InvalidField);
        }
    }
    Ok(())
}

fn digest_from_vec(value: Vec<u8>) -> Result<[u8; 32], FormatError> {
    value
        .try_into()
        .map_err(|_| FormatError::InvalidDigestLength)
}

fn encode_wire<T: Serialize>(value: &T) -> Result<Vec<u8>, FormatError> {
    let mut bytes = Vec::new();
    let mut serializer = rmp_serde::Serializer::new(&mut bytes)
        .with_struct_map()
        .with_bytes(rmp_serde::config::BytesMode::ForceAll);
    value
        .serialize(&mut serializer)
        .map_err(|_| FormatError::Serialization)?;
    Ok(bytes)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample() -> OperationJournalData {
        OperationJournalData {
            journal_version: CURRENT_OPERATION_JOURNAL_VERSION,
            operation_id: 7,
            repository_format_version: 1,
            repository_identity: String::from("test"),
            base_generation: 2,
            base_snapshot: Some(String::from("snapshots/base")),
            base_storage_version: Some(vec![1, 2, 3]),
            request_fingerprint: [4; 32],
            source_fingerprint: [5; 32],
            source_fingerprint_ready: true,
            source_root: String::from("/source"),
            checkpoint: OperationJournalCheckpoint {
                stage: 8,
                sequence: 12,
            },
            completed_objects: vec![OperationJournalObject {
                key: String::from("packs/abc"),
                size: 3,
                digest: [6; 32],
            }],
            target_snapshot: None,
            snapshot_created_at: 12,
            created_at: 10,
            updated_at: 11,
            state: OperationJournalState::Active,
        }
    }

    #[test]
    fn journal_round_trips_with_a_checksum() {
        let data = sample();
        let bytes = encode_operation_journal(&data, None).expect("journal encodes");
        assert_eq!(decode_operation_journal(&bytes, None), Ok(data));
        let mut corrupted = bytes;
        let last = corrupted.len().saturating_sub(1);
        corrupted[last] ^= 1;
        assert!(decode_operation_journal(&corrupted, None).is_err());
    }

    #[test]
    fn encrypted_journal_requires_and_accepts_its_repository_key() {
        let data = sample();
        let context = super::super::transform::generate_encryption_context(b"test")
            .expect("test encryption context");
        let bytes = encode_operation_journal(&data, Some(&context)).expect("journal encrypts");
        assert_eq!(
            decode_operation_journal(&bytes, None),
            Err(FormatError::EncryptionKeyRequired)
        );
        assert_eq!(decode_operation_journal(&bytes, Some(&context)), Ok(data));
    }

    #[test]
    fn source_fingerprint_must_be_ready_before_completed_objects_exist() {
        let mut data = sample();
        data.source_fingerprint_ready = false;
        assert_eq!(
            encode_operation_journal(&data, None),
            Err(FormatError::InvalidField)
        );
    }

    #[test]
    fn journal_rejects_an_oversized_completion_set_before_encoding() {
        let mut data = sample();
        data.completed_objects = (0..=MAX_OPERATION_JOURNAL_COMPLETED_OBJECTS)
            .map(|index| OperationJournalObject {
                key: format!("packs/{index}"),
                size: 1,
                digest: [0; 32],
            })
            .collect();
        assert_eq!(
            encode_operation_journal(&data, None),
            Err(FormatError::InputTooLarge)
        );
    }

    #[test]
    fn journal_rejects_invalid_identity_root_and_object_references() {
        let mut data = sample();
        data.repository_identity = String::from("../repository");
        assert_eq!(
            encode_operation_journal(&data, None),
            Err(FormatError::InvalidField)
        );

        let mut data = sample();
        data.source_root.clear();
        assert_eq!(
            encode_operation_journal(&data, None),
            Err(FormatError::InvalidField)
        );

        let mut data = sample();
        data.completed_objects[0].key = String::from("packs/../object");
        assert_eq!(
            encode_operation_journal(&data, None),
            Err(FormatError::InvalidField)
        );
    }
}
