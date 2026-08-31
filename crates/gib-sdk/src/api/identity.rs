use super::client::Client;
use super::error::{SdkError, SdkResult};
pub use crate::application::ports::{
    ConfigurationError, ConfigurationResult, ConfigurationStorage,
};
use crate::application::{
    IdentityError, get_identity as get_identity_use_case, read_identity as read_identity_use_case,
    set_identity as set_identity_use_case,
};
pub use crate::domain::{AuthorIdentity, MAX_AUTHOR_IDENTITY_LENGTH};
pub use crate::infrastructure::configuration::{
    GLOBAL_CONFIGURATION_DIRECTORY, IDENTITY_CONFIGURATION_FILE_NAME, LocalConfiguration,
    MemoryConfiguration,
};
use std::fmt;
use std::sync::Arc;

pub use crate::application::ports::ConfigurationStorage as IdentityConfigurationStorage;
pub use crate::domain::AuthorIdentity as Author;
pub use crate::domain::AuthorIdentity as UserIdentity;

/// The current version of the global identity configuration.
pub const CURRENT_IDENTITY_CONFIGURATION_VERSION: u16 =
    crate::format::CURRENT_IDENTITY_CONFIGURATION_VERSION;

/// The largest accepted global author identity length in UTF-8 bytes.
pub const MAX_AUTHOR_LENGTH: usize = MAX_AUTHOR_IDENTITY_LENGTH;

/// A cloneable type-erased handle for a global configuration store.
#[derive(Clone)]
pub struct ConfigurationHandle {
    inner: Arc<dyn ConfigurationStorage>,
}

impl ConfigurationHandle {
    /// Wraps a thread-safe configuration storage adapter.
    pub fn new<S>(storage: S) -> Self
    where
        S: ConfigurationStorage + 'static,
    {
        Self {
            inner: Arc::new(storage),
        }
    }

    /// Wraps an existing type-erased configuration storage adapter.
    pub fn from_arc(storage: Arc<dyn ConfigurationStorage>) -> Self {
        Self { inner: storage }
    }

    /// Returns the storage adapter used by identity operations.
    pub fn as_storage(&self) -> &dyn ConfigurationStorage {
        self.inner.as_ref()
    }
}

impl<S> From<S> for ConfigurationHandle
where
    S: ConfigurationStorage + 'static,
{
    fn from(storage: S) -> Self {
        Self::new(storage)
    }
}

impl From<&LocalConfiguration> for ConfigurationHandle {
    fn from(storage: &LocalConfiguration) -> Self {
        Self::new(storage.clone())
    }
}

impl From<&MemoryConfiguration> for ConfigurationHandle {
    fn from(storage: &MemoryConfiguration) -> Self {
        Self::new(storage.clone())
    }
}

impl fmt::Debug for ConfigurationHandle {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("ConfigurationHandle(..)")
    }
}

/// A validated request for setting the global author identity.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct SetIdentityRequest {
    identity: AuthorIdentity,
}

impl SetIdentityRequest {
    /// Creates a request by validating a `Name <email>` representation.
    pub fn new(value: impl AsRef<str>) -> SdkResult<Self> {
        Ok(Self {
            identity: AuthorIdentity::new(value.as_ref()).map_err(SdkError::from)?,
        })
    }

    /// Creates a request from an already validated identity.
    pub const fn from_identity(identity: AuthorIdentity) -> Self {
        Self { identity }
    }

    /// Returns the validated identity to persist.
    pub const fn identity(&self) -> &AuthorIdentity {
        &self.identity
    }

    /// Alias for [`Self::identity`] using author terminology.
    pub const fn author(&self) -> &AuthorIdentity {
        self.identity()
    }

    /// Consumes the request and returns its validated identity.
    pub fn into_identity(self) -> AuthorIdentity {
        self.identity
    }
}

impl AsRef<str> for SetIdentityRequest {
    fn as_ref(&self) -> &str {
        self.identity.as_str()
    }
}

impl From<AuthorIdentity> for SetIdentityRequest {
    fn from(identity: AuthorIdentity) -> Self {
        Self::from_identity(identity)
    }
}

/// Alias for [`SetIdentityRequest`] using author terminology.
pub type SetAuthorRequest = SetIdentityRequest;

/// Returns the configured global author identity or a typed not-configured
/// error when no identity file exists.
pub fn get_identity<S>(storage: S) -> SdkResult<AuthorIdentity>
where
    S: Into<ConfigurationHandle>,
{
    get_identity_use_case(storage.into().as_storage()).map_err(SdkError::from)
}

/// Reads the configured global author identity, preserving absence as `None`.
pub fn read_identity<S>(storage: S) -> SdkResult<Option<AuthorIdentity>>
where
    S: Into<ConfigurationHandle>,
{
    read_identity_use_case(storage.into().as_storage()).map_err(SdkError::from)
}

/// Validates and atomically persists a global author identity.
pub fn set_identity<S>(storage: S, value: impl AsRef<str>) -> SdkResult<AuthorIdentity>
where
    S: Into<ConfigurationHandle>,
{
    let identity = AuthorIdentity::new(value.as_ref()).map_err(SdkError::from)?;
    set_identity_use_case(storage.into().as_storage(), identity).map_err(SdkError::from)
}

/// Returns the configured global author identity using the current user's
/// default `.gib/config.msgpack` location.
pub fn get_global_identity() -> SdkResult<AuthorIdentity> {
    let storage = LocalConfiguration::global().map_err(SdkError::from)?;
    get_identity(storage)
}

/// Validates and atomically persists an identity in the current user's global
/// `.gib/config.msgpack` file.
pub fn set_global_identity(value: impl AsRef<str>) -> SdkResult<AuthorIdentity> {
    let storage = LocalConfiguration::global().map_err(SdkError::from)?;
    set_identity(storage, value)
}

impl Client {
    /// Returns the configured author identity from an injected configuration
    /// store or a typed not-configured error.
    pub fn get_identity<S>(&self, storage: S) -> SdkResult<AuthorIdentity>
    where
        S: Into<ConfigurationHandle>,
    {
        get_identity(storage)
    }

    /// Reads the configured author identity from an injected configuration
    /// store, preserving absence as `None`.
    pub fn read_identity<S>(&self, storage: S) -> SdkResult<Option<AuthorIdentity>>
    where
        S: Into<ConfigurationHandle>,
    {
        read_identity(storage)
    }

    /// Validates and atomically persists an author identity in an injected
    /// configuration store.
    pub fn set_identity<S>(&self, storage: S, value: impl AsRef<str>) -> SdkResult<AuthorIdentity>
    where
        S: Into<ConfigurationHandle>,
    {
        set_identity(storage, value)
    }

    /// Returns the configured author identity from the current user's global
    /// configuration.
    pub fn get_global_identity(&self) -> SdkResult<AuthorIdentity> {
        get_global_identity()
    }

    /// Validates and persists an author identity in the current user's global
    /// configuration.
    pub fn set_global_identity(&self, value: impl AsRef<str>) -> SdkResult<AuthorIdentity> {
        set_global_identity(value)
    }

    /// Alias for [`Self::get_identity`] using author terminology.
    pub fn get_author<S>(&self, storage: S) -> SdkResult<AuthorIdentity>
    where
        S: Into<ConfigurationHandle>,
    {
        self.get_identity(storage)
    }

    /// Alias for [`Self::set_identity`] using author terminology.
    pub fn set_author<S>(&self, storage: S, value: impl AsRef<str>) -> SdkResult<AuthorIdentity>
    where
        S: Into<ConfigurationHandle>,
    {
        self.set_identity(storage, value)
    }
}

/// Returns the configured global author identity using the default location.
pub fn get_author() -> SdkResult<AuthorIdentity> {
    get_global_identity()
}

/// Validates and persists a global author identity using the default location.
pub fn set_author(value: impl AsRef<str>) -> SdkResult<AuthorIdentity> {
    set_global_identity(value)
}

/// Compatibility name for [`LocalConfiguration`].
pub type GlobalConfiguration = LocalConfiguration;

/// Compatibility name for [`LocalConfiguration`].
pub type LocalIdentityConfiguration = LocalConfiguration;

/// Compatibility name for [`MemoryConfiguration`].
pub type MemoryIdentityConfiguration = MemoryConfiguration;

/// The global configuration directory name used by the default location.
pub const GLOBAL_CONFIG_DIRECTORY: &str = GLOBAL_CONFIGURATION_DIRECTORY;

/// The global identity configuration filename used by the default location.
pub const GLOBAL_CONFIG_FILE_NAME: &str = IDENTITY_CONFIGURATION_FILE_NAME;

impl From<ConfigurationError> for SdkError {
    fn from(error: ConfigurationError) -> Self {
        match error {
            ConfigurationError::NotFound => Self::IdentityNotConfigured,
            ConfigurationError::InvalidPath
            | ConfigurationError::TooLarge
            | ConfigurationError::Io
            | ConfigurationError::Unavailable => Self::ConfigurationFailure {
                operation: "configuration",
            },
        }
    }
}

impl From<IdentityError> for SdkError {
    fn from(error: IdentityError) -> Self {
        match error {
            IdentityError::NotConfigured => Self::IdentityNotConfigured,
            IdentityError::Malformed => Self::ConfigurationMalformed {
                reason: "configuration is not a valid identity record",
            },
            IdentityError::UnsupportedVersion { version } => {
                Self::ConfigurationUnsupportedVersion { version }
            }
            IdentityError::Storage { operation } => Self::ConfigurationFailure { operation },
        }
    }
}

/// The raw configuration adapter result type exposed for adapter authors.
pub type IdentityStorageResult<T> = ConfigurationResult<T>;
