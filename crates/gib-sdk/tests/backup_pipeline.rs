use gib::{
    BackupBudgetError, BackupBudgets, BackupRequest, ChunkingConfiguration, Client, ErrorCode,
    MAX_BACKUP_CONCURRENCY, MemoryStorage, MemoryStorageOperation, ObjectKey, ObjectRead,
    ObjectWriteOptions, PackConfiguration, PackIndexConfiguration, RepositoryIdentity,
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
