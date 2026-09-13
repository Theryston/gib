//! Versioned immutable path deltas and path checkpoints.
//!
//! A path delta records the path-level difference between one snapshot and its
//! parent as sorted add/modify/delete records. A path checkpoint records the
//! complete path listing of one snapshot. Both are derived data: restore never
//! reads them, and missing or corrupt derived objects are regenerated from the
//! authoritative Merkle trees. Renames are always represented as a delete of
//! the old path plus an add of the new path; there is no rename operation.

use super::{DomainError, RelativePath, RepositoryObject, SnapshotId, TreeNodeKind};
use std::collections::BTreeMap;
use std::fmt;

/// The payload version for path-delta objects.
pub const CURRENT_PATH_DELTA_VERSION: u16 = 1;

/// The payload version for path-checkpoint objects.
pub const CURRENT_PATH_CHECKPOINT_VERSION: u16 = 1;

/// The logical prefix containing immutable path-delta objects.
pub const PATH_DELTAS_PREFIX: &str = "path-deltas";

/// The logical prefix containing immutable path-checkpoint objects.
pub const PATH_CHECKPOINTS_PREFIX: &str = "checkpoints";

/// The snapshot-generation interval between full path checkpoints.
///
/// Every snapshot whose generation is a multiple of this interval publishes an
/// additional checkpoint object, so rebuilding any path state reads at most
/// this many deltas plus one checkpoint. The genesis delta of a parentless
/// snapshot is already a full listing and covers generations before the first
/// checkpoint.
pub const PATH_CHECKPOINT_INTERVAL: u64 = 16;

/// Returns whether a snapshot generation publishes a path checkpoint.
pub fn is_checkpoint_generation(generation: u64) -> bool {
    generation > 0 && generation.is_multiple_of(PATH_CHECKPOINT_INTERVAL)
}

/// One path-level change recorded in a delta or checkpoint.
#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub enum DeltaOperation {
    /// The path is present in the new snapshot but absent in the parent.
    Add,
    /// The path is present in both snapshots with a different node.
    Modify,
    /// The path is present in the parent but absent in the new snapshot.
    ///
    /// Deleting a directory path implicitly deletes every path below it; the
    /// generator never emits records for paths under a deleted directory.
    Delete,
}

impl DeltaOperation {
    /// Returns the stable wire discriminator.
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Add => "add",
            Self::Modify => "modify",
            Self::Delete => "delete",
        }
    }

    /// Parses a wire discriminator.
    pub fn parse(value: &str) -> Option<Self> {
        match value {
            "add" => Some(Self::Add),
            "modify" => Some(Self::Modify),
            "delete" => Some(Self::Delete),
            _ => None,
        }
    }
}

impl fmt::Display for DeltaOperation {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(self.as_str())
    }
}

/// One sorted path record in a delta or checkpoint.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct PathDeltaRecord {
    path: RelativePath,
    operation: DeltaOperation,
    kind: TreeNodeKind,
    size: u64,
}

impl PathDeltaRecord {
    /// Creates a record after validating its size against its kind.
    ///
    /// Only regular files carry a logical size; directories and symbolic
    /// links record zero.
    pub fn new(
        path: RelativePath,
        operation: DeltaOperation,
        kind: TreeNodeKind,
        size: u64,
    ) -> Result<Self, DomainError> {
        if !matches!(kind, TreeNodeKind::RegularFile) && size != 0 {
            return Err(DomainError::InvalidPathDelta {
                reason: "only regular files carry a logical size",
            });
        }
        Ok(Self {
            path,
            operation,
            kind,
            size,
        })
    }

    /// Returns the normalized record path.
    pub fn path(&self) -> &RelativePath {
        &self.path
    }

    /// Returns the recorded operation.
    pub const fn operation(&self) -> DeltaOperation {
        self.operation
    }

    /// Returns the node kind at the recorded path.
    pub const fn kind(&self) -> TreeNodeKind {
        self.kind
    }

    /// Returns the logical file size, or zero for non-files.
    pub const fn size(&self) -> u64 {
        self.size
    }
}

/// An immutable path delta tied to one snapshot and its parent.
///
/// A delta with no parent is a full listing of its snapshot.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct PathDelta {
    snapshot: SnapshotId,
    parent: Option<SnapshotId>,
    generation: u64,
    records: Vec<PathDeltaRecord>,
}

impl PathDelta {
    /// Creates a delta after validating its header and record order.
    pub fn new(
        snapshot: SnapshotId,
        parent: Option<SnapshotId>,
        generation: u64,
        records: Vec<PathDeltaRecord>,
    ) -> Result<Self, DomainError> {
        if generation == 0 {
            return Err(DomainError::InvalidPathDelta {
                reason: "delta generation must be positive",
            });
        }
        validate_records(&records)?;
        Ok(Self {
            snapshot,
            parent,
            generation,
            records,
        })
    }

    /// Returns the snapshot this delta describes.
    pub fn snapshot(&self) -> &SnapshotId {
        &self.snapshot
    }

    /// Returns the parent snapshot this delta was computed against, if any.
    pub fn parent(&self) -> Option<&SnapshotId> {
        self.parent.as_ref()
    }

    /// Returns the snapshot generation this delta was published with.
    pub const fn generation(&self) -> u64 {
        self.generation
    }

    /// Returns whether this delta is a full listing of its snapshot.
    pub const fn is_full(&self) -> bool {
        self.parent.is_none()
    }

    /// Returns the records in ascending path order.
    pub fn records(&self) -> &[PathDeltaRecord] {
        &self.records
    }
}

/// An immutable full path listing published for one checkpoint generation.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct PathCheckpoint {
    snapshot: SnapshotId,
    generation: u64,
    entries: Vec<PathDeltaRecord>,
}

impl PathCheckpoint {
    /// Creates a checkpoint after validating its header and entry order.
    pub fn new(
        snapshot: SnapshotId,
        generation: u64,
        entries: Vec<PathDeltaRecord>,
    ) -> Result<Self, DomainError> {
        if !is_checkpoint_generation(generation) {
            return Err(DomainError::InvalidPathDelta {
                reason: "checkpoint generation must match the checkpoint policy",
            });
        }
        validate_records(&entries)?;
        if entries
            .iter()
            .any(|entry| entry.operation != DeltaOperation::Add)
        {
            return Err(DomainError::InvalidPathDelta {
                reason: "checkpoint entries must all be additions",
            });
        }
        Ok(Self {
            snapshot,
            generation,
            entries,
        })
    }

    /// Returns the snapshot this checkpoint lists.
    pub fn snapshot(&self) -> &SnapshotId {
        &self.snapshot
    }

    /// Returns the checkpoint generation.
    pub const fn generation(&self) -> u64 {
        self.generation
    }

    /// Returns the full listing in ascending path order.
    pub fn entries(&self) -> &[PathDeltaRecord] {
        &self.entries
    }
}

/// Returns the deterministic delta object key for one snapshot.
///
/// Deltas are sharded by the first two snapshot-ID bytes so prefix listings
/// stay narrow; the key remains derivable from the snapshot alone.
pub fn path_delta_key(snapshot: &SnapshotId) -> Result<RepositoryObject, DomainError> {
    let id = snapshot.as_str();
    let shard: String = id.chars().take(2).collect();
    if shard.len() != 2 || !shard.bytes().all(|byte| byte.is_ascii_alphanumeric()) {
        return Err(DomainError::InvalidPathDelta {
            reason: "snapshot ID cannot be sharded into a delta key",
        });
    }
    RepositoryObject::new(format!("{PATH_DELTAS_PREFIX}/{shard}/{id}")).map_err(|_| {
        DomainError::InvalidPathDelta {
            reason: "snapshot ID does not fit a delta key",
        }
    })
}

/// Returns the deterministic checkpoint object key for one snapshot.
pub fn path_checkpoint_key(snapshot: &SnapshotId) -> Result<RepositoryObject, DomainError> {
    RepositoryObject::new(format!("{PATH_CHECKPOINTS_PREFIX}/{}", snapshot.as_str())).map_err(
        |_| DomainError::InvalidPathDelta {
            reason: "snapshot ID does not fit a checkpoint key",
        },
    )
}

/// One live path entry rebuilt from deltas and checkpoints.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct PathEntry {
    kind: TreeNodeKind,
    size: u64,
    last_seen: SnapshotId,
}

impl PathEntry {
    /// Creates a live entry stamped with its snapshot.
    pub const fn new(kind: TreeNodeKind, size: u64, last_seen: SnapshotId) -> Self {
        Self {
            kind,
            size,
            last_seen,
        }
    }

    /// Returns the entry kind.
    pub const fn kind(&self) -> TreeNodeKind {
        self.kind
    }

    /// Returns the logical file size, or zero for non-files.
    pub const fn size(&self) -> u64 {
        self.size
    }

    /// Returns the snapshot that last contained this path.
    pub fn last_seen(&self) -> &SnapshotId {
        &self.last_seen
    }
}

/// Applies delta records to a live path map in record order.
///
/// Deleting a directory path removes every path below it. Callers replay
/// deltas from oldest to newest; each record is stamped with its snapshot.
pub fn apply_path_records(
    entries: &mut BTreeMap<RelativePath, PathEntry>,
    records: &[PathDeltaRecord],
    snapshot: &SnapshotId,
) {
    for record in records {
        match record.operation {
            DeltaOperation::Add | DeltaOperation::Modify => {
                entries.insert(
                    record.path.clone(),
                    PathEntry::new(record.kind, record.size, snapshot.clone()),
                );
            }
            DeltaOperation::Delete => {
                let prefix = format!("{}/", record.path.as_str());
                entries.retain(|path, _| {
                    path.as_str() != record.path.as_str()
                        && !path.as_str().starts_with(prefix.as_str())
                });
            }
        }
    }
}

/// Validates ascending path order, path uniqueness, and the root rule.
///
/// The root path may be added or modified but never deleted; replay starts
/// from an empty map, so deleting it would have no defined meaning.
pub(crate) fn validate_records(records: &[PathDeltaRecord]) -> Result<(), DomainError> {
    let mut previous: Option<&str> = None;
    for record in records {
        let path = record.path.as_str();
        if previous.is_some_and(|previous| path <= previous) {
            return Err(DomainError::InvalidPathDelta {
                reason: "delta records must be sorted without duplicates",
            });
        }
        if record.operation == DeltaOperation::Delete && record.path.is_root() {
            return Err(DomainError::InvalidPathDelta {
                reason: "delta records must never delete the root path",
            });
        }
        previous = Some(path);
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn record(path: &str, operation: DeltaOperation) -> PathDeltaRecord {
        PathDeltaRecord::new(
            RelativePath::new(path).expect("test path"),
            operation,
            TreeNodeKind::RegularFile,
            7,
        )
        .expect("test record")
    }

    #[test]
    fn checkpoint_policy_selects_every_interval_generation() {
        assert!(!is_checkpoint_generation(0));
        assert!(!is_checkpoint_generation(1));
        assert!(!is_checkpoint_generation(15));
        assert!(is_checkpoint_generation(16));
        assert!(is_checkpoint_generation(32));
        assert!(!is_checkpoint_generation(33));
    }

    #[test]
    fn delta_keys_are_deterministic_and_sharded() {
        let snapshot = SnapshotId::new("ab12").expect("test snapshot");
        let key = path_delta_key(&snapshot).expect("delta key");
        assert_eq!(key.as_str(), "path-deltas/ab/ab12");
        let checkpoint = path_checkpoint_key(&snapshot).expect("checkpoint key");
        assert_eq!(checkpoint.as_str(), "checkpoints/ab12");
    }

    #[test]
    fn records_reject_duplicates_and_root_deletes() {
        let snapshot = SnapshotId::new("ab12").expect("test snapshot");
        let duplicate = vec![
            record("a", DeltaOperation::Add),
            record("a", DeltaOperation::Add),
        ];
        assert!(PathDelta::new(snapshot.clone(), None, 1, duplicate).is_err());
        let root_delete = vec![
            PathDeltaRecord::new(
                RelativePath::root(),
                DeltaOperation::Delete,
                TreeNodeKind::Directory,
                0,
            )
            .expect("test record"),
        ];
        assert!(PathDelta::new(snapshot, None, 1, root_delete).is_err());
    }

    #[test]
    fn replay_applies_prefix_deletes_in_any_arrival_batch() {
        let snapshot = SnapshotId::new("ab12").expect("test snapshot");
        let mut entries = BTreeMap::new();
        apply_path_records(
            &mut entries,
            &[
                record("a/b", DeltaOperation::Add),
                record("a/c", DeltaOperation::Add),
            ],
            &snapshot,
        );
        assert_eq!(entries.len(), 2);
        let delete = PathDeltaRecord::new(
            RelativePath::new("a").expect("test path"),
            DeltaOperation::Delete,
            TreeNodeKind::Directory,
            0,
        )
        .expect("test record");
        apply_path_records(&mut entries, &[delete], &snapshot);
        assert!(entries.is_empty());
    }
}
