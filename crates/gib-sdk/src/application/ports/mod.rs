mod configuration;
mod storage;

pub use configuration::{ConfigurationError, ConfigurationResult, ConfigurationStorage};
pub use storage::{
    RepositoryStorage, StorageError, StorageResult, StorageVersion, StorageVersionToken,
    VersionToken, VersionedObject, VersionedStorageObject,
};
