//! Versioned encoding for immutable path deltas and path checkpoints.
//!
//! Deltas and checkpoints share one canonical MessagePack payload behind
//! distinct envelope kinds (`path-delta`, `checkpoint`). Like trees and
//! snapshots they are stored without compression or encryption: they derive
//! from already-plain tree objects and carry only paths, kinds, and sizes.

use super::envelope::{decode_object_envelope, encode_object_envelope};
use super::repository::{FormatError, decode_messagepack_with_limits};
use crate::domain::{
    CURRENT_PATH_CHECKPOINT_VERSION, CURRENT_PATH_DELTA_VERSION, DeltaOperation,
    MAX_TREE_PATH_BYTES, ObjectCodec, ObjectEncryption, ObjectKind, PathCheckpoint, PathDelta,
    PathDeltaRecord, RelativePath, SnapshotId, TreeNodeKind,
};
use serde::{Deserialize, Serialize};

/// The largest complete delta or checkpoint object accepted by the loader.
pub(crate) const MAX_PATH_DELTA_BYTES: usize = 8 * 1024 * 1024;

/// The largest unencrypted canonical delta payload accepted by this release.
const MAX_PATH_DELTA_PAYLOAD_BYTES: usize = MAX_PATH_DELTA_BYTES - 1_024;

/// The maximum number of path records in one delta or checkpoint.
pub(crate) const MAX_PATH_DELTA_RECORDS: usize = 1_048_576;

const MAX_DELTA_STRING_BYTES: u32 = MAX_TREE_PATH_BYTES as u32;
const MAX_DELTA_BINARY_BYTES: u32 = 256;
const MAX_DELTA_COLLECTION_ITEMS: u32 = 1_048_576;
const MAX_DELTA_DEPTH: usize = 8;

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct PathDeltaWire {
    format_version: u16,
    snapshot: String,
    parent: Option<String>,
    generation: u64,
    records: Vec<PathDeltaRecordWire>,
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct PathDeltaRecordWire {
    path: String,
    operation: u8,
    kind: u8,
    size: u64,
}

/// Encodes one delta as a checksummed immutable object.
pub(crate) fn encode_path_delta(delta: &PathDelta) -> Result<Vec<u8>, FormatError> {
    let payload = encode_wire(&to_wire(delta)?)?;
    if payload.len() > MAX_PATH_DELTA_PAYLOAD_BYTES {
        return Err(FormatError::InputTooLarge);
    }
    let bytes = encode_object_envelope(
        ObjectKind::PathDelta,
        CURRENT_PATH_DELTA_VERSION,
        ObjectCodec::None,
        ObjectEncryption::None,
        &payload,
    )?;
    if bytes.len() > MAX_PATH_DELTA_BYTES {
        return Err(FormatError::InputTooLarge);
    }
    Ok(bytes)
}

/// Encodes one checkpoint as a checksummed immutable object.
pub(crate) fn encode_path_checkpoint(checkpoint: &PathCheckpoint) -> Result<Vec<u8>, FormatError> {
    let wire = PathDeltaWire {
        format_version: CURRENT_PATH_CHECKPOINT_VERSION,
        snapshot: checkpoint.snapshot().as_str().to_owned(),
        parent: None,
        generation: checkpoint.generation(),
        records: checkpoint
            .entries()
            .iter()
            .map(record_to_wire)
            .collect::<Result<Vec<_>, _>>()?,
    };
    let payload = encode_wire(&wire)?;
    if payload.len() > MAX_PATH_DELTA_PAYLOAD_BYTES {
        return Err(FormatError::InputTooLarge);
    }
    let bytes = encode_object_envelope(
        ObjectKind::Checkpoint,
        CURRENT_PATH_CHECKPOINT_VERSION,
        ObjectCodec::None,
        ObjectEncryption::None,
        &payload,
    )?;
    if bytes.len() > MAX_PATH_DELTA_BYTES {
        return Err(FormatError::InputTooLarge);
    }
    Ok(bytes)
}

/// Decodes and validates one bounded delta object.
pub(crate) fn decode_path_delta(bytes: &[u8]) -> Result<PathDelta, FormatError> {
    let wire = decode_wire(bytes, ObjectKind::PathDelta)?;
    if wire.format_version != CURRENT_PATH_DELTA_VERSION {
        return Err(FormatError::UnsupportedVersion {
            version: wire.format_version,
        });
    }
    if wire.parent.is_none() && wire.generation == 0 {
        return Err(FormatError::InvalidField);
    }
    let parent = wire
        .parent
        .map(SnapshotId::new)
        .transpose()
        .map_err(|_| FormatError::InvalidField)?;
    let snapshot = SnapshotId::new(wire.snapshot).map_err(|_| FormatError::InvalidField)?;
    let records = wire
        .records
        .into_iter()
        .map(record_from_wire)
        .collect::<Result<Vec<_>, _>>()?;
    PathDelta::new(snapshot, parent, wire.generation, records)
        .map_err(|_| FormatError::InvalidField)
}

/// Decodes and validates one bounded checkpoint object.
pub(crate) fn decode_path_checkpoint(bytes: &[u8]) -> Result<PathCheckpoint, FormatError> {
    let wire = decode_wire(bytes, ObjectKind::Checkpoint)?;
    if wire.format_version != CURRENT_PATH_CHECKPOINT_VERSION {
        return Err(FormatError::UnsupportedVersion {
            version: wire.format_version,
        });
    }
    if wire.parent.is_some() {
        return Err(FormatError::InvalidField);
    }
    let snapshot = SnapshotId::new(wire.snapshot).map_err(|_| FormatError::InvalidField)?;
    let entries = wire
        .records
        .into_iter()
        .map(record_from_wire)
        .collect::<Result<Vec<_>, _>>()?;
    if entries
        .iter()
        .any(|entry| entry.operation() != DeltaOperation::Add)
    {
        return Err(FormatError::InvalidField);
    }
    PathCheckpoint::new(snapshot, wire.generation, entries).map_err(|_| FormatError::InvalidField)
}

fn to_wire(delta: &PathDelta) -> Result<PathDeltaWire, FormatError> {
    if delta.records().len() > MAX_PATH_DELTA_RECORDS {
        return Err(FormatError::InputTooLarge);
    }
    Ok(PathDeltaWire {
        format_version: CURRENT_PATH_DELTA_VERSION,
        snapshot: delta.snapshot().as_str().to_owned(),
        parent: delta.parent().map(|parent| parent.as_str().to_owned()),
        generation: delta.generation(),
        records: delta
            .records()
            .iter()
            .map(record_to_wire)
            .collect::<Result<Vec<_>, _>>()?,
    })
}

fn record_to_wire(record: &PathDeltaRecord) -> Result<PathDeltaRecordWire, FormatError> {
    if record.path().as_str().len() > MAX_TREE_PATH_BYTES {
        return Err(FormatError::InputTooLarge);
    }
    Ok(PathDeltaRecordWire {
        path: record.path().as_str().to_owned(),
        operation: match record.operation() {
            DeltaOperation::Add => 0,
            DeltaOperation::Modify => 1,
            DeltaOperation::Delete => 2,
        },
        kind: match record.kind() {
            TreeNodeKind::Directory => 0,
            TreeNodeKind::RegularFile => 1,
            TreeNodeKind::SymbolicLink => 2,
        },
        size: record.size(),
    })
}

fn record_from_wire(record: PathDeltaRecordWire) -> Result<PathDeltaRecord, FormatError> {
    if record.path.len() > MAX_TREE_PATH_BYTES {
        return Err(FormatError::InputTooLarge);
    }
    let operation = match record.operation {
        0 => DeltaOperation::Add,
        1 => DeltaOperation::Modify,
        2 => DeltaOperation::Delete,
        _ => return Err(FormatError::InvalidField),
    };
    let kind = match record.kind {
        0 => TreeNodeKind::Directory,
        1 => TreeNodeKind::RegularFile,
        2 => TreeNodeKind::SymbolicLink,
        _ => return Err(FormatError::InvalidField),
    };
    let path = RelativePath::new(record.path).map_err(|_| FormatError::InvalidField)?;
    PathDeltaRecord::new(path, operation, kind, record.size).map_err(|_| FormatError::InvalidField)
}

fn decode_wire(bytes: &[u8], kind: ObjectKind) -> Result<PathDeltaWire, FormatError> {
    if bytes.len() > MAX_PATH_DELTA_BYTES {
        return Err(FormatError::InputTooLarge);
    }
    let object = decode_object_envelope(bytes)?;
    if object.kind() != kind {
        return Err(FormatError::InvalidObjectKind);
    }
    if object.version() != kind.current_version() {
        return Err(FormatError::UnsupportedObjectVersion {
            version: object.version(),
        });
    }
    let payload = object.payload();
    if payload.len() > MAX_PATH_DELTA_PAYLOAD_BYTES {
        return Err(FormatError::InputTooLarge);
    }
    let wire: PathDeltaWire = decode_messagepack_with_limits(
        payload,
        MAX_PATH_DELTA_PAYLOAD_BYTES,
        MAX_DELTA_STRING_BYTES,
        MAX_DELTA_BINARY_BYTES,
        MAX_DELTA_COLLECTION_ITEMS,
        MAX_DELTA_DEPTH,
    )?;
    if wire.records.len() > MAX_PATH_DELTA_RECORDS {
        return Err(FormatError::InputTooLarge);
    }
    if encode_wire(&wire)? != payload {
        return Err(FormatError::InvalidEncoding);
    }
    Ok(wire)
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

    fn sample_delta() -> PathDelta {
        PathDelta::new(
            SnapshotId::new("aa12").expect("test snapshot"),
            Some(SnapshotId::new("bb34").expect("test parent")),
            2,
            vec![
                PathDeltaRecord::new(
                    RelativePath::new("a").expect("test path"),
                    DeltaOperation::Add,
                    TreeNodeKind::Directory,
                    0,
                )
                .expect("test record"),
                PathDeltaRecord::new(
                    RelativePath::new("a/b").expect("test path"),
                    DeltaOperation::Modify,
                    TreeNodeKind::RegularFile,
                    9,
                )
                .expect("test record"),
                PathDeltaRecord::new(
                    RelativePath::new("gone").expect("test path"),
                    DeltaOperation::Delete,
                    TreeNodeKind::SymbolicLink,
                    0,
                )
                .expect("test record"),
            ],
        )
        .expect("test delta")
    }

    #[test]
    fn delta_round_trips_with_a_checksum() {
        let delta = sample_delta();
        let bytes = encode_path_delta(&delta).expect("delta encodes");
        assert_eq!(decode_path_delta(&bytes), Ok(delta));
        let mut corrupted = bytes;
        let last = corrupted.len().saturating_sub(1);
        corrupted[last] ^= 1;
        assert!(decode_path_delta(&corrupted).is_err());
    }

    #[test]
    fn checkpoint_round_trips_without_a_parent() {
        let checkpoint = PathCheckpoint::new(
            SnapshotId::new("aa12").expect("test snapshot"),
            16,
            vec![
                PathDeltaRecord::new(
                    RelativePath::root(),
                    DeltaOperation::Add,
                    TreeNodeKind::Directory,
                    0,
                )
                .expect("test record"),
            ],
        )
        .expect("test checkpoint");
        let bytes = encode_path_checkpoint(&checkpoint).expect("checkpoint encodes");
        assert_eq!(decode_path_checkpoint(&bytes), Ok(checkpoint));
        assert!(decode_path_delta(&bytes).is_err());
    }

    #[test]
    fn decoder_rejects_oversized_and_unknown_records() {
        let mut delta = sample_delta();
        delta = PathDelta::new(
            SnapshotId::new("aa12").expect("test snapshot"),
            Some(SnapshotId::new("bb34").expect("test parent")),
            2,
            delta.records().to_vec(),
        )
        .expect("test delta");
        let _ = delta;
        let oversized = PathDeltaRecordWire {
            path: "x".repeat(MAX_TREE_PATH_BYTES + 1),
            operation: 0,
            kind: 1,
            size: 1,
        };
        assert!(record_from_wire(oversized).is_err());
        let unknown = PathDeltaRecordWire {
            path: String::from("a"),
            operation: 9,
            kind: 1,
            size: 1,
        };
        assert!(record_from_wire(unknown).is_err());
    }
}
