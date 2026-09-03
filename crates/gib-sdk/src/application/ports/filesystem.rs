use crate::domain::{FilesystemMetadata, FilesystemScanError};
use std::ffi::OsString;
use std::io::{self, Read};
use std::path::Path;
use std::sync::Arc;

/// One directory name returned by an injected filesystem adapter.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct FilesystemDirectoryEntry {
    name: OsString,
}

impl FilesystemDirectoryEntry {
    /// Creates a directory entry from an operating-system name.
    pub fn new(name: impl Into<OsString>) -> Self {
        Self { name: name.into() }
    }

    /// Returns the unmodified operating-system name.
    pub fn file_name(&self) -> &std::ffi::OsStr {
        &self.name
    }
}

/// An incrementally readable directory handle.
pub trait FilesystemDirectory: Send {
    /// Returns the next entry name, or `None` at end of directory.
    fn next_entry(&mut self) -> io::Result<Option<FilesystemDirectoryEntry>>;

    /// Returns no-follow metadata for the directory represented by this
    /// enumerator.
    ///
    /// Implementations backed by a native directory handle should inspect the
    /// handle itself. Path-based test adapters may re-inspect their configured
    /// path.
    fn metadata(&self) -> io::Result<FilesystemMetadata>;
}

/// A readable regular-file handle that can report metadata for the opened
/// object itself.
pub trait FilesystemFile: Read + Send {
    /// Returns metadata for the already-opened file handle.
    fn metadata(&self) -> io::Result<FilesystemMetadata>;
}

/// The filesystem capability required by [`crate::FilesystemScanner`].
///
/// All path arguments are untrusted. Implementations must make
/// [`Self::symlink_metadata`] a no-follow operation for the final component,
/// preserve links in [`Self::read_link`], and avoid following the final
/// component in [`Self::open_file`]. The scanner supplies validated relative
/// components and performs the root-boundary checks.
pub trait Filesystem: Send + Sync {
    /// Inspects one path without following its final symbolic link.
    fn symlink_metadata(&self, path: &Path) -> io::Result<FilesystemMetadata>;

    /// Opens one directory for incremental enumeration.
    fn read_dir(&self, path: &Path) -> io::Result<Box<dyn FilesystemDirectory>>;

    /// Reads a symbolic-link target as raw portable bytes without resolving
    /// that target.
    fn read_link(&self, path: &Path) -> io::Result<Vec<u8>>;

    /// Opens one regular file for a verified streaming read.
    fn open_file(&self, path: &Path) -> io::Result<Box<dyn FilesystemFile>>;
}

impl<T> Filesystem for Arc<T>
where
    T: Filesystem + ?Sized,
{
    fn symlink_metadata(&self, path: &Path) -> io::Result<FilesystemMetadata> {
        self.as_ref().symlink_metadata(path)
    }

    fn read_dir(&self, path: &Path) -> io::Result<Box<dyn FilesystemDirectory>> {
        self.as_ref().read_dir(path)
    }

    fn read_link(&self, path: &Path) -> io::Result<Vec<u8>> {
        self.as_ref().read_link(path)
    }

    fn open_file(&self, path: &Path) -> io::Result<Box<dyn FilesystemFile>> {
        self.as_ref().open_file(path)
    }
}

impl<T> Filesystem for &T
where
    T: Filesystem + ?Sized,
{
    fn symlink_metadata(&self, path: &Path) -> io::Result<FilesystemMetadata> {
        (*self).symlink_metadata(path)
    }

    fn read_dir(&self, path: &Path) -> io::Result<Box<dyn FilesystemDirectory>> {
        (*self).read_dir(path)
    }

    fn read_link(&self, path: &Path) -> io::Result<Vec<u8>> {
        (*self).read_link(path)
    }

    fn open_file(&self, path: &Path) -> io::Result<Box<dyn FilesystemFile>> {
        (*self).open_file(path)
    }
}

/// A clock injected into scanning so observation times are deterministic in
/// tests and do not come from ambient process state.
pub trait FilesystemClock: Send + Sync {
    /// Returns signed Unix-epoch nanoseconds for the current observation.
    fn now_unix_nanos(&self) -> i64;
}

impl<T> FilesystemClock for Arc<T>
where
    T: FilesystemClock + ?Sized,
{
    fn now_unix_nanos(&self) -> i64 {
        self.as_ref().now_unix_nanos()
    }
}

impl<T> FilesystemClock for &T
where
    T: FilesystemClock + ?Sized,
{
    fn now_unix_nanos(&self) -> i64 {
        (*self).now_unix_nanos()
    }
}

/// Converts an operating-system I/O error to the stable scanner error model.
pub(crate) fn map_io_error(error: &io::Error) -> crate::domain::FilesystemErrorKind {
    #[cfg(unix)]
    if let Some(code) = error.raw_os_error()
        && (code == libc::EMFILE || code == libc::ENFILE)
    {
        return crate::domain::FilesystemErrorKind::TooManyOpenFiles;
    }

    match error.kind() {
        io::ErrorKind::NotFound => crate::domain::FilesystemErrorKind::NotFound,
        io::ErrorKind::PermissionDenied => crate::domain::FilesystemErrorKind::PermissionDenied,
        io::ErrorKind::InvalidInput => crate::domain::FilesystemErrorKind::InvalidInput,
        io::ErrorKind::NotADirectory => crate::domain::FilesystemErrorKind::NotADirectory,
        io::ErrorKind::Interrupted
        | io::ErrorKind::TimedOut
        | io::ErrorKind::WouldBlock
        | io::ErrorKind::ConnectionRefused
        | io::ErrorKind::ConnectionReset
        | io::ErrorKind::ConnectionAborted
        | io::ErrorKind::NotConnected
        | io::ErrorKind::BrokenPipe => crate::domain::FilesystemErrorKind::Transient,
        io::ErrorKind::Unsupported => crate::domain::FilesystemErrorKind::Unsupported,
        _ => crate::domain::FilesystemErrorKind::Other,
    }
}

/// Creates a scanner error for one failed filesystem operation.
pub(crate) fn io_error(
    path: crate::domain::RelativePath,
    operation: crate::domain::FilesystemOperation,
    error: &io::Error,
) -> FilesystemScanError {
    FilesystemScanError::io(path, operation, map_io_error(error))
}
