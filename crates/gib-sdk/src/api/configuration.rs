use std::fmt;
use std::path::{Path, PathBuf};
use std::time::Duration;

use crate::domain::ValidatedConfiguration;
use crate::format::{
    ConfigurationDocumentError, ConfigurationDocumentErrorKind,
    MAX_CONFIGURATION_BYTES as MAX_CONFIGURATION_DOCUMENT_BYTES,
};
use crate::infrastructure::project_configuration::{
    ProjectConfigurationLoadError, parse_project_configuration,
};

/// The current `gib.toml` schema version supported by this SDK.
pub const CURRENT_CONFIGURATION_VERSION: u32 = crate::domain::CURRENT_CONFIGURATION_VERSION;

/// Compatibility name for [`CURRENT_CONFIGURATION_VERSION`].
pub const CURRENT_GIB_CONFIGURATION_VERSION: u32 = CURRENT_CONFIGURATION_VERSION;

/// The conventional project-local configuration filename.
pub const GIB_CONFIGURATION_FILE_NAME: &str = "gib.toml";

/// Compatibility name for [`GIB_CONFIGURATION_FILE_NAME`].
pub const CONFIGURATION_FILE_NAME: &str = GIB_CONFIGURATION_FILE_NAME;

/// Compatibility name for [`GIB_CONFIGURATION_FILE_NAME`].
pub const PROJECT_CONFIGURATION_FILE_NAME: &str = GIB_CONFIGURATION_FILE_NAME;

/// Compatibility name for [`GIB_CONFIGURATION_FILE_NAME`].
pub const LOCAL_CONFIGURATION_FILE_NAME: &str = GIB_CONFIGURATION_FILE_NAME;

/// The largest project-local TOML document accepted by the parser.
pub const MAX_CONFIGURATION_BYTES: usize = MAX_CONFIGURATION_DOCUMENT_BYTES;

/// The inclusive lower bound for Zstandard compression levels.
pub const MIN_COMPRESSION_LEVEL: i32 = crate::domain::MIN_COMPRESSION_LEVEL;

/// The inclusive upper bound for Zstandard compression levels.
pub const MAX_COMPRESSION_LEVEL: i32 = crate::domain::MAX_COMPRESSION_LEVEL;

/// The largest accepted backup chunk size in bytes.
pub const MAX_CHUNK_SIZE_BYTES: u64 = crate::domain::MAX_CHUNK_SIZE_BYTES;

/// The largest accepted backup concurrency.
pub const MAX_BACKUP_CONCURRENCY: usize = crate::domain::MAX_BACKUP_CONCURRENCY;

/// The largest accepted Live interval in milliseconds.
pub const MAX_LIVE_INTERVAL_MS: u64 = crate::domain::MAX_LIVE_INTERVAL_MS;

/// Categories of failures returned while reading or validating `gib.toml`.
#[non_exhaustive]
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ProjectConfigurationErrorKind {
    /// The file could not be read or its path is not a regular file.
    Io,
    /// The document is larger than the bounded parser input.
    InputTooLarge,
    /// TOML syntax or document structure is invalid.
    Parse,
    /// A required field is absent.
    MissingField,
    /// A field is not part of the versioned schema.
    UnknownField,
    /// A field has a TOML type different from its schema type.
    InvalidType,
    /// A field has a schema type but fails domain validation.
    InvalidValue,
    /// The document declares a version this SDK does not support.
    UnsupportedVersion,
    /// The supplied configuration file path is not usable.
    InvalidPath,
}

/// A typed project-configuration failure with optional file and field context.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ProjectConfigurationError {
    kind: ProjectConfigurationErrorKind,
    file: Option<PathBuf>,
    field: Option<String>,
    reason: String,
    version: Option<u32>,
}

impl ProjectConfigurationError {
    fn new(
        kind: ProjectConfigurationErrorKind,
        file: Option<&Path>,
        field: Option<String>,
        reason: impl Into<String>,
    ) -> Self {
        Self {
            kind,
            file: file.map(Path::to_path_buf),
            field,
            reason: reason.into(),
            version: None,
        }
    }

    fn unsupported_version(
        file: Option<&Path>,
        field: Option<String>,
        version: Option<u32>,
        reason: impl Into<String>,
    ) -> Self {
        Self {
            kind: ProjectConfigurationErrorKind::UnsupportedVersion,
            file: file.map(Path::to_path_buf),
            field,
            reason: reason.into(),
            version,
        }
    }

    /// Returns the stable error category.
    pub const fn kind(&self) -> ProjectConfigurationErrorKind {
        self.kind
    }

    /// Returns the configuration file involved in the failure, when one was
    /// supplied to a file-loading API.
    pub fn file(&self) -> Option<&Path> {
        self.file.as_deref()
    }

    /// Returns the schema field involved in the failure, when identifiable.
    pub fn field(&self) -> Option<&str> {
        self.field.as_deref()
    }

    /// Returns the schema field path as an alias for [`Self::field`].
    pub fn field_path(&self) -> Option<&str> {
        self.field()
    }

    /// Returns the stable human-readable validation reason.
    pub fn reason(&self) -> &str {
        &self.reason
    }

    /// Returns the unsupported version found in the document, when relevant.
    pub const fn version(&self) -> Option<u32> {
        self.version
    }

    /// Returns the configured file path as an alias for [`Self::file`].
    pub fn path(&self) -> Option<&Path> {
        self.file()
    }

    /// Returns the configuration file path as an alias for [`Self::file`].
    pub fn file_path(&self) -> Option<&Path> {
        self.file()
    }
}

impl fmt::Display for ProjectConfigurationError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        let file = self
            .file
            .as_ref()
            .map(|path| format!(" in configuration file '{}'", path.display()))
            .unwrap_or_default();
        match self.kind {
            ProjectConfigurationErrorKind::Io => {
                write!(
                    formatter,
                    "could not read configuration{file}: {}",
                    self.reason
                )
            }
            ProjectConfigurationErrorKind::InputTooLarge => {
                write!(
                    formatter,
                    "configuration is too large{file}: {}",
                    self.reason
                )
            }
            ProjectConfigurationErrorKind::Parse => {
                write!(
                    formatter,
                    "could not parse configuration{file}: {}",
                    self.reason
                )
            }
            ProjectConfigurationErrorKind::MissingField => {
                write!(formatter, "missing configuration field")?;
                if let Some(field) = self.field() {
                    write!(formatter, " '{field}'")?;
                }
                write!(formatter, "{file}: {}", self.reason)
            }
            ProjectConfigurationErrorKind::UnknownField => {
                write!(formatter, "unknown configuration field")?;
                if let Some(field) = self.field() {
                    write!(formatter, " '{field}'")?;
                }
                write!(formatter, "{file}: {}", self.reason)
            }
            ProjectConfigurationErrorKind::InvalidType => {
                write!(formatter, "invalid configuration type")?;
                if let Some(field) = self.field() {
                    write!(formatter, " for '{field}'")?;
                }
                write!(formatter, "{file}: {}", self.reason)
            }
            ProjectConfigurationErrorKind::InvalidValue => {
                write!(formatter, "invalid configuration value")?;
                if let Some(field) = self.field() {
                    write!(formatter, " for '{field}'")?;
                }
                write!(formatter, "{file}: {}", self.reason)
            }
            ProjectConfigurationErrorKind::UnsupportedVersion => {
                write!(formatter, "unsupported configuration version")?;
                if let Some(version) = self.version {
                    write!(formatter, " {version}")?;
                }
                write!(formatter, "{file}")?;
                if let Some(field) = self.field() {
                    write!(formatter, " (field '{field}')")?;
                }
                write!(formatter, ": {}", self.reason)
            }
            ProjectConfigurationErrorKind::InvalidPath => {
                write!(
                    formatter,
                    "invalid configuration path{file}: {}",
                    self.reason
                )
            }
        }
    }
}

impl std::error::Error for ProjectConfigurationError {}

/// A validated, versioned project-local configuration.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ProjectConfiguration {
    version: u32,
    repository: RepositoryConfiguration,
    backup: BackupConfiguration,
    live: LiveConfiguration,
    restore: RestoreConfiguration,
}

impl ProjectConfiguration {
    /// Parses a TOML document using the supplied configuration directory for
    /// relative backup and restore paths.
    pub fn parse(
        contents: impl AsRef<str>,
        config_directory: impl AsRef<Path>,
    ) -> Result<Self, ProjectConfigurationError> {
        parse_project_configuration(contents.as_ref(), config_directory.as_ref())
            .map(Self::from_validated)
            .map_err(|error| map_parse_error(error, None))
    }

    /// Alias for [`Self::parse`] using TOML terminology.
    pub fn from_toml(
        contents: impl AsRef<str>,
        config_directory: impl AsRef<Path>,
    ) -> Result<Self, ProjectConfigurationError> {
        Self::parse(contents, config_directory)
    }

    /// Alias for [`Self::parse`] using string parsing terminology.
    pub fn from_str(
        contents: impl AsRef<str>,
        config_directory: impl AsRef<Path>,
    ) -> Result<Self, ProjectConfigurationError> {
        Self::parse(contents, config_directory)
    }

    /// Reads, parses, validates, and resolves a project-local configuration
    /// file without writing to the filesystem.
    pub fn from_file(path: impl AsRef<Path>) -> Result<Self, ProjectConfigurationError> {
        crate::infrastructure::project_configuration::load_project_configuration(path.as_ref())
            .map(Self::from_validated)
            .map_err(map_load_error)
    }

    /// Alias for [`Self::from_file`].
    pub fn load(path: impl AsRef<Path>) -> Result<Self, ProjectConfigurationError> {
        Self::from_file(path)
    }

    /// Alias for [`Self::from_file`].
    pub fn load_from_file(path: impl AsRef<Path>) -> Result<Self, ProjectConfigurationError> {
        Self::from_file(path)
    }

    /// Returns the schema version accepted from the file.
    pub const fn version(&self) -> u32 {
        self.version
    }

    /// Returns repository selection defaults.
    pub const fn repository(&self) -> &RepositoryConfiguration {
        &self.repository
    }

    /// Returns backup defaults.
    pub const fn backup(&self) -> &BackupConfiguration {
        &self.backup
    }

    /// Returns Live synchronization defaults.
    pub const fn live(&self) -> &LiveConfiguration {
        &self.live
    }

    /// Returns restore defaults.
    pub const fn restore(&self) -> &RestoreConfiguration {
        &self.restore
    }

    fn from_validated(configuration: ValidatedConfiguration) -> Self {
        Self {
            version: configuration.version,
            repository: RepositoryConfiguration {
                storage: configuration
                    .repository
                    .storage
                    .map(|storage| storage.as_str().to_owned()),
                key: configuration
                    .repository
                    .key
                    .map(|key| key.as_str().to_owned()),
            },
            backup: BackupConfiguration {
                root_path: configuration.backup.root_path,
                message: configuration.backup.message,
                compress: configuration.backup.compress,
                chunk_size: configuration
                    .backup
                    .chunk_size
                    .map(|chunk_size| ByteSize(chunk_size.bytes())),
                concurrency: configuration
                    .backup
                    .concurrency
                    .map(|concurrency| concurrency.get()),
                ignore: configuration.backup.ignore,
            },
            live: LiveConfiguration {
                message: configuration.live.message,
                debounce: configuration.live.debounce,
                poll: configuration.live.poll,
            },
            restore: RestoreConfiguration {
                target_path: configuration.restore.target_path,
            },
        }
    }
}

impl Default for ProjectConfiguration {
    fn default() -> Self {
        Self {
            version: CURRENT_CONFIGURATION_VERSION,
            repository: RepositoryConfiguration::default(),
            backup: BackupConfiguration::default(),
            live: LiveConfiguration::default(),
            restore: RestoreConfiguration::default(),
        }
    }
}

/// Repository selection defaults from `gib.toml`.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct RepositoryConfiguration {
    storage: Option<String>,
    key: Option<String>,
}

impl RepositoryConfiguration {
    /// Returns the configured storage name, if present.
    pub fn storage(&self) -> Option<&str> {
        self.storage.as_deref()
    }

    /// Returns the configured repository namespace key, if present.
    pub fn key(&self) -> Option<&str> {
        self.key.as_deref()
    }
}

/// Backup defaults from `gib.toml`.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct BackupConfiguration {
    root_path: Option<PathBuf>,
    message: Option<String>,
    compress: Option<i32>,
    chunk_size: Option<ByteSize>,
    concurrency: Option<usize>,
    ignore: Vec<String>,
}

impl BackupConfiguration {
    /// Returns the root path resolved against the configuration directory.
    pub fn root_path(&self) -> Option<&Path> {
        self.root_path.as_deref()
    }

    /// Returns the default backup message, if present.
    pub fn message(&self) -> Option<&str> {
        self.message.as_deref()
    }

    /// Returns the configured Zstandard compression level, if present.
    pub const fn compress(&self) -> Option<i32> {
        self.compress
    }

    /// Alias for [`Self::compress`] using the descriptive field name.
    pub const fn compression(&self) -> Option<i32> {
        self.compress()
    }

    /// Alias for [`Self::compress`] using compression-level terminology.
    pub const fn compression_level(&self) -> Option<i32> {
        self.compress()
    }

    /// Returns the configured chunk size as validated bytes, if present.
    pub const fn chunk_size(&self) -> Option<ByteSize> {
        self.chunk_size
    }

    /// Returns the configured backup concurrency, if present.
    pub const fn concurrency(&self) -> Option<usize> {
        self.concurrency
    }

    /// Returns the configured ignore rules in file order.
    pub fn ignore(&self) -> &[String] {
        &self.ignore
    }

    /// Alias for [`Self::ignore`].
    pub fn ignore_rules(&self) -> &[String] {
        self.ignore()
    }
}

/// A validated positive byte size parsed from a TOML size string.
#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub struct ByteSize(u64);

impl ByteSize {
    /// Returns the size in bytes.
    pub const fn bytes(self) -> u64 {
        self.0
    }

    /// Alias for [`Self::bytes`].
    pub const fn as_u64(self) -> u64 {
        self.bytes()
    }

    /// Alias for [`Self::bytes`].
    pub const fn as_bytes(self) -> u64 {
        self.bytes()
    }

    /// Alias for [`Self::bytes`].
    pub const fn get(self) -> u64 {
        self.bytes()
    }
}

impl fmt::Display for ByteSize {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.bytes().fmt(formatter)
    }
}

/// Live synchronization defaults from `gib.toml`.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct LiveConfiguration {
    message: Option<String>,
    debounce: Option<Duration>,
    poll: Option<Duration>,
}

impl LiveConfiguration {
    /// Returns the Live default message, if present.
    pub fn message(&self) -> Option<&str> {
        self.message.as_deref()
    }

    /// Returns the debounce interval as a duration, if present.
    pub const fn debounce(&self) -> Option<Duration> {
        self.debounce
    }

    /// Returns the remote polling interval as a duration, if present.
    pub const fn poll(&self) -> Option<Duration> {
        self.poll
    }

    /// Alias for [`Self::debounce`] using duration terminology.
    pub const fn debounce_duration(&self) -> Option<Duration> {
        self.debounce()
    }

    /// Alias for [`Self::poll`] using duration terminology.
    pub const fn poll_duration(&self) -> Option<Duration> {
        self.poll()
    }

    /// Returns the debounce interval in milliseconds, if present.
    pub fn debounce_ms(&self) -> Option<u64> {
        self.debounce.map(|duration| duration.as_millis() as u64)
    }

    /// Returns the remote polling interval in milliseconds, if present.
    pub fn poll_ms(&self) -> Option<u64> {
        self.poll.map(|duration| duration.as_millis() as u64)
    }
}

/// Restore defaults from `gib.toml`.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct RestoreConfiguration {
    target_path: Option<PathBuf>,
}

impl RestoreConfiguration {
    /// Returns the restore target resolved against the configuration directory.
    pub fn target_path(&self) -> Option<&Path> {
        self.target_path.as_deref()
    }
}

/// Alias for [`ProjectConfiguration`].
pub type Configuration = ProjectConfiguration;

/// Alias for [`ProjectConfiguration`].
pub type GibConfiguration = ProjectConfiguration;

/// Alias for [`ProjectConfiguration`].
pub type GibConfig = ProjectConfiguration;

/// Alias for [`ProjectConfiguration`].
pub type LocalConfig = ProjectConfiguration;

/// Alias for [`ProjectConfiguration`].
pub type ProjectConfig = ProjectConfiguration;

/// Alias for [`RepositoryConfiguration`].
pub type ProjectRepositoryConfiguration = RepositoryConfiguration;

/// Alias for [`RepositoryConfiguration`].
pub type RepositoryConfig = RepositoryConfiguration;

/// Alias for [`BackupConfiguration`].
pub type BackupConfig = BackupConfiguration;

/// Alias for [`LiveConfiguration`].
pub type LiveConfig = LiveConfiguration;

/// Alias for [`RestoreConfiguration`].
pub type RestoreConfig = RestoreConfiguration;

/// Alias for [`ProjectConfigurationError`].
pub type ConfigurationFileError = ProjectConfigurationError;

/// Alias for [`ProjectConfigurationError`].
pub type ConfigurationParseError = ProjectConfigurationError;

/// Alias for [`ProjectConfigurationError`].
pub type GibConfigurationError = ProjectConfigurationError;

/// Alias for [`ProjectConfigurationError`].
pub type ProjectConfigError = ProjectConfigurationError;

/// Alias for [`ProjectConfigurationError`].
pub type ConfigError = ProjectConfigurationError;

/// Alias for [`ProjectConfigurationErrorKind`].
pub type ConfigErrorKind = ProjectConfigurationErrorKind;

/// Alias for [`ProjectConfigurationErrorKind`].
pub type GibConfigurationErrorKind = ProjectConfigurationErrorKind;

/// Reads and validates a project-local `gib.toml` file.
pub fn load_configuration(
    path: impl AsRef<Path>,
) -> Result<ProjectConfiguration, ProjectConfigurationError> {
    ProjectConfiguration::from_file(path)
}

/// Parses and validates TOML using an explicit configuration directory for
/// relative backup and restore paths.
pub fn parse_configuration(
    contents: impl AsRef<str>,
    config_directory: impl AsRef<Path>,
) -> Result<ProjectConfiguration, ProjectConfigurationError> {
    ProjectConfiguration::parse(contents, config_directory)
}

impl super::client::Client {
    /// Reads and validates a project-local `gib.toml` file.
    pub fn load_configuration(
        &self,
        path: impl AsRef<Path>,
    ) -> Result<ProjectConfiguration, ProjectConfigurationError> {
        load_configuration(path)
    }

    /// Parses and validates TOML using an explicit configuration directory for
    /// relative backup and restore paths.
    pub fn parse_configuration(
        &self,
        contents: impl AsRef<str>,
        config_directory: impl AsRef<Path>,
    ) -> Result<ProjectConfiguration, ProjectConfigurationError> {
        parse_configuration(contents, config_directory)
    }
}

fn map_parse_error(
    error: crate::infrastructure::project_configuration::ProjectConfigurationParseError,
    file: Option<&Path>,
) -> ProjectConfigurationError {
    match error {
        crate::infrastructure::project_configuration::ProjectConfigurationParseError::Document(
            error,
        ) => map_document_error(error, file),
        crate::infrastructure::project_configuration::ProjectConfigurationParseError::Validation(
            error,
        ) => ProjectConfigurationError::new(
            ProjectConfigurationErrorKind::InvalidValue,
            file,
            Some(error.field().to_owned()),
            error.reason(),
        ),
    }
}

fn map_load_error(error: ProjectConfigurationLoadError) -> ProjectConfigurationError {
    match error {
        ProjectConfigurationLoadError::Read { path } => ProjectConfigurationError::new(
            ProjectConfigurationErrorKind::Io,
            Some(&path),
            None,
            "the file could not be read",
        ),
        ProjectConfigurationLoadError::InvalidPath { path } => ProjectConfigurationError::new(
            ProjectConfigurationErrorKind::InvalidPath,
            Some(&path),
            None,
            "the path must identify a regular file",
        ),
        ProjectConfigurationLoadError::InputTooLarge { path } => ProjectConfigurationError::new(
            ProjectConfigurationErrorKind::InputTooLarge,
            Some(&path),
            None,
            format!("must be at most {MAX_CONFIGURATION_BYTES} bytes"),
        ),
        ProjectConfigurationLoadError::Document { path, error } => {
            map_document_error(error, Some(&path))
        }
        ProjectConfigurationLoadError::Validation { path, error } => {
            ProjectConfigurationError::new(
                ProjectConfigurationErrorKind::InvalidValue,
                Some(&path),
                Some(error.field().to_owned()),
                error.reason(),
            )
        }
    }
}

fn map_document_error(
    error: ConfigurationDocumentError,
    file: Option<&Path>,
) -> ProjectConfigurationError {
    let field = error.field().map(str::to_owned);
    match error.kind() {
        ConfigurationDocumentErrorKind::Parse => ProjectConfigurationError::new(
            ProjectConfigurationErrorKind::Parse,
            file,
            field,
            error.reason(),
        ),
        ConfigurationDocumentErrorKind::MissingField => ProjectConfigurationError::new(
            ProjectConfigurationErrorKind::MissingField,
            file,
            field,
            error.reason(),
        ),
        ConfigurationDocumentErrorKind::UnknownField => ProjectConfigurationError::new(
            ProjectConfigurationErrorKind::UnknownField,
            file,
            field,
            error.reason(),
        ),
        ConfigurationDocumentErrorKind::InvalidType => ProjectConfigurationError::new(
            ProjectConfigurationErrorKind::InvalidType,
            file,
            field,
            error.reason(),
        ),
        ConfigurationDocumentErrorKind::InvalidValue => ProjectConfigurationError::new(
            ProjectConfigurationErrorKind::InvalidValue,
            file,
            field,
            error.reason(),
        ),
        ConfigurationDocumentErrorKind::UnsupportedVersion => {
            ProjectConfigurationError::unsupported_version(
                file,
                field,
                error.version(),
                error.reason(),
            )
        }
        ConfigurationDocumentErrorKind::InputTooLarge => ProjectConfigurationError::new(
            ProjectConfigurationErrorKind::InputTooLarge,
            file,
            field,
            error.reason(),
        ),
    }
}
