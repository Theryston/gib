//! Immutable path deltas and checkpoints as derived repository data.
//!
//! Deltas are computed by comparing Merkle trees, never by rescanning the
//! filesystem. Identical subtrees are pruned by node identity, so an
//! incremental delta loads only tree nodes along the changed frontier. A
//! parentless snapshot produces a full listing, which doubles as the genesis
//! checkpoint. Restore never reads these objects; a missing or corrupt delta
//! is regenerated from the authoritative trees with the same generator.

use super::ports::{ObjectKey, RepositoryStorage, StorageError, read_stream_to_vec};
use super::repository::{RepositoryError, read_snapshot, read_tree_node};
use crate::domain::{
    DeltaOperation, DirectoryNode, EntryName, ObjectId, ObjectKind, PathCheckpoint, PathDelta,
    PathDeltaRecord, PathEntry, RelativePath, RepositoryObject, Snapshot, SnapshotId,
    SnapshotReference, TreeNode, TreeNodeKind, TreeNodeReference, apply_path_records,
    is_checkpoint_generation, path_checkpoint_key,
};
use crate::format::{
    MAX_PATH_DELTA_BYTES, MAX_PATH_DELTA_RECORDS, decode_path_checkpoint, decode_path_delta,
};
use std::collections::BTreeMap;

/// The maximum number of snapshots walked while rebuilding path state.
const MAX_PATH_REBUILD_STEPS: usize = 4_096;

/// The per-record memory estimate charged to pipeline budgets.
const PATH_RECORD_MEMORY_ESTIMATE: usize = 64;

/// A failure while generating, reading, or replaying path deltas.
///
/// The type deliberately contains no backend error or path data.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum PathDeltaError {
    NotFound,
    Malformed,
    TooLarge,
    Unavailable,
    Cancelled,
    Budget { requested: usize, limit: usize },
    Storage { operation: &'static str },
}

/// Path state rebuilt from one checkpoint plus a bounded delta chain.
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct RebuiltPathState {
    /// Live paths in ascending order with their last-seen snapshots.
    pub(crate) entries: BTreeMap<RelativePath, PathEntry>,
    /// The checkpoint the rebuild started from, if one was used.
    pub(crate) checkpoint: Option<SnapshotId>,
    /// The number of delta objects applied, including a full base delta.
    pub(crate) deltas_applied: u64,
}

/// Computes sorted delta records by comparing two Merkle trees.
///
/// Identical subtrees are pruned by node identity without loading children.
/// `reserve` is charged once per emitted record; tree nodes come from `load`.
/// Records for removed subtrees are single prefix deletes without descending.
pub(crate) fn diff_trees(
    load: &mut dyn FnMut(&TreeNodeReference) -> Result<TreeNode, PathDeltaError>,
    parent_root: Option<&TreeNodeReference>,
    new_root: &TreeNodeReference,
    is_cancelled: &dyn Fn() -> bool,
    reserve: &mut dyn FnMut(usize) -> Result<(), PathDeltaError>,
) -> Result<Vec<PathDeltaRecord>, PathDeltaError> {
    let mut records = Vec::new();
    let mut stack = Vec::new();
    push_frame(
        &mut stack,
        DiffFrame {
            parent: parent_root.cloned(),
            current: new_root.clone(),
            path: RelativePath::root(),
        },
    )?;
    while let Some(frame) = stack.pop() {
        if is_cancelled() {
            return Err(PathDeltaError::Cancelled);
        }
        if frame.parent.as_ref() == Some(&frame.current) {
            continue;
        }
        let current = load(&frame.current)?;
        match current {
            TreeNode::Directory(directory) => {
                diff_directory(
                    load,
                    frame.parent.as_ref(),
                    &directory,
                    &frame.path,
                    &mut stack,
                    &mut records,
                    reserve,
                )?;
            }
            TreeNode::RegularFile(file) => {
                diff_leaf(
                    load,
                    frame.parent.as_ref(),
                    &frame.path,
                    TreeNodeKind::RegularFile,
                    file.size(),
                    &mut records,
                    reserve,
                )?;
            }
            TreeNode::SymbolicLink(_) => {
                diff_leaf(
                    load,
                    frame.parent.as_ref(),
                    &frame.path,
                    TreeNodeKind::SymbolicLink,
                    0,
                    &mut records,
                    reserve,
                )?;
            }
        }
    }
    if records.len() > MAX_PATH_DELTA_RECORDS {
        return Err(PathDeltaError::TooLarge);
    }
    records.sort_by(|left, right| left.path().as_str().cmp(right.path().as_str()));
    Ok(records)
}

/// Reads and verifies the delta referenced by one snapshot.
pub(crate) fn read_path_delta(
    storage: &dyn RepositoryStorage,
    snapshot: &Snapshot,
) -> Result<PathDelta, PathDeltaError> {
    let reference = snapshot.path_delta().ok_or(PathDeltaError::Unavailable)?;
    let bytes = read_delta_bytes(storage, reference.as_str())?;
    let delta = decode_path_delta(&bytes).map_err(map_format_error)?;
    if delta.snapshot() != snapshot.id() {
        return Err(PathDeltaError::Malformed);
    }
    Ok(delta)
}

/// Reads and verifies the checkpoint published for one snapshot.
pub(crate) fn read_path_checkpoint(
    storage: &dyn RepositoryStorage,
    snapshot: &SnapshotId,
) -> Result<PathCheckpoint, PathDeltaError> {
    let key = path_checkpoint_key(snapshot).map_err(|_| PathDeltaError::Malformed)?;
    let bytes = read_delta_bytes(storage, key.as_str())?;
    let checkpoint = decode_path_checkpoint(&bytes).map_err(map_format_error)?;
    if checkpoint.snapshot() != snapshot {
        return Err(PathDeltaError::Malformed);
    }
    if !is_checkpoint_generation(checkpoint.generation()) {
        return Err(PathDeltaError::Malformed);
    }
    Ok(checkpoint)
}

/// Regenerates a delta from the authoritative trees.
///
/// The result is byte-identical to the original when the trees are intact,
/// because generation is deterministic over sorted records.
pub(crate) fn regenerate_path_delta(
    storage: &dyn RepositoryStorage,
    snapshot: &Snapshot,
    generation: u64,
    is_cancelled: &dyn Fn() -> bool,
) -> Result<PathDelta, PathDeltaError> {
    let new_root = snapshot
        .root_tree()
        .ok_or(PathDeltaError::Malformed)
        .and_then(|root| directory_reference(root.as_str()))?;
    if load_tree(storage, &new_root)?.as_directory().is_none() {
        return Err(PathDeltaError::Malformed);
    }
    let parent = match snapshot.parent() {
        Some(parent_id) => {
            let reference = parent_id
                .object_reference()
                .map_err(|_| PathDeltaError::Malformed)?;
            let parent_snapshot = read_snapshot(storage, &reference)
                .map_err(|error| map_snapshot_error(error, "regenerate_path_delta"))?;
            let parent_root = parent_snapshot
                .root_tree()
                .ok_or(PathDeltaError::Malformed)
                .and_then(|root| directory_reference(root.as_str()))?;
            Some((parent_id.clone(), parent_root))
        }
        None => None,
    };
    let mut load = |reference: &TreeNodeReference| load_tree(storage, reference);
    let mut reserve = |_: usize| Ok(());
    let records = diff_trees(
        &mut load,
        parent.as_ref().map(|(_, root)| root),
        &new_root,
        is_cancelled,
        &mut reserve,
    )?;
    PathDelta::new(
        snapshot.id().clone(),
        parent.map(|(id, _)| id),
        generation,
        records,
    )
    .map_err(|_| PathDeltaError::Malformed)
}

/// Rebuilds the live path state of one snapshot.
///
/// The walk loads the nearest checkpoint at or below the target and applies
/// at most one checkpoint interval of deltas. Checkpoints are best-effort
/// acceleration: a missing or corrupt checkpoint falls back to the delta
/// chain, while a missing or corrupt delta fails closed.
pub(crate) fn rebuild_path_state(
    storage: &dyn RepositoryStorage,
    target: &SnapshotReference,
    is_cancelled: &dyn Fn() -> bool,
) -> Result<RebuiltPathState, PathDeltaError> {
    let mut snapshot = read_snapshot(storage, target)
        .map_err(|error| map_snapshot_error(error, "rebuild_path_state"))?;
    if snapshot.path_delta().is_none() {
        return Err(PathDeltaError::Unavailable);
    }
    let mut chain = Vec::new();
    let mut checkpoint_base = None;
    for _ in 0..MAX_PATH_REBUILD_STEPS {
        if is_cancelled() {
            return Err(PathDeltaError::Cancelled);
        }
        if let Ok(checkpoint) = read_path_checkpoint(storage, snapshot.id()) {
            checkpoint_base = Some(checkpoint);
            break;
        }
        let delta = read_path_delta(storage, &snapshot)?;
        let is_full = delta.is_full();
        let parent = delta.parent().cloned();
        chain.push(delta);
        if is_full {
            break;
        }
        let parent_id = parent.ok_or(PathDeltaError::Malformed)?;
        let reference = parent_id
            .object_reference()
            .map_err(|_| PathDeltaError::Malformed)?;
        snapshot = read_snapshot(storage, &reference)
            .map_err(|error| map_snapshot_error(error, "rebuild_path_state"))?;
    }
    if chain.len() >= MAX_PATH_REBUILD_STEPS && checkpoint_base.is_none() {
        return Err(PathDeltaError::TooLarge);
    }
    let mut entries = BTreeMap::new();
    let mut expected_parent = None;
    let mut deltas_applied = 0_u64;
    if let Some(checkpoint) = &checkpoint_base {
        for entry in checkpoint.entries() {
            entries.insert(
                entry.path().clone(),
                PathEntry::new(entry.kind(), entry.size(), checkpoint.snapshot().clone()),
            );
        }
        expected_parent = Some(checkpoint.snapshot().clone());
    }
    chain.reverse();
    for delta in &chain {
        if is_cancelled() {
            return Err(PathDeltaError::Cancelled);
        }
        match (&expected_parent, delta.parent()) {
            (None, None) => {}
            (Some(expected), Some(parent)) if expected == parent => {}
            _ => return Err(PathDeltaError::Malformed),
        }
        apply_path_records(&mut entries, delta.records(), delta.snapshot());
        expected_parent = Some(delta.snapshot().clone());
        deltas_applied = deltas_applied.saturating_add(1);
    }
    let target_id = read_snapshot(storage, target)
        .map(|snapshot| snapshot.id().clone())
        .map_err(|error| map_snapshot_error(error, "rebuild_path_state"))?;
    if expected_parent.as_ref() != Some(&target_id) {
        return Err(PathDeltaError::Malformed);
    }
    Ok(RebuiltPathState {
        entries,
        checkpoint: checkpoint_base.map(|checkpoint| checkpoint.snapshot().clone()),
        deltas_applied,
    })
}

struct DiffFrame {
    parent: Option<TreeNodeReference>,
    current: TreeNodeReference,
    path: RelativePath,
}

fn push_frame(stack: &mut Vec<DiffFrame>, frame: DiffFrame) -> Result<(), PathDeltaError> {
    stack.try_reserve(1).map_err(|_| PathDeltaError::TooLarge)?;
    stack.push(frame);
    Ok(())
}

#[allow(clippy::too_many_arguments)]
fn diff_directory(
    load: &mut dyn FnMut(&TreeNodeReference) -> Result<TreeNode, PathDeltaError>,
    parent: Option<&TreeNodeReference>,
    directory: &DirectoryNode,
    path: &RelativePath,
    stack: &mut Vec<DiffFrame>,
    records: &mut Vec<PathDeltaRecord>,
    reserve: &mut dyn FnMut(usize) -> Result<(), PathDeltaError>,
) -> Result<(), PathDeltaError> {
    let Some(parent_reference) = parent else {
        emit_record(
            records,
            reserve,
            RelativePath::clone(path),
            DeltaOperation::Add,
            TreeNodeKind::Directory,
            0,
        )?;
        for entry in directory.entries() {
            push_frame(
                stack,
                DiffFrame {
                    parent: None,
                    current: entry.reference().clone(),
                    path: child_path(path, entry.name().as_str())?,
                },
            )?;
        }
        return Ok(());
    };
    let parent_node = load(parent_reference)?;
    let TreeNode::Directory(parent_directory) = parent_node else {
        emit_record(
            records,
            reserve,
            RelativePath::clone(path),
            DeltaOperation::Modify,
            TreeNodeKind::Directory,
            0,
        )?;
        for entry in directory.entries() {
            push_frame(
                stack,
                DiffFrame {
                    parent: None,
                    current: entry.reference().clone(),
                    path: child_path(path, entry.name().as_str())?,
                },
            )?;
        }
        return Ok(());
    };
    emit_record(
        records,
        reserve,
        RelativePath::clone(path),
        DeltaOperation::Modify,
        TreeNodeKind::Directory,
        0,
    )?;
    let mut parent_entries = parent_directory.entries().iter().peekable();
    for entry in directory.entries() {
        while parent_entries
            .peek()
            .is_some_and(|parent_entry| parent_entry.name().as_str() < entry.name().as_str())
        {
            let removed = parent_entries.next().ok_or(PathDeltaError::Malformed)?;
            emit_record(
                records,
                reserve,
                child_path(path, removed.name().as_str())?,
                DeltaOperation::Delete,
                removed.kind(),
                0,
            )?;
        }
        match parent_entries.peek() {
            Some(parent_entry) if parent_entry.name() == entry.name() => {
                let parent_entry = parent_entries.next().ok_or(PathDeltaError::Malformed)?;
                if parent_entry.reference() != entry.reference() {
                    push_frame(
                        stack,
                        DiffFrame {
                            parent: Some(parent_entry.reference().clone()),
                            current: entry.reference().clone(),
                            path: child_path(path, entry.name().as_str())?,
                        },
                    )?;
                }
            }
            _ => push_frame(
                stack,
                DiffFrame {
                    parent: None,
                    current: entry.reference().clone(),
                    path: child_path(path, entry.name().as_str())?,
                },
            )?,
        }
    }
    for removed in parent_entries {
        emit_record(
            records,
            reserve,
            child_path(path, removed.name().as_str())?,
            DeltaOperation::Delete,
            removed.kind(),
            0,
        )?;
    }
    Ok(())
}

#[allow(clippy::too_many_arguments)]
fn diff_leaf(
    load: &mut dyn FnMut(&TreeNodeReference) -> Result<TreeNode, PathDeltaError>,
    parent: Option<&TreeNodeReference>,
    path: &RelativePath,
    kind: TreeNodeKind,
    size: u64,
    records: &mut Vec<PathDeltaRecord>,
    reserve: &mut dyn FnMut(usize) -> Result<(), PathDeltaError>,
) -> Result<(), PathDeltaError> {
    let Some(parent_reference) = parent else {
        return emit_record(
            records,
            reserve,
            RelativePath::clone(path),
            DeltaOperation::Add,
            kind,
            size,
        );
    };
    let parent_node = load(parent_reference)?;
    if let TreeNode::Directory(parent_directory) = parent_node {
        for entry in parent_directory.entries() {
            emit_record(
                records,
                reserve,
                child_path(path, entry.name().as_str())?,
                DeltaOperation::Delete,
                entry.kind(),
                0,
            )?;
        }
    }
    emit_record(
        records,
        reserve,
        RelativePath::clone(path),
        DeltaOperation::Modify,
        kind,
        size,
    )
}

fn emit_record(
    records: &mut Vec<PathDeltaRecord>,
    reserve: &mut dyn FnMut(usize) -> Result<(), PathDeltaError>,
    path: RelativePath,
    operation: DeltaOperation,
    kind: TreeNodeKind,
    size: u64,
) -> Result<(), PathDeltaError> {
    reserve(
        path.as_str()
            .len()
            .saturating_add(PATH_RECORD_MEMORY_ESTIMATE),
    )?;
    let record =
        PathDeltaRecord::new(path, operation, kind, size).map_err(|_| PathDeltaError::Malformed)?;
    records
        .try_reserve(1)
        .map_err(|_| PathDeltaError::TooLarge)?;
    records.push(record);
    if records.len() > MAX_PATH_DELTA_RECORDS {
        return Err(PathDeltaError::TooLarge);
    }
    Ok(())
}

fn child_path(path: &RelativePath, name: &str) -> Result<RelativePath, PathDeltaError> {
    let component = EntryName::new(name.to_owned()).map_err(|_| PathDeltaError::Malformed)?;
    path.join(&component).map_err(|_| PathDeltaError::Malformed)
}

pub(crate) fn load_tree(
    storage: &dyn RepositoryStorage,
    reference: &TreeNodeReference,
) -> Result<TreeNode, PathDeltaError> {
    read_tree_node(storage, reference).map_err(|error| match error {
        RepositoryError::SnapshotMissing | RepositoryError::RequiredObjectMissing => {
            PathDeltaError::NotFound
        }
        RepositoryError::Malformed { .. }
        | RepositoryError::InvalidPublication { .. }
        | RepositoryError::Incompatible { .. } => PathDeltaError::Malformed,
        RepositoryError::UnsupportedVersion { .. } => PathDeltaError::Malformed,
        RepositoryError::Cancelled => PathDeltaError::Cancelled,
        _ => PathDeltaError::Storage {
            operation: "load_delta_tree",
        },
    })
}

/// Builds a directory reference for a tree root object key.
///
/// Roots are always directories; non-root references keep the kinds carried
/// by their validated parent entries.
fn directory_reference(value: &str) -> Result<TreeNodeReference, PathDeltaError> {
    let object = RepositoryObject::new(value.to_owned()).map_err(|_| PathDeltaError::Malformed)?;
    let (prefix, id) = object
        .as_str()
        .split_once('/')
        .ok_or(PathDeltaError::Malformed)?;
    if prefix != ObjectKind::Tree.storage_prefix() || id.is_empty() {
        return Err(PathDeltaError::Malformed);
    }
    let object_id = ObjectId::from_hex(id.to_owned()).map_err(|_| PathDeltaError::Malformed)?;
    Ok(TreeNodeReference::new(object_id, TreeNodeKind::Directory))
}

fn read_delta_bytes(storage: &dyn RepositoryStorage, key: &str) -> Result<Vec<u8>, PathDeltaError> {
    let object_key = ObjectKey::new(key.to_owned()).map_err(|_| PathDeltaError::Malformed)?;
    let mut object = storage
        .read_stream(&object_key)
        .map_err(|error| map_storage_error(error, "read_path_delta", PathDeltaError::NotFound))?;
    let size = object.metadata().size();
    if size > MAX_PATH_DELTA_BYTES as u64 {
        return Err(PathDeltaError::TooLarge);
    }
    read_stream_to_vec(object.reader(), Some(size)).map_err(|_| PathDeltaError::Storage {
        operation: "read_path_delta",
    })
}

fn map_snapshot_error(error: RepositoryError, operation: &'static str) -> PathDeltaError {
    match error {
        RepositoryError::SnapshotMissing => PathDeltaError::NotFound,
        RepositoryError::RequiredObjectMissing => PathDeltaError::NotFound,
        RepositoryError::Malformed { .. }
        | RepositoryError::InvalidPublication { .. }
        | RepositoryError::Incompatible { .. } => PathDeltaError::Malformed,
        RepositoryError::UnsupportedVersion { .. } => PathDeltaError::Malformed,
        RepositoryError::Cancelled => PathDeltaError::Cancelled,
        _ => PathDeltaError::Storage { operation },
    }
}

fn map_storage_error(
    error: StorageError,
    operation: &'static str,
    missing: PathDeltaError,
) -> PathDeltaError {
    match error {
        StorageError::NotFound => missing,
        StorageError::InvalidObjectKey | StorageError::InvalidVersion => PathDeltaError::Malformed,
        StorageError::Cancelled => PathDeltaError::Cancelled,
        _ => PathDeltaError::Storage { operation },
    }
}

fn map_format_error(error: crate::format::FormatError) -> PathDeltaError {
    match error {
        crate::format::FormatError::UnsupportedVersion { .. }
        | crate::format::FormatError::UnsupportedObjectVersion { .. } => PathDeltaError::Malformed,
        crate::format::FormatError::InputTooLarge => PathDeltaError::TooLarge,
        _ => PathDeltaError::Malformed,
    }
}
