//! Public storage-configuration persistence contracts and adapters.

pub use crate::application::ports::{
    CURRENT_STORAGE_BACKEND_VERSION, CURRENT_STORAGE_CONFIGURATION_VERSION, CredentialReference,
    CredentialStore, CredentialStoreError, CredentialStoreOperation, CredentialStoreResult,
    LocalStorageSettings, MAX_STORAGE_CONFIGURATION_BYTES, MAX_STORAGE_CREDENTIAL_LENGTH,
    MAX_STORAGE_NAME_LENGTH, MAX_STORAGE_SETTING_LENGTH, S3StorageCredentials, S3StorageSettings,
    STORAGE_CONFIGURATION_FILE_SUFFIX, StorageBackend, StorageBackendKind, StorageConfiguration,
    StorageConfigurationError, StorageConfigurationOperation, StorageConfigurationResult,
    StorageCredentialKind, StorageCredentials, StorageName, WebDavStorageCredentials,
    WebDavStorageSettings,
};
pub use crate::infrastructure::storage_configuration::{
    CredentialStoreHandle, LocalStorageConfiguration, MemoryCredentialStore,
    StorageConfigurationStore,
};
