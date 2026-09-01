use gib::{
    LocalStorage, MemoryCredentialStore, ObjectKey, ObjectWriteOptions, RepositoryStorage,
    S3StorageCredentials, S3StorageSettings, StorageAddRequest, StorageConfiguration,
    StorageConfigurationError, StorageConfigurationListRequest, StorageConfigurationStore,
    StorageConnectivity, StorageError, StorageHealth, StorageManager, StorageRemoveRequest,
    WebDavStorageCredentials, WebDavStorageSettings,
};
use std::fs;
use std::io::Cursor;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

static NEXT_TEST_DIRECTORY: AtomicU64 = AtomicU64::new(1);

struct TestDirectory(PathBuf);

impl TestDirectory {
    fn new() -> Result<Self, Box<dyn std::error::Error>> {
        let path = std::env::temp_dir().join(format!(
            "gib-storage-management-{}-{}",
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

#[derive(Clone, Copy, Debug, Default)]
struct HealthyConnectivity;

impl StorageConnectivity for HealthyConnectivity {
    fn check(
        &self,
        _configuration: &StorageConfiguration,
    ) -> Result<StorageHealth, StorageConfigurationError> {
        Ok(StorageHealth::Healthy)
    }
}

#[derive(Clone, Copy, Debug, Default)]
struct FailingConnectivity;

impl StorageConnectivity for FailingConnectivity {
    fn check(
        &self,
        configuration: &StorageConfiguration,
    ) -> Result<StorageHealth, StorageConfigurationError> {
        Err(StorageConfigurationError::ConnectivityFailure {
            backend: configuration.backend().kind(),
            error: StorageError::Transient,
        })
    }
}

#[test]
fn add_list_and_remove_support_all_backend_request_types() -> Result<(), Box<dyn std::error::Error>>
{
    let directory = TestDirectory::new()?;
    let credentials = MemoryCredentialStore::new();
    let store =
        StorageConfigurationStore::new(directory.path().join("configs"), credentials.clone())?;
    let manager = StorageManager::with_connectivity(store.clone(), HealthyConnectivity);

    let local = StorageAddRequest::local("local", directory.path().join("local"))?;
    let s3 = StorageAddRequest::s3(
        "s3",
        S3StorageSettings::new("us-east-1", "gib-test-bucket")?
            .with_endpoint("https://s3.example.test")?,
        S3StorageCredentials::new("access-key", "secret-key")?,
    )?;
    let webdav = StorageAddRequest::webdav(
        "webdav",
        WebDavStorageSettings::new("https://dav.example.test/collection")?,
        WebDavStorageCredentials::new("username", "password")?,
    )?;

    assert_eq!(manager.add(local)?.health(), StorageHealth::Healthy);
    assert_eq!(
        manager.add(s3)?.metadata().backend().kind(),
        gib::StorageBackendKind::S3
    );
    assert_eq!(
        manager.add(webdav)?.metadata().backend().kind(),
        gib::StorageBackendKind::WebDav
    );
    assert_eq!(credentials.len(), 2);

    let listed = manager.list(StorageConfigurationListRequest::new())?;
    assert_eq!(listed.storages().len(), 3);
    assert_eq!(listed.storages()[0].name().as_str(), "local");
    assert_eq!(listed.storages()[1].name().as_str(), "s3");
    assert!(listed.storages()[1].credentials_configured());
    assert_eq!(listed.storages()[2].name().as_str(), "webdav");
    assert!(
        listed
            .storages()
            .iter()
            .all(|entry| entry.health() == StorageHealth::NotChecked)
    );
    let checked = manager.list(StorageConfigurationListRequest::new().check_health())?;
    assert!(
        checked
            .storages()
            .iter()
            .all(|entry| entry.health() == StorageHealth::Healthy)
    );

    let removed = manager.remove(StorageRemoveRequest::new("s3")?)?;
    assert_eq!(removed.backend(), gib::StorageBackendKind::S3);
    assert!(removed.credentials_removed());
    assert!(removed.repository_data_preserved());
    assert_eq!(credentials.len(), 1);
    assert_eq!(store.load("s3"), Err(StorageConfigurationError::NotFound));
    Ok(())
}

#[test]
fn duplicate_names_require_explicit_replacement() -> Result<(), Box<dyn std::error::Error>> {
    let directory = TestDirectory::new()?;
    let store = StorageConfigurationStore::new(directory.path(), MemoryCredentialStore::new())?;
    let manager = StorageManager::with_connectivity(store.clone(), HealthyConnectivity);
    let first_root = directory.path().join("first");
    let second_root = directory.path().join("second");

    manager.add(StorageAddRequest::local("same", &first_root)?)?;
    assert_eq!(
        manager.add(StorageAddRequest::local("same", &second_root)?),
        Err(StorageConfigurationError::AlreadyExists)
    );
    assert_eq!(
        store
            .load("same")?
            .backend()
            .as_local()
            .map(|settings| settings.root()),
        Some(first_root.as_path())
    );

    let replacement =
        manager.add(StorageAddRequest::local("same", &second_root)?.with_replacement(true))?;
    assert!(replacement.replaced_existing());
    assert_eq!(
        store
            .load("same")?
            .backend()
            .as_local()
            .map(|settings| settings.root()),
        Some(second_root.as_path())
    );
    Ok(())
}

#[test]
fn connectivity_failure_is_checked_before_persistence() -> Result<(), Box<dyn std::error::Error>> {
    let directory = TestDirectory::new()?;
    let credentials = MemoryCredentialStore::new();
    let store = StorageConfigurationStore::new(directory.path(), credentials.clone())?;
    let manager = StorageManager::with_connectivity(store.clone(), FailingConnectivity);
    let request = StorageAddRequest::s3(
        "remote",
        S3StorageSettings::new("us-east-1", "gib-test-bucket")?,
        S3StorageCredentials::new("access-key", "secret-key")?,
    )?;

    assert!(matches!(
        manager.add(request),
        Err(StorageConfigurationError::ConnectivityFailure {
            backend: gib::StorageBackendKind::S3,
            error: StorageError::Transient,
        })
    ));
    assert_eq!(
        store.load("remote"),
        Err(StorageConfigurationError::NotFound)
    );
    assert!(credentials.is_empty());
    Ok(())
}

#[test]
fn invalid_names_are_rejected_before_an_add_request_exists() {
    assert_eq!(
        StorageAddRequest::local("../unsafe", "/tmp/repository"),
        Err(StorageConfigurationError::InvalidName)
    );
}

#[test]
fn removal_preserves_local_repository_objects() -> Result<(), Box<dyn std::error::Error>> {
    let directory = TestDirectory::new()?;
    let repository_root = directory.path().join("repository");
    let storage = LocalStorage::new(&repository_root)?;
    let object_key = ObjectKey::new("objects/kept")?;
    let mut source = Cursor::new(b"must remain".to_vec());
    storage.write_stream(&object_key, &mut source, ObjectWriteOptions::if_absent())?;

    let store = StorageConfigurationStore::new(
        directory.path().join("configs"),
        MemoryCredentialStore::new(),
    )?;
    let manager = StorageManager::with_connectivity(store, HealthyConnectivity);
    manager.add(StorageAddRequest::local("local", &repository_root)?)?;
    let removed = manager.remove(StorageRemoveRequest::new("local")?)?;
    assert!(removed.repository_data_preserved());
    assert_eq!(storage.metadata(&object_key)?.size(), 11);
    assert!(repository_root.exists());
    Ok(())
}

#[test]
fn management_debug_values_do_not_include_remote_secrets() -> Result<(), Box<dyn std::error::Error>>
{
    let directory = TestDirectory::new()?;
    let store = StorageConfigurationStore::new(directory.path(), MemoryCredentialStore::new())?;
    let manager = StorageManager::with_connectivity(store, HealthyConnectivity);
    let request = StorageAddRequest::s3(
        "remote",
        S3StorageSettings::new("us-east-1", "gib-test-bucket")?,
        S3StorageCredentials::new("access-key-secret", "secret-key-secret")?,
    )?;
    let debug = format!("{request:?}");
    assert!(!debug.contains("access-key-secret"));
    assert!(!debug.contains("secret-key-secret"));
    let result = manager.add(request)?;
    let debug = format!("{result:?}");
    assert!(!debug.contains("access-key-secret"));
    assert!(!debug.contains("secret-key-secret"));
    Ok(())
}
