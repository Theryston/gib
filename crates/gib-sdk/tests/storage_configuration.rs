use gib::{
    CredentialStore, CredentialStoreError, CredentialStoreOperation, MemoryCredentialStore,
    S3StorageCredentials, S3StorageSettings, StorageBackend, StorageConfiguration,
    StorageConfigurationError, StorageConfigurationOperation, StorageConfigurationStore,
    StorageCredentials, StorageName, WebDavStorageCredentials, WebDavStorageSettings,
};
use serde::Serialize;
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Barrier};
use std::thread;

static NEXT_TEST_DIRECTORY: AtomicU64 = AtomicU64::new(1);

struct TestDirectory(PathBuf);

impl TestDirectory {
    fn new() -> Result<Self, Box<dyn std::error::Error>> {
        let path = std::env::temp_dir().join(format!(
            "gib-storage-configuration-{}-{}",
            std::process::id(),
            NEXT_TEST_DIRECTORY.fetch_add(1, Ordering::Relaxed)
        ));
        fs::create_dir(&path)?;
        Ok(Self(path))
    }

    fn path(&self) -> &Path {
        &self.0
    }
}

impl Drop for TestDirectory {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

#[test]
fn configurations_round_trip_without_persisting_secrets() -> Result<(), Box<dyn std::error::Error>>
{
    let directory = TestDirectory::new()?;
    let credential_store = MemoryCredentialStore::new();
    let store = StorageConfigurationStore::new(directory.path(), credential_store.clone())?;

    let local = StorageConfiguration::local(directory.path().join("repository"))?;
    store.save("local", local.clone())?;
    assert_eq!(store.load("local")?, local);

    let s3_settings = S3StorageSettings::new("us-east-1", "gib-test-bucket")?
        .with_endpoint("https://s3.example.test")?;
    let s3_credentials = S3StorageCredentials::with_session_token(
        "recognizable-access-key",
        "recognizable-secret-key",
        Some(String::from("recognizable-session-token")),
    )?;
    let remote = StorageConfiguration::s3(s3_settings, s3_credentials)?;
    let debug = format!("{remote:?}");
    assert!(!debug.contains("recognizable-access-key"));
    assert!(!debug.contains("recognizable-secret-key"));
    assert!(!debug.contains("recognizable-session-token"));
    store.save("remote", remote.clone())?;

    let record = fs::read(store.record_path("remote")?)?;
    for secret in [
        "recognizable-access-key",
        "recognizable-secret-key",
        "recognizable-session-token",
    ] {
        assert!(
            !record
                .windows(secret.len())
                .any(|window| window == secret.as_bytes())
        );
    }

    let loaded = store.load("remote")?;
    assert_eq!(loaded.backend(), remote.backend());
    assert_eq!(loaded.credentials(), remote.credentials());
    assert!(loaded.credential_reference().is_some());
    assert_eq!(credential_store.len(), 1);

    let webdav_settings = WebDavStorageSettings::new("https://dav.example.test/collection/")?;
    let webdav = StorageConfiguration::webdav(
        webdav_settings,
        WebDavStorageCredentials::new("recognizable-user", "recognizable-password")?,
    )?;
    store.save("webdav", webdav.clone())?;
    let webdav_record = fs::read(store.record_path("webdav")?)?;
    assert!(
        !webdav_record
            .windows("recognizable-user".len())
            .any(|window| window == b"recognizable-user")
    );
    assert!(
        !webdav_record
            .windows("recognizable-password".len())
            .any(|window| window == b"recognizable-password")
    );
    assert_eq!(store.load("webdav")?.credentials(), webdav.credentials());

    let names = store.enumerate()?;
    assert_eq!(
        names,
        vec![
            StorageName::new("local")?,
            StorageName::new("remote")?,
            StorageName::new("webdav")?,
        ]
    );

    store.delete("remote")?;
    assert_eq!(
        store.load("remote"),
        Err(StorageConfigurationError::NotFound)
    );
    assert_eq!(credential_store.len(), 1);
    Ok(())
}

#[test]
fn failed_updates_leave_the_previous_configuration_and_credential()
-> Result<(), Box<dyn std::error::Error>> {
    let directory = TestDirectory::new()?;
    let credential_store = MemoryCredentialStore::new();
    let store = StorageConfigurationStore::new(directory.path(), credential_store.clone())?;
    let initial = s3_configuration("initial")?;
    store.save("remote", initial.clone())?;
    let initial_loaded = store.load("remote")?;
    assert_eq!(credential_store.len(), 1);

    let replacement = s3_configuration("replacement")?;
    for operation in [
        StorageConfigurationOperation::Write,
        StorageConfigurationOperation::Flush,
        StorageConfigurationOperation::Rename,
        StorageConfigurationOperation::DirectorySync,
    ] {
        store.clear_injected_failures();
        store.inject_failure(operation, StorageConfigurationError::Io);
        assert_eq!(
            store.save("remote", replacement.clone()),
            Err(StorageConfigurationError::Io)
        );
        let loaded = store.load("remote")?;
        assert_eq!(loaded.backend(), initial_loaded.backend());
        assert_eq!(loaded.credentials(), initial_loaded.credentials());
        assert_eq!(credential_store.len(), 1);
    }

    store.clear_injected_failures();
    credential_store.inject_failure(CredentialStoreOperation::Delete, CredentialStoreError::Io);
    assert_eq!(
        store.save("remote", replacement.clone()),
        Err(StorageConfigurationError::CredentialStoreFailure)
    );
    let loaded = store.load("remote")?;
    assert_eq!(loaded.backend(), initial_loaded.backend());
    assert_eq!(loaded.credentials(), initial_loaded.credentials());
    assert_eq!(credential_store.len(), 1);

    credential_store.inject_failure(CredentialStoreOperation::Store, CredentialStoreError::Io);
    assert_eq!(
        store.save("new", replacement),
        Err(StorageConfigurationError::CredentialStoreFailure)
    );
    assert_eq!(store.load("new"), Err(StorageConfigurationError::NotFound));
    assert_eq!(credential_store.len(), 1);
    Ok(())
}

#[test]
fn delete_rolls_back_when_credential_removal_fails() -> Result<(), Box<dyn std::error::Error>> {
    let directory = TestDirectory::new()?;
    let credential_store = MemoryCredentialStore::new();
    let store = StorageConfigurationStore::new(directory.path(), credential_store.clone())?;
    let configuration = s3_configuration("delete")?;
    store.save("remote", configuration.clone())?;
    let loaded_before_delete = store.load("remote")?;

    credential_store.inject_failure(CredentialStoreOperation::Delete, CredentialStoreError::Io);
    assert_eq!(
        store.delete("remote"),
        Err(StorageConfigurationError::CredentialStoreFailure)
    );
    let loaded = store.load("remote")?;
    assert_eq!(loaded.backend(), loaded_before_delete.backend());
    assert_eq!(loaded.credentials(), loaded_before_delete.credentials());
    assert_eq!(credential_store.len(), 1);

    store.delete("remote")?;
    assert_eq!(
        store.load("remote"),
        Err(StorageConfigurationError::NotFound)
    );
    assert_eq!(credential_store.len(), 0);
    Ok(())
}

#[test]
fn a_missing_credential_reference_is_distinct_from_invalid_settings()
-> Result<(), Box<dyn std::error::Error>> {
    let directory = TestDirectory::new()?;
    let credential_store = MemoryCredentialStore::new();
    let store = StorageConfigurationStore::new(directory.path(), credential_store.clone())?;
    store.save("remote", s3_configuration("missing")?)?;
    let loaded = store.load("remote")?;
    let reference = loaded
        .credential_reference()
        .cloned()
        .ok_or("saved remote configuration did not have a credential reference")?;
    credential_store.delete(&reference)?;

    assert_eq!(
        store.load("remote"),
        Err(StorageConfigurationError::MissingCredentialReference)
    );
    Ok(())
}

#[test]
fn concurrent_handles_publish_complete_records() -> Result<(), Box<dyn std::error::Error>> {
    let directory = TestDirectory::new()?;
    let credential_store = MemoryCredentialStore::new();
    let first_store = StorageConfigurationStore::new(directory.path(), credential_store.clone())?;
    let second_store = StorageConfigurationStore::new(directory.path(), credential_store.clone())?;
    let barrier = Arc::new(Barrier::new(2));

    let first_barrier = barrier.clone();
    let first = thread::spawn(move || {
        first_barrier.wait();
        first_store.save("remote", s3_configuration("first")?)
    });
    let second_barrier = barrier;
    let second = thread::spawn(move || {
        second_barrier.wait();
        second_store.save("remote", s3_configuration("second")?)
    });
    first.join().map_err(|_| "first writer panicked")??;
    second.join().map_err(|_| "second writer panicked")??;

    let loaded = StorageConfigurationStore::new(directory.path(), credential_store.clone())?
        .load("remote")?;
    assert!(loaded.credentials().is_some());
    assert_eq!(credential_store.len(), 1);
    let record = fs::read(
        StorageConfigurationStore::new(directory.path(), credential_store)?
            .record_path("remote")?,
    )?;
    assert!(!record.is_empty());
    Ok(())
}

#[test]
fn names_are_single_components_and_symlinked_records_are_rejected()
-> Result<(), Box<dyn std::error::Error>> {
    let directory = TestDirectory::new()?;
    let store = StorageConfigurationStore::new(directory.path(), MemoryCredentialStore::new())?;
    for name in [
        "",
        ".",
        "..",
        "../escape",
        "/absolute",
        "nested/name",
        "nested\\name",
        "CON",
        "bad:name",
        "trailing.",
        "trailing ",
    ] {
        assert_eq!(
            store.record_path(name),
            Err(StorageConfigurationError::InvalidName),
            "name should be rejected: {name:?}"
        );
    }

    #[cfg(unix)]
    {
        std::os::unix::fs::symlink(
            directory.path().join("outside"),
            store.record_path("linked")?,
        )?;
        assert_eq!(
            store.load("linked"),
            Err(StorageConfigurationError::InvalidPath)
        );
        assert_eq!(
            store.enumerate(),
            Err(StorageConfigurationError::InvalidPath)
        );

        let target_directory = directory.path().join("target-directory");
        fs::create_dir(&target_directory)?;
        let configured_link = directory.path().join("configured-link");
        std::os::unix::fs::symlink(&target_directory, &configured_link)?;
        assert!(matches!(
            StorageConfigurationStore::new(&configured_link, MemoryCredentialStore::new()),
            Err(StorageConfigurationError::InvalidPath)
        ));
    }
    Ok(())
}

#[test]
fn future_and_unknown_records_fail_without_being_treated_as_valid()
-> Result<(), Box<dyn std::error::Error>> {
    let directory = TestDirectory::new()?;
    let store = StorageConfigurationStore::new(directory.path(), MemoryCredentialStore::new())?;

    for (name, record, expected) in [
        (
            "future-schema",
            wire_record(99, "local", 1, Some("/tmp/repository")),
            StorageConfigurationError::UnsupportedSchemaVersion { version: 99 },
        ),
        (
            "future-backend",
            wire_record(1, "future", 1, Some("/tmp/repository")),
            StorageConfigurationError::UnsupportedBackend {
                kind: String::from("future"),
            },
        ),
        (
            "future-backend-version",
            wire_record(1, "local", 99, Some("/tmp/repository")),
            StorageConfigurationError::UnsupportedBackendVersion {
                kind: String::from("local"),
                version: 99,
            },
        ),
    ] {
        fs::write(store.record_path(name)?, record?)?;
        assert_eq!(store.load(name), Err(expected));
    }
    Ok(())
}

fn s3_configuration(label: &str) -> Result<StorageConfiguration, StorageConfigurationError> {
    let settings = S3StorageSettings::new("us-east-1", "gib-test-bucket")?;
    let credentials = StorageCredentials::s3(format!("access-{label}"), format!("secret-{label}"))
        .map_err(|_| StorageConfigurationError::InvalidConfiguration)?;
    StorageConfiguration::new(StorageBackend::S3(settings), Some(credentials))
}

#[derive(Serialize)]
struct TestWire<'a> {
    schema_version: u16,
    backend: &'a str,
    backend_version: u16,
    credential_reference: Option<&'a str>,
    root_path: Option<&'a str>,
    region: Option<&'a str>,
    bucket: Option<&'a str>,
    endpoint: Option<&'a str>,
    force_path_style: Option<bool>,
    multipart_threshold: Option<u64>,
    multipart_part_size: Option<u64>,
    max_concurrency: Option<u64>,
    capability_cache_path: Option<&'a str>,
    collection_url: Option<&'a str>,
    allow_insecure_http: Option<bool>,
}

fn wire_record(
    schema_version: u16,
    backend: &str,
    backend_version: u16,
    root_path: Option<&str>,
) -> Result<Vec<u8>, rmp_serde::encode::Error> {
    rmp_serde::to_vec_named(&TestWire {
        schema_version,
        backend,
        backend_version,
        credential_reference: None,
        root_path,
        region: None,
        bucket: None,
        endpoint: None,
        force_path_style: None,
        multipart_threshold: None,
        multipart_part_size: None,
        max_concurrency: None,
        capability_cache_path: None,
        collection_url: None,
        allow_insecure_http: None,
    })
}
