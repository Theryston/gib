use super::error::{SdkError, SdkResult};
use crate::application::repository::{
    RepositoryError, RepositoryOpenExpectations, initialize_repository as initialize_use_case,
    open_repository as open_use_case,
};
use crate::domain::DomainError;
use std::fmt;
use std::sync::Arc;

pub use crate::application::ports::{RepositoryStorage, StorageError, StorageResult};
pub use crate::domain::{
    CURRENT_REPOSITORY_BOOTSTRAP_VERSION, CURRENT_REPOSITORY_DESCRIPTOR_VERSION,
    CURRENT_REPOSITORY_FORMAT_VERSION, FORMAT_OBJECT_KEY, REPOSITORY_DESCRIPTOR_OBJECT_KEY,
    REPOSITORY_MAGIC, REQUIRED_REPOSITORY_FEATURE,
};
pub use crate::domain::{
    RepositoryDescriptor, RepositoryFeature, RepositoryId, RepositoryIdentity, RepositoryKey,
    RepositoryObject, RepositoryRoots,
};
pub use crate::infrastructure::storage::{LocalStorage, MemoryStorage};

/// A cloneable type-erased handle for a repository storage backend.
#[derive(Clone)]
pub struct StorageHandle {
    inner: Arc<dyn RepositoryStorage>,
}

impl StorageHandle {
    /// Wraps a thread-safe storage backend in a shareable handle.
    pub fn new<S>(storage: S) -> Self
    where
        S: RepositoryStorage + 'static,
    {
        Self {
            inner: Arc::new(storage),
        }
    }

    /// Wraps an existing type-erased storage backend.
    pub fn from_arc(storage: Arc<dyn RepositoryStorage>) -> Self {
        Self { inner: storage }
    }

    /// Returns the backend object used by the repository handle.
    pub fn as_storage(&self) -> &dyn RepositoryStorage {
        self.inner.as_ref()
    }
}

impl<S> From<S> for StorageHandle
where
    S: RepositoryStorage + 'static,
{
    fn from(storage: S) -> Self {
        Self::new(storage)
    }
}

impl From<&MemoryStorage> for StorageHandle {
    fn from(storage: &MemoryStorage) -> Self {
        Self::new(storage.clone())
    }
}

impl From<&LocalStorage> for StorageHandle {
    fn from(storage: &LocalStorage) -> Self {
        Self::new(storage.clone())
    }
}

impl fmt::Debug for StorageHandle {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("StorageHandle(..)")
    }
}

/// Validated input for repository initialization.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct RepositoryInitRequest {
    identity: RepositoryIdentity,
    repository_key: RepositoryKey,
}

impl RepositoryInitRequest {
    /// Creates an initialization request from validated domain values.
    pub const fn new(identity: RepositoryIdentity, repository_key: RepositoryKey) -> Self {
        Self {
            identity,
            repository_key,
        }
    }

    /// Creates an initialization request by validating string values.
    pub fn from_values(
        identity: impl Into<String>,
        repository_key: impl Into<String>,
    ) -> SdkResult<Self> {
        Ok(Self::new(
            RepositoryIdentity::new(identity)?,
            RepositoryKey::new(repository_key)?,
        ))
    }

    /// Returns the identity to persist in the new repository.
    pub const fn identity(&self) -> &RepositoryIdentity {
        &self.identity
    }

    /// Alias for [`Self::identity`] using the repository-ID terminology.
    pub const fn repository_id(&self) -> &RepositoryIdentity {
        self.identity()
    }

    /// Returns the namespace key to persist in the new repository.
    pub const fn repository_key(&self) -> &RepositoryKey {
        &self.repository_key
    }

    /// Consumes the request and returns its validated values.
    pub fn into_parts(self) -> (RepositoryIdentity, RepositoryKey) {
        (self.identity, self.repository_key)
    }
}

/// Compatibility alias for callers that use the longer request name.
pub type RepositoryInitializationRequest = RepositoryInitRequest;

/// Compatibility alias for callers that use an imperative request name.
pub type InitializeRepositoryRequest = RepositoryInitRequest;

/// Validated optional expectations used when opening a repository.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct RepositoryOpenRequest {
    expected_identity: Option<RepositoryIdentity>,
    expected_repository_key: Option<RepositoryKey>,
}

impl RepositoryOpenRequest {
    /// Creates an open request that accepts any identity and repository key
    /// after the persisted descriptor has been validated.
    pub const fn new() -> Self {
        Self {
            expected_identity: None,
            expected_repository_key: None,
        }
    }

    /// Creates an open request that verifies the persisted repository identity.
    pub fn for_identity(identity: RepositoryIdentity) -> Self {
        Self {
            expected_identity: Some(identity),
            expected_repository_key: None,
        }
    }

    /// Creates an open request that verifies both persisted identity values.
    pub fn for_repository(identity: RepositoryIdentity, repository_key: RepositoryKey) -> Self {
        Self {
            expected_identity: Some(identity),
            expected_repository_key: Some(repository_key),
        }
    }

    /// Sets an expected identity for the open operation.
    pub fn with_identity(mut self, identity: RepositoryIdentity) -> Self {
        self.expected_identity = Some(identity);
        self
    }

    /// Sets an expected key for the open operation.
    pub fn with_repository_key(mut self, repository_key: RepositoryKey) -> Self {
        self.expected_repository_key = Some(repository_key);
        self
    }

    /// Alias for [`Self::with_identity`].
    pub fn identity(self, identity: RepositoryIdentity) -> Self {
        self.with_identity(identity)
    }

    /// Alias for [`Self::with_repository_key`].
    pub fn repository_key(self, repository_key: RepositoryKey) -> Self {
        self.with_repository_key(repository_key)
    }

    /// Returns the expected identity, when one was configured.
    pub const fn expected_identity(&self) -> Option<&RepositoryIdentity> {
        self.expected_identity.as_ref()
    }

    /// Returns the expected key, when one was configured.
    pub const fn expected_repository_key(&self) -> Option<&RepositoryKey> {
        self.expected_repository_key.as_ref()
    }
}

impl Default for RepositoryOpenRequest {
    fn default() -> Self {
        Self::new()
    }
}

/// Compatibility alias for callers that use an imperative request name.
pub type OpenRepositoryRequest = RepositoryOpenRequest;

/// A usable handle for a repository whose root objects have been validated.
#[derive(Clone)]
pub struct Repository {
    descriptor: RepositoryDescriptor,
    storage: StorageHandle,
}

impl Repository {
    /// Initializes the minimum valid repository using validated domain values.
    pub fn initialize<S>(
        storage: S,
        identity: RepositoryIdentity,
        repository_key: RepositoryKey,
    ) -> SdkResult<Self>
    where
        S: Into<StorageHandle>,
    {
        Self::initialize_with_request(
            storage,
            RepositoryInitRequest::new(identity, repository_key),
        )
    }

    /// Initializes the minimum valid repository from a typed request.
    pub fn initialize_with_request<S>(storage: S, request: RepositoryInitRequest) -> SdkResult<Self>
    where
        S: Into<StorageHandle>,
    {
        let storage = storage.into();
        let (identity, repository_key) = request.into_parts();
        let descriptor = initialize_use_case(storage.as_storage(), identity, repository_key)
            .map_err(SdkError::from)?;
        Ok(Self {
            descriptor,
            storage,
        })
    }

    /// Opens a repository after validating its persisted root objects.
    pub fn open<S>(storage: S) -> SdkResult<Self>
    where
        S: Into<StorageHandle>,
    {
        Self::open_with_request(storage, RepositoryOpenRequest::new())
    }

    /// Opens a repository using optional identity expectations.
    pub fn open_with_request<S>(storage: S, request: RepositoryOpenRequest) -> SdkResult<Self>
    where
        S: Into<StorageHandle>,
    {
        let storage = storage.into();
        let descriptor = open_use_case(
            storage.as_storage(),
            RepositoryOpenExpectations {
                identity: request.expected_identity(),
                repository_key: request.expected_repository_key(),
            },
        )
        .map_err(SdkError::from)?;
        Ok(Self {
            descriptor,
            storage,
        })
    }

    /// Opens a repository and verifies its persisted identity and key.
    pub fn open_for_repository<S>(
        storage: S,
        identity: RepositoryIdentity,
        repository_key: RepositoryKey,
    ) -> SdkResult<Self>
    where
        S: Into<StorageHandle>,
    {
        Self::open_with_request(
            storage,
            RepositoryOpenRequest::for_repository(identity, repository_key),
        )
    }

    /// Alias for [`Self::open_for_repository`].
    pub fn open_with_identity<S>(
        storage: S,
        identity: RepositoryIdentity,
        repository_key: RepositoryKey,
    ) -> SdkResult<Self>
    where
        S: Into<StorageHandle>,
    {
        Self::open_for_repository(storage, identity, repository_key)
    }

    /// Returns the validated repository descriptor.
    pub const fn descriptor(&self) -> &RepositoryDescriptor {
        &self.descriptor
    }

    /// Returns the repository identity reported by the descriptor.
    pub fn identity(&self) -> &RepositoryIdentity {
        self.descriptor.identity()
    }

    /// Returns the repository ID reported by the descriptor.
    pub fn repository_id(&self) -> &RepositoryIdentity {
        self.identity()
    }

    /// Returns the namespace key reported by the descriptor.
    pub fn repository_key(&self) -> &RepositoryKey {
        self.descriptor.repository_key()
    }

    /// Returns the repository format version.
    pub const fn format_version(&self) -> u16 {
        self.descriptor.format_version()
    }

    /// Returns the descriptor schema version.
    pub const fn descriptor_version(&self) -> u16 {
        self.descriptor.descriptor_version()
    }

    /// Returns the required root object references.
    pub fn roots(&self) -> &RepositoryRoots {
        self.descriptor.roots()
    }

    /// Returns the storage handle retained by this repository.
    pub fn storage(&self) -> StorageHandle {
        self.storage.clone()
    }

    /// Returns whether a published snapshot exists.
    ///
    /// Snapshot publication is not part of repository initialization in 0.1.0,
    /// so a newly initialized or opened repository always reports `false`.
    pub const fn has_published_snapshot(&self) -> bool {
        false
    }
}

impl PartialEq for Repository {
    fn eq(&self, other: &Self) -> bool {
        self.descriptor == other.descriptor
    }
}

impl Eq for Repository {}

impl fmt::Debug for Repository {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("Repository")
            .field("descriptor", &self.descriptor)
            .finish()
    }
}

/// Initializes a repository through the storage abstraction.
pub fn initialize_repository<S>(storage: S, request: RepositoryInitRequest) -> SdkResult<Repository>
where
    S: Into<StorageHandle>,
{
    Repository::initialize_with_request(storage, request)
}

/// Opens a repository through the storage abstraction.
pub fn open_repository<S>(storage: S, request: RepositoryOpenRequest) -> SdkResult<Repository>
where
    S: Into<StorageHandle>,
{
    Repository::open_with_request(storage, request)
}

impl From<DomainError> for SdkError {
    fn from(error: DomainError) -> Self {
        match error {
            DomainError::InvalidRepositoryIdentity { reason } => SdkError::InvalidRequest {
                field: "repository_identity",
                reason,
            },
            DomainError::InvalidRepositoryKey { reason } => SdkError::InvalidRequest {
                field: "repository_key",
                reason,
            },
            DomainError::InvalidRepositoryObject { reason } => SdkError::InvalidRequest {
                field: "repository_object",
                reason,
            },
        }
    }
}

impl From<RepositoryError> for SdkError {
    fn from(error: RepositoryError) -> Self {
        match error {
            RepositoryError::AlreadyExists => Self::RepositoryAlreadyExists,
            RepositoryError::Missing => Self::RepositoryMissing,
            RepositoryError::Malformed { reason } => Self::RepositoryMalformed { reason },
            RepositoryError::UnsupportedVersion { version } => {
                Self::RepositoryUnsupportedVersion { version }
            }
            RepositoryError::Incompatible { reason } => Self::RepositoryIncompatible { reason },
            RepositoryError::Storage { operation } => Self::StorageFailure { operation },
        }
    }
}

/// Alias for the current repository format version.
pub const REPOSITORY_FORMAT_VERSION: u16 = CURRENT_REPOSITORY_FORMAT_VERSION;

/// Alias for the current repository bootstrap schema version.
pub const REPOSITORY_BOOTSTRAP_VERSION: u16 = CURRENT_REPOSITORY_BOOTSTRAP_VERSION;

/// Alias for the current repository descriptor version.
pub const REPOSITORY_DESCRIPTOR_VERSION: u16 = CURRENT_REPOSITORY_DESCRIPTOR_VERSION;
