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
}

impl fmt::Display for StorageError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        let message = match self {
            Self::NotFound => "storage object was not found",
            Self::AlreadyExists => "storage object already exists",
            Self::InvalidObjectKey => "storage object key is invalid",
            Self::Io => "storage I/O operation failed",
            Self::Unavailable => "storage is unavailable",
        };
        formatter.write_str(message)
    }
}

impl std::error::Error for StorageError {}

/// Result type returned by repository storage adapters.
pub type StorageResult<T> = std::result::Result<T, StorageError>;

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
}
