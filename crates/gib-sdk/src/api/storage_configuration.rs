//! Public storage-management contracts, operations, and persistence adapters.

use crate::application::storage_management;
use std::fmt;
use std::sync::Arc;

pub use crate::application::ports::{
    AddStorageRequest, CURRENT_STORAGE_BACKEND_VERSION, CURRENT_STORAGE_CONFIGURATION_VERSION,
    CredentialReference, CredentialStore, CredentialStoreError, CredentialStoreOperation,
    CredentialStoreResult, ListStorageRequest, LocalStorageSettings,
    MAX_STORAGE_CONFIGURATION_BYTES, MAX_STORAGE_CREDENTIAL_LENGTH, MAX_STORAGE_NAME_LENGTH,
    MAX_STORAGE_SETTING_LENGTH, RemoveStorageRequest, S3StorageCredentials, S3StorageSettings,
    STORAGE_CONFIGURATION_FILE_SUFFIX, StorageAddRequest, StorageAddResult, StorageBackend,
    StorageBackendKind, StorageConfiguration, StorageConfigurationError,
    StorageConfigurationListRequest, StorageConfigurationMetadata, StorageConfigurationOperation,
    StorageConfigurationRepository, StorageConfigurationResult, StorageConnectivity,
    StorageCredentialKind, StorageCredentials, StorageEntry, StorageHealth, StorageInfo,
    StorageListResult, StorageName, StorageProbe, StorageRemoveRequest, StorageRemoveResult,
    WebDavStorageCredentials, WebDavStorageSettings,
};
pub use crate::infrastructure::credential_store::PlatformCredentialStore;
pub use crate::infrastructure::storage_configuration::{
    CredentialStoreHandle, LocalStorageConfiguration, MemoryCredentialStore,
    STORAGE_CONFIGURATION_DIRECTORY_NAME, StorageConfigurationStore,
};
pub use crate::infrastructure::storage_connectivity::DefaultStorageConnectivity;

/// Coordinates validated storage-management operations.
///
/// The manager owns the persistence adapter and delegates connectivity checks
/// to an injectable read-only probe. Configurations are published only after
/// their backend has passed validation.
#[derive(Clone)]
pub struct StorageManager {
    store: StorageConfigurationStore,
    connectivity: Arc<dyn StorageConnectivity>,
}

impl StorageManager {
    /// Creates a manager backed by the current user's global storage
    /// configuration and operating-system credential store.
    pub fn global() -> StorageConfigurationResult<Self> {
        StorageConfigurationStore::global(PlatformCredentialStore::new()).map(Self::new)
    }

    /// Creates a manager using the SDK's Local, S3, and WebDAV connectivity
    /// checks.
    pub fn new(store: StorageConfigurationStore) -> Self {
        Self {
            store,
            connectivity: Arc::new(DefaultStorageConnectivity),
        }
    }

    /// Creates a manager with an application-provided connectivity checker.
    pub fn with_connectivity<C>(store: StorageConfigurationStore, connectivity: C) -> Self
    where
        C: StorageConnectivity + 'static,
    {
        Self {
            store,
            connectivity: Arc::new(connectivity),
        }
    }

    /// Returns the persistence adapter used by this manager.
    pub const fn store(&self) -> &StorageConfigurationStore {
        &self.store
    }

    /// Adds a new storage or explicitly replaces an existing named storage.
    pub fn add(&self, request: StorageAddRequest) -> StorageConfigurationResult<StorageAddResult> {
        storage_management::add_storage(&self.store, self.connectivity.as_ref(), request)
    }

    /// Lists safe metadata for configured storages.
    pub fn list(
        &self,
        request: StorageConfigurationListRequest,
    ) -> StorageConfigurationResult<StorageListResult> {
        storage_management::list_storages(&self.store, self.connectivity.as_ref(), request)
    }

    /// Removes a configuration and its credential reference without touching
    /// repository data in the configured backend.
    pub fn remove(
        &self,
        request: StorageRemoveRequest,
    ) -> StorageConfigurationResult<StorageRemoveResult> {
        storage_management::remove_storage(&self.store, request)
    }

    /// Adds a storage using the longer operation name.
    pub fn add_storage(
        &self,
        request: StorageAddRequest,
    ) -> StorageConfigurationResult<StorageAddResult> {
        self.add(request)
    }

    /// Lists storages using the longer operation name.
    pub fn list_storages(
        &self,
        request: StorageConfigurationListRequest,
    ) -> StorageConfigurationResult<StorageListResult> {
        self.list(request)
    }

    /// Removes a storage using the longer operation name.
    pub fn remove_storage(
        &self,
        request: StorageRemoveRequest,
    ) -> StorageConfigurationResult<StorageRemoveResult> {
        self.remove(request)
    }
}

impl fmt::Debug for StorageManager {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("StorageManager")
            .field("store", &self.store)
            .field("connectivity", &"<configured>")
            .finish()
    }
}

/// Adds or explicitly replaces a named storage through a manager.
pub fn add_storage(
    manager: &StorageManager,
    request: StorageAddRequest,
) -> StorageConfigurationResult<StorageAddResult> {
    manager.add(request)
}

/// Lists configured storages through a manager.
pub fn list_storages(
    manager: &StorageManager,
    request: StorageConfigurationListRequest,
) -> StorageConfigurationResult<StorageListResult> {
    manager.list(request)
}

/// Removes a named storage configuration through a manager.
pub fn remove_storage(
    manager: &StorageManager,
    request: StorageRemoveRequest,
) -> StorageConfigurationResult<StorageRemoveResult> {
    manager.remove(request)
}

/// Compatibility alias for [`StorageManager`].
pub type StorageConfigurationManager = StorageManager;
