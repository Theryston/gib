#[cfg(feature = "async")]
pub use crate::application::filesystem::AsyncFilesystemScan;
pub use crate::application::filesystem::{
    FilesystemScan, FilesystemScanOptions, FilesystemScanner, VerifiedFileReader,
};
pub use crate::application::ports::{
    Filesystem, FilesystemClock, FilesystemDirectory, FilesystemDirectoryEntry, FilesystemFile,
};
pub use crate::domain::{
    FilesystemChangePhase, FilesystemChangeReason, FilesystemEntry, FilesystemEntryError,
    FilesystemEntryKind, FilesystemErrorKind, FilesystemIdentity, FilesystemMetadata,
    FilesystemOperation, FilesystemPermissionPolicy, FilesystemScanError,
    MAX_FILESYSTEM_SCAN_OPEN_DIRECTORIES, PermissionErrorPolicy,
};
pub use crate::infrastructure::filesystem::{LocalFilesystem, SystemClock};

/// A scanner configured with the host filesystem and wall clock.
pub type LocalFilesystemScanner = FilesystemScanner<LocalFilesystem, SystemClock>;

/// Creates a scanner using the host filesystem and wall clock.
pub fn local_filesystem_scanner() -> LocalFilesystemScanner {
    FilesystemScanner::new(LocalFilesystem, SystemClock)
}
