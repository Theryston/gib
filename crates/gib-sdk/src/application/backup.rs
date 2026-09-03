use super::filesystem::FilesystemScanner;
use super::ports::{ObjectKey, ObjectWriteOptions, RepositoryStorage, StorageError};
use super::repository::{self, HeadRead, RepositoryError};
use crate::domain::{
    BackupBudgets, BackupMetrics, BackupResource, BackupStage, CURRENT_OBJECT_ENVELOPE_VERSION,
    CURRENT_PACK_OBJECT_VERSION, CURRENT_TRANSFORMED_OBJECT_ENVELOPE_VERSION, ChunkId,
    ChunkingConfiguration, CompressionLevel, DirectoryNode, EntryName, FileChunkReference,
    FilesystemEntry, FilesystemEntryKind, FilesystemErrorKind, FilesystemMetadata,
    FilesystemOperation, MAX_FILESYSTEM_SCAN_OPEN_DIRECTORIES, MAX_IMMUTABLE_OBJECT_BYTES,
    MAX_IMMUTABLE_OBJECT_STORED_PAYLOAD_BYTES, ObjectCodec, ObjectEncryption, ObjectId, ObjectKind,
    ObjectTransformOptions, PACK_ALIGNMENT, PACK_ENTRY_HEADER_LENGTH, PACK_FOOTER_LENGTH,
    PACK_HEADER_LENGTH, PACK_INDEX_RECORD_LENGTH, PackConfiguration, PackEntryInput,
    PackIndexConfiguration, PackIndexEntry, PackIndexShardId, PackIndexTransform, PortableMetadata,
    RegularFileNode, RepositoryHead, SealedPack, SealedPackIndexShard, Snapshot, SnapshotId,
    SnapshotPublication, SnapshotReference, SymbolicLinkNode, TreeEntry, TreeNode,
    TreeNodeReference,
};
use crate::format::{
    EncryptionContext, PackBuilder as FormatPackBuilder,
    PackIndexShardBuilder as FormatPackIndexShardBuilder, encode_object_envelope_with_options,
    encode_snapshot, encode_tree_node_with_id,
};
use sha2::{Digest, Sha256};
use std::collections::BTreeMap;
use std::fs::{self, File, OpenOptions};
use std::io::{self, Cursor, Read, Write};
use std::panic::{AssertUnwindSafe, catch_unwind};
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};
use std::sync::mpsc::{
    Receiver, RecvTimeoutError, SyncSender, TryRecvError, TrySendError, sync_channel,
};
use std::sync::{Arc, Condvar, Mutex};
use std::thread::{self, JoinHandle};
use std::time::Duration;

const CHANNEL_WAIT: Duration = Duration::from_millis(5);
const PROGRESS_REPORT_CAPACITY: usize = 2;
const SCAN_ENTRY_OVERHEAD: usize = 512;
const CHUNK_MESSAGE_OVERHEAD: usize = 256;
const CHUNK_BUFFER_SAFETY_MULTIPLIER: usize = 2;
const DIRECTORY_MEMORY_OVERHEAD: usize = 256;
const TREE_NODE_MEMORY_OVERHEAD: usize = 4 * 1024;
const TRANSFORM_MEMORY_OVERHEAD: usize = 16 * 1024;
const INDEX_SPOOL_RECORD_BYTES: usize = PACK_INDEX_RECORD_LENGTH;
const INDEX_MEMORY_SAFETY_MULTIPLIER: usize = 4;
const INDEX_SPOOL_DIRECTORY_ATTEMPTS: usize = 16;

static NEXT_INDEX_SPOOL_ID: AtomicU64 = AtomicU64::new(1);

/// The application-owned request passed by the public API after validation.
pub(crate) struct BackupRunRequest {
    pub(crate) root: PathBuf,
    pub(crate) message: String,
    pub(crate) author: Option<String>,
    pub(crate) created_at: Option<u64>,
    pub(crate) budgets: BackupBudgets,
    pub(crate) chunking: ChunkingConfiguration,
    pub(crate) pack: PackConfiguration,
    pub(crate) index: PackIndexConfiguration,
    pub(crate) transforms: ObjectTransformOptions,
}

/// The application result returned after HEAD publication succeeds.
pub(crate) struct BackupRunResult {
    pub(crate) snapshot: SnapshotReference,
    pub(crate) metrics: BackupMetrics,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum BackupRepositoryFailure {
    AlreadyExists,
    Missing,
    Malformed,
    UnsupportedVersion { version: u16 },
    Incompatible,
    PublicationConflict,
    SnapshotMissing,
    RequiredObjectMissing,
    InvalidPublication,
    GenerationExhausted,
    UnsupportedCapability,
    Cancelled,
    NoSnapshots,
    SnapshotReference,
    Storage,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum BackupError {
    Cancelled,
    Budget {
        stage: BackupStage,
        resource: BackupResource,
        requested: usize,
        limit: usize,
    },
    Filesystem {
        stage: BackupStage,
        operation: Option<FilesystemOperation>,
        kind: Option<FilesystemErrorKind>,
        race: bool,
    },
    Storage {
        stage: BackupStage,
        operation: &'static str,
        error: StorageError,
    },
    Format {
        stage: BackupStage,
    },
    Invalid {
        stage: BackupStage,
    },
    Repository {
        stage: BackupStage,
        failure: BackupRepositoryFailure,
    },
    Thread {
        stage: BackupStage,
    },
}

impl BackupError {
    const fn is_cancelled(self) -> bool {
        matches!(self, Self::Cancelled)
    }
}

/// Runs one complete bounded backup pipeline.
pub(crate) fn run_backup<F, C>(
    storage: Arc<dyn RepositoryStorage>,
    scanner: FilesystemScanner<F, C>,
    request: BackupRunRequest,
    is_cancelled: Arc<dyn Fn() -> bool + Send + Sync>,
    progress: Arc<dyn Fn(u64) + Send + Sync>,
    encryption: Option<EncryptionContext>,
) -> Result<BackupRunResult, BackupError>
where
    F: super::ports::Filesystem + 'static,
    C: super::ports::FilesystemClock + 'static,
{
    validate_request(&request, encryption.as_ref())?;
    let plan = WorkerPlan::from_request(request.budgets);
    let control = PipelineControl::new(is_cancelled);
    control.check()?;

    let memory = Arc::new(ResourceBudget::new(request.budgets.memory_bytes()));
    let cpu_workers = Arc::new(ResourceBudget::new(request.budgets.cpu_workers()));
    let file_descriptors = Arc::new(ResourceBudget::new(request.budgets.file_descriptors()));
    let network_requests = Arc::new(ResourceBudget::new(request.budgets.network_requests()));
    let metrics = Arc::new(MetricsState::default());
    let progress = ProgressReporter::new(request.budgets.queue_capacity(), progress);

    let head = match read_current_head(
        storage.as_ref(),
        &network_requests,
        &file_descriptors,
        &memory,
        &cpu_workers,
        &control,
    ) {
        Ok(head) => head,
        Err(error) => {
            progress.close();
            progress.join();
            return Err(error);
        }
    };

    let (scan_tx, scan_rx) = sync_channel(request.budgets.queue_capacity());
    let (read_tx, read_rx) = sync_channel(request.budgets.queue_capacity());
    let (hash_tx, hash_rx) = sync_channel(request.budgets.queue_capacity());
    let (tree_tx, tree_rx) = sync_channel(request.budgets.queue_capacity());
    let (pack_tx, pack_rx) = sync_channel(request.budgets.queue_capacity());
    let (upload_tx, upload_rx) = sync_channel(request.budgets.queue_capacity());
    let (index_tx, index_rx) = sync_channel(request.budgets.queue_capacity());
    let (tree_result_tx, tree_result_rx) = sync_channel(1);

    let scan_rx = Arc::new(Mutex::new(scan_rx));
    let read_rx = Arc::new(Mutex::new(read_rx));
    let hash_rx = Arc::new(Mutex::new(hash_rx));
    let tree_rx = Arc::new(Mutex::new(tree_rx));
    let pack_rx = Arc::new(Mutex::new(pack_rx));
    let upload_rx = Arc::new(Mutex::new(upload_rx));
    let index_rx = Arc::new(Mutex::new(index_rx));

    let mut handles = Vec::new();
    spawn_stage(
        &mut handles,
        "gib-backup-index",
        BackupStage::Index,
        {
            let control = control.clone();
            let index_rx = Arc::clone(&index_rx);
            let upload_tx = upload_tx.clone();
            let cpu_workers = Arc::clone(&cpu_workers);
            let file_descriptors = Arc::clone(&file_descriptors);
            let memory = Arc::clone(&memory);
            let metrics = Arc::clone(&metrics);
            let progress = progress.clone();
            let index = request.index;
            move || {
                run_index_worker(
                    index_rx,
                    upload_tx,
                    cpu_workers,
                    file_descriptors,
                    memory,
                    control,
                    metrics,
                    progress,
                    index,
                );
            }
        },
        &control,
    )?;

    for worker_id in 0..plan.network_workers {
        let control = control.clone();
        let upload_rx = Arc::clone(&upload_rx);
        let index_tx = index_tx.clone();
        let storage = Arc::clone(&storage);
        let network_requests = Arc::clone(&network_requests);
        let file_descriptors = Arc::clone(&file_descriptors);
        let metrics = Arc::clone(&metrics);
        let progress = progress.clone();
        let worker = UploadWorker {
            receiver: upload_rx,
            index_sender: index_tx,
            uploader: ImmutableObjectUploader {
                storage,
                network_requests,
                file_descriptors,
                control: control.clone(),
            },
            metrics,
            progress,
        };
        spawn_stage(
            &mut handles,
            &format!("gib-backup-upload-{worker_id}"),
            BackupStage::Upload,
            move || run_upload_worker(worker),
            &control,
        )?;
    }

    {
        let control = control.clone();
        let worker_control = control.clone();
        let tree_rx = Arc::clone(&tree_rx);
        let pack_tx = pack_tx.clone();
        let upload_tx = upload_tx.clone();
        let memory = Arc::clone(&memory);
        let cpu_workers = Arc::clone(&cpu_workers);
        let metrics = Arc::clone(&metrics);
        let progress = progress.clone();
        let chunking = request.chunking;
        spawn_stage(
            &mut handles,
            "gib-backup-tree",
            BackupStage::Pack,
            move || {
                run_tree_worker(
                    tree_rx,
                    pack_tx,
                    upload_tx,
                    tree_result_tx,
                    memory,
                    cpu_workers,
                    worker_control,
                    metrics,
                    progress,
                    chunking,
                );
            },
            &control,
        )?;
    }

    {
        let control = control.clone();
        let worker_control = control.clone();
        let pack_rx = Arc::clone(&pack_rx);
        let upload_tx = upload_tx.clone();
        let index_tx = index_tx.clone();
        let memory = Arc::clone(&memory);
        let cpu_workers = Arc::clone(&cpu_workers);
        let metrics = Arc::clone(&metrics);
        let progress = progress.clone();
        let pack = request.pack;
        let transforms = request.transforms;
        spawn_stage(
            &mut handles,
            "gib-backup-pack",
            BackupStage::Pack,
            move || {
                run_pack_worker(
                    pack_rx,
                    upload_tx,
                    index_tx,
                    memory,
                    cpu_workers,
                    worker_control,
                    metrics,
                    progress,
                    pack,
                    transforms,
                );
            },
            &control,
        )?;
    }

    let transform_workers = plan.transform_workers;
    for worker_id in 0..transform_workers {
        let control = control.clone();
        let worker_control = control.clone();
        let hash_rx = Arc::clone(&hash_rx);
        let tree_tx = tree_tx.clone();
        let memory = Arc::clone(&memory);
        let cpu_workers = Arc::clone(&cpu_workers);
        let metrics = Arc::clone(&metrics);
        let progress = progress.clone();
        let transforms = request.transforms;
        let encryption = encryption.clone();
        spawn_stage(
            &mut handles,
            &format!("gib-backup-transform-{worker_id}"),
            BackupStage::Transform,
            move || {
                run_transform_worker(
                    hash_rx,
                    tree_tx,
                    memory,
                    cpu_workers,
                    worker_control,
                    metrics,
                    progress,
                    transforms,
                    encryption,
                );
            },
            &control,
        )?;
    }

    for worker_id in 0..plan.hash_workers {
        let control = control.clone();
        let worker_control = control.clone();
        let read_rx = Arc::clone(&read_rx);
        let hash_tx = hash_tx.clone();
        let cpu_workers = Arc::clone(&cpu_workers);
        let metrics = Arc::clone(&metrics);
        let progress = progress.clone();
        spawn_stage(
            &mut handles,
            &format!("gib-backup-hash-{worker_id}"),
            BackupStage::Hash,
            move || {
                run_hash_worker(
                    read_rx,
                    hash_tx,
                    cpu_workers,
                    worker_control,
                    metrics,
                    progress,
                );
            },
            &control,
        )?;
    }

    for worker_id in 0..plan.read_workers {
        let control = control.clone();
        let worker_control = control.clone();
        let scan_rx = Arc::clone(&scan_rx);
        let read_sender = read_tx.clone();
        let scanner = scanner.clone();
        let root = request.root.clone();
        let file_descriptors = Arc::clone(&file_descriptors);
        let metrics = Arc::clone(&metrics);
        let progress = progress.clone();
        let memory = Arc::clone(&memory);
        let cpu_workers = Arc::clone(&cpu_workers);
        let chunking = request.chunking;
        spawn_stage(
            &mut handles,
            &format!("gib-backup-read-{worker_id}"),
            BackupStage::Read,
            move || {
                run_read_worker(
                    scan_rx,
                    read_sender,
                    scanner,
                    root,
                    file_descriptors,
                    memory,
                    cpu_workers,
                    worker_control,
                    metrics,
                    progress,
                    chunking,
                );
            },
            &control,
        )?;
    }

    {
        let control = control.clone();
        let worker_control = control.clone();
        let scanner = scanner.clone().with_options(
            scanner
                .options()
                .with_max_open_directories(plan.scanner_directories),
        );
        let root = request.root.clone();
        let file_descriptors = Arc::clone(&file_descriptors);
        let memory = Arc::clone(&memory);
        let metrics = Arc::clone(&metrics);
        let progress = progress.clone();
        spawn_stage(
            &mut handles,
            "gib-backup-scan",
            BackupStage::Scan,
            move || {
                run_scan_worker(
                    scanner,
                    root,
                    scan_tx,
                    file_descriptors,
                    memory,
                    worker_control,
                    metrics,
                    progress,
                    plan.scanner_directories,
                );
            },
            &control,
        )?;
    }

    drop(read_tx);
    drop(hash_tx);
    drop(tree_tx);
    drop(pack_tx);
    drop(index_tx);
    drop(upload_tx);
    for handle in handles {
        if handle.join().is_err() {
            control.fail(BackupError::Thread {
                stage: BackupStage::Coordinator,
            });
        }
    }
    progress.close();
    progress.join();

    if let Some(error) = control.first_error() {
        return Err(error);
    }
    if control.is_cancelled() {
        return Err(BackupError::Cancelled);
    }
    let tree_result = match tree_result_rx.try_recv() {
        Ok(result) => result,
        Err(TryRecvError::Empty | TryRecvError::Disconnected) => {
            return Err(BackupError::Thread {
                stage: BackupStage::Pack,
            });
        }
    };

    let created_at = request.created_at.unwrap_or_else(current_unix_seconds);
    let parent = head
        .head
        .snapshot()
        .and_then(|reference| SnapshotId::from_reference(reference).ok());
    let snapshot_id = {
        let _cpu = UnitPermit::acquire(
            &cpu_workers,
            &control,
            BackupStage::Publish,
            BackupResource::CpuWorkers,
        )?;
        snapshot_id_for(&tree_result.root, &request, &head.head, created_at)
    }?;
    let root_object = tree_result
        .root
        .object_reference()
        .map_err(|_| BackupError::Invalid {
            stage: BackupStage::Publish,
        })?;
    let mut snapshot = Snapshot::new(snapshot_id, request.message, created_at).map_err(|_| {
        BackupError::Invalid {
            stage: BackupStage::Publish,
        }
    })?;
    snapshot = snapshot.with_parent(parent).with_root_tree(root_object);
    if let Some(author) = request.author {
        snapshot = snapshot
            .with_author(author)
            .map_err(|_| BackupError::Invalid {
                stage: BackupStage::Publish,
            })?;
    }
    snapshot = snapshot.with_statistics(
        tree_result.files,
        tree_result.directories,
        tree_result.total_size,
    );
    let snapshot_reference = snapshot.reference().map_err(|_| BackupError::Invalid {
        stage: BackupStage::Publish,
    })?;
    let snapshot_bytes = {
        let _cpu = UnitPermit::acquire(
            &cpu_workers,
            &control,
            BackupStage::Publish,
            BackupResource::CpuWorkers,
        )?;
        encode_snapshot(&snapshot).map_err(|_| BackupError::Format {
            stage: BackupStage::Publish,
        })?
    };
    let snapshot_memory = memory.reserve(
        snapshot_bytes.capacity().max(snapshot_bytes.len()),
        &control,
        BackupStage::Publish,
    )?;
    let uploader = ImmutableObjectUploader {
        storage: Arc::clone(&storage),
        network_requests: Arc::clone(&network_requests),
        file_descriptors: Arc::clone(&file_descriptors),
        control: control.clone(),
    };
    uploader.upload(
        BackupStage::Publish,
        "snapshot",
        snapshot_reference.as_str(),
        &snapshot_bytes,
    )?;
    drop(snapshot_memory);
    control.check()?;

    let cancelled = || control.is_cancelled();
    let _publish_network = UnitPermit::acquire(
        &network_requests,
        &control,
        BackupStage::Publish,
        BackupResource::NetworkRequests,
    )?;
    let _publish_descriptor = UnitPermit::acquire(
        &file_descriptors,
        &control,
        BackupStage::Publish,
        BackupResource::FileDescriptors,
    )?;
    let _publish_cpu = UnitPermit::acquire(
        &cpu_workers,
        &control,
        BackupStage::Publish,
        BackupResource::CpuWorkers,
    )?;
    repository::publish_head(
        storage.as_ref(),
        &head,
        &SnapshotPublication::new(snapshot_reference.clone()),
        Some(&cancelled),
    )
    .map_err(|error| map_repository_error(BackupStage::Publish, error))?;
    progress.emit(1);

    Ok(BackupRunResult {
        snapshot: snapshot_reference,
        metrics: metrics.snapshot(&memory, &cpu_workers, &file_descriptors, &network_requests),
    })
}

fn validate_request(
    request: &BackupRunRequest,
    encryption: Option<&EncryptionContext>,
) -> Result<(), BackupError> {
    if request.root.as_os_str().is_empty() {
        return Err(BackupError::Invalid {
            stage: BackupStage::Scan,
        });
    }
    if request.transforms.encryption() != ObjectEncryption::None && encryption.is_none() {
        return Err(BackupError::Format {
            stage: BackupStage::Transform,
        });
    }
    let plan = WorkerPlan::from_request(request.budgets);
    let transform_bound = transform_bound(request.chunking.max_size_usize(), request.transforms);
    let pack_bound = usize::try_from(request.pack.max_size()).unwrap_or(usize::MAX);
    let index_bound = usize::try_from(request.index.max_shard_bytes()).unwrap_or(usize::MAX);
    let read_scratch = plan
        .read_workers
        .saturating_mul(chunker_memory_size(request.chunking));
    let minimum = read_scratch
        .saturating_add(transform_bound)
        .saturating_add(pack_bound)
        .saturating_add(index_bound.saturating_mul(INDEX_MEMORY_SAFETY_MULTIPLIER))
        .saturating_add(TRANSFORM_MEMORY_OVERHEAD);
    if minimum > request.budgets.memory_bytes() {
        return Err(BackupError::Budget {
            stage: BackupStage::Transform,
            resource: BackupResource::Memory,
            requested: minimum,
            limit: request.budgets.memory_bytes(),
        });
    }
    let single_entry_pack = transform_bound
        .saturating_add(PACK_HEADER_LENGTH)
        .saturating_add(PACK_FOOTER_LENGTH);
    if single_entry_pack > pack_bound {
        return Err(BackupError::Budget {
            stage: BackupStage::Pack,
            resource: BackupResource::Memory,
            requested: single_entry_pack,
            limit: pack_bound,
        });
    }
    if transform_bound > MAX_IMMUTABLE_OBJECT_STORED_PAYLOAD_BYTES
        || transform_bound > MAX_IMMUTABLE_OBJECT_BYTES
    {
        return Err(BackupError::Invalid {
            stage: BackupStage::Transform,
        });
    }
    Ok(())
}

#[derive(Clone, Copy)]
struct WorkerPlan {
    read_workers: usize,
    hash_workers: usize,
    transform_workers: usize,
    network_workers: usize,
    scanner_directories: usize,
}

impl WorkerPlan {
    fn from_request(budgets: BackupBudgets) -> Self {
        let read_workers = (budgets.cpu_workers() / 3)
            .max(1)
            .min(budgets.file_descriptors().saturating_sub(3).max(1));
        let scanner_directories = budgets
            .file_descriptors()
            .saturating_sub(read_workers)
            .saturating_sub(1)
            .saturating_sub(1)
            .clamp(1, MAX_FILESYSTEM_SCAN_OPEN_DIRECTORIES);
        let remaining = budgets.cpu_workers().saturating_sub(read_workers);
        let hash_workers = (remaining / 2).max(1);
        let transform_workers = remaining.saturating_sub(hash_workers).max(1);
        Self {
            read_workers,
            hash_workers,
            transform_workers,
            network_workers: budgets.network_requests(),
            scanner_directories,
        }
    }
}

#[derive(Clone)]
struct PipelineControl {
    cancelled: Arc<AtomicBool>,
    external: Arc<dyn Fn() -> bool + Send + Sync>,
    first_error: Arc<Mutex<Option<BackupError>>>,
    wake: Arc<(Mutex<bool>, Condvar)>,
}

impl PipelineControl {
    fn new(external: Arc<dyn Fn() -> bool + Send + Sync>) -> Self {
        Self {
            cancelled: Arc::new(AtomicBool::new(false)),
            external,
            first_error: Arc::new(Mutex::new(None)),
            wake: Arc::new((Mutex::new(false), Condvar::new())),
        }
    }

    fn is_cancelled(&self) -> bool {
        self.cancelled.load(Ordering::Acquire) || (self.external)()
    }

    fn check(&self) -> Result<(), BackupError> {
        if self.is_cancelled() {
            Err(BackupError::Cancelled)
        } else {
            Ok(())
        }
    }

    fn cancel(&self) {
        self.cancelled.store(true, Ordering::Release);
        let (_, changed) = &*self.wake;
        changed.notify_all();
    }

    fn fail(&self, error: BackupError) {
        if !error.is_cancelled() {
            let mut first_error = lock_or_recover(&self.first_error);
            if first_error.is_none() {
                *first_error = Some(error);
            }
        }
        self.cancel();
    }

    fn first_error(&self) -> Option<BackupError> {
        *lock_or_recover(&self.first_error)
    }

    fn wait(&self) {
        let (lock, changed) = &*self.wake;
        let guard = lock_or_recover(lock);
        let _ = changed.wait_timeout(guard, CHANNEL_WAIT);
    }
}

struct ResourceBudget {
    limit: usize,
    state: Mutex<ResourceState>,
    changed: Condvar,
    peak: AtomicUsize,
}

#[derive(Default)]
struct ResourceState {
    used: usize,
}

impl ResourceBudget {
    const fn new(limit: usize) -> Self {
        Self {
            limit,
            state: Mutex::new(ResourceState { used: 0 }),
            changed: Condvar::new(),
            peak: AtomicUsize::new(0),
        }
    }

    fn reserve(
        self: &Arc<Self>,
        amount: usize,
        control: &PipelineControl,
        stage: BackupStage,
    ) -> Result<MemoryPermit, BackupError> {
        if amount > self.limit {
            return Err(BackupError::Budget {
                stage,
                resource: BackupResource::Memory,
                requested: amount,
                limit: self.limit,
            });
        }
        loop {
            control.check()?;
            let mut state = lock_or_recover(&self.state);
            if self.limit.saturating_sub(state.used) >= amount {
                state.used = state.used.saturating_add(amount);
                update_peak(&self.peak, state.used);
                return Ok(MemoryPermit {
                    budget: Arc::clone(self),
                    amount,
                });
            }
            let _ = self.changed.wait_timeout(state, CHANNEL_WAIT);
        }
    }

    fn release(&self, amount: usize) {
        let mut state = lock_or_recover(&self.state);
        state.used = state.used.saturating_sub(amount);
        self.changed.notify_all();
    }

    fn peak(&self) -> usize {
        self.peak.load(Ordering::Acquire)
    }
}

struct MemoryPermit {
    budget: Arc<ResourceBudget>,
    amount: usize,
}

impl MemoryPermit {
    fn grow(
        &mut self,
        additional: usize,
        control: &PipelineControl,
        stage: BackupStage,
    ) -> Result<(), BackupError> {
        if additional == 0 {
            return Ok(());
        }
        if self.amount.saturating_add(additional) > self.budget.limit {
            return Err(BackupError::Budget {
                stage,
                resource: BackupResource::Memory,
                requested: self.amount.saturating_add(additional),
                limit: self.budget.limit,
            });
        }
        loop {
            control.check()?;
            let mut state = lock_or_recover(&self.budget.state);
            if self.budget.limit.saturating_sub(state.used) >= additional {
                state.used = state.used.saturating_add(additional);
                self.amount = self.amount.saturating_add(additional);
                update_peak(&self.budget.peak, state.used);
                return Ok(());
            }
            let _ = self.budget.changed.wait_timeout(state, CHANNEL_WAIT);
        }
    }
}

impl Drop for MemoryPermit {
    fn drop(&mut self) {
        self.budget.release(self.amount);
    }
}

struct UnitPermit {
    budget: Arc<ResourceBudget>,
    amount: usize,
    resource: BackupResource,
    stage: BackupStage,
}

impl UnitPermit {
    fn acquire(
        budget: &Arc<ResourceBudget>,
        control: &PipelineControl,
        stage: BackupStage,
        resource: BackupResource,
    ) -> Result<Self, BackupError> {
        loop {
            control.check()?;
            let mut state = lock_or_recover(&budget.state);
            if state.used < budget.limit {
                state.used += 1;
                update_peak(&budget.peak, state.used);
                return Ok(Self {
                    budget: Arc::clone(budget),
                    amount: 1,
                    resource,
                    stage,
                });
            }
            let _ = budget.changed.wait_timeout(state, CHANNEL_WAIT);
        }
    }
}

impl Drop for UnitPermit {
    fn drop(&mut self) {
        let _ = (self.resource, self.stage);
        self.budget.release(self.amount);
    }
}

#[derive(Default)]
struct MetricsState {
    scanned_entries: AtomicU64,
    files: AtomicU64,
    directories: AtomicU64,
    bytes_read: AtomicU64,
    total_size: AtomicU64,
    chunks: AtomicU64,
    transformed_chunks: AtomicU64,
    packs: AtomicU64,
    index_shards: AtomicU64,
    uploaded_objects: AtomicU64,
    hash_active: AtomicUsize,
    hash_peak: AtomicUsize,
    transform_active: AtomicUsize,
    transform_peak: AtomicUsize,
}

impl MetricsState {
    fn snapshot(
        &self,
        memory: &ResourceBudget,
        cpu_workers: &ResourceBudget,
        file_descriptors: &ResourceBudget,
        network_requests: &ResourceBudget,
    ) -> BackupMetrics {
        BackupMetrics::from_parts(
            self.scanned_entries.load(Ordering::Acquire),
            self.files.load(Ordering::Acquire),
            self.directories.load(Ordering::Acquire),
            self.bytes_read.load(Ordering::Acquire),
            self.total_size.load(Ordering::Acquire),
            self.chunks.load(Ordering::Acquire),
            self.transformed_chunks.load(Ordering::Acquire),
            self.packs.load(Ordering::Acquire),
            self.index_shards.load(Ordering::Acquire),
            self.uploaded_objects.load(Ordering::Acquire),
            memory.peak(),
            file_descriptors.peak(),
            network_requests.peak(),
            cpu_workers.peak(),
            self.hash_peak.load(Ordering::Acquire),
            self.transform_peak.load(Ordering::Acquire),
        )
    }
}

#[derive(Clone)]
struct ProgressReporter {
    completed: Arc<AtomicU64>,
    sender: SyncSender<u64>,
    closed: Arc<AtomicBool>,
    join: Arc<Mutex<Option<JoinHandle<()>>>>,
}

impl ProgressReporter {
    fn new(queue_capacity: usize, callback: Arc<dyn Fn(u64) + Send + Sync>) -> Self {
        let (sender, receiver) = sync_channel(queue_capacity.clamp(1, PROGRESS_REPORT_CAPACITY));
        let closed = Arc::new(AtomicBool::new(false));
        let worker_closed = Arc::clone(&closed);
        let join = thread::Builder::new()
            .name(String::from("gib-backup-progress"))
            .spawn(move || run_progress_worker(receiver, callback, worker_closed))
            .ok();
        Self {
            completed: Arc::new(AtomicU64::new(0)),
            sender,
            closed,
            join: Arc::new(Mutex::new(join)),
        }
    }

    fn emit(&self, units: u64) {
        let completed = self.completed.fetch_add(units, Ordering::AcqRel) + units;
        let _ = self.sender.try_send(completed);
    }

    fn close(&self) {
        self.closed.store(true, Ordering::Release);
    }

    fn join(&self) {
        let mut join = lock_or_recover(&self.join);
        if let Some(handle) = join.take() {
            let _ = handle.join();
        }
    }
}

fn run_progress_worker(
    receiver: Receiver<u64>,
    callback: Arc<dyn Fn(u64) + Send + Sync>,
    closed: Arc<AtomicBool>,
) {
    loop {
        match receiver.recv_timeout(CHANNEL_WAIT) {
            Ok(units) => callback(units),
            Err(RecvTimeoutError::Timeout) if closed.load(Ordering::Acquire) => break,
            Err(RecvTimeoutError::Timeout) => continue,
            Err(RecvTimeoutError::Disconnected) => break,
        }
    }
}

struct EntryMessage {
    sequence: u64,
    entry: FilesystemEntry,
    memory: MemoryPermit,
}

enum ScanMessage {
    Entry(EntryMessage),
    DirectoryEnd {
        sequence: u64,
        path: crate::domain::RelativePath,
        memory: MemoryPermit,
    },
}

enum ReadMessage {
    Entry(EntryMessage),
    Chunk {
        sequence: u64,
        ordinal: u64,
        id: ChunkId,
        plaintext_length: u64,
        bytes: Vec<u8>,
        memory: MemoryPermit,
    },
    FileEnd {
        sequence: u64,
        chunk_count: u64,
    },
    DirectoryEnd {
        sequence: u64,
        path: crate::domain::RelativePath,
        memory: MemoryPermit,
    },
}

enum TreeMessage {
    Entry(EntryMessage),
    Chunk {
        sequence: u64,
        ordinal: u64,
        entry: PackEntryInput,
        memory: MemoryPermit,
    },
    FileEnd {
        sequence: u64,
        chunk_count: u64,
    },
    DirectoryEnd {
        sequence: u64,
        path: crate::domain::RelativePath,
        memory: MemoryPermit,
    },
}

struct PackMessage {
    entry: PackEntryInput,
    memory: MemoryPermit,
}

enum UploadWork {
    Tree {
        id: ObjectId,
        bytes: Vec<u8>,
        memory: MemoryPermit,
    },
    Pack {
        pack: SealedPack,
        memory: MemoryPermit,
        transform: PackIndexTransform,
    },
    Index {
        shard: SealedPackIndexShard,
        memory: MemoryPermit,
    },
}

enum IndexMessage {
    PackEntry(PackIndexEntry),
    PackUploaded,
    PacksFinished { expected: u64 },
}

struct TreeResult {
    root: TreeNodeReference,
    files: u64,
    directories: u64,
    total_size: u64,
}

struct OpenDirectory {
    path: crate::domain::RelativePath,
    memory: MemoryPermit,
}

struct DirectoryAccumulator {
    path: crate::domain::RelativePath,
    metadata: PortableMetadata,
    entries: Vec<TreeEntry>,
    memory: MemoryPermit,
}

struct TreeSequenceState {
    entry: Option<EntryMessage>,
    pending_chunks: BTreeMap<u64, (PackEntryInput, MemoryPermit)>,
    references: Vec<FileChunkReference>,
    reference_memory: Option<MemoryPermit>,
    next_ordinal: u64,
    file_end: Option<u64>,
    completed: bool,
}

impl TreeSequenceState {
    fn new() -> Self {
        Self {
            entry: None,
            pending_chunks: BTreeMap::new(),
            references: Vec::new(),
            reference_memory: None,
            next_ordinal: 0,
            file_end: None,
            completed: false,
        }
    }
}

struct TreeBuildState {
    directories: Vec<DirectoryAccumulator>,
    root: Option<TreeNodeReference>,
    files: u64,
    directory_count: u64,
    total_size: u64,
}

impl TreeBuildState {
    fn new() -> Self {
        Self {
            directories: Vec::new(),
            root: None,
            files: 0,
            directory_count: 0,
            total_size: 0,
        }
    }
}

struct TreeStageContext {
    pack_sender: SyncSender<PackMessage>,
    upload_sender: SyncSender<UploadWork>,
    memory: Arc<ResourceBudget>,
    cpu_workers: Arc<ResourceBudget>,
    control: PipelineControl,
    progress: ProgressReporter,
}

#[allow(clippy::too_many_arguments)]
fn run_scan_worker<F, C>(
    scanner: FilesystemScanner<F, C>,
    root: PathBuf,
    sender: SyncSender<ScanMessage>,
    file_descriptors: Arc<ResourceBudget>,
    memory: Arc<ResourceBudget>,
    control: PipelineControl,
    metrics: Arc<MetricsState>,
    progress: ProgressReporter,
    scanner_directories: usize,
) where
    F: super::ports::Filesystem + 'static,
    C: super::ports::FilesystemClock + 'static,
{
    let mut directory_permits = Vec::new();
    for _ in 0..scanner_directories {
        match UnitPermit::acquire(
            &file_descriptors,
            &control,
            BackupStage::Scan,
            BackupResource::FileDescriptors,
        ) {
            Ok(permit) => directory_permits.push(permit),
            Err(error) => {
                control.fail(error);
                return;
            }
        }
    }
    let mut scan = match scanner.scan(root) {
        Ok(scan) => scan,
        Err(error) => {
            control.fail(filesystem_error(BackupStage::Scan, &error));
            return;
        }
    };
    let mut open_directories: Vec<OpenDirectory> = Vec::new();
    let mut sequence = 0_u64;
    loop {
        if control.check().is_err() {
            return;
        }
        let next = scan.next();
        let Some(item) = next else {
            while let Some(directory) = open_directories.pop() {
                if emit_directory_end(&sender, directory.path, &memory, &control, &mut sequence)
                    .is_err()
                {
                    return;
                }
                drop(directory.memory);
            }
            return;
        };
        let entry = match item {
            Ok(entry) => entry,
            Err(error) => {
                control.fail(filesystem_error(BackupStage::Scan, &error));
                return;
            }
        };
        while open_directories
            .last()
            .is_some_and(|directory| !is_ancestor(&directory.path, entry.path()))
        {
            let Some(directory) = open_directories.pop() else {
                break;
            };
            if emit_directory_end(&sender, directory.path, &memory, &control, &mut sequence)
                .is_err()
            {
                return;
            }
            drop(directory.memory);
        }
        let entry_memory =
            match memory.reserve(entry_memory_size(&entry), &control, BackupStage::Scan) {
                Ok(memory) => memory,
                Err(error) => {
                    control.fail(error);
                    return;
                }
            };
        let entry_sequence = sequence;
        sequence = match sequence.checked_add(1) {
            Some(next) => next,
            None => {
                control.fail(BackupError::Thread {
                    stage: BackupStage::Scan,
                });
                return;
            }
        };
        metrics.scanned_entries.fetch_add(1, Ordering::Relaxed);
        match entry.kind() {
            FilesystemEntryKind::Directory => {
                metrics.directories.fetch_add(1, Ordering::Relaxed);
                let directory_memory = match memory.reserve(
                    entry.path().as_str().len() + DIRECTORY_MEMORY_OVERHEAD,
                    &control,
                    BackupStage::Scan,
                ) {
                    Ok(memory) => memory,
                    Err(error) => {
                        control.fail(error);
                        return;
                    }
                };
                open_directories.push(OpenDirectory {
                    path: entry.path().clone(),
                    memory: directory_memory,
                });
            }
            FilesystemEntryKind::RegularFile => {
                metrics.files.fetch_add(1, Ordering::Relaxed);
                metrics
                    .total_size
                    .fetch_add(entry.metadata().size(), Ordering::Relaxed);
            }
            FilesystemEntryKind::SymbolicLink | FilesystemEntryKind::Other => {}
        }
        progress.emit(1);
        if send_message(
            &sender,
            ScanMessage::Entry(EntryMessage {
                sequence: entry_sequence,
                entry,
                memory: entry_memory,
            }),
            &control,
        )
        .is_err()
        {
            return;
        }
    }
}

fn emit_directory_end(
    sender: &SyncSender<ScanMessage>,
    path: crate::domain::RelativePath,
    memory: &Arc<ResourceBudget>,
    control: &PipelineControl,
    sequence: &mut u64,
) -> Result<(), ()> {
    let event_memory = match memory.reserve(
        path.as_str().len() + DIRECTORY_MEMORY_OVERHEAD,
        control,
        BackupStage::Scan,
    ) {
        Ok(memory) => memory,
        Err(error) => {
            control.fail(error);
            return Err(());
        }
    };
    let current = *sequence;
    *sequence = match sequence.checked_add(1) {
        Some(next) => next,
        None => {
            control.fail(BackupError::Thread {
                stage: BackupStage::Scan,
            });
            return Err(());
        }
    };
    send_message(
        sender,
        ScanMessage::DirectoryEnd {
            sequence: current,
            path,
            memory: event_memory,
        },
        control,
    )
}

#[allow(clippy::too_many_arguments)]
fn run_read_worker<F, C>(
    receiver: Arc<Mutex<Receiver<ScanMessage>>>,
    sender: SyncSender<ReadMessage>,
    scanner: FilesystemScanner<F, C>,
    root: PathBuf,
    file_descriptors: Arc<ResourceBudget>,
    memory: Arc<ResourceBudget>,
    cpu_workers: Arc<ResourceBudget>,
    control: PipelineControl,
    metrics: Arc<MetricsState>,
    progress: ProgressReporter,
    chunking: ChunkingConfiguration,
) where
    F: super::ports::Filesystem + 'static,
    C: super::ports::FilesystemClock + 'static,
{
    loop {
        let Some(message) = receive_message(&receiver, &control) else {
            return;
        };
        match message {
            ScanMessage::DirectoryEnd {
                sequence,
                path,
                memory,
            } => {
                if send_message(
                    &sender,
                    ReadMessage::DirectoryEnd {
                        sequence,
                        path,
                        memory,
                    },
                    &control,
                )
                .is_err()
                {
                    return;
                }
            }
            ScanMessage::Entry(message) => {
                let EntryMessage {
                    sequence,
                    entry,
                    memory: entry_memory,
                } = message;
                let is_file = entry.kind() == FilesystemEntryKind::RegularFile;
                let (descriptor, reader) = if is_file {
                    let descriptor = match UnitPermit::acquire(
                        &file_descriptors,
                        &control,
                        BackupStage::Read,
                        BackupResource::FileDescriptors,
                    ) {
                        Ok(permit) => permit,
                        Err(error) => {
                            control.fail(error);
                            return;
                        }
                    };
                    let reader = match scanner.open_file(&root, &entry) {
                        Ok(reader) => reader,
                        Err(error) => {
                            control.fail(filesystem_error(BackupStage::Read, &error));
                            drop(descriptor);
                            return;
                        }
                    };
                    (Some(descriptor), Some(reader))
                } else {
                    (None, None)
                };
                if send_message(
                    &sender,
                    ReadMessage::Entry(EntryMessage {
                        sequence,
                        entry,
                        memory: entry_memory,
                    }),
                    &control,
                )
                .is_err()
                {
                    return;
                }
                let (Some(descriptor), Some(reader)) = (descriptor, reader) else {
                    continue;
                };
                let chunker_memory_size = chunker_memory_size(chunking);
                let chunker_memory =
                    match memory.reserve(chunker_memory_size, &control, BackupStage::Chunk) {
                        Ok(memory) => memory,
                        Err(error) => {
                            control.fail(error);
                            drop(descriptor);
                            return;
                        }
                    };
                let mut chunker = crate::domain::Chunker::with_cancellation(reader, chunking, {
                    let control = control.clone();
                    move || control.is_cancelled()
                });
                let mut ordinal = 0_u64;
                loop {
                    let chunk = {
                        let _cpu = match UnitPermit::acquire(
                            &cpu_workers,
                            &control,
                            BackupStage::Chunk,
                            BackupResource::CpuWorkers,
                        ) {
                            Ok(permit) => permit,
                            Err(error) => {
                                drop(descriptor);
                                control.fail(error);
                                return;
                            }
                        };
                        match chunker.next_chunk() {
                            Ok(Some(chunk)) => chunk,
                            Ok(None) => break,
                            Err(crate::domain::ChunkingError::Cancelled) => {
                                drop(descriptor);
                                return;
                            }
                            Err(crate::domain::ChunkingError::Io(error)) => {
                                control.fail(BackupError::Filesystem {
                                    stage: BackupStage::Chunk,
                                    operation: Some(FilesystemOperation::ReadFile),
                                    kind: Some(super::ports::map_io_error(&error)),
                                    race: false,
                                });
                                drop(descriptor);
                                return;
                            }
                            Err(crate::domain::ChunkingError::InvalidSourceRead)
                            | Err(crate::domain::ChunkingError::OffsetOverflow) => {
                                control.fail(BackupError::Filesystem {
                                    stage: BackupStage::Chunk,
                                    operation: None,
                                    kind: Some(FilesystemErrorKind::Other),
                                    race: false,
                                });
                                drop(descriptor);
                                return;
                            }
                        }
                    };
                    let (offset, id, _, bytes) = chunk.into_parts();
                    let _ = offset;
                    let byte_count = bytes
                        .capacity()
                        .max(bytes.len())
                        .saturating_add(CHUNK_MESSAGE_OVERHEAD);
                    let chunk_memory =
                        match memory.reserve(byte_count, &control, BackupStage::Chunk) {
                            Ok(memory) => memory,
                            Err(error) => {
                                control.fail(error);
                                drop(descriptor);
                                return;
                            }
                        };
                    let plaintext_length = match u64::try_from(bytes.len()) {
                        Ok(length) => length,
                        Err(_) => {
                            control.fail(BackupError::Invalid {
                                stage: BackupStage::Chunk,
                            });
                            drop(chunk_memory);
                            drop(descriptor);
                            return;
                        }
                    };
                    if send_message(
                        &sender,
                        ReadMessage::Chunk {
                            sequence,
                            ordinal,
                            id,
                            plaintext_length,
                            bytes,
                            memory: chunk_memory,
                        },
                        &control,
                    )
                    .is_err()
                    {
                        drop(descriptor);
                        return;
                    }
                    ordinal = match ordinal.checked_add(1) {
                        Some(next) => next,
                        None => {
                            control.fail(BackupError::Thread {
                                stage: BackupStage::Chunk,
                            });
                            drop(descriptor);
                            return;
                        }
                    };
                    metrics.chunks.fetch_add(1, Ordering::Relaxed);
                    metrics
                        .bytes_read
                        .fetch_add(plaintext_length, Ordering::Relaxed);
                    progress.emit(1);
                }
                let reader = chunker.into_inner();
                if let Err(error) = reader.finish() {
                    control.fail(filesystem_error(BackupStage::Read, &error));
                    drop(descriptor);
                    return;
                }
                if send_message(
                    &sender,
                    ReadMessage::FileEnd {
                        sequence,
                        chunk_count: ordinal,
                    },
                    &control,
                )
                .is_err()
                {
                    drop(descriptor);
                    return;
                }
                drop(chunker_memory);
                drop(descriptor);
            }
        }
    }
}

fn run_hash_worker(
    receiver: Arc<Mutex<Receiver<ReadMessage>>>,
    sender: SyncSender<ReadMessage>,
    cpu_workers: Arc<ResourceBudget>,
    control: PipelineControl,
    metrics: Arc<MetricsState>,
    progress: ProgressReporter,
) {
    loop {
        let Some(message) = receive_message(&receiver, &control) else {
            return;
        };
        match message {
            ReadMessage::Chunk {
                sequence,
                ordinal,
                id,
                plaintext_length,
                bytes,
                memory,
            } => {
                let calculated = {
                    let _cpu = match UnitPermit::acquire(
                        &cpu_workers,
                        &control,
                        BackupStage::Hash,
                        BackupResource::CpuWorkers,
                    ) {
                        Ok(permit) => permit,
                        Err(error) => {
                            drop(memory);
                            control.fail(error);
                            return;
                        }
                    };
                    let active = metrics.hash_active.fetch_add(1, Ordering::AcqRel) + 1;
                    update_peak(&metrics.hash_peak, active);
                    let calculated = ChunkId::from_content(&bytes);
                    metrics.hash_active.fetch_sub(1, Ordering::AcqRel);
                    calculated
                };
                if calculated != id {
                    control.fail(BackupError::Invalid {
                        stage: BackupStage::Hash,
                    });
                    drop(memory);
                    return;
                }
                if send_message(
                    &sender,
                    ReadMessage::Chunk {
                        sequence,
                        ordinal,
                        id,
                        plaintext_length,
                        bytes,
                        memory,
                    },
                    &control,
                )
                .is_err()
                {
                    return;
                }
                progress.emit(1);
            }
            other => {
                if send_message(&sender, other, &control).is_err() {
                    return;
                }
            }
        }
    }
}

#[allow(clippy::too_many_arguments)]
fn run_transform_worker(
    receiver: Arc<Mutex<Receiver<ReadMessage>>>,
    sender: SyncSender<TreeMessage>,
    memory: Arc<ResourceBudget>,
    cpu_workers: Arc<ResourceBudget>,
    control: PipelineControl,
    metrics: Arc<MetricsState>,
    progress: ProgressReporter,
    transforms: ObjectTransformOptions,
    encryption: Option<EncryptionContext>,
) {
    loop {
        let Some(message) = receive_message(&receiver, &control) else {
            return;
        };
        match message {
            ReadMessage::Chunk {
                sequence,
                ordinal,
                id,
                plaintext_length,
                bytes,
                memory: input_memory,
            } => {
                let output_bound = transform_bound(bytes.len(), transforms);
                let mut output_memory =
                    match memory.reserve(output_bound, &control, BackupStage::Transform) {
                        Ok(memory) => memory,
                        Err(error) => {
                            drop(input_memory);
                            control.fail(error);
                            return;
                        }
                    };
                let encoded = {
                    let _cpu = match UnitPermit::acquire(
                        &cpu_workers,
                        &control,
                        BackupStage::Transform,
                        BackupResource::CpuWorkers,
                    ) {
                        Ok(permit) => permit,
                        Err(error) => {
                            drop(output_memory);
                            drop(input_memory);
                            control.fail(error);
                            return;
                        }
                    };
                    let active = metrics.transform_active.fetch_add(1, Ordering::AcqRel) + 1;
                    update_peak(&metrics.transform_peak, active);
                    let result = encode_object_envelope_with_options(
                        ObjectKind::Pack,
                        CURRENT_PACK_OBJECT_VERSION,
                        transforms,
                        encryption.as_ref(),
                        &bytes,
                    );
                    metrics.transform_active.fetch_sub(1, Ordering::AcqRel);
                    match result {
                        Ok(encoded) => encoded,
                        Err(_) => {
                            drop(output_memory);
                            drop(input_memory);
                            control.fail(BackupError::Format {
                                stage: BackupStage::Transform,
                            });
                            return;
                        }
                    }
                };
                let actual = encoded.capacity().max(encoded.len());
                if actual > output_bound
                    && output_memory
                        .grow(
                            actual.saturating_sub(output_bound),
                            &control,
                            BackupStage::Transform,
                        )
                        .is_err()
                {
                    drop(output_memory);
                    drop(input_memory);
                    control.fail(BackupError::Budget {
                        stage: BackupStage::Transform,
                        resource: BackupResource::Memory,
                        requested: actual,
                        limit: memory.limit,
                    });
                    return;
                }
                let entry = match PackEntryInput::new(id, plaintext_length, encoded) {
                    Ok(entry) => entry,
                    Err(_) => {
                        drop(output_memory);
                        drop(input_memory);
                        control.fail(BackupError::Invalid {
                            stage: BackupStage::Transform,
                        });
                        return;
                    }
                };
                metrics.transformed_chunks.fetch_add(1, Ordering::Relaxed);
                drop(input_memory);
                if send_message(
                    &sender,
                    TreeMessage::Chunk {
                        sequence,
                        ordinal,
                        entry,
                        memory: output_memory,
                    },
                    &control,
                )
                .is_err()
                {
                    return;
                }
                progress.emit(1);
            }
            ReadMessage::Entry(message) => {
                if send_message(&sender, TreeMessage::Entry(message), &control).is_err() {
                    return;
                }
            }
            ReadMessage::FileEnd {
                sequence,
                chunk_count,
            } => {
                if send_message(
                    &sender,
                    TreeMessage::FileEnd {
                        sequence,
                        chunk_count,
                    },
                    &control,
                )
                .is_err()
                {
                    return;
                }
            }
            ReadMessage::DirectoryEnd {
                sequence,
                path,
                memory,
            } => {
                if send_message(
                    &sender,
                    TreeMessage::DirectoryEnd {
                        sequence,
                        path,
                        memory,
                    },
                    &control,
                )
                .is_err()
                {
                    return;
                }
            }
        }
    }
}

#[allow(clippy::too_many_arguments)]
fn run_tree_worker(
    receiver: Arc<Mutex<Receiver<TreeMessage>>>,
    pack_sender: SyncSender<PackMessage>,
    upload_sender: SyncSender<UploadWork>,
    result_sender: SyncSender<TreeResult>,
    memory: Arc<ResourceBudget>,
    cpu_workers: Arc<ResourceBudget>,
    control: PipelineControl,
    _metrics: Arc<MetricsState>,
    progress: ProgressReporter,
    _chunking: ChunkingConfiguration,
) {
    let mut pending: BTreeMap<u64, Vec<TreeMessage>> = BTreeMap::new();
    let mut current = None;
    let mut next_sequence = 0_u64;
    let mut tree = TreeBuildState::new();
    let context = TreeStageContext {
        pack_sender,
        upload_sender,
        memory,
        cpu_workers,
        control,
        progress,
    };
    loop {
        let Some(message) = receive_message(&receiver, &context.control) else {
            break;
        };
        let sequence = tree_message_sequence(&message);
        if sequence < next_sequence {
            context.control.fail(BackupError::Invalid {
                stage: BackupStage::Pack,
            });
            return;
        }
        if sequence > next_sequence {
            pending.entry(sequence).or_default().push(message);
            continue;
        }
        if current.is_none() {
            current = Some(TreeSequenceState::new());
        }
        let Some(state) = current.as_mut() else {
            context.control.fail(BackupError::Thread {
                stage: BackupStage::Pack,
            });
            return;
        };
        if consume_tree_message(message, state, &mut tree, &context).is_err() {
            return;
        }
        if advance_completed_tree_sequences(
            &mut current,
            &mut pending,
            &mut next_sequence,
            &mut tree,
            &context,
        )
        .is_err()
        {
            return;
        }
    }
    if context.control.is_cancelled() {
        return;
    }
    if current.is_some() || !pending.is_empty() || !tree.directories.is_empty() {
        context.control.fail(BackupError::Invalid {
            stage: BackupStage::Pack,
        });
        return;
    }
    let Some(root) = tree.root else {
        context.control.fail(BackupError::Invalid {
            stage: BackupStage::Pack,
        });
        return;
    };
    let _ = send_message(
        &result_sender,
        TreeResult {
            root,
            files: tree.files,
            directories: tree.directory_count,
            total_size: tree.total_size,
        },
        &context.control,
    );
}

#[allow(clippy::too_many_arguments)]
fn advance_completed_tree_sequences(
    current: &mut Option<TreeSequenceState>,
    pending: &mut BTreeMap<u64, Vec<TreeMessage>>,
    next_sequence: &mut u64,
    tree: &mut TreeBuildState,
    context: &TreeStageContext,
) -> Result<(), ()> {
    loop {
        if !current.as_ref().is_some_and(|state| state.completed) {
            return Ok(());
        }
        current.take();
        *next_sequence = match next_sequence.checked_add(1) {
            Some(next) => next,
            None => {
                context.control.fail(BackupError::Thread {
                    stage: BackupStage::Pack,
                });
                return Err(());
            }
        };
        let Some(messages) = pending.remove(next_sequence) else {
            return Ok(());
        };
        let message_count = messages.len();
        for (index, message) in messages.into_iter().enumerate() {
            if tree_message_sequence(&message) != *next_sequence {
                context.control.fail(BackupError::Invalid {
                    stage: BackupStage::Pack,
                });
                return Err(());
            }
            if current.is_none() {
                *current = Some(TreeSequenceState::new());
            }
            let Some(state) = current.as_mut() else {
                context.control.fail(BackupError::Thread {
                    stage: BackupStage::Pack,
                });
                return Err(());
            };
            if consume_tree_message(message, state, tree, context).is_err() {
                return Err(());
            }
            if state.completed && index + 1 < message_count {
                context.control.fail(BackupError::Invalid {
                    stage: BackupStage::Pack,
                });
                return Err(());
            }
        }
    }
}

fn consume_tree_message(
    message: TreeMessage,
    state: &mut TreeSequenceState,
    tree: &mut TreeBuildState,
    context: &TreeStageContext,
) -> Result<(), ()> {
    if state.completed {
        context.control.fail(BackupError::Invalid {
            stage: BackupStage::Pack,
        });
        return Err(());
    }
    match message {
        TreeMessage::Entry(message) => {
            if state.entry.is_some() {
                context.control.fail(BackupError::Invalid {
                    stage: BackupStage::Pack,
                });
                return Err(());
            }
            let regular_file = message.entry.kind() == FilesystemEntryKind::RegularFile;
            state.entry = Some(message);
            if regular_file {
                flush_tree_chunks(state, context)?;
                finish_tree_file_if_ready(state, tree, context)?;
            } else {
                if state.file_end.is_some() || !state.pending_chunks.is_empty() {
                    context.control.fail(BackupError::Invalid {
                        stage: BackupStage::Pack,
                    });
                    return Err(());
                }
                let Some(message) = state.entry.take() else {
                    context.control.fail(BackupError::Thread {
                        stage: BackupStage::Pack,
                    });
                    return Err(());
                };
                process_tree_non_file(message, tree, context)?;
                state.completed = true;
            }
        }
        TreeMessage::Chunk {
            ordinal,
            entry,
            memory,
            ..
        } => {
            if state
                .entry
                .as_ref()
                .is_some_and(|message| message.entry.kind() != FilesystemEntryKind::RegularFile)
            {
                context.control.fail(BackupError::Invalid {
                    stage: BackupStage::Pack,
                });
                return Err(());
            }
            if state
                .pending_chunks
                .insert(ordinal, (entry, memory))
                .is_some()
            {
                context.control.fail(BackupError::Invalid {
                    stage: BackupStage::Pack,
                });
                return Err(());
            }
            flush_tree_chunks(state, context)?;
            finish_tree_file_if_ready(state, tree, context)?;
        }
        TreeMessage::FileEnd { chunk_count, .. } => {
            if state.file_end.replace(chunk_count).is_some() {
                context.control.fail(BackupError::Invalid {
                    stage: BackupStage::Pack,
                });
                return Err(());
            }
            flush_tree_chunks(state, context)?;
            finish_tree_file_if_ready(state, tree, context)?;
        }
        TreeMessage::DirectoryEnd { path, memory, .. } => {
            if state.entry.is_some()
                || !state.pending_chunks.is_empty()
                || !state.references.is_empty()
                || state.reference_memory.is_some()
                || state.next_ordinal != 0
                || state.file_end.is_some()
            {
                context.control.fail(BackupError::Invalid {
                    stage: BackupStage::Pack,
                });
                return Err(());
            }
            process_tree_directory_end(path, memory, tree, context)?;
            state.completed = true;
        }
    }
    Ok(())
}

fn flush_tree_chunks(state: &mut TreeSequenceState, context: &TreeStageContext) -> Result<(), ()> {
    loop {
        let ordinal = state.next_ordinal;
        let Some((entry, memory)) = state.pending_chunks.remove(&ordinal) else {
            break;
        };
        let reference = match FileChunkReference::new(entry.chunk_id(), entry.plaintext_length()) {
            Ok(reference) => reference,
            Err(_) => {
                context.control.fail(BackupError::Invalid {
                    stage: BackupStage::Pack,
                });
                return Err(());
            }
        };
        if send_message(
            &context.pack_sender,
            PackMessage { entry, memory },
            &context.control,
        )
        .is_err()
        {
            return Err(());
        }
        if state.reference_memory.is_none() {
            let permit = match context
                .memory
                .reserve(0, &context.control, BackupStage::Pack)
            {
                Ok(permit) => permit,
                Err(error) => {
                    context.control.fail(error);
                    return Err(());
                }
            };
            state.reference_memory = Some(permit);
        }
        let Some(reference_memory) = state.reference_memory.as_mut() else {
            context.control.fail(BackupError::Thread {
                stage: BackupStage::Pack,
            });
            return Err(());
        };
        if let Err(error) = reference_memory.grow(
            std::mem::size_of::<FileChunkReference>(),
            &context.control,
            BackupStage::Pack,
        ) {
            context.control.fail(error);
            return Err(());
        }
        if state.references.try_reserve(1).is_err() {
            context.control.fail(BackupError::Format {
                stage: BackupStage::Pack,
            });
            return Err(());
        }
        state.references.push(reference);
        state.next_ordinal = match state.next_ordinal.checked_add(1) {
            Some(next) => next,
            None => {
                context.control.fail(BackupError::Thread {
                    stage: BackupStage::Pack,
                });
                return Err(());
            }
        };
    }
    if let Some(count) = state.file_end
        && (state.next_ordinal > count
            || state
                .pending_chunks
                .keys()
                .next()
                .is_some_and(|ordinal| *ordinal >= count))
    {
        context.control.fail(BackupError::Invalid {
            stage: BackupStage::Pack,
        });
        return Err(());
    }
    Ok(())
}

fn finish_tree_file_if_ready(
    state: &mut TreeSequenceState,
    tree: &mut TreeBuildState,
    context: &TreeStageContext,
) -> Result<(), ()> {
    let Some(chunk_count) = state.file_end else {
        return Ok(());
    };
    if state.next_ordinal != chunk_count
        || !state.pending_chunks.is_empty()
        || state.entry.is_none()
    {
        return Ok(());
    }
    let Some(entry_message) = state.entry.take() else {
        context.control.fail(BackupError::Thread {
            stage: BackupStage::Pack,
        });
        return Err(());
    };
    if entry_message.entry.kind() != FilesystemEntryKind::RegularFile {
        context.control.fail(BackupError::Invalid {
            stage: BackupStage::Pack,
        });
        return Err(());
    }
    let EntryMessage {
        entry: filesystem_entry,
        memory: entry_memory,
        ..
    } = entry_message;
    let metadata = portable_metadata(filesystem_entry.metadata());
    let references = std::mem::take(&mut state.references);
    let reference_memory = state.reference_memory.take();
    let node = match RegularFileNode::new(filesystem_entry.metadata().size(), references, metadata)
    {
        Ok(node) => TreeNode::RegularFile(node),
        Err(_) => {
            context.control.fail(BackupError::Invalid {
                stage: BackupStage::Pack,
            });
            return Err(());
        }
    };
    let reference = emit_tree_node(
        node,
        entry_memory,
        &context.upload_sender,
        &context.memory,
        &context.cpu_workers,
        &context.control,
    )?;
    drop(reference_memory);
    add_tree_reference(tree, filesystem_entry.name(), reference, context)?;
    tree.files = tree.files.saturating_add(1);
    tree.total_size = tree
        .total_size
        .saturating_add(filesystem_entry.metadata().size());
    context.progress.emit(1);
    state.completed = true;
    Ok(())
}

fn process_tree_non_file(
    message: EntryMessage,
    tree: &mut TreeBuildState,
    context: &TreeStageContext,
) -> Result<(), ()> {
    let EntryMessage {
        entry: filesystem_entry,
        memory: entry_memory,
        ..
    } = message;
    let name = filesystem_entry.name();
    let metadata = portable_metadata(filesystem_entry.metadata());
    match filesystem_entry.kind() {
        FilesystemEntryKind::Directory => {
            tree.directories.push(DirectoryAccumulator {
                path: filesystem_entry.path().clone(),
                metadata,
                entries: Vec::new(),
                memory: entry_memory,
            });
            Ok(())
        }
        FilesystemEntryKind::SymbolicLink => {
            let Some(target) = filesystem_entry.symlink_target().cloned() else {
                context.control.fail(BackupError::Invalid {
                    stage: BackupStage::Pack,
                });
                return Err(());
            };
            let node = TreeNode::SymbolicLink(SymbolicLinkNode::new(target, metadata));
            let reference = emit_tree_node(
                node,
                entry_memory,
                &context.upload_sender,
                &context.memory,
                &context.cpu_workers,
                &context.control,
            )?;
            add_tree_reference(tree, name, reference, context)
        }
        FilesystemEntryKind::RegularFile | FilesystemEntryKind::Other => {
            context.control.fail(BackupError::Invalid {
                stage: BackupStage::Pack,
            });
            Err(())
        }
    }
}

fn add_tree_reference(
    tree: &mut TreeBuildState,
    name: Option<EntryName>,
    reference: TreeNodeReference,
    context: &TreeStageContext,
) -> Result<(), ()> {
    let Some(parent) = tree.directories.last_mut() else {
        context.control.fail(BackupError::Invalid {
            stage: BackupStage::Pack,
        });
        return Err(());
    };
    add_directory_entry(parent, name, reference, &context.control)
}

fn process_tree_directory_end(
    path: crate::domain::RelativePath,
    end_memory: MemoryPermit,
    tree: &mut TreeBuildState,
    context: &TreeStageContext,
) -> Result<(), ()> {
    let Some(directory) = tree.directories.pop() else {
        context.control.fail(BackupError::Invalid {
            stage: BackupStage::Pack,
        });
        return Err(());
    };
    let DirectoryAccumulator {
        path: directory_path,
        metadata: directory_metadata,
        entries: directory_entries,
        memory: directory_memory,
    } = directory;
    if directory_path != path {
        context.control.fail(BackupError::Invalid {
            stage: BackupStage::Pack,
        });
        return Err(());
    }
    let estimate = directory_entries
        .iter()
        .fold(TREE_NODE_MEMORY_OVERHEAD, |total, entry| {
            total.saturating_add(entry.name().as_str().len() + 256)
        });
    let mut node_memory =
        match context
            .memory
            .reserve(estimate, &context.control, BackupStage::Pack)
        {
            Ok(memory) => memory,
            Err(error) => {
                context.control.fail(error);
                return Err(());
            }
        };
    let (kind, encoded) = {
        let _cpu = match UnitPermit::acquire(
            &context.cpu_workers,
            &context.control,
            BackupStage::Pack,
            BackupResource::CpuWorkers,
        ) {
            Ok(permit) => permit,
            Err(error) => {
                context.control.fail(error);
                return Err(());
            }
        };
        let node = match DirectoryNode::new(directory_metadata, directory_entries) {
            Ok(node) => TreeNode::Directory(node),
            Err(_) => {
                context.control.fail(BackupError::Invalid {
                    stage: BackupStage::Pack,
                });
                return Err(());
            }
        };
        let kind = node.kind();
        let encoded = encode_tree_node_with_id(&node);
        (kind, encoded)
    };
    let (id, bytes) = match encoded {
        Ok(value) => value,
        Err(_) => {
            context.control.fail(BackupError::Format {
                stage: BackupStage::Pack,
            });
            return Err(());
        }
    };
    let actual = bytes.capacity().max(bytes.len());
    if actual > estimate
        && node_memory
            .grow(
                actual.saturating_sub(estimate),
                &context.control,
                BackupStage::Pack,
            )
            .is_err()
    {
        context.control.fail(BackupError::Budget {
            stage: BackupStage::Pack,
            resource: BackupResource::Memory,
            requested: actual,
            limit: context.memory.limit,
        });
        return Err(());
    }
    let reference_id = id.clone();
    drop(directory_memory);
    drop(end_memory);
    if send_message(
        &context.upload_sender,
        UploadWork::Tree {
            id,
            bytes,
            memory: node_memory,
        },
        &context.control,
    )
    .is_err()
    {
        return Err(());
    }
    let reference = TreeNodeReference::new(reference_id, kind);
    if let Some(parent) = tree.directories.last_mut() {
        add_directory_entry(
            parent,
            path.file_name(),
            reference.clone(),
            &context.control,
        )?;
    } else if tree.root.is_some() {
        context.control.fail(BackupError::Invalid {
            stage: BackupStage::Pack,
        });
        return Err(());
    } else {
        tree.root = Some(reference);
    }
    tree.directory_count = tree.directory_count.saturating_add(1);
    context.progress.emit(1);
    Ok(())
}

fn emit_tree_node(
    node: TreeNode,
    input_memory: MemoryPermit,
    upload_sender: &SyncSender<UploadWork>,
    memory: &Arc<ResourceBudget>,
    cpu_workers: &Arc<ResourceBudget>,
    control: &PipelineControl,
) -> Result<TreeNodeReference, ()> {
    let estimate = tree_node_estimate(&node);
    let mut node_memory = match memory.reserve(estimate, control, BackupStage::Pack) {
        Ok(memory) => memory,
        Err(error) => {
            control.fail(error);
            return Err(());
        }
    };
    let encoded = {
        let _cpu = match UnitPermit::acquire(
            cpu_workers,
            control,
            BackupStage::Pack,
            BackupResource::CpuWorkers,
        ) {
            Ok(permit) => permit,
            Err(error) => {
                control.fail(error);
                return Err(());
            }
        };
        encode_tree_node_with_id(&node)
    };
    let (id, bytes) = match encoded {
        Ok(value) => value,
        Err(_) => {
            control.fail(BackupError::Format {
                stage: BackupStage::Pack,
            });
            return Err(());
        }
    };
    let actual = bytes.capacity().max(bytes.len());
    if actual > estimate
        && node_memory
            .grow(actual - estimate, control, BackupStage::Pack)
            .is_err()
    {
        control.fail(BackupError::Budget {
            stage: BackupStage::Pack,
            resource: BackupResource::Memory,
            requested: actual,
            limit: memory.limit,
        });
        return Err(());
    }
    let reference = TreeNodeReference::new(id.clone(), node.kind());
    if send_message(
        upload_sender,
        UploadWork::Tree {
            id,
            bytes,
            memory: node_memory,
        },
        control,
    )
    .is_err()
    {
        return Err(());
    }
    drop(input_memory);
    Ok(reference)
}

fn add_directory_entry(
    directory: &mut DirectoryAccumulator,
    name: Option<EntryName>,
    reference: TreeNodeReference,
    control: &PipelineControl,
) -> Result<(), ()> {
    let Some(name) = name else {
        control.fail(BackupError::Invalid {
            stage: BackupStage::Pack,
        });
        return Err(());
    };
    let name_bytes = name.as_str().len();
    let additional_memory = name_bytes
        .saturating_add(std::mem::size_of::<TreeEntry>())
        .saturating_add(64);
    if let Err(error) = directory
        .memory
        .grow(additional_memory, control, BackupStage::Pack)
    {
        control.fail(error);
        return Err(());
    }
    if directory.entries.try_reserve(1).is_err() {
        control.fail(BackupError::Format {
            stage: BackupStage::Pack,
        });
        return Err(());
    }
    let entry = match TreeEntry::new(name.as_str().to_owned(), reference) {
        Ok(entry) => entry,
        Err(_) => {
            control.fail(BackupError::Invalid {
                stage: BackupStage::Pack,
            });
            return Err(());
        }
    };
    directory.entries.push(entry);
    Ok(())
}

#[allow(clippy::too_many_arguments)]
fn run_pack_worker(
    receiver: Arc<Mutex<Receiver<PackMessage>>>,
    upload_sender: SyncSender<UploadWork>,
    index_sender: SyncSender<IndexMessage>,
    memory: Arc<ResourceBudget>,
    cpu_workers: Arc<ResourceBudget>,
    control: PipelineControl,
    metrics: Arc<MetricsState>,
    progress: ProgressReporter,
    configuration: PackConfiguration,
    transforms: ObjectTransformOptions,
) {
    let mut builder: Option<FormatPackBuilder> = None;
    let mut current_memory = None;
    let transform = match pack_index_transform(transforms) {
        Ok(transform) => transform,
        Err(error) => {
            control.fail(error);
            return;
        }
    };
    let pack_reserve = usize::try_from(configuration.max_size()).unwrap_or(usize::MAX);
    let mut expected_packs = 0_u64;
    let mut current_body_length = 0_u64;
    let mut current_entry_count = 0_u64;
    loop {
        let Some(message) = receive_message(&receiver, &control) else {
            break;
        };
        let entry_length = match pack_entry_frame_length(message.entry.payload().len()) {
            Some(length) => length,
            None => {
                control.fail(BackupError::Format {
                    stage: BackupStage::Pack,
                });
                return;
            }
        };
        if builder.is_none() {
            let permit = match memory.reserve(pack_reserve, &control, BackupStage::Pack) {
                Ok(permit) => permit,
                Err(error) => {
                    control.fail(error);
                    return;
                }
            };
            builder = Some(FormatPackBuilder::new(configuration));
            current_memory = Some(permit);
            current_body_length = PACK_HEADER_LENGTH as u64;
            current_entry_count = 0;
        }
        let would_cross = current_entry_count > 0
            && current_body_length
                .checked_add(entry_length)
                .and_then(|length| length.checked_add(PACK_FOOTER_LENGTH as u64))
                .is_some_and(|length| {
                    length > configuration.target_size() || length > configuration.max_size()
                });
        if would_cross {
            if seal_pack(
                &mut builder,
                &mut current_memory,
                &upload_sender,
                &control,
                &metrics,
                &progress,
                &cpu_workers,
                transform,
            )
            .is_err()
            {
                return;
            }
            expected_packs = expected_packs.saturating_add(1);
            current_entry_count = 0;
            let permit = match memory.reserve(pack_reserve, &control, BackupStage::Pack) {
                Ok(permit) => permit,
                Err(error) => {
                    control.fail(error);
                    return;
                }
            };
            builder = Some(FormatPackBuilder::new(configuration));
            current_memory = Some(permit);
            current_body_length = PACK_HEADER_LENGTH as u64;
        }
        let add_result = {
            let _cpu = match UnitPermit::acquire(
                &cpu_workers,
                &control,
                BackupStage::Pack,
                BackupResource::CpuWorkers,
            ) {
                Ok(permit) => permit,
                Err(error) => {
                    control.fail(error);
                    return;
                }
            };
            let Some(builder_ref) = builder.as_mut() else {
                control.fail(BackupError::Thread {
                    stage: BackupStage::Pack,
                });
                return;
            };
            builder_ref.add(message.entry)
        };
        match add_result {
            Ok(Some(_)) => {
                control.fail(BackupError::Thread {
                    stage: BackupStage::Pack,
                });
                return;
            }
            Ok(None) => {
                current_body_length = current_body_length.saturating_add(entry_length);
                current_entry_count = current_entry_count.saturating_add(1);
            }
            Err(_) => {
                control.fail(BackupError::Format {
                    stage: BackupStage::Pack,
                });
                return;
            }
        }
        drop(message.memory);
    }
    if control.is_cancelled() {
        return;
    }
    if current_entry_count > 0 {
        if seal_pack(
            &mut builder,
            &mut current_memory,
            &upload_sender,
            &control,
            &metrics,
            &progress,
            &cpu_workers,
            transform,
        )
        .is_err()
        {
            return;
        }
        expected_packs = expected_packs.saturating_add(1);
    }
    if send_message(
        &index_sender,
        IndexMessage::PacksFinished {
            expected: expected_packs,
        },
        &control,
    )
    .is_err()
        && !control.is_cancelled()
    {
        control.fail(BackupError::Thread {
            stage: BackupStage::Pack,
        });
    }
}

fn pack_entry_frame_length(payload_length: usize) -> Option<u64> {
    let raw = u64::try_from(PACK_ENTRY_HEADER_LENGTH)
        .ok()?
        .checked_add(u64::try_from(payload_length).ok()?)?;
    let aligned = raw.checked_add(PACK_ALIGNMENT - 1)? / PACK_ALIGNMENT * PACK_ALIGNMENT;
    Some(aligned)
}

#[allow(clippy::too_many_arguments)]
fn seal_pack(
    builder: &mut Option<FormatPackBuilder>,
    memory: &mut Option<MemoryPermit>,
    upload_sender: &SyncSender<UploadWork>,
    control: &PipelineControl,
    metrics: &MetricsState,
    progress: &ProgressReporter,
    cpu_workers: &Arc<ResourceBudget>,
    transform: PackIndexTransform,
) -> Result<(), ()> {
    let Some(mut current) = builder.take() else {
        return Ok(());
    };
    let pack = {
        let _cpu = match UnitPermit::acquire(
            cpu_workers,
            control,
            BackupStage::Pack,
            BackupResource::CpuWorkers,
        ) {
            Ok(permit) => permit,
            Err(error) => {
                control.fail(error);
                return Err(());
            }
        };
        match current.finish() {
            Ok(Some(pack)) => pack,
            Ok(None) => {
                drop(memory.take());
                return Ok(());
            }
            Err(_) => {
                control.fail(BackupError::Format {
                    stage: BackupStage::Pack,
                });
                return Err(());
            }
        }
    };
    let memory = memory.take();
    if send_pack(
        pack,
        memory,
        transform,
        upload_sender,
        control,
        metrics,
        progress,
    )
    .is_err()
    {
        return Err(());
    }
    Ok(())
}

fn send_pack(
    pack: SealedPack,
    memory: Option<MemoryPermit>,
    transform: PackIndexTransform,
    upload_sender: &SyncSender<UploadWork>,
    control: &PipelineControl,
    metrics: &MetricsState,
    progress: &ProgressReporter,
) -> Result<(), ()> {
    let Some(memory) = memory else {
        control.fail(BackupError::Thread {
            stage: BackupStage::Pack,
        });
        return Err(());
    };
    if send_message(
        upload_sender,
        UploadWork::Pack {
            pack,
            memory,
            transform,
        },
        control,
    )
    .is_err()
    {
        return Err(());
    }
    metrics.packs.fetch_add(1, Ordering::Relaxed);
    progress.emit(1);
    Ok(())
}

struct ImmutableObjectUploader {
    storage: Arc<dyn RepositoryStorage>,
    network_requests: Arc<ResourceBudget>,
    file_descriptors: Arc<ResourceBudget>,
    control: PipelineControl,
}

impl ImmutableObjectUploader {
    fn upload(
        &self,
        stage: BackupStage,
        operation: &'static str,
        key: &str,
        bytes: &[u8],
    ) -> Result<(), BackupError> {
        let _network = UnitPermit::acquire(
            &self.network_requests,
            &self.control,
            stage,
            BackupResource::NetworkRequests,
        )?;
        let _descriptor = UnitPermit::acquire(
            &self.file_descriptors,
            &self.control,
            stage,
            BackupResource::FileDescriptors,
        )?;
        let object_key = ObjectKey::new(key.to_owned()).map_err(|_| BackupError::Storage {
            stage,
            operation,
            error: StorageError::InvalidObjectKey,
        })?;
        let mut source = Cursor::new(bytes);
        let is_cancelled = || self.control.is_cancelled();
        match self.storage.write_stream_with_cancellation(
            &object_key,
            &mut source,
            ObjectWriteOptions::if_absent().with_expected_size(bytes.len() as u64),
            &is_cancelled,
        ) {
            Ok(_) | Err(StorageError::AlreadyExists) => Ok(()),
            Err(StorageError::Cancelled) => Err(BackupError::Cancelled),
            Err(error) => Err(BackupError::Storage {
                stage,
                operation,
                error,
            }),
        }
    }
}

struct UploadWorker {
    receiver: Arc<Mutex<Receiver<UploadWork>>>,
    index_sender: SyncSender<IndexMessage>,
    uploader: ImmutableObjectUploader,
    metrics: Arc<MetricsState>,
    progress: ProgressReporter,
}

fn run_upload_worker(worker: UploadWorker) {
    let UploadWorker {
        receiver,
        index_sender,
        uploader,
        metrics,
        progress,
    } = worker;
    let control = &uploader.control;
    loop {
        let Some(work) = receive_message(&receiver, control) else {
            return;
        };
        match work {
            UploadWork::Tree { id, bytes, memory } => {
                if let Err(error) = uploader.upload(
                    BackupStage::Upload,
                    "tree",
                    &format!("trees/{}", id.as_str()),
                    &bytes,
                ) {
                    drop(memory);
                    control.fail(error);
                    return;
                }
                metrics.uploaded_objects.fetch_add(1, Ordering::Relaxed);
                progress.emit(1);
                drop(memory);
            }
            UploadWork::Pack {
                pack,
                memory,
                transform,
            } => {
                let pack_id = pack.id();
                if let Err(error) = uploader.upload(
                    BackupStage::Upload,
                    "pack",
                    &format!("packs/{}", pack_id.as_hex()),
                    pack.as_bytes(),
                ) {
                    drop(memory);
                    control.fail(error);
                    return;
                }
                metrics.uploaded_objects.fetch_add(1, Ordering::Relaxed);
                progress.emit(1);
                for location in pack.entries() {
                    let entry = match PackIndexEntry::from_location(*location, transform) {
                        Ok(entry) => entry,
                        Err(_) => {
                            drop(memory);
                            control.fail(BackupError::Format {
                                stage: BackupStage::Index,
                            });
                            return;
                        }
                    };
                    if send_message(&index_sender, IndexMessage::PackEntry(entry), control).is_err()
                    {
                        drop(memory);
                        return;
                    }
                }
                if send_message(&index_sender, IndexMessage::PackUploaded, control).is_err() {
                    drop(memory);
                    return;
                }
                drop(memory);
            }
            UploadWork::Index { shard, memory } => {
                let key = format!("indexes/{}", shard.id().as_hex());
                if let Err(error) =
                    uploader.upload(BackupStage::Upload, "index", &key, shard.as_bytes())
                {
                    drop(memory);
                    control.fail(error);
                    return;
                }
                metrics.uploaded_objects.fetch_add(1, Ordering::Relaxed);
                progress.emit(1);
                drop(memory);
            }
        }
    }
}

#[allow(clippy::too_many_arguments)]
fn run_index_worker(
    receiver: Arc<Mutex<Receiver<IndexMessage>>>,
    upload_sender: SyncSender<UploadWork>,
    cpu_workers: Arc<ResourceBudget>,
    file_descriptors: Arc<ResourceBudget>,
    memory: Arc<ResourceBudget>,
    control: PipelineControl,
    metrics: Arc<MetricsState>,
    progress: ProgressReporter,
    configuration: PackIndexConfiguration,
) {
    let spool_descriptor = match UnitPermit::acquire(
        &file_descriptors,
        &control,
        BackupStage::Index,
        BackupResource::FileDescriptors,
    ) {
        Ok(permit) => permit,
        Err(error) => {
            control.fail(error);
            return;
        }
    };
    let mut spool = match IndexSpool::create() {
        Ok(spool) => spool,
        Err(_) => {
            drop(spool_descriptor);
            control.fail(BackupError::Storage {
                stage: BackupStage::Index,
                operation: "create_index_spool",
                error: StorageError::Io,
            });
            return;
        }
    };
    let mut expected = None;
    let mut uploaded = 0_u64;
    while let Some(message) = receive_message(&receiver, &control) {
        match message {
            IndexMessage::PackEntry(entry) => {
                if spool.append(entry).is_err() {
                    control.fail(BackupError::Storage {
                        stage: BackupStage::Index,
                        operation: "write_index_spool",
                        error: StorageError::Io,
                    });
                    return;
                }
            }
            IndexMessage::PackUploaded => {
                uploaded = uploaded.saturating_add(1);
            }
            IndexMessage::PacksFinished { expected: count } => {
                expected = Some(count);
            }
        }
        if expected.is_some_and(|count| uploaded >= count) {
            break;
        }
    }
    let Some(expected) = expected else {
        if !control.is_cancelled() {
            control.fail(BackupError::Thread {
                stage: BackupStage::Index,
            });
        }
        return;
    };
    if uploaded != expected {
        control.fail(BackupError::Thread {
            stage: BackupStage::Index,
        });
        return;
    }
    if spool.flush().is_err() {
        control.fail(BackupError::Storage {
            stage: BackupStage::Index,
            operation: "flush_index_spool",
            error: StorageError::Io,
        });
        return;
    }
    for raw_shard in 0_u16..=u8::MAX as u16 {
        if control.check().is_err() {
            return;
        }
        let shard_id = PackIndexShardId::from_byte(raw_shard as u8);
        let shard_memory_size = usize::try_from(configuration.max_shard_bytes())
            .unwrap_or(usize::MAX)
            .saturating_mul(INDEX_MEMORY_SAFETY_MULTIPLIER);
        let shard_memory = match memory.reserve(shard_memory_size, &control, BackupStage::Index) {
            Ok(memory) => memory,
            Err(error) => {
                control.fail(error);
                return;
            }
        };
        let entry_capacity = match spool.shard_entry_count(shard_id) {
            Ok(count) => count,
            Err(_) => {
                drop(shard_memory);
                control.fail(BackupError::Storage {
                    stage: BackupStage::Index,
                    operation: "stat_index_spool",
                    error: StorageError::Io,
                });
                return;
            }
        };
        if entry_capacity == 0 {
            drop(shard_memory);
            continue;
        }
        let mut entries = Vec::new();
        if entries.try_reserve(entry_capacity).is_err() {
            drop(shard_memory);
            control.fail(BackupError::Format {
                stage: BackupStage::Index,
            });
            return;
        }
        if spool.read_shard(shard_id, &mut entries).is_err() {
            drop(shard_memory);
            control.fail(BackupError::Storage {
                stage: BackupStage::Index,
                operation: "read_index_spool",
                error: StorageError::Io,
            });
            return;
        }
        let _sort_cpu = match UnitPermit::acquire(
            &cpu_workers,
            &control,
            BackupStage::Index,
            BackupResource::CpuWorkers,
        ) {
            Ok(permit) => permit,
            Err(error) => {
                drop(shard_memory);
                control.fail(error);
                return;
            }
        };
        entries.sort_unstable_by_key(|entry| {
            (entry.chunk_id(), entry.pack_id(), entry.entry_offset())
        });
        entries.dedup_by(|left, right| left.chunk_id() == right.chunk_id());
        drop(_sort_cpu);
        if entries.is_empty() {
            drop(shard_memory);
            continue;
        }
        let _build_cpu = match UnitPermit::acquire(
            &cpu_workers,
            &control,
            BackupStage::Index,
            BackupResource::CpuWorkers,
        ) {
            Ok(permit) => permit,
            Err(error) => {
                drop(shard_memory);
                control.fail(error);
                return;
            }
        };
        let mut builder = match FormatPackIndexShardBuilder::new(configuration, shard_id) {
            Ok(builder) => builder,
            Err(_) => {
                drop(shard_memory);
                control.fail(BackupError::Format {
                    stage: BackupStage::Index,
                });
                return;
            }
        };
        for entry in entries {
            if builder.add(entry).is_err() {
                drop(shard_memory);
                control.fail(BackupError::Format {
                    stage: BackupStage::Index,
                });
                return;
            }
        }
        let shard = match builder.finish() {
            Ok(shard) => shard,
            Err(_) => {
                drop(shard_memory);
                control.fail(BackupError::Format {
                    stage: BackupStage::Index,
                });
                return;
            }
        };
        metrics.index_shards.fetch_add(1, Ordering::Relaxed);
        if send_message(
            &upload_sender,
            UploadWork::Index {
                shard,
                memory: shard_memory,
            },
            &control,
        )
        .is_err()
        {
            return;
        }
        progress.emit(1);
    }
    drop(spool_descriptor);
}

struct IndexSpool {
    directory: PathBuf,
    active_shard: Option<PackIndexShardId>,
    active_file: Option<File>,
}

impl IndexSpool {
    fn create() -> io::Result<Self> {
        let base = std::env::temp_dir();
        let process = std::process::id();
        let mut directory = None;
        for _ in 0..INDEX_SPOOL_DIRECTORY_ATTEMPTS {
            let id = NEXT_INDEX_SPOOL_ID.fetch_add(1, Ordering::Relaxed);
            let candidate = base.join(format!("gib-backup-index-{process}-{id}"));
            match fs::create_dir(&candidate) {
                Ok(()) => {
                    directory = Some(candidate);
                    break;
                }
                Err(error) if error.kind() == io::ErrorKind::AlreadyExists => continue,
                Err(error) => return Err(error),
            }
        }
        let Some(directory) = directory else {
            return Err(io::Error::new(
                io::ErrorKind::AlreadyExists,
                "could not allocate a unique backup index spool directory",
            ));
        };
        Ok(Self {
            directory,
            active_shard: None,
            active_file: None,
        })
    }

    fn append(&mut self, entry: PackIndexEntry) -> io::Result<()> {
        let shard = PackIndexShardId::from_chunk_id(entry.chunk_id());
        if self.active_shard != Some(shard) {
            self.active_file = None;
            let path = self.shard_path(shard);
            let file = OpenOptions::new().create(true).append(true).open(path)?;
            self.active_shard = Some(shard);
            self.active_file = Some(file);
        }
        let mut record = [0_u8; INDEX_SPOOL_RECORD_BYTES];
        encode_index_record(&mut record, entry);
        let Some(file) = self.active_file.as_mut() else {
            return Err(io::Error::other("index spool shard is not open"));
        };
        file.write_all(&record)
    }

    fn flush(&mut self) -> io::Result<()> {
        if let Some(file) = self.active_file.as_mut() {
            file.flush()?;
        }
        Ok(())
    }

    fn read_shard(
        &mut self,
        shard_id: PackIndexShardId,
        entries: &mut Vec<PackIndexEntry>,
    ) -> io::Result<()> {
        self.active_file = None;
        let path = self.shard_path(shard_id);
        let mut file = OpenOptions::new().read(true).open(path)?;
        let mut record = [0_u8; INDEX_SPOOL_RECORD_BYTES];
        loop {
            let mut filled = 0;
            while filled < record.len() {
                let read = file.read(&mut record[filled..])?;
                if read == 0 {
                    if filled == 0 {
                        return Ok(());
                    }
                    return Err(io::Error::new(
                        io::ErrorKind::InvalidData,
                        "index spool shard is truncated",
                    ));
                }
                filled += read;
            }
            let entry = decode_index_record(&record)?;
            if PackIndexShardId::from_chunk_id(entry.chunk_id()) == shard_id {
                entries.push(entry);
            }
        }
    }

    fn shard_entry_count(&self, shard_id: PackIndexShardId) -> io::Result<usize> {
        let path = self.shard_path(shard_id);
        let metadata = match fs::metadata(path) {
            Ok(metadata) => metadata,
            Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(0),
            Err(error) => return Err(error),
        };
        let length = metadata.len();
        let record_length = u64::try_from(INDEX_SPOOL_RECORD_BYTES).map_err(|_| {
            io::Error::new(
                io::ErrorKind::InvalidData,
                "index spool record size is not representable",
            )
        })?;
        if !length.is_multiple_of(record_length) {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "index spool shard has a partial record",
            ));
        }
        usize::try_from(length / record_length).map_err(|_| {
            io::Error::new(
                io::ErrorKind::InvalidData,
                "index spool shard has too many records",
            )
        })
    }

    fn shard_path(&self, shard_id: PackIndexShardId) -> PathBuf {
        self.directory
            .join(format!("shard-{:02x}.bin", shard_id.as_byte()))
    }
}

impl Drop for IndexSpool {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.directory);
    }
}

fn encode_index_record(record: &mut [u8; INDEX_SPOOL_RECORD_BYTES], entry: PackIndexEntry) {
    record[0..32].copy_from_slice(&entry.chunk_id().as_bytes());
    record[32..64].copy_from_slice(&entry.pack_id().as_bytes());
    record[64..72].copy_from_slice(&entry.entry_offset().to_be_bytes());
    record[72..80].copy_from_slice(&entry.payload_offset().to_be_bytes());
    record[80..88].copy_from_slice(&entry.entry_length().to_be_bytes());
    record[88..96].copy_from_slice(&entry.stored_length().to_be_bytes());
    record[96..104].copy_from_slice(&entry.logical_length().to_be_bytes());
    let transform = entry.transform();
    record[104..106].copy_from_slice(&transform.envelope_version().to_be_bytes());
    record[106..108].copy_from_slice(&transform.object_version().to_be_bytes());
    record[108] = match transform.codec() {
        ObjectCodec::None => 0,
        ObjectCodec::Zstd => 1,
    };
    record[109] = match transform.encryption() {
        ObjectEncryption::None => 0,
        ObjectEncryption::XChaCha20Poly1305 => 1,
    };
    let compression_level = if transform.codec() == ObjectCodec::None {
        0
    } else {
        transform.compression_level().value()
    };
    record[112..116].copy_from_slice(&compression_level.to_be_bytes());
}

fn decode_index_record(record: &[u8; INDEX_SPOOL_RECORD_BYTES]) -> io::Result<PackIndexEntry> {
    let mut chunk_id = [0_u8; 32];
    chunk_id.copy_from_slice(&record[0..32]);
    let mut pack_id = [0_u8; 32];
    pack_id.copy_from_slice(&record[32..64]);
    let envelope_version = u16::from_be_bytes([record[104], record[105]]);
    let object_version = u16::from_be_bytes([record[106], record[107]]);
    let codec = match record[108] {
        0 => ObjectCodec::None,
        1 => ObjectCodec::Zstd,
        _ => {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "invalid index codec",
            ));
        }
    };
    let encryption = match record[109] {
        0 => ObjectEncryption::None,
        1 => ObjectEncryption::XChaCha20Poly1305,
        _ => {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "invalid index encryption",
            ));
        }
    };
    let raw_level = i32::from_be_bytes([record[112], record[113], record[114], record[115]]);
    let compression_level = if codec == ObjectCodec::None {
        CompressionLevel::DEFAULT
    } else {
        CompressionLevel::new(raw_level)
            .map_err(|_| io::Error::new(io::ErrorKind::InvalidData, "invalid index level"))?
    };
    let options =
        ObjectTransformOptions::new(codec, encryption).with_compression_level(compression_level);
    let transform =
        crate::domain::PackIndexTransform::new(envelope_version, object_version, options)
            .map_err(|_| io::Error::new(io::ErrorKind::InvalidData, "invalid index transform"))?;
    PackIndexEntry::new(
        ChunkId::from_digest(chunk_id),
        crate::domain::PackId::from_digest(pack_id),
        u64::from_be_bytes(record[64..72].try_into().map_err(|_| {
            io::Error::new(io::ErrorKind::InvalidData, "invalid index entry offset")
        })?),
        u64::from_be_bytes(record[72..80].try_into().map_err(|_| {
            io::Error::new(io::ErrorKind::InvalidData, "invalid index payload offset")
        })?),
        u64::from_be_bytes(record[80..88].try_into().map_err(|_| {
            io::Error::new(io::ErrorKind::InvalidData, "invalid index entry length")
        })?),
        u64::from_be_bytes(record[88..96].try_into().map_err(|_| {
            io::Error::new(io::ErrorKind::InvalidData, "invalid index stored length")
        })?),
        u64::from_be_bytes(record[96..104].try_into().map_err(|_| {
            io::Error::new(io::ErrorKind::InvalidData, "invalid index logical length")
        })?),
        transform,
    )
    .map_err(|_| io::Error::new(io::ErrorKind::InvalidData, "invalid index entry"))
}

fn tree_message_sequence(message: &TreeMessage) -> u64 {
    match message {
        TreeMessage::Entry(message) => message.sequence,
        TreeMessage::Chunk { sequence, .. }
        | TreeMessage::FileEnd { sequence, .. }
        | TreeMessage::DirectoryEnd { sequence, .. } => *sequence,
    }
}

fn read_current_head(
    storage: &dyn RepositoryStorage,
    network_requests: &Arc<ResourceBudget>,
    file_descriptors: &Arc<ResourceBudget>,
    memory: &Arc<ResourceBudget>,
    cpu_workers: &Arc<ResourceBudget>,
    control: &PipelineControl,
) -> Result<HeadRead, BackupError> {
    let _memory = memory.reserve(4 * 1024, control, BackupStage::Publish)?;
    let _network = UnitPermit::acquire(
        network_requests,
        control,
        BackupStage::Publish,
        BackupResource::NetworkRequests,
    )?;
    let _descriptor = UnitPermit::acquire(
        file_descriptors,
        control,
        BackupStage::Publish,
        BackupResource::FileDescriptors,
    )?;
    let _cpu = UnitPermit::acquire(
        cpu_workers,
        control,
        BackupStage::Publish,
        BackupResource::CpuWorkers,
    )?;
    repository::read_head(storage)
        .map_err(|error| map_repository_error(BackupStage::Publish, error))
}

fn map_repository_error(stage: BackupStage, error: RepositoryError) -> BackupError {
    let failure = match error {
        RepositoryError::AlreadyExists => BackupRepositoryFailure::AlreadyExists,
        RepositoryError::Missing => BackupRepositoryFailure::Missing,
        RepositoryError::Malformed { .. } => BackupRepositoryFailure::Malformed,
        RepositoryError::UnsupportedVersion { version } => {
            BackupRepositoryFailure::UnsupportedVersion { version }
        }
        RepositoryError::Incompatible { .. } => BackupRepositoryFailure::Incompatible,
        RepositoryError::PublicationConflict => BackupRepositoryFailure::PublicationConflict,
        RepositoryError::SnapshotMissing => BackupRepositoryFailure::SnapshotMissing,
        RepositoryError::RequiredObjectMissing => BackupRepositoryFailure::RequiredObjectMissing,
        RepositoryError::InvalidPublication { .. } => BackupRepositoryFailure::InvalidPublication,
        RepositoryError::GenerationExhausted => BackupRepositoryFailure::GenerationExhausted,
        RepositoryError::UnsupportedCapability => BackupRepositoryFailure::UnsupportedCapability,
        RepositoryError::Cancelled => BackupRepositoryFailure::Cancelled,
        RepositoryError::NoSnapshots => BackupRepositoryFailure::NoSnapshots,
        RepositoryError::SnapshotReferenceEmpty
        | RepositoryError::SnapshotReferenceMalformed
        | RepositoryError::SnapshotReferenceNotFound
        | RepositoryError::SnapshotReferenceAmbiguous
        | RepositoryError::SnapshotHistoryRequestInvalid
        | RepositoryError::SnapshotHistoryCursorInvalid => {
            BackupRepositoryFailure::SnapshotReference
        }
        RepositoryError::Storage { .. } => BackupRepositoryFailure::Storage,
    };
    if matches!(failure, BackupRepositoryFailure::Cancelled) {
        BackupError::Cancelled
    } else {
        BackupError::Repository { stage, failure }
    }
}

fn filesystem_error(stage: BackupStage, error: &crate::domain::FilesystemScanError) -> BackupError {
    let (operation, kind) = match error {
        crate::domain::FilesystemScanError::RootIo { operation, kind }
        | crate::domain::FilesystemScanError::Io {
            operation, kind, ..
        } => (Some(*operation), Some(*kind)),
        _ => (None, error.error_kind()),
    };
    BackupError::Filesystem {
        stage,
        operation,
        kind,
        race: error.is_race(),
    }
}

fn portable_metadata(metadata: &FilesystemMetadata) -> PortableMetadata {
    let permissions = metadata.permissions().unwrap_or_default();
    let mut portable = PortableMetadata::new(permissions);
    if let Some(modified_at) = metadata.modified_at() {
        portable = portable.with_modified_at(modified_at);
    }
    portable
}

fn entry_memory_size(entry: &FilesystemEntry) -> usize {
    entry
        .path()
        .as_str()
        .len()
        .saturating_add(
            entry
                .symlink_target()
                .map_or(0, |target| target.as_bytes().len()),
        )
        .saturating_add(SCAN_ENTRY_OVERHEAD)
}

fn chunker_memory_size(configuration: ChunkingConfiguration) -> usize {
    let target = usize::try_from(configuration.target_size()).unwrap_or(usize::MAX);
    crate::domain::CHUNKING_READ_BUFFER_SIZE
        .saturating_add(
            configuration
                .max_size_usize()
                .saturating_mul(CHUNK_BUFFER_SAFETY_MULTIPLIER),
        )
        .saturating_add(target)
        .saturating_add(CHUNK_MESSAGE_OVERHEAD)
}

fn tree_node_estimate(node: &TreeNode) -> usize {
    let base = TREE_NODE_MEMORY_OVERHEAD;
    match node {
        TreeNode::Directory(node) => node.entries().iter().fold(base, |total, entry| {
            total.saturating_add(entry.name().as_str().len() + 256)
        }),
        TreeNode::RegularFile(node) => base.saturating_add(node.chunks().len() * 96),
        TreeNode::SymbolicLink(node) => base.saturating_add(node.target().as_bytes().len() + 256),
    }
}

fn transform_bound(length: usize, options: ObjectTransformOptions) -> usize {
    let envelope =
        if options.codec() == ObjectCodec::None && options.encryption() == ObjectEncryption::None {
            4 * 1024
        } else {
            1024 * 1024 + 16 * 1024
        };
    length
        .saturating_add(envelope)
        .min(MAX_IMMUTABLE_OBJECT_BYTES)
}

fn pack_index_transform(
    options: ObjectTransformOptions,
) -> Result<PackIndexTransform, BackupError> {
    let envelope_version =
        if options.codec() == ObjectCodec::None && options.encryption() == ObjectEncryption::None {
            CURRENT_OBJECT_ENVELOPE_VERSION
        } else {
            CURRENT_TRANSFORMED_OBJECT_ENVELOPE_VERSION
        };
    PackIndexTransform::new(envelope_version, CURRENT_PACK_OBJECT_VERSION, options).map_err(|_| {
        BackupError::Invalid {
            stage: BackupStage::Index,
        }
    })
}

fn snapshot_id_for(
    root: &TreeNodeReference,
    request: &BackupRunRequest,
    head: &RepositoryHead,
    created_at: u64,
) -> Result<SnapshotId, BackupError> {
    let mut hasher = Sha256::new();
    hasher.update(b"GIB backup snapshot identity\0");
    hasher.update(root.id().as_digest());
    hasher.update(created_at.to_be_bytes());
    hasher.update(request.message.as_bytes());
    if let Some(author) = &request.author {
        hasher.update(author.as_bytes());
    }
    if let Some(parent) = head.snapshot() {
        hasher.update(parent.as_str().as_bytes());
    }
    let digest = hasher.finalize();
    let id = digest
        .iter()
        .fold(String::with_capacity(64), |mut id, byte| {
            id.push(char::from(b"0123456789abcdef"[(byte >> 4) as usize]));
            id.push(char::from(b"0123456789abcdef"[(byte & 0x0f) as usize]));
            id
        });
    SnapshotId::new(id).map_err(|_| BackupError::Invalid {
        stage: BackupStage::Publish,
    })
}

fn current_unix_seconds() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |duration| duration.as_secs())
}

fn is_ancestor(
    directory: &crate::domain::RelativePath,
    path: &crate::domain::RelativePath,
) -> bool {
    directory.is_root()
        || directory == path
        || path
            .as_str()
            .strip_prefix(directory.as_str())
            .is_some_and(|suffix| suffix.starts_with('/'))
}

fn send_message<T>(sender: &SyncSender<T>, value: T, control: &PipelineControl) -> Result<(), ()> {
    let mut pending = Some(value);
    loop {
        if control.is_cancelled() {
            return Err(());
        }
        let Some(value) = pending.take() else {
            return Ok(());
        };
        match sender.try_send(value) {
            Ok(()) => return Ok(()),
            Err(TrySendError::Full(value)) => {
                pending = Some(value);
                control.wait();
            }
            Err(TrySendError::Disconnected(_)) => return Err(()),
        }
    }
}

fn receive_message<T>(receiver: &Arc<Mutex<Receiver<T>>>, control: &PipelineControl) -> Option<T> {
    loop {
        if control.is_cancelled() {
            return None;
        }
        let result = match receiver.lock() {
            Ok(guard) => guard.recv_timeout(CHANNEL_WAIT),
            Err(poisoned) => poisoned.into_inner().recv_timeout(CHANNEL_WAIT),
        };
        match result {
            Ok(message) => return Some(message),
            Err(RecvTimeoutError::Timeout) => continue,
            Err(RecvTimeoutError::Disconnected) => return None,
        }
    }
}

fn spawn_stage<FN>(
    handles: &mut Vec<JoinHandle<()>>,
    name: &str,
    stage: BackupStage,
    function: FN,
    control: &PipelineControl,
) -> Result<(), BackupError>
where
    FN: FnOnce() + Send + 'static,
{
    let worker_control = control.clone();
    match thread::Builder::new().name(name.to_owned()).spawn(move || {
        if catch_unwind(AssertUnwindSafe(function)).is_err() {
            worker_control.fail(BackupError::Thread { stage });
        }
    }) {
        Ok(handle) => {
            handles.push(handle);
            Ok(())
        }
        Err(_) => {
            control.fail(BackupError::Thread { stage });
            for handle in handles.drain(..) {
                let _ = handle.join();
            }
            Err(BackupError::Thread { stage })
        }
    }
}

fn update_peak(peak: &AtomicUsize, value: usize) {
    let mut current = peak.load(Ordering::Relaxed);
    while value > current {
        match peak.compare_exchange_weak(current, value, Ordering::AcqRel, Ordering::Relaxed) {
            Ok(_) => break,
            Err(observed) => current = observed,
        }
    }
}

fn lock_or_recover<T>(mutex: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    match mutex.lock() {
        Ok(guard) => guard,
        Err(poisoned) => poisoned.into_inner(),
    }
}
