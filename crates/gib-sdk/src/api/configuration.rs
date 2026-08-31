use std::collections::BTreeSet;
use std::fmt;
use std::path::{Path, PathBuf};
use std::time::Duration;

pub use crate::application::ports::{ConfigurationFileMetadata, ConfigurationFileSystem};
use crate::domain::{
    BackupConfigurationInput, ConfigurationInput, LiveConfigurationInput,
    RepositoryConfigurationInput, RestoreConfigurationInput, ValidatedConfiguration,
    validate_configuration,
};
use crate::format::{
    ConfigurationDocumentError, ConfigurationDocumentErrorKind,
    MAX_CONFIGURATION_BYTES as MAX_CONFIGURATION_DOCUMENT_BYTES,
};
use crate::infrastructure::project_configuration::{
    ProjectConfigurationLoadError, load_project_configuration_with_file_system,
    parse_project_configuration,
};

pub use crate::application::ports::{
    ConfigurationFileMetadata as ProjectConfigurationFileMetadata,
    ConfigurationFileSystem as ProjectConfigurationFileSystem,
};
pub use crate::infrastructure::project_configuration::LocalConfigurationFileSystem;
pub use crate::infrastructure::project_configuration::LocalConfigurationFileSystem as OsConfigurationFileSystem;

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

/// Selects how a project configuration file is chosen for one invocation.
#[non_exhaustive]
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum ConfigurationSelection {
    /// Search the supplied starting directory and its ancestors.
    Discover,
    /// Load the supplied file after resolving and canonicalizing its path.
    Explicit(PathBuf),
    /// Do not inspect or load any project configuration file.
    Disabled,
}

impl ConfigurationSelection {
    /// Selects nearest-file discovery.
    pub const fn discover() -> Self {
        Self::Discover
    }

    /// Selects one explicit configuration path.
    pub fn explicit(path: impl AsRef<Path>) -> Self {
        Self::Explicit(path.as_ref().to_path_buf())
    }

    /// Disables project configuration discovery and loading.
    pub const fn disabled() -> Self {
        Self::Disabled
    }

    /// Returns the explicit path, when this selection uses one.
    pub fn path(&self) -> Option<&Path> {
        match self {
            Self::Explicit(path) => Some(path),
            Self::Discover | Self::Disabled => None,
        }
    }

    /// Returns whether nearest-file discovery is selected.
    pub const fn is_discovery(&self) -> bool {
        matches!(self, Self::Discover)
    }

    /// Returns whether an explicit file is selected.
    pub const fn is_explicit(&self) -> bool {
        matches!(self, Self::Explicit(_))
    }

    /// Returns whether project configuration is disabled.
    pub const fn is_disabled(&self) -> bool {
        matches!(self, Self::Disabled)
    }
}

/// Identifies the project configuration source used for one resolution.
#[non_exhaustive]
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum ConfigurationSource {
    /// No file was found and SDK defaults were used.
    Defaults,
    /// A file found by nearest-ancestor discovery was used.
    Discovered(PathBuf),
    /// A caller-selected file was used.
    Explicit(PathBuf),
    /// File loading was disabled for this resolution.
    Disabled,
}

impl ConfigurationSource {
    /// Returns the selected file path, when a file was loaded.
    pub fn path(&self) -> Option<&Path> {
        match self {
            Self::Discovered(path) | Self::Explicit(path) => Some(path),
            Self::Defaults | Self::Disabled => None,
        }
    }

    /// Returns whether a project configuration file was loaded.
    pub const fn is_loaded(&self) -> bool {
        matches!(self, Self::Discovered(_) | Self::Explicit(_))
    }

    /// Returns whether SDK defaults were used because no file was found.
    pub const fn is_default(&self) -> bool {
        matches!(self, Self::Defaults)
    }

    /// Returns whether file loading was explicitly disabled.
    pub const fn is_disabled(&self) -> bool {
        matches!(self, Self::Disabled)
    }
}

impl fmt::Display for ConfigurationSource {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Defaults => formatter.write_str("defaults"),
            Self::Discovered(path) => write!(formatter, "discovered file '{}'", path.display()),
            Self::Explicit(path) => write!(formatter, "explicit file '{}'", path.display()),
            Self::Disabled => formatter.write_str("disabled"),
        }
    }
}

/// Structured diagnostic event describing the selected project configuration.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ConfigurationSourceEvent {
    loaded: bool,
    path: Option<PathBuf>,
    source: ConfigurationSource,
}

impl ConfigurationSourceEvent {
    fn from_source(source: &ConfigurationSource) -> Self {
        Self {
            loaded: source.is_loaded(),
            path: source.path().map(Path::to_path_buf),
            source: source.clone(),
        }
    }

    /// Returns whether a project configuration file was loaded.
    pub const fn loaded(&self) -> bool {
        self.loaded
    }

    /// Returns the loaded configuration path, when present.
    pub fn path(&self) -> Option<&Path> {
        self.path.as_deref()
    }

    /// Returns the source classification.
    pub const fn source(&self) -> &ConfigurationSource {
        &self.source
    }
}

/// Values supplied by a command-line invocation.
///
/// The type stores overrides only. It does not read files, consult the current
/// directory, or apply defaults; [`ConfigurationResolver`] performs that work
/// after receiving an explicit resolution request.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct ConfigurationOverrides {
    repository_storage: Option<String>,
    repository_key: Option<String>,
    backup_root_path: Option<String>,
    backup_message: Option<String>,
    backup_compress: Option<i32>,
    backup_chunk_size: Option<String>,
    backup_concurrency: Option<usize>,
    backup_ignore: Vec<String>,
    live_message: Option<String>,
    live_debounce_ms: Option<u64>,
    live_poll_ms: Option<u64>,
    restore_target_path: Option<String>,
}

impl ConfigurationOverrides {
    /// Creates an empty set of command-line overrides.
    pub const fn new() -> Self {
        Self {
            repository_storage: None,
            repository_key: None,
            backup_root_path: None,
            backup_message: None,
            backup_compress: None,
            backup_chunk_size: None,
            backup_concurrency: None,
            backup_ignore: Vec::new(),
            live_message: None,
            live_debounce_ms: None,
            live_poll_ms: None,
            restore_target_path: None,
        }
    }

    /// Overrides `repository.storage`.
    pub fn with_repository_storage(mut self, value: impl Into<String>) -> Self {
        self.repository_storage = Some(value.into());
        self
    }

    /// Alias for [`Self::with_repository_storage`].
    pub fn with_storage(self, value: impl Into<String>) -> Self {
        self.with_repository_storage(value)
    }

    /// Overrides `repository.key`.
    pub fn with_repository_key(mut self, value: impl Into<String>) -> Self {
        self.repository_key = Some(value.into());
        self
    }

    /// Alias for [`Self::with_repository_key`].
    pub fn with_key(self, value: impl Into<String>) -> Self {
        self.with_repository_key(value)
    }

    /// Overrides `backup.root_path`.
    pub fn with_backup_root_path(mut self, value: impl AsRef<Path>) -> Self {
        self.backup_root_path = Some(value.as_ref().to_string_lossy().into_owned());
        self
    }

    /// Alias for [`Self::with_backup_root_path`].
    pub fn with_root_path(self, value: impl AsRef<Path>) -> Self {
        self.with_backup_root_path(value)
    }

    /// Overrides `backup.message`.
    pub fn with_backup_message(mut self, value: impl Into<String>) -> Self {
        self.backup_message = Some(value.into());
        self
    }

    /// Alias for [`Self::with_backup_message`].
    pub fn with_message(self, value: impl Into<String>) -> Self {
        self.with_backup_message(value)
    }

    /// Overrides `backup.compress`.
    pub const fn with_backup_compress(mut self, value: i32) -> Self {
        self.backup_compress = Some(value);
        self
    }

    /// Alias for [`Self::with_backup_compress`].
    pub const fn with_compress(self, value: i32) -> Self {
        self.with_backup_compress(value)
    }

    /// Overrides `backup.chunk_size` with its configuration string.
    pub fn with_backup_chunk_size(mut self, value: impl Into<String>) -> Self {
        self.backup_chunk_size = Some(value.into());
        self
    }

    /// Alias for [`Self::with_backup_chunk_size`].
    pub fn with_chunk_size(self, value: impl Into<String>) -> Self {
        self.with_backup_chunk_size(value)
    }

    /// Overrides `backup.concurrency`.
    pub const fn with_backup_concurrency(mut self, value: usize) -> Self {
        self.backup_concurrency = Some(value);
        self
    }

    /// Alias for [`Self::with_backup_concurrency`].
    pub const fn with_concurrency(self, value: usize) -> Self {
        self.with_backup_concurrency(value)
    }

    /// Adds one command-line ignore rule.
    pub fn with_ignore_rule(mut self, value: impl Into<String>) -> Self {
        self.backup_ignore.push(value.into());
        self
    }

    /// Adds repeated command-line ignore rules.
    pub fn with_ignore_rules<I, T>(mut self, values: I) -> Self
    where
        I: IntoIterator<Item = T>,
        T: Into<String>,
    {
        self.backup_ignore
            .extend(values.into_iter().map(Into::into));
        self
    }

    /// Alias for [`Self::with_ignore_rules`].
    pub fn with_backup_ignore_rules<I, T>(self, values: I) -> Self
    where
        I: IntoIterator<Item = T>,
        T: Into<String>,
    {
        self.with_ignore_rules(values)
    }

    /// Overrides `live.message`.
    pub fn with_live_message(mut self, value: impl Into<String>) -> Self {
        self.live_message = Some(value.into());
        self
    }

    /// Overrides `live.debounce_ms`.
    pub const fn with_live_debounce_ms(mut self, value: u64) -> Self {
        self.live_debounce_ms = Some(value);
        self
    }

    /// Overrides `live.poll_ms`.
    pub const fn with_live_poll_ms(mut self, value: u64) -> Self {
        self.live_poll_ms = Some(value);
        self
    }

    /// Overrides `restore.target_path`.
    pub fn with_restore_target_path(mut self, value: impl AsRef<Path>) -> Self {
        self.restore_target_path = Some(value.as_ref().to_string_lossy().into_owned());
        self
    }

    /// Alias for [`Self::with_restore_target_path`].
    pub fn with_target_path(self, value: impl AsRef<Path>) -> Self {
        self.with_restore_target_path(value)
    }

    /// Returns the `repository.storage` override, when present.
    pub fn repository_storage(&self) -> Option<&str> {
        self.repository_storage.as_deref()
    }

    /// Returns the `repository.key` override, when present.
    pub fn repository_key(&self) -> Option<&str> {
        self.repository_key.as_deref()
    }

    /// Returns the `backup.root_path` override, when present.
    pub fn backup_root_path(&self) -> Option<&Path> {
        self.backup_root_path.as_deref().map(Path::new)
    }

    /// Returns the `backup.message` override, when present.
    pub fn backup_message(&self) -> Option<&str> {
        self.backup_message.as_deref()
    }

    /// Returns the `backup.compress` override, when present.
    pub const fn backup_compress(&self) -> Option<i32> {
        self.backup_compress
    }

    /// Returns the `backup.chunk_size` override, when present.
    pub fn backup_chunk_size(&self) -> Option<&str> {
        self.backup_chunk_size.as_deref()
    }

    /// Returns the `backup.concurrency` override, when present.
    pub const fn backup_concurrency(&self) -> Option<usize> {
        self.backup_concurrency
    }

    /// Returns repeated `backup.ignore` overrides in supplied order.
    pub fn backup_ignore_rules(&self) -> &[String] {
        &self.backup_ignore
    }

    /// Returns the `live.message` override, when present.
    pub fn live_message(&self) -> Option<&str> {
        self.live_message.as_deref()
    }

    /// Returns the `live.debounce_ms` override, when present.
    pub const fn live_debounce_ms(&self) -> Option<u64> {
        self.live_debounce_ms
    }

    /// Returns the `live.poll_ms` override, when present.
    pub const fn live_poll_ms(&self) -> Option<u64> {
        self.live_poll_ms
    }

    /// Returns the `restore.target_path` override, when present.
    pub fn restore_target_path(&self) -> Option<&Path> {
        self.restore_target_path.as_deref().map(Path::new)
    }

    /// Returns whether no override has been supplied.
    pub fn is_empty(&self) -> bool {
        self == &Self::default()
    }
}

/// Inputs used to resolve one command's effective configuration.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ConfigurationResolutionRequest {
    starting_directory: PathBuf,
    selection: ConfigurationSelection,
    overrides: ConfigurationOverrides,
}

impl ConfigurationResolutionRequest {
    /// Creates a request using nearest-file discovery and no CLI overrides.
    pub fn new(starting_directory: impl AsRef<Path>) -> Self {
        Self {
            starting_directory: starting_directory.as_ref().to_path_buf(),
            selection: ConfigurationSelection::Discover,
            overrides: ConfigurationOverrides::default(),
        }
    }

    /// Returns the directory from which discovery and relative CLI paths start.
    pub fn starting_directory(&self) -> &Path {
        &self.starting_directory
    }

    /// Returns the file-selection policy.
    pub const fn selection(&self) -> &ConfigurationSelection {
        &self.selection
    }

    /// Returns the command-line overrides.
    pub const fn overrides(&self) -> &ConfigurationOverrides {
        &self.overrides
    }

    /// Selects an explicit configuration file.
    pub fn with_config_path(mut self, path: impl AsRef<Path>) -> Self {
        self.selection = ConfigurationSelection::explicit(path);
        self
    }

    /// Alias for [`Self::with_config_path`].
    pub fn with_explicit_path(self, path: impl AsRef<Path>) -> Self {
        self.with_config_path(path)
    }

    /// Disables project configuration loading.
    pub fn without_config(mut self) -> Self {
        self.selection = ConfigurationSelection::Disabled;
        self
    }

    /// Alias for [`Self::without_config`].
    pub fn no_config(self) -> Self {
        self.without_config()
    }

    /// Replaces the file-selection policy.
    pub fn with_selection(mut self, selection: ConfigurationSelection) -> Self {
        self.selection = selection;
        self
    }

    /// Replaces the command-line override set.
    pub fn with_overrides(mut self, overrides: ConfigurationOverrides) -> Self {
        self.overrides = overrides;
        self
    }
}

/// Effective project configuration and the source selected to produce it.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ResolvedConfiguration {
    configuration: ProjectConfiguration,
    source: ConfigurationSource,
}

impl ResolvedConfiguration {
    fn new(configuration: ProjectConfiguration, source: ConfigurationSource) -> Self {
        Self {
            configuration,
            source,
        }
    }

    /// Returns the effective configuration after defaults, file values, and
    /// command-line overrides have been applied.
    pub const fn configuration(&self) -> &ProjectConfiguration {
        &self.configuration
    }

    /// Alias for [`Self::configuration`].
    pub const fn config(&self) -> &ProjectConfiguration {
        self.configuration()
    }

    /// Returns the selected configuration source.
    pub const fn source(&self) -> &ConfigurationSource {
        &self.source
    }

    /// Returns the selected configuration file path, when a file was loaded.
    pub fn path(&self) -> Option<&Path> {
        self.source.path()
    }

    /// Creates the structured diagnostic event for this resolution.
    pub fn source_event(&self) -> ConfigurationSourceEvent {
        ConfigurationSourceEvent::from_source(&self.source)
    }
}

/// Alias for [`ResolvedConfiguration`].
pub type ConfigurationResolution = ResolvedConfiguration;

/// Resolves project configuration through an injected filesystem adapter.
pub struct ConfigurationResolver<F = LocalConfigurationFileSystem> {
    file_system: F,
}

impl<F> ConfigurationResolver<F>
where
    F: ConfigurationFileSystem,
{
    /// Creates a resolver using the supplied filesystem capability.
    pub const fn new(file_system: F) -> Self {
        Self { file_system }
    }

    /// Finds the nearest canonical `gib.toml` at or above `start_directory`.
    pub fn discover(
        &self,
        start_directory: impl AsRef<Path>,
    ) -> Result<Option<PathBuf>, ProjectConfigurationError> {
        discover_with_file_system(&self.file_system, start_directory.as_ref())
    }

    /// Applies selection, file loading, defaults, and CLI overrides.
    pub fn resolve(
        &self,
        request: ConfigurationResolutionRequest,
    ) -> Result<ResolvedConfiguration, ProjectConfigurationError> {
        let (configuration, source) = match &request.selection {
            ConfigurationSelection::Disabled => (
                ProjectConfiguration::default(),
                ConfigurationSource::Disabled,
            ),
            ConfigurationSelection::Discover => match self.discover(&request.starting_directory)? {
                Some(path) => (self.load(&path)?, ConfigurationSource::Discovered(path)),
                None => (
                    ProjectConfiguration::default(),
                    ConfigurationSource::Defaults,
                ),
            },
            ConfigurationSelection::Explicit(path) => {
                let requested_path = if path.is_absolute() {
                    path.clone()
                } else {
                    request.starting_directory.join(path)
                };
                let canonical_path = self.canonicalize_explicit(&requested_path)?;
                (
                    self.load(&canonical_path)?,
                    ConfigurationSource::Explicit(canonical_path),
                )
            }
        };
        let configuration =
            configuration.with_overrides(&request.overrides, &request.starting_directory)?;
        Ok(ResolvedConfiguration::new(configuration, source))
    }

    fn canonicalize_explicit(&self, path: &Path) -> Result<PathBuf, ProjectConfigurationError> {
        let canonical_path = self.file_system.canonicalize(path).map_err(|_| {
            ProjectConfigurationError::new(
                ProjectConfigurationErrorKind::InvalidPath,
                Some(path),
                None,
                "the path does not exist or could not be canonicalized",
            )
        })?;
        let metadata = self.file_system.metadata(&canonical_path).map_err(|_| {
            ProjectConfigurationError::new(
                ProjectConfigurationErrorKind::InvalidPath,
                Some(&canonical_path),
                None,
                "the path could not be inspected",
            )
        })?;
        if !metadata.is_file() {
            return Err(ProjectConfigurationError::new(
                ProjectConfigurationErrorKind::InvalidPath,
                Some(&canonical_path),
                None,
                "the path must identify a regular file",
            ));
        }
        Ok(canonical_path)
    }

    fn load(&self, path: &Path) -> Result<ProjectConfiguration, ProjectConfigurationError> {
        load_project_configuration_with_file_system(&self.file_system, path)
            .map(ProjectConfiguration::from_validated)
            .map_err(map_load_error)
    }
}

impl Default for ConfigurationResolver<LocalConfigurationFileSystem> {
    fn default() -> Self {
        Self::new(LocalConfigurationFileSystem)
    }
}

/// Finds the nearest canonical `gib.toml` using the host filesystem.
pub fn discover_configuration(
    start_directory: impl AsRef<Path>,
) -> Result<Option<PathBuf>, ProjectConfigurationError> {
    ConfigurationResolver::default().discover(start_directory)
}

/// Finds the nearest canonical `gib.toml` using an injected filesystem.
pub fn discover_configuration_with_file_system<F>(
    file_system: &F,
    start_directory: impl AsRef<Path>,
) -> Result<Option<PathBuf>, ProjectConfigurationError>
where
    F: ConfigurationFileSystem + ?Sized,
{
    discover_with_file_system(file_system, start_directory.as_ref())
}

/// Resolves configuration using the host filesystem.
pub fn resolve_configuration(
    request: ConfigurationResolutionRequest,
) -> Result<ResolvedConfiguration, ProjectConfigurationError> {
    ConfigurationResolver::default().resolve(request)
}

/// Resolves configuration using an injected filesystem.
pub fn resolve_configuration_with_file_system<F>(
    file_system: F,
    request: ConfigurationResolutionRequest,
) -> Result<ResolvedConfiguration, ProjectConfigurationError>
where
    F: ConfigurationFileSystem,
{
    ConfigurationResolver::new(file_system).resolve(request)
}

fn discover_with_file_system<F>(
    file_system: &F,
    start_directory: &Path,
) -> Result<Option<PathBuf>, ProjectConfigurationError>
where
    F: ConfigurationFileSystem + ?Sized,
{
    let mut directory = file_system.canonicalize(start_directory).map_err(|error| {
        ProjectConfigurationError::new(
            ProjectConfigurationErrorKind::Io,
            Some(start_directory),
            None,
            format!("could not canonicalize the starting directory: {error}"),
        )
    })?;

    loop {
        let candidate = directory.join(GIB_CONFIGURATION_FILE_NAME);
        match file_system.metadata(&candidate) {
            Ok(metadata) if metadata.is_file() => {
                let canonical_path = file_system.canonicalize(&candidate).map_err(|error| {
                    ProjectConfigurationError::new(
                        ProjectConfigurationErrorKind::Io,
                        Some(&candidate),
                        None,
                        format!("could not canonicalize the discovered file: {error}"),
                    )
                })?;
                return Ok(Some(canonical_path));
            }
            Ok(_) => {}
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => {
                return Err(ProjectConfigurationError::new(
                    ProjectConfigurationErrorKind::Io,
                    Some(&candidate),
                    None,
                    format!("could not inspect the candidate file: {error}"),
                ));
            }
        }

        let Some(parent) = directory.parent() else {
            return Ok(None);
        };
        if parent == directory {
            return Ok(None);
        }
        directory = parent.to_path_buf();
    }
}

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

    /// Returns the stable machine-readable configuration error code.
    pub const fn code(&self) -> &'static str {
        match self.kind {
            ProjectConfigurationErrorKind::Io => "configuration_io",
            ProjectConfigurationErrorKind::InputTooLarge => "configuration_input_too_large",
            ProjectConfigurationErrorKind::Parse => "configuration_parse",
            ProjectConfigurationErrorKind::MissingField => "configuration_missing_field",
            ProjectConfigurationErrorKind::UnknownField => "configuration_unknown_field",
            ProjectConfigurationErrorKind::InvalidType => "configuration_invalid_type",
            ProjectConfigurationErrorKind::InvalidValue => "configuration_invalid_value",
            ProjectConfigurationErrorKind::UnsupportedVersion => {
                "configuration_unsupported_version"
            }
            ProjectConfigurationErrorKind::InvalidPath => "configuration_invalid_path",
        }
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

    /// Applies command-line overrides using the supplied invocation directory
    /// for relative CLI paths.
    pub fn with_overrides(
        &self,
        overrides: &ConfigurationOverrides,
        invocation_directory: impl AsRef<Path>,
    ) -> Result<Self, ProjectConfigurationError> {
        let invocation_directory = invocation_directory.as_ref();
        let mut input = input_from_configuration(self);

        if let Some(value) = &overrides.repository_storage {
            input.repository.storage = Some(value.clone());
        }
        if let Some(value) = &overrides.repository_key {
            input.repository.key = Some(value.clone());
        }
        if let Some(value) = &overrides.backup_root_path {
            input.backup.root_path = Some(resolve_cli_path(value, invocation_directory));
        }
        if let Some(value) = &overrides.backup_message {
            input.backup.message = Some(value.clone());
        }
        if let Some(value) = overrides.backup_compress {
            input.backup.compress = Some(value);
        }
        if let Some(value) = &overrides.backup_chunk_size {
            input.backup.chunk_size = Some(value.clone());
        }
        if let Some(value) = overrides.backup_concurrency {
            input.backup.concurrency = Some(value);
        }
        input.backup.ignore = merge_ignore_rules(&self.backup.ignore, &overrides.backup_ignore);
        if let Some(value) = &overrides.live_message {
            input.live.message = Some(value.clone());
        }
        if let Some(value) = overrides.live_debounce_ms {
            input.live.debounce_ms = Some(value);
        }
        if let Some(value) = overrides.live_poll_ms {
            input.live.poll_ms = Some(value);
        }
        if let Some(value) = &overrides.restore_target_path {
            input.restore.target_path = Some(resolve_cli_path(value, invocation_directory));
        }

        validate_configuration(input, Path::new(""))
            .map(Self::from_validated)
            .map_err(|error| {
                ProjectConfigurationError::new(
                    ProjectConfigurationErrorKind::InvalidValue,
                    None,
                    Some(error.field().to_owned()),
                    error.reason(),
                )
            })
    }

    /// Alias for [`Self::with_overrides`].
    pub fn merge(
        &self,
        overrides: &ConfigurationOverrides,
        invocation_directory: impl AsRef<Path>,
    ) -> Result<Self, ProjectConfigurationError> {
        self.with_overrides(overrides, invocation_directory)
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

fn input_from_configuration(configuration: &ProjectConfiguration) -> ConfigurationInput {
    ConfigurationInput {
        version: configuration.version,
        repository: RepositoryConfigurationInput {
            storage: configuration.repository.storage.clone(),
            key: configuration.repository.key.clone(),
        },
        backup: BackupConfigurationInput {
            root_path: configuration
                .backup
                .root_path
                .as_deref()
                .map(path_to_string),
            message: configuration.backup.message.clone(),
            compress: configuration.backup.compress,
            chunk_size: configuration
                .backup
                .chunk_size
                .map(|value| format!("{} B", value.bytes())),
            concurrency: configuration.backup.concurrency,
            ignore: configuration.backup.ignore.clone(),
        },
        live: LiveConfigurationInput {
            message: configuration.live.message.clone(),
            debounce_ms: configuration
                .live
                .debounce
                .map(|value| value.as_millis() as u64),
            poll_ms: configuration
                .live
                .poll
                .map(|value| value.as_millis() as u64),
        },
        restore: RestoreConfigurationInput {
            target_path: configuration
                .restore
                .target_path
                .as_deref()
                .map(path_to_string),
        },
    }
}

fn path_to_string(path: &Path) -> String {
    path.to_string_lossy().into_owned()
}

fn resolve_cli_path(value: &str, invocation_directory: &Path) -> String {
    if value.is_empty() {
        return String::new();
    }
    let path = Path::new(value);
    if path.is_absolute() {
        path_to_string(path)
    } else {
        path_to_string(&invocation_directory.join(path))
    }
}

/// Merges file and command-line ignore rules in deterministic sorted order.
pub fn merge_ignore_rules(config_values: &[String], cli_values: &[String]) -> Vec<String> {
    config_values
        .iter()
        .chain(cli_values)
        .cloned()
        .collect::<BTreeSet<_>>()
        .into_iter()
        .collect()
}

/// Alias for [`merge_ignore_rules`].
pub fn merge_ignore_patterns(config_values: &[String], cli_values: &[String]) -> Vec<String> {
    merge_ignore_rules(config_values, cli_values)
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

    /// Resolves project configuration using the host filesystem.
    pub fn resolve_configuration(
        &self,
        request: ConfigurationResolutionRequest,
    ) -> Result<ResolvedConfiguration, ProjectConfigurationError> {
        resolve_configuration(request)
    }

    /// Resolves project configuration through an injected filesystem adapter.
    pub fn resolve_configuration_with_file_system<F>(
        &self,
        file_system: F,
        request: ConfigurationResolutionRequest,
    ) -> Result<ResolvedConfiguration, ProjectConfigurationError>
    where
        F: ConfigurationFileSystem,
    {
        resolve_configuration_with_file_system(file_system, request)
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
