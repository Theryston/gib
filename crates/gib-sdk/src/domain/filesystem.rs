use super::tree::{
    EntryName, EntryNameError, FilePermissions, RelativePath, RelativePathError, SymlinkTarget,
    SymlinkTargetError,
};
use std::fmt;

/// The largest number of simultaneously open directory enumerators allowed by
/// the default filesystem scanner.
///
/// The bound is derived from the portable path limit: a valid path needs at
/// least one byte for each component and one byte for each separator. A scan
/// cannot therefore create an unbounded directory stack while emitting valid
/// paths.
pub const MAX_FILESYSTEM_SCAN_OPEN_DIRECTORIES: usize = super::tree::MAX_TREE_PATH_BYTES / 2 + 2;

/// The kind of one filesystem entry, classified without following a link.
#[non_exhaustive]
#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub enum FilesystemEntryKind {
    /// A directory that may be enumerated.
    Directory,
    /// A regular file whose contents may be read.
    RegularFile,
    /// A symbolic link or link-like reparse point.
    SymbolicLink,
    /// A special filesystem object that is preserved as an entry but is not
    /// read or traversed by the scanner.
    Other,
}

impl FilesystemEntryKind {
    /// Returns whether this kind is a directory.
    pub const fn is_directory(self) -> bool {
        matches!(self, Self::Directory)
    }

    /// Returns whether this kind is a regular file.
    pub const fn is_regular_file(self) -> bool {
        matches!(self, Self::RegularFile)
    }

    /// Returns whether this kind is a symbolic link.
    pub const fn is_symbolic_link(self) -> bool {
        matches!(self, Self::SymbolicLink)
    }

    /// Returns the stable display value for this kind.
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Directory => "directory",
            Self::RegularFile => "file",
            Self::SymbolicLink => "symlink",
            Self::Other => "other",
        }
    }
}

impl fmt::Display for FilesystemEntryKind {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(self.as_str())
    }
}

/// Best-effort filesystem identity values used to detect replacement races.
///
/// Unix adapters normally populate the device and inode values. Windows
/// adapters normally populate the volume serial number and file index. A
/// field may be absent on a filesystem that does not expose that hint; callers
/// must then rely on the remaining metadata checks.
#[derive(Clone, Copy, Debug, Default, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub struct FilesystemIdentity {
    volume_id: Option<u64>,
    file_id: Option<u64>,
}

impl FilesystemIdentity {
    /// Creates an identity from provider-neutral volume and file hints.
    pub const fn new(volume_id: Option<u64>, file_id: Option<u64>) -> Self {
        Self { volume_id, file_id }
    }

    /// Returns the volume or device hint, when available.
    pub const fn volume_id(self) -> Option<u64> {
        self.volume_id
    }

    /// Returns the file or inode hint, when available.
    pub const fn file_id(self) -> Option<u64> {
        self.file_id
    }

    /// Returns whether both identity components are available.
    pub const fn is_complete(self) -> bool {
        self.volume_id.is_some() && self.file_id.is_some()
    }

    /// Compares all identity components that both observations provide.
    ///
    /// A missing component does not by itself prove a change. The scanner
    /// combines this result with size and timestamp observations.
    pub const fn matches_observation(self, other: Self) -> bool {
        match (self.volume_id, other.volume_id) {
            (Some(left), Some(right)) if left != right => return false,
            _ => {}
        }
        match (self.file_id, other.file_id) {
            (Some(left), Some(right)) if left != right => return false,
            _ => {}
        }
        true
    }
}

/// Filesystem metadata captured without following a symbolic link.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct FilesystemMetadata {
    kind: FilesystemEntryKind,
    size: u64,
    permissions: Option<FilePermissions>,
    modified_at: Option<i64>,
    created_at: Option<i64>,
    accessed_at: Option<i64>,
    identity: FilesystemIdentity,
}

impl FilesystemMetadata {
    /// Creates metadata with the required kind and byte size.
    pub const fn new(kind: FilesystemEntryKind, size: u64) -> Self {
        Self {
            kind,
            size,
            permissions: None,
            modified_at: None,
            created_at: None,
            accessed_at: None,
            identity: FilesystemIdentity::new(None, None),
        }
    }

    /// Sets the supported portable permission bits.
    pub const fn with_permissions(mut self, permissions: FilePermissions) -> Self {
        self.permissions = Some(permissions);
        self
    }

    /// Sets the modification timestamp as signed Unix-epoch nanoseconds.
    pub const fn with_modified_at(mut self, modified_at: i64) -> Self {
        self.modified_at = Some(modified_at);
        self
    }

    /// Sets the creation timestamp as signed Unix-epoch nanoseconds.
    pub const fn with_created_at(mut self, created_at: i64) -> Self {
        self.created_at = Some(created_at);
        self
    }

    /// Sets the last-access timestamp as signed Unix-epoch nanoseconds.
    pub const fn with_accessed_at(mut self, accessed_at: i64) -> Self {
        self.accessed_at = Some(accessed_at);
        self
    }

    /// Sets the provider-neutral identity hints.
    pub const fn with_identity(mut self, identity: FilesystemIdentity) -> Self {
        self.identity = identity;
        self
    }

    /// Returns the entry kind.
    pub const fn kind(&self) -> FilesystemEntryKind {
        self.kind
    }

    /// Returns the entry size in bytes.
    pub const fn size(&self) -> u64 {
        self.size
    }

    /// Returns portable permissions when the platform supports them.
    pub const fn permissions(&self) -> Option<FilePermissions> {
        self.permissions
    }

    /// Returns the modification timestamp, when supported.
    pub const fn modified_at(&self) -> Option<i64> {
        self.modified_at
    }

    /// Returns the creation timestamp, when supported.
    pub const fn created_at(&self) -> Option<i64> {
        self.created_at
    }

    /// Returns the last-access timestamp, when supported.
    pub const fn accessed_at(&self) -> Option<i64> {
        self.accessed_at
    }

    /// Returns provider-neutral identity hints.
    pub const fn identity(&self) -> FilesystemIdentity {
        self.identity
    }
}

/// A validated filesystem entry emitted by a scanner.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct FilesystemEntry {
    path: RelativePath,
    metadata: FilesystemMetadata,
    symlink_target: Option<SymlinkTarget>,
    observed_at: i64,
}

impl FilesystemEntry {
    /// Creates an entry after validating its path and link-target shape.
    pub fn new(
        path: RelativePath,
        metadata: FilesystemMetadata,
        symlink_target: Option<SymlinkTarget>,
        observed_at: i64,
    ) -> Result<Self, FilesystemEntryError> {
        match (metadata.kind(), symlink_target.is_some()) {
            (FilesystemEntryKind::SymbolicLink, false) => {
                return Err(FilesystemEntryError::MissingSymlinkTarget);
            }
            (
                FilesystemEntryKind::Directory
                | FilesystemEntryKind::RegularFile
                | FilesystemEntryKind::Other,
                true,
            ) => {
                return Err(FilesystemEntryError::UnexpectedSymlinkTarget);
            }
            _ => {}
        }
        Ok(Self {
            path,
            metadata,
            symlink_target,
            observed_at,
        })
    }

    /// Returns the normalized path relative to the selected source root.
    pub fn path(&self) -> &RelativePath {
        &self.path
    }

    /// Returns the final validated name, or `None` for the root entry.
    pub fn name(&self) -> Option<EntryName> {
        self.path.file_name()
    }

    /// Returns the captured metadata.
    pub fn metadata(&self) -> &FilesystemMetadata {
        &self.metadata
    }

    /// Returns the entry kind.
    pub const fn kind(&self) -> FilesystemEntryKind {
        self.metadata.kind()
    }

    /// Returns the raw symbolic-link target without resolving it.
    pub fn symlink_target(&self) -> Option<&SymlinkTarget> {
        self.symlink_target.as_ref()
    }

    /// Returns the scanner clock value at which this entry was observed.
    pub const fn observed_at(&self) -> i64 {
        self.observed_at
    }

    /// Returns whether this entry is the selected root directory.
    pub const fn is_root(&self) -> bool {
        self.path.is_root()
    }
}

/// A malformed combination of filesystem entry fields.
#[non_exhaustive]
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum FilesystemEntryError {
    /// A symbolic link did not provide its raw target.
    MissingSymlinkTarget,
    /// A non-link entry carried a symbolic-link target.
    UnexpectedSymlinkTarget,
}

impl fmt::Display for FilesystemEntryError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        let message = match self {
            Self::MissingSymlinkTarget => "symbolic-link entry is missing its raw target",
            Self::UnexpectedSymlinkTarget => "non-link entry contains a symbolic-link target",
        };
        formatter.write_str(message)
    }
}

impl std::error::Error for FilesystemEntryError {}

/// The filesystem operation associated with an adapter failure.
#[non_exhaustive]
#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub enum FilesystemOperation {
    /// Inspect an entry with no-follow metadata.
    SymlinkMetadata,
    /// Open a directory for incremental enumeration.
    ReadDirectory,
    /// Inspect the directory handle used for enumeration.
    DirectoryMetadata,
    /// Read one symbolic-link target.
    ReadLink,
    /// Open a regular file for verified reading.
    OpenFile,
    /// Read bytes from an opened regular file.
    ReadFile,
    /// Read metadata from an opened file handle.
    FileHandleMetadata,
}

impl FilesystemOperation {
    /// Returns the stable display value for this operation.
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::SymlinkMetadata => "symlink_metadata",
            Self::ReadDirectory => "read_directory",
            Self::DirectoryMetadata => "directory_metadata",
            Self::ReadLink => "read_link",
            Self::OpenFile => "open_file",
            Self::ReadFile => "read_file",
            Self::FileHandleMetadata => "file_handle_metadata",
        }
    }
}

impl fmt::Display for FilesystemOperation {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(self.as_str())
    }
}

/// A provider-neutral subset of I/O failure categories.
#[non_exhaustive]
#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub enum FilesystemErrorKind {
    /// The path disappeared.
    NotFound,
    /// Access was denied.
    PermissionDenied,
    /// The path or operation was invalid.
    InvalidInput,
    /// The path names a non-directory where a directory was required.
    NotADirectory,
    /// The process could not obtain another directory or file handle.
    TooManyOpenFiles,
    /// The operation was interrupted or temporarily unavailable.
    Transient,
    /// The platform does not support the requested operation.
    Unsupported,
    /// Another adapter failure category.
    Other,
}

impl FilesystemErrorKind {
    /// Returns whether the failure is a permission failure.
    pub const fn is_permission_denied(self) -> bool {
        matches!(self, Self::PermissionDenied)
    }

    /// Returns whether retrying the same low-level operation may help.
    pub const fn is_retryable(self) -> bool {
        matches!(self, Self::Transient)
    }
}

impl fmt::Display for FilesystemErrorKind {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        let message = match self {
            Self::NotFound => "not found",
            Self::PermissionDenied => "permission denied",
            Self::InvalidInput => "invalid input",
            Self::NotADirectory => "not a directory",
            Self::TooManyOpenFiles => "too many open files",
            Self::Transient => "transient I/O failure",
            Self::Unsupported => "unsupported filesystem operation",
            Self::Other => "I/O failure",
        };
        formatter.write_str(message)
    }
}

/// The phase in which a file replacement or mutation was detected.
#[non_exhaustive]
#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub enum FilesystemChangePhase {
    /// The entry changed between discovery and opening.
    BeforeOpen,
    /// The opened file changed before the expected end was reached.
    DuringRead,
    /// The final handle or path check disagreed with discovery metadata.
    AfterRead,
}

impl FilesystemChangePhase {
    /// Returns the stable display value for this phase.
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::BeforeOpen => "before_open",
            Self::DuringRead => "during_read",
            Self::AfterRead => "after_read",
        }
    }
}

impl fmt::Display for FilesystemChangePhase {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(self.as_str())
    }
}

/// The observable reason for reporting a filesystem race.
#[non_exhaustive]
#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub enum FilesystemChangeReason {
    /// The entry changed kind.
    TypeChanged,
    /// Provider identity hints changed.
    IdentityChanged,
    /// The byte size changed or the read length was not exact.
    SizeChanged,
    /// The modification timestamp changed.
    ModifiedTimeChanged,
    /// The path now resolves to a different object.
    PathReplaced,
}

impl FilesystemChangeReason {
    /// Returns the stable display value for this reason.
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::TypeChanged => "type_changed",
            Self::IdentityChanged => "identity_changed",
            Self::SizeChanged => "size_changed",
            Self::ModifiedTimeChanged => "modified_time_changed",
            Self::PathReplaced => "path_replaced",
        }
    }
}

impl fmt::Display for FilesystemChangeReason {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(self.as_str())
    }
}

/// A visible failure encountered while discovering or verifying a filesystem
/// entry.
#[non_exhaustive]
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum FilesystemScanError {
    /// The selected root could not be inspected.
    RootIo {
        /// The failed operation.
        operation: FilesystemOperation,
        /// The provider-neutral failure category.
        kind: FilesystemErrorKind,
    },
    /// The selected root is not a directory.
    RootNotDirectory,
    /// The selected root is a symbolic link or link-like reparse point.
    RootIsSymbolicLink,
    /// An operation on a relative entry path failed.
    Io {
        /// The normalized path associated with the operation.
        path: RelativePath,
        /// The failed operation.
        operation: FilesystemOperation,
        /// The provider-neutral failure category.
        kind: FilesystemErrorKind,
    },
    /// A directory changed identity while it was being enumerated.
    DirectoryChanged {
        /// The normalized directory path.
        path: RelativePath,
        /// The observable race reason.
        reason: FilesystemChangeReason,
    },
    /// A directory or its descendant would exceed the configured open-handle
    /// bound.
    OpenDirectoryLimit {
        /// The directory that could not be descended into.
        path: RelativePath,
        /// The configured limit.
        limit: usize,
    },
    /// A directory entry name cannot be represented by the portable path
    /// contract.
    InvalidEntryName {
        /// The parent directory of the invalid name.
        parent: RelativePath,
        /// The name validation error.
        reason: EntryNameError,
    },
    /// A generated relative path is not portable or exceeds its bound.
    InvalidEntryPath {
        /// The parent directory used to construct the path.
        parent: RelativePath,
        /// The path validation error.
        reason: RelativePathError,
    },
    /// A directory name was not valid UTF-8 and cannot be persisted by the
    /// portable tree contract.
    NonUtf8EntryName {
        /// The parent directory of the unrepresentable name.
        parent: RelativePath,
    },
    /// A symbolic-link target could not be read without resolving the link.
    InvalidSymlinkTarget {
        /// The normalized link path.
        path: RelativePath,
        /// The target validation error.
        reason: SymlinkTargetError,
    },
    /// An entry could not be assembled from the adapter's metadata.
    InvalidEntry {
        /// The normalized path of the malformed entry.
        path: RelativePath,
        /// The entry-shape error.
        reason: FilesystemEntryError,
    },
    /// The entry kind is not suitable for a requested regular-file read.
    InvalidFileKind {
        /// The normalized path passed to the read operation.
        path: RelativePath,
        /// The observed kind.
        kind: FilesystemEntryKind,
    },
    /// A file's observable identity or metadata changed during a verified
    /// read.
    FileChanged {
        /// The normalized file path.
        path: RelativePath,
        /// The verification phase.
        phase: FilesystemChangePhase,
        /// The observable race reason.
        reason: FilesystemChangeReason,
    },
    /// A blocking worker used by the async adapter failed before returning a
    /// scan item.
    BlockingWorkerFailed,
}

impl FilesystemScanError {
    /// Creates an adapter I/O error for internal scanner use.
    pub(crate) const fn io(
        path: RelativePath,
        operation: FilesystemOperation,
        kind: FilesystemErrorKind,
    ) -> Self {
        Self::Io {
            path,
            operation,
            kind,
        }
    }

    /// Creates an adapter I/O error associated with the selected root.
    pub(crate) const fn root_io(operation: FilesystemOperation, kind: FilesystemErrorKind) -> Self {
        Self::RootIo { operation, kind }
    }

    /// Returns the normalized path associated with this error, when any.
    pub fn path(&self) -> Option<&RelativePath> {
        match self {
            Self::RootIo { .. }
            | Self::RootNotDirectory
            | Self::RootIsSymbolicLink
            | Self::BlockingWorkerFailed => None,
            Self::Io { path, .. }
            | Self::DirectoryChanged { path, .. }
            | Self::InvalidSymlinkTarget { path, .. }
            | Self::InvalidEntry { path, .. }
            | Self::InvalidFileKind { path, .. }
            | Self::FileChanged { path, .. } => Some(path),
            Self::OpenDirectoryLimit { path, .. } => Some(path),
            Self::InvalidEntryName { parent, .. }
            | Self::InvalidEntryPath { parent, .. }
            | Self::NonUtf8EntryName { parent } => Some(parent),
        }
    }

    /// Returns the provider-neutral I/O category, when this is an adapter
    /// failure.
    pub const fn error_kind(&self) -> Option<FilesystemErrorKind> {
        match self {
            Self::RootIo { kind, .. } | Self::Io { kind, .. } => Some(*kind),
            Self::RootNotDirectory
            | Self::RootIsSymbolicLink
            | Self::DirectoryChanged { .. }
            | Self::OpenDirectoryLimit { .. }
            | Self::InvalidEntryName { .. }
            | Self::InvalidEntryPath { .. }
            | Self::NonUtf8EntryName { .. }
            | Self::InvalidSymlinkTarget { .. }
            | Self::InvalidEntry { .. }
            | Self::InvalidFileKind { .. }
            | Self::FileChanged { .. }
            | Self::BlockingWorkerFailed => None,
        }
    }

    /// Returns whether the error is a permission failure.
    pub fn is_permission_denied(&self) -> bool {
        self.error_kind()
            .is_some_and(FilesystemErrorKind::is_permission_denied)
    }

    /// Returns whether retrying the operation or restarting the affected
    /// scan/read may be useful.
    pub fn is_retryable(&self) -> bool {
        self.is_race()
            || self
                .error_kind()
                .is_some_and(FilesystemErrorKind::is_retryable)
    }

    /// Returns whether the error reports a filesystem race.
    pub const fn is_race(&self) -> bool {
        matches!(
            self,
            Self::DirectoryChanged { .. } | Self::FileChanged { .. }
        )
    }
}

impl fmt::Display for FilesystemScanError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::RootIo { operation, kind } => {
                write!(formatter, "filesystem root {operation} failed: {kind}")
            }
            Self::RootNotDirectory => {
                formatter.write_str("filesystem scan root is not a directory")
            }
            Self::RootIsSymbolicLink => {
                formatter.write_str("filesystem scan root must not be a symbolic link")
            }
            Self::Io {
                path,
                operation,
                kind,
            } => write!(
                formatter,
                "filesystem {operation} failed for '{path}': {kind}"
            ),
            Self::DirectoryChanged { path, reason } => {
                write!(
                    formatter,
                    "directory '{path}' changed during scan: {reason}"
                )
            }
            Self::OpenDirectoryLimit { path, limit } => write!(
                formatter,
                "directory '{path}' was not descended into because the open-directory limit is {limit}"
            ),
            Self::InvalidEntryName { parent, reason } => {
                write!(
                    formatter,
                    "entry name in '{parent}' is not portable: {reason}"
                )
            }
            Self::InvalidEntryPath { parent, reason } => {
                write!(
                    formatter,
                    "entry path below '{parent}' is not portable: {reason}"
                )
            }
            Self::NonUtf8EntryName { parent } => {
                write!(formatter, "entry name in '{parent}' is not valid UTF-8")
            }
            Self::InvalidSymlinkTarget { path, reason } => {
                write!(
                    formatter,
                    "symbolic-link target for '{path}' is invalid: {reason}"
                )
            }
            Self::InvalidEntry { path, reason } => {
                write!(
                    formatter,
                    "filesystem entry '{path}' is malformed: {reason}"
                )
            }
            Self::InvalidFileKind { path, kind } => {
                write!(
                    formatter,
                    "cannot read '{path}' as a regular file; it is {kind}"
                )
            }
            Self::FileChanged {
                path,
                phase,
                reason,
            } => write!(formatter, "file '{path}' changed {phase}: {reason}"),
            Self::BlockingWorkerFailed => formatter.write_str("blocking filesystem worker failed"),
        }
    }
}

impl std::error::Error for FilesystemScanError {}

/// Policy for permission failures encountered below an otherwise readable
/// source root.
#[derive(Clone, Copy, Debug, Default, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub enum FilesystemPermissionPolicy {
    /// Stop the scan after yielding the permission failure.
    #[default]
    Fail,
    /// Yield the permission failure and continue with other reachable
    /// siblings. The inaccessible entry is never silently omitted.
    Warn,
}

impl fmt::Display for FilesystemPermissionPolicy {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(match self {
            Self::Fail => "fail",
            Self::Warn => "warn",
        })
    }
}

/// Compatibility alias for callers that use the shorter policy name.
pub type PermissionErrorPolicy = FilesystemPermissionPolicy;
