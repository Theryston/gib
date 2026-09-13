//! Public contracts for immutable path deltas and checkpoints.
//!
//! Deltas record the path-level difference between one snapshot and its
//! parent; checkpoints record complete listings every
//! [`PATH_CHECKPOINT_INTERVAL`](crate::PATH_CHECKPOINT_INTERVAL)
//! generations. Both are derived data: restore never reads them, and a
//! missing or corrupt object is regenerated from the authoritative trees.

use super::error::{SdkError, SdkResult};
use super::repository::Repository;
use crate::application::path_delta::{
    PathDeltaError, RebuiltPathState as ApplicationRebuiltPathState,
    read_path_checkpoint as read_path_checkpoint_use_case,
    read_path_delta as read_path_delta_use_case, rebuild_path_state as rebuild_path_state_use_case,
    regenerate_path_delta as regenerate_path_delta_use_case,
};
use crate::application::ports::StorageError;
use crate::application::repository::read_snapshot;
use crate::domain::{
    BackupResource, BackupStage, PathCheckpoint, PathDelta, PathEntry, RelativePath, SnapshotId,
    SnapshotReference, path_delta_key,
};
use crate::format::{FormatError, encode_path_delta};
use std::collections::BTreeMap;

/// Live path state rebuilt from stored deltas and checkpoints.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct RebuiltPathState {
    entries: BTreeMap<RelativePath, PathEntry>,
    checkpoint: Option<SnapshotId>,
    deltas_applied: u64,
}

impl RebuiltPathState {
    /// Returns live paths in ascending order with last-seen snapshots.
    pub fn entries(&self) -> &BTreeMap<RelativePath, PathEntry> {
        &self.entries
    }

    /// Returns the checkpoint the rebuild started from, if one was used.
    pub fn checkpoint(&self) -> Option<&SnapshotId> {
        self.checkpoint.as_ref()
    }

    /// Returns the number of delta objects applied, including a full base.
    pub const fn deltas_applied(&self) -> u64 {
        self.deltas_applied
    }
}

pub(crate) fn rebuilt_from_application(state: ApplicationRebuiltPathState) -> RebuiltPathState {
    RebuiltPathState {
        entries: state.entries,
        checkpoint: state.checkpoint,
        deltas_applied: state.deltas_applied,
    }
}

impl Repository {
    /// Reads and verifies the immutable path delta of one snapshot.
    ///
    /// Fails with [`SdkError::PathDeltaUnavailable`] for snapshots that
    /// predate deltas and with [`SdkError::PathDeltaMalformed`] when the
    /// referenced object is corrupt.
    pub fn read_path_delta(&self, snapshot: &SnapshotReference) -> SdkResult<PathDelta> {
        let header =
            read_snapshot(self.storage().as_storage(), snapshot).map_err(SdkError::from)?;
        read_path_delta_use_case(self.storage().as_storage(), &header).map_err(map_path_delta_error)
    }

    /// Reads and verifies the immutable checkpoint published for one
    /// checkpoint-generation snapshot.
    pub fn read_path_checkpoint(&self, snapshot: &SnapshotReference) -> SdkResult<PathCheckpoint> {
        let header =
            read_snapshot(self.storage().as_storage(), snapshot).map_err(SdkError::from)?;
        read_path_checkpoint_use_case(self.storage().as_storage(), header.id())
            .map_err(map_path_delta_error)
    }

    /// Rebuilds the live path state of one snapshot from the nearest
    /// checkpoint plus a bounded delta chain.
    pub fn rebuild_path_state(&self, snapshot: &SnapshotReference) -> SdkResult<RebuiltPathState> {
        rebuild_path_state_use_case(self.storage().as_storage(), snapshot, &|| false)
            .map_err(map_path_delta_error)
            .map(rebuilt_from_application)
    }

    /// Regenerates a missing path delta from the authoritative trees and
    /// republishes it under its deterministic key.
    ///
    /// The generation must match the snapshot's publication generation, which
    /// callers track alongside the snapshot. Republishing is idempotent: an
    /// already-present object is left untouched.
    pub fn regenerate_path_delta(
        &self,
        snapshot: &SnapshotReference,
        generation: u64,
    ) -> SdkResult<PathDelta> {
        let header =
            read_snapshot(self.storage().as_storage(), snapshot).map_err(SdkError::from)?;
        let delta = regenerate_path_delta_use_case(
            self.storage().as_storage(),
            &header,
            generation,
            &|| false,
        )
        .map_err(map_path_delta_error)?;
        let key = path_delta_key(delta.snapshot())?;
        let bytes = encode_path_delta(&delta).map_err(|error| match error {
            FormatError::InputTooLarge => SdkError::PathDeltaTooLarge,
            _ => SdkError::PathDeltaMalformed,
        })?;
        match self
            .storage()
            .as_storage()
            .create_if_absent(key.as_str(), &bytes)
        {
            Ok(()) | Err(StorageError::AlreadyExists) => Ok(delta),
            Err(StorageError::Cancelled) => {
                Err(SdkError::OperationCancelled { operation_id: None })
            }
            Err(_) => Err(SdkError::StorageFailure {
                operation: "regenerate_path_delta",
            }),
        }
    }
}

pub(crate) fn map_path_delta_error(error: PathDeltaError) -> SdkError {
    match error {
        PathDeltaError::NotFound => SdkError::PathDeltaNotFound,
        PathDeltaError::Malformed => SdkError::PathDeltaMalformed,
        PathDeltaError::TooLarge => SdkError::PathDeltaTooLarge,
        PathDeltaError::Unavailable => SdkError::PathDeltaUnavailable,
        PathDeltaError::Cancelled => SdkError::OperationCancelled { operation_id: None },
        PathDeltaError::Budget { requested, limit } => SdkError::BackupBudgetExceeded {
            stage: BackupStage::Publish,
            resource: BackupResource::Memory,
            requested,
            limit,
        },
        PathDeltaError::Storage { operation } => SdkError::StorageFailure { operation },
    }
}
