#[cfg(feature = "s3")]
use gib::{
    CancellationHandle, DEFAULT_S3_MULTIPART_PART_SIZE, MIN_S3_MULTIPART_PART_SIZE, S3Storage,
    S3StorageConfig,
};
use gib::{
    LocalStorage, LocalStorageOperation, MemoryStorage, MemoryStorageOperation, ObjectKey,
    ObjectListRequest, ObjectPrefix, ObjectRange, ObjectStorage, ObjectWriteOptions,
    RepositoryStorage, STORAGE_TRANSFER_BUFFER_SIZE, StorageCapabilities, StorageCapability,
    StorageError, StorageResult,
};
use std::error::Error;
use std::io::{self, Cursor, Read};
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Barrier, Mutex};
use std::thread;

static NEXT_DIRECTORY: AtomicU64 = AtomicU64::new(1);

fn assert_object_storage<S: ObjectStorage>() {}

fn run_storage_contract<S>(storage: S) -> Result<(), Box<dyn Error>>
where
    S: ObjectStorage + Clone + 'static,
{
    run_storage_contract_in_namespace(storage, "objects")
}

fn run_storage_contract_in_namespace<S>(storage: S, namespace: &str) -> Result<(), Box<dyn Error>>
where
    S: ObjectStorage + Clone + 'static,
{
    assert_object_storage::<S>();
    let all = storage.capabilities();
    assert!(all.contains(StorageCapabilities::ALL));
    for capability in [
        StorageCapability::StreamingRead,
        StorageCapability::StreamingWrite,
        StorageCapability::Metadata,
        StorageCapability::PrefixListing,
        StorageCapability::Delete,
        StorageCapability::RangeRead,
        StorageCapability::ConditionalWrite,
    ] {
        assert!(all.supports(capability));
    }

    let empty_key = namespaced_key(namespace, "empty")?;
    let mut empty_source = Cursor::new(Vec::<u8>::new());
    let empty_metadata = storage.write_stream(
        &empty_key,
        &mut empty_source,
        ObjectWriteOptions::if_absent().with_expected_size(0),
    )?;
    assert_eq!(empty_metadata.key(), &empty_key);
    assert_eq!(empty_metadata.size(), 0);
    assert!(empty_metadata.version().is_some());
    assert_eq!(storage.metadata(&empty_key)?, empty_metadata);
    let mut empty_read = storage.read_stream(&empty_key)?;
    assert_eq!(empty_read.metadata(), &empty_metadata);
    let mut empty_contents = Vec::new();
    empty_read.read_to_end(&mut empty_contents)?;
    assert!(empty_contents.is_empty());
    let mut duplicate_source = Cursor::new(b"duplicate".to_vec());
    assert_eq!(
        storage.write_stream(
            &empty_key,
            &mut duplicate_source,
            ObjectWriteOptions::if_absent(),
        ),
        Err(StorageError::AlreadyExists)
    );
    let mut replacement_source = Cursor::new(b"replacement".to_vec());
    let replacement = storage.write_stream(
        &empty_key,
        &mut replacement_source,
        ObjectWriteOptions::new(),
    )?;
    assert_ne!(replacement.version(), empty_metadata.version());
    assert_eq!(replacement.size(), 11);

    let large_size = 3 * 1024 * 1024 + 37;
    let large_key = namespaced_key(namespace, "large")?;
    let requests = Arc::new(Mutex::new(Vec::new()));
    let mut large_source = PatternReader::new(large_size, 97, requests.clone());
    let large_metadata = storage.write_stream(
        &large_key,
        &mut large_source,
        ObjectWriteOptions::if_absent().with_expected_size(large_size as u64),
    )?;
    assert_eq!(large_metadata.size(), large_size as u64);
    let requested_buffers = requests.lock().map_err(|_| "request lock poisoned")?;
    assert!(!requested_buffers.is_empty());
    assert!(
        requested_buffers
            .iter()
            .all(|size| *size <= STORAGE_TRANSFER_BUFFER_SIZE)
    );
    drop(requested_buffers);

    let expected_large = pattern(large_size);
    let mut large_read = storage.read_stream(&large_key)?;
    let mut large_contents = Vec::new();
    large_read.read_to_end(&mut large_contents)?;
    assert_eq!(large_contents, expected_large);

    for (start, length) in [(0, 31), (17, 4_097), (large_size - 1_024, 1_024)] {
        let range = ObjectRange::new(start as u64, length as u64)?;
        let mut range_read = storage.read_range(&large_key, range)?;
        assert_eq!(range_read.metadata().size(), large_size as u64);
        let mut range_contents = Vec::new();
        range_read.read_to_end(&mut range_contents)?;
        assert_eq!(range_contents, expected_large[start..start + length]);
    }
    assert!(matches!(
        storage.read_range(&large_key, ObjectRange::new((large_size - 1) as u64, 2)?,),
        Err(StorageError::InvalidRange)
    ));
    assert_eq!(
        ObjectRange::new(u64::MAX, 1),
        Err(StorageError::InvalidRange)
    );

    for suffix in ["list/a", "list/c", "list/b/nested"] {
        let key = namespaced_key(namespace, suffix)?;
        let mut source = Cursor::new(key.as_str().as_bytes().to_vec());
        storage.write_stream(&key, &mut source, ObjectWriteOptions::if_absent())?;
    }
    let outside_key = namespaced_key(namespace, "listing-outside")?;
    let mut outside_source = Cursor::new(b"outside".to_vec());
    storage.write_stream(
        &outside_key,
        &mut outside_source,
        ObjectWriteOptions::if_absent(),
    )?;

    let list_prefix = format!("{namespace}/list");
    let prefix = ObjectPrefix::new(format!("{list_prefix}/"))?;
    let mut request = ObjectListRequest::new(prefix).with_limit(2);
    let mut listed = Vec::new();
    let mut page_count = 0;
    loop {
        let page = storage.list_page(&request)?;
        assert!(page.objects().len() <= 2);
        page_count += 1;
        listed.extend(
            page.objects()
                .iter()
                .map(|object| object.key().as_str().to_owned()),
        );
        let Some(cursor) = page.next_cursor().cloned() else {
            break;
        };
        request = request.with_cursor(cursor);
    }
    assert!(page_count >= 2);
    assert_eq!(
        listed,
        vec![
            format!("{list_prefix}/a"),
            format!("{list_prefix}/b/nested"),
            format!("{list_prefix}/c"),
        ]
    );
    assert_eq!(storage.list_objects(&list_prefix)?, listed);
    let root_page = storage.list_page(&ObjectListRequest::root().with_limit(1000))?;
    assert!(
        root_page
            .objects()
            .iter()
            .any(|object| object.key().as_str() == format!("{list_prefix}/a"))
    );

    storage.delete(&empty_key)?;
    assert_eq!(storage.metadata(&empty_key), Err(StorageError::NotFound));
    assert_eq!(storage.delete(&empty_key), Err(StorageError::NotFound));

    let conditional_key = namespaced_key(namespace, "conditional")?;
    let mut initial_source = Cursor::new(b"initial".to_vec());
    let initial = storage.write_stream(
        &conditional_key,
        &mut initial_source,
        ObjectWriteOptions::if_absent(),
    )?;
    let initial_version = initial
        .version()
        .cloned()
        .ok_or("conditional test requires a version")?;
    let barrier = Arc::new(Barrier::new(2));
    let first_storage = storage.clone();
    let first_barrier = barrier.clone();
    let first_key = conditional_key.clone();
    let first_version = initial_version.clone();
    let first = thread::spawn(move || {
        first_barrier.wait();
        let mut source = Cursor::new(b"winner-one".to_vec());
        first_storage.write_stream(
            &first_key,
            &mut source,
            ObjectWriteOptions::if_version(first_version),
        )
    });
    let second_storage = storage.clone();
    let second_barrier = barrier;
    let second_key = conditional_key.clone();
    let second = thread::spawn(move || {
        second_barrier.wait();
        let mut source = Cursor::new(b"winner-two".to_vec());
        second_storage.write_stream(
            &second_key,
            &mut source,
            ObjectWriteOptions::if_version(initial_version),
        )
    });
    let results = [
        first.join().map_err(|_| "first writer panicked")?,
        second.join().map_err(|_| "second writer panicked")?,
    ];
    assert_eq!(results.iter().filter(|result| result.is_ok()).count(), 1);
    assert_eq!(
        results
            .iter()
            .filter(|result| matches!(result, Err(StorageError::Conflict)))
            .count(),
        1
    );

    let mut failed_source = InterruptingReader::new(2);
    assert_eq!(
        storage.write_stream(
            &namespaced_key(namespace, "cancelled")?,
            &mut failed_source,
            ObjectWriteOptions::if_absent(),
        ),
        Err(StorageError::Cancelled)
    );
    assert_eq!(
        storage.read(&namespaced_key(namespace, "cancelled")?.into_string()),
        Err(StorageError::NotFound)
    );

    let mut size_mismatch_source = Cursor::new(b"short".to_vec());
    assert_eq!(
        storage.write_stream(
            &namespaced_key(namespace, "size-mismatch")?,
            &mut size_mismatch_source,
            ObjectWriteOptions::if_absent().with_expected_size(100),
        ),
        Err(StorageError::InvalidRequest)
    );
    Ok(())
}

fn namespaced_key(namespace: &str, suffix: &str) -> StorageResult<ObjectKey> {
    ObjectKey::new(format!("{namespace}/{suffix}"))
}

fn unique_storage_namespace(prefix: &str) -> String {
    format!(
        "{prefix}/{process}-{sequence}",
        process = std::process::id(),
        sequence = NEXT_DIRECTORY.fetch_add(1, Ordering::Relaxed),
    )
}

#[test]
fn memory_storage_runs_the_shared_contract_suite() -> Result<(), Box<dyn Error>> {
    run_storage_contract(MemoryStorage::new())
}

#[test]
fn local_storage_runs_the_shared_contract_suite() -> Result<(), Box<dyn Error>> {
    let directory = TestDirectory::new()?;
    let storage = LocalStorage::new(directory.path())?;
    run_storage_contract_in_namespace(storage, &unique_storage_namespace("s3-contract"))
}

#[cfg(feature = "s3")]
#[test]
fn s3_storage_runs_the_shared_contract_suite_when_configured() -> Result<(), Box<dyn Error>> {
    let Some(storage) = s3_storage_from_environment()? else {
        eprintln!("skipping S3 contract: GIB_S3_TEST_* environment is not configured");
        return Ok(());
    };
    run_storage_contract(storage)
}

#[cfg(feature = "s3")]
#[test]
fn s3_storage_supports_multipart_boundary_ranges_and_cancellation() -> Result<(), Box<dyn Error>> {
    let Some(storage) = s3_storage_from_environment()? else {
        eprintln!("skipping S3 multipart test: GIB_S3_TEST_* environment is not configured");
        return Ok(());
    };
    let namespace = unique_storage_namespace("s3-contract");
    let multipart_key = namespaced_key(&namespace, "multipart")?;
    match storage.delete(&multipart_key) {
        Ok(()) | Err(StorageError::NotFound) => {}
        Err(error) => return Err(error.into()),
    }
    let part_size = usize::try_from(storage.config().multipart_part_size())?;
    let size = part_size
        .checked_mul(3)
        .and_then(|value| value.checked_add(19))
        .ok_or("multipart test size overflow")?;
    let requests = Arc::new(Mutex::new(Vec::new()));
    let mut source = PatternReader::new(size, 13_777, requests.clone());
    let metadata = storage.write_stream(
        &multipart_key,
        &mut source,
        ObjectWriteOptions::if_absent().with_expected_size(size as u64),
    )?;
    assert_eq!(metadata.size(), size as u64);
    assert!(
        requests
            .lock()
            .map_err(|_| "request lock poisoned")?
            .iter()
            .all(|size| *size <= STORAGE_TRANSFER_BUFFER_SIZE)
    );

    let start = part_size - 31;
    let range = ObjectRange::new(start as u64, 79)?;
    let mut range_read = storage.read_range(&multipart_key, range)?;
    let mut contents = Vec::new();
    range_read.read_to_end(&mut contents)?;
    assert_eq!(contents, pattern(size)[start..start + 79]);

    let cancelled_key = namespaced_key(&namespace, "cancelled")?;
    match storage.delete(&cancelled_key) {
        Ok(()) | Err(StorageError::NotFound) => {}
        Err(error) => return Err(error.into()),
    }
    let cancellation = CancellationHandle::new();
    let writer_storage = storage.clone();
    let writer_key = cancelled_key.clone();
    let writer_cancellation = cancellation.clone();
    let writer = thread::spawn(move || {
        let mut source = SlowPatternReader {
            length: size,
            position: 0,
            delay: std::time::Duration::from_millis(2),
        };
        writer_storage.write_stream_with_cancellation(
            &writer_key,
            &mut source,
            ObjectWriteOptions::if_absent().with_expected_size(size as u64),
            Some(&writer_cancellation),
        )
    });
    thread::sleep(std::time::Duration::from_millis(100));
    cancellation.cancel();
    assert_eq!(
        writer
            .join()
            .map_err(|_| "S3 cancellation writer panicked")?,
        Err(StorageError::Cancelled)
    );
    assert_eq!(
        storage.metadata(&cancelled_key),
        Err(StorageError::NotFound)
    );
    storage.delete(&multipart_key)?;
    Ok(())
}

#[cfg(feature = "s3")]
fn s3_storage_from_environment() -> Result<Option<S3Storage>, Box<dyn Error>> {
    let names = [
        "GIB_S3_TEST_REGION",
        "GIB_S3_TEST_BUCKET",
        "GIB_S3_TEST_ACCESS_KEY",
        "GIB_S3_TEST_SECRET_KEY",
    ];
    let [
        Some(region),
        Some(bucket),
        Some(access_key),
        Some(secret_key),
    ] = names.map(|name| std::env::var(name).ok())
    else {
        return Ok(None);
    };
    let mut config = S3StorageConfig::new(region, bucket, access_key, secret_key)?;
    if let Ok(endpoint) = std::env::var("GIB_S3_TEST_ENDPOINT") {
        config = config.with_endpoint(endpoint);
    }
    if let Ok(session_token) = std::env::var("GIB_S3_TEST_SESSION_TOKEN") {
        config = config.with_session_token(session_token);
    }
    Ok(Some(S3Storage::new(
        config
            .with_multipart_threshold(MIN_S3_MULTIPART_PART_SIZE)
            .with_multipart_part_size(DEFAULT_S3_MULTIPART_PART_SIZE),
    )?))
}

#[test]
fn local_storage_lists_root_and_canonicalizes_trailing_prefixes() -> Result<(), Box<dyn Error>> {
    let directory = TestDirectory::new()?;
    let storage = LocalStorage::new(directory.path())?;
    let key = ObjectKey::new("objects/root-list")?;
    let mut source = Cursor::new(b"root-list".to_vec());
    storage.write_stream(&key, &mut source, ObjectWriteOptions::if_absent())?;

    assert_eq!(storage.list_objects("")?, vec!["objects/root-list"]);
    assert_eq!(storage.list_objects("objects/")?, vec!["objects/root-list"]);
    Ok(())
}

#[test]
fn local_storage_faults_never_publish_partial_objects() -> Result<(), Box<dyn Error>> {
    let directory = TestDirectory::new()?;
    let storage = LocalStorage::new(directory.path())?;
    let key = ObjectKey::new("objects/atomic")?;

    storage.inject_failure(LocalStorageOperation::Write, StorageError::Transient);
    let mut source = Cursor::new(b"write-failure".to_vec());
    assert_eq!(
        storage.write_stream(&key, &mut source, ObjectWriteOptions::if_absent()),
        Err(StorageError::Transient)
    );
    assert_eq!(storage.read(key.as_str()), Err(StorageError::NotFound));

    let mut source = Cursor::new(b"old-complete-object".to_vec());
    storage.write_stream(&key, &mut source, ObjectWriteOptions::if_absent())?;

    for operation in [LocalStorageOperation::Flush, LocalStorageOperation::Rename] {
        storage.inject_failure(operation, StorageError::Transient);
        let mut replacement = Cursor::new(b"replacement-that-must-not-publish".to_vec());
        assert_eq!(
            storage.write_stream(&key, &mut replacement, ObjectWriteOptions::new()),
            Err(StorageError::Transient)
        );
        assert_eq!(storage.read(key.as_str())?, b"old-complete-object");
    }

    storage.inject_failure(
        LocalStorageOperation::DirectorySync,
        StorageError::Transient,
    );
    let mut replacement = Cursor::new(b"complete-before-directory-sync-error".to_vec());
    assert_eq!(
        storage.write_stream(&key, &mut replacement, ObjectWriteOptions::new()),
        Err(StorageError::Transient)
    );
    assert_eq!(
        storage.read(key.as_str())?,
        b"complete-before-directory-sync-error"
    );

    let mut failed = InterruptingReader::new(1);
    assert_eq!(
        storage.write_stream(&key, &mut failed, ObjectWriteOptions::new()),
        Err(StorageError::Cancelled)
    );
    assert_eq!(
        storage.read(key.as_str())?,
        b"complete-before-directory-sync-error"
    );
    Ok(())
}

#[test]
fn local_storage_rejects_invalid_and_symlinked_paths() -> Result<(), Box<dyn Error>> {
    let directory = TestDirectory::new()?;
    let storage = LocalStorage::new(directory.path())?;
    for key in [
        "../escape",
        "/absolute",
        "objects/../../escape",
        "objects\\escape",
        "objects//escape",
        "objects/escape:name",
    ] {
        assert_eq!(
            storage.read(key),
            Err(StorageError::InvalidObjectKey),
            "invalid key should be rejected: {key}"
        );
    }

    #[cfg(unix)]
    {
        let outside = directory.path().join("outside");
        std::fs::create_dir_all(&outside)?;
        let outside_file = outside.join("outside-object");
        std::fs::write(&outside_file, b"outside")?;
        std::os::unix::fs::symlink(&outside, directory.path().join("linked-parent"))?;
        std::os::unix::fs::symlink(&outside_file, directory.path().join("linked-object"))?;

        let parent_key = ObjectKey::new("linked-parent/created")?;
        assert!(matches!(
            storage.read_stream(&parent_key),
            Err(StorageError::InvalidObjectKey)
        ));
        let mut source = Cursor::new(b"must-stay-inside".to_vec());
        assert_eq!(
            storage.write_stream(&parent_key, &mut source, ObjectWriteOptions::new()),
            Err(StorageError::InvalidObjectKey)
        );
        assert_eq!(
            storage.delete(&parent_key),
            Err(StorageError::InvalidObjectKey)
        );

        let final_key = ObjectKey::new("linked-object")?;
        assert_eq!(
            storage.metadata(&final_key),
            Err(StorageError::InvalidObjectKey)
        );
        assert_eq!(
            storage.read(final_key.as_str()),
            Err(StorageError::InvalidObjectKey)
        );
        let mut source = Cursor::new(b"must-not-follow".to_vec());
        assert_eq!(
            storage.write_stream(&final_key, &mut source, ObjectWriteOptions::new()),
            Err(StorageError::InvalidObjectKey)
        );
        assert_eq!(
            storage.delete(&final_key),
            Err(StorageError::InvalidObjectKey)
        );
        assert_eq!(std::fs::read(&outside_file)?, b"outside");
        assert!(!outside.join("created").exists());
        assert!(
            !storage
                .list_objects("")?
                .iter()
                .any(|key| { key.starts_with("linked-parent/") || key == "linked-object" })
        );

        let root_link = directory.path().join("root-link");
        std::os::unix::fs::symlink(&outside, &root_link)?;
        assert_eq!(
            LocalStorage::new(&root_link),
            Err(StorageError::InvalidObjectKey)
        );
    }
    Ok(())
}

#[test]
fn local_storage_conditional_writes_are_race_safe_across_handles() -> Result<(), Box<dyn Error>> {
    let directory = TestDirectory::new()?;
    let first_storage =
        LocalStorage::new(directory.path()).map_err(|error| format!("first storage: {error:?}"))?;
    let second_storage = LocalStorage::new(directory.path())
        .map_err(|error| format!("second storage: {error:?}"))?;
    let key = ObjectKey::new("objects/cross-handle")?;
    let mut initial_source = Cursor::new(b"initial".to_vec());
    let initial = first_storage
        .write_stream(&key, &mut initial_source, ObjectWriteOptions::if_absent())
        .map_err(|error| format!("initial write: {error:?}"))?;
    let version = initial
        .version()
        .cloned()
        .ok_or("local storage should return a version")?;
    let barrier = Arc::new(Barrier::new(2));

    let first_barrier = barrier.clone();
    let first_key = key.clone();
    let first_version = version.clone();
    let first = thread::spawn(move || {
        first_barrier.wait();
        let mut source = Cursor::new(b"winner-one".to_vec());
        first_storage.write_stream(
            &first_key,
            &mut source,
            ObjectWriteOptions::if_version(first_version),
        )
    });
    let second_barrier = barrier;
    let second_key = key.clone();
    let second = thread::spawn(move || {
        second_barrier.wait();
        let mut source = Cursor::new(b"winner-two".to_vec());
        second_storage.write_stream(
            &second_key,
            &mut source,
            ObjectWriteOptions::if_version(version),
        )
    });

    let results = [
        first.join().map_err(|_| "first writer panicked")?,
        second.join().map_err(|_| "second writer panicked")?,
    ];
    assert_eq!(results.iter().filter(|result| result.is_ok()).count(), 1);
    assert_eq!(
        results
            .iter()
            .filter(|result| matches!(result, Err(StorageError::Conflict)))
            .count(),
        1
    );
    Ok(())
}

#[test]
fn capability_negotiation_and_provider_neutral_errors_are_explicit() -> Result<(), Box<dyn Error>> {
    let storage = MemoryStorage::new();
    assert!(matches!(
        ObjectKey::new("../unsafe"),
        Err(StorageError::InvalidObjectKey)
    ));
    assert!(matches!(
        ObjectPrefix::new("bad//prefix"),
        Err(StorageError::InvalidPrefix)
    ));
    assert!(matches!(
        storage.list_page(&ObjectListRequest::root().with_limit(0)),
        Err(StorageError::InvalidRequest)
    ));
    assert!(StorageCapabilities::ALL.supports(StorageCapability::RangeRead));
    assert_eq!(StorageCapabilities::from_bits(u32::MAX), None);
    assert_eq!(
        StorageError::NotFound.to_string(),
        "storage object was not found"
    );
    assert!(StorageError::RateLimited.is_retryable());
    assert!(StorageError::Transient.is_retryable());
    assert!(StorageError::Conflict.is_conflict());

    for error in [
        StorageError::NotFound,
        StorageError::Conflict,
        StorageError::UnsupportedCapability,
        StorageError::Authentication,
        StorageError::PermissionDenied,
        StorageError::RateLimited,
        StorageError::Transient,
    ] {
        storage.inject_failure(MemoryStorageOperation::Read, error);
        assert_eq!(storage.read("objects/error"), Err(error));
    }

    assert_eq!(
        StorageError::from_io_error(&io::Error::from(io::ErrorKind::PermissionDenied)),
        StorageError::PermissionDenied
    );
    assert_eq!(
        StorageError::from_io_error(&io::Error::from(io::ErrorKind::Interrupted)),
        StorageError::Cancelled
    );
    assert_eq!(
        StorageError::from_io_error(&io::Error::from(io::ErrorKind::TimedOut)),
        StorageError::Transient
    );
    assert_eq!(
        StorageError::from_http_status(401),
        StorageError::Authentication
    );
    assert_eq!(
        StorageError::from_http_status(403),
        StorageError::PermissionDenied
    );
    assert_eq!(StorageError::from_http_status(404), StorageError::NotFound);
    assert_eq!(StorageError::from_http_status(409), StorageError::Conflict);
    assert_eq!(
        StorageError::from_http_status(429),
        StorageError::RateLimited
    );
    assert_eq!(
        StorageError::from_http_status(503),
        StorageError::Unavailable
    );

    let key = ObjectKey::new("objects/injected")?;
    storage.inject_failure(MemoryStorageOperation::Write, StorageError::Transient);
    let mut source = Cursor::new(b"write".to_vec());
    assert_eq!(
        storage.write_stream(&key, &mut source, ObjectWriteOptions::new()),
        Err(StorageError::Transient)
    );
    storage.inject_failure(MemoryStorageOperation::Metadata, StorageError::RateLimited);
    assert_eq!(storage.metadata(&key), Err(StorageError::RateLimited));
    storage.inject_failure(
        MemoryStorageOperation::Range,
        StorageError::PermissionDenied,
    );
    assert!(matches!(
        storage.read_range(&key, ObjectRange::new(0, 0)?),
        Err(StorageError::PermissionDenied)
    ));
    storage.inject_failure(MemoryStorageOperation::List, StorageError::Unavailable);
    assert!(matches!(
        storage.list_page(&ObjectListRequest::root().with_limit(1)),
        Err(StorageError::Unavailable)
    ));
    storage.inject_failure(MemoryStorageOperation::Delete, StorageError::Authentication);
    assert_eq!(storage.delete(&key), Err(StorageError::Authentication));
    storage.inject_failure(
        MemoryStorageOperation::ConditionalWrite,
        StorageError::Conflict,
    );
    let mut conditional_source = Cursor::new(b"conditional".to_vec());
    assert_eq!(
        storage.write_stream(
            &key,
            &mut conditional_source,
            ObjectWriteOptions::if_absent(),
        ),
        Err(StorageError::Conflict)
    );
    Ok(())
}

#[test]
fn legacy_backends_must_advertise_and_implement_optional_capabilities() -> Result<(), Box<dyn Error>>
{
    let storage = LegacyStorage {
        inner: MemoryStorage::new(),
    };
    assert_eq!(storage.capabilities(), StorageCapabilities::NONE);
    let key = ObjectKey::new("legacy/object")?;
    assert!(matches!(
        storage.read_stream(&key),
        Err(StorageError::UnsupportedCapability)
    ));
    assert!(matches!(
        storage.read_range(&key, ObjectRange::new(0, 0)?),
        Err(StorageError::UnsupportedCapability)
    ));
    assert!(matches!(
        storage.metadata(&key),
        Err(StorageError::UnsupportedCapability)
    ));
    assert!(matches!(
        storage.delete(&key),
        Err(StorageError::UnsupportedCapability)
    ));
    Ok(())
}

struct LegacyStorage {
    inner: MemoryStorage,
}

impl RepositoryStorage for LegacyStorage {
    fn create_if_absent(&self, object_key: &str, contents: &[u8]) -> StorageResult<()> {
        self.inner.create_if_absent(object_key, contents)
    }

    fn read(&self, object_key: &str) -> StorageResult<Vec<u8>> {
        self.inner.read(object_key)
    }
}

struct PatternReader {
    length: usize,
    position: usize,
    chunk_size: usize,
    requests: Arc<Mutex<Vec<usize>>>,
}

impl PatternReader {
    fn new(length: usize, chunk_size: usize, requests: Arc<Mutex<Vec<usize>>>) -> Self {
        Self {
            length,
            position: 0,
            chunk_size,
            requests,
        }
    }
}

impl Read for PatternReader {
    fn read(&mut self, buffer: &mut [u8]) -> io::Result<usize> {
        self.requests
            .lock()
            .map_err(|_| io::Error::other("request lock poisoned"))?
            .push(buffer.len());
        if self.position == self.length || buffer.is_empty() {
            return Ok(0);
        }
        let amount = (self.length - self.position)
            .min(self.chunk_size)
            .min(buffer.len());
        for (offset, byte) in buffer[..amount].iter_mut().enumerate() {
            *byte = ((self.position + offset) % 251) as u8;
        }
        self.position += amount;
        Ok(amount)
    }
}

struct InterruptingReader {
    successful_reads: usize,
    position: usize,
}

#[cfg(feature = "s3")]
struct SlowPatternReader {
    length: usize,
    position: usize,
    delay: std::time::Duration,
}

#[cfg(feature = "s3")]
impl Read for SlowPatternReader {
    fn read(&mut self, buffer: &mut [u8]) -> io::Result<usize> {
        if self.position == self.length || buffer.is_empty() {
            return Ok(0);
        }
        let amount = (self.length - self.position).min(buffer.len());
        for (offset, byte) in buffer[..amount].iter_mut().enumerate() {
            *byte = ((self.position + offset) % 251) as u8;
        }
        self.position += amount;
        thread::sleep(self.delay);
        Ok(amount)
    }
}

impl InterruptingReader {
    fn new(successful_reads: usize) -> Self {
        Self {
            successful_reads,
            position: 0,
        }
    }
}

impl Read for InterruptingReader {
    fn read(&mut self, buffer: &mut [u8]) -> io::Result<usize> {
        if self.position >= self.successful_reads {
            return Err(io::Error::from(io::ErrorKind::Interrupted));
        }
        let amount = buffer.len().min(8);
        buffer[..amount].fill(self.position as u8);
        self.position += 1;
        Ok(amount)
    }
}

fn pattern(length: usize) -> Vec<u8> {
    (0..length).map(|position| (position % 251) as u8).collect()
}

struct TestDirectory {
    path: PathBuf,
}

impl TestDirectory {
    fn new() -> Result<Self, Box<dyn Error>> {
        let sequence = NEXT_DIRECTORY.fetch_add(1, Ordering::Relaxed);
        let path = std::env::temp_dir().join(format!(
            "gib-storage-contract-{}-{sequence}",
            std::process::id()
        ));
        std::fs::create_dir_all(&path)?;
        Ok(Self { path })
    }

    fn path(&self) -> &std::path::Path {
        &self.path
    }
}

impl Drop for TestDirectory {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.path);
    }
}
