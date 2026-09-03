//! Public orchestration contracts for bounded backups.

use super::error::{SdkError, SdkResult};
use super::event::{EventDispatcher, Progress};
use super::filesystem::{FilesystemScanner, local_filesystem_scanner};
use super::operation::{
    OperationHandle, OperationId, OperationKind, OperationRequest, OperationResult, OperationStatus,
};
use super::repository::{Repository, RepositoryEncryption};
use crate::application::backup::{
    BackupError, BackupRepositoryFailure, BackupRunRequest, BackupRunResult, run_backup,
};
use crate::application::ports::{Filesystem, FilesystemClock};
use crate::domain::{
    BackupBudgets, BackupMetrics, BackupStage, ChunkingConfiguration, MAX_SNAPSHOT_AUTHOR_LENGTH,
    MAX_SNAPSHOT_MESSAGE_LENGTH, ObjectTransformOptions, PackConfiguration, PackIndexConfiguration,
    SnapshotReference,
};
use std::fmt;
use std::path::PathBuf;
use std::sync::Arc;
use std::thread::{self, JoinHandle};

/// A validated request for one bounded backup.
///
/// The request owns all policy that affects the run. Every inter-stage queue
/// uses [`BackupBudgets::queue_capacity`], while memory, CPU workers, file
/// descriptors, and storage requests are reserved for the work that owns
/// them. A request is therefore safe to use with a source larger than RAM.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct BackupRequest {
    root: PathBuf,
    message: String,
    author: Option<String>,
    created_at: Option<u64>,
    budgets: BackupBudgets,
    chunking: ChunkingConfiguration,
    pack: PackConfiguration,
    index: PackIndexConfiguration,
    transforms: ObjectTransformOptions,
}

impl BackupRequest {
    /// Creates a request for a source directory using the SDK policies.
    pub fn new(root: impl Into<PathBuf>) -> Self {
        Self {
            root: root.into(),
            message: String::from("backup"),
            author: None,
            created_at: None,
            budgets: BackupBudgets::default(),
            chunking: ChunkingConfiguration::default_policy(),
            pack: PackConfiguration::default_policy(),
            index: PackIndexConfiguration::default_policy(),
            transforms: ObjectTransformOptions::new(
                crate::domain::ObjectCodec::None,
                crate::domain::ObjectEncryption::None,
            ),
        }
    }

    /// Replaces the human-readable snapshot message.
    pub fn with_message(mut self, message: impl Into<String>) -> Self {
        self.message = message.into();
        self
    }

    /// Replaces the optional snapshot author.
    pub fn with_author(mut self, author: impl Into<String>) -> Self {
        self.author = Some(author.into());
        self
    }

    /// Removes the optional snapshot author.
    pub fn without_author(mut self) -> Self {
        self.author = None;
        self
    }

    /// Replaces the snapshot creation timestamp in Unix seconds.
    ///
    /// When omitted, the pipeline captures the current wall-clock time at the
    /// publication boundary.
    pub const fn with_created_at(mut self, created_at: u64) -> Self {
        self.created_at = Some(created_at);
        self
    }

    /// Replaces the complete request-level resource policy.
    pub const fn with_budgets(mut self, budgets: BackupBudgets) -> Self {
        self.budgets = budgets;
        self
    }

    /// Replaces the content-defined chunking policy.
    pub const fn with_chunking(mut self, chunking: ChunkingConfiguration) -> Self {
        self.chunking = chunking;
        self
    }

    /// Replaces the immutable pack policy.
    pub const fn with_pack_configuration(mut self, pack: PackConfiguration) -> Self {
        self.pack = pack;
        self
    }

    /// Alias for [`Self::with_pack_configuration`].
    pub const fn with_pack(self, pack: PackConfiguration) -> Self {
        self.with_pack_configuration(pack)
    }

    /// Replaces the pack-index shard policy.
    pub const fn with_index_configuration(mut self, index: PackIndexConfiguration) -> Self {
        self.index = index;
        self
    }

    /// Alias for [`Self::with_index_configuration`].
    pub const fn with_index(self, index: PackIndexConfiguration) -> Self {
        self.with_index_configuration(index)
    }

    /// Replaces compression and encryption policy for immutable objects.
    pub const fn with_transform_options(mut self, transforms: ObjectTransformOptions) -> Self {
        self.transforms = transforms;
        self
    }

    /// Alias for [`Self::with_transform_options`].
    pub const fn with_transforms(self, transforms: ObjectTransformOptions) -> Self {
        self.with_transform_options(transforms)
    }

    /// Returns the source directory.
    pub fn root(&self) -> &std::path::Path {
        &self.root
    }

    /// Returns the snapshot message.
    pub fn message(&self) -> &str {
        &self.message
    }

    /// Returns the optional snapshot author.
    pub fn author(&self) -> Option<&str> {
        self.author.as_deref()
    }

    /// Returns the optional explicit creation timestamp.
    pub const fn created_at(&self) -> Option<u64> {
        self.created_at
    }

    /// Returns the request-level resource policy.
    pub const fn budgets(&self) -> BackupBudgets {
        self.budgets
    }

    /// Returns the content-defined chunking policy.
    pub const fn chunking(&self) -> ChunkingConfiguration {
        self.chunking
    }

    /// Returns the immutable pack policy.
    pub const fn pack(&self) -> PackConfiguration {
        self.pack
    }

    /// Returns the pack-index shard policy.
    pub const fn index(&self) -> PackIndexConfiguration {
        self.index
    }

    /// Returns the immutable-object transform policy.
    pub const fn transform_options(&self) -> ObjectTransformOptions {
        self.transforms
    }

    /// Validates request values before an operation is allocated.
    pub fn validate(&self) -> SdkResult<()> {
        if self.root.as_os_str().is_empty() {
            return Err(SdkError::InvalidRequest {
                field: "backup.root",
                reason: "must not be empty",
            });
        }
        if self.message.len() > MAX_SNAPSHOT_MESSAGE_LENGTH {
            return Err(SdkError::InvalidRequest {
                field: "backup.message",
                reason: "exceeds the snapshot message limit",
            });
        }
        if self
            .author
            .as_ref()
            .is_some_and(|author| author.len() > MAX_SNAPSHOT_AUTHOR_LENGTH)
        {
            return Err(SdkError::InvalidRequest {
                field: "backup.author",
                reason: "exceeds the snapshot author limit",
            });
        }
        Ok(())
    }

    pub(crate) fn into_run_request(self) -> BackupRunRequest {
        BackupRunRequest {
            root: self.root,
            message: self.message,
            author: self.author,
            created_at: self.created_at,
            budgets: self.budgets,
            chunking: self.chunking,
            pack: self.pack,
            index: self.index,
            transforms: self.transforms,
        }
    }
}

/// The successful result of a bounded backup.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct BackupResult {
    snapshot: SnapshotReference,
    metrics: BackupMetrics,
}

impl BackupResult {
    /// Returns the newly published snapshot reference.
    pub const fn snapshot(&self) -> &SnapshotReference {
        &self.snapshot
    }

    /// Alias for [`Self::snapshot`].
    pub const fn snapshot_reference(&self) -> &SnapshotReference {
        self.snapshot()
    }

    /// Returns immutable counters and observed resource maxima.
    pub const fn metrics(&self) -> BackupMetrics {
        self.metrics
    }

    /// Consumes the result and returns its snapshot and metrics.
    pub fn into_parts(self) -> (SnapshotReference, BackupMetrics) {
        (self.snapshot, self.metrics)
    }
}

/// A configured bounded backup orchestrator.
///
/// The orchestrator uses one scan worker, fixed read/hash/transform pools,
/// single ordered tree/pack/index coordinators, and a fixed upload pool. All
/// hand-off channels are synchronous and bounded. Files and chunks are
/// processed in sequence order at the tree boundary; independent uploads may
/// complete in any order, and object publication remains content-addressed.
/// Progress is sent through a separate two-slot coalescing path, so a slow
/// event consumer cannot grow the critical data queues or retain hot-worker
/// buffers indefinitely.
pub struct BackupPipeline<F, C> {
    repository: Repository,
    scanner: FilesystemScanner<F, C>,
    events: EventDispatcher,
    encryption: Option<RepositoryEncryption>,
}

impl<F, C> Clone for BackupPipeline<F, C>
where
    FilesystemScanner<F, C>: Clone,
{
    fn clone(&self) -> Self {
        Self {
            repository: self.repository.clone(),
            scanner: self.scanner.clone(),
            events: self.events.clone(),
            encryption: self.encryption.clone(),
        }
    }
}

impl<F, C> BackupPipeline<F, C>
where
    F: Filesystem + 'static,
    C: FilesystemClock + 'static,
{
    /// Creates a pipeline from a validated repository, scanner, and event
    /// dispatcher.
    pub fn new(
        repository: Repository,
        scanner: FilesystemScanner<F, C>,
        events: EventDispatcher,
    ) -> Self {
        Self {
            repository,
            scanner,
            events,
            encryption: None,
        }
    }

    /// Installs repository encryption material for transformed object writes.
    pub fn with_encryption(mut self, encryption: RepositoryEncryption) -> Self {
        self.encryption = Some(encryption);
        self
    }

    /// Returns the repository used for this pipeline.
    pub const fn repository(&self) -> &Repository {
        &self.repository
    }

    /// Returns the event dispatcher used for operation lifecycle and progress.
    pub fn events(&self) -> EventDispatcher {
        self.events.clone()
    }

    /// Starts a bounded backup operation.
    ///
    /// The returned handle owns the coordinator join operation. Dropping it
    /// requests cancellation and joins the bounded worker set, so no worker
    /// is detached. Call [`BackupHandle::join`] to obtain the result and the
    /// terminal SDK error, if any.
    pub fn start(&self, request: BackupRequest) -> SdkResult<BackupHandle> {
        request.validate()?;
        if self.events.is_closed() {
            return Err(SdkError::EventDispatcherClosed);
        }
        let operation = OperationHandle::start(
            self.events.clone(),
            OperationRequest::new(OperationKind::Backup),
        )?;
        let worker_operation = operation.clone();
        let cancellation = operation.cancellation_handle();
        let progress_operation = operation.clone();
        let progress = Arc::new(move |completed: u64| {
            let _ = progress_operation.report_progress(Progress::indeterminate(completed));
        });
        let storage = self.repository.storage().as_arc();
        let scanner = self.scanner.clone();
        let encryption = self
            .encryption
            .as_ref()
            .map(|encryption| encryption.context().clone());
        let run_request = request.into_run_request();
        let join = thread::Builder::new()
            .name(String::from("gib-backup-coordinator"))
            .spawn(move || {
                let result = run_backup(
                    storage,
                    scanner,
                    run_request,
                    Arc::new(move || cancellation.is_cancelled()),
                    progress,
                    encryption,
                );
                finish_operation(&worker_operation, result)
            })
            .map_err(|_| {
                let error = backup_stage_error(
                    BackupStage::Coordinator,
                    SdkError::InvalidRequest {
                        field: "backup",
                        reason: "coordinator worker could not be started",
                    },
                );
                let _ = operation.fail(error.clone());
                error
            })?;
        Ok(BackupHandle {
            operation,
            join: Some(join),
        })
    }

    /// Runs a bounded backup to completion on the calling thread after
    /// starting its fixed worker set.
    pub fn run(&self, request: BackupRequest) -> SdkResult<BackupResult> {
        self.start(request)?.join()
    }
}

/// A live handle for one bounded backup operation.
pub struct BackupHandle {
    operation: OperationHandle,
    join: Option<JoinHandle<SdkResult<BackupResult>>>,
}

impl BackupHandle {
    /// Returns the underlying operation lifecycle handle.
    pub const fn operation(&self) -> &OperationHandle {
        &self.operation
    }

    /// Returns the operation identifier.
    pub fn id(&self) -> OperationId {
        self.operation.id()
    }

    /// Returns the current lifecycle status.
    pub fn status(&self) -> OperationStatus {
        self.operation.status()
    }

    /// Returns the current lifecycle result metadata.
    pub fn result(&self) -> OperationResult {
        self.operation.result()
    }

    /// Returns a cloneable cancellation source for integrations that cannot
    /// retain the full backup handle.
    pub fn cancellation_handle(&self) -> super::operation::CancellationHandle {
        self.operation.cancellation_handle()
    }

    /// Requests cooperative cancellation and emits the terminal cancellation
    /// event immediately. Worker joins still occur when [`Self::join`] is
    /// called or when this handle is dropped.
    pub fn cancel(&self) -> SdkResult<OperationResult> {
        self.operation.cancel()
    }

    /// Waits for all fixed workers to stop and returns the backup result.
    pub fn join(mut self) -> SdkResult<BackupResult> {
        let Some(join) = self.join.take() else {
            return Err(SdkError::InvalidRequest {
                field: "backup_handle",
                reason: "has already been joined",
            });
        };
        match join.join() {
            Ok(result) => result,
            Err(_) => {
                let error = backup_stage_error(
                    BackupStage::Coordinator,
                    SdkError::InvalidRequest {
                        field: "backup",
                        reason: "a pipeline worker panicked",
                    },
                );
                if self.operation.is_cancelled() {
                    Err(SdkError::OperationCancelled {
                        operation_id: Some(self.id()),
                    })
                } else {
                    let _ = self.operation.fail(error.clone());
                    Err(error)
                }
            }
        }
    }
}

impl fmt::Debug for BackupHandle {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("BackupHandle")
            .field("operation_id", &self.id())
            .field("status", &self.status())
            .finish()
    }
}

impl Drop for BackupHandle {
    fn drop(&mut self) {
        if let Some(join) = self.join.take() {
            let _ = self.operation.cancel();
            let _ = join.join();
        }
    }
}

fn finish_operation(
    operation: &OperationHandle,
    result: Result<BackupRunResult, BackupError>,
) -> SdkResult<BackupResult> {
    if operation.is_cancelled() {
        return Err(SdkError::OperationCancelled {
            operation_id: Some(operation.id()),
        });
    }
    match result {
        Ok(result) => {
            operation.complete()?;
            Ok(BackupResult {
                snapshot: result.snapshot,
                metrics: result.metrics,
            })
        }
        Err(BackupError::Cancelled) => {
            let _ = operation.cancel();
            Err(SdkError::OperationCancelled {
                operation_id: Some(operation.id()),
            })
        }
        Err(error) => {
            let error = map_backup_error(error);
            if operation.is_cancelled() {
                Err(SdkError::OperationCancelled {
                    operation_id: Some(operation.id()),
                })
            } else {
                let _ = operation.fail(error.clone());
                Err(error)
            }
        }
    }
}

fn map_backup_error(error: BackupError) -> SdkError {
    match error {
        BackupError::Cancelled => SdkError::OperationCancelled { operation_id: None },
        BackupError::Budget {
            stage,
            resource,
            requested,
            limit,
        } => SdkError::BackupBudgetExceeded {
            stage,
            resource,
            requested,
            limit,
        },
        BackupError::Filesystem {
            stage,
            operation,
            kind,
            race,
        } => SdkError::BackupFilesystemFailure {
            stage,
            operation,
            kind,
            race,
        },
        BackupError::Storage {
            stage,
            operation,
            error,
        } => SdkError::BackupStorageFailure {
            stage,
            operation,
            error,
        },
        BackupError::Format { stage } => backup_stage_error(stage, format_error(stage)),
        BackupError::Invalid { stage } => backup_stage_error(
            stage,
            SdkError::InvalidRequest {
                field: "backup",
                reason: "the pipeline produced an invalid repository object",
            },
        ),
        BackupError::Repository { stage, failure } => {
            let source = map_repository_failure(failure);
            if matches!(&source, SdkError::OperationCancelled { .. }) {
                source
            } else {
                backup_stage_error(stage, source)
            }
        }
        BackupError::Thread { stage } => backup_stage_error(
            stage,
            SdkError::InvalidRequest {
                field: "backup",
                reason: "a pipeline worker terminated unexpectedly",
            },
        ),
    }
}

fn format_error(stage: BackupStage) -> SdkError {
    match stage {
        BackupStage::Transform => SdkError::RepositoryTransformFailed {
            reason: "backup object transform failed",
        },
        BackupStage::Pack => SdkError::RepositoryPackWriteFailed,
        BackupStage::Index => SdkError::RepositoryPackIndexWriteFailed,
        _ => SdkError::InvalidRequest {
            field: "backup",
            reason: "repository object encoding failed",
        },
    }
}

fn backup_stage_error(stage: BackupStage, source: SdkError) -> SdkError {
    SdkError::BackupStageFailed {
        stage,
        source: Box::new(source),
    }
}

fn map_repository_failure(failure: BackupRepositoryFailure) -> SdkError {
    match failure {
        BackupRepositoryFailure::AlreadyExists => SdkError::RepositoryAlreadyExists,
        BackupRepositoryFailure::Missing => SdkError::RepositoryMissing,
        BackupRepositoryFailure::Malformed => SdkError::RepositoryMalformed {
            reason: "repository object is malformed",
        },
        BackupRepositoryFailure::UnsupportedVersion { version } => {
            SdkError::RepositoryUnsupportedVersion { version }
        }
        BackupRepositoryFailure::Incompatible => SdkError::RepositoryIncompatible {
            reason: "repository is incompatible with this backup pipeline",
        },
        BackupRepositoryFailure::PublicationConflict => SdkError::RepositoryPublicationConflict,
        BackupRepositoryFailure::SnapshotMissing => SdkError::RepositorySnapshotMissing,
        BackupRepositoryFailure::RequiredObjectMissing => SdkError::RepositoryRequiredObjectMissing,
        BackupRepositoryFailure::InvalidPublication => SdkError::InvalidRequest {
            field: "backup.publication",
            reason: "snapshot publication is invalid",
        },
        BackupRepositoryFailure::GenerationExhausted => SdkError::RepositoryGenerationExhausted,
        BackupRepositoryFailure::UnsupportedCapability => SdkError::StorageCapabilityUnsupported,
        BackupRepositoryFailure::Cancelled => SdkError::OperationCancelled { operation_id: None },
        BackupRepositoryFailure::NoSnapshots => SdkError::RepositoryNoSnapshots,
        BackupRepositoryFailure::SnapshotReference => SdkError::SnapshotReferenceMalformed,
        BackupRepositoryFailure::Storage => SdkError::StorageFailure {
            operation: "repository",
        },
    }
}

/// A convenient client entry point that uses the local filesystem scanner.
impl super::client::Client {
    /// Starts a bounded backup using the local filesystem scanner.
    pub fn start_backup(
        &self,
        repository: Repository,
        request: BackupRequest,
    ) -> SdkResult<BackupHandle> {
        BackupPipeline::new(repository, local_filesystem_scanner(), self.events()).start(request)
    }

    /// Runs a bounded backup using the local filesystem scanner.
    pub fn backup(
        &self,
        repository: Repository,
        request: BackupRequest,
    ) -> SdkResult<BackupResult> {
        self.start_backup(repository, request)?.join()
    }
}
