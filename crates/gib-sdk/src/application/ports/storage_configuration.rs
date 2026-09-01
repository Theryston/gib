use std::fmt;
use std::net::IpAddr;
use std::path::{Path, PathBuf};

/// The current version of the named-storage configuration schema.
pub const CURRENT_STORAGE_CONFIGURATION_VERSION: u16 = 1;

/// The current version of each backend settings schema.
pub const CURRENT_STORAGE_BACKEND_VERSION: u16 = 1;

/// The maximum encoded size of one named-storage configuration.
pub const MAX_STORAGE_CONFIGURATION_BYTES: usize = 16 * 1024;

/// The maximum UTF-8 length of a named storage.
pub const MAX_STORAGE_NAME_LENGTH: usize = 128;

/// The maximum length of one non-secret backend setting.
pub const MAX_STORAGE_SETTING_LENGTH: usize = 8 * 1024;

/// The maximum length of one credential value accepted by the credential port.
pub const MAX_STORAGE_CREDENTIAL_LENGTH: usize = 4 * 1024;

/// The filename suffix used for named-storage records.
pub const STORAGE_CONFIGURATION_FILE_SUFFIX: &str = ".msgpack";

/// A failure returned while accessing the approved encrypted credential store.
#[non_exhaustive]
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum CredentialStoreError {
    /// The referenced credential is absent.
    NotFound,
    /// The reference or credential value is invalid.
    Invalid,
    /// The credential store denied the operation.
    PermissionDenied,
    /// The credential store failed an I/O operation.
    Io,
    /// The credential store could not provide a consistent result.
    Unavailable,
}

impl fmt::Display for CredentialStoreError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        let message = match self {
            Self::NotFound => "credential was not found",
            Self::Invalid => "credential reference or value is invalid",
            Self::PermissionDenied => "credential store permission was denied",
            Self::Io => "credential store I/O operation failed",
            Self::Unavailable => "credential store is unavailable",
        };
        formatter.write_str(message)
    }
}

impl std::error::Error for CredentialStoreError {}

/// A result returned by a credential-store adapter.
pub type CredentialStoreResult<T> = Result<T, CredentialStoreError>;

/// Credential-store operations that can be injected for rollback tests.
#[non_exhaustive]
#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub enum CredentialStoreOperation {
    /// Storing a new credential.
    Store,
    /// Loading an existing credential.
    Load,
    /// Deleting an existing credential.
    Delete,
}

/// A validated opaque identifier held by the non-secret configuration file.
#[derive(Clone, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub struct CredentialReference(String);

impl CredentialReference {
    /// The maximum UTF-8 length of one opaque reference.
    pub const MAX_LENGTH: usize = 256;

    /// Creates a reference after rejecting path-like and control characters.
    pub fn new(value: impl Into<String>) -> CredentialStoreResult<Self> {
        let value = value.into();
        if value.is_empty()
            || value.len() > Self::MAX_LENGTH
            || value.chars().any(|character| character.is_control())
            || value.contains(['/', '\\', ':'])
            || value == "."
            || value == ".."
        {
            return Err(CredentialStoreError::Invalid);
        }
        Ok(Self(value))
    }

    /// Returns the opaque reference value.
    pub fn as_str(&self) -> &str {
        &self.0
    }

    /// Consumes the reference and returns its value.
    pub fn into_string(self) -> String {
        self.0
    }
}

impl fmt::Debug for CredentialReference {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("CredentialReference(<opaque>)")
    }
}

impl fmt::Display for CredentialReference {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(self.as_str())
    }
}

impl TryFrom<&str> for CredentialReference {
    type Error = CredentialStoreError;

    fn try_from(value: &str) -> CredentialStoreResult<Self> {
        Self::new(value)
    }
}

impl TryFrom<String> for CredentialReference {
    type Error = CredentialStoreError;

    fn try_from(value: String) -> CredentialStoreResult<Self> {
        Self::new(value)
    }
}

/// The credential kind expected by one remote backend.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
#[non_exhaustive]
pub enum StorageCredentialKind {
    /// S3 access, secret, and optional session credentials.
    S3,
    /// WebDAV Basic-auth credentials.
    WebDav,
}

/// S3 transport credentials held only in memory or by a secure credential
/// store.
#[derive(Clone, Eq, PartialEq)]
pub struct S3StorageCredentials {
    access_key: String,
    secret_key: String,
    session_token: Option<String>,
}

impl S3StorageCredentials {
    /// Creates long-lived S3 credentials.
    pub fn new(
        access_key: impl Into<String>,
        secret_key: impl Into<String>,
    ) -> CredentialStoreResult<Self> {
        Self::with_session_token(access_key, secret_key, None)
    }

    /// Creates S3 credentials with an optional temporary-session token.
    pub fn with_session_token(
        access_key: impl Into<String>,
        secret_key: impl Into<String>,
        session_token: Option<String>,
    ) -> CredentialStoreResult<Self> {
        let credentials = Self {
            access_key: access_key.into(),
            secret_key: secret_key.into(),
            session_token,
        };
        credentials.validate()?;
        Ok(credentials)
    }

    /// Returns the access key for constructing an authenticated backend.
    pub fn access_key(&self) -> &str {
        &self.access_key
    }

    /// Returns the secret key for constructing an authenticated backend.
    pub fn secret_key(&self) -> &str {
        &self.secret_key
    }

    /// Returns the optional temporary-session token.
    pub fn session_token(&self) -> Option<&str> {
        self.session_token.as_deref()
    }

    fn validate(&self) -> CredentialStoreResult<()> {
        validate_credential_value(&self.access_key)?;
        validate_credential_value(&self.secret_key)?;
        if let Some(session_token) = &self.session_token {
            validate_credential_value(session_token)?;
        }
        Ok(())
    }
}

impl fmt::Debug for S3StorageCredentials {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("S3StorageCredentials")
            .field("access_key", &"<redacted>")
            .field("secret_key", &"<redacted>")
            .field(
                "session_token",
                &self.session_token.as_ref().map(|_| "<redacted>"),
            )
            .finish()
    }
}

/// WebDAV Basic-auth credentials held only in memory or by a secure
/// credential store.
#[derive(Clone, Eq, PartialEq)]
pub struct WebDavStorageCredentials {
    username: String,
    password: String,
}

impl WebDavStorageCredentials {
    /// Creates WebDAV Basic-auth credentials.
    pub fn new(
        username: impl Into<String>,
        password: impl Into<String>,
    ) -> CredentialStoreResult<Self> {
        let credentials = Self {
            username: username.into(),
            password: password.into(),
        };
        credentials.validate()?;
        Ok(credentials)
    }

    /// Returns the username for constructing an authenticated backend.
    pub fn username(&self) -> &str {
        &self.username
    }

    /// Returns the password for constructing an authenticated backend.
    pub fn password(&self) -> &str {
        &self.password
    }

    fn validate(&self) -> CredentialStoreResult<()> {
        validate_credential_value(&self.username)?;
        validate_credential_value(&self.password)?;
        Ok(())
    }
}

impl fmt::Debug for WebDavStorageCredentials {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("WebDavStorageCredentials")
            .field("username", &"<redacted>")
            .field("password", &"<redacted>")
            .finish()
    }
}

/// Credentials required by a named remote storage.
#[derive(Clone, Eq, PartialEq)]
#[non_exhaustive]
pub enum StorageCredentials {
    /// S3 credentials.
    S3(S3StorageCredentials),
    /// WebDAV Basic-auth credentials.
    WebDav(WebDavStorageCredentials),
}

impl StorageCredentials {
    /// Creates S3 credentials.
    pub fn s3(
        access_key: impl Into<String>,
        secret_key: impl Into<String>,
    ) -> CredentialStoreResult<Self> {
        S3StorageCredentials::new(access_key, secret_key).map(Self::S3)
    }

    /// Creates S3 credentials with an optional session token.
    pub fn s3_with_session_token(
        access_key: impl Into<String>,
        secret_key: impl Into<String>,
        session_token: Option<String>,
    ) -> CredentialStoreResult<Self> {
        S3StorageCredentials::with_session_token(access_key, secret_key, session_token)
            .map(Self::S3)
    }

    /// Creates WebDAV Basic-auth credentials.
    pub fn webdav(
        username: impl Into<String>,
        password: impl Into<String>,
    ) -> CredentialStoreResult<Self> {
        WebDavStorageCredentials::new(username, password).map(Self::WebDav)
    }

    /// Returns the credential kind.
    pub const fn kind(&self) -> StorageCredentialKind {
        match self {
            Self::S3(_) => StorageCredentialKind::S3,
            Self::WebDav(_) => StorageCredentialKind::WebDav,
        }
    }

    /// Returns S3 credentials when this is an S3 credential.
    pub fn as_s3(&self) -> Option<&S3StorageCredentials> {
        match self {
            Self::S3(credentials) => Some(credentials),
            Self::WebDav(_) => None,
        }
    }

    /// Returns WebDAV credentials when this is a WebDAV credential.
    pub fn as_webdav(&self) -> Option<&WebDavStorageCredentials> {
        match self {
            Self::S3(_) => None,
            Self::WebDav(credentials) => Some(credentials),
        }
    }
}

impl fmt::Debug for StorageCredentials {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::S3(credentials) => formatter.debug_tuple("S3").field(credentials).finish(),
            Self::WebDav(credentials) => {
                formatter.debug_tuple("WebDav").field(credentials).finish()
            }
        }
    }
}

/// The approved encrypted credential-store boundary used by named-storage
/// persistence.
pub trait CredentialStore: Send + Sync {
    /// Stores credentials in a new entry and returns an opaque reference
    /// suitable for a non-secret configuration record.
    ///
    /// Implementations must not overwrite an existing reference. The
    /// configuration store relies on this property to roll back a failed
    /// publication without changing the credentials used by the old record.
    fn store(&self, credentials: &StorageCredentials)
    -> CredentialStoreResult<CredentialReference>;

    /// Loads credentials by opaque reference.
    fn load(&self, reference: &CredentialReference) -> CredentialStoreResult<StorageCredentials>;

    /// Removes credentials by opaque reference.
    fn delete(&self, reference: &CredentialReference) -> CredentialStoreResult<()>;
}

impl<T> CredentialStore for std::sync::Arc<T>
where
    T: CredentialStore + ?Sized,
{
    fn store(
        &self,
        credentials: &StorageCredentials,
    ) -> CredentialStoreResult<CredentialReference> {
        self.as_ref().store(credentials)
    }

    fn load(&self, reference: &CredentialReference) -> CredentialStoreResult<StorageCredentials> {
        self.as_ref().load(reference)
    }

    fn delete(&self, reference: &CredentialReference) -> CredentialStoreResult<()> {
        self.as_ref().delete(reference)
    }
}

impl<T> CredentialStore for &T
where
    T: CredentialStore + ?Sized,
{
    fn store(
        &self,
        credentials: &StorageCredentials,
    ) -> CredentialStoreResult<CredentialReference> {
        (*self).store(credentials)
    }

    fn load(&self, reference: &CredentialReference) -> CredentialStoreResult<StorageCredentials> {
        (*self).load(reference)
    }

    fn delete(&self, reference: &CredentialReference) -> CredentialStoreResult<()> {
        (*self).delete(reference)
    }
}

/// Local filesystem settings for a named storage.
#[derive(Clone, Eq, PartialEq)]
pub struct LocalStorageSettings {
    root: PathBuf,
}

impl LocalStorageSettings {
    /// Creates validated local-root settings.
    pub fn new(root: impl AsRef<Path>) -> StorageConfigurationResult<Self> {
        let root = root.as_ref().to_path_buf();
        let settings = Self { root };
        settings.validate()?;
        Ok(settings)
    }

    /// Returns the configured local root.
    pub fn root(&self) -> &Path {
        &self.root
    }

    pub(crate) fn validate(&self) -> StorageConfigurationResult<()> {
        if self.root.as_os_str().is_empty()
            || self.root.as_os_str().to_str().is_none()
            || self
                .root
                .components()
                .any(|component| component.as_os_str().to_string_lossy().contains('\0'))
        {
            return Err(StorageConfigurationError::InvalidConfiguration);
        }
        Ok(())
    }
}

impl fmt::Debug for LocalStorageSettings {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("LocalStorageSettings")
            .field("root", &self.root)
            .finish()
    }
}

/// Non-secret S3 settings for a named storage.
#[derive(Clone, Eq, PartialEq)]
pub struct S3StorageSettings {
    region: String,
    bucket: String,
    endpoint: Option<String>,
    force_path_style: bool,
    multipart_threshold: u64,
    multipart_part_size: u64,
    max_concurrency: usize,
    capability_cache_path: Option<PathBuf>,
}

impl S3StorageSettings {
    /// Creates S3 settings with the standard SDK transfer defaults.
    pub fn new(
        region: impl Into<String>,
        bucket: impl Into<String>,
    ) -> StorageConfigurationResult<Self> {
        let settings = Self {
            region: region.into(),
            bucket: bucket.into(),
            endpoint: None,
            force_path_style: false,
            multipart_threshold: 8 * 1024 * 1024,
            multipart_part_size: 8 * 1024 * 1024,
            max_concurrency: 4,
            capability_cache_path: None,
        };
        settings.validate()?;
        Ok(settings)
    }

    /// Sets an optional S3-compatible endpoint.
    pub fn with_endpoint(
        mut self,
        endpoint: impl Into<String>,
    ) -> StorageConfigurationResult<Self> {
        self.endpoint = Some(endpoint.into());
        self.validate()?;
        Ok(self)
    }

    /// Sets path-style endpoint addressing.
    pub const fn with_force_path_style(mut self, force_path_style: bool) -> Self {
        self.force_path_style = force_path_style;
        self
    }

    /// Sets the multipart threshold.
    pub const fn with_multipart_threshold(mut self, threshold: u64) -> Self {
        self.multipart_threshold = threshold;
        self
    }

    /// Sets the multipart part size.
    pub const fn with_multipart_part_size(mut self, part_size: u64) -> Self {
        self.multipart_part_size = part_size;
        self
    }

    /// Sets multipart request concurrency.
    pub const fn with_max_concurrency(mut self, max_concurrency: usize) -> Self {
        self.max_concurrency = max_concurrency;
        self
    }

    /// Sets the optional conditional-capability cache path.
    pub fn with_capability_cache_path(mut self, path: impl AsRef<Path>) -> Self {
        self.capability_cache_path = Some(path.as_ref().to_path_buf());
        self
    }

    /// Returns the configured S3 region.
    pub fn region(&self) -> &str {
        &self.region
    }

    /// Returns the configured bucket.
    pub fn bucket(&self) -> &str {
        &self.bucket
    }

    /// Returns the optional endpoint.
    pub fn endpoint(&self) -> Option<&str> {
        self.endpoint.as_deref()
    }

    /// Returns whether path-style endpoint addressing is enabled.
    pub const fn force_path_style(&self) -> bool {
        self.force_path_style
    }

    /// Returns the multipart threshold.
    pub const fn multipart_threshold(&self) -> u64 {
        self.multipart_threshold
    }

    /// Returns the multipart part size.
    pub const fn multipart_part_size(&self) -> u64 {
        self.multipart_part_size
    }

    /// Returns multipart request concurrency.
    pub const fn max_concurrency(&self) -> usize {
        self.max_concurrency
    }

    /// Returns the optional capability-cache path.
    pub fn capability_cache_path(&self) -> Option<&Path> {
        self.capability_cache_path.as_deref()
    }

    fn validate(&self) -> StorageConfigurationResult<()> {
        validate_setting(&self.region, 128, true)?;
        validate_bucket(&self.bucket)?;
        if let Some(endpoint) = &self.endpoint {
            validate_endpoint(endpoint)?;
        }
        if self.multipart_threshold == 0 || self.multipart_threshold > 64 * 1024 * 1024 {
            return Err(StorageConfigurationError::InvalidConfiguration);
        }
        if !(5 * 1024 * 1024..=64 * 1024 * 1024).contains(&self.multipart_part_size)
            || !(1..=64).contains(&self.max_concurrency)
        {
            return Err(StorageConfigurationError::InvalidConfiguration);
        }
        if self
            .capability_cache_path
            .as_ref()
            .is_some_and(|path| path.as_os_str().is_empty())
        {
            return Err(StorageConfigurationError::InvalidConfiguration);
        }
        Ok(())
    }
}

impl fmt::Debug for S3StorageSettings {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("S3StorageSettings")
            .field("region", &self.region)
            .field("bucket", &self.bucket)
            .field("endpoint", &self.endpoint.as_ref().map(|_| "<redacted>"))
            .field("force_path_style", &self.force_path_style)
            .field("multipart_threshold", &self.multipart_threshold)
            .field("multipart_part_size", &self.multipart_part_size)
            .field("max_concurrency", &self.max_concurrency)
            .field(
                "capability_cache_path",
                &self.capability_cache_path.as_ref().map(|_| "<configured>"),
            )
            .finish()
    }
}

/// Non-secret WebDAV settings for a named storage.
#[derive(Clone, Eq, PartialEq)]
pub struct WebDavStorageSettings {
    collection_url: String,
    allow_insecure_http: bool,
    max_concurrency: usize,
}

impl WebDavStorageSettings {
    /// Creates WebDAV settings with HTTPS required by default.
    pub fn new(collection_url: impl Into<String>) -> StorageConfigurationResult<Self> {
        let settings = Self {
            collection_url: normalize_webdav_url(&collection_url.into())?,
            allow_insecure_http: false,
            max_concurrency: 8,
        };
        settings.validate_syntax()?;
        Ok(settings)
    }

    /// Explicitly permits an insecure HTTP endpoint.
    pub const fn with_allow_insecure_http(mut self, allow: bool) -> Self {
        self.allow_insecure_http = allow;
        self
    }

    /// Sets WebDAV request concurrency.
    pub const fn with_max_concurrency(mut self, max_concurrency: usize) -> Self {
        self.max_concurrency = max_concurrency;
        self
    }

    /// Returns the normalized collection URL.
    pub fn collection_url(&self) -> &str {
        &self.collection_url
    }

    /// Returns whether insecure HTTP is explicitly permitted.
    pub const fn allow_insecure_http(&self) -> bool {
        self.allow_insecure_http
    }

    /// Returns WebDAV request concurrency.
    pub const fn max_concurrency(&self) -> usize {
        self.max_concurrency
    }

    fn validate(&self) -> StorageConfigurationResult<()> {
        self.validate_syntax()?;
        let url = url::Url::parse(&self.collection_url)
            .map_err(|_| StorageConfigurationError::InvalidConfiguration)?;
        if url.scheme() == "http" && !self.allow_insecure_http {
            return Err(StorageConfigurationError::InvalidConfiguration);
        }
        Ok(())
    }

    fn validate_syntax(&self) -> StorageConfigurationResult<()> {
        validate_webdav_url(&self.collection_url)?;
        if !(1..=64).contains(&self.max_concurrency) {
            return Err(StorageConfigurationError::InvalidConfiguration);
        }
        Ok(())
    }
}

impl fmt::Debug for WebDavStorageSettings {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("WebDavStorageSettings")
            .field("collection_url", &self.collection_url)
            .field("allow_insecure_http", &self.allow_insecure_http)
            .field("max_concurrency", &self.max_concurrency)
            .finish()
    }
}

/// A typed non-secret backend definition.
#[derive(Clone, Eq, PartialEq)]
#[non_exhaustive]
pub enum StorageBackend {
    /// A filesystem-backed repository root.
    Local(LocalStorageSettings),
    /// An S3 or S3-compatible bucket.
    S3(S3StorageSettings),
    /// A WebDAV collection.
    WebDav(WebDavStorageSettings),
}

impl StorageBackend {
    /// Returns the backend kind as a stable string.
    pub const fn kind(&self) -> StorageBackendKind {
        match self {
            Self::Local(_) => StorageBackendKind::Local,
            Self::S3(_) => StorageBackendKind::S3,
            Self::WebDav(_) => StorageBackendKind::WebDav,
        }
    }

    /// Returns the local settings, when this is a local backend.
    pub fn as_local(&self) -> Option<&LocalStorageSettings> {
        match self {
            Self::Local(settings) => Some(settings),
            Self::S3(_) | Self::WebDav(_) => None,
        }
    }

    /// Returns the S3 settings, when this is an S3 backend.
    pub fn as_s3(&self) -> Option<&S3StorageSettings> {
        match self {
            Self::S3(settings) => Some(settings),
            Self::Local(_) | Self::WebDav(_) => None,
        }
    }

    /// Returns the WebDAV settings, when this is a WebDAV backend.
    pub fn as_webdav(&self) -> Option<&WebDavStorageSettings> {
        match self {
            Self::WebDav(settings) => Some(settings),
            Self::Local(_) | Self::S3(_) => None,
        }
    }

    pub(crate) fn validate(&self) -> StorageConfigurationResult<()> {
        match self {
            Self::Local(settings) => settings.validate(),
            Self::S3(settings) => settings.validate(),
            Self::WebDav(settings) => settings.validate(),
        }
    }
}

/// The backend kind used by a persisted definition.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
#[non_exhaustive]
pub enum StorageBackendKind {
    /// Filesystem backend.
    Local,
    /// S3 backend.
    S3,
    /// WebDAV backend.
    WebDav,
}

impl fmt::Display for StorageBackendKind {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        let value = match self {
            Self::Local => "local",
            Self::S3 => "s3",
            Self::WebDav => "webdav",
        };
        formatter.write_str(value)
    }
}

impl fmt::Debug for StorageBackend {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Local(settings) => formatter.debug_tuple("Local").field(settings).finish(),
            Self::S3(settings) => formatter.debug_tuple("S3").field(settings).finish(),
            Self::WebDav(settings) => formatter.debug_tuple("WebDav").field(settings).finish(),
        }
    }
}

/// A validated name used to select one persisted storage record.
#[derive(Clone, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub struct StorageName(String);

impl StorageName {
    /// Creates a portable single-component storage name.
    pub fn new(value: impl Into<String>) -> StorageConfigurationResult<Self> {
        let value = value.into();
        validate_storage_name(&value).map(|()| Self(value))
    }

    /// Returns the validated storage name.
    pub fn as_str(&self) -> &str {
        &self.0
    }

    /// Consumes the name and returns its string value.
    pub fn into_string(self) -> String {
        self.0
    }
}

impl fmt::Display for StorageName {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(self.as_str())
    }
}

/// A complete in-memory named-storage definition.
#[derive(Clone, Eq, PartialEq)]
pub struct StorageConfiguration {
    backend: StorageBackend,
    credentials: Option<StorageCredentials>,
    credential_reference: Option<CredentialReference>,
}

impl StorageConfiguration {
    /// Creates a configuration and validates the backend/credential pairing.
    pub fn new(
        backend: StorageBackend,
        credentials: Option<StorageCredentials>,
    ) -> StorageConfigurationResult<Self> {
        backend.validate()?;
        validate_backend_credentials(&backend, credentials.as_ref())?;
        Ok(Self {
            backend,
            credentials,
            credential_reference: None,
        })
    }

    /// Creates a local storage configuration.
    pub fn local(root: impl AsRef<Path>) -> StorageConfigurationResult<Self> {
        Self::new(
            StorageBackend::Local(LocalStorageSettings::new(root)?),
            None,
        )
    }

    /// Creates an S3 storage configuration.
    pub fn s3(
        settings: S3StorageSettings,
        credentials: S3StorageCredentials,
    ) -> StorageConfigurationResult<Self> {
        Self::new(
            StorageBackend::S3(settings),
            Some(StorageCredentials::S3(credentials)),
        )
    }

    /// Creates a WebDAV storage configuration.
    pub fn webdav(
        settings: WebDavStorageSettings,
        credentials: WebDavStorageCredentials,
    ) -> StorageConfigurationResult<Self> {
        Self::new(
            StorageBackend::WebDav(settings),
            Some(StorageCredentials::WebDav(credentials)),
        )
    }

    /// Returns the non-secret backend definition.
    pub fn backend(&self) -> &StorageBackend {
        &self.backend
    }

    /// Returns the in-memory credentials, when configured.
    pub fn credentials(&self) -> Option<&StorageCredentials> {
        self.credentials.as_ref()
    }

    /// Returns the opaque reference used by the persisted record, when this
    /// configuration was loaded from a store.
    pub fn credential_reference(&self) -> Option<&CredentialReference> {
        self.credential_reference.as_ref()
    }

    pub(crate) fn with_loaded_credential_reference(
        mut self,
        reference: Option<CredentialReference>,
    ) -> Self {
        self.credential_reference = reference;
        self
    }
}

impl fmt::Debug for StorageConfiguration {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("StorageConfiguration")
            .field("backend", &self.backend)
            .field(
                "credentials",
                &self.credentials.as_ref().map(|_| "<redacted>"),
            )
            .field("credential_reference", &self.credential_reference)
            .finish()
    }
}

/// The result of a storage connectivity check.
#[non_exhaustive]
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub enum StorageHealth {
    /// The configured backend accepted a read-only validation request.
    Healthy,
    /// Connectivity was not requested for this result.
    NotChecked,
}

impl StorageHealth {
    /// Returns whether the backend was checked successfully.
    pub const fn is_healthy(self) -> bool {
        matches!(self, Self::Healthy)
    }
}

impl fmt::Display for StorageHealth {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(match self {
            Self::Healthy => "healthy",
            Self::NotChecked => "not_checked",
        })
    }
}

/// A validated request to add or explicitly replace a named storage.
#[derive(Clone, Eq, PartialEq)]
pub struct StorageAddRequest {
    name: StorageName,
    configuration: StorageConfiguration,
    replace_existing: bool,
}

impl StorageAddRequest {
    /// Creates a request from a validated name and configuration.
    pub fn new(
        name: impl Into<String>,
        configuration: StorageConfiguration,
    ) -> StorageConfigurationResult<Self> {
        Ok(Self {
            name: StorageName::new(name)?,
            configuration,
            replace_existing: false,
        })
    }

    /// Creates a validated local-storage request.
    pub fn local(
        name: impl Into<String>,
        root: impl AsRef<Path>,
    ) -> StorageConfigurationResult<Self> {
        Self::new(name, StorageConfiguration::local(root)?)
    }

    /// Creates a validated S3-storage request.
    pub fn s3(
        name: impl Into<String>,
        settings: S3StorageSettings,
        credentials: S3StorageCredentials,
    ) -> StorageConfigurationResult<Self> {
        Self::new(name, StorageConfiguration::s3(settings, credentials)?)
    }

    /// Creates a validated WebDAV-storage request.
    pub fn webdav(
        name: impl Into<String>,
        settings: WebDavStorageSettings,
        credentials: WebDavStorageCredentials,
    ) -> StorageConfigurationResult<Self> {
        Self::new(name, StorageConfiguration::webdav(settings, credentials)?)
    }

    /// Creates a request using the explicit `for_*` naming convention.
    pub fn for_local(
        name: impl Into<String>,
        root: impl AsRef<Path>,
    ) -> StorageConfigurationResult<Self> {
        Self::local(name, root)
    }

    /// Creates a request using the explicit `for_*` naming convention.
    pub fn for_s3(
        name: impl Into<String>,
        settings: S3StorageSettings,
        credentials: S3StorageCredentials,
    ) -> StorageConfigurationResult<Self> {
        Self::s3(name, settings, credentials)
    }

    /// Creates a request using the explicit `for_*` naming convention.
    pub fn for_webdav(
        name: impl Into<String>,
        settings: WebDavStorageSettings,
        credentials: WebDavStorageCredentials,
    ) -> StorageConfigurationResult<Self> {
        Self::webdav(name, settings, credentials)
    }

    /// Requires an existing name to be replaced instead of rejected.
    pub const fn with_replacement(mut self, replace_existing: bool) -> Self {
        self.replace_existing = replace_existing;
        self
    }

    /// Marks this request as an explicit replacement.
    pub const fn replace_existing(mut self) -> Self {
        self.replace_existing = true;
        self
    }

    /// Returns the validated storage name.
    pub const fn name(&self) -> &StorageName {
        &self.name
    }

    /// Returns the validated backend configuration.
    pub const fn configuration(&self) -> &StorageConfiguration {
        &self.configuration
    }

    /// Returns whether an existing name may be replaced.
    pub const fn replaces_existing(&self) -> bool {
        self.replace_existing
    }

    /// Consumes the request into its validated parts.
    pub fn into_parts(self) -> (StorageName, StorageConfiguration, bool) {
        (self.name, self.configuration, self.replace_existing)
    }
}

impl fmt::Debug for StorageAddRequest {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("StorageAddRequest")
            .field("name", &self.name)
            .field("configuration", &self.configuration)
            .field("replace_existing", &self.replace_existing)
            .finish()
    }
}

/// Options for listing named storage configurations.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct StorageConfigurationListRequest {
    check_health: bool,
}

impl StorageConfigurationListRequest {
    /// Creates a metadata-only list request.
    pub const fn new() -> Self {
        Self {
            check_health: false,
        }
    }

    /// Requests a read-only connectivity check for every listed storage.
    pub const fn with_health_check(mut self, check_health: bool) -> Self {
        self.check_health = check_health;
        self
    }

    /// Requests a read-only connectivity check for every listed storage.
    pub const fn check_health(self) -> Self {
        self.with_health_check(true)
    }

    /// Returns whether health checks were requested.
    pub const fn checks_health(self) -> bool {
        self.check_health
    }
}

/// A validated request to remove one named storage configuration.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct StorageRemoveRequest {
    name: StorageName,
}

impl StorageRemoveRequest {
    /// Creates a request from a storage name.
    pub fn new(name: impl Into<String>) -> StorageConfigurationResult<Self> {
        Ok(Self {
            name: StorageName::new(name)?,
        })
    }

    /// Returns the validated storage name.
    pub const fn name(&self) -> &StorageName {
        &self.name
    }

    /// Consumes the request and returns its name.
    pub fn into_name(self) -> StorageName {
        self.name
    }
}

/// Non-secret metadata for one configured storage.
#[derive(Clone, Eq, PartialEq)]
pub struct StorageConfigurationMetadata {
    name: StorageName,
    backend: StorageBackend,
    credentials_configured: bool,
    health: StorageHealth,
}

impl StorageConfigurationMetadata {
    pub(crate) fn new(
        name: StorageName,
        backend: StorageBackend,
        credentials_configured: bool,
        health: StorageHealth,
    ) -> Self {
        Self {
            name,
            backend,
            credentials_configured,
            health,
        }
    }

    /// Returns the storage name.
    pub const fn name(&self) -> &StorageName {
        &self.name
    }

    /// Returns the non-secret backend settings.
    pub const fn backend(&self) -> &StorageBackend {
        &self.backend
    }

    /// Returns whether credentials are configured without exposing them.
    pub const fn credentials_configured(&self) -> bool {
        self.credentials_configured
    }

    /// Returns the health state represented by this metadata.
    pub const fn health(&self) -> StorageHealth {
        self.health
    }

    pub(crate) fn with_health(mut self, health: StorageHealth) -> Self {
        self.health = health;
        self
    }
}

impl fmt::Debug for StorageConfigurationMetadata {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("StorageConfigurationMetadata")
            .field("name", &self.name)
            .field("backend", &self.backend)
            .field("credentials_configured", &self.credentials_configured)
            .field("health", &self.health)
            .finish()
    }
}

/// Result returned after adding or replacing a storage.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct StorageAddResult {
    metadata: StorageConfigurationMetadata,
    replaced_existing: bool,
}

impl StorageAddResult {
    pub(crate) fn new(metadata: StorageConfigurationMetadata, replaced_existing: bool) -> Self {
        Self {
            metadata,
            replaced_existing,
        }
    }

    /// Returns the safe metadata for the stored configuration.
    pub const fn metadata(&self) -> &StorageConfigurationMetadata {
        &self.metadata
    }

    /// Returns whether the operation replaced a previous configuration.
    pub const fn replaced_existing(&self) -> bool {
        self.replaced_existing
    }

    /// Returns the storage name.
    pub const fn name(&self) -> &StorageName {
        self.metadata.name()
    }

    /// Returns the connectivity result used before publication.
    pub const fn health(&self) -> StorageHealth {
        self.metadata.health()
    }
}

/// Result returned by a named-storage list operation.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct StorageListResult {
    storages: Vec<StorageConfigurationMetadata>,
}

impl StorageListResult {
    pub(crate) const fn new(storages: Vec<StorageConfigurationMetadata>) -> Self {
        Self { storages }
    }

    /// Returns storages in deterministic name order.
    pub fn storages(&self) -> &[StorageConfigurationMetadata] {
        &self.storages
    }

    /// Returns whether no named storages were found.
    pub fn is_empty(&self) -> bool {
        self.storages.is_empty()
    }

    /// Consumes the result and returns the safe metadata entries.
    pub fn into_storages(self) -> Vec<StorageConfigurationMetadata> {
        self.storages
    }
}

/// Result returned after removing a named storage configuration.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct StorageRemoveResult {
    name: StorageName,
    backend: StorageBackendKind,
    credentials_removed: bool,
}

impl StorageRemoveResult {
    pub(crate) const fn new(
        name: StorageName,
        backend: StorageBackendKind,
        credentials_removed: bool,
    ) -> Self {
        Self {
            name,
            backend,
            credentials_removed,
        }
    }

    /// Returns the removed storage name.
    pub const fn name(&self) -> &StorageName {
        &self.name
    }

    /// Returns the removed backend kind.
    pub const fn backend(&self) -> StorageBackendKind {
        self.backend
    }

    /// Returns whether the referenced credential entry was removed.
    pub const fn credentials_removed(&self) -> bool {
        self.credentials_removed
    }

    /// Always returns `true`; this operation never removes repository data.
    pub const fn repository_data_preserved(&self) -> bool {
        true
    }
}

/// Compatibility name for [`StorageConfigurationMetadata`].
pub type StorageInfo = StorageConfigurationMetadata;

/// Compatibility name for [`StorageConfigurationMetadata`].
pub type StorageEntry = StorageConfigurationMetadata;

/// Compatibility name for [`StorageAddRequest`].
pub type AddStorageRequest = StorageAddRequest;

/// Compatibility name for [`StorageRemoveRequest`].
pub type RemoveStorageRequest = StorageRemoveRequest;

/// Compatibility name for [`StorageConfigurationListRequest`].
pub type ListStorageRequest = StorageConfigurationListRequest;

/// Connectivity boundary used by storage-management operations.
pub trait StorageConnectivity: Send + Sync {
    /// Validates a configured backend without publishing repository data.
    fn check(
        &self,
        configuration: &StorageConfiguration,
    ) -> StorageConfigurationResult<StorageHealth>;
}

impl<T> StorageConnectivity for std::sync::Arc<T>
where
    T: StorageConnectivity + ?Sized,
{
    fn check(
        &self,
        configuration: &StorageConfiguration,
    ) -> StorageConfigurationResult<StorageHealth> {
        self.as_ref().check(configuration)
    }
}

impl<T> StorageConnectivity for &T
where
    T: StorageConnectivity + ?Sized,
{
    fn check(
        &self,
        configuration: &StorageConfiguration,
    ) -> StorageConfigurationResult<StorageHealth> {
        (*self).check(configuration)
    }
}

/// Compatibility name for [`StorageConnectivity`].
pub trait StorageProbe: StorageConnectivity {}

impl<T> StorageProbe for T where T: StorageConnectivity + ?Sized {}

/// Persistence boundary used by the storage-management use case.
pub trait StorageConfigurationRepository: Send + Sync {
    /// Returns whether one validated name has a record.
    fn contains(&self, name: &StorageName) -> StorageConfigurationResult<bool>;

    /// Publishes a new record and rejects an existing name.
    fn save_new(
        &self,
        name: &StorageName,
        configuration: StorageConfiguration,
    ) -> StorageConfigurationResult<()>;

    /// Publishes a new record or replaces an existing record, returning whether
    /// a previous record was replaced.
    fn save_replacement(
        &self,
        name: &StorageName,
        configuration: StorageConfiguration,
    ) -> StorageConfigurationResult<bool>;

    /// Returns safe metadata without resolving credentials.
    fn describe(
        &self,
        name: &StorageName,
    ) -> StorageConfigurationResult<StorageConfigurationMetadata>;

    /// Lists safe metadata in deterministic order.
    fn list_metadata(&self) -> StorageConfigurationResult<Vec<StorageConfigurationMetadata>>;

    /// Resolves a configuration and its credentials for a requested health check.
    fn load(&self, name: &StorageName) -> StorageConfigurationResult<StorageConfiguration>;

    /// Removes the record and its credential reference, never backend contents.
    fn remove(&self, name: &StorageName) -> StorageConfigurationResult<()>;
}

/// Failures returned by named-storage configuration persistence.
#[non_exhaustive]
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum StorageConfigurationError {
    /// The named configuration does not exist.
    NotFound,
    /// A named configuration already exists and replacement was not requested.
    AlreadyExists,
    /// The storage name is unsafe or invalid.
    InvalidName,
    /// Backend settings or a backend/credential pairing is invalid.
    InvalidConfiguration,
    /// A credential reference is absent from the approved credential store.
    MissingCredentialReference,
    /// The credential store rejected an operation without exposing its secret.
    CredentialStoreFailure,
    /// The persisted record is malformed or contains an invalid reference.
    Malformed,
    /// The persisted schema version is newer than this SDK supports.
    UnsupportedSchemaVersion {
        /// The unsupported schema version.
        version: u16,
    },
    /// The persisted backend kind is not recognized by this SDK.
    UnsupportedBackend {
        /// The backend tag found in the record.
        kind: String,
    },
    /// The persisted backend settings version is newer than this SDK supports.
    UnsupportedBackendVersion {
        /// The backend tag whose version was rejected.
        kind: String,
        /// The unsupported backend settings version.
        version: u16,
    },
    /// The configuration directory or record path is unsafe.
    InvalidPath,
    /// The persisted record exceeds the bounded input size.
    TooLarge,
    /// A filesystem operation failed.
    Io,
    /// The store could not provide a consistent result.
    Unavailable,
    /// A read-only backend connectivity check failed.
    ConnectivityFailure {
        /// The backend whose connectivity was checked.
        backend: StorageBackendKind,
        /// The provider-neutral failure returned by the adapter.
        error: crate::application::ports::StorageError,
    },
}

impl fmt::Display for StorageConfigurationError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::NotFound => formatter.write_str("storage configuration was not found"),
            Self::AlreadyExists => formatter
                .write_str("storage configuration already exists; request explicit replacement"),
            Self::InvalidName => formatter.write_str("storage configuration name is invalid"),
            Self::InvalidConfiguration => {
                formatter.write_str("storage backend configuration is invalid")
            }
            Self::MissingCredentialReference => {
                formatter.write_str("storage credential reference is missing")
            }
            Self::CredentialStoreFailure => {
                formatter.write_str("storage credential store operation failed")
            }
            Self::Malformed => formatter.write_str("storage configuration is malformed"),
            Self::UnsupportedSchemaVersion { version } => write!(
                formatter,
                "storage configuration schema version {version} is unsupported"
            ),
            Self::UnsupportedBackend { kind } => {
                write!(formatter, "storage backend '{kind}' is unsupported")
            }
            Self::UnsupportedBackendVersion { kind, version } => write!(
                formatter,
                "storage backend '{kind}' settings version {version} is unsupported"
            ),
            Self::InvalidPath => formatter.write_str("storage configuration path is invalid"),
            Self::TooLarge => formatter.write_str("storage configuration is too large"),
            Self::Io => formatter.write_str("storage configuration I/O operation failed"),
            Self::Unavailable => formatter.write_str("storage configuration is unavailable"),
            Self::ConnectivityFailure { backend, error } => write!(
                formatter,
                "{backend} storage connectivity check failed: {error}"
            ),
        }
    }
}

impl StorageConfigurationError {
    /// Returns the stable machine-readable category for this error.
    pub const fn code(&self) -> &'static str {
        match self {
            Self::NotFound => "storage_not_found",
            Self::AlreadyExists => "storage_already_exists",
            Self::InvalidName => "storage_invalid_name",
            Self::InvalidConfiguration => "storage_invalid_configuration",
            Self::MissingCredentialReference => "storage_missing_credentials",
            Self::CredentialStoreFailure => "storage_credential_store_failure",
            Self::Malformed => "storage_configuration_malformed",
            Self::UnsupportedSchemaVersion { .. } => "storage_configuration_unsupported_version",
            Self::UnsupportedBackend { .. } => "storage_backend_unsupported",
            Self::UnsupportedBackendVersion { .. } => "storage_backend_unsupported_version",
            Self::InvalidPath => "storage_invalid_path",
            Self::TooLarge => "storage_configuration_too_large",
            Self::Io => "storage_configuration_io",
            Self::Unavailable => "storage_configuration_unavailable",
            Self::ConnectivityFailure { .. } => "storage_connectivity_failure",
        }
    }

    /// Returns whether the operation requires explicit replacement consent.
    pub const fn is_conflict(&self) -> bool {
        matches!(self, Self::AlreadyExists)
    }

    /// Returns whether the error represents invalid caller input.
    pub const fn is_input_error(&self) -> bool {
        matches!(self, Self::InvalidName | Self::InvalidConfiguration)
    }
}

impl std::error::Error for StorageConfigurationError {}

/// Result returned by named-storage configuration stores.
pub type StorageConfigurationResult<T> = Result<T, StorageConfigurationError>;

/// Filesystem operations that can be injected for atomic persistence tests.
#[non_exhaustive]
#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub enum StorageConfigurationOperation {
    /// Failure before staging bytes.
    Write,
    /// Failure while flushing staged bytes.
    Flush,
    /// Failure while replacing the destination.
    Rename,
    /// Failure while synchronizing the configuration directory.
    DirectorySync,
    /// Failure while removing a configuration record.
    Remove,
}

fn validate_credential_value(value: &str) -> CredentialStoreResult<()> {
    if value.is_empty()
        || value.len() > MAX_STORAGE_CREDENTIAL_LENGTH
        || value.bytes().any(|byte| byte.is_ascii_control())
    {
        return Err(CredentialStoreError::Invalid);
    }
    Ok(())
}

fn validate_setting(
    value: &str,
    max_length: usize,
    reject_whitespace: bool,
) -> StorageConfigurationResult<()> {
    if value.is_empty()
        || value.len() > max_length
        || value.chars().any(|character| {
            character.is_control() || (reject_whitespace && character.is_whitespace())
        })
    {
        return Err(StorageConfigurationError::InvalidConfiguration);
    }
    Ok(())
}

fn validate_bucket(bucket: &str) -> StorageConfigurationResult<()> {
    if bucket.len() < 3
        || bucket.len() > 63
        || bucket.parse::<IpAddr>().is_ok()
        || !bucket.bytes().all(|byte| {
            byte.is_ascii_lowercase() || byte.is_ascii_digit() || byte == b'.' || byte == b'-'
        })
        || !bucket
            .as_bytes()
            .first()
            .is_some_and(u8::is_ascii_alphanumeric)
        || !bucket
            .as_bytes()
            .last()
            .is_some_and(u8::is_ascii_alphanumeric)
        || bucket.contains("..")
        || bucket.contains(".-")
        || bucket.contains("-.")
    {
        return Err(StorageConfigurationError::InvalidConfiguration);
    }
    Ok(())
}

fn validate_endpoint(endpoint: &str) -> StorageConfigurationResult<()> {
    if endpoint.is_empty() || endpoint.len() > 2_048 {
        return Err(StorageConfigurationError::InvalidConfiguration);
    }
    let url =
        url::Url::parse(endpoint).map_err(|_| StorageConfigurationError::InvalidConfiguration)?;
    if !matches!(url.scheme(), "http" | "https")
        || url.host_str().is_none()
        || !url.username().is_empty()
        || url.password().is_some()
        || url.query().is_some()
        || url.fragment().is_some()
    {
        return Err(StorageConfigurationError::InvalidConfiguration);
    }
    Ok(())
}

fn validate_webdav_url(value: &str) -> StorageConfigurationResult<()> {
    validate_setting(value, MAX_STORAGE_SETTING_LENGTH, false)?;
    let url =
        url::Url::parse(value).map_err(|_| StorageConfigurationError::InvalidConfiguration)?;
    if !matches!(url.scheme(), "http" | "https")
        || url.host_str().is_none()
        || !url.username().is_empty()
        || url.password().is_some()
        || url.query().is_some()
        || url.fragment().is_some()
    {
        return Err(StorageConfigurationError::InvalidConfiguration);
    }
    Ok(())
}

fn normalize_webdav_url(value: &str) -> StorageConfigurationResult<String> {
    let value = value.trim();
    validate_webdav_url(value)?;
    let mut url =
        url::Url::parse(value).map_err(|_| StorageConfigurationError::InvalidConfiguration)?;
    let path = url.path().to_owned();
    if path.is_empty() {
        let mut segments = url
            .path_segments_mut()
            .map_err(|_| StorageConfigurationError::InvalidConfiguration)?;
        segments.push("");
    } else if path != "/" && !path.ends_with('/') {
        let mut segments = url
            .path_segments_mut()
            .map_err(|_| StorageConfigurationError::InvalidConfiguration)?;
        segments.push("");
    }
    Ok(url.into())
}

fn validate_storage_name(value: &str) -> StorageConfigurationResult<()> {
    if value.is_empty()
        || value.len() > MAX_STORAGE_NAME_LENGTH
        || value == "."
        || value == ".."
        || value.ends_with('.')
        || value.ends_with(' ')
        || value.chars().any(|character| {
            character.is_control()
                || matches!(
                    character,
                    '/' | '\\' | ':' | '*' | '?' | '"' | '<' | '>' | '|'
                )
        })
    {
        return Err(StorageConfigurationError::InvalidName);
    }
    let stem = value
        .split('.')
        .next()
        .map_or_else(String::new, str::to_ascii_uppercase);
    if matches!(
        stem.as_str(),
        "CON"
            | "PRN"
            | "AUX"
            | "NUL"
            | "COM1"
            | "COM2"
            | "COM3"
            | "COM4"
            | "COM5"
            | "COM6"
            | "COM7"
            | "COM8"
            | "COM9"
            | "LPT1"
            | "LPT2"
            | "LPT3"
            | "LPT4"
            | "LPT5"
            | "LPT6"
            | "LPT7"
            | "LPT8"
            | "LPT9"
    ) {
        return Err(StorageConfigurationError::InvalidName);
    }
    Ok(())
}

fn validate_backend_credentials(
    backend: &StorageBackend,
    credentials: Option<&StorageCredentials>,
) -> StorageConfigurationResult<()> {
    match (backend, credentials) {
        (StorageBackend::Local(_), None) => Ok(()),
        (StorageBackend::Local(_), Some(_)) => Err(StorageConfigurationError::InvalidConfiguration),
        (StorageBackend::S3(_), Some(StorageCredentials::S3(_)))
        | (StorageBackend::WebDav(_), Some(StorageCredentials::WebDav(_))) => Ok(()),
        (StorageBackend::S3(_) | StorageBackend::WebDav(_), None) => {
            Err(StorageConfigurationError::MissingCredentialReference)
        }
        (StorageBackend::S3(_) | StorageBackend::WebDav(_), Some(_)) => {
            Err(StorageConfigurationError::InvalidConfiguration)
        }
    }
}

impl TryFrom<&str> for StorageName {
    type Error = StorageConfigurationError;

    fn try_from(value: &str) -> StorageConfigurationResult<Self> {
        Self::new(value)
    }
}

impl TryFrom<String> for StorageName {
    type Error = StorageConfigurationError;

    fn try_from(value: String) -> StorageConfigurationResult<Self> {
        Self::new(value)
    }
}
