use crate::domain::validate_repository_object;
use std::fmt;
use std::io::{self, Cursor, Read, Write};
use std::ops::{BitAnd, BitAndAssign, BitOr, BitOrAssign, Not};
use std::sync::Arc;

/// The default maximum number of entries returned by one object listing page.
pub const DEFAULT_OBJECT_LIST_PAGE_SIZE: usize = 100;

/// The largest object listing page accepted by the storage contract.
pub const MAX_OBJECT_LIST_PAGE_SIZE: usize = 1_000;

/// The fixed per-operation transfer buffer used by the reference adapters.
pub const STORAGE_TRANSFER_BUFFER_SIZE: usize = 64 * 1024;

/// A storage failure understood by repository use cases and infrastructure
/// adapters.
#[non_exhaustive]
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum StorageError {
    /// The requested logical object does not exist.
    NotFound,
    /// An object with the requested logical key already exists.
    AlreadyExists,
    /// A conditional operation observed a different object version.
    Conflict,
    /// The logical object key is not safe for this storage abstraction.
    InvalidObjectKey,
    /// The logical listing prefix is not valid.
    InvalidPrefix,
    /// A byte range is malformed or falls outside the object.
    InvalidRange,
    /// A listing cursor is malformed or no longer usable.
    InvalidCursor,
    /// A request violates a provider-neutral storage contract constraint.
    InvalidRequest,
    /// The configured storage root or backend could not complete an operation.
    Io,
    /// The backend could not provide a consistent operation result.
    Unavailable,
    /// The backend does not implement the requested capability.
    UnsupportedCapability,
    /// The supplied conditional-write version token did not match the current
    /// object.
    ConditionNotMet,
    /// The backend rejected the configured or supplied credentials.
    Authentication,
    /// The credentials are valid but do not permit the operation.
    PermissionDenied,
    /// The backend asked the caller to slow down or retry later because a
    /// request limit was reached.
    RateLimited,
    /// The operation failed in a way that is safe for a caller to retry.
    Transient,
    /// Cooperative cancellation interrupted the operation.
    Cancelled,
    /// A backend version token is empty or exceeds the SDK limit.
    InvalidVersion,
}

impl StorageError {
    /// Maps a standard-library I/O error to the provider-neutral contract.
    pub fn from_io_error(error: &io::Error) -> Self {
        match error.kind() {
            io::ErrorKind::NotFound => Self::NotFound,
            io::ErrorKind::AlreadyExists => Self::AlreadyExists,
            io::ErrorKind::PermissionDenied => Self::PermissionDenied,
            io::ErrorKind::Interrupted => Self::Cancelled,
            io::ErrorKind::TimedOut
            | io::ErrorKind::WouldBlock
            | io::ErrorKind::ConnectionRefused
            | io::ErrorKind::ConnectionReset
            | io::ErrorKind::ConnectionAborted
            | io::ErrorKind::NotConnected
            | io::ErrorKind::BrokenPipe => Self::Transient,
            _ => Self::Io,
        }
    }

    /// Maps a conventional HTTP response status to the provider-neutral
    /// contract without exposing an HTTP or provider SDK type.
    pub const fn from_http_status(status: u16) -> Self {
        match status {
            400 => Self::InvalidRequest,
            401 | 407 => Self::Authentication,
            403 => Self::PermissionDenied,
            404 => Self::NotFound,
            409 | 412 => Self::Conflict,
            416 => Self::InvalidRange,
            408 | 500 | 502 | 504 => Self::Transient,
            429 => Self::RateLimited,
            503 => Self::Unavailable,
            _ => Self::Io,
        }
    }

    /// Returns whether a caller may retry the failed operation.
    pub const fn is_retryable(self) -> bool {
        matches!(
            self,
            Self::RateLimited | Self::Transient | Self::Unavailable
        )
    }

    /// Returns whether this is a conditional-operation conflict.
    pub const fn is_conflict(self) -> bool {
        matches!(
            self,
            Self::Conflict | Self::ConditionNotMet | Self::AlreadyExists
        )
    }
}

impl fmt::Display for StorageError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        let message = match self {
            Self::NotFound => "storage object was not found",
            Self::AlreadyExists => "storage object already exists",
            Self::Conflict => "storage conditional operation conflicted",
            Self::InvalidObjectKey => "storage object key is invalid",
            Self::InvalidPrefix => "storage object prefix is invalid",
            Self::InvalidRange => "storage byte range is invalid",
            Self::InvalidCursor => "storage listing cursor is invalid",
            Self::InvalidRequest => "storage request is invalid",
            Self::Io => "storage I/O operation failed",
            Self::Unavailable => "storage is unavailable",
            Self::UnsupportedCapability => "storage does not support the requested capability",
            Self::ConditionNotMet => "storage conditional-write version did not match",
            Self::Authentication => "storage authentication failed",
            Self::PermissionDenied => "storage permission was denied",
            Self::RateLimited => "storage request was rate limited",
            Self::Transient => "storage operation failed transiently",
            Self::Cancelled => "storage operation was cancelled",
            Self::InvalidVersion => "storage returned an invalid version token",
        };
        formatter.write_str(message)
    }
}

impl std::error::Error for StorageError {}

/// Result type returned by repository storage adapters.
pub type StorageResult<T> = std::result::Result<T, StorageError>;

/// A validated, provider-neutral logical object key.
///
/// Keys are relative slash-separated names. They are deliberately narrower
/// than the set accepted by some providers so that a repository can move
/// between Local, S3, WebDAV, and future adapters without changing identity.
#[derive(Clone, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub struct ObjectKey(String);

impl ObjectKey {
    /// The largest accepted object-key length in UTF-8 bytes.
    pub const MAX_LENGTH: usize = crate::domain::RepositoryObject::MAX_LENGTH;

    /// Creates a validated logical object key.
    pub fn new(value: impl Into<String>) -> StorageResult<Self> {
        let value = value.into();
        validate_key(&value).map(|()| Self(value))
    }

    /// Returns the canonical logical key.
    pub fn as_str(&self) -> &str {
        &self.0
    }

    /// Consumes the key and returns its canonical string.
    pub fn into_string(self) -> String {
        self.0
    }
}

impl AsRef<str> for ObjectKey {
    fn as_ref(&self) -> &str {
        self.as_str()
    }
}

impl fmt::Display for ObjectKey {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(self.as_str())
    }
}

impl TryFrom<&str> for ObjectKey {
    type Error = StorageError;

    fn try_from(value: &str) -> StorageResult<Self> {
        Self::new(value)
    }
}

impl TryFrom<String> for ObjectKey {
    type Error = StorageError;

    fn try_from(value: String) -> StorageResult<Self> {
        Self::new(value)
    }
}

/// A validated object-listing prefix.
///
/// The empty prefix is the root listing. A single trailing slash is accepted
/// and removed so that `snapshots` and `snapshots/` have one stable form.
#[derive(Clone, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub struct ObjectPrefix(String);

impl ObjectPrefix {
    /// Creates a validated listing prefix.
    pub fn new(value: impl Into<String>) -> StorageResult<Self> {
        let value = value.into();
        if value.is_empty() {
            return Ok(Self(value));
        }
        let canonical = if let Some(stripped) = value.strip_suffix('/') {
            if stripped.is_empty() || stripped.ends_with('/') {
                return Err(StorageError::InvalidPrefix);
            }
            stripped.to_owned()
        } else {
            value
        };
        validate_key(&canonical)
            .map(|()| Self(canonical))
            .map_err(|_| StorageError::InvalidPrefix)
    }

    /// Creates the root listing prefix.
    pub const fn root() -> Self {
        Self(String::new())
    }

    /// Returns the canonical prefix.
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl AsRef<str> for ObjectPrefix {
    fn as_ref(&self) -> &str {
        self.as_str()
    }
}

impl fmt::Display for ObjectPrefix {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(self.as_str())
    }
}

impl TryFrom<&str> for ObjectPrefix {
    type Error = StorageError;

    fn try_from(value: &str) -> StorageResult<Self> {
        Self::new(value)
    }
}

/// Compatibility name for [`ObjectKey`] used by storage-focused callers.
pub type StorageKey = ObjectKey;

/// Compatibility name for [`ObjectPrefix`].
pub type StoragePrefix = ObjectPrefix;

/// An exact half-open byte range, `[start, end)`.
#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub struct ObjectRange {
    start: u64,
    end: u64,
}

impl ObjectRange {
    /// Creates a range from an offset and a byte length.
    pub fn new(start: u64, length: u64) -> StorageResult<Self> {
        let end = start
            .checked_add(length)
            .ok_or(StorageError::InvalidRange)?;
        Ok(Self { start, end })
    }

    /// Creates a range from half-open bounds.
    pub fn from_bounds(start: u64, end: u64) -> StorageResult<Self> {
        if end < start {
            return Err(StorageError::InvalidRange);
        }
        Ok(Self { start, end })
    }

    /// Returns the first byte offset.
    pub const fn start(self) -> u64 {
        self.start
    }

    /// Returns the exclusive end offset.
    pub const fn end(self) -> u64 {
        self.end
    }

    /// Returns the exact number of bytes in the range.
    pub const fn length(self) -> u64 {
        self.end - self.start
    }

    /// Returns whether the range is empty.
    pub const fn is_empty(self) -> bool {
        self.start == self.end
    }
}

/// Compatibility name for [`ObjectRange`].
pub type ByteRange = ObjectRange;

/// Compatibility name for [`ObjectRange`] used by backend adapters.
pub type StorageRange = ObjectRange;

/// An opaque cursor for a lexically ordered object listing.
#[derive(Clone, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub struct ObjectCursor(String);

impl ObjectCursor {
    /// The largest accepted listing-cursor length in UTF-8 bytes.
    pub const MAX_LENGTH: usize = ObjectKey::MAX_LENGTH;

    /// Creates a cursor token.
    pub fn new(value: impl Into<String>) -> StorageResult<Self> {
        let value = value.into();
        if value.is_empty() || value.len() > Self::MAX_LENGTH {
            return Err(StorageError::InvalidCursor);
        }
        Ok(Self(value))
    }

    /// Returns the opaque cursor token.
    pub fn as_str(&self) -> &str {
        &self.0
    }

    /// Consumes the cursor and returns its token.
    pub fn into_string(self) -> String {
        self.0
    }
}

impl AsRef<str> for ObjectCursor {
    fn as_ref(&self) -> &str {
        self.as_str()
    }
}

/// Compatibility name for [`ObjectCursor`].
pub type ListCursor = ObjectCursor;

impl fmt::Display for ObjectCursor {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(self.as_str())
    }
}

/// A bounded object-listing request.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ObjectListRequest {
    prefix: ObjectPrefix,
    cursor: Option<ObjectCursor>,
    limit: usize,
}

impl ObjectListRequest {
    /// Creates a listing request with the default page size.
    pub fn new(prefix: ObjectPrefix) -> Self {
        Self {
            prefix,
            cursor: None,
            limit: DEFAULT_OBJECT_LIST_PAGE_SIZE,
        }
    }

    /// Creates a root listing request.
    pub fn root() -> Self {
        Self::new(ObjectPrefix::root())
    }

    /// Sets the maximum number of entries in the returned page.
    pub const fn with_limit(mut self, limit: usize) -> Self {
        self.limit = limit;
        self
    }

    /// Sets the exclusive continuation cursor.
    pub fn with_cursor(mut self, cursor: ObjectCursor) -> Self {
        self.cursor = Some(cursor);
        self
    }

    /// Returns the requested prefix.
    pub fn prefix(&self) -> &ObjectPrefix {
        &self.prefix
    }

    /// Returns the exclusive continuation cursor, if any.
    pub fn cursor(&self) -> Option<&ObjectCursor> {
        self.cursor.as_ref()
    }

    /// Returns the requested page size.
    pub const fn limit(&self) -> usize {
        self.limit
    }

    pub(crate) fn validate(&self) -> StorageResult<()> {
        if !(1..=MAX_OBJECT_LIST_PAGE_SIZE).contains(&self.limit) {
            return Err(StorageError::InvalidRequest);
        }
        Ok(())
    }
}

/// Compatibility name for [`ObjectListRequest`].
pub type StorageListRequest = ObjectListRequest;

/// Metadata returned for one object.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ObjectMetadata {
    key: ObjectKey,
    size: u64,
    version: Option<StorageVersion>,
}

impl ObjectMetadata {
    /// Creates metadata for a validated key.
    pub const fn new(key: ObjectKey, size: u64, version: Option<StorageVersion>) -> Self {
        Self { key, size, version }
    }

    /// Returns the logical object key.
    pub const fn key(&self) -> &ObjectKey {
        &self.key
    }

    /// Returns the object size in bytes.
    pub const fn size(&self) -> u64 {
        self.size
    }

    /// Returns the opaque version token, when the backend supplies one.
    pub const fn version(&self) -> Option<&StorageVersion> {
        self.version.as_ref()
    }

    /// Consumes metadata and returns its values.
    pub fn into_parts(self) -> (ObjectKey, u64, Option<StorageVersion>) {
        (self.key, self.size, self.version)
    }
}

/// Compatibility name for [`ObjectMetadata`].
pub type StorageMetadata = ObjectMetadata;

/// A streaming object read together with metadata for the same object view.
pub struct ObjectRead {
    metadata: ObjectMetadata,
    reader: StorageReader,
}

impl ObjectRead {
    /// Wraps a reader and metadata returned by a storage adapter.
    pub fn new<R>(metadata: ObjectMetadata, reader: R) -> Self
    where
        R: Read + Send + 'static,
    {
        Self {
            metadata,
            reader: Box::new(reader),
        }
    }

    /// Returns metadata for the full object.
    pub const fn metadata(&self) -> &ObjectMetadata {
        &self.metadata
    }

    /// Returns the underlying bounded reader.
    pub fn reader(&mut self) -> &mut dyn Read {
        self.reader.as_mut()
    }

    /// Consumes the read and returns its metadata and reader.
    pub fn into_parts(self) -> (ObjectMetadata, StorageReader) {
        (self.metadata, self.reader)
    }

    /// Consumes the read and returns only the bounded payload reader.
    pub fn into_reader(self) -> StorageReader {
        self.reader
    }
}

impl fmt::Debug for ObjectRead {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("ObjectRead")
            .field("metadata", &self.metadata)
            .finish_non_exhaustive()
    }
}

impl Read for ObjectRead {
    fn read(&mut self, buffer: &mut [u8]) -> io::Result<usize> {
        self.reader.read(buffer)
    }
}

/// The bounded reader returned by a storage adapter.
pub type StorageReader = Box<dyn Read + Send>;

/// Compatibility name for [`StorageReader`].
pub type ObjectReader = StorageReader;

/// The condition applied to a streaming write.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum StorageWriteCondition {
    /// Write regardless of the currently stored version.
    Any,
    /// Write only when the object is absent.
    IfAbsent,
    /// Write only when the object has exactly this version token.
    IfVersion(StorageVersion),
}

/// Options for a streaming object write.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ObjectWriteOptions {
    condition: StorageWriteCondition,
    expected_size: Option<u64>,
}

impl ObjectWriteOptions {
    /// Creates an unconditional write option.
    pub const fn new() -> Self {
        Self {
            condition: StorageWriteCondition::Any,
            expected_size: None,
        }
    }

    /// Creates an option for atomic create-if-absent publication.
    pub const fn if_absent() -> Self {
        Self {
            condition: StorageWriteCondition::IfAbsent,
            expected_size: None,
        }
    }

    /// Creates an option for an atomic version-conditional replacement.
    pub fn if_version(version: StorageVersion) -> Self {
        Self {
            condition: StorageWriteCondition::IfVersion(version),
            expected_size: None,
        }
    }

    /// Sets an exact source size, allowing adapters to reject truncation or
    /// excess input before publication.
    pub const fn with_expected_size(mut self, expected_size: u64) -> Self {
        self.expected_size = Some(expected_size);
        self
    }

    /// Returns the write condition.
    pub const fn condition(&self) -> &StorageWriteCondition {
        &self.condition
    }

    /// Returns the expected source size, if supplied.
    pub const fn expected_size(&self) -> Option<u64> {
        self.expected_size
    }
}

impl Default for ObjectWriteOptions {
    fn default() -> Self {
        Self::new()
    }
}

/// Compatibility name for [`ObjectWriteOptions`].
pub type StorageWriteOptions = ObjectWriteOptions;

/// Compatibility name for [`StorageWriteCondition`].
pub type WriteCondition = StorageWriteCondition;

/// A page of lexically ordered object metadata.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ObjectListPage {
    objects: Vec<ObjectMetadata>,
    next_cursor: Option<ObjectCursor>,
}

impl ObjectListPage {
    /// Creates a listing page.
    pub fn new(objects: Vec<ObjectMetadata>, next_cursor: Option<ObjectCursor>) -> Self {
        Self {
            objects,
            next_cursor,
        }
    }

    /// Returns the objects in strictly increasing logical-key order.
    pub fn objects(&self) -> &[ObjectMetadata] {
        &self.objects
    }

    /// Returns the cursor for the next page, if more objects exist.
    pub const fn next_cursor(&self) -> Option<&ObjectCursor> {
        self.next_cursor.as_ref()
    }

    /// Consumes the page and returns its entries and continuation cursor.
    pub fn into_parts(self) -> (Vec<ObjectMetadata>, Option<ObjectCursor>) {
        (self.objects, self.next_cursor)
    }
}

/// Compatibility name for [`ObjectListPage`].
pub type StorageListPage = ObjectListPage;

/// The individual capability represented by [`StorageCapabilities`].
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
#[non_exhaustive]
pub enum StorageCapability {
    /// Streaming whole-object reads.
    StreamingRead,
    /// Streaming writes.
    StreamingWrite,
    /// Object metadata reads.
    Metadata,
    /// Prefix listing with bounded pages.
    PrefixListing,
    /// Object deletion.
    Delete,
    /// Exact byte-range reads.
    RangeRead,
    /// Atomic conditional writes.
    ConditionalWrite,
}

impl StorageCapability {
    const fn flag(self) -> StorageCapabilities {
        match self {
            Self::StreamingRead => StorageCapabilities::STREAMING_READ,
            Self::StreamingWrite => StorageCapabilities::STREAMING_WRITE,
            Self::Metadata => StorageCapabilities::METADATA,
            Self::PrefixListing => StorageCapabilities::PREFIX_LISTING,
            Self::Delete => StorageCapabilities::DELETE,
            Self::RangeRead => StorageCapabilities::RANGE_READ,
            Self::ConditionalWrite => StorageCapabilities::CONDITIONAL_WRITE,
        }
    }
}

/// Capability flags advertised by a storage adapter.
#[derive(Clone, Copy, Debug, Default, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub struct StorageCapabilities(u32);

impl StorageCapabilities {
    /// No optional storage capabilities.
    pub const NONE: Self = Self(0);
    /// Bounded whole-object reads are supported.
    pub const STREAMING_READ: Self = Self(1 << 0);
    /// Bounded source-driven writes are supported.
    pub const STREAMING_WRITE: Self = Self(1 << 1);
    /// Metadata reads are supported.
    pub const METADATA: Self = Self(1 << 2);
    /// Paged prefix listings are supported.
    pub const PREFIX_LISTING: Self = Self(1 << 3);
    /// Object deletion is supported.
    pub const DELETE: Self = Self(1 << 4);
    /// Exact byte-range reads are supported.
    pub const RANGE_READ: Self = Self(1 << 5);
    /// Atomic conditional writes are supported.
    pub const CONDITIONAL_WRITE: Self = Self(1 << 6);

    /// Compatibility spelling for [`Self::STREAMING_READ`].
    pub const READ_STREAM: Self = Self::STREAMING_READ;
    /// Compatibility spelling for [`Self::STREAMING_WRITE`].
    pub const WRITE_STREAM: Self = Self::STREAMING_WRITE;
    /// Compatibility spelling for [`Self::PREFIX_LISTING`].
    pub const LIST: Self = Self::PREFIX_LISTING;
    /// All capabilities currently defined by this contract.
    pub const ALL: Self = Self(
        Self::STREAMING_READ.0
            | Self::STREAMING_WRITE.0
            | Self::METADATA.0
            | Self::PREFIX_LISTING.0
            | Self::DELETE.0
            | Self::RANGE_READ.0
            | Self::CONDITIONAL_WRITE.0,
    );

    /// Creates flags from raw bits, rejecting unknown future bits.
    pub const fn from_bits(bits: u32) -> Option<Self> {
        if bits & !Self::ALL.0 == 0 {
            Some(Self(bits))
        } else {
            None
        }
    }

    /// Returns the raw capability bits.
    pub const fn bits(self) -> u32 {
        self.0
    }

    /// Returns whether all flags in `required` are present.
    pub const fn contains(self, required: Self) -> bool {
        self.0 & required.0 == required.0
    }

    /// Returns whether an individual capability is present.
    pub const fn supports(self, capability: StorageCapability) -> bool {
        self.contains(capability.flag())
    }
}

impl BitOr for StorageCapabilities {
    type Output = Self;

    fn bitor(self, rhs: Self) -> Self::Output {
        Self(self.0 | rhs.0)
    }
}

impl BitOrAssign for StorageCapabilities {
    fn bitor_assign(&mut self, rhs: Self) {
        self.0 |= rhs.0;
    }
}

impl BitAnd for StorageCapabilities {
    type Output = Self;

    fn bitand(self, rhs: Self) -> Self::Output {
        Self(self.0 & rhs.0)
    }
}

impl BitAndAssign for StorageCapabilities {
    fn bitand_assign(&mut self, rhs: Self) {
        self.0 &= rhs.0;
    }
}

impl Not for StorageCapabilities {
    type Output = Self;

    fn not(self) -> Self::Output {
        Self(!self.0 & Self::ALL.0)
    }
}

/// An opaque version token returned by a storage backend.
///
/// Tokens are compared byte-for-byte and are never interpreted by the
/// application layer. Backends may use an object generation, an entity tag,
/// or another native conditional-write token. Tokens are deliberately bounded
/// because they may be carried through a retry request.
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

/// Backend-neutral object-storage operations required by use cases.
///
/// The `read`, `create_if_absent`, `list_objects`, `read_with_version`, and
/// `compare_and_swap` methods are retained as compatibility conveniences for
/// the repository lifecycle API. New large-object code must use the streaming
/// methods and negotiate their capability before use.
pub trait RepositoryStorage: Send + Sync {
    /// Creates one immutable object only when its logical key is absent.
    fn create_if_absent(&self, object_key: &str, contents: &[u8]) -> StorageResult<()> {
        let key = ObjectKey::new(object_key)?;
        let mut source = Cursor::new(contents);
        self.write_stream(&key, &mut source, ObjectWriteOptions::if_absent())
            .map(|_| ())
    }

    /// Reads one object into memory for small repository metadata objects.
    fn read(&self, object_key: &str) -> StorageResult<Vec<u8>> {
        let key = ObjectKey::new(object_key)?;
        let mut object = self.read_stream(&key)?;
        let size = object.metadata().size();
        read_stream_to_vec(object.reader(), Some(size))
    }

    /// Returns the capabilities advertised by this adapter.
    fn capabilities(&self) -> StorageCapabilities {
        StorageCapabilities::NONE
    }

    /// Opens a whole-object reader without buffering the object in the port.
    fn read_stream(&self, object_key: &ObjectKey) -> StorageResult<ObjectRead> {
        let _ = object_key;
        Err(StorageError::UnsupportedCapability)
    }

    /// Opens an exact half-open byte range without buffering the range.
    fn read_range(&self, object_key: &ObjectKey, range: ObjectRange) -> StorageResult<ObjectRead> {
        let _ = (object_key, range);
        Err(StorageError::UnsupportedCapability)
    }

    /// Returns metadata without opening an object payload.
    fn metadata(&self, object_key: &ObjectKey) -> StorageResult<ObjectMetadata> {
        let _ = object_key;
        Err(StorageError::UnsupportedCapability)
    }

    /// Writes from a caller-owned source using bounded adapter buffers.
    ///
    /// A conditional write checks and publishes atomically. If the source
    /// fails or its declared size does not match, the target is left unchanged.
    fn write_stream(
        &self,
        object_key: &ObjectKey,
        source: &mut dyn Read,
        options: ObjectWriteOptions,
    ) -> StorageResult<ObjectMetadata> {
        let _ = (object_key, source, options);
        Err(StorageError::UnsupportedCapability)
    }

    /// Deletes one object.
    fn delete(&self, object_key: &ObjectKey) -> StorageResult<()> {
        let _ = object_key;
        Err(StorageError::UnsupportedCapability)
    }

    /// Lists one bounded page in strictly increasing lexical key order.
    ///
    /// The cursor is exclusive. A page is a snapshot of the adapter's result
    /// at call time; callers must tolerate objects being added or removed
    /// between pages and must not assume a cursor can be reused forever.
    fn list_page(&self, request: &ObjectListRequest) -> StorageResult<ObjectListPage> {
        let _ = request;
        Err(StorageError::UnsupportedCapability)
    }

    /// Lists all keys below a prefix for the legacy repository API.
    fn list_objects(&self, prefix: &str) -> StorageResult<Vec<String>> {
        let prefix = ObjectPrefix::new(prefix)?;
        let mut request = ObjectListRequest::new(prefix);
        let mut keys = Vec::new();
        loop {
            let page = self.list_page(&request)?;
            let (objects, next_cursor) = page.into_parts();
            keys.extend(objects.into_iter().map(|object| object.key.into_string()));
            let Some(cursor) = next_cursor else {
                break;
            };
            request = request.with_cursor(cursor);
        }
        Ok(keys)
    }

    /// Alias for [`Self::list_objects`] using shorter storage terminology.
    fn list(&self, prefix: &str) -> StorageResult<Vec<String>> {
        self.list_objects(prefix)
    }

    /// Reads one object and returns the backend version token for that read.
    fn read_with_version(&self, object_key: &str) -> StorageResult<VersionedObject> {
        let key = ObjectKey::new(object_key)?;
        let mut object = self.read_stream(&key)?;
        let version = object
            .metadata()
            .version()
            .cloned()
            .ok_or(StorageError::InvalidVersion)?;
        let size = object.metadata().size();
        let contents = read_stream_to_vec(object.reader(), Some(size))?;
        Ok(VersionedObject::new(contents, version))
    }

    /// Replaces one object only when its version still equals `expected`.
    fn compare_and_swap(
        &self,
        object_key: &str,
        expected: Option<&StorageVersion>,
        contents: &[u8],
    ) -> StorageResult<StorageVersion> {
        let key = ObjectKey::new(object_key)?;
        let mut source = Cursor::new(contents);
        let options = match expected {
            Some(version) => ObjectWriteOptions::if_version(version.clone()),
            None => ObjectWriteOptions::if_absent(),
        };
        let metadata = self.write_stream(&key, &mut source, options)?;
        metadata
            .version()
            .cloned()
            .ok_or(StorageError::InvalidVersion)
    }

    /// Alias for [`Self::read_with_version`] using shorter version wording.
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

    /// Performs a conditional write from a bounded source reader.
    fn conditional_write_stream(
        &self,
        object_key: &ObjectKey,
        expected: Option<&StorageVersion>,
        source: &mut dyn Read,
        expected_size: Option<u64>,
    ) -> StorageResult<ObjectMetadata> {
        let condition = match expected {
            Some(version) => StorageWriteCondition::IfVersion(version.clone()),
            None => StorageWriteCondition::IfAbsent,
        };
        self.write_stream(
            object_key,
            source,
            ObjectWriteOptions {
                condition,
                expected_size,
            },
        )
    }

    /// Alias for [`Self::read_stream`] using object-storage terminology.
    fn get(&self, object_key: &ObjectKey) -> StorageResult<ObjectRead> {
        self.read_stream(object_key)
    }

    /// Alias for [`Self::read_range`].
    fn get_range(&self, object_key: &ObjectKey, range: ObjectRange) -> StorageResult<ObjectRead> {
        self.read_range(object_key, range)
    }

    /// Alias for [`Self::metadata`].
    fn head(&self, object_key: &ObjectKey) -> StorageResult<ObjectMetadata> {
        self.metadata(object_key)
    }

    /// Alias for [`Self::write_stream`].
    fn put_stream(
        &self,
        object_key: &ObjectKey,
        source: &mut dyn Read,
        options: ObjectWriteOptions,
    ) -> StorageResult<ObjectMetadata> {
        self.write_stream(object_key, source, options)
    }

    /// Alias for [`Self::delete`].
    fn delete_object(&self, object_key: &ObjectKey) -> StorageResult<()> {
        self.delete(object_key)
    }
}

/// Canonical provider-neutral name for the storage port.
pub trait ObjectStorage: RepositoryStorage {}

impl<T> ObjectStorage for T where T: RepositoryStorage + ?Sized {}

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

    fn capabilities(&self) -> StorageCapabilities {
        self.as_ref().capabilities()
    }

    fn read_stream(&self, object_key: &ObjectKey) -> StorageResult<ObjectRead> {
        self.as_ref().read_stream(object_key)
    }

    fn read_range(&self, object_key: &ObjectKey, range: ObjectRange) -> StorageResult<ObjectRead> {
        self.as_ref().read_range(object_key, range)
    }

    fn metadata(&self, object_key: &ObjectKey) -> StorageResult<ObjectMetadata> {
        self.as_ref().metadata(object_key)
    }

    fn write_stream(
        &self,
        object_key: &ObjectKey,
        source: &mut dyn Read,
        options: ObjectWriteOptions,
    ) -> StorageResult<ObjectMetadata> {
        self.as_ref().write_stream(object_key, source, options)
    }

    fn delete(&self, object_key: &ObjectKey) -> StorageResult<()> {
        self.as_ref().delete(object_key)
    }

    fn list_page(&self, request: &ObjectListRequest) -> StorageResult<ObjectListPage> {
        self.as_ref().list_page(request)
    }

    fn list_objects(&self, prefix: &str) -> StorageResult<Vec<String>> {
        self.as_ref().list_objects(prefix)
    }

    fn list(&self, prefix: &str) -> StorageResult<Vec<String>> {
        self.as_ref().list(prefix)
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

    fn conditional_write_stream(
        &self,
        object_key: &ObjectKey,
        expected: Option<&StorageVersion>,
        source: &mut dyn Read,
        expected_size: Option<u64>,
    ) -> StorageResult<ObjectMetadata> {
        self.as_ref()
            .conditional_write_stream(object_key, expected, source, expected_size)
    }

    fn get(&self, object_key: &ObjectKey) -> StorageResult<ObjectRead> {
        self.as_ref().get(object_key)
    }

    fn get_range(&self, object_key: &ObjectKey, range: ObjectRange) -> StorageResult<ObjectRead> {
        self.as_ref().get_range(object_key, range)
    }

    fn head(&self, object_key: &ObjectKey) -> StorageResult<ObjectMetadata> {
        self.as_ref().head(object_key)
    }

    fn put_stream(
        &self,
        object_key: &ObjectKey,
        source: &mut dyn Read,
        options: ObjectWriteOptions,
    ) -> StorageResult<ObjectMetadata> {
        self.as_ref().put_stream(object_key, source, options)
    }

    fn delete_object(&self, object_key: &ObjectKey) -> StorageResult<()> {
        self.as_ref().delete_object(object_key)
    }
}

pub(crate) fn read_stream_to_vec(
    source: &mut dyn Read,
    expected_size: Option<u64>,
) -> StorageResult<Vec<u8>> {
    let mut contents = Vec::new();
    copy_stream(source, &mut contents, expected_size)?;
    Ok(contents)
}

pub(crate) fn copy_stream(
    source: &mut dyn Read,
    target: &mut dyn Write,
    expected_size: Option<u64>,
) -> StorageResult<u64> {
    let mut buffer = [0_u8; STORAGE_TRANSFER_BUFFER_SIZE];
    let mut total = 0_u64;
    loop {
        let read = source.read(&mut buffer).map_err(|error| {
            if error.kind() == io::ErrorKind::Interrupted {
                StorageError::Cancelled
            } else {
                StorageError::from_io_error(&error)
            }
        })?;
        if read == 0 {
            break;
        }
        if read > buffer.len() {
            return Err(StorageError::InvalidRequest);
        }
        total = total
            .checked_add(u64::try_from(read).map_err(|_| StorageError::InvalidRequest)?)
            .ok_or(StorageError::InvalidRequest)?;
        if expected_size.is_some_and(|expected| total > expected) {
            return Err(StorageError::InvalidRequest);
        }
        target
            .write_all(&buffer[..read])
            .map_err(|error| StorageError::from_io_error(&error))?;
    }
    if expected_size.is_some_and(|expected| total != expected) {
        return Err(StorageError::InvalidRequest);
    }
    Ok(total)
}

fn validate_key(value: &str) -> StorageResult<()> {
    validate_repository_object(value).map_err(|_| StorageError::InvalidObjectKey)
}
