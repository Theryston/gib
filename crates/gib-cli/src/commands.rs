pub mod config;
pub mod log;
pub mod resolve;
pub mod storage;
pub mod whoami;

use gib::{
    ConfigurationResolutionRequest, LocalStorage, ProjectConfigurationError, Repository,
    RepositoryOpenRequest, ResolvedConfiguration, SdkError, StorageConfigurationError,
    StorageError,
};
use std::fmt;
use std::path::Path;

#[derive(Debug)]
pub enum CommandError {
    Storage(StorageError),
    StorageConfiguration(StorageConfigurationError),
    Sdk(SdkError),
    Configuration(ProjectConfigurationError),
}

impl CommandError {
    pub fn code(&self) -> &'static str {
        match self {
            Self::Storage(_) => "storage_failure",
            Self::StorageConfiguration(error) => error.code(),
            Self::Sdk(error) => error.code().as_str(),
            Self::Configuration(error) => error.code(),
        }
    }

    pub fn field(&self) -> Option<&'static str> {
        match self {
            Self::Sdk(gib::SdkError::InvalidConfiguration { field, .. })
            | Self::Sdk(gib::SdkError::InvalidRequest { field, .. }) => Some(field),
            _ => None,
        }
    }
}

impl fmt::Display for CommandError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Storage(error) => error.fmt(formatter),
            Self::StorageConfiguration(error) => error.fmt(formatter),
            Self::Sdk(error) => error.fmt(formatter),
            Self::Configuration(error) => error.fmt(formatter),
        }
    }
}

impl std::error::Error for CommandError {}

pub fn resolve_configuration(
    request: ConfigurationResolutionRequest,
) -> Result<ResolvedConfiguration, CommandError> {
    gib::resolve_configuration(request).map_err(CommandError::Configuration)
}

pub fn open_repository(path: &Path) -> Result<Repository, CommandError> {
    let storage = LocalStorage::new(path).map_err(CommandError::Storage)?;
    Repository::open_with_request(storage, RepositoryOpenRequest::new()).map_err(CommandError::Sdk)
}
