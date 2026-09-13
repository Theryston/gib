use gib::{
    BackupBudgets, BackupDeduplicationConfiguration, BackupPipeline, BackupRequest, BackupStage,
    ChunkingConfiguration, Client, ErrorCode, MemoryStorage, ObjectCodec, ObjectEncryption,
    ObjectKey, ObjectListPage, ObjectListRequest, ObjectRange, ObjectRead, ObjectTransformOptions,
    ObjectWriteOptions, PackConfiguration, PackIndexCacheConfiguration, PackIndexConfiguration,
    PendingOperationListRequest, PendingOperationStatus, Repository, RepositoryEncryption,
    RepositoryIdentity, RepositoryInitRequest, RepositoryKey, RepositorySalt, RepositoryStorage,
    SdkError, StorageCapabilities, StorageError, StorageVersion, local_filesystem_scanner,
};
use std::error::Error;
use std::fs;
use std::io::Read;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::Duration;

static NEXT_TEST_DIRECTORY: AtomicU64 = AtomicU64::new(1);

struct TestDirectory {
    path: PathBuf,
}

impl TestDirectory {
    fn new() -> Result<Self, Box<dyn Error>> {
        let id = NEXT_TEST_DIRECTORY.fetch_add(1, Ordering::Relaxed);
        for attempt in 0..32 {
            let path = std::env::temp_dir().join(format!(
                "gib-resumable-backup-{}-{id}-{attempt}",
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
            "could not allocate a resumable-backup test directory",
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

fn request(root: &Path) -> Result<BackupRequest, Box<dyn Error>> {
    Ok(BackupRequest::new(root)
        .with_message("resumable backup")
        .with_created_at(4_242)
        .with_budgets(BackupBudgets::with_queue_capacity(
            16 * 1024 * 1024,
            3,
            8,
            1,
            1,
        )?)
        .with_chunking(ChunkingConfiguration::new(32, 64, 128)?)
        .with_pack_configuration(PackConfiguration::new(4 * 1024, 8 * 1024)?)
        .with_index_configuration(PackIndexConfiguration::new(1_024)?))
}

fn repository<S>(client: &Client, storage: S) -> Result<Repository, Box<dyn Error>>
where
    S: RepositoryStorage + 'static,
{
    Ok(client.initialize_repository(
        storage,
        RepositoryInitRequest::new(
            RepositoryIdentity::new("resumable-backup-test")?,
            RepositoryKey::new("test")?,
        ),
    )?)
}

fn populate_source(source: &TestDirectory) -> Result<(), Box<dyn Error>> {
    for file_index in 0..96_usize {
        let contents = (0..2_048_usize)
            .map(|offset| {
                ((offset
                    .wrapping_mul(37)
                    .wrapping_add(file_index.wrapping_mul(11)))
                    % 251) as u8
            })
            .collect::<Vec<_>>();
        fs::write(
            source.path().join(format!("file-{file_index:03}.bin")),
            contents,
        )?;
    }
    Ok(())
}
fn populate_small_source(source: &TestDirectory, files: usize) -> Result<(), Box<dyn Error>> {
    for file_index in 0..files {
        let contents = (0..2_048_usize)
            .map(|offset| {
                ((offset
                    .wrapping_mul(37)
                    .wrapping_add(file_index.wrapping_mul(11)))
                    % 251) as u8
            })
            .collect::<Vec<_>>();
        fs::write(
            source.path().join(format!("small-{file_index:03}.bin")),
            contents,
        )?;
    }
    Ok(())
}

fn small_interrupted_backup_with_message(
    message: &str,
    author: Option<&str>,
) -> Result<InterruptedBackup, Box<dyn Error>> {
    let source = TestDirectory::new()?;
    populate_small_source(&source, 16)?;
    let storage = DelayedStorage::new(MemoryStorage::new(), Duration::from_millis(2));
    let client = Client::default();
    let repository = repository(&client, storage.clone())?;
    let mut request = request(source.path())?.with_message(message);
    if let Some(author) = author {
        request = request.with_author(author);
    }
    let handle = client.start_backup(repository.clone(), request.clone())?;
    let mut identifier = None;
    for _ in 0..1_000 {
        let page = repository.list_pending_operations(PendingOperationListRequest::new())?;
        let ready = page
            .operations()
            .iter()
            .find(|operation| operation.completed_objects() >= 1)
            .map(|operation| operation.identifier().to_owned());
        if ready.is_some() && storage.has_pack_write() {
            identifier = ready;
            break;
        }
        thread::sleep(Duration::from_millis(2));
    }
    let _ = handle.cancel();
    let result = handle.join();
    if !matches!(result, Err(SdkError::OperationCancelled { .. })) {
        return Err(format!("expected cooperative cancellation, got {result:?}").into());
    }
    let identifier = identifier.ok_or("backup did not persist a partial journal")?;
    Ok(InterruptedBackup {
        source,
        storage,
        repository,
        client,
        request,
        identifier,
    })
}

#[derive(Clone)]
struct PrefixFaultStorage {
    inner: MemoryStorage,
    prefix: String,
    fired: Arc<AtomicBool>,
}

impl PrefixFaultStorage {
    fn new(inner: MemoryStorage, prefix: &str) -> Self {
        Self {
            inner,
            prefix: prefix.to_owned(),
            fired: Arc::new(AtomicBool::new(false)),
        }
    }

    fn fault_fired(&self) -> bool {
        self.fired.load(Ordering::Acquire)
    }
}

impl RepositoryStorage for PrefixFaultStorage {
    fn capabilities(&self) -> StorageCapabilities {
        self.inner.capabilities()
    }

    fn read_stream(&self, object_key: &ObjectKey) -> Result<ObjectRead, StorageError> {
        self.inner.read_stream(object_key)
    }

    fn read_range(
        &self,
        object_key: &ObjectKey,
        range: ObjectRange,
    ) -> Result<ObjectRead, StorageError> {
        self.inner.read_range(object_key, range)
    }

    fn metadata(&self, object_key: &ObjectKey) -> Result<gib::ObjectMetadata, StorageError> {
        self.inner.metadata(object_key)
    }

    fn write_stream(
        &self,
        object_key: &ObjectKey,
        source: &mut dyn Read,
        options: ObjectWriteOptions,
    ) -> Result<gib::ObjectMetadata, StorageError> {
        if object_key.as_str().starts_with(self.prefix.as_str())
            && !self.fired.swap(true, Ordering::AcqRel)
        {
            return Err(StorageError::Transient);
        }
        self.inner.write_stream(object_key, source, options)
    }

    fn delete(&self, object_key: &ObjectKey) -> Result<(), StorageError> {
        self.inner.delete(object_key)
    }

    fn list_page(&self, request: &ObjectListRequest) -> Result<ObjectListPage, StorageError> {
        self.inner.list_page(request)
    }
}

struct InterruptedBackup {
    source: TestDirectory,
    storage: DelayedStorage,
    repository: Repository,
    client: Client,
    request: BackupRequest,
    identifier: String,
}

fn interrupted_backup() -> Result<InterruptedBackup, Box<dyn Error>> {
    let source = TestDirectory::new()?;
    populate_source(&source)?;
    let storage = DelayedStorage::new(MemoryStorage::new(), Duration::from_millis(8));
    let client = Client::default();
    let repository = repository(&client, storage.clone())?;
    let request = request(source.path())?;
    let handle = client.start_backup(repository.clone(), request.clone())?;

    let mut identifier = None;
    for _ in 0..1_000 {
        let page = repository.list_pending_operations(PendingOperationListRequest::new())?;
        let has_persisted_work = page
            .operations()
            .iter()
            .find(|operation| operation.completed_objects() >= 4)
            .map(|operation| operation.identifier().to_owned());
        if has_persisted_work.is_some() && storage.has_pack_write() {
            // The write log records at upload entry while the journal records
            // after the store completes. Settling lets the in-flight journal
            // CAS commit, so the logged pack is also journaled and resume can
            // skip it deterministically instead of re-attempting it.
            thread::sleep(Duration::from_millis(50));
            identifier = has_persisted_work;
            break;
        }
        thread::sleep(Duration::from_millis(5));
    }
    let _ = handle.cancel();
    let result = handle.join();
    if !matches!(result, Err(SdkError::OperationCancelled { .. })) {
        return Err(format!("expected cooperative cancellation, got {result:?}").into());
    }
    let identifier = identifier.ok_or("backup did not persist a partial journal")?;
    Ok(InterruptedBackup {
        source,
        storage,
        repository,
        client,
        request,
        identifier,
    })
}

fn nested_code(error: &SdkError) -> ErrorCode {
    match error {
        SdkError::BackupStageFailed { source, .. } => source.code(),
        other => other.code(),
    }
}

#[test]
fn interrupted_backup_resumes_without_active_journal() -> Result<(), Box<dyn Error>> {
    let interrupted = interrupted_backup()?;
    let before = interrupted.storage.writes();
    let ranges_before = interrupted.storage.range_reads();
    let resumed = interrupted.client.resume_backup(
        interrupted.repository.clone(),
        &interrupted.identifier,
        interrupted.request.clone(),
    )?;
    let resumed_result = resumed.join()?;
    let after = interrupted.storage.writes();
    let resumed_pack_writes = after[before.len()..]
        .iter()
        .filter(|key| key.starts_with("packs/"))
        .count();
    let initial_pack_writes = before
        .iter()
        .filter(|key| key.starts_with("packs/"))
        .count();
    assert!(initial_pack_writes > 0);
    assert!(resumed_pack_writes < resumed_result.metrics().packs() as usize);
    assert_eq!(interrupted.storage.range_reads(), ranges_before);

    let clean_storage = MemoryStorage::new();
    let clean_repository = repository(&interrupted.client, clean_storage)?;
    let clean_result = interrupted
        .client
        .backup(clean_repository, interrupted.request.clone())?;
    assert_eq!(resumed_result.snapshot(), clean_result.snapshot());
    assert!(
        interrupted
            .repository
            .list_pending_operations(PendingOperationListRequest::new())?
            .operations()
            .is_empty()
    );
    Ok(())
}

#[test]
fn resume_rejects_request_and_source_changes() -> Result<(), Box<dyn Error>> {
    let interrupted = interrupted_backup()?;
    let mismatch = interrupted.client.resume_backup(
        interrupted.repository.clone(),
        &interrupted.identifier,
        interrupted
            .request
            .clone()
            .with_message("different request"),
    )?;
    let mismatch = mismatch
        .join()
        .expect_err("request mismatch must be rejected");
    assert_eq!(
        nested_code(&mismatch),
        ErrorCode::OperationJournalIncompatible
    );

    fs::write(
        interrupted.source.path().join("file-000.bin"),
        vec![b'c'; 2_048],
    )?;
    let source_change = interrupted.client.resume_backup(
        interrupted.repository.clone(),
        &interrupted.identifier,
        interrupted.request,
    )?;
    let source_change = source_change
        .join()
        .expect_err("source changes must be rejected");
    assert_eq!(
        nested_code(&source_change),
        ErrorCode::OperationJournalSourceChanged
    );
    assert!(!interrupted.repository.read_head()?.has_snapshot());
    Ok(())
}

#[test]
fn corrupt_journal_cannot_publish_data() -> Result<(), Box<dyn Error>> {
    let interrupted = interrupted_backup()?;
    let key = format!("operations/{}", interrupted.identifier);
    let mut bytes = interrupted.storage.inner.read_object(&key)?;
    let middle = bytes.len() / 2;
    bytes[middle] ^= 0x80;
    interrupted.storage.inner.replace_object(&key, bytes)?;

    let pending = interrupted
        .repository
        .list_pending_operations(PendingOperationListRequest::new())?;
    let row = pending
        .operations()
        .first()
        .ok_or("corrupt journal should remain listed")?;
    assert_eq!(row.status().as_str(), "corrupt");

    let error = interrupted
        .client
        .resume_backup(
            interrupted.repository.clone(),
            &interrupted.identifier,
            interrupted.request,
        )
        .expect_err("corrupt journals must fail before starting a run");
    assert_eq!(error.code(), ErrorCode::OperationJournalMalformed);
    assert!(!interrupted.repository.read_head()?.has_snapshot());
    Ok(())
}

#[test]
fn missing_completed_objects_are_rebuilt_before_resume() -> Result<(), Box<dyn Error>> {
    let interrupted = interrupted_backup()?;
    let tree_key = interrupted
        .storage
        .inner
        .objects()?
        .into_iter()
        .find(|key| key.starts_with("trees/"))
        .ok_or("interrupted backup should have a completed tree object")?;
    assert!(interrupted.storage.inner.remove_object(&tree_key)?);

    let result = interrupted
        .client
        .resume_backup(
            interrupted.repository.clone(),
            interrupted.identifier,
            interrupted.request,
        )?
        .join()?;
    assert!(result.snapshot().as_str().starts_with("snapshots/"));
    assert!(
        interrupted
            .storage
            .inner
            .objects()?
            .iter()
            .any(|key| key == &tree_key)
    );
    assert!(
        interrupted
            .repository
            .list_pending_operations(PendingOperationListRequest::new())?
            .operations()
            .is_empty()
    );
    Ok(())
}

#[test]
fn pending_listing_rejects_an_unbounded_page_request() -> Result<(), Box<dyn Error>> {
    let client = Client::default();
    let repository = repository(&client, MemoryStorage::new())?;
    let error = repository
        .list_pending_operations(PendingOperationListRequest::new().with_limit(101))
        .expect_err("pending journal pages must remain bounded");
    assert_eq!(error.code(), ErrorCode::OperationJournalTooLarge);
    Ok(())
}

#[test]
fn already_published_journal_is_detected_and_cleaned() -> Result<(), Box<dyn Error>> {
    let source = TestDirectory::new()?;
    fs::write(source.path().join("payload"), b"payload")?;
    let storage = PublicationFailureStorage::new();
    let client = Client::default();
    let repository = repository(&client, storage.clone())?;
    let request = request(source.path())?;
    let error = client
        .backup(repository.clone(), request.clone())
        .expect_err("journal cleanup fault should not hide the published HEAD");
    assert_eq!(
        nested_code(&error),
        ErrorCode::OperationJournalStorageFailure
    );
    assert!(repository.read_head()?.has_snapshot());
    let pending = repository.list_pending_operations(PendingOperationListRequest::new())?;
    let identifier = pending
        .operations()
        .first()
        .ok_or("published journal should remain recoverable")?
        .identifier()
        .to_owned();

    let error = client
        .resume_backup(repository.clone(), identifier, request)?
        .join()
        .expect_err("already-published journals are explicit outcomes");
    assert_eq!(
        nested_code(&error),
        ErrorCode::OperationJournalAlreadyPublished
    );
    assert!(
        repository
            .list_pending_operations(PendingOperationListRequest::new())?
            .operations()
            .is_empty()
    );
    Ok(())
}

#[test]
fn publish_checkpoint_resumes_without_rewriting_immutable_objects() -> Result<(), Box<dyn Error>> {
    let source = TestDirectory::new()?;
    fs::write(source.path().join("payload"), b"payload")?;
    let storage = PublicationFailureStorage::for_head_failure();
    let client = Client::default();
    let repository = repository(&client, storage.clone())?;
    let request = request(source.path())?;

    let first = client
        .backup(repository.clone(), request.clone())
        .expect_err("the injected HEAD failure should leave a publish checkpoint");
    assert_eq!(nested_code(&first), ErrorCode::StorageFailure);
    let pending = repository.list_pending_operations(PendingOperationListRequest::new())?;
    let operation = pending
        .operations()
        .first()
        .ok_or("the publish checkpoint should remain pending")?;
    assert_eq!(operation.checkpoint(), Some(BackupStage::Publish));
    assert!(operation.target_snapshot().is_some());
    let packs_before = storage
        .inner
        .objects()?
        .into_iter()
        .filter(|key| key.starts_with("packs/"))
        .collect::<Vec<_>>();

    let result = client
        .resume_backup(repository.clone(), operation.identifier(), request)?
        .join()?;
    assert_eq!(result.metrics().packs(), 1);
    let packs_after = storage
        .inner
        .objects()?
        .into_iter()
        .filter(|key| key.starts_with("packs/"))
        .collect::<Vec<_>>();
    assert_eq!(packs_after, packs_before);
    assert!(
        repository
            .list_pending_operations(PendingOperationListRequest::new())?
            .operations()
            .is_empty()
    );
    Ok(())
}
#[test]
fn stage_faults_at_each_prefix_leave_a_resumable_journal() -> Result<(), Box<dyn Error>> {
    let source = TestDirectory::new()?;
    populate_small_source(&source, 16)?;
    let base = request(source.path())?;
    let client = Client::default();
    let clean_storage = MemoryStorage::new();
    let clean_repository = repository(&client, clean_storage)?;
    let clean = client.backup(clean_repository, base.clone())?;
    let mut snapshots = Vec::new();
    for prefix in ["operations/", "trees/", "packs/", "indexes/", "snapshots/"] {
        let storage = PrefixFaultStorage::new(MemoryStorage::new(), prefix);
        let repository = repository(&client, storage.clone())?;
        let fault = client
            .backup(repository.clone(), base.clone())
            .expect_err("the injected stage fault must fail the first attempt");
        assert!(
            storage.fault_fired(),
            "fault on {prefix} should have fired, got {fault:?}"
        );
        if prefix == "operations/" {
            assert!(
                repository
                    .list_pending_operations(PendingOperationListRequest::new())?
                    .operations()
                    .is_empty(),
                "a journal-create fault must leave no journal"
            );
            let retried = client.backup(repository.clone(), base.clone())?;
            snapshots.push(retried.snapshot().as_str().to_owned());
            continue;
        }
        let pending = repository.list_pending_operations(PendingOperationListRequest::new())?;
        assert_eq!(
            pending.operations().len(),
            1,
            "fault on {prefix} must leave one journal"
        );
        assert_eq!(
            pending.operations()[0].status(),
            PendingOperationStatus::Active
        );
        let identifier = pending.operations()[0].identifier().to_owned();
        let resumed = client
            .resume_backup(repository.clone(), &identifier, base.clone())?
            .join()?;
        snapshots.push(resumed.snapshot().as_str().to_owned());
        assert!(
            repository
                .list_pending_operations(PendingOperationListRequest::new())?
                .operations()
                .is_empty(),
            "fault on {prefix} must resume to no pending entry"
        );
        assert!(repository.read_head()?.has_snapshot());
    }
    for snapshot in &snapshots {
        assert_eq!(
            snapshot,
            clean.snapshot().as_str(),
            "every stage resume must converge on the clean snapshot"
        );
    }
    Ok(())
}

#[test]
fn immediate_cancellation_before_first_upload_still_resumes() -> Result<(), Box<dyn Error>> {
    let source = TestDirectory::new()?;
    populate_small_source(&source, 8)?;
    let storage = DelayedStorage::new(MemoryStorage::new(), Duration::from_millis(2));
    let client = Client::default();
    let repository = repository(&client, storage.clone())?;
    let pending_request = request(source.path())?;
    let handle = client.start_backup(repository.clone(), pending_request.clone())?;
    let mut identifier = None;
    for _ in 0..1_000 {
        let page = repository.list_pending_operations(PendingOperationListRequest::new())?;
        if let Some(operation) = page.operations().first() {
            identifier = Some(operation.identifier().to_owned());
            break;
        }
        thread::sleep(Duration::from_millis(2));
    }
    let _ = handle.cancel();
    let result = handle.join();
    if !matches!(result, Err(SdkError::OperationCancelled { .. })) {
        return Err(format!("expected cooperative cancellation, got {result:?}").into());
    }
    let identifier = identifier.ok_or("cancelled backup should leave its journal")?;
    let pending = repository.list_pending_operations(PendingOperationListRequest::new())?;
    assert_eq!(pending.operations().len(), 1);
    let resumed = client
        .resume_backup(repository.clone(), &identifier, pending_request)?
        .join()?;
    assert!(resumed.snapshot().as_str().starts_with("snapshots/"));
    assert!(
        repository
            .list_pending_operations(PendingOperationListRequest::new())?
            .operations()
            .is_empty()
    );
    Ok(())
}
#[test]
fn resume_rejects_every_fingerprinted_option_change() -> Result<(), Box<dyn Error>> {
    let interrupted = small_interrupted_backup_with_message("option matrix", None)?;
    let cache = PackIndexCacheConfiguration::new(32 * 1024, 1)?;
    let variants: Vec<(&str, BackupRequest)> = vec![
        (
            "message",
            interrupted.request.clone().with_message("changed message"),
        ),
        (
            "author",
            interrupted
                .request
                .clone()
                .with_author("changed <changed@example.test>"),
        ),
        (
            "created_at",
            interrupted.request.clone().with_created_at(9_999),
        ),
        (
            "chunking",
            interrupted
                .request
                .clone()
                .with_chunking(ChunkingConfiguration::new(64, 128, 256)?),
        ),
        (
            "pack",
            interrupted
                .request
                .clone()
                .with_pack_configuration(PackConfiguration::new(8 * 1024, 16 * 1024)?),
        ),
        (
            "index",
            interrupted
                .request
                .clone()
                .with_index_configuration(PackIndexConfiguration::new(2_048)?),
        ),
        (
            "dedup",
            interrupted
                .request
                .clone()
                .with_deduplication_configuration(BackupDeduplicationConfiguration::new(
                    4, 1_024, cache,
                )?),
        ),
        (
            "transforms",
            interrupted
                .request
                .clone()
                .with_pack_configuration(PackConfiguration::new(2 * 1024 * 1024, 4 * 1024 * 1024)?)
                .with_transform_options(ObjectTransformOptions::new(
                    ObjectCodec::Zstd,
                    ObjectEncryption::None,
                )),
        ),
        (
            "budgets",
            interrupted
                .request
                .clone()
                .with_budgets(BackupBudgets::with_queue_capacity(
                    32 * 1024 * 1024,
                    3,
                    8,
                    1,
                    1,
                )?),
        ),
        ("parent", interrupted.request.clone().without_parent()),
    ];
    for (name, variant) in variants {
        let error = interrupted
            .client
            .resume_backup(
                interrupted.repository.clone(),
                &interrupted.identifier,
                variant,
            )?
            .join()
            .expect_err(&format!("changed {name} must be rejected"));
        assert_eq!(
            nested_code(&error),
            ErrorCode::OperationJournalIncompatible,
            "option {name} must map to incompatible"
        );
    }
    assert!(!interrupted.repository.read_head()?.has_snapshot());
    Ok(())
}

#[test]
fn advanced_head_rejects_stale_resume() -> Result<(), Box<dyn Error>> {
    let interrupted = small_interrupted_backup_with_message("stale base", None)?;
    let advance_dir = TestDirectory::new()?;
    fs::write(advance_dir.path().join("advance.txt"), b"advancing head")?;
    let advance_request = request(advance_dir.path())?
        .with_message("advance head")
        .with_created_at(5_000);
    let advanced = interrupted
        .client
        .backup(interrupted.repository.clone(), advance_request)?;
    assert_eq!(interrupted.repository.read_head()?.generation(), 1);
    let error = interrupted
        .client
        .resume_backup(
            interrupted.repository.clone(),
            &interrupted.identifier,
            interrupted.request.clone(),
        )?
        .join()
        .expect_err("stale base must be rejected");
    assert_eq!(nested_code(&error), ErrorCode::OperationJournalStale);
    assert_eq!(
        interrupted.repository.read_head()?.snapshot(),
        Some(advanced.snapshot())
    );
    Ok(())
}

#[test]
fn tampered_completed_object_bytes_fail_closed() -> Result<(), Box<dyn Error>> {
    let interrupted = small_interrupted_backup_with_message("tamper check", None)?;
    let mut tampered = false;
    for key in interrupted.storage.inner.objects()? {
        if key.starts_with("trees/") || key.starts_with("packs/") || key.starts_with("indexes/") {
            let mut bytes = interrupted.storage.inner.read_object(&key)?;
            let middle = bytes.len() / 2;
            bytes[middle] ^= 0x80;
            interrupted.storage.inner.replace_object(&key, bytes)?;
            tampered = true;
        }
    }
    if !tampered {
        return Err("interrupted backup should have completed immutable objects".into());
    }
    let error = interrupted
        .client
        .resume_backup(
            interrupted.repository.clone(),
            &interrupted.identifier,
            interrupted.request,
        )?
        .join()
        .expect_err("tampered objects must fail closed");
    assert_eq!(nested_code(&error), ErrorCode::OperationJournalMalformed);
    assert!(!interrupted.repository.read_head()?.has_snapshot());
    Ok(())
}
#[test]
fn oversized_journal_lists_as_too_large_and_cannot_resume() -> Result<(), Box<dyn Error>> {
    let client = Client::default();
    let storage = MemoryStorage::new();
    let repository = repository(&client, storage.clone())?;
    let key = "operations/op-999-abcdef0123456789";
    let oversized = vec![0u8; 8 * 1024 * 1024 + 1];
    storage.put(key, &oversized)?;
    let pending = repository.list_pending_operations(PendingOperationListRequest::new())?;
    assert_eq!(pending.operations().len(), 1);
    let row = &pending.operations()[0];
    assert_eq!(row.status(), PendingOperationStatus::TooLarge);
    assert_eq!(row.object_size(), oversized.len() as u64);
    let error = client
        .resume_backup(
            repository.clone(),
            "op-999-abcdef0123456789",
            request(std::env::temp_dir().as_path())?,
        )
        .expect_err("oversized journals must be rejected");
    assert_eq!(error.code(), ErrorCode::OperationJournalTooLarge);
    assert!(!repository.read_head()?.has_snapshot());
    Ok(())
}

#[test]
fn pending_pagination_lists_only_journals_in_bounded_pages() -> Result<(), Box<dyn Error>> {
    let source = TestDirectory::new()?;
    populate_small_source(&source, 2)?;
    let storage = MemoryStorage::new();
    let client = Client::default();
    let repository = repository(&client, storage.clone())?;
    client.backup(repository.clone(), request(source.path())?)?;
    assert!(
        storage
            .objects()?
            .iter()
            .any(|key| key.starts_with("packs/")),
        "the clean backup should leave immutable packs"
    );
    for index in 0..5 {
        let key = format!("operations/op-500-test-{index}");
        storage.put(key.as_str(), b"not-a-journal")?;
    }
    let mut identifiers = Vec::new();
    let mut cursor = None;
    loop {
        let mut list = PendingOperationListRequest::new().with_limit(2);
        if let Some(cursor_value) = cursor {
            list = list.with_cursor(cursor_value);
        }
        let page = repository.list_pending_operations(list)?;
        assert!(page.operations().len() <= 2);
        for operation in page.operations() {
            assert_eq!(operation.status(), PendingOperationStatus::Corrupt);
            assert!(!operation.identifier().starts_with("packs/"));
            identifiers.push(operation.identifier().to_owned());
        }
        cursor = page.next_cursor().cloned();
        if cursor.is_none() {
            break;
        }
    }
    identifiers.sort();
    identifiers.dedup();
    assert_eq!(identifiers.len(), 5);
    Ok(())
}

#[test]
fn journals_never_serialize_request_secrets_or_source_contents() -> Result<(), Box<dyn Error>> {
    let message = "sentinel-message-7f3a9c1e42";
    let author = "sentinel-author-4d2e81f0";
    let file_marker = "file-secret-sentinel-9c8d7e6f5a";
    let source = TestDirectory::new()?;
    populate_small_source(&source, 4)?;
    fs::write(source.path().join("secret.txt"), file_marker)?;
    let storage = DelayedStorage::new(MemoryStorage::new(), Duration::from_millis(2));
    let client = Client::default();
    let repository = repository(&client, storage.clone())?;
    let pending_request = request(source.path())?
        .with_message(message)
        .with_author(author);
    let handle = client.start_backup(repository.clone(), pending_request)?;
    let mut identifier = None;
    for _ in 0..1_000 {
        let page = repository.list_pending_operations(PendingOperationListRequest::new())?;
        if let Some(operation) = page.operations().first() {
            identifier = Some(operation.identifier().to_owned());
            break;
        }
        thread::sleep(Duration::from_millis(2));
    }
    let identifier = identifier.ok_or("backup should persist a journal")?;
    let bytes = storage
        .inner
        .read_object(format!("operations/{identifier}").as_str())?;
    let text = String::from_utf8_lossy(&bytes);
    for secret in [message, author, file_marker] {
        assert!(
            !text.contains(secret),
            "journal must not serialize secret material"
        );
    }
    let _ = handle.cancel();
    let _ = handle.join();
    Ok(())
}

#[test]
fn encrypted_journal_lists_as_encrypted_and_resumes_with_key() -> Result<(), Box<dyn Error>> {
    let source = TestDirectory::new()?;
    populate_small_source(&source, 8)?;
    let password = b"journal-encryption-test-password";
    let encryption =
        RepositoryEncryption::from_password(password, RepositorySalt::from_bytes([0x71; 16]))?;
    let storage = DelayedStorage::new(MemoryStorage::new(), Duration::from_millis(2));
    let client = Client::default();
    let repository = repository(&client, storage.clone())?;
    let pending_request = request(source.path())?
        .with_budgets(BackupBudgets::with_queue_capacity(
            64 * 1024 * 1024,
            3,
            8,
            1,
            1,
        )?)
        .with_pack_configuration(PackConfiguration::new(2 * 1024 * 1024, 4 * 1024 * 1024)?)
        .with_transform_options(ObjectTransformOptions::new(
            ObjectCodec::Zstd,
            ObjectEncryption::XChaCha20Poly1305,
        ));
    let pipeline = BackupPipeline::new(
        repository.clone(),
        local_filesystem_scanner(),
        client.events(),
    )
    .with_encryption(encryption.clone());
    let handle = pipeline.start(pending_request.clone())?;
    let mut identifier = None;
    for _ in 0..1_000 {
        let page = pipeline.list_pending_operations(PendingOperationListRequest::new())?;
        let ready = page
            .operations()
            .iter()
            .find(|operation| operation.completed_objects() >= 1)
            .map(|operation| operation.identifier().to_owned());
        if ready.is_some() && storage.has_pack_write() {
            identifier = ready;
            break;
        }
        thread::sleep(Duration::from_millis(2));
    }
    let identifier = match identifier {
        Some(identifier) => identifier,
        None => {
            let _ = handle.cancel();
            let outcome = handle.join();
            return Err(
                format!("encrypted backup did not persist a partial journal: {outcome:?}").into(),
            );
        }
    };
    let bytes = storage
        .inner
        .read_object(format!("operations/{identifier}").as_str())?;
    assert!(
        !String::from_utf8_lossy(&bytes).contains("journal-encryption-test-password"),
        "encrypted journals must not serialize password material"
    );
    let _ = handle.cancel();
    let result = handle.join();
    if !matches!(result, Err(SdkError::OperationCancelled { .. })) {
        return Err(format!("expected cooperative cancellation, got {result:?}").into());
    }
    let plain = repository.list_pending_operations(PendingOperationListRequest::new())?;
    assert_eq!(plain.operations().len(), 1);
    assert_eq!(
        plain.operations()[0].status(),
        PendingOperationStatus::Encrypted
    );
    let missing_key = client
        .resume_backup(repository.clone(), &identifier, pending_request.clone())
        .expect_err("encrypted journals need repository material");
    assert_eq!(
        missing_key.code(),
        ErrorCode::OperationJournalEncryptionRequired
    );
    let resumed = BackupPipeline::new(
        repository.clone(),
        local_filesystem_scanner(),
        client.events(),
    )
    .with_encryption(encryption)
    .resume(&identifier, pending_request)?
    .join()?;
    assert!(resumed.snapshot().as_str().starts_with("snapshots/"));
    assert!(
        repository
            .list_pending_operations(PendingOperationListRequest::new())?
            .operations()
            .is_empty()
    );
    Ok(())
}

#[derive(Clone)]
struct DelayedStorage {
    inner: MemoryStorage,
    delay: Duration,
    writes: Arc<Mutex<Vec<String>>>,
    range_reads: Arc<AtomicUsize>,
}

impl DelayedStorage {
    fn new(inner: MemoryStorage, delay: Duration) -> Self {
        Self {
            inner,
            delay,
            writes: Arc::new(Mutex::new(Vec::new())),
            range_reads: Arc::new(AtomicUsize::new(0)),
        }
    }

    fn writes(&self) -> Vec<String> {
        self.writes
            .lock()
            .map_or_else(|_| Vec::new(), |value| value.clone())
    }

    fn has_pack_write(&self) -> bool {
        self.writes
            .lock()
            .is_ok_and(|writes| writes.iter().any(|key| key.starts_with("packs/")))
    }

    fn range_reads(&self) -> usize {
        self.range_reads.load(Ordering::Acquire)
    }

    fn record_write(&self, key: &ObjectKey) {
        if let Ok(mut writes) = self.writes.lock() {
            writes.push(key.as_str().to_owned());
        }
    }
}

impl RepositoryStorage for DelayedStorage {
    fn capabilities(&self) -> StorageCapabilities {
        self.inner.capabilities()
    }

    fn read_stream(&self, object_key: &ObjectKey) -> Result<ObjectRead, StorageError> {
        thread::sleep(self.delay);
        self.inner.read_stream(object_key)
    }

    fn read_range(
        &self,
        object_key: &ObjectKey,
        range: ObjectRange,
    ) -> Result<ObjectRead, StorageError> {
        self.range_reads.fetch_add(1, Ordering::AcqRel);
        thread::sleep(self.delay);
        self.inner.read_range(object_key, range)
    }

    fn metadata(&self, object_key: &ObjectKey) -> Result<gib::ObjectMetadata, StorageError> {
        thread::sleep(self.delay);
        self.inner.metadata(object_key)
    }

    fn write_stream(
        &self,
        object_key: &ObjectKey,
        source: &mut dyn Read,
        options: ObjectWriteOptions,
    ) -> Result<gib::ObjectMetadata, StorageError> {
        self.record_write(object_key);
        thread::sleep(self.delay);
        self.inner.write_stream(object_key, source, options)
    }

    fn delete(&self, object_key: &ObjectKey) -> Result<(), StorageError> {
        thread::sleep(self.delay);
        self.inner.delete(object_key)
    }

    fn list_page(&self, request: &ObjectListRequest) -> Result<ObjectListPage, StorageError> {
        thread::sleep(self.delay);
        self.inner.list_page(request)
    }
}

#[derive(Clone)]
struct PublicationFailureStorage {
    inner: MemoryStorage,
    fail_next_journal_write: Arc<AtomicBool>,
    fail_next_head_write: Arc<AtomicBool>,
    fail_journal_cleanup: bool,
}

impl PublicationFailureStorage {
    fn new() -> Self {
        Self {
            inner: MemoryStorage::new(),
            fail_next_journal_write: Arc::new(AtomicBool::new(false)),
            fail_next_head_write: Arc::new(AtomicBool::new(false)),
            fail_journal_cleanup: true,
        }
    }

    fn for_head_failure() -> Self {
        Self {
            inner: MemoryStorage::new(),
            fail_next_journal_write: Arc::new(AtomicBool::new(false)),
            fail_next_head_write: Arc::new(AtomicBool::new(true)),
            fail_journal_cleanup: false,
        }
    }
}

impl RepositoryStorage for PublicationFailureStorage {
    fn capabilities(&self) -> StorageCapabilities {
        self.inner.capabilities()
    }

    fn read_stream(&self, object_key: &ObjectKey) -> Result<ObjectRead, StorageError> {
        self.inner.read_stream(object_key)
    }

    fn read_range(
        &self,
        object_key: &ObjectKey,
        range: ObjectRange,
    ) -> Result<ObjectRead, StorageError> {
        self.inner.read_range(object_key, range)
    }

    fn metadata(&self, object_key: &ObjectKey) -> Result<gib::ObjectMetadata, StorageError> {
        self.inner.metadata(object_key)
    }

    fn write_stream(
        &self,
        object_key: &ObjectKey,
        source: &mut dyn Read,
        options: ObjectWriteOptions,
    ) -> Result<gib::ObjectMetadata, StorageError> {
        if object_key.as_str().starts_with("operations/")
            && self
                .fail_next_journal_write
                .compare_exchange(true, false, Ordering::AcqRel, Ordering::Acquire)
                .is_ok()
        {
            return Err(StorageError::Transient);
        }
        self.inner.write_stream(object_key, source, options)
    }

    fn conditional_write(
        &self,
        object_key: &str,
        expected: Option<&StorageVersion>,
        contents: &[u8],
    ) -> Result<StorageVersion, StorageError> {
        if object_key == "refs/latest"
            && self
                .fail_next_head_write
                .compare_exchange(true, false, Ordering::AcqRel, Ordering::Acquire)
                .is_ok()
        {
            return Err(StorageError::Transient);
        }
        let result = self.inner.conditional_write(object_key, expected, contents);
        if self.fail_journal_cleanup && object_key == "refs/latest" && result.is_ok() {
            self.fail_next_journal_write.store(true, Ordering::Release);
        }
        result
    }

    fn delete(&self, object_key: &ObjectKey) -> Result<(), StorageError> {
        self.inner.delete(object_key)
    }

    fn list_page(&self, request: &ObjectListRequest) -> Result<ObjectListPage, StorageError> {
        self.inner.list_page(request)
    }
}
