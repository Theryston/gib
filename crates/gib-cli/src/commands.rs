pub mod config;
pub mod log;
pub mod resolve;
pub mod whoami;

use gib::{LocalStorage, Repository, RepositoryOpenRequest, SdkError, StorageError};
use std::fmt;
use std::path::Path;

#[derive(Debug)]
pub enum CommandError {
    Storage(StorageError),
    Sdk(SdkError),
}

impl CommandError {
    pub fn code(&self) -> &'static str {
        match self {
            Self::Storage(_) => "storage_failure",
            Self::Sdk(error) => error.code().as_str(),
        }
    }
}

impl fmt::Display for CommandError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Storage(error) => error.fmt(formatter),
            Self::Sdk(error) => error.fmt(formatter),
        }
    }
}

impl std::error::Error for CommandError {}

pub fn open_repository(path: &Path) -> Result<Repository, CommandError> {
    let storage = LocalStorage::new(path).map_err(CommandError::Storage)?;
    Repository::open_with_request(storage, RepositoryOpenRequest::new()).map_err(CommandError::Sdk)
}
