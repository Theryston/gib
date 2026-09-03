use gib::{
    BackupBudgetError, BackupBudgets, BackupDeduplicationConfiguration, BackupRequest, BackupStage,
    ChunkingConfiguration, Client, ErrorCode, MAX_BACKUP_CONCURRENCY, MemoryStorage,
    MemoryStorageOperation, ObjectKey, ObjectRead, ObjectWriteOptions, PackConfiguration,
    PackIndexCacheConfiguration, PackIndexConfiguration, PackReader, RepositoryIdentity,
    RepositoryInitRequest, RepositoryKey, RepositoryStorage, SdkError, StorageCapabilities,
    StorageError,
};
use std::error::Error;
use std::fs;
use std::io::Read;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use std::thread;
use std::time::{Duration, Instant};

static NEXT_BACKUP_TEST_DIRECTORY: AtomicU64 = AtomicU64::new(1);

struct TestDirectory {
    path: PathBuf,
}

impl TestDirectory {
    fn new() -> Result<Self, Box<dyn Error>> {
        let id = NEXT_BACKUP_TEST_DIRECTORY.fetch_add(1, Ordering::Relaxed);
        for attempt in 0..16 {
            let path = std::env::temp_dir().join(format!(
                "gib-backup-pipeline-{}-{id}-{attempt}",
                std::process::id()
            ));
            match fs::create_dir(&path) {
                Ok(()) => return Ok(Self { path }),
                Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => continue,
                Err(error) => return Err(error.into()),
            }
        }
        Err(std::io::Error::new(
            std::io::ErrorKind::AlreadyExists,
            "could not allocate a unique backup test directory",
        )
        .into())
    }

    fn path(&self) -> &Path {
        &self.path
    }
}

impl Drop for TestDirectory {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.path);
    }
}

fn small_request(root: &Path) -> Result<BackupRequest, Box<dyn Error>> {
    Ok(BackupRequest::new(root)
        .with_message("bounded test")
        .with_budgets(BackupBudgets::with_queue_capacity(
            2 * 1024 * 1024,
            3,
            8,
            1,
            1,
        )?)
        .with_chunking(ChunkingConfiguration::new(16, 32, 64)?)
        .with_pack_configuration(PackConfiguration::new(4096, 8192)?)
        .with_index_configuration(PackIndexConfiguration::new(512)?))
}

fn repository(client: &Client, storage: &MemoryStorage) -> Result<gib::Repository, Box<dyn Error>> {
    Ok(client.initialize_repository(
        storage.clone(),
        RepositoryInitRequest::new(
            RepositoryIdentity::new("backup-pipeline-test")?,
            RepositoryKey::new("test")?,
        ),
    )?)
}

fn object_keys(storage: &MemoryStorage, prefix: &str) -> Result<Vec<String>, Box<dyn Error>> {
    Ok(storage
        .objects()?
        .into_iter()
        .filter(|key| key.starts_with(prefix))
        .collect())
}

fn immutable_object_bytes(storage: &MemoryStorage) -> Result<u64, Box<dyn Error>> {
    object_keys(storage, "")?
        .into_iter()
        .filter(|key| {
            key.starts_with("packs/")
                || key.starts_with("indexes/")
                || key.starts_with("trees/")
                || key.starts_with("snapshots/")
        })
        .try_fold(0_u64, |total, key| {
            let length = u64::try_from(storage.read_object(&key)?.len())?;
            Ok(total.saturating_add(length))
        })
}

fn immutable_object_count(storage: &MemoryStorage) -> Result<usize, Box<dyn Error>> {
    Ok(storage
        .objects()?
        .into_iter()
        .filter(|key| {
            key.starts_with("packs/")
                || key.starts_with("indexes/")
                || key.starts_with("trees/")
                || key.starts_with("snapshots/")
        })
        .count())
}

fn pack_entry_count(storage: &MemoryStorage) -> Result<usize, Box<dyn Error>> {
    object_keys(storage, "packs/")?
        .into_iter()
        .try_fold(0_usize, |count, key| {
            let bytes = storage.read_object(&key)?;
            let pack = PackReader::new(&bytes)?;
            Ok(count.saturating_add(pack.entries().len()))
        })
}

fn deterministic_payload(length: usize) -> Vec<u8> {
    (0..length)
        .map(|index| {
            let value = index
                .wrapping_mul(37)
                .wrapping_add(index / 97)
                .wrapping_add(index / 4093);
            (value % 251) as u8
        })
        .collect()
}

fn incremental_request(root: &Path) -> Result<BackupRequest, Box<dyn Error>> {
    Ok(BackupRequest::new(root)
        .with_budgets(BackupBudgets::with_queue_capacity(
            32 * 1024 * 1024,
            3,
            8,
            1,
            2,
        )?)
        .with_chunking(ChunkingConfiguration::new(256, 512, 1024)?)
        .with_pack_configuration(PackConfiguration::new(64 * 1024, 128 * 1024)?)
        .with_index_configuration(PackIndexConfiguration::new(4096)?))
}

fn assert_dedup_error(error: SdkError, expected: ErrorCode) {
    match error {
        SdkError::BackupStageFailed { stage, source } => {
            assert_eq!(stage, BackupStage::Dedup);
            assert_eq!(source.code(), expected);
        }
        other => panic!("expected a deduplication stage failure, got {other:?}"),
    }
}

#[test]
fn identical_snapshot_reuses_content_and_writes_no_content_packs() -> Result<(), Box<dyn Error>> {
    let source = TestDirectory::new()?;
    fs::write(
        source.path().join("payload.bin"),
        deterministic_payload(96 * 1024),
    )?;
    let storage = MemoryStorage::new();
    let client = Client::default();
    let repository = repository(&client, &storage)?;
    let request = incremental_request(source.path())?;

    let first = client.backup(repository.clone(), request.clone())?;
    let packs_after_first = object_keys(&storage, "packs/")?;
    let bytes_after_first = immutable_object_bytes(&storage)?;
    let objects_after_first = immutable_object_count(&storage)?;
    assert_eq!(first.metrics().new_stored_bytes(), bytes_after_first);
    assert_eq!(
        first.metrics().uploaded_objects() as usize,
        objects_after_first
    );
    let second = client.backup(repository, request)?;

    assert!(first.metrics().new_stored_bytes() > 0);
    assert_eq!(second.metrics().packs(), 0);
    assert_eq!(second.metrics().index_shards(), 0);
    assert_eq!(second.metrics().transformed_chunks(), 0);
    assert_eq!(
        second.metrics().logical_bytes(),
        second.metrics().reused_bytes()
    );
    assert_eq!(object_keys(&storage, "packs/")?, packs_after_first);
    assert_eq!(second.metrics().uploaded_objects(), 1);
    assert_eq!(
        second.metrics().new_stored_bytes(),
        immutable_object_bytes(&storage)?.saturating_sub(bytes_after_first)
    );
    assert_eq!(
        second.metrics().uploaded_objects() as usize,
        immutable_object_count(&storage)?.saturating_sub(objects_after_first)
    );
    Ok(())
}

#[test]
fn duplicate_files_reuse_chunks_within_one_snapshot() -> Result<(), Box<dyn Error>> {
    let source = TestDirectory::new()?;
    let payload = deterministic_payload(96 * 1024);
    fs::write(source.path().join("one.bin"), &payload)?;
    fs::write(source.path().join("two.bin"), &payload)?;
    let storage = MemoryStorage::new();
    let client = Client::default();
    let repository = repository(&client, &storage)?;

    let result = client.backup(repository, incremental_request(source.path())?)?;

    assert_eq!(result.metrics().logical_bytes(), (payload.len() * 2) as u64);
    assert!(result.metrics().reused_bytes() >= payload.len() as u64);
    assert!(result.metrics().reused_bytes() < result.metrics().logical_bytes());
    assert!(result.metrics().chunks() > 2);
    assert!(pack_entry_count(&storage)? < result.metrics().chunks() as usize);
    Ok(())
}

#[test]
fn shifted_file_reuses_later_content_defined_chunks() -> Result<(), Box<dyn Error>> {
    let source = TestDirectory::new()?;
    let original = deterministic_payload(512 * 1024);
    fs::write(source.path().join("payload.bin"), &original)?;
    let storage = MemoryStorage::new();
    let client = Client::default();
    let repository = repository(&client, &storage)?;
    let request = incremental_request(source.path())?;
    client.backup(repository.clone(), request.clone())?;

    let mut shifted = b"inserted near the beginning".to_vec();
    shifted.extend_from_slice(&original);
    fs::write(source.path().join("payload.bin"), shifted)?;
    let result = client.backup(repository, request)?;

    assert!(result.metrics().reused_bytes() > (original.len() as u64 / 2));
    assert!(result.metrics().packs() > 0);
    assert!(result.metrics().transformed_chunks() < result.metrics().chunks());
    Ok(())
}

#[test]
fn one_leaf_edit_reuses_unchanged_tree_subtrees() -> Result<(), Box<dyn Error>> {
    let source = TestDirectory::new()?;
    fs::create_dir(source.path().join("left"))?;
    fs::create_dir(source.path().join("right"))?;
    fs::write(source.path().join("left/a.txt"), b"left-a")?;
    fs::write(source.path().join("left/b.txt"), b"left-b")?;
    fs::write(source.path().join("right/c.txt"), b"right-c")?;
    let storage = MemoryStorage::new();
    let client = Client::default();
    let repository = repository(&client, &storage)?;
    let request = incremental_request(source.path())?;
    client.backup(repository.clone(), request.clone())?;
    let first_trees = object_keys(&storage, "trees/")?;

    fs::write(source.path().join("left/a.txt"), b"left-a-edited")?;
    let result = client.backup(repository, request)?;
    let second_trees = object_keys(&storage, "trees/")?;
    let new_trees = second_trees
        .iter()
        .filter(|key| !first_trees.contains(key))
        .count();

    assert!(result.metrics().reused_bytes() > 0);
    assert!(new_trees < first_trees.len());
    assert!(result.metrics().packs() > 0);
    Ok(())
}

#[test]
fn missing_referenced_pack_fails_closed_during_deduplication() -> Result<(), Box<dyn Error>> {
    let source = TestDirectory::new()?;
    fs::write(
        source.path().join("payload.bin"),
        deterministic_payload(96 * 1024),
    )?;
    let storage = MemoryStorage::new();
    let client = Client::default();
    let repository = repository(&client, &storage)?;
    let request = incremental_request(source.path())?;
    client.backup(repository.clone(), request.clone())?;
    let pack = object_keys(&storage, "packs/")?
        .into_iter()
        .next()
        .ok_or("first backup did not create a pack")?;
    assert!(storage.remove_object(&pack)?);

    let error = client
        .backup(repository, request)
        .expect_err("a missing indexed pack must not be treated as new content");
    assert_dedup_error(error, ErrorCode::RepositoryRequiredObjectMissing);
    Ok(())
}

#[test]
fn corrupt_referenced_index_fails_closed_during_deduplication() -> Result<(), Box<dyn Error>> {
    let source = TestDirectory::new()?;
    fs::write(
        source.path().join("payload.bin"),
        deterministic_payload(96 * 1024),
    )?;
    let storage = MemoryStorage::new();
    let client = Client::default();
    let repository = repository(&client, &storage)?;
    let request = incremental_request(source.path())?;
    client.backup(repository.clone(), request.clone())?;
    let index = object_keys(&storage, "indexes/")?
        .into_iter()
        .find(|key| !key.contains("/pack-v1/"))
        .ok_or("first backup did not create an immutable index")?;
    let mut bytes = storage.read_object(&index)?;
    let first = bytes.first_mut().ok_or("index object is empty")?;
    *first ^= 0xff;
    storage.replace_object(&index, bytes)?;

    let error = client
        .backup(repository, request)
        .expect_err("a corrupt referenced index must not be ignored");
    assert_dedup_error(error, ErrorCode::RepositoryMalformed);
    Ok(())
}

#[test]
fn deduplication_respects_a_one_shard_index_cache() -> Result<(), Box<dyn Error>> {
    let source = TestDirectory::new()?;
    fs::write(
        source.path().join("payload.bin"),
        deterministic_payload(128 * 1024),
    )?;
    let storage = MemoryStorage::new();
    let client = Client::default();
    let repository = repository(&client, &storage)?;
    let cache = PackIndexCacheConfiguration::new(32 * 1024, 1)?;
    let dedup = BackupDeduplicationConfiguration::new(8, 65_536, cache)?;
    let request = incremental_request(source.path())?.with_deduplication(dedup);
    client.backup(repository.clone(), request.clone())?;

    fs::write(
        source.path().join("payload.bin"),
        deterministic_payload(128 * 1024 + 17),
    )?;
    let result = client.backup(repository, request)?;

    assert!(result.metrics().reused_bytes() > 0);
    assert!(result.metrics().packs() > 0);
    Ok(())
}

#[test]
fn worker_pool_budgets_have_a_fixed_safety_limit() {
    assert!(matches!(
        BackupBudgets::new(1, MAX_BACKUP_CONCURRENCY + 1, 4, 1),
        Err(BackupBudgetError::CpuWorkersExceedLimit { .. })
    ));
    assert!(matches!(
        BackupBudgets::new(1, 1, 4, MAX_BACKUP_CONCURRENCY + 1),
        Err(BackupBudgetError::NetworkRequestsExceedLimit { .. })
    ));
}

#[derive(Clone)]
struct SlowStorage {
    inner: MemoryStorage,
    delay: Duration,
    active: Arc<AtomicUsize>,
    peak: Arc<AtomicUsize>,
}

impl SlowStorage {
    fn new(inner: MemoryStorage, delay: Duration) -> Self {
        Self {
            inner,
            delay,
            active: Arc::new(AtomicUsize::new(0)),
            peak: Arc::new(AtomicUsize::new(0)),
        }
    }

    fn peak(&self) -> usize {
        self.peak.load(Ordering::Acquire)
    }

    fn enter(&self) {
        let active = self.active.fetch_add(1, Ordering::AcqRel) + 1;
        let mut peak = self.peak.load(Ordering::Acquire);
        while active > peak {
            match self
                .peak
                .compare_exchange_weak(peak, active, Ordering::AcqRel, Ordering::Acquire)
            {
                Ok(_) => break,
                Err(observed) => peak = observed,
            }
        }
        thread::sleep(self.delay);
    }

    fn leave(&self) {
        self.active.fetch_sub(1, Ordering::AcqRel);
    }
}

impl RepositoryStorage for SlowStorage {
    fn capabilities(&self) -> StorageCapabilities {
        self.inner.capabilities()
    }

    fn read_stream(&self, object_key: &ObjectKey) -> Result<ObjectRead, StorageError> {
        self.enter();
        let result = self.inner.read_stream(object_key);
        self.leave();
        result
    }

    fn write_stream(
        &self,
        object_key: &ObjectKey,
        source: &mut dyn Read,
        options: ObjectWriteOptions,
    ) -> Result<gib::ObjectMetadata, StorageError> {
        self.enter();
        let result = self.inner.write_stream(object_key, source, options);
        self.leave();
        result
    }
}

#[test]
fn backup_publishes_snapshot_and_reports_observed_budgets() -> Result<(), Box<dyn Error>> {
    let source = TestDirectory::new()?;
    fs::create_dir(source.path().join("nested"))?;
    fs::write(source.path().join("alpha.txt"), b"alpha content")?;
    fs::write(source.path().join("nested/beta.txt"), b"beta content")?;

    let storage = MemoryStorage::new();
    let client = Client::builder().event_buffer_capacity(1).build()?;
    let repository = repository(&client, &storage)?;
    let result = client.backup(repository, small_request(source.path())?)?;

    assert!(result.snapshot().as_str().starts_with("snapshots/"));
    assert_eq!(result.metrics().files(), 2);
    assert!(result.metrics().chunks() >= 2);
    assert_eq!(
        result.metrics().transformed_chunks(),
        result.metrics().chunks()
    );
    assert!(result.metrics().peak_memory_bytes() <= 2 * 1024 * 1024);
    assert!(result.metrics().peak_open_file_descriptors() <= 8);
    assert!(result.metrics().peak_network_requests() <= 1);
    assert!(result.metrics().peak_cpu_workers() <= 3);
    assert!(result.metrics().peak_hash_workers() <= 1);
    assert!(result.metrics().peak_transform_workers() <= 1);
    assert!(
        storage
            .objects()?
            .iter()
            .any(|object| object == result.snapshot().as_str())
    );
    assert!(
        storage
            .objects()?
            .iter()
            .any(|object| object == "refs/latest")
    );
    Ok(())
}

#[test]
fn tiny_memory_budget_fails_before_pipeline_publication() -> Result<(), Box<dyn Error>> {
    let source = TestDirectory::new()?;
    fs::write(source.path().join("payload.bin"), b"payload")?;
    let storage = MemoryStorage::new();
    let client = Client::default();
    let repository = repository(&client, &storage)?;
    let request = BackupRequest::new(source.path())
        .with_budgets(BackupBudgets::with_queue_capacity(1, 1, 4, 1, 1)?);

    let error = client
        .backup(repository, request)
        .expect_err("the one-byte memory budget must be rejected");
    assert!(matches!(
        error,
        SdkError::BackupBudgetExceeded {
            resource: gib::BackupResource::Memory,
            ..
        }
    ));
    assert!(
        !storage
            .objects()?
            .iter()
            .any(|object| object == "HEAD" || object == "refs/latest")
    );
    Ok(())
}

#[test]
fn large_file_releases_chunk_memory_between_bounded_handoffs() -> Result<(), Box<dyn Error>> {
    let source = TestDirectory::new()?;
    fs::write(source.path().join("large.bin"), vec![b'l'; 4 * 1024 * 1024])?;
    let storage = MemoryStorage::new();
    let client = Client::default();
    let repository = repository(&client, &storage)?;
    let request = BackupRequest::new(source.path())
        .with_budgets(BackupBudgets::with_queue_capacity(
            32 * 1024 * 1024,
            3,
            8,
            1,
            1,
        )?)
        .with_chunking(ChunkingConfiguration::new(
            256 * 1024,
            512 * 1024,
            1024 * 1024,
        )?)
        .with_pack_configuration(PackConfiguration::new(2 * 1024 * 1024, 4 * 1024 * 1024)?)
        .with_index_configuration(PackIndexConfiguration::new(512)?);
    let result = client.backup(repository, request)?;

    assert_eq!(result.metrics().files(), 1);
    assert_eq!(result.metrics().total_size(), 4 * 1024 * 1024);
    assert!(result.metrics().chunks() > 1);
    assert!(result.metrics().peak_memory_bytes() <= 32 * 1024 * 1024);
    Ok(())
}

#[test]
fn cancellation_stops_pipeline_without_publishing_head() -> Result<(), Box<dyn Error>> {
    let source = TestDirectory::new()?;
    for index in 0..64 {
        fs::write(
            source.path().join(format!("file-{index:03}.txt")),
            vec![b'x'; 256],
        )?;
    }
    let storage = MemoryStorage::new();
    let client = Client::default();
    let repository = repository(&client, &storage)?;
    let handle = client.start_backup(repository, small_request(source.path())?)?;
    handle.cancel()?;
    assert!(matches!(
        handle.join(),
        Err(SdkError::OperationCancelled { .. })
    ));
    assert!(
        !storage
            .objects()?
            .iter()
            .any(|object| object == "HEAD" || object == "refs/latest")
    );
    Ok(())
}

#[test]
fn injected_upload_failure_keeps_typed_stage_and_storage_context() -> Result<(), Box<dyn Error>> {
    let source = TestDirectory::new()?;
    fs::write(source.path().join("payload.bin"), vec![b'p'; 512])?;
    let storage = MemoryStorage::new();
    let client = Client::default();
    let repository = repository(&client, &storage)?;
    storage.inject_failure(
        MemoryStorageOperation::ConditionalWrite,
        StorageError::Transient,
    );

    let error = client
        .backup(repository, small_request(source.path())?)
        .expect_err("the injected conditional write must fail the backup");
    assert_eq!(error.code(), ErrorCode::BackupStorageFailure);
    assert!(matches!(
        error,
        SdkError::BackupStorageFailure {
            stage: gib::BackupStage::Upload,
            error: StorageError::Transient,
            ..
        }
    ));
    assert!(
        !storage
            .objects()?
            .iter()
            .any(|object| object == "HEAD" || object == "refs/latest")
    );
    Ok(())
}

#[test]
fn slow_storage_is_backpressured_by_the_network_budget() -> Result<(), Box<dyn Error>> {
    let source = TestDirectory::new()?;
    for index in 0..48 {
        fs::write(
            source.path().join(format!("file-{index:03}.bin")),
            vec![index as u8; 512],
        )?;
    }
    let storage = SlowStorage::new(MemoryStorage::new(), Duration::from_millis(2));
    let client = Client::default();
    let repository = client.initialize_repository(
        storage.clone(),
        RepositoryInitRequest::new(
            RepositoryIdentity::new("slow-backup-test")?,
            RepositoryKey::new("test")?,
        ),
    )?;
    let started = Instant::now();
    let result = client.backup(
        repository,
        BackupRequest::new(source.path())
            .with_budgets(BackupBudgets::with_queue_capacity(
                4 * 1024 * 1024,
                4,
                12,
                2,
                1,
            )?)
            .with_chunking(ChunkingConfiguration::new(64, 128, 256)?)
            .with_pack_configuration(PackConfiguration::new(4096, 8192)?)
            .with_index_configuration(PackIndexConfiguration::new(512)?),
    )?;
    assert_eq!(result.metrics().files(), 48);
    assert!(storage.peak() <= 2);
    assert!(started.elapsed() >= Duration::from_millis(2));
    Ok(())
}

#[test]
fn slow_event_consumer_cannot_grow_pipeline_queues() -> Result<(), Box<dyn Error>> {
    let source = TestDirectory::new()?;
    for index in 0..32 {
        fs::write(
            source.path().join(format!("file-{index:03}.txt")),
            vec![b'e'; 256],
        )?;
    }
    let client = Client::builder().event_buffer_capacity(1).build()?;
    let observed = Arc::new(AtomicUsize::new(0));
    let consumer_observed = Arc::clone(&observed);
    let subscription = client.register_event_consumer(move |_event| {
        consumer_observed.fetch_add(1, Ordering::AcqRel);
        thread::sleep(Duration::from_millis(3));
    })?;
    let storage = MemoryStorage::new();
    let repository = repository(&client, &storage)?;
    let started = Instant::now();
    let result = client.backup(repository, small_request(source.path())?)?;
    assert_eq!(result.metrics().files(), 32);
    assert!(started.elapsed() < Duration::from_secs(10));
    drop(subscription);
    assert!(observed.load(Ordering::Acquire) > 0);
    Ok(())
}

#[test]
#[ignore = "run explicitly for a large bounded-workload stress test"]
fn stress_opt_in_large_entry_count() -> Result<(), Box<dyn Error>> {
    let entries = std::env::var("GIB_BACKUP_STRESS_ENTRIES")
        .ok()
        .and_then(|value| value.parse::<usize>().ok())
        .unwrap_or(1_000_000);
    let source = TestDirectory::new()?;
    const ENTRIES_PER_DIRECTORY: usize = 256;
    for index in 0..entries {
        if index % ENTRIES_PER_DIRECTORY == 0 {
            fs::create_dir(
                source
                    .path()
                    .join(format!("bucket-{:05}", index / ENTRIES_PER_DIRECTORY)),
            )?;
        }
        let bucket = source
            .path()
            .join(format!("bucket-{:05}", index / ENTRIES_PER_DIRECTORY));
        fs::write(bucket.join(format!("file-{index:07}.bin")), [index as u8])?;
    }
    let storage = MemoryStorage::new();
    let client = Client::default();
    let repository = repository(&client, &storage)?;
    let result = client.backup(
        repository,
        BackupRequest::new(source.path())
            .with_budgets(BackupBudgets::with_queue_capacity(
                16 * 1024 * 1024,
                4,
                32,
                2,
                2,
            )?)
            .with_chunking(ChunkingConfiguration::new(16, 32, 64)?)
            .with_pack_configuration(PackConfiguration::new(4096, 8192)?)
            .with_index_configuration(PackIndexConfiguration::new(4096)?),
    )?;
    assert_eq!(result.metrics().files(), entries as u64);
    assert!(result.metrics().peak_memory_bytes() <= 16 * 1024 * 1024);
    Ok(())
}
