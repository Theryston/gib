use gib::{
    CURRENT_REPOSITORY_BOOTSTRAP_VERSION, CURRENT_REPOSITORY_FORMAT_VERSION, Client, LocalStorage,
    MemoryStorage, RepositoryIdentity, RepositoryInitRequest, RepositoryKey, RepositoryOpenRequest,
    SdkError,
};
use serde::{Deserialize, Serialize, de::DeserializeOwned};
use std::error::Error;
use std::fs;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
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
