//! Public orchestration contracts for bounded backups.

use super::error::{SdkError, SdkResult};
use super::event::{EventDispatcher, Progress};
use super::filesystem::{FilesystemScanner, local_filesystem_scanner};
use super::operation::{
    OperationHandle, OperationId, OperationKind, OperationRequest, OperationResult, OperationStatus,
};
use super::repository::{HeadState, Repository, RepositoryEncryption};
use crate::application::backup::{
    BackupError, BackupRepositoryFailure, BackupRunRequest, BackupRunResult, run_backup,
};
use crate::application::journal::{
    JournalError, LoadedOperationJournal, PendingOperationInfo as ApplicationPendingOperationInfo,
    PendingOperationPage as ApplicationPendingOperationPage,
    PendingOperationStatus as ApplicationPendingOperationStatus, list_pending_operations,
    load_journal, new_journal_identifier,
};
use crate::application::ports::{
    DEFAULT_OBJECT_LIST_PAGE_SIZE, Filesystem, FilesystemClock, MAX_OBJECT_LIST_PAGE_SIZE,
    ObjectCursor, ObjectListRequest, ObjectPrefix,
};
use crate::domain::{
    BackupBudgets, BackupDeduplicationConfiguration, BackupMetrics, BackupReference, BackupStage,
    ChunkingConfiguration, MAX_SNAPSHOT_AUTHOR_LENGTH, MAX_SNAPSHOT_MESSAGE_LENGTH,
    ObjectTransformOptions, PackConfiguration, PackIndexConfiguration, SnapshotReference,
    SnapshotSelector,
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
    deduplication: BackupDeduplicationConfiguration,
    transforms: ObjectTransformOptions,
    parent: Option<SnapshotSelector>,
    parent_disabled: bool,
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
            deduplication: BackupDeduplicationConfiguration::default_policy(),
            transforms: ObjectTransformOptions::new(
                crate::domain::ObjectCodec::None,
                crate::domain::ObjectEncryption::None,
            ),
            parent: None,
            parent_disabled: false,
        }
    }

    /// Selects a snapshot as the immutable parent used to derive this backup.
    ///
    /// The selector is resolved against repository state when the operation
    /// starts. Use [`Self::with_parent_latest`] for the `latest` alias or
    /// [`Self::with_parent_reference`] when the selector is still text.
    pub fn with_parent(mut self, parent: impl Into<BackupReference>) -> Self {
        let parent = parent.into();
        self.parent = Some(parent);
        self.parent_disabled = false;
        self
    }

    /// Selects repository HEAD as the parent at operation start.
    pub fn with_parent_latest(self) -> Self {
        self.with_parent(SnapshotSelector::latest())
    }

    /// Parses and selects a full snapshot ID, unique prefix, or `latest` as
    /// the immutable parent.
    pub fn with_parent_reference(self, reference: impl AsRef<str>) -> SdkResult<Self> {
        let selector =
            SnapshotSelector::parse(reference.as_ref().to_owned()).map_err(SdkError::from)?;
        Ok(self.with_parent(selector))
    }

    /// Explicitly requests a parentless snapshot.
    ///
    /// This is distinct from the default automatic HEAD behavior retained for
    /// compatibility with existing callers.
    pub fn without_parent(mut self) -> Self {
        self.parent = None;
        self.parent_disabled = true;
        self
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

    /// Replaces the bounded content-reuse policy.
    pub const fn with_deduplication_configuration(
        mut self,
        deduplication: BackupDeduplicationConfiguration,
    ) -> Self {
        self.deduplication = deduplication;
        self
    }

    /// Alias for [`Self::with_deduplication_configuration`].
    pub const fn with_deduplication(self, deduplication: BackupDeduplicationConfiguration) -> Self {
        self.with_deduplication_configuration(deduplication)
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

    /// Returns the bounded content-reuse policy.
    pub const fn deduplication(&self) -> BackupDeduplicationConfiguration {
        self.deduplication
    }

    /// Returns the immutable-object transform policy.
    pub const fn transform_options(&self) -> ObjectTransformOptions {
        self.transforms
    }

    /// Returns the explicitly selected parent selector, if one was supplied.
    pub fn parent(&self) -> Option<&BackupReference> {
        self.parent.as_ref()
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

    pub(crate) fn into_run_request(
        self,
        operation_id: u64,
        journal_key: String,
        repository: &Repository,
        resume_journal: Option<LoadedOperationJournal>,
    ) -> BackupRunRequest {
        BackupRunRequest {
            operation_id,
            journal_key,
            repository_format_version: repository.format_version(),
            repository_identity: repository.identity().as_str().to_owned(),
            resume_journal,
            root: self.root,
            message: self.message,
            author: self.author,
            created_at: self.created_at,
            budgets: self.budgets,
            chunking: self.chunking,
            pack: self.pack,
            index: self.index,
            deduplication: self.deduplication,
            transforms: self.transforms,
            parent: self.parent,
            parent_disabled: self.parent_disabled,
        }
    }
}

/// The parse state shown for one pending operation journal.
#[non_exhaustive]
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub enum PendingOperationStatus {
    /// The journal is valid and can be considered for resumption.
    Active,
    /// The journal failed integrity or structural validation.
    Corrupt,
    /// The journal version is newer than this SDK understands.
    UnsupportedVersion,
    /// The journal is authenticated and needs repository encryption material.
    Encrypted,
    /// The journal exceeded a bounded loader or record limit.
    TooLarge,
}

impl PendingOperationStatus {
    /// Returns the stable display value for this status.
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Active => "active",
            Self::Corrupt => "corrupt",
            Self::UnsupportedVersion => "unsupported_version",
            Self::Encrypted => "encrypted",
            Self::TooLarge => "too_large",
        }
    }
}

impl fmt::Display for PendingOperationStatus {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(self.as_str())
    }
}

/// A bounded request for pending backup journals.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct PendingOperationListRequest {
    limit: usize,
    after: Option<ObjectCursor>,
}

impl PendingOperationListRequest {
    /// Creates a request using the default bounded page size.
    pub const fn new() -> Self {
        Self {
            limit: DEFAULT_OBJECT_LIST_PAGE_SIZE,
            after: None,
        }
    }

    /// Sets the maximum number of journal rows to inspect in one page.
    pub const fn with_limit(mut self, limit: usize) -> Self {
        self.limit = limit;
        self
    }

    /// Sets the opaque cursor returned by the previous page.
    pub fn with_cursor(mut self, cursor: ObjectCursor) -> Self {
        self.after = Some(cursor);
        self
    }

    /// Returns the requested page size.
    pub const fn limit(&self) -> usize {
        self.limit
    }

    /// Returns the exclusive continuation cursor, when present.
    pub const fn cursor(&self) -> Option<&ObjectCursor> {
        self.after.as_ref()
    }

    pub(crate) fn into_application(self) -> SdkResult<ObjectListRequest> {
        if !(1..=MAX_OBJECT_LIST_PAGE_SIZE).contains(&self.limit) {
            return Err(SdkError::InvalidRequest {
                field: "pending_operations.limit",
                reason: "must be within the supported object-list page limit",
            });
        }
        let prefix = ObjectPrefix::new("operations").map_err(|_| SdkError::InvalidRequest {
            field: "pending_operations.prefix",
            reason: "the operation journal prefix is invalid",
        })?;
        let request = ObjectListRequest::new(prefix).with_limit(self.limit);
        Ok(match self.after {
            Some(cursor) => request.with_cursor(cursor),
            None => request,
        })
    }
}

impl Default for PendingOperationListRequest {
    fn default() -> Self {
        Self::new()
    }
}

/// Safe metadata projected from one operation journal.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct PendingOperation {
    identifier: String,
    operation_id: Option<OperationId>,
    status: PendingOperationStatus,
    object_size: u64,
    repository_format_version: Option<u16>,
    base_generation: Option<u64>,
    base_snapshot: Option<SnapshotReference>,
    source_root: Option<PathBuf>,
    checkpoint: Option<BackupStage>,
    checkpoint_sequence: Option<u64>,
    completed_objects: usize,
    created_at: Option<u64>,
    updated_at: Option<u64>,
    target_snapshot: Option<SnapshotReference>,
}

impl PendingOperation {
    /// Returns the stable journal identifier accepted by resume APIs.
    pub fn identifier(&self) -> &str {
        &self.identifier
    }

    /// Returns the persisted operation number, when the journal is valid.
    pub const fn operation_id(&self) -> Option<OperationId> {
        self.operation_id
    }

    /// Returns the parse status.
    pub const fn status(&self) -> PendingOperationStatus {
        self.status
    }

    /// Returns the stored journal object size.
    pub const fn object_size(&self) -> u64 {
        self.object_size
    }

    /// Returns the repository format version captured by the journal.
    pub const fn repository_format_version(&self) -> Option<u16> {
        self.repository_format_version
    }

    /// Returns the HEAD generation captured before the backup started.
    pub const fn base_generation(&self) -> Option<u64> {
        self.base_generation
    }

    /// Returns the captured base snapshot, when one existed.
    pub fn base_snapshot(&self) -> Option<&SnapshotReference> {
        self.base_snapshot.as_ref()
    }

    /// Returns the source root captured by the journal.
    pub fn source_root(&self) -> Option<&std::path::Path> {
        self.source_root.as_deref()
    }

    /// Returns the last durable pipeline stage.
    pub const fn checkpoint(&self) -> Option<BackupStage> {
        self.checkpoint
    }

    /// Returns the monotonic checkpoint sequence.
    pub const fn checkpoint_sequence(&self) -> Option<u64> {
        self.checkpoint_sequence
    }

    /// Returns the number of immutable object completion records.
    pub const fn completed_objects(&self) -> usize {
        self.completed_objects
    }

    /// Returns the journal creation timestamp in Unix seconds.
    pub const fn created_at(&self) -> Option<u64> {
        self.created_at
    }

    /// Returns the journal update timestamp in Unix seconds.
    pub const fn updated_at(&self) -> Option<u64> {
        self.updated_at
    }

    /// Returns the target snapshot, when publication preparation reached it.
    pub fn target_snapshot(&self) -> Option<&SnapshotReference> {
        self.target_snapshot.as_ref()
    }
}

/// One bounded page of pending backup journals.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct PendingOperationPage {
    operations: Vec<PendingOperation>,
    next_cursor: Option<ObjectCursor>,
}

impl PendingOperationPage {
    /// Returns the rows in lexical journal-key order.
    pub fn operations(&self) -> &[PendingOperation] {
        &self.operations
    }

    /// Returns the cursor for the next bounded page.
    pub const fn next_cursor(&self) -> Option<&ObjectCursor> {
        self.next_cursor.as_ref()
    }

    /// Consumes the page into its rows and cursor.
    pub fn into_parts(self) -> (Vec<PendingOperation>, Option<ObjectCursor>) {
        (self.operations, self.next_cursor)
    }
}

pub(crate) fn pending_page_from_application(
    page: ApplicationPendingOperationPage,
) -> SdkResult<PendingOperationPage> {
    let mut operations = Vec::new();
    operations
        .try_reserve(page.operations.len())
        .map_err(|_| SdkError::OperationJournalTooLarge)?;
    for operation in page.operations {
        operations.push(pending_operation_from_application(operation)?);
    }
    Ok(PendingOperationPage {
        operations,
        next_cursor: page.next_cursor,
    })
}

fn pending_operation_from_application(
    operation: ApplicationPendingOperationInfo,
) -> SdkResult<PendingOperation> {
    let status = map_pending_status(operation.status);
    let Some(operation_id) = operation.operation_id else {
        return Ok(PendingOperation {
            identifier: operation.identifier,
            operation_id: None,
            status,
            object_size: operation.object_size,
            repository_format_version: operation.repository_format_version,
            base_generation: operation.base_generation,
            base_snapshot: None,
            source_root: operation.source_root.map(PathBuf::from),
            checkpoint: None,
            checkpoint_sequence: None,
            completed_objects: operation.completed_objects,
            created_at: operation.created_at,
            updated_at: operation.updated_at,
            target_snapshot: None,
        });
    };
    let operation_id =
        OperationId::from_u64(operation_id).ok_or(SdkError::OperationJournalMalformed)?;
    let base_snapshot = operation
        .base_snapshot
        .as_deref()
        .map(SnapshotReference::new)
        .transpose()
        .map_err(|_| SdkError::OperationJournalMalformed)?;
    let target_snapshot = operation
        .target_snapshot
        .as_deref()
        .map(SnapshotReference::new)
        .transpose()
        .map_err(|_| SdkError::OperationJournalMalformed)?;
    let checkpoint_data = operation
        .checkpoint
        .ok_or(SdkError::OperationJournalMalformed)?;
    let checkpoint =
        backup_stage_from_code(checkpoint_data.stage).ok_or(SdkError::OperationJournalMalformed)?;
    Ok(PendingOperation {
        identifier: operation.identifier,
        operation_id: Some(operation_id),
        status,
        object_size: operation.object_size,
        repository_format_version: operation.repository_format_version,
        base_generation: operation.base_generation,
        base_snapshot,
        source_root: operation.source_root.map(PathBuf::from),
        checkpoint: Some(checkpoint),
        checkpoint_sequence: Some(checkpoint_data.sequence),
        completed_objects: operation.completed_objects,
        created_at: operation.created_at,
        updated_at: operation.updated_at,
        target_snapshot,
    })
}

fn map_pending_status(status: ApplicationPendingOperationStatus) -> PendingOperationStatus {
    match status {
        ApplicationPendingOperationStatus::Active => PendingOperationStatus::Active,
        ApplicationPendingOperationStatus::Corrupt => PendingOperationStatus::Corrupt,
        ApplicationPendingOperationStatus::UnsupportedVersion => {
            PendingOperationStatus::UnsupportedVersion
        }
        ApplicationPendingOperationStatus::Encrypted => PendingOperationStatus::Encrypted,
        ApplicationPendingOperationStatus::TooLarge => PendingOperationStatus::TooLarge,
    }
}

fn backup_stage_from_code(value: u8) -> Option<BackupStage> {
    match value {
        0 => Some(BackupStage::Scan),
        1 => Some(BackupStage::Read),
        2 => Some(BackupStage::Chunk),
        3 => Some(BackupStage::Hash),
        4 => Some(BackupStage::Dedup),
        5 => Some(BackupStage::Transform),
        6 => Some(BackupStage::Pack),
        7 => Some(BackupStage::Index),
        8 => Some(BackupStage::Upload),
        9 => Some(BackupStage::Publish),
        10 => Some(BackupStage::Coordinator),
        _ => None,
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
        let journal_key = match new_journal_identifier(operation.id().as_u64()) {
            Ok(key) => key,
            Err(error) => {
                let error = map_journal_error(error);
                let _ = operation.fail(error.clone());
                return Err(error);
            }
        };
        let pending_identifier = pending_identifier(&journal_key);
        let run_request =
            request.into_run_request(operation.id().as_u64(), journal_key, &self.repository, None);
        self.spawn_run(operation, run_request, pending_identifier)
    }

    /// Resumes a compatible interrupted backup identified by its journal ID.
    ///
    /// The journal is loaded and authenticated before a worker is started. A
    /// new lifecycle operation is used for the resumed invocation, while the
    /// persisted operation identity remains the one recorded in the journal.
    pub fn resume(
        &self,
        identifier: impl AsRef<str>,
        request: BackupRequest,
    ) -> SdkResult<BackupHandle> {
        request.validate()?;
        if self.events.is_closed() {
            return Err(SdkError::EventDispatcherClosed);
        }
        let storage = self.repository.storage().as_arc();
        let encryption = self
            .encryption
            .as_ref()
            .map(|encryption| encryption.context().clone());
        let loaded = load_journal(Arc::clone(&storage), identifier.as_ref(), encryption)
            .map_err(map_journal_error)?;
        let pending_identifier = pending_identifier(&loaded.key);
        let operation = OperationHandle::start(
            self.events.clone(),
            OperationRequest::new(OperationKind::Backup),
        )?;
        let run_request = request.into_run_request(
            loaded.data.operation_id,
            loaded.key.clone(),
            &self.repository,
            Some(loaded),
        );
        self.spawn_run(operation, run_request, pending_identifier)
    }

    /// Alias for [`Self::resume`] using the command-line terminology.
    pub fn continue_backup(
        &self,
        identifier: impl AsRef<str>,
        request: BackupRequest,
    ) -> SdkResult<BackupHandle> {
        self.resume(identifier, request)
    }

    /// Lists active operation journals using a bounded prefix listing.
    pub fn list_pending_operations(
        &self,
        request: impl Into<PendingOperationListRequest>,
    ) -> SdkResult<PendingOperationPage> {
        let request = request.into().into_application()?;
        let encryption = self
            .encryption
            .as_ref()
            .map(|encryption| encryption.context());
        list_pending_operations(self.repository.storage().as_storage(), &request, encryption)
            .map_err(map_journal_error)
            .and_then(pending_page_from_application)
    }

    fn spawn_run(
        &self,
        operation: OperationHandle,
        run_request: BackupRunRequest,
        pending_identifier: String,
    ) -> SdkResult<BackupHandle> {
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
            pending_identifier,
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
    pending_identifier: String,
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

    /// Returns the journal identifier used to resume this operation.
    pub fn pending_identifier(&self) -> &str {
        &self.pending_identifier
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
        BackupError::Journal { stage, error } => {
            let source = map_journal_error(error);
            if matches!(source, SdkError::OperationCancelled { .. }) {
                source
            } else {
                backup_stage_error(stage, source)
            }
        }
        BackupError::PublicationConflict {
            stage,
            expected,
            current,
        } => backup_stage_error(
            stage,
            SdkError::RepositoryPublicationConflictContext {
                expected: Box::new(HeadState::from_application_read(*expected)),
                current: current.map(|head| Box::new(HeadState::from_application_read(*head))),
            },
        ),
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
        BackupRepositoryFailure::SnapshotReferenceEmpty => SdkError::SnapshotReferenceEmpty,
        BackupRepositoryFailure::SnapshotReferenceMalformed => SdkError::SnapshotReferenceMalformed,
        BackupRepositoryFailure::SnapshotReferenceNotFound => SdkError::SnapshotReferenceNotFound,
        BackupRepositoryFailure::SnapshotReferenceAmbiguous => SdkError::SnapshotReferenceAmbiguous,
        BackupRepositoryFailure::Storage => SdkError::StorageFailure {
            operation: "repository",
        },
    }
}

pub(crate) fn map_journal_error(error: JournalError) -> SdkError {
    match error {
        JournalError::NotFound => SdkError::OperationJournalNotFound,
        JournalError::AlreadyExists => SdkError::OperationJournalAlreadyExists,
        JournalError::Malformed => SdkError::OperationJournalMalformed,
        JournalError::UnsupportedVersion { version } => {
            SdkError::OperationJournalUnsupportedVersion { version }
        }
        JournalError::Incompatible => SdkError::OperationJournalIncompatible {
            reason: "journal metadata or storage capabilities are incompatible",
        },
        JournalError::Stale => SdkError::OperationJournalStale {
            reason: "repository HEAD no longer matches the journal base",
        },
        JournalError::AlreadyPublished => SdkError::OperationJournalAlreadyPublished,
        JournalError::SourceChanged => SdkError::OperationJournalSourceChanged,
        JournalError::TooLarge => SdkError::OperationJournalTooLarge,
        JournalError::EncryptionKeyRequired => SdkError::OperationJournalEncryptionRequired,
        JournalError::Cancelled => SdkError::OperationCancelled { operation_id: None },
        JournalError::Storage { operation } => {
            SdkError::OperationJournalStorageFailure { operation }
        }
    }
}

fn pending_identifier(key: &str) -> String {
    key.strip_prefix("operations/").unwrap_or(key).to_owned()
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

    /// Resumes a bounded backup using the local filesystem scanner.
    pub fn resume_backup(
        &self,
        repository: Repository,
        identifier: impl AsRef<str>,
        request: BackupRequest,
    ) -> SdkResult<BackupHandle> {
        BackupPipeline::new(repository, local_filesystem_scanner(), self.events())
            .resume(identifier, request)
    }

    /// Continues a bounded backup using the local filesystem scanner.
    pub fn continue_backup(
        &self,
        repository: Repository,
        identifier: impl AsRef<str>,
        request: BackupRequest,
    ) -> SdkResult<BackupHandle> {
        self.resume_backup(repository, identifier, request)
    }

    /// Lists pending backup journals using the local client event context.
    pub fn list_pending_operations(
        &self,
        repository: Repository,
        request: impl Into<PendingOperationListRequest>,
    ) -> SdkResult<PendingOperationPage> {
        BackupPipeline::new(repository, local_filesystem_scanner(), self.events())
            .list_pending_operations(request)
    }
}
