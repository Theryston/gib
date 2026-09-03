use gib::{
    CURRENT_REPOSITORY_BOOTSTRAP_VERSION, CURRENT_REPOSITORY_FORMAT_VERSION, CancellationHandle,
    Client, LocalStorage, MemoryStorage, Repository, RepositoryIdentity, RepositoryInitRequest,
    RepositoryKey, RepositoryObject, RepositoryOpenRequest, RepositoryStorage, SdkError,
    SnapshotPublication, SnapshotReference, StorageError, StorageVersion, VersionedObject,
};
use serde::{Deserialize, Serialize, de::DeserializeOwned};
use std::error::Error;
use std::fs;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Barrier};
use std::thread;

static NEXT_TEST_DIRECTORY: AtomicU64 = AtomicU64::new(1);

#[test]
fn memory_initialize_and_open_round_trip_without_snapshot_or_indexes() -> Result<(), Box<dyn Error>>
{
    let storage = MemoryStorage::new();
    let identity = RepositoryIdentity::new("memory-round-trip")?;
    let repository_key = RepositoryKey::new("default")?;
    let request = RepositoryInitRequest::new(identity.clone(), repository_key.clone());

    let initialized = Client::default().initialize_repository(storage.clone(), request)?;
    assert_eq!(initialized.identity(), &identity);
    assert_eq!(initialized.repository_key(), &repository_key);
    assert_eq!(initialized.format_version(), 1);
    assert_eq!(initialized.descriptor_version(), 1);
    assert!(!initialized.has_published_snapshot());
    let format_bytes = storage.read_object("format")?;
    let descriptor_bytes = storage.read_object("config/repository")?;
    assert_eq!(format_bytes.first().copied(), Some(0x84));
    assert_eq!(descriptor_bytes.first().copied(), Some(0x87));
    assert_ne!(format_bytes.first().copied(), Some(b'{'));
    assert_ne!(descriptor_bytes.first().copied(), Some(b'{'));
    assert_eq!(
        storage.objects()?,
        vec![String::from("config/repository"), String::from("format")]
    );

    let opened = Client::default().open_repository(
        storage,
        RepositoryOpenRequest::for_repository(identity, repository_key),
    )?;
    assert_eq!(opened, initialized);
    Ok(())
}

#[test]
fn local_initialize_and_open_round_trip_creates_only_required_root_objects()
-> Result<(), Box<dyn Error>> {
    let directory = TestDirectory::new();
    let storage = LocalStorage::new(directory.path())?;
    let identity = RepositoryIdentity::new("local-round-trip")?;
    let repository_key = RepositoryKey::new("laptop")?;

    let initialized = Client::default().initialize_repository(
        storage.clone(),
        RepositoryInitRequest::new(identity.clone(), repository_key.clone()),
    )?;
    let opened = Client::default().open_repository(storage, RepositoryOpenRequest::new())?;

    assert_eq!(opened.identity(), &identity);
    assert_eq!(opened.repository_key(), &repository_key);
    assert_eq!(opened.format_version(), initialized.format_version());
    assert!(directory.path().join("format").is_file());
    assert!(directory.path().join("config/repository").is_file());
    assert_ne!(
        fs::read(directory.path().join("format"))?.first().copied(),
        Some(b'{')
    );
    assert_ne!(
        fs::read(directory.path().join("config/repository"))?
            .first()
            .copied(),
        Some(b'{')
    );
    assert!(!directory.path().join("refs/latest").exists());
    assert!(!directory.path().join("indexes").exists());
    Ok(())
}

#[test]
fn repeated_initialization_is_a_conflict_and_preserves_existing_bytes() -> Result<(), Box<dyn Error>>
{
    let storage = MemoryStorage::new();
    let request = RepositoryInitRequest::new(
        RepositoryIdentity::new("existing-repository")?,
        RepositoryKey::new("default")?,
    );
    Client::default().initialize_repository(storage.clone(), request.clone())?;
    let format_before = storage.read_object("format")?;
    let descriptor_before = storage.read_object("config/repository")?;

    let error = Client::default()
        .initialize_repository(storage.clone(), request)
        .expect_err("a second initialization must conflict");
    assert_eq!(error, SdkError::RepositoryAlreadyExists);
    assert_eq!(storage.read_object("format")?, format_before);
    assert_eq!(storage.read_object("config/repository")?, descriptor_before);
    Ok(())
}

#[test]
fn initialization_detects_an_existing_descriptor_before_creating_other_roots()
-> Result<(), Box<dyn Error>> {
    let storage = MemoryStorage::new();
    storage.put("config/repository", b"pre-existing-data")?;
    let error = Client::default()
        .initialize_repository(
            storage.clone(),
            RepositoryInitRequest::new(
                RepositoryIdentity::new("pre-existing")?,
                RepositoryKey::new("default")?,
            ),
        )
        .expect_err("an existing descriptor must make initialization conflict");
    assert_eq!(error, SdkError::RepositoryAlreadyExists);
    assert_eq!(storage.objects()?, vec![String::from("config/repository")]);
    assert_eq!(
        storage.read_object("config/repository")?,
        b"pre-existing-data"
    );
    Ok(())
}

#[test]
fn opening_missing_roots_returns_a_typed_missing_error() {
    let storage = MemoryStorage::new();
    let error = Client::default()
        .open_repository(storage, RepositoryOpenRequest::new())
        .expect_err("an empty storage is not a repository");
    assert_eq!(error, SdkError::RepositoryMissing);
}

#[test]
fn opening_truncated_or_corrupted_objects_returns_malformed_without_repair()
-> Result<(), Box<dyn Error>> {
    let storage = MemoryStorage::new();
    Client::default().initialize_repository(
        storage.clone(),
        RepositoryInitRequest::new(
            RepositoryIdentity::new("corruption-test")?,
            RepositoryKey::new("default")?,
        ),
    )?;

    storage.replace_object("config/repository", [0x87])?;
    let truncated = Client::default()
        .open_repository(storage.clone(), RepositoryOpenRequest::new())
        .expect_err("truncated descriptor must fail");
    assert!(matches!(truncated, SdkError::RepositoryMalformed { .. }));
    assert_eq!(storage.read_object("config/repository")?, [0x87]);

    storage.replace_object("format", [0xc1])?;
    let corrupted = Client::default()
        .open_repository(storage, RepositoryOpenRequest::new())
        .expect_err("corrupted bootstrap record must fail");
    assert!(matches!(corrupted, SdkError::RepositoryMalformed { .. }));
    Ok(())
}

#[test]
fn opening_invalid_magic_or_root_references_returns_malformed() -> Result<(), Box<dyn Error>> {
    let storage = MemoryStorage::new();
    Client::default().initialize_repository(
        storage.clone(),
        RepositoryInitRequest::new(
            RepositoryIdentity::new("root-validation")?,
            RepositoryKey::new("default")?,
        ),
    )?;

    let mut bootstrap = decode_fixture::<TestBootstrap>(&storage.read_object("format")?)?;
    bootstrap.magic = String::from("OTHER");
    storage.replace_object("format", encode_fixture(&bootstrap)?)?;
    let magic_error = Client::default()
        .open_repository(storage.clone(), RepositoryOpenRequest::new())
        .expect_err("invalid magic must fail");
    assert!(matches!(magic_error, SdkError::RepositoryMalformed { .. }));

    let mut bootstrap = decode_fixture::<TestBootstrap>(&storage.read_object("format")?)?;
    bootstrap.descriptor = String::from("wrong");
    storage.replace_object("format", encode_fixture(&bootstrap)?)?;
    let root_error = Client::default()
        .open_repository(storage, RepositoryOpenRequest::new())
        .expect_err("invalid root reference must fail");
    assert!(matches!(root_error, SdkError::RepositoryMalformed { .. }));

    let descriptor_storage = MemoryStorage::new();
    Client::default().initialize_repository(
        descriptor_storage.clone(),
        RepositoryInitRequest::new(
            RepositoryIdentity::new("descriptor-root-validation")?,
            RepositoryKey::new("default")?,
        ),
    )?;
    let mut descriptor =
        decode_fixture::<TestDescriptor>(&descriptor_storage.read_object("config/repository")?)?;
    descriptor.roots.format = String::from("wrong");
    descriptor_storage.replace_object("config/repository", encode_fixture(&descriptor)?)?;
    let descriptor_root_error = Client::default()
        .open_repository(descriptor_storage, RepositoryOpenRequest::new())
        .expect_err("descriptor root references must be validated");
    assert!(matches!(
        descriptor_root_error,
        SdkError::RepositoryMalformed { .. }
    ));
    Ok(())
}

#[test]
fn unknown_format_and_descriptor_versions_fail_explicitly() -> Result<(), Box<dyn Error>> {
    let storage = MemoryStorage::new();
    storage.put(
        "format",
        encode_fixture(&TestBootstrap {
            bootstrap_version: CURRENT_REPOSITORY_BOOTSTRAP_VERSION,
            magic: String::from("GIB"),
            format_version: 99,
            descriptor: String::from("config/repository"),
        })?,
    )?;
    storage.put("config/repository", b"legacy-data")?;
    let format_error = Client::default()
        .open_repository(storage.clone(), RepositoryOpenRequest::new())
        .expect_err("unknown marker versions must not use a fallback");
    assert_eq!(
        format_error,
        SdkError::RepositoryUnsupportedVersion { version: 99 }
    );

    let descriptor_storage = MemoryStorage::new();
    Client::default().initialize_repository(
        descriptor_storage.clone(),
        RepositoryInitRequest::new(
            RepositoryIdentity::new("version-test")?,
            RepositoryKey::new("default")?,
        ),
    )?;
    let mut descriptor =
        decode_fixture::<TestDescriptor>(&descriptor_storage.read_object("config/repository")?)?;
    descriptor.descriptor_version = 99;
    descriptor_storage.replace_object("config/repository", encode_fixture(&descriptor)?)?;
    let descriptor_error = Client::default()
        .open_repository(descriptor_storage, RepositoryOpenRequest::new())
        .expect_err("unknown descriptor versions must be explicit");
    assert_eq!(
        descriptor_error,
        SdkError::RepositoryUnsupportedVersion { version: 99 }
    );
    Ok(())
}

#[test]
fn missing_required_feature_and_mismatched_identity_are_incompatible() -> Result<(), Box<dyn Error>>
{
    let storage = MemoryStorage::new();
    let identity = RepositoryIdentity::new("incompatible-test")?;
    let repository_key = RepositoryKey::new("default")?;
    Client::default().initialize_repository(
        storage.clone(),
        RepositoryInitRequest::new(identity.clone(), repository_key.clone()),
    )?;

    let mut descriptor =
        decode_fixture::<TestDescriptor>(&storage.read_object("config/repository")?)?;
    descriptor.required_features = vec![String::from("future.feature")];
    storage.replace_object("config/repository", encode_fixture(&descriptor)?)?;
    let feature_error = Client::default()
        .open_repository(storage.clone(), RepositoryOpenRequest::new())
        .expect_err("unknown required features must be incompatible");
    assert!(matches!(
        feature_error,
        SdkError::RepositoryIncompatible { .. }
    ));

    let missing_feature_storage = MemoryStorage::new();
    Client::default().initialize_repository(
        missing_feature_storage.clone(),
        RepositoryInitRequest::new(
            RepositoryIdentity::new("missing-feature")?,
            RepositoryKey::new("default")?,
        ),
    )?;
    let mut descriptor = decode_fixture::<TestDescriptor>(
        &missing_feature_storage.read_object("config/repository")?,
    )?;
    descriptor.required_features.clear();
    missing_feature_storage.replace_object("config/repository", encode_fixture(&descriptor)?)?;
    let missing_feature_error = Client::default()
        .open_repository(missing_feature_storage, RepositoryOpenRequest::new())
        .expect_err("a missing required feature must be incompatible");
    assert!(matches!(
        missing_feature_error,
        SdkError::RepositoryIncompatible { .. }
    ));

    let valid_storage = MemoryStorage::new();
    Client::default().initialize_repository(
        valid_storage.clone(),
        RepositoryInitRequest::new(identity, repository_key),
    )?;
    let wrong_identity = RepositoryIdentity::new("another-repository")?;
    let identity_error = Client::default()
        .open_repository(
            valid_storage,
            RepositoryOpenRequest::for_identity(wrong_identity),
        )
        .expect_err("an unexpected identity must be incompatible");
    assert!(matches!(
        identity_error,
        SdkError::RepositoryIncompatible { .. }
    ));
    Ok(())
}

#[test]
fn persisted_first_bootstrap_and_descriptor_fixtures_open() -> Result<(), Box<dyn Error>> {
    let storage = MemoryStorage::new();
    storage.put(
        "format",
        include_bytes!("../../../tests/fixtures/repository/v1/format"),
    )?;
    storage.put(
        "config/repository",
        include_bytes!("../../../tests/fixtures/repository/v1/config/repository"),
    )?;
    let repository = Client::default().open_repository(storage, RepositoryOpenRequest::new())?;
    assert_eq!(repository.identity().as_str(), "fixture-repository");
    assert_eq!(repository.repository_key().as_str(), "fixture-key");
    assert_eq!(repository.format_version(), 1);
    Ok(())
}

#[test]
fn persisted_head_fixture_reads_with_its_generation_and_snapshot() -> Result<(), Box<dyn Error>> {
    let storage = MemoryStorage::new();
    storage.put(
        "format",
        include_bytes!("../../../tests/fixtures/repository/v1/format"),
    )?;
    storage.put(
        "config/repository",
        include_bytes!("../../../tests/fixtures/repository/v1/config/repository"),
    )?;
    storage.put(
        "refs/latest",
        include_bytes!("../../../tests/fixtures/repository/v1/refs/latest"),
    )?;

    let repository = Client::default().open_repository(storage, RepositoryOpenRequest::new())?;
    let head = repository.read_head()?;
    assert_eq!(head.generation(), 1);
    assert_eq!(
        head.snapshot().map(SnapshotReference::as_str),
        Some("snapshots/fixture")
    );
    Ok(())
}

#[test]
fn opening_trailing_or_oversized_messagepack_is_rejected() -> Result<(), Box<dyn Error>> {
    let storage = MemoryStorage::new();
    Client::default().initialize_repository(
        storage.clone(),
        RepositoryInitRequest::new(
            RepositoryIdentity::new("bounded-input")?,
            RepositoryKey::new("default")?,
        ),
    )?;

    let mut trailing = storage.read_object("format")?;
    trailing.push(0);
    storage.replace_object("format", trailing)?;
    let trailing_error = Client::default()
        .open_repository(storage.clone(), RepositoryOpenRequest::new())
        .expect_err("trailing MessagePack bytes must fail");
    assert!(matches!(
        trailing_error,
        SdkError::RepositoryMalformed { .. }
    ));

    let oversized = vec![0; 16 * 1024];
    storage.replace_object("format", oversized)?;
    let oversized_error = Client::default()
        .open_repository(storage, RepositoryOpenRequest::new())
        .expect_err("oversized MessagePack input must fail");
    assert!(matches!(
        oversized_error,
        SdkError::RepositoryMalformed { .. }
    ));
    Ok(())
}

#[test]
fn an_unknown_bootstrap_version_is_not_a_legacy_fallback() -> Result<(), Box<dyn Error>> {
    let storage = MemoryStorage::new();
    storage.put(
        "format",
        encode_fixture(&TestBootstrap {
            bootstrap_version: 99,
            magic: String::from("GIB"),
            format_version: CURRENT_REPOSITORY_FORMAT_VERSION,
            descriptor: String::from("config/repository"),
        })?,
    )?;
    storage.put("config/repository", [0xc1])?;

    let error = Client::default()
        .open_repository(storage, RepositoryOpenRequest::new())
        .expect_err("unknown bootstrap versions must fail explicitly");
    assert_eq!(
        error,
        SdkError::RepositoryUnsupportedVersion { version: 99 }
    );
    Ok(())
}

#[test]
fn concurrent_initialization_has_one_winner_and_one_conflict() -> Result<(), Box<dyn Error>> {
    let storage = MemoryStorage::new();
    let barrier = Arc::new(Barrier::new(2));
    let mut workers = Vec::new();
    for _ in 0..2 {
        let storage = storage.clone();
        let barrier = barrier.clone();
        workers.push(thread::spawn(move || {
            let client = Client::default();
            let identity =
                RepositoryIdentity::new("concurrent-repository").map_err(SdkError::from)?;
            let repository_key = RepositoryKey::new("default").map_err(SdkError::from)?;
            barrier.wait();
            client.initialize_repository(
                storage,
                RepositoryInitRequest::new(identity, repository_key),
            )
        }));
    }

    let mut successes = 0;
    let mut conflicts = 0;
    for worker in workers {
        match worker.join().map_err(|_| "worker panicked")? {
            Ok(_) => successes += 1,
            Err(SdkError::RepositoryAlreadyExists) => conflicts += 1,
            Err(error) => return Err(format!("unexpected initialization error: {error}").into()),
        }
    }
    assert_eq!(successes, 1);
    assert_eq!(conflicts, 1);
    assert_eq!(storage.objects()?.len(), 2);
    Ok(())
}

#[test]
fn first_and_later_publications_advance_head_generation_and_persist_across_open()
-> Result<(), Box<dyn Error>> {
    let storage = MemoryStorage::new();
    let repository = initialize_repository(storage.clone())?;
    let first = snapshot("snapshots/first")?;
    let second = snapshot("snapshots/second")?;
    let required = RepositoryObject::new("trees/first")?;
    storage.put(first.as_str(), b"snapshot-first")?;
    storage.put(second.as_str(), b"snapshot-second")?;
    storage.put(required.as_str(), b"tree-first")?;

    let empty = repository.read_head()?;
    assert_eq!(empty.generation(), 0);
    assert!(empty.snapshot().is_none());
    assert!(empty.version().is_none());

    let first_head = repository.publish_snapshot(
        &empty,
        SnapshotPublication::with_required_objects(first.clone(), [required.clone()]),
    )?;
    assert_eq!(first_head.generation(), 1);
    assert_eq!(first_head.snapshot(), Some(&first));
    assert!(first_head.version().is_some());

    let second_head = repository.publish_snapshot(&first_head, second.clone())?;
    assert_eq!(second_head.generation(), 2);
    assert_eq!(second_head.snapshot(), Some(&second));

    let reopened = Client::default().open_repository(storage, RepositoryOpenRequest::new())?;
    let persisted = reopened.read_head()?;
    assert_eq!(persisted.generation(), 2);
    assert_eq!(persisted.snapshot(), Some(&second));
    assert!(reopened.has_published_snapshot());
    Ok(())
}

#[test]
fn local_publication_replaces_head_atomically_and_reopens() -> Result<(), Box<dyn Error>> {
    let directory = TestDirectory::new();
    let storage = LocalStorage::new(directory.path())?;
    let repository = initialize_repository(storage.clone())?;
    let first = snapshot("snapshots/local-first")?;
    let second = snapshot("snapshots/local-second")?;
    storage.create_if_absent(first.as_str(), b"snapshot-first")?;
    storage.create_if_absent(second.as_str(), b"snapshot-second")?;

    let first_head = repository.publish_snapshot(&repository.read_head()?, first)?;
    let second_head = repository.publish_snapshot(&first_head, second.clone())?;
    assert_eq!(second_head.generation(), 2);
    assert_eq!(second_head.snapshot(), Some(&second));

    let reopened = Client::default().open_repository(storage, RepositoryOpenRequest::new())?;
    assert_eq!(reopened.read_head()?.snapshot(), Some(&second));
    assert!(directory.path().join("refs/latest").is_file());
    Ok(())
}

#[test]
fn independent_local_storage_handles_serialize_head_cas() -> Result<(), Box<dyn Error>> {
    let directory = TestDirectory::new();
    let first_storage = LocalStorage::new(directory.path())?;
    let second_storage = LocalStorage::new(directory.path())?;
    let repository = initialize_repository(first_storage.clone())?;
    let first_repository =
        Client::default().open_repository(first_storage, RepositoryOpenRequest::new())?;
    let second_repository =
        Client::default().open_repository(second_storage, RepositoryOpenRequest::new())?;
    let first = snapshot("snapshots/independent-first")?;
    let second = snapshot("snapshots/independent-second")?;
    let storage = repository.storage();
    storage
        .as_storage()
        .create_if_absent(first.as_str(), b"snapshot-first")?;
    storage
        .as_storage()
        .create_if_absent(second.as_str(), b"snapshot-second")?;
    let first_expected = first_repository.read_head()?;
    let second_expected = second_repository.read_head()?;
    let barrier = Arc::new(Barrier::new(2));

    let first_barrier = barrier.clone();
    let first_worker = thread::spawn(move || {
        first_barrier.wait();
        first_repository.publish_snapshot(&first_expected, first)
    });
    let second_barrier = barrier;
    let second_worker = thread::spawn(move || {
        second_barrier.wait();
        second_repository.publish_snapshot(&second_expected, second)
    });

    let first_result = first_worker
        .join()
        .map_err(|_| "first publisher panicked")?;
    let second_result = second_worker
        .join()
        .map_err(|_| "second publisher panicked")?;
    let results = [first_result, second_result];
    assert_eq!(results.iter().filter(|result| result.is_ok()).count(), 1);
    assert_eq!(
        results
            .iter()
            .filter(|result| matches!(result, Err(SdkError::RepositoryPublicationConflict)))
            .count(),
        1
    );
    Ok(())
}

#[test]
fn stale_head_publication_returns_a_typed_conflict_and_keeps_the_winner()
-> Result<(), Box<dyn Error>> {
    let storage = MemoryStorage::new();
    let repository = initialize_repository(storage.clone())?;
    let first = snapshot("snapshots/stale-first")?;
    let second = snapshot("snapshots/stale-second")?;
    storage.put(first.as_str(), b"snapshot-first")?;
    storage.put(second.as_str(), b"snapshot-second")?;
    let expected = repository.read_head()?;

    let winner = repository.publish_snapshot(&expected, first.clone())?;
    let conflict = repository
        .publish_snapshot(&expected, second)
        .expect_err("the stale version must lose the CAS");
    assert_eq!(conflict, SdkError::RepositoryPublicationConflict);
    assert_eq!(repository.read_head()?.head(), winner.head());
    assert_eq!(repository.read_head()?.generation(), 1);
    Ok(())
}

#[test]
fn publication_conflict_context_contains_expected_and_current_heads() -> Result<(), Box<dyn Error>>
{
    let storage = MemoryStorage::new();
    let repository = initialize_repository(storage.clone())?;
    let first = snapshot("snapshots/context-first")?;
    let second = snapshot("snapshots/context-second")?;
    storage.put(first.as_str(), b"snapshot-first")?;
    storage.put(second.as_str(), b"snapshot-second")?;
    let expected = repository.read_head()?;

    let winner = repository.publish_snapshot(&expected, first)?;
    let conflict = repository
        .publish_head(
            &expected,
            SnapshotPublication::new(second).with_conflict_context(),
        )
        .expect_err("the stale version must return reconciliation context");
    match conflict {
        SdkError::RepositoryPublicationConflictContext {
            expected: observed_expected,
            current: Some(current),
        } => {
            assert_eq!(*observed_expected, expected);
            assert_eq!(*current, winner);
        }
        other => panic!("expected publication conflict context, got {other:?}"),
    }
    assert_eq!(repository.read_head()?, winner);
    Ok(())
}

#[test]
fn missing_snapshot_dependencies_cannot_publish_head() -> Result<(), Box<dyn Error>> {
    let storage = MemoryStorage::new();
    let repository = initialize_repository(storage.clone())?;
    let target = snapshot("snapshots/missing-dependency")?;
    let required = RepositoryObject::new("trees/missing")?;
    storage.put(target.as_str(), b"snapshot")?;
    let expected = repository.read_head()?;

    let error = repository
        .publish_snapshot(
            &expected,
            SnapshotPublication::with_required_objects(target, [required]),
        )
        .expect_err("a snapshot with missing immutable dependencies is invalid");
    assert_eq!(error, SdkError::RepositoryRequiredObjectMissing);
    assert_eq!(
        storage.read_object("refs/latest"),
        Err(StorageError::NotFound)
    );
    Ok(())
}

#[test]
fn missing_target_snapshot_cannot_publish_head() -> Result<(), Box<dyn Error>> {
    let storage = MemoryStorage::new();
    let repository = initialize_repository(storage.clone())?;
    let expected = repository.read_head()?;

    let error = repository
        .publish_snapshot(&expected, snapshot("snapshots/missing")?)
        .expect_err("the target snapshot must exist before publication");
    assert_eq!(error, SdkError::RepositorySnapshotMissing);
    assert_eq!(
        storage.read_object("refs/latest"),
        Err(StorageError::NotFound)
    );
    Ok(())
}

#[test]
fn cancellation_before_cas_leaves_head_absent() -> Result<(), Box<dyn Error>> {
    let storage = MemoryStorage::new();
    let repository = initialize_repository(storage.clone())?;
    let target = snapshot("snapshots/cancelled")?;
    storage.put(target.as_str(), b"snapshot-cancelled")?;
    let cancellation = CancellationHandle::new();
    assert!(cancellation.cancel());

    let error = repository
        .publish_snapshot_with_cancellation(&repository.read_head()?, target, Some(&cancellation))
        .expect_err("a cancelled publication must not write HEAD");
    assert_eq!(error, SdkError::OperationCancelled { operation_id: None });
    assert_eq!(
        storage.read_object("refs/latest"),
        Err(StorageError::NotFound)
    );
    Ok(())
}

#[test]
fn injected_cas_failure_leaves_the_previous_head_unchanged() -> Result<(), Box<dyn Error>> {
    let backend = FailingCasStorage::new();
    let repository = initialize_repository(backend.clone())?;
    let first = snapshot("snapshots/failure-first")?;
    let second = snapshot("snapshots/failure-second")?;
    backend.inner.put(first.as_str(), b"snapshot-first")?;
    backend.inner.put(second.as_str(), b"snapshot-second")?;

    let empty = repository.read_head()?;
    let first_head = repository.publish_snapshot(&empty, first)?;
    let bytes_before = backend.inner.read_object("refs/latest")?;
    backend.fail_next_cas();

    let error = repository
        .publish_snapshot(&first_head, second)
        .expect_err("the injected final write failure must be reported");
    assert_eq!(
        error,
        SdkError::StorageFailure {
            operation: "publish_head"
        }
    );
    assert_eq!(backend.inner.read_object("refs/latest")?, bytes_before);
    assert_eq!(repository.read_head()?.generation(), 1);
    Ok(())
}

#[test]
fn a_backend_without_conditional_write_is_rejected_without_fallback() -> Result<(), Box<dyn Error>>
{
    let backend = ReadOnlyStorage::new();
    let repository = initialize_repository(backend.clone())?;
    let target = snapshot("snapshots/no-cas")?;
    backend.inner.put(target.as_str(), b"snapshot")?;

    let error = repository
        .read_head()
        .expect_err("HEAD reads require a versioned storage capability");
    assert_eq!(error, SdkError::StorageCapabilityUnsupported);
    assert_eq!(
        backend.inner.read_object("refs/latest"),
        Err(StorageError::NotFound)
    );
    Ok(())
}

#[test]
fn concurrent_publishers_using_one_prior_head_have_exactly_one_success()
-> Result<(), Box<dyn Error>> {
    let storage = MemoryStorage::new();
    let repository = initialize_repository(storage.clone())?;
    let first = snapshot("snapshots/concurrent-first")?;
    let second = snapshot("snapshots/concurrent-second")?;
    storage.put(first.as_str(), b"snapshot-first")?;
    storage.put(second.as_str(), b"snapshot-second")?;
    let expected = repository.read_head()?;
    let barrier = Arc::new(Barrier::new(2));
    let mut workers = Vec::new();
    for target in [first, second] {
        let repository = repository.clone();
        let expected = expected.clone();
        let barrier = barrier.clone();
        workers.push(thread::spawn(move || {
            barrier.wait();
            repository.publish_snapshot(&expected, target)
        }));
    }

    let mut successes = 0;
    let mut conflicts = 0;
    for worker in workers {
        match worker.join().map_err(|_| "publisher panicked")? {
            Ok(head) => {
                successes += 1;
                assert_eq!(head.generation(), 1);
            }
            Err(SdkError::RepositoryPublicationConflict) => conflicts += 1,
            Err(error) => return Err(format!("unexpected publication error: {error}").into()),
        }
    }
    assert_eq!(successes, 1);
    assert_eq!(conflicts, 1);
    assert_eq!(repository.read_head()?.generation(), 1);
    Ok(())
}

#[test]
fn corrupt_head_is_not_treated_as_an_empty_head() -> Result<(), Box<dyn Error>> {
    let storage = MemoryStorage::new();
    let repository = initialize_repository(storage.clone())?;
    let target = snapshot("snapshots/corrupt-head")?;
    storage.put(target.as_str(), b"snapshot")?;
    let _ = repository.publish_snapshot(&repository.read_head()?, target)?;
    storage.replace_object("refs/latest", [0x81])?;

    let error = repository
        .read_head()
        .expect_err("corrupt HEAD must remain an error");
    assert!(matches!(error, SdkError::RepositoryMalformed { .. }));
    let open_error = Client::default()
        .open_repository(storage, RepositoryOpenRequest::new())
        .expect_err("opening a repository with a corrupt HEAD must fail");
    assert!(matches!(open_error, SdkError::RepositoryMalformed { .. }));
    Ok(())
}

fn initialize_repository<S>(storage: S) -> Result<Repository, Box<dyn Error>>
where
    S: Into<gib::StorageHandle>,
{
    Ok(Client::default().initialize_repository(
        storage,
        RepositoryInitRequest::new(
            RepositoryIdentity::new("head-test-repository")?,
            RepositoryKey::new("default")?,
        ),
    )?)
}

fn snapshot(value: &str) -> Result<SnapshotReference, Box<dyn Error>> {
    Ok(SnapshotReference::new(value)?)
}

#[derive(Clone)]
struct FailingCasStorage {
    inner: MemoryStorage,
    fail_cas: Arc<AtomicBool>,
}

#[derive(Clone)]
struct ReadOnlyStorage {
    inner: MemoryStorage,
}

impl ReadOnlyStorage {
    fn new() -> Self {
        Self {
            inner: MemoryStorage::new(),
        }
    }
}

impl RepositoryStorage for ReadOnlyStorage {
    fn create_if_absent(&self, object_key: &str, contents: &[u8]) -> Result<(), StorageError> {
        self.inner.create_if_absent(object_key, contents)
    }

    fn read(&self, object_key: &str) -> Result<Vec<u8>, StorageError> {
        self.inner.read(object_key)
    }
}

impl FailingCasStorage {
    fn new() -> Self {
        Self {
            inner: MemoryStorage::new(),
            fail_cas: Arc::new(AtomicBool::new(false)),
        }
    }

    fn fail_next_cas(&self) {
        self.fail_cas.store(true, Ordering::Release);
    }
}

impl RepositoryStorage for FailingCasStorage {
    fn create_if_absent(&self, object_key: &str, contents: &[u8]) -> Result<(), StorageError> {
        self.inner.create_if_absent(object_key, contents)
    }

    fn read(&self, object_key: &str) -> Result<Vec<u8>, StorageError> {
        self.inner.read(object_key)
    }

    fn read_with_version(&self, object_key: &str) -> Result<VersionedObject, StorageError> {
        self.inner.read_with_version(object_key)
    }

    fn compare_and_swap(
        &self,
        object_key: &str,
        expected: Option<&StorageVersion>,
        contents: &[u8],
    ) -> Result<StorageVersion, StorageError> {
        if self.fail_cas.swap(false, Ordering::AcqRel) {
            return Err(StorageError::Io);
        }
        self.inner.compare_and_swap(object_key, expected, contents)
    }
}

struct TestDirectory {
    path: PathBuf,
}

impl TestDirectory {
    fn new() -> Self {
        let sequence = NEXT_TEST_DIRECTORY.fetch_add(1, Ordering::Relaxed);
        let path = std::env::temp_dir().join(format!(
            "gib-sdk-repository-test-{}-{sequence}",
            std::process::id()
        ));
        Self { path }
    }

    fn path(&self) -> &std::path::Path {
        &self.path
    }
}

impl Drop for TestDirectory {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.path);
    }
}

#[derive(Deserialize, Serialize)]
struct TestBootstrap {
    bootstrap_version: u16,
    magic: String,
    format_version: u16,
    descriptor: String,
}

#[derive(Deserialize, Serialize)]
struct TestDescriptor {
    descriptor_version: u16,
    magic: String,
    format_version: u16,
    repository_id: String,
    repository_key: String,
    required_features: Vec<String>,
    roots: TestRoots,
}

#[derive(Deserialize, Serialize)]
struct TestRoots {
    format: String,
    descriptor: String,
}

fn decode_fixture<T>(bytes: &[u8]) -> Result<T, Box<dyn Error>>
where
    T: DeserializeOwned,
{
    Ok(rmp_serde::from_slice(bytes)?)
}

fn encode_fixture<T>(value: &T) -> Result<Vec<u8>, Box<dyn Error>>
where
    T: Serialize,
{
    Ok(rmp_serde::to_vec_named(value)?)
}
