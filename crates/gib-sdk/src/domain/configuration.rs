use std::num::{NonZeroU64, NonZeroUsize};
use std::path::{Path, PathBuf};
use std::time::Duration;

use parse_size::parse_size;

use super::chunk::{
    CONTENT_DEFINED_CHUNKING_ALGORITHM, CURRENT_CHUNKING_VERSION, ChunkingConfiguration,
    DEFAULT_MAX_CHUNK_SIZE_BYTES, DEFAULT_MIN_CHUNK_SIZE_BYTES, DEFAULT_TARGET_CHUNK_SIZE_BYTES,
};
use super::repository::{DomainError, RepositoryKey};
use super::snapshot::MAX_SNAPSHOT_MESSAGE_LENGTH;

/// The current project-local `gib.toml` schema version.
pub(crate) const CURRENT_CONFIGURATION_VERSION: u32 = 1;

/// The inclusive lower bound for Zstandard compression levels.
pub(crate) const MIN_COMPRESSION_LEVEL: i32 = 1;

/// The inclusive upper bound for Zstandard compression levels.
pub(crate) const MAX_COMPRESSION_LEVEL: i32 = 22;

/// The largest chunk size accepted by the bounded backup pipeline.
pub(crate) const MAX_CHUNK_SIZE_BYTES: u64 = 1024 * 1024 * 1024;

/// The largest configured backup concurrency.
pub(crate) const MAX_BACKUP_CONCURRENCY: usize = 1_024;

/// The largest live debounce or polling interval in milliseconds.
pub(crate) const MAX_LIVE_INTERVAL_MS: u64 = 365 * 24 * 60 * 60 * 1_000;

/// The largest storage name in UTF-8 bytes.
pub(crate) const MAX_STORAGE_NAME_LENGTH: usize = 128;

/// The largest configured path value in UTF-8 bytes.
pub(crate) const MAX_CONFIGURATION_PATH_LENGTH: usize = 32 * 1_024;

/// The largest ignore rule in UTF-8 bytes.
pub(crate) const MAX_IGNORE_RULE_LENGTH: usize = 4 * 1_024;

/// The largest number of ignore rules in one configuration.
pub(crate) const MAX_IGNORE_RULES: usize = 1_024;

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct ConfigurationInput {
    pub(crate) version: u32,
    pub(crate) repository: RepositoryConfigurationInput,
    pub(crate) backup: BackupConfigurationInput,
    pub(crate) live: LiveConfigurationInput,
    pub(crate) restore: RestoreConfigurationInput,
}

#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub(crate) struct RepositoryConfigurationInput {
    pub(crate) storage: Option<String>,
    pub(crate) key: Option<String>,
}

#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub(crate) struct BackupConfigurationInput {
    pub(crate) root_path: Option<String>,
    pub(crate) message: Option<String>,
    pub(crate) compress: Option<i32>,
    pub(crate) chunk_size: Option<String>,
    pub(crate) chunking: Option<ChunkingConfigurationInput>,
    pub(crate) concurrency: Option<usize>,
    pub(crate) ignore: Vec<String>,
}

#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub(crate) struct ChunkingConfigurationInput {
    pub(crate) version: Option<u16>,
    pub(crate) algorithm: Option<String>,
    pub(crate) min_size: Option<String>,
    pub(crate) target_size: Option<String>,
    pub(crate) max_size: Option<String>,
}

#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub(crate) struct LiveConfigurationInput {
    pub(crate) message: Option<String>,
    pub(crate) debounce_ms: Option<u64>,
    pub(crate) poll_ms: Option<u64>,
}

#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub(crate) struct RestoreConfigurationInput {
    pub(crate) target_path: Option<String>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct ConfigurationValidationError {
    field: String,
    reason: String,
}

impl ConfigurationValidationError {
    fn new(field: impl Into<String>, reason: impl Into<String>) -> Self {
        Self {
            field: field.into(),
            reason: reason.into(),
        }
    }

    pub(crate) fn field(&self) -> &str {
        &self.field
    }

    pub(crate) fn reason(&self) -> &str {
        &self.reason
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct ValidatedConfiguration {
    pub(crate) version: u32,
    pub(crate) repository: ValidatedRepositoryConfiguration,
    pub(crate) backup: ValidatedBackupConfiguration,
    pub(crate) live: ValidatedLiveConfiguration,
    pub(crate) restore: ValidatedRestoreConfiguration,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct ValidatedRepositoryConfiguration {
    pub(crate) storage: Option<StorageName>,
    pub(crate) key: Option<RepositoryKey>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct ValidatedBackupConfiguration {
    pub(crate) root_path: Option<PathBuf>,
    pub(crate) message: Option<String>,
    pub(crate) compress: Option<i32>,
    pub(crate) chunk_size: Option<ByteSize>,
    pub(crate) chunking: ChunkingConfiguration,
    pub(crate) concurrency: Option<NonZeroUsize>,
    pub(crate) ignore: Vec<String>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct ValidatedLiveConfiguration {
    pub(crate) message: Option<String>,
    pub(crate) debounce: Option<Duration>,
    pub(crate) poll: Option<Duration>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct ValidatedRestoreConfiguration {
    pub(crate) target_path: Option<PathBuf>,
}

#[derive(Clone, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub(crate) struct StorageName(String);

impl StorageName {
    fn new(value: String) -> Result<Self, ConfigurationValidationError> {
        if value.trim().is_empty() {
            return Err(ConfigurationValidationError::new(
                "repository.storage",
                "must contain at least one non-whitespace character",
            ));
        }
        if value.len() > MAX_STORAGE_NAME_LENGTH {
            return Err(ConfigurationValidationError::new(
                "repository.storage",
                format!("must contain at most {MAX_STORAGE_NAME_LENGTH} UTF-8 bytes"),
            ));
        }
        if !value
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'_' | b'-'))
        {
            return Err(ConfigurationValidationError::new(
                "repository.storage",
                "must contain only ASCII letters, digits, underscores, or hyphens",
            ));
        }
        Ok(Self(value))
    }

    pub(crate) fn as_str(&self) -> &str {
        &self.0
    }
}

#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub(crate) struct ByteSize(NonZeroU64);

impl ByteSize {
    fn new(bytes: u64, field: &str) -> Result<Self, ConfigurationValidationError> {
        if bytes > MAX_CHUNK_SIZE_BYTES {
            return Err(ConfigurationValidationError::new(
                field,
                format!("must be at most {MAX_CHUNK_SIZE_BYTES} bytes"),
            ));
        }
        let Some(bytes) = NonZeroU64::new(bytes) else {
            return Err(ConfigurationValidationError::new(
                field,
                "must be greater than zero",
            ));
        };
        Ok(Self(bytes))
    }

    pub(crate) const fn bytes(self) -> u64 {
        self.0.get()
    }
}

pub(crate) fn validate_configuration(
    input: ConfigurationInput,
    config_directory: &Path,
) -> Result<ValidatedConfiguration, ConfigurationValidationError> {
    if input.version != CURRENT_CONFIGURATION_VERSION {
        return Err(ConfigurationValidationError::new(
            "version",
            "is not supported",
        ));
    }

    let repository = validate_repository(input.repository)?;
    let backup = validate_backup(input.backup, config_directory)?;
    let live = validate_live(input.live)?;
    let restore = validate_restore(input.restore, config_directory)?;

    Ok(ValidatedConfiguration {
        version: input.version,
        repository,
        backup,
        live,
        restore,
    })
}

fn validate_repository(
    input: RepositoryConfigurationInput,
) -> Result<ValidatedRepositoryConfiguration, ConfigurationValidationError> {
    let storage = input.storage.map(StorageName::new).transpose()?;
    let key = input
        .key
        .map(|value| {
            RepositoryKey::new(value).map_err(|error| match error {
                DomainError::InvalidRepositoryKey { reason } => {
                    ConfigurationValidationError::new("repository.key", reason)
                }
                _ => ConfigurationValidationError::new(
                    "repository.key",
                    "must be a valid repository key",
                ),
            })
        })
        .transpose()?;
    Ok(ValidatedRepositoryConfiguration { storage, key })
}

fn validate_backup(
    input: BackupConfigurationInput,
    config_directory: &Path,
) -> Result<ValidatedBackupConfiguration, ConfigurationValidationError> {
    let root_path = input
        .root_path
        .map(|value| resolve_path(value, config_directory, "backup.root_path"))
        .transpose()?;
    let message = input
        .message
        .map(|value| validate_message(value, "backup.message"))
        .transpose()?;
    let compress = input
        .compress
        .map(|value| validate_compression(value, "backup.compress"))
        .transpose()?;
    let chunk_size = input
        .chunk_size
        .map(|value| validate_chunk_size(value, "backup.chunk_size"))
        .transpose()?;
    let chunking = validate_chunking(input.chunking)?;
    let concurrency = input
        .concurrency
        .map(|value| validate_concurrency(value, "backup.concurrency"))
        .transpose()?;
    let ignore = validate_ignore_rules(input.ignore)?;

    Ok(ValidatedBackupConfiguration {
        root_path,
        message,
        compress,
        chunk_size,
        chunking,
        concurrency,
        ignore,
    })
}

fn validate_chunking(
    input: Option<ChunkingConfigurationInput>,
) -> Result<ChunkingConfiguration, ConfigurationValidationError> {
    let Some(input) = input else {
        return Ok(ChunkingConfiguration::default());
    };
    let version = input.version.unwrap_or(CURRENT_CHUNKING_VERSION);
    let algorithm = input
        .algorithm
        .unwrap_or_else(|| CONTENT_DEFINED_CHUNKING_ALGORITHM.to_owned());
    let min_size = input
        .min_size
        .map(|value| validate_chunk_size(value, "backup.chunking.min_size"))
        .transpose()?
        .map(ByteSize::bytes)
        .unwrap_or(DEFAULT_MIN_CHUNK_SIZE_BYTES);
    let target_size = input
        .target_size
        .map(|value| validate_chunk_size(value, "backup.chunking.target_size"))
        .transpose()?
        .map(ByteSize::bytes)
        .unwrap_or(DEFAULT_TARGET_CHUNK_SIZE_BYTES);
    let max_size = input
        .max_size
        .map(|value| validate_chunk_size(value, "backup.chunking.max_size"))
        .transpose()?
        .map(ByteSize::bytes)
        .unwrap_or(DEFAULT_MAX_CHUNK_SIZE_BYTES);

    ChunkingConfiguration::from_parts(version, &algorithm, min_size, target_size, max_size)
        .map_err(|error| ConfigurationValidationError::new("backup.chunking", error.to_string()))
}

fn validate_live(
    input: LiveConfigurationInput,
) -> Result<ValidatedLiveConfiguration, ConfigurationValidationError> {
    let message = input
        .message
        .map(|value| validate_message(value, "live.message"))
        .transpose()?;
    let debounce = input
        .debounce_ms
        .map(|value| validate_interval(value, "live.debounce_ms"))
        .transpose()?;
    let poll = input
        .poll_ms
        .map(|value| validate_interval(value, "live.poll_ms"))
        .transpose()?;
    Ok(ValidatedLiveConfiguration {
        message,
        debounce,
        poll,
    })
}

fn validate_restore(
    input: RestoreConfigurationInput,
    config_directory: &Path,
) -> Result<ValidatedRestoreConfiguration, ConfigurationValidationError> {
    let target_path = input
        .target_path
        .map(|value| resolve_path(value, config_directory, "restore.target_path"))
        .transpose()?;
    Ok(ValidatedRestoreConfiguration { target_path })
}

fn resolve_path(
    value: String,
    config_directory: &Path,
    field: &str,
) -> Result<PathBuf, ConfigurationValidationError> {
    validate_path_value(&value, field)?;
    let path = Path::new(&value);
    Ok(if path.is_absolute() {
        path.to_path_buf()
    } else {
        config_directory.join(path)
    })
}

fn validate_path_value(value: &str, field: &str) -> Result<(), ConfigurationValidationError> {
    if value.is_empty() {
        return Err(ConfigurationValidationError::new(
            field,
            "must not be empty",
        ));
    }
    if value.len() > MAX_CONFIGURATION_PATH_LENGTH {
        return Err(ConfigurationValidationError::new(
            field,
            format!("must contain at most {MAX_CONFIGURATION_PATH_LENGTH} UTF-8 bytes"),
        ));
    }
    if value.contains('\0') {
        return Err(ConfigurationValidationError::new(
            field,
            "must not contain a NUL byte",
        ));
    }
    Ok(())
}

fn validate_message(value: String, field: &str) -> Result<String, ConfigurationValidationError> {
    if value.len() > MAX_SNAPSHOT_MESSAGE_LENGTH {
        return Err(ConfigurationValidationError::new(
            field,
            format!("must contain at most {MAX_SNAPSHOT_MESSAGE_LENGTH} UTF-8 bytes"),
        ));
    }
    if value.contains('\0') {
        return Err(ConfigurationValidationError::new(
            field,
            "must not contain a NUL byte",
        ));
    }
    Ok(value)
}

fn validate_compression(value: i32, field: &str) -> Result<i32, ConfigurationValidationError> {
    if !(MIN_COMPRESSION_LEVEL..=MAX_COMPRESSION_LEVEL).contains(&value) {
        return Err(ConfigurationValidationError::new(
            field,
            "must be between 1 and 22",
        ));
    }
    Ok(value)
}

fn validate_chunk_size(
    value: String,
    field: &str,
) -> Result<ByteSize, ConfigurationValidationError> {
    let bytes = parse_size(&value)
        .map_err(|_| ConfigurationValidationError::new(field, "must be a valid byte size"))?;
    ByteSize::new(bytes, field)
}

fn validate_concurrency(
    value: usize,
    field: &str,
) -> Result<NonZeroUsize, ConfigurationValidationError> {
    if value > MAX_BACKUP_CONCURRENCY {
        return Err(ConfigurationValidationError::new(
            field,
            format!("must be at most {MAX_BACKUP_CONCURRENCY}"),
        ));
    }
    NonZeroUsize::new(value)
        .ok_or_else(|| ConfigurationValidationError::new(field, "must be greater than zero"))
}

fn validate_ignore_rules(values: Vec<String>) -> Result<Vec<String>, ConfigurationValidationError> {
    if values.len() > MAX_IGNORE_RULES {
        return Err(ConfigurationValidationError::new(
            "backup.ignore",
            format!("must contain at most {MAX_IGNORE_RULES} rules"),
        ));
    }
    values
        .into_iter()
        .enumerate()
        .map(|(index, value)| {
            let field = format!("backup.ignore[{index}]");
            if value.trim().is_empty() {
                return Err(ConfigurationValidationError::new(
                    field,
                    "must contain at least one non-whitespace character",
                ));
            }
            if value.len() > MAX_IGNORE_RULE_LENGTH {
                return Err(ConfigurationValidationError::new(
                    field,
                    format!("must contain at most {MAX_IGNORE_RULE_LENGTH} UTF-8 bytes"),
                ));
            }
            if value.contains('\0') {
                return Err(ConfigurationValidationError::new(
                    field,
                    "must not contain a NUL byte",
                ));
            }
            Ok(value)
        })
        .collect()
}

fn validate_interval(value: u64, field: &str) -> Result<Duration, ConfigurationValidationError> {
    if value == 0 {
        return Err(ConfigurationValidationError::new(
            field,
            "must be greater than zero",
        ));
    }
    if value > MAX_LIVE_INTERVAL_MS {
        return Err(ConfigurationValidationError::new(
            field,
            format!("must be at most {MAX_LIVE_INTERVAL_MS} milliseconds"),
        ));
    }
    Ok(Duration::from_millis(value))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn valid_input() -> ConfigurationInput {
        ConfigurationInput {
            version: CURRENT_CONFIGURATION_VERSION,
            repository: RepositoryConfigurationInput::default(),
            backup: BackupConfigurationInput::default(),
            live: LiveConfigurationInput::default(),
            restore: RestoreConfigurationInput::default(),
        }
    }

    fn invalid_field(input: ConfigurationInput, field: &str) {
        let error = validate_configuration(input, Path::new("/project"))
            .expect_err("invalid configuration should fail");
        assert_eq!(error.field(), field);
    }

    #[test]
    fn accepts_the_documented_inclusive_resource_boundaries() {
        let mut input = valid_input();
        input.backup.compress = Some(MIN_COMPRESSION_LEVEL);
        assert!(validate_configuration(input, Path::new("/project")).is_ok());

        let mut input = valid_input();
        input.backup.compress = Some(MAX_COMPRESSION_LEVEL);
        input.backup.chunk_size = Some(String::from("1073741824 B"));
        input.backup.concurrency = Some(MAX_BACKUP_CONCURRENCY);
        input.live.debounce_ms = Some(MAX_LIVE_INTERVAL_MS);
        input.live.poll_ms = Some(MAX_LIVE_INTERVAL_MS);
        assert!(validate_configuration(input, Path::new("/project")).is_ok());
    }

    #[test]
    fn rejects_invalid_repository_values() {
        let mut input = valid_input();
        input.repository.storage = Some(String::from("  "));
        invalid_field(input, "repository.storage");

        let mut input = valid_input();
        input.repository.storage = Some(String::from("storage/name"));
        invalid_field(input, "repository.storage");

        let mut input = valid_input();
        input.repository.key = Some(String::from("not a key"));
        invalid_field(input, "repository.key");
    }

    #[test]
    fn rejects_invalid_backup_values() {
        for value in [0, MIN_COMPRESSION_LEVEL - 1, MAX_COMPRESSION_LEVEL + 1] {
            let mut input = valid_input();
            input.backup.compress = Some(value);
            invalid_field(input, "backup.compress");
        }

        for value in [String::from("0 B"), String::from("not-a-size")] {
            let mut input = valid_input();
            input.backup.chunk_size = Some(value);
            invalid_field(input, "backup.chunk_size");
        }

        let mut input = valid_input();
        input.backup.chunk_size = Some(String::from("1073741825 B"));
        invalid_field(input, "backup.chunk_size");

        for value in [0, MAX_BACKUP_CONCURRENCY + 1] {
            let mut input = valid_input();
            input.backup.concurrency = Some(value);
            invalid_field(input, "backup.concurrency");
        }

        let mut input = valid_input();
        input.backup.root_path = Some(String::new());
        invalid_field(input, "backup.root_path");

        let mut input = valid_input();
        input.backup.message = Some("x".repeat(MAX_SNAPSHOT_MESSAGE_LENGTH + 1));
        invalid_field(input, "backup.message");

        let mut input = valid_input();
        input.backup.ignore = vec![String::new()];
        invalid_field(input, "backup.ignore[0]");

        let mut input = valid_input();
        input.backup.ignore = vec![String::from("x").repeat(MAX_IGNORE_RULE_LENGTH + 1)];
        invalid_field(input, "backup.ignore[0]");

        let mut input = valid_input();
        input.backup.ignore = vec![String::from("rule"); MAX_IGNORE_RULES + 1];
        invalid_field(input, "backup.ignore");
    }

    #[test]
    fn rejects_invalid_live_and_restore_values() {
        for field in ["live.debounce_ms", "live.poll_ms"] {
            let mut input = valid_input();
            if field == "live.debounce_ms" {
                input.live.debounce_ms = Some(0);
            } else {
                input.live.poll_ms = Some(0);
            }
            invalid_field(input, field);

            let mut input = valid_input();
            if field == "live.debounce_ms" {
                input.live.debounce_ms = Some(MAX_LIVE_INTERVAL_MS + 1);
            } else {
                input.live.poll_ms = Some(MAX_LIVE_INTERVAL_MS + 1);
            }
            invalid_field(input, field);
        }

        let mut input = valid_input();
        input.live.message = Some("x".repeat(MAX_SNAPSHOT_MESSAGE_LENGTH + 1));
        invalid_field(input, "live.message");

        let mut input = valid_input();
        input.restore.target_path = Some(String::new());
        invalid_field(input, "restore.target_path");
    }

    #[test]
    fn resolves_relative_paths_without_consulting_process_state() {
        let mut input = valid_input();
        input.backup.root_path = Some(String::from("source"));
        input.restore.target_path = Some(String::from("restore"));
        let configuration =
            validate_configuration(input, Path::new("/config/project")).expect("valid paths");
        assert_eq!(
            configuration.backup.root_path,
            Some(PathBuf::from("/config/project/source"))
        );
        assert_eq!(
            configuration.restore.target_path,
            Some(PathBuf::from("/config/project/restore"))
        );

        let mut input = valid_input();
        input.backup.root_path = Some(String::from("/absolute/source"));
        let configuration =
            validate_configuration(input, Path::new("/config/project")).expect("valid path");
        assert_eq!(
            configuration.backup.root_path,
            Some(PathBuf::from("/absolute/source"))
        );
    }

    #[test]
    fn rejects_invalid_path_bytes_and_versions() {
        let mut input = valid_input();
        input.version = CURRENT_CONFIGURATION_VERSION + 1;
        invalid_field(input, "version");

        let mut input = valid_input();
        input.backup.root_path = Some(String::from("bad\0path"));
        invalid_field(input, "backup.root_path");

        let mut input = valid_input();
        input.restore.target_path = Some(String::from("bad\0path"));
        invalid_field(input, "restore.target_path");
    }
}
