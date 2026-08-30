use std::fmt;
use std::sync::Arc;

/// A storage failure understood by repository use cases.
#[non_exhaustive]
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum StorageError {
    /// The requested logical object does not exist.
    NotFound,
    /// An object with the requested logical key already exists.
    AlreadyExists,
    /// The logical object key is not safe for this storage abstraction.
    InvalidObjectKey,
    /// The configured storage root or backend could not complete an operation.
    Io,
    /// The backend could not provide a consistent operation result.
    Unavailable,
    /// The backend does not implement the conditional-write capability.
    UnsupportedCapability,
    /// The conditional-write version token did not match the current object.
    ConditionNotMet,
    /// A backend version token is empty or exceeds the SDK limit.
    InvalidVersion,
}

impl fmt::Display for StorageError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        let message = match self {
            Self::NotFound => "storage object was not found",
            Self::AlreadyExists => "storage object already exists",
            Self::InvalidObjectKey => "storage object key is invalid",
            Self::Io => "storage I/O operation failed",
            Self::Unavailable => "storage is unavailable",
            Self::UnsupportedCapability => {
                "storage does not support conditional repository publication"
            }
            Self::ConditionNotMet => "storage conditional-write version did not match",
            Self::InvalidVersion => "storage returned an invalid version token",
        };
        formatter.write_str(message)
    }
}

impl std::error::Error for StorageError {}

/// Result type returned by repository storage adapters.
pub type StorageResult<T> = std::result::Result<T, StorageError>;

/// An opaque version token returned by a storage backend.
///
/// Tokens are compared byte-for-byte and are never interpreted by the
/// application layer. Backends may use an object generation, an entity tag,
/// or another native conditional-write token. Tokens are deliberately bounded
/// because they are retained in a public HEAD read and may be carried through
/// a retry request.
#[derive(Clone, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub struct StorageVersion(Vec<u8>);

impl StorageVersion {
    /// The largest accepted backend version-token size in bytes.
    pub const MAX_LENGTH: usize = 256;

    /// Creates a version token after applying the common storage bounds.
    pub fn new(value: impl Into<Vec<u8>>) -> StorageResult<Self> {
        let value = value.into();
        if value.is_empty() || value.len() > Self::MAX_LENGTH {
            return Err(StorageError::InvalidVersion);
        }
        Ok(Self(value))
    }

    /// Creates a version token from bytes.
    pub fn from_bytes(value: impl Into<Vec<u8>>) -> StorageResult<Self> {
        Self::new(value)
    }

    /// Returns the opaque token bytes.
    pub fn as_bytes(&self) -> &[u8] {
        &self.0
    }

    /// Consumes the token and returns its bytes.
    pub fn into_bytes(self) -> Vec<u8> {
        self.0
    }
}

impl AsRef<[u8]> for StorageVersion {
    fn as_ref(&self) -> &[u8] {
        self.as_bytes()
    }
}

/// Compatibility name for [`StorageVersion`] used by conditional-write APIs.
pub type VersionToken = StorageVersion;

/// Compatibility name for [`StorageVersion`] used by backend adapters.
pub type StorageVersionToken = StorageVersion;

/// One object read together with the backend token for that exact read.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct VersionedObject {
    contents: Vec<u8>,
    version: StorageVersion,
}

impl VersionedObject {
    /// Creates a versioned object result for a storage adapter.
    pub fn new(contents: impl Into<Vec<u8>>, version: StorageVersion) -> Self {
        Self {
            contents: contents.into(),
            version,
        }
    }

    /// Returns the object bytes.
    pub fn contents(&self) -> &[u8] {
        &self.contents
    }

    /// Returns the backend version token observed with these bytes.
    pub fn version(&self) -> &StorageVersion {
        &self.version
    }

    /// Consumes the result and returns the object bytes and version token.
    pub fn into_parts(self) -> (Vec<u8>, StorageVersion) {
        (self.contents, self.version)
    }
}

/// Compatibility name for [`VersionedObject`].
pub type VersionedStorageObject = VersionedObject;

/// Backend-neutral operations required by repository lifecycle use cases.
///
/// Object keys are validated relative references such as `format` and
/// `config/repository`. Implementations must make [`Self::create_if_absent`]
/// atomic with respect to concurrent callers and must never replace an object
/// that already exists.
pub trait RepositoryStorage: Send + Sync {
    /// Creates one immutable object only when its logical key is absent.
    fn create_if_absent(&self, object_key: &str, contents: &[u8]) -> StorageResult<()>;

    /// Reads one object without creating or modifying it.
    fn read(&self, object_key: &str) -> StorageResult<Vec<u8>>;

    /// Reads one object and returns the backend version token for that read.
    ///
    /// This is a required capability for repository HEAD reads. The default
    /// implementation is deliberately unsupported: a plain read cannot be
    /// upgraded into a safe compare-and-swap precondition.
    fn read_with_version(&self, object_key: &str) -> StorageResult<VersionedObject> {
        let _ = object_key;
        Err(StorageError::UnsupportedCapability)
    }

    /// Replaces one object only when its version still equals `expected`.
    ///
    /// `None` means that the object must still be absent, which is used for
    /// first publication. The check and replacement must be one backend
    /// conditional-write operation; callers must not emulate this with a
    /// separate read followed by an unconditional write.
    fn compare_and_swap(
        &self,
        object_key: &str,
        expected: Option<&StorageVersion>,
        contents: &[u8],
    ) -> StorageResult<StorageVersion> {
        let _ = (object_key, expected, contents);
        Err(StorageError::UnsupportedCapability)
    }

    /// Alias for [`Self::read_with_version`] used by callers that call tokens
    /// versions.
    fn read_versioned(&self, object_key: &str) -> StorageResult<VersionedObject> {
        self.read_with_version(object_key)
    }

    /// Alias for [`Self::compare_and_swap`] using conditional-write wording.
    fn conditional_write(
        &self,
        object_key: &str,
        expected: Option<&StorageVersion>,
        contents: &[u8],
    ) -> StorageResult<StorageVersion> {
        self.compare_and_swap(object_key, expected, contents)
    }
}

impl<T> RepositoryStorage for Arc<T>
where
    T: RepositoryStorage + ?Sized,
{
    fn create_if_absent(&self, object_key: &str, contents: &[u8]) -> StorageResult<()> {
        self.as_ref().create_if_absent(object_key, contents)
    }

    fn read(&self, object_key: &str) -> StorageResult<Vec<u8>> {
        self.as_ref().read(object_key)
    }

    fn read_with_version(&self, object_key: &str) -> StorageResult<VersionedObject> {
        self.as_ref().read_with_version(object_key)
    }

    fn read_versioned(&self, object_key: &str) -> StorageResult<VersionedObject> {
        self.as_ref().read_versioned(object_key)
    }

    fn compare_and_swap(
        &self,
        object_key: &str,
        expected: Option<&StorageVersion>,
        contents: &[u8],
    ) -> StorageResult<StorageVersion> {
        self.as_ref()
            .compare_and_swap(object_key, expected, contents)
    }

    fn conditional_write(
        &self,
        object_key: &str,
        expected: Option<&StorageVersion>,
        contents: &[u8],
    ) -> StorageResult<StorageVersion> {
        self.as_ref()
            .conditional_write(object_key, expected, contents)
    }
}
