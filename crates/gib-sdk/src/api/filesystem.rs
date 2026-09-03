#[cfg(feature = "async")]
pub use crate::application::filesystem::AsyncFilesystemScan;
pub use crate::application::filesystem::{
    FilesystemScan, FilesystemScanOptions, FilesystemScanner, VerifiedFileReader,
};
pub use crate::application::ports::{
    Filesystem, FilesystemClock, FilesystemDirectory, FilesystemDirectoryEntry, FilesystemFile,
};
pub use crate::domain::{
    DEFAULT_IGNORE_GIT, FilesystemChangePhase, FilesystemChangeReason, FilesystemEntry,
    FilesystemEntryError, FilesystemEntryKind, FilesystemErrorKind, FilesystemIdentity,
    FilesystemMetadata, FilesystemOperation, FilesystemPermissionPolicy, FilesystemScanError,
    IgnoreDecision, IgnoreMatch, IgnorePathError, IgnorePattern, IgnorePatternError, IgnorePolicy,
    IgnoreReason, IgnoreRule, IgnoreRuleError, MAX_FILESYSTEM_SCAN_OPEN_DIRECTORIES,
    MAX_IGNORE_RULE_LENGTH, MAX_IGNORE_RULES, PermissionErrorPolicy, is_git_path,
};
pub use crate::infrastructure::filesystem::{LocalFilesystem, SystemClock};

/// A scanner configured with the host filesystem and wall clock.
pub type LocalFilesystemScanner = FilesystemScanner<LocalFilesystem, SystemClock>;

/// Creates a scanner using the host filesystem and wall clock.
pub fn local_filesystem_scanner() -> LocalFilesystemScanner {
    FilesystemScanner::new(LocalFilesystem, SystemClock)
}
