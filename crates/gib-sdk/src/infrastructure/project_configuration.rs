use std::fs::{self, File};
use std::io::Read;
use std::path::{Path, PathBuf};

use crate::domain::{
    BackupConfigurationInput, ConfigurationInput, ConfigurationValidationError,
    LiveConfigurationInput, RepositoryConfigurationInput, RestoreConfigurationInput,
    ValidatedConfiguration, validate_configuration,
};
use crate::format::{
    ConfigurationDocumentError, PersistedConfiguration, parse_configuration_document,
};

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) enum ProjectConfigurationLoadError {
    Read {
        path: PathBuf,
    },
    InvalidPath {
        path: PathBuf,
    },
    InputTooLarge {
        path: PathBuf,
    },
    Document {
        path: PathBuf,
        error: ConfigurationDocumentError,
    },
    Validation {
        path: PathBuf,
        error: ConfigurationValidationError,
    },
}

pub(crate) enum ProjectConfigurationParseError {
    Document(ConfigurationDocumentError),
    Validation(ConfigurationValidationError),
}

pub(crate) fn parse_project_configuration(
    contents: &str,
    config_directory: &Path,
) -> Result<ValidatedConfiguration, ProjectConfigurationParseError> {
    let document =
        parse_configuration_document(contents).map_err(ProjectConfigurationParseError::Document)?;
    let input = input_from_document(document);
    validate_configuration(input, config_directory)
        .map_err(ProjectConfigurationParseError::Validation)
}

pub(crate) fn load_project_configuration(
    path: &Path,
) -> Result<ValidatedConfiguration, ProjectConfigurationLoadError> {
    let resolved_path =
        fs::canonicalize(path).map_err(|_| ProjectConfigurationLoadError::Read {
            path: path.to_path_buf(),
        })?;
    let metadata =
        fs::symlink_metadata(&resolved_path).map_err(|_| ProjectConfigurationLoadError::Read {
            path: resolved_path.clone(),
        })?;
    if !metadata.is_file() {
        return Err(ProjectConfigurationLoadError::InvalidPath {
            path: resolved_path,
        });
    }
    if metadata.len() > crate::format::MAX_CONFIGURATION_BYTES as u64 {
        return Err(ProjectConfigurationLoadError::InputTooLarge {
            path: resolved_path,
        });
    }

    let file = File::open(&resolved_path).map_err(|_| ProjectConfigurationLoadError::Read {
        path: resolved_path.clone(),
    })?;
    let capacity = usize::try_from(
        metadata
            .len()
            .min((crate::format::MAX_CONFIGURATION_BYTES + 1) as u64),
    )
    .map_err(|_| ProjectConfigurationLoadError::InputTooLarge {
        path: resolved_path.clone(),
    })?;
    let mut bytes = Vec::with_capacity(capacity);
    file.take((crate::format::MAX_CONFIGURATION_BYTES + 1) as u64)
        .read_to_end(&mut bytes)
        .map_err(|_| ProjectConfigurationLoadError::Read {
            path: resolved_path.clone(),
        })?;
    if bytes.len() > crate::format::MAX_CONFIGURATION_BYTES {
        return Err(ProjectConfigurationLoadError::InputTooLarge {
            path: resolved_path,
        });
    }
    let contents =
        String::from_utf8(bytes).map_err(|_| ProjectConfigurationLoadError::Document {
            path: resolved_path.clone(),
            error: ConfigurationDocumentError::invalid_encoding(),
        })?;
    let config_directory =
        resolved_path
            .parent()
            .ok_or_else(|| ProjectConfigurationLoadError::InvalidPath {
                path: resolved_path.clone(),
            })?;
    parse_project_configuration(&contents, config_directory).map_err(|error| match error {
        ProjectConfigurationParseError::Document(error) => {
            ProjectConfigurationLoadError::Document {
                path: resolved_path.clone(),
                error,
            }
        }
        ProjectConfigurationParseError::Validation(error) => {
            ProjectConfigurationLoadError::Validation {
                path: resolved_path,
                error,
            }
        }
    })
}

fn input_from_document(document: PersistedConfiguration) -> ConfigurationInput {
    ConfigurationInput {
        version: document.version,
        repository: RepositoryConfigurationInput {
            storage: document.repository.storage,
            key: document.repository.key,
        },
        backup: BackupConfigurationInput {
            root_path: document.backup.root_path,
            message: document.backup.message,
            compress: document.backup.compress,
            chunk_size: document.backup.chunk_size,
            concurrency: document.backup.concurrency,
            ignore: document.backup.ignore,
        },
        live: LiveConfigurationInput {
            message: document.live.message,
            debounce_ms: document.live.debounce_ms,
            poll_ms: document.live.poll_ms,
        },
        restore: RestoreConfigurationInput {
            target_path: document.restore.target_path,
        },
    }
}
