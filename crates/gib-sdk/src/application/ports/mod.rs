mod configuration;
mod storage;
mod storage_configuration;

pub use configuration::{
    ConfigurationError, ConfigurationFileMetadata, ConfigurationFileSystem, ConfigurationResult,
    ConfigurationStorage,
};
pub use storage::{
    ByteRange, DEFAULT_OBJECT_LIST_PAGE_SIZE, ListCursor, MAX_OBJECT_LIST_PAGE_SIZE, ObjectCursor,
    ObjectKey, ObjectListPage, ObjectListRequest, ObjectMetadata, ObjectPrefix, ObjectRange,
    ObjectRead, ObjectReader, ObjectStorage, ObjectWriteOptions, RepositoryStorage,
    STORAGE_TRANSFER_BUFFER_SIZE, StorageCapabilities, StorageCapability, StorageError, StorageKey,
    StorageListPage, StorageListRequest, StorageMetadata, StoragePrefix, StorageRange,
    StorageReader, StorageResult, StorageVersion, StorageVersionToken, StorageWriteCondition,
    StorageWriteOptions, VersionToken, VersionedObject, VersionedStorageObject, WriteCondition,
};
pub use storage_configuration::{
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

pub(crate) use storage::{copy_stream, read_stream_to_vec};
