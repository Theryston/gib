use super::{ChunkId, DomainError, ObjectId, ObjectKind, RepositoryObject};
use std::collections::BTreeSet;
use std::fmt;
use std::sync::Arc;

/// The current logical node schema version.
pub const CURRENT_TREE_NODE_VERSION: u16 = super::CURRENT_TREE_OBJECT_VERSION;

/// The current portable metadata schema version embedded in each tree node.
pub const CURRENT_TREE_METADATA_VERSION: u16 = 1;

/// The largest UTF-8 name accepted in a portable tree.
pub const MAX_TREE_NAME_BYTES: usize = 255;

/// The largest UTF-8 path accepted by tree lookup and traversal.
pub const MAX_TREE_PATH_BYTES: usize = 4 * 1024;

/// The largest raw symlink target accepted in a portable tree.
pub const MAX_SYMLINK_TARGET_BYTES: usize = 16 * 1024;

/// The largest number of entries in one directory node.
pub const MAX_TREE_ENTRIES: usize = 1_000_000;

/// The largest number of chunk references in one regular-file node.
pub const MAX_FILE_CHUNK_REFERENCES: usize = 1_000_000;

/// The largest metadata namespace identifier.
pub const MAX_METADATA_NAMESPACE_BYTES: usize = 64;

/// The largest opaque value in one metadata extension.
pub const MAX_METADATA_EXTENSION_BYTES: usize = 64 * 1024;

/// The largest number of optional metadata extensions on one node.
pub const MAX_METADATA_EXTENSIONS: usize = 32;

/// A validated single path component.
///
/// Names are kept byte-for-byte in UTF-8 form. The tree format deliberately
/// does not apply Unicode normalization because changing normalization rules
/// would change filesystem identity; callers should normalize at their
/// filesystem boundary when that is part of their platform policy.
#[derive(Clone, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub struct EntryName(String);

impl EntryName {
    /// Creates a portable name after rejecting traversal and platform-
    /// ambiguous syntax.
    pub fn new(value: impl Into<String>) -> Result<Self, EntryNameError> {
        let value = value.into();
        if value.is_empty() {
            return Err(EntryNameError::Empty);
        }
        if value.len() > MAX_TREE_NAME_BYTES {
            return Err(EntryNameError::TooLong);
        }
        if value == "." || value == ".." {
            return Err(EntryNameError::Traversal);
        }
        if value.chars().any(|character| {
            character.is_control()
                || matches!(
                    character,
                    '/' | '\\' | ':' | '*' | '?' | '"' | '<' | '>' | '|'
                )
        }) {
            return Err(EntryNameError::InvalidCharacter);
        }
        if value
            .chars()
            .last()
            .is_some_and(|character| matches!(character, '.' | ' '))
        {
            return Err(EntryNameError::TrailingSpaceOrDot);
        }
        if is_windows_reserved_name(&value) {
            return Err(EntryNameError::ReservedDeviceName);
        }
        Ok(Self(value))
    }

    /// Returns the validated UTF-8 name.
    pub fn as_str(&self) -> &str {
        &self.0
    }

    /// Returns the name as UTF-8 bytes.
    pub fn as_bytes(&self) -> &[u8] {
        self.as_str().as_bytes()
    }
}

impl AsRef<str> for EntryName {
    fn as_ref(&self) -> &str {
        self.as_str()
    }
}

impl From<EntryName> for String {
    fn from(value: EntryName) -> Self {
        value.0
    }
}

impl fmt::Display for EntryName {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(self.as_str())
    }
}

impl TryFrom<&str> for EntryName {
    type Error = EntryNameError;

    fn try_from(value: &str) -> Result<Self, Self::Error> {
        Self::new(value)
    }
}

impl TryFrom<String> for EntryName {
    type Error = EntryNameError;

    fn try_from(value: String) -> Result<Self, Self::Error> {
        Self::new(value)
    }
}

/// Explains why a path component cannot be represented portably.
#[non_exhaustive]
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum EntryNameError {
    /// The component is empty.
    Empty,
    /// The component exceeds [`MAX_TREE_NAME_BYTES`].
    TooLong,
    /// The component is `.` or `..`.
    Traversal,
    /// The component contains a separator, NUL, control character, or a
    /// platform-reserved punctuation character.
    InvalidCharacter,
    /// The component ends in a space or dot, which is ambiguous on Windows.
    TrailingSpaceOrDot,
    /// The component names a Windows device such as `CON` or `LPT1`.
    ReservedDeviceName,
}

impl fmt::Display for EntryNameError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        let message = match self {
            Self::Empty => "tree entry name must not be empty",
            Self::TooLong => "tree entry name exceeds the portable length limit",
            Self::Traversal => "tree entry name must not be a traversal component",
            Self::InvalidCharacter => "tree entry name contains an invalid or ambiguous character",
            Self::TrailingSpaceOrDot => "tree entry name must not end with a space or dot",
            Self::ReservedDeviceName => "tree entry name is reserved by Windows",
        };
        formatter.write_str(message)
    }
}

impl std::error::Error for EntryNameError {}

/// A normalized, slash-separated relative path.
///
/// The root path is represented by the empty string. Accepted paths contain
/// validated [`EntryName`] components and therefore never contain absolute
/// prefixes, parent traversal, duplicate separators, or backslashes.
#[derive(Clone, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub struct RelativePath(String);

impl RelativePath {
    /// Returns the normalized root path.
    pub const fn root() -> Self {
        Self(String::new())
    }

    /// Parses a normalized relative path.
    pub fn new(value: impl Into<String>) -> Result<Self, RelativePathError> {
        let value = value.into();
        if value.is_empty() {
            return Ok(Self::root());
        }
        if value.len() > MAX_TREE_PATH_BYTES {
            return Err(RelativePathError::TooLong);
        }
        if value.starts_with('/') || value.ends_with('/') {
            return Err(RelativePathError::AbsoluteOrTrailingSeparator);
        }
        if value.contains("//") || value.contains('\\') || value.contains('\0') {
            return Err(RelativePathError::InvalidSeparator);
        }
        let mut components = Vec::new();
        for component in value.split('/') {
            components.push(EntryName::new(component).map_err(RelativePathError::InvalidName)?);
        }
        Self::from_components(components).map_err(|error| match error {
            RelativePathError::TooLong => RelativePathError::TooLong,
            other => other,
        })
    }

    /// Builds a normalized path from validated components.
    pub fn from_components<I>(components: I) -> Result<Self, RelativePathError>
    where
        I: IntoIterator<Item = EntryName>,
    {
        let mut value = String::new();
        for component in components {
            if !value.is_empty() {
                value.push('/');
            }
            value.push_str(component.as_str());
            if value.len() > MAX_TREE_PATH_BYTES {
                return Err(RelativePathError::TooLong);
            }
        }
        Ok(Self(value))
    }

    /// Returns the canonical slash-separated representation.
    pub fn as_str(&self) -> &str {
        &self.0
    }

    /// Returns whether this is the root path.
    pub const fn is_root(&self) -> bool {
        self.0.is_empty()
    }

    /// Returns the validated components in order.
    pub fn components(&self) -> impl Iterator<Item = EntryName> + '_ {
        self.0
            .split('/')
            .filter(|component| !component.is_empty())
            .map(|component| EntryName(component.to_owned()))
    }

    /// Returns the final component, if this is not the root path.
    pub fn file_name(&self) -> Option<EntryName> {
        self.0
            .rsplit('/')
            .next()
            .filter(|component| !component.is_empty())
            .map(|component| EntryName(component.to_owned()))
    }

    /// Returns the parent path, or root for a direct child of root.
    pub fn parent(&self) -> Self {
        match self.0.rfind('/') {
            Some(index) => Self(self.0[..index].to_owned()),
            None => Self::root(),
        }
    }

    /// Appends one validated component.
    pub fn join(&self, component: &EntryName) -> Result<Self, RelativePathError> {
        let mut value = self.0.clone();
        if !value.is_empty() {
            value.push('/');
        }
        value.push_str(component.as_str());
        Self::new(value)
    }
}

impl Default for RelativePath {
    fn default() -> Self {
        Self::root()
    }
}

impl AsRef<str> for RelativePath {
    fn as_ref(&self) -> &str {
        self.as_str()
    }
}

impl fmt::Display for RelativePath {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(self.as_str())
    }
}

impl TryFrom<&str> for RelativePath {
    type Error = RelativePathError;

    fn try_from(value: &str) -> Result<Self, Self::Error> {
        Self::new(value)
    }
}

impl TryFrom<String> for RelativePath {
    type Error = RelativePathError;

    fn try_from(value: String) -> Result<Self, Self::Error> {
        Self::new(value)
    }
}

/// Explains why a relative path is not normalized and safe.
#[non_exhaustive]
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum RelativePathError {
    /// A path component failed name validation.
    InvalidName(EntryNameError),
    /// The path exceeds [`MAX_TREE_PATH_BYTES`].
    TooLong,
    /// The path has an absolute prefix or trailing separator.
    AbsoluteOrTrailingSeparator,
    /// The path contains a backslash, NUL, or duplicate separator.
    InvalidSeparator,
}

impl fmt::Display for RelativePathError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::InvalidName(error) => error.fmt(formatter),
            Self::TooLong => {
                formatter.write_str("relative tree path exceeds the portable length limit")
            }
            Self::AbsoluteOrTrailingSeparator => {
                formatter.write_str("relative tree path must not be absolute or end in a separator")
            }
            Self::InvalidSeparator => {
                formatter.write_str("relative tree path contains an invalid or duplicate separator")
            }
        }
    }
}

impl std::error::Error for RelativePathError {}

/// A portable permission mode.
///
/// The value contains the POSIX permission and special bits (`0..=0o7777`)
/// without a platform-specific file-type bit.
#[derive(Clone, Copy, Debug, Default, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub struct FilePermissions(u32);

impl FilePermissions {
    /// The largest portable permission value.
    pub const MAX_MODE: u32 = 0o7777;

    /// Creates validated portable permission bits.
    pub const fn new(mode: u32) -> Result<Self, PermissionError> {
        if mode <= Self::MAX_MODE {
            Ok(Self(mode))
        } else {
            Err(PermissionError)
        }
    }

    /// Returns the permission and special bits.
    pub const fn mode(self) -> u32 {
        self.0
    }
}

/// A permission value outside the portable mode range.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct PermissionError;

impl fmt::Display for PermissionError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("portable permissions must fit in 12 bits")
    }
}

impl std::error::Error for PermissionError {}

/// A validated namespace for optional metadata extensions.
#[derive(Clone, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub struct MetadataNamespace(String);

impl MetadataNamespace {
    /// Creates a lowercase ASCII namespace such as `posix`, `windows`, or
    /// `macos`.
    pub fn new(value: impl Into<String>) -> Result<Self, MetadataNamespaceError> {
        let value = value.into();
        if value.is_empty() || value.len() > MAX_METADATA_NAMESPACE_BYTES {
            return Err(MetadataNamespaceError::InvalidSyntax);
        }
        if !value.bytes().all(|byte| {
            byte.is_ascii_lowercase() || byte.is_ascii_digit() || matches!(byte, b'.' | b'_' | b'-')
        }) {
            return Err(MetadataNamespaceError::InvalidSyntax);
        }
        Ok(Self(value))
    }

    /// Returns the canonical namespace identifier.
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for MetadataNamespace {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(self.as_str())
    }
}

impl TryFrom<&str> for MetadataNamespace {
    type Error = MetadataNamespaceError;

    fn try_from(value: &str) -> Result<Self, Self::Error> {
        Self::new(value)
    }
}

/// A malformed metadata namespace identifier.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum MetadataNamespaceError {
    /// The namespace is empty, too long, or contains a non-portable byte.
    InvalidSyntax,
}

impl fmt::Display for MetadataNamespaceError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("metadata namespace must be lowercase ASCII and path-safe")
    }
}

impl std::error::Error for MetadataNamespaceError {}

/// One optional, opaque metadata extension.
///
/// Extensions are ordered by namespace and version before encoding. A version
/// is required so a platform adapter can evolve its payload without changing
/// the meaning of an older namespace.
#[derive(Clone, Debug, Eq, Hash, PartialEq)]
pub struct MetadataExtension {
    namespace: MetadataNamespace,
    version: u16,
    value: Vec<u8>,
}

impl MetadataExtension {
    /// Creates one versioned extension value.
    pub fn new(
        namespace: MetadataNamespace,
        version: u16,
        value: impl AsRef<[u8]>,
    ) -> Result<Self, MetadataError> {
        if version == 0 {
            return Err(MetadataError::InvalidVersion);
        }
        let value = value.as_ref();
        if value.len() > MAX_METADATA_EXTENSION_BYTES {
            return Err(MetadataError::ValueTooLong);
        }
        Ok(Self {
            namespace,
            version,
            value: value.to_vec(),
        })
    }

    /// Returns the extension namespace.
    pub fn namespace(&self) -> &MetadataNamespace {
        &self.namespace
    }

    /// Returns the extension schema version.
    pub const fn version(&self) -> u16 {
        self.version
    }

    /// Returns the opaque extension bytes.
    pub fn value(&self) -> &[u8] {
        &self.value
    }
}

/// Portable metadata shared by directory, regular-file, and symlink nodes.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct PortableMetadata {
    permissions: FilePermissions,
    modified_at: Option<i64>,
    extensions: Arc<[MetadataExtension]>,
}

impl PortableMetadata {
    /// Creates metadata with required portable permission bits and no optional
    /// fields.
    pub fn new(permissions: FilePermissions) -> Self {
        Self {
            permissions,
            modified_at: None,
            extensions: Arc::from([]),
        }
    }

    /// Replaces the optional modification timestamp.
    ///
    /// The value is nanoseconds since the Unix epoch. It is kept signed so
    /// pre-epoch filesystem timestamps can round-trip.
    pub const fn with_modified_at(mut self, modified_at: i64) -> Self {
        self.modified_at = Some(modified_at);
        self
    }

    /// Adds one optional namespaced metadata extension.
    pub fn with_extension(
        mut self,
        namespace: MetadataNamespace,
        version: u16,
        value: impl AsRef<[u8]>,
    ) -> Result<Self, MetadataError> {
        let extension = MetadataExtension::new(namespace, version, value)?;
        let mut extensions = self.extensions.to_vec();
        extensions.push(extension);
        validate_and_sort_extensions(&mut extensions)?;
        self.extensions = Arc::from(extensions.into_boxed_slice());
        Ok(self)
    }

    /// Returns the portable permission bits.
    pub const fn permissions(&self) -> FilePermissions {
        self.permissions
    }

    /// Returns the optional modification timestamp.
    pub const fn modified_at(&self) -> Option<i64> {
        self.modified_at
    }

    /// Returns extensions in canonical namespace/version order.
    pub fn extensions(&self) -> &[MetadataExtension] {
        &self.extensions
    }
}

impl Default for PortableMetadata {
    fn default() -> Self {
        Self::new(FilePermissions::default())
    }
}

/// A validation failure in portable metadata.
#[non_exhaustive]
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum MetadataError {
    /// An extension schema version was zero.
    InvalidVersion,
    /// An extension value exceeds [`MAX_METADATA_EXTENSION_BYTES`].
    ValueTooLong,
    /// More than [`MAX_METADATA_EXTENSIONS`] extensions were supplied.
    TooManyExtensions,
    /// The same namespace/version pair was supplied more than once.
    DuplicateExtension,
}

impl fmt::Display for MetadataError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        let message = match self {
            Self::InvalidVersion => "metadata extension version must be non-zero",
            Self::ValueTooLong => "metadata extension value exceeds the portable limit",
            Self::TooManyExtensions => "node contains too many metadata extensions",
            Self::DuplicateExtension => "node contains a duplicate metadata namespace/version pair",
        };
        formatter.write_str(message)
    }
}

impl std::error::Error for MetadataError {}

/// A validated plaintext chunk reference in a regular file node.
#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub struct FileChunkReference {
    id: ChunkId,
    size: u64,
}

impl FileChunkReference {
    /// Creates a reference with its exact plaintext size.
    pub const fn new(id: ChunkId, size: u64) -> Result<Self, ChunkReferenceError> {
        if size == 0 {
            return Err(ChunkReferenceError::ZeroSize);
        }
        Ok(Self { id, size })
    }

    /// Returns the referenced chunk ID.
    pub const fn id(self) -> ChunkId {
        self.id
    }

    /// Returns the referenced plaintext size.
    pub const fn size(self) -> u64 {
        self.size
    }
}

/// Compatibility name for [`FileChunkReference`].
pub type ChunkReference = FileChunkReference;

/// Explains why a file chunk reference is invalid.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ChunkReferenceError {
    /// A chunk reference cannot have zero length.
    ZeroSize,
}

impl fmt::Display for ChunkReferenceError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("file chunk reference size must be greater than zero")
    }
}

impl std::error::Error for ChunkReferenceError {}

/// The raw target of a symbolic link.
///
/// Targets are intentionally not resolved, normalized, or followed. They may
/// contain separators and parent components because those bytes are part of
/// link semantics. NUL is rejected because it cannot be represented safely by
/// filesystem link APIs.
#[derive(Clone, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub struct SymlinkTarget(Vec<u8>);

impl SymlinkTarget {
    /// Creates a target from its raw bytes.
    pub fn new(value: impl AsRef<[u8]>) -> Result<Self, SymlinkTargetError> {
        let value = value.as_ref();
        if value.is_empty() {
            return Err(SymlinkTargetError::Empty);
        }
        if value.len() > MAX_SYMLINK_TARGET_BYTES {
            return Err(SymlinkTargetError::TooLong);
        }
        if value.contains(&0) {
            return Err(SymlinkTargetError::ContainsNul);
        }
        Ok(Self(value.to_vec()))
    }

    /// Returns the raw target bytes without following the link.
    pub fn as_bytes(&self) -> &[u8] {
        &self.0
    }

    /// Returns the target as UTF-8 when it is valid UTF-8.
    pub fn as_str(&self) -> Option<&str> {
        std::str::from_utf8(&self.0).ok()
    }
}

impl AsRef<[u8]> for SymlinkTarget {
    fn as_ref(&self) -> &[u8] {
        self.as_bytes()
    }
}

/// Explains why a symbolic-link target cannot be represented.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum SymlinkTargetError {
    /// The target is empty.
    Empty,
    /// The target exceeds [`MAX_SYMLINK_TARGET_BYTES`].
    TooLong,
    /// The target contains NUL.
    ContainsNul,
}

impl fmt::Display for SymlinkTargetError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        let message = match self {
            Self::Empty => "symbolic-link target must not be empty",
            Self::TooLong => "symbolic-link target exceeds the portable length limit",
            Self::ContainsNul => "symbolic-link target must not contain NUL",
        };
        formatter.write_str(message)
    }
}

impl std::error::Error for SymlinkTargetError {}

/// The kind recorded beside every directory entry reference.
#[non_exhaustive]
#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub enum TreeNodeKind {
    /// A directory node.
    Directory,
    /// A regular-file node.
    RegularFile,
    /// A symbolic-link node.
    SymbolicLink,
}

impl TreeNodeKind {
    /// Returns the canonical wire discriminator.
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Directory => "directory",
            Self::RegularFile => "file",
            Self::SymbolicLink => "symlink",
        }
    }

    /// Parses a canonical or historical spelling.
    pub fn parse(value: &str) -> Option<Self> {
        match value {
            "directory" => Some(Self::Directory),
            "file" | "regular_file" => Some(Self::RegularFile),
            "symlink" | "symbolic_link" => Some(Self::SymbolicLink),
            _ => None,
        }
    }

    /// Returns whether this kind is a directory.
    pub const fn is_directory(self) -> bool {
        matches!(self, Self::Directory)
    }
}

impl fmt::Display for TreeNodeKind {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(self.as_str())
    }
}

/// A content-addressed reference to one immutable tree node.
#[derive(Clone, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub struct TreeNodeReference {
    id: ObjectId,
    kind: TreeNodeKind,
}

impl TreeNodeReference {
    /// Creates a reference to an existing tree object.
    pub fn new(id: ObjectId, kind: TreeNodeKind) -> Self {
        Self { id, kind }
    }

    /// Creates a directory reference.
    pub fn directory(id: ObjectId) -> Self {
        Self::new(id, TreeNodeKind::Directory)
    }

    /// Creates a regular-file reference.
    pub fn regular_file(id: ObjectId) -> Self {
        Self::new(id, TreeNodeKind::RegularFile)
    }

    /// Creates a symbolic-link reference.
    pub fn symbolic_link(id: ObjectId) -> Self {
        Self::new(id, TreeNodeKind::SymbolicLink)
    }

    /// Returns the immutable object ID.
    pub fn id(&self) -> &ObjectId {
        &self.id
    }

    /// Alias for [`Self::id`] using node terminology.
    pub fn node_id(&self) -> &ObjectId {
        self.id()
    }

    /// Returns the expected node kind.
    pub const fn kind(&self) -> TreeNodeKind {
        self.kind
    }

    /// Returns the conventional tree-object storage reference.
    pub fn object_reference(&self) -> Result<RepositoryObject, DomainError> {
        self.id.object_reference(ObjectKind::Tree)
    }
}

impl From<ObjectId> for TreeNodeReference {
    fn from(value: ObjectId) -> Self {
        Self::directory(value)
    }
}

/// One canonically ordered directory entry.
#[derive(Clone, Debug, Eq, Hash, PartialEq)]
pub struct TreeEntry {
    name: EntryName,
    reference: TreeNodeReference,
}

impl TreeEntry {
    /// Creates an entry after validating its name.
    pub fn new(
        name: impl Into<String>,
        reference: TreeNodeReference,
    ) -> Result<Self, TreeValidationError> {
        let name = EntryName::new(name).map_err(TreeValidationError::InvalidName)?;
        Ok(Self { name, reference })
    }

    /// Returns the validated entry name.
    pub fn name(&self) -> &EntryName {
        &self.name
    }

    /// Returns the referenced child object.
    pub fn reference(&self) -> &TreeNodeReference {
        &self.reference
    }

    /// Returns the child object ID.
    pub fn node_id(&self) -> &ObjectId {
        self.reference.id()
    }

    /// Returns the child kind recorded in this entry.
    pub const fn kind(&self) -> TreeNodeKind {
        self.reference.kind()
    }
}

/// Compatibility name for [`TreeEntry`].
pub type DirectoryEntry = TreeEntry;

/// A directory node containing sorted, unique child entries.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct DirectoryNode {
    metadata: PortableMetadata,
    entries: Arc<[TreeEntry]>,
}

impl DirectoryNode {
    /// Creates a directory and canonicalizes entry order.
    pub fn new<I>(metadata: PortableMetadata, entries: I) -> Result<Self, TreeValidationError>
    where
        I: IntoIterator<Item = TreeEntry>,
    {
        let mut entries: Vec<_> = entries.into_iter().collect();
        if entries.len() > MAX_TREE_ENTRIES {
            return Err(TreeValidationError::TooManyEntries);
        }
        entries.sort_unstable_by(|left, right| left.name.cmp(&right.name));
        if entries
            .windows(2)
            .any(|window| window[0].name == window[1].name)
        {
            return Err(TreeValidationError::DuplicateEntryName);
        }
        Ok(Self {
            metadata,
            entries: Arc::from(entries.into_boxed_slice()),
        })
    }

    /// Creates an empty directory.
    pub fn empty(metadata: PortableMetadata) -> Self {
        Self {
            metadata,
            entries: Arc::from([]),
        }
    }

    /// Returns directory metadata.
    pub fn metadata(&self) -> &PortableMetadata {
        &self.metadata
    }

    /// Returns entries in canonical name order.
    pub fn entries(&self) -> &[TreeEntry] {
        &self.entries
    }

    /// Looks up one direct child without loading any descendant.
    pub fn entry(&self, name: &EntryName) -> Option<&TreeEntry> {
        self.entries
            .binary_search_by(|entry| entry.name.cmp(name))
            .ok()
            .map(|index| &self.entries[index])
    }

    /// Returns the direct child reference for one validated name.
    pub fn child(&self, name: &EntryName) -> Option<&TreeNodeReference> {
        self.entry(name).map(TreeEntry::reference)
    }
}

/// A regular-file node with ordered chunk references and exact size.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct RegularFileNode {
    size: u64,
    chunks: Arc<[FileChunkReference]>,
    metadata: PortableMetadata,
}

impl RegularFileNode {
    /// Creates a file after checking that chunk sizes sum exactly to `size`.
    pub fn new<I>(
        size: u64,
        chunks: I,
        metadata: PortableMetadata,
    ) -> Result<Self, TreeValidationError>
    where
        I: IntoIterator<Item = FileChunkReference>,
    {
        let chunks: Vec<_> = chunks.into_iter().collect();
        if chunks.len() > MAX_FILE_CHUNK_REFERENCES {
            return Err(TreeValidationError::TooManyChunks);
        }
        let referenced_size = chunks.iter().try_fold(0_u64, |total, chunk| {
            total
                .checked_add(chunk.size())
                .ok_or(TreeValidationError::FileSizeOverflow)
        })?;
        if referenced_size != size {
            return Err(TreeValidationError::FileSizeMismatch {
                declared: size,
                referenced: referenced_size,
            });
        }
        Ok(Self {
            size,
            chunks: Arc::from(chunks.into_boxed_slice()),
            metadata,
        })
    }

    /// Creates a file whose size is derived from its chunk references.
    pub fn from_chunks<I>(
        chunks: I,
        metadata: PortableMetadata,
    ) -> Result<Self, TreeValidationError>
    where
        I: IntoIterator<Item = FileChunkReference>,
    {
        let chunks: Vec<_> = chunks.into_iter().collect();
        let size = chunks.iter().try_fold(0_u64, |total, chunk| {
            total
                .checked_add(chunk.size())
                .ok_or(TreeValidationError::FileSizeOverflow)
        })?;
        Self::new(size, chunks, metadata)
    }

    /// Returns the exact logical file size.
    pub const fn size(&self) -> u64 {
        self.size
    }

    /// Returns ordered chunk references.
    pub fn chunks(&self) -> &[FileChunkReference] {
        &self.chunks
    }

    /// Returns file metadata.
    pub fn metadata(&self) -> &PortableMetadata {
        &self.metadata
    }
}

/// A symbolic-link node that stores its raw target.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct SymbolicLinkNode {
    target: SymlinkTarget,
    metadata: PortableMetadata,
}

impl SymbolicLinkNode {
    /// Creates a symbolic-link node without resolving its target.
    pub fn new(target: SymlinkTarget, metadata: PortableMetadata) -> Self {
        Self { target, metadata }
    }

    /// Returns the raw target.
    pub fn target(&self) -> &SymlinkTarget {
        &self.target
    }

    /// Returns symlink metadata.
    pub fn metadata(&self) -> &PortableMetadata {
        &self.metadata
    }
}

/// Compatibility name for [`RegularFileNode`].
pub type FileNode = RegularFileNode;

/// Compatibility name for [`SymbolicLinkNode`].
pub type SymlinkNode = SymbolicLinkNode;

/// One immutable node in a snapshot tree.
#[non_exhaustive]
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum TreeNode {
    /// A directory and its direct child references.
    Directory(DirectoryNode),
    /// A regular file and its ordered chunk references.
    RegularFile(RegularFileNode),
    /// A symbolic link and its uninterpreted target.
    SymbolicLink(SymbolicLinkNode),
}

impl TreeNode {
    /// Returns the node kind.
    pub const fn kind(&self) -> TreeNodeKind {
        match self {
            Self::Directory(_) => TreeNodeKind::Directory,
            Self::RegularFile(_) => TreeNodeKind::RegularFile,
            Self::SymbolicLink(_) => TreeNodeKind::SymbolicLink,
        }
    }

    /// Returns the node metadata.
    pub fn metadata(&self) -> &PortableMetadata {
        match self {
            Self::Directory(node) => node.metadata(),
            Self::RegularFile(node) => node.metadata(),
            Self::SymbolicLink(node) => node.metadata(),
        }
    }

    /// Returns the directory payload when this is a directory.
    pub fn as_directory(&self) -> Option<&DirectoryNode> {
        match self {
            Self::Directory(node) => Some(node),
            Self::RegularFile(_) | Self::SymbolicLink(_) => None,
        }
    }

    /// Returns the regular-file payload when this is a regular file.
    pub fn as_regular_file(&self) -> Option<&RegularFileNode> {
        match self {
            Self::RegularFile(node) => Some(node),
            Self::Directory(_) | Self::SymbolicLink(_) => None,
        }
    }

    /// Returns the symbolic-link payload when this is a symbolic link.
    pub fn as_symbolic_link(&self) -> Option<&SymbolicLinkNode> {
        match self {
            Self::SymbolicLink(node) => Some(node),
            Self::Directory(_) | Self::RegularFile(_) => None,
        }
    }
}

/// A validation failure while constructing a tree node.
#[non_exhaustive]
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum TreeValidationError {
    /// A directory entry name failed validation.
    InvalidName(EntryNameError),
    /// A directory contains the same name more than once.
    DuplicateEntryName,
    /// A directory exceeds [`MAX_TREE_ENTRIES`].
    TooManyEntries,
    /// A file exceeds [`MAX_FILE_CHUNK_REFERENCES`].
    TooManyChunks,
    /// File chunk sizes overflowed their declared total.
    FileSizeOverflow,
    /// File chunk sizes do not equal the declared file size.
    FileSizeMismatch {
        /// The size recorded by the file node.
        declared: u64,
        /// The sum of referenced chunk sizes.
        referenced: u64,
    },
}

impl fmt::Display for TreeValidationError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::InvalidName(error) => error.fmt(formatter),
            Self::DuplicateEntryName => {
                formatter.write_str("directory contains a duplicate entry name")
            }
            Self::TooManyEntries => formatter.write_str("directory contains too many entries"),
            Self::TooManyChunks => {
                formatter.write_str("regular file contains too many chunk references")
            }
            Self::FileSizeOverflow => {
                formatter.write_str("regular-file chunk sizes overflow the file size")
            }
            Self::FileSizeMismatch {
                declared,
                referenced,
            } => write!(
                formatter,
                "regular-file size {declared} does not equal referenced chunk size {referenced}"
            ),
        }
    }
}

impl std::error::Error for TreeValidationError {}

/// A source of immutable tree nodes for lazy lookup and traversal.
///
/// Implementations must return the node addressed by `reference`, verify the
/// content-addressed ID, and reject a decoded node whose kind differs from the
/// reference. The built-in repository adapter performs all three checks.
pub trait TreeNodeStore {
    /// The storage or decoding error returned by this source.
    type Error;

    /// Loads exactly one node and no descendants.
    fn load(&self, reference: &TreeNodeReference) -> Result<TreeNode, Self::Error>;
}

impl<T> TreeNodeStore for Arc<T>
where
    T: TreeNodeStore + ?Sized,
{
    type Error = T::Error;

    fn load(&self, reference: &TreeNodeReference) -> Result<TreeNode, Self::Error> {
        self.as_ref().load(reference)
    }
}

/// A lazy view rooted at one directory reference.
pub struct LazyTree<S> {
    root: TreeNodeReference,
    store: S,
}

impl<S> LazyTree<S>
where
    S: TreeNodeStore,
{
    /// Creates a lazy tree whose root object is a directory. An [`ObjectId`]
    /// is interpreted as a directory reference; callers that already have an
    /// expected kind can pass a [`TreeNodeReference`].
    pub fn new(root: impl Into<TreeNodeReference>, store: S) -> Self {
        Self {
            root: root.into(),
            store,
        }
    }

    /// Creates a lazy tree from an explicit root reference.
    pub fn from_reference(
        root: TreeNodeReference,
        store: S,
    ) -> Result<Self, TreeTraversalError<S::Error>> {
        if !root.kind().is_directory() {
            return Err(TreeTraversalError::InvalidRootKind {
                actual: root.kind(),
            });
        }
        Ok(Self { root, store })
    }

    /// Returns the root reference without loading it.
    pub fn root_reference(&self) -> &TreeNodeReference {
        &self.root
    }

    /// Returns the root object ID without loading it.
    pub fn root_id(&self) -> &ObjectId {
        self.root.id()
    }

    /// Loads only the root node.
    pub fn root_node(&self) -> Result<TreeNode, TreeTraversalError<S::Error>> {
        let node = self
            .store
            .load(&self.root)
            .map_err(TreeTraversalError::Store)?;
        validate_loaded_kind(&self.root, &node)?;
        Ok(node)
    }

    /// Looks up one path while loading only its ancestor chain.
    pub fn lookup(
        &self,
        path: &RelativePath,
    ) -> Result<Option<TreeNode>, TreeTraversalError<S::Error>> {
        let mut node = self.root_node()?;
        if path.is_root() {
            return Ok(Some(node));
        }
        let components: Vec<_> = path.components().collect();
        for (index, component) in components.iter().enumerate() {
            let Some(directory) = node.as_directory() else {
                return Ok(None);
            };
            let Some(entry) = directory.entry(component) else {
                return Ok(None);
            };
            let reference = entry.reference().clone();
            node = self
                .store
                .load(&reference)
                .map_err(TreeTraversalError::Store)?;
            validate_loaded_kind(&reference, &node)?;
            if index + 1 == components.len() {
                return Ok(Some(node));
            }
        }
        Ok(None)
    }

    /// Returns a depth-first iterator that loads one node at a time.
    pub fn walk(&self) -> TreeWalker<'_, S> {
        TreeWalker {
            tree: self,
            root_pending: true,
            stack: Vec::new(),
            active: BTreeSet::new(),
            finished: false,
        }
    }
}

/// One item yielded by a lazy depth-first tree walk.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct TreeWalkEntry {
    path: RelativePath,
    reference: TreeNodeReference,
}

impl TreeWalkEntry {
    /// Returns the normalized path of this node.
    pub fn path(&self) -> &RelativePath {
        &self.path
    }

    /// Returns the node reference without loading descendants.
    pub fn reference(&self) -> &TreeNodeReference {
        &self.reference
    }

    /// Returns the node kind.
    pub const fn kind(&self) -> TreeNodeKind {
        self.reference.kind()
    }
}

struct WalkFrame {
    path: RelativePath,
    reference: TreeNodeReference,
    directory: DirectoryNode,
    next_entry: usize,
}

/// The bounded-memory iterator returned by [`LazyTree::walk`].
pub struct TreeWalker<'a, S>
where
    S: TreeNodeStore,
{
    tree: &'a LazyTree<S>,
    root_pending: bool,
    stack: Vec<WalkFrame>,
    active: BTreeSet<ObjectId>,
    finished: bool,
}

impl<S> Iterator for TreeWalker<'_, S>
where
    S: TreeNodeStore,
{
    type Item = Result<TreeWalkEntry, TreeTraversalError<S::Error>>;

    fn next(&mut self) -> Option<Self::Item> {
        if self.finished {
            return None;
        }
        if self.root_pending {
            self.root_pending = false;
            let root = self.tree.root.clone();
            if !self.active.insert(root.id().clone()) {
                self.finished = true;
                return Some(Err(TreeTraversalError::Cycle));
            }
            let node = match self.tree.store.load(&root) {
                Ok(node) => node,
                Err(error) => {
                    self.finished = true;
                    return Some(Err(TreeTraversalError::Store(error)));
                }
            };
            if let Err(error) = validate_loaded_kind(&root, &node) {
                self.finished = true;
                return Some(Err(error));
            }
            let TreeNode::Directory(directory) = node else {
                self.finished = true;
                return Some(Err(TreeTraversalError::InvalidRootKind {
                    actual: root.kind(),
                }));
            };
            self.stack.push(WalkFrame {
                path: RelativePath::root(),
                reference: root.clone(),
                directory,
                next_entry: 0,
            });
            return Some(Ok(TreeWalkEntry {
                path: RelativePath::root(),
                reference: root,
            }));
        }

        loop {
            let Some(frame) = self.stack.last_mut() else {
                self.finished = true;
                return None;
            };
            if frame.next_entry == frame.directory.entries().len() {
                let id = frame.reference.id().clone();
                self.stack.pop();
                self.active.remove(&id);
                continue;
            }

            let entry = frame.directory.entries()[frame.next_entry].clone();
            frame.next_entry += 1;
            let child_reference = entry.reference().clone();
            if self.active.contains(child_reference.id()) {
                self.finished = true;
                return Some(Err(TreeTraversalError::Cycle));
            }
            let child = match self.tree.store.load(&child_reference) {
                Ok(child) => child,
                Err(error) => {
                    self.finished = true;
                    return Some(Err(TreeTraversalError::Store(error)));
                }
            };
            if let Err(error) = validate_loaded_kind(&child_reference, &child) {
                self.finished = true;
                return Some(Err(error));
            }
            let path = match frame.path.join(entry.name()) {
                Ok(path) => path,
                Err(error) => {
                    self.finished = true;
                    return Some(Err(TreeTraversalError::InvalidPath(error)));
                }
            };
            if let TreeNode::Directory(directory) = child {
                self.active.insert(child_reference.id().clone());
                self.stack.push(WalkFrame {
                    path: path.clone(),
                    reference: child_reference.clone(),
                    directory,
                    next_entry: 0,
                });
            }
            return Some(Ok(TreeWalkEntry {
                path,
                reference: child_reference,
            }));
        }
    }
}

/// An error encountered while lazily loading or validating a tree graph.
#[non_exhaustive]
#[derive(Debug, Eq, PartialEq)]
pub enum TreeTraversalError<E> {
    /// The node source failed.
    Store(E),
    /// The root reference did not identify a directory.
    InvalidRootKind {
        /// The kind recorded by the root reference.
        actual: TreeNodeKind,
    },
    /// A reference and loaded node disagree about kind.
    NodeKindMismatch {
        /// The kind recorded in the reference.
        expected: TreeNodeKind,
        /// The kind returned by the source.
        actual: TreeNodeKind,
    },
    /// A directory graph points to one of its active ancestors.
    Cycle,
    /// A generated path exceeded the portable path limit.
    InvalidPath(RelativePathError),
}

impl<E: fmt::Display> fmt::Display for TreeTraversalError<E> {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Store(error) => write!(formatter, "tree node source failed: {error}"),
            Self::InvalidRootKind { actual } => {
                write!(formatter, "tree root must be a directory, got {actual}")
            }
            Self::NodeKindMismatch { expected, actual } => write!(
                formatter,
                "tree reference declares {expected}, loaded node is {actual}"
            ),
            Self::Cycle => formatter.write_str("tree graph contains a cycle"),
            Self::InvalidPath(error) => error.fmt(formatter),
        }
    }
}

impl<E: std::error::Error + 'static> std::error::Error for TreeTraversalError<E> {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Store(error) => Some(error),
            Self::InvalidPath(error) => Some(error),
            Self::InvalidRootKind { .. } | Self::NodeKindMismatch { .. } | Self::Cycle => None,
        }
    }
}

fn validate_loaded_kind<S>(
    reference: &TreeNodeReference,
    node: &TreeNode,
) -> Result<(), TreeTraversalError<S>> {
    if reference.kind() != node.kind() {
        return Err(TreeTraversalError::NodeKindMismatch {
            expected: reference.kind(),
            actual: node.kind(),
        });
    }
    Ok(())
}

fn validate_and_sort_extensions(extensions: &mut [MetadataExtension]) -> Result<(), MetadataError> {
    if extensions.len() > MAX_METADATA_EXTENSIONS {
        return Err(MetadataError::TooManyExtensions);
    }
    extensions.sort_unstable_by(|left, right| {
        left.namespace
            .cmp(&right.namespace)
            .then(left.version.cmp(&right.version))
    });
    if extensions.windows(2).any(|window| {
        window[0].namespace == window[1].namespace && window[0].version == window[1].version
    }) {
        return Err(MetadataError::DuplicateExtension);
    }
    Ok(())
}

fn is_windows_reserved_name(value: &str) -> bool {
    let stem = value.split_once('.').map_or(value, |(stem, _)| stem);
    let uppercase = stem.to_ascii_uppercase();
    matches!(uppercase.as_str(), "CON" | "PRN" | "AUX" | "NUL")
        || (uppercase.len() == 4
            && (uppercase.starts_with("COM") || uppercase.starts_with("LPT"))
            && uppercase.as_bytes()[3].is_ascii_digit()
            && uppercase.as_bytes()[3] != b'0')
}

/// Compatibility alias for [`EntryName`].
pub type ValidatedName = EntryName;

/// Compatibility alias for [`EntryName`].
pub type Name = EntryName;

/// Compatibility alias for [`RelativePath`].
pub type NormalizedRelativePath = RelativePath;

/// Compatibility alias for [`TreeNodeKind`].
pub type NodeKind = TreeNodeKind;

/// The content ID of an immutable tree node.
pub type TreeNodeId = ObjectId;

/// Compatibility alias for [`TreeNodeReference`].
pub type NodeReference = TreeNodeReference;
