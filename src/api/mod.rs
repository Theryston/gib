//! Public, silent programmatic API for GIB.

pub mod autostart;
pub mod backup;
pub mod catalog;
pub mod client;
pub mod destructive;
pub mod error;
pub mod event;
pub mod identity;
pub mod live;
pub mod repository;
pub mod restore;
pub mod storage;

pub use crate::storage::{FS, MemoryFS};
pub use autostart::{
    AddAutostartRequest, AutostartChange, AutostartJob, AutostartOverrides, AutostartRun,
    AutostartStatus, UpdateAutostartRequest,
};
pub use backup::{BackupInfo, BackupRequest, BackupResult, BackupWarning};
pub use catalog::{
    BackupSummary, ExploreDirectoryRequest, ExploreDirectoryResponse, ExploreEntry,
    ExploreFileRequest, ExploreFileResponse, ExploreHistoryRequest, ExploreHistoryResponse,
    ExploreRestoreRequest, ExploreRestoreResult, ExploreScope, ExploreSearchRequest,
    ExploreSearchResponse, ExploreSelection, ExploreSort, FileRevisionInfo, ListBackupsRequest,
    ListBackupsResponse, ListPendingBackupsRequest, ListPendingBackupsResponse, PendingBackupInfo,
    SearchIndexStatus, SearchRequest, SearchResponse, SearchResult,
};
pub use client::{ConfigDefaults, Gib, GibBuilder, GibContext};
pub use destructive::{
    DeleteBackupRequest, DeleteBackupResult, EncryptRepositoryRequest, EncryptRepositoryResult,
    PruneFailure, PruneItem, PrunePlan, PruneRequest, PruneResult,
};
pub use error::{ErrorCode, GibError};
pub use event::{
    AutostartEvent, BackupEvent, EventCallback, GibEvent, LiveEvent, OperationKind,
    OperationStarted, ProgressEvent, RestoreEvent, WarningEvent,
};
pub use identity::{
    Identity, IdentityChange, SetIdentityRequest, SetupRequest, SetupResult, SetupSkippedPath,
};
pub use live::{ConflictPolicy, LiveHandle, LiveRequest, LiveResult};
pub use repository::RepositoryRequest;
pub use restore::{RestoreFailure, RestoreRequest, RestoreResult};
pub use storage::{
    AddStorageRequest, LocalStorageConfig, S3StorageConfig, StorageChange, StorageConfig,
    StorageInfo, WebDavStorageConfig,
};

#[cfg(test)]
mod tests;
