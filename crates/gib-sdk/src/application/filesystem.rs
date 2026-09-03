use super::ports::{
    Filesystem, FilesystemClock, FilesystemDirectory, FilesystemFile, io_error, map_io_error,
};
use crate::domain::{
    FilesystemChangePhase, FilesystemChangeReason, FilesystemEntry, FilesystemEntryKind,
    FilesystemMetadata, FilesystemOperation, FilesystemPermissionPolicy, FilesystemScanError,
    IgnoreDecision, IgnorePathError, IgnorePatternError, IgnorePolicy,
    MAX_FILESYSTEM_SCAN_OPEN_DIRECTORIES, RelativePath,
};
use std::io::{self, Read};
use std::path::{Path, PathBuf};
use std::sync::Arc;

/// Options controlling bounded filesystem discovery.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct FilesystemScanOptions {
    permission_policy: FilesystemPermissionPolicy,
    max_open_directories: usize,
}

impl FilesystemScanOptions {
    /// Creates the default fail-closed scan policy.
    pub const fn new() -> Self {
        Self {
            permission_policy: FilesystemPermissionPolicy::Fail,
            max_open_directories: MAX_FILESYSTEM_SCAN_OPEN_DIRECTORIES,
        }
    }

    /// Selects whether permission failures stop the scan or are yielded as
    /// warnings before reachable siblings continue.
    pub const fn with_permission_policy(mut self, policy: FilesystemPermissionPolicy) -> Self {
        self.permission_policy = policy;
        self
    }

    /// Selects the maximum number of open directory enumerators.
    ///
    /// A zero value is rejected when scanning starts. Keeping this setter
    /// infallible makes options easy to compose while still ensuring an
    /// invalid resource policy cannot result in an unbounded traversal.
    pub const fn with_max_open_directories(mut self, limit: usize) -> Self {
        self.max_open_directories = limit;
        self
    }

    /// Returns the permission policy.
    pub const fn permission_policy(self) -> FilesystemPermissionPolicy {
        self.permission_policy
    }

    /// Returns the open-directory bound.
    pub const fn max_open_directories(self) -> usize {
        self.max_open_directories
    }
}

impl Default for FilesystemScanOptions {
    fn default() -> Self {
        Self::new()
    }
}

/// A filesystem scanner with explicit filesystem and clock dependencies.
///
/// The scanner emits the selected root first and then performs a depth-first
/// traversal. It keeps one directory enumerator per active path component, so
/// memory and descriptors are bounded by the configured open-directory limit
/// and by the portable relative-path limit. Symbolic links and link-like
/// reparse points are emitted as links and are never descended into.
pub struct FilesystemScanner<F, C> {
    filesystem: Arc<F>,
    clock: Arc<C>,
    options: FilesystemScanOptions,
    ignore_policy: Arc<IgnorePolicy>,
}

impl<F, C> Clone for FilesystemScanner<F, C> {
    fn clone(&self) -> Self {
        Self {
            filesystem: Arc::clone(&self.filesystem),
            clock: Arc::clone(&self.clock),
            options: self.options,
            ignore_policy: Arc::clone(&self.ignore_policy),
        }
    }
}

impl<F, C> FilesystemScanner<F, C>
where
    F: Filesystem + 'static,
    C: FilesystemClock + 'static,
{
    /// Creates a scanner from an explicit filesystem adapter and clock.
    pub fn new(filesystem: F, clock: C) -> Self {
        Self {
            filesystem: Arc::new(filesystem),
            clock: Arc::new(clock),
            options: FilesystemScanOptions::default(),
            ignore_policy: Arc::new(IgnorePolicy::default()),
        }
    }

    /// Replaces the scanner options.
    pub const fn with_options(mut self, options: FilesystemScanOptions) -> Self {
        self.options = options;
        self
    }

    /// Returns the scanner options.
    pub const fn options(&self) -> FilesystemScanOptions {
        self.options
    }

    /// Replaces the policy used to select entries for a capture scan.
    ///
    /// The policy is evaluated against each normalized path before that entry
    /// is inspected or, for directories, opened. Backup and Live callers can
    /// therefore pass the same resolved policy and receive identical
    /// selection and traversal behavior.
    pub fn with_ignore_policy(mut self, policy: IgnorePolicy) -> Self {
        self.ignore_policy = Arc::new(policy);
        self
    }

    /// Parses and installs repeated ignore patterns for a capture scan.
    pub fn with_ignore_patterns<I, T>(self, patterns: I) -> Result<Self, IgnorePatternError>
    where
        I: IntoIterator<Item = T>,
        T: AsRef<str>,
    {
        Ok(self.with_ignore_policy(IgnorePolicy::new(patterns)?))
    }

    /// Opens a root with the supplied capture policy.
    pub fn scan_with_ignore_policy(
        &self,
        root: impl AsRef<Path>,
        policy: IgnorePolicy,
    ) -> Result<FilesystemScan<F, C>, FilesystemScanError> {
        self.clone().with_ignore_policy(policy).scan(root)
    }

    /// Includes `.git` directories and files in this capture scan.
    pub fn with_no_ignore_git(self) -> Self {
        let policy = self.ignore_policy.as_ref().clone().with_no_ignore_git();
        self.with_ignore_policy(policy)
    }

    /// Returns the policy used by this scanner for future scans.
    pub fn ignore_policy(&self) -> &IgnorePolicy {
        &self.ignore_policy
    }

    /// Evaluates a relative path using this scanner's capture policy.
    pub fn ignore_decision<P>(&self, path: P) -> Result<IgnoreDecision, IgnorePathError>
    where
        P: AsRef<str>,
    {
        self.ignore_policy.decision(path)
    }

    /// Opens and validates a source root, returning an incremental scan.
    ///
    /// The root itself must be a directory and must not be a symbolic link.
    /// Root failures are returned immediately because there is no safe
    /// sibling path to continue with. Descendant failures are yielded by the
    /// returned iterator according to the configured permission policy.
    pub fn scan(
        &self,
        root: impl AsRef<Path>,
    ) -> Result<FilesystemScan<F, C>, FilesystemScanError> {
        if self.options.max_open_directories == 0 {
            return Err(FilesystemScanError::OpenDirectoryLimit {
                path: RelativePath::root(),
                limit: 0,
            });
        }

        let root = root.as_ref().to_path_buf();
        let started_at = self.clock.now_unix_nanos();
        let metadata = self.filesystem.symlink_metadata(&root).map_err(|error| {
            FilesystemScanError::root_io(FilesystemOperation::SymlinkMetadata, map_io_error(&error))
        })?;
        if metadata.kind() == FilesystemEntryKind::SymbolicLink {
            return Err(FilesystemScanError::RootIsSymbolicLink);
        }
        if metadata.kind() != FilesystemEntryKind::Directory {
            return Err(FilesystemScanError::RootNotDirectory);
        }

        let relative = RelativePath::root();
        let root_entry = FilesystemEntry::new(relative.clone(), metadata.clone(), None, started_at)
            .map_err(|reason| FilesystemScanError::InvalidEntry {
                path: relative.clone(),
                reason,
            })?;
        let reader = self.filesystem.read_dir(&root).map_err(|error| {
            FilesystemScanError::root_io(FilesystemOperation::ReadDirectory, map_io_error(&error))
        })?;

        Ok(FilesystemScan {
            filesystem: Arc::clone(&self.filesystem),
            clock: Arc::clone(&self.clock),
            root_path: root.clone(),
            root_entry,
            pending_root: true,
            frames: vec![DirectoryFrame {
                relative_path: relative,
                metadata,
                reader,
            }],
            options: self.options,
            ignore_policy: Arc::clone(&self.ignore_policy),
            pending_error: None,
            finished: false,
        })
    }

    /// Opens a scanned regular file for a verified streaming read.
    ///
    /// The entry is re-inspected before opening. The returned reader must be
    /// read to EOF and finalized with [`VerifiedFileReader::finish`]. A path
    /// replacement, size change, identity change, or modification timestamp
    /// change is reported as a typed race error.
    pub fn open_file(
        &self,
        root: impl AsRef<Path>,
        entry: &FilesystemEntry,
    ) -> Result<VerifiedFileReader<F>, FilesystemScanError> {
        open_verified_file(Arc::clone(&self.filesystem), root.as_ref(), entry)
    }

    /// Starts an async scan whose blocking filesystem work runs on Tokio's
    /// blocking pool.
    #[cfg(feature = "async")]
    pub async fn scan_async(
        &self,
        root: impl Into<PathBuf>,
    ) -> Result<AsyncFilesystemScan<F, C>, FilesystemScanError> {
        let scanner = self.clone();
        let root = root.into();
        tokio::task::spawn_blocking(move || scanner.scan(root))
            .await
            .map_err(|_| FilesystemScanError::BlockingWorkerFailed)?
            .map(AsyncFilesystemScan::new)
    }
}

/// The incremental result of [`FilesystemScanner::scan`].
pub struct FilesystemScan<F, C> {
    filesystem: Arc<F>,
    clock: Arc<C>,
    root_path: PathBuf,
    root_entry: FilesystemEntry,
    pending_root: bool,
    frames: Vec<DirectoryFrame>,
    options: FilesystemScanOptions,
    ignore_policy: Arc<IgnorePolicy>,
    pending_error: Option<FilesystemScanError>,
    finished: bool,
}

impl<F, C> FilesystemScan<F, C>
where
    F: Filesystem + 'static,
    C: FilesystemClock + 'static,
{
    /// Returns the selected source root path.
    pub fn root(&self) -> &Path {
        &self.root_path
    }

    /// Returns the scanner clock value captured when the scan was opened.
    pub const fn started_at(&self) -> i64 {
        self.root_entry.observed_at()
    }

    /// Returns the scan options.
    pub const fn options(&self) -> FilesystemScanOptions {
        self.options
    }

    /// Returns the policy used for this scan.
    pub fn ignore_policy(&self) -> &IgnorePolicy {
        &self.ignore_policy
    }

    /// Evaluates a relative path using this scan's capture policy.
    pub fn ignore_decision<P>(&self, path: P) -> Result<IgnoreDecision, IgnorePathError>
    where
        P: AsRef<str>,
    {
        self.ignore_policy.decision(path)
    }

    /// Opens an emitted regular-file entry using this scan's selected root.
    pub fn open_file(
        &self,
        entry: &FilesystemEntry,
    ) -> Result<VerifiedFileReader<F>, FilesystemScanError> {
        open_verified_file(Arc::clone(&self.filesystem), &self.root_path, entry)
    }
}

impl<F, C> Iterator for FilesystemScan<F, C>
where
    F: Filesystem + 'static,
    C: FilesystemClock + 'static,
{
    type Item = Result<FilesystemEntry, FilesystemScanError>;

    fn next(&mut self) -> Option<Self::Item> {
        if self.finished {
            return None;
        }
        if self.pending_root {
            self.pending_root = false;
            return Some(Ok(self.root_entry.clone()));
        }
        if let Some(error) = self.pending_error.take() {
            return Some(self.emit_error(error));
        }

        loop {
            let Some(frame_index) = self.frames.len().checked_sub(1) else {
                self.finished = true;
                return None;
            };
            let directory_check = {
                let frame = &self.frames[frame_index];
                frame.reader.metadata()
            };
            let (directory_path, directory_metadata) = match directory_check {
                Ok(metadata) => {
                    let frame = &self.frames[frame_index];
                    (frame.relative_path.clone(), metadata)
                }
                Err(error) => {
                    let Some(frame) = self.frames.pop() else {
                        self.finished = true;
                        return None;
                    };
                    let scan_error = io_error(
                        frame.relative_path,
                        FilesystemOperation::DirectoryMetadata,
                        &error,
                    );
                    return Some(self.emit_error(scan_error));
                }
            };
            let expected_metadata = &self.frames[frame_index].metadata;
            if let Some(reason) = directory_change_reason(expected_metadata, &directory_metadata) {
                self.frames.pop();
                return Some(self.emit_error(FilesystemScanError::DirectoryChanged {
                    path: directory_path,
                    reason,
                }));
            }

            let next_entry = {
                let frame = &mut self.frames[frame_index];
                frame.reader.next_entry()
            };
            let directory_entry = match next_entry {
                Ok(Some(entry)) => entry,
                Ok(None) => {
                    let Some(frame) = self.frames.pop() else {
                        self.finished = true;
                        return None;
                    };
                    let final_metadata = match frame.reader.metadata() {
                        Ok(metadata) => metadata,
                        Err(error) => {
                            return Some(self.emit_error(io_error(
                                frame.relative_path,
                                FilesystemOperation::DirectoryMetadata,
                                &error,
                            )));
                        }
                    };
                    if let Some(reason) = directory_change_reason(&frame.metadata, &final_metadata)
                    {
                        return Some(self.emit_error(FilesystemScanError::DirectoryChanged {
                            path: frame.relative_path,
                            reason,
                        }));
                    }
                    continue;
                }
                Err(error) => {
                    let Some(frame) = self.frames.pop() else {
                        self.finished = true;
                        return None;
                    };
                    return Some(self.emit_error(io_error(
                        frame.relative_path,
                        FilesystemOperation::ReadDirectory,
                        &error,
                    )));
                }
            };

            let frame = &self.frames[frame_index];
            let parent = frame.relative_path.clone();
            let name = match directory_entry.file_name().to_str() {
                Some(name) => name,
                None => {
                    return Some(self.emit_error(FilesystemScanError::NonUtf8EntryName { parent }));
                }
            };
            let name = match crate::domain::EntryName::new(name) {
                Ok(name) => name,
                Err(reason) => {
                    return Some(
                        self.emit_error(FilesystemScanError::InvalidEntryName { parent, reason }),
                    );
                }
            };
            let relative_path = match frame.relative_path.join(&name) {
                Ok(path) => path,
                Err(reason) => {
                    return Some(
                        self.emit_error(FilesystemScanError::InvalidEntryPath { parent, reason }),
                    );
                }
            };
            if self.ignore_policy.is_ignored(relative_path.as_str()) {
                continue;
            }
            let absolute_path = join_root(&self.root_path, &relative_path);
            let metadata = match self.filesystem.symlink_metadata(&absolute_path) {
                Ok(metadata) => metadata,
                Err(error) => {
                    return Some(self.emit_error(io_error(
                        relative_path,
                        FilesystemOperation::SymlinkMetadata,
                        &error,
                    )));
                }
            };
            let symlink_target = if metadata.kind() == FilesystemEntryKind::SymbolicLink {
                let target = match self.filesystem.read_link(&absolute_path) {
                    Ok(target) => target,
                    Err(error) => {
                        return Some(self.emit_error(io_error(
                            relative_path,
                            FilesystemOperation::ReadLink,
                            &error,
                        )));
                    }
                };
                match crate::domain::SymlinkTarget::new(target) {
                    Ok(target) => Some(target),
                    Err(reason) => {
                        return Some(self.emit_error(FilesystemScanError::InvalidSymlinkTarget {
                            path: relative_path,
                            reason,
                        }));
                    }
                }
            } else {
                None
            };
            let observed_at = self.clock.now_unix_nanos();
            let entry = match FilesystemEntry::new(
                relative_path.clone(),
                metadata.clone(),
                symlink_target,
                observed_at,
            ) {
                Ok(entry) => entry,
                Err(error) => {
                    return Some(self.emit_error(FilesystemScanError::InvalidEntry {
                        path: relative_path,
                        reason: error,
                    }));
                }
            };

            if metadata.kind() == FilesystemEntryKind::Directory {
                if self.frames.len() >= self.options.max_open_directories {
                    self.pending_error = Some(FilesystemScanError::OpenDirectoryLimit {
                        path: relative_path,
                        limit: self.options.max_open_directories,
                    });
                } else {
                    match self.filesystem.read_dir(&absolute_path) {
                        Ok(reader) => match reader.metadata() {
                            Ok(current) => {
                                if let Some(reason) = directory_change_reason(&metadata, &current) {
                                    self.pending_error =
                                        Some(FilesystemScanError::DirectoryChanged {
                                            path: relative_path,
                                            reason,
                                        });
                                } else {
                                    self.frames.push(DirectoryFrame {
                                        relative_path,
                                        metadata,
                                        reader,
                                    });
                                }
                            }
                            Err(error) => {
                                self.pending_error = Some(io_error(
                                    entry.path().clone(),
                                    FilesystemOperation::DirectoryMetadata,
                                    &error,
                                ));
                            }
                        },
                        Err(error) => {
                            self.pending_error = Some(io_error(
                                entry.path().clone(),
                                FilesystemOperation::ReadDirectory,
                                &error,
                            ));
                        }
                    }
                }
            }
            return Some(Ok(entry));
        }
    }
}

impl<F, C> FilesystemScan<F, C> {
    fn emit_error(
        &mut self,
        error: FilesystemScanError,
    ) -> Result<FilesystemEntry, FilesystemScanError> {
        if !(self.options.permission_policy == FilesystemPermissionPolicy::Warn
            && error.is_permission_denied())
        {
            self.finished = true;
        }
        Err(error)
    }
}

struct DirectoryFrame {
    relative_path: RelativePath,
    metadata: FilesystemMetadata,
    reader: Box<dyn FilesystemDirectory>,
}

fn join_root(root: &Path, relative: &RelativePath) -> PathBuf {
    let mut path = root.to_path_buf();
    for component in relative.components() {
        path.push(component.as_str());
    }
    path
}

fn directory_change_reason(
    expected: &FilesystemMetadata,
    actual: &FilesystemMetadata,
) -> Option<FilesystemChangeReason> {
    if expected.kind() != actual.kind() {
        return Some(FilesystemChangeReason::TypeChanged);
    }
    if let Some(reason) = identity_change_reason(expected, actual) {
        return Some(reason);
    }
    if expected.size() != actual.size() {
        return Some(FilesystemChangeReason::SizeChanged);
    }
    if expected.modified_at().is_some() && expected.modified_at() != actual.modified_at() {
        return Some(FilesystemChangeReason::ModifiedTimeChanged);
    }
    if expected.created_at().is_some() && expected.created_at() != actual.created_at() {
        return Some(FilesystemChangeReason::ModifiedTimeChanged);
    }
    None
}

fn file_change_reason(
    expected: &FilesystemMetadata,
    actual: &FilesystemMetadata,
) -> Option<FilesystemChangeReason> {
    if expected.kind() != actual.kind() {
        return Some(FilesystemChangeReason::TypeChanged);
    }
    if let Some(reason) = identity_change_reason(expected, actual) {
        return Some(reason);
    }
    if expected.size() != actual.size() {
        return Some(FilesystemChangeReason::SizeChanged);
    }
    if expected.modified_at().is_some() && expected.modified_at() != actual.modified_at() {
        return Some(FilesystemChangeReason::ModifiedTimeChanged);
    }
    if expected.created_at().is_some() && expected.created_at() != actual.created_at() {
        return Some(FilesystemChangeReason::ModifiedTimeChanged);
    }
    None
}

fn identity_change_reason(
    expected: &FilesystemMetadata,
    actual: &FilesystemMetadata,
) -> Option<FilesystemChangeReason> {
    let expected_identity = expected.identity();
    let actual_identity = actual.identity();
    if expected_identity.is_complete() && !actual_identity.is_complete() {
        return Some(FilesystemChangeReason::IdentityChanged);
    }
    if !expected_identity.matches_observation(actual_identity) {
        return Some(FilesystemChangeReason::IdentityChanged);
    }
    None
}

fn open_verified_file<F>(
    filesystem: Arc<F>,
    root: &Path,
    entry: &FilesystemEntry,
) -> Result<VerifiedFileReader<F>, FilesystemScanError>
where
    F: Filesystem + 'static,
{
    if entry.kind() != FilesystemEntryKind::RegularFile {
        return Err(FilesystemScanError::InvalidFileKind {
            path: entry.path().clone(),
            kind: entry.kind(),
        });
    }
    let absolute_path = join_root(root, entry.path());
    let current = filesystem
        .symlink_metadata(&absolute_path)
        .map_err(|error| {
            io_error(
                entry.path().clone(),
                FilesystemOperation::SymlinkMetadata,
                &error,
            )
        })?;
    if let Some(reason) = file_change_reason(entry.metadata(), &current) {
        return Err(FilesystemScanError::FileChanged {
            path: entry.path().clone(),
            phase: FilesystemChangePhase::BeforeOpen,
            reason: if reason == FilesystemChangeReason::IdentityChanged {
                FilesystemChangeReason::PathReplaced
            } else {
                reason
            },
        });
    }
    let file = filesystem
        .open_file(&absolute_path)
        .map_err(|error| io_error(entry.path().clone(), FilesystemOperation::OpenFile, &error))?;
    let opened_metadata = file.metadata().map_err(|error| {
        io_error(
            entry.path().clone(),
            FilesystemOperation::FileHandleMetadata,
            &error,
        )
    })?;
    if let Some(reason) = file_change_reason(entry.metadata(), &opened_metadata) {
        return Err(FilesystemScanError::FileChanged {
            path: entry.path().clone(),
            phase: FilesystemChangePhase::BeforeOpen,
            reason: if reason == FilesystemChangeReason::IdentityChanged {
                FilesystemChangeReason::PathReplaced
            } else {
                reason
            },
        });
    }
    Ok(VerifiedFileReader {
        filesystem,
        absolute_path,
        relative_path: entry.path().clone(),
        expected: entry.metadata().clone(),
        file,
        bytes_read: 0,
        eof: false,
        terminal_error: None,
    })
}

/// A regular-file reader that verifies the scanned object before and after
/// streaming its contents.
pub struct VerifiedFileReader<F> {
    filesystem: Arc<F>,
    absolute_path: PathBuf,
    relative_path: RelativePath,
    expected: FilesystemMetadata,
    file: Box<dyn FilesystemFile>,
    bytes_read: u64,
    eof: bool,
    terminal_error: Option<FilesystemScanError>,
}

impl<F> VerifiedFileReader<F>
where
    F: Filesystem + 'static,
{
    /// Returns the normalized path being read.
    pub fn path(&self) -> &RelativePath {
        &self.relative_path
    }

    /// Returns the expected size captured during discovery.
    pub const fn expected_size(&self) -> u64 {
        self.expected.size()
    }

    /// Returns the number of bytes successfully returned to the caller.
    pub const fn bytes_read(&self) -> u64 {
        self.bytes_read
    }

    /// Returns whether EOF was reached and all final checks passed.
    pub const fn is_complete(&self) -> bool {
        self.eof && self.terminal_error.is_none()
    }

    /// Completes verification after the caller has read the stream to EOF.
    ///
    /// A returned race error invalidates the partial byte stream. The caller
    /// may discard it and restart discovery or retry the affected file from a
    /// fresh entry.
    pub fn finish(mut self) -> Result<(), FilesystemScanError> {
        if let Some(error) = self.terminal_error.take() {
            return Err(error);
        }
        if !self.eof {
            return Err(FilesystemScanError::FileChanged {
                path: self.relative_path,
                phase: FilesystemChangePhase::DuringRead,
                reason: FilesystemChangeReason::SizeChanged,
            });
        }
        self.verify_after_read()
    }

    fn verify_after_read(&self) -> Result<(), FilesystemScanError> {
        let handle_metadata = self.file.metadata().map_err(|error| {
            io_error(
                self.relative_path.clone(),
                FilesystemOperation::FileHandleMetadata,
                &error,
            )
        })?;
        if let Some(reason) = file_change_reason(&self.expected, &handle_metadata) {
            return Err(FilesystemScanError::FileChanged {
                path: self.relative_path.clone(),
                phase: FilesystemChangePhase::AfterRead,
                reason,
            });
        }
        let path_metadata = self
            .filesystem
            .symlink_metadata(&self.absolute_path)
            .map_err(|error| {
                io_error(
                    self.relative_path.clone(),
                    FilesystemOperation::SymlinkMetadata,
                    &error,
                )
            })?;
        if let Some(reason) = file_change_reason(&self.expected, &path_metadata) {
            return Err(FilesystemScanError::FileChanged {
                path: self.relative_path.clone(),
                phase: FilesystemChangePhase::AfterRead,
                reason: if reason == FilesystemChangeReason::IdentityChanged {
                    FilesystemChangeReason::PathReplaced
                } else {
                    reason
                },
            });
        }
        if self.bytes_read != self.expected.size() {
            return Err(FilesystemScanError::FileChanged {
                path: self.relative_path.clone(),
                phase: FilesystemChangePhase::AfterRead,
                reason: FilesystemChangeReason::SizeChanged,
            });
        }
        Ok(())
    }

    fn terminal(&mut self, error: FilesystemScanError) -> io::Result<usize> {
        self.terminal_error = Some(error.clone());
        Err(io::Error::other(error))
    }
}

impl<F> Read for VerifiedFileReader<F>
where
    F: Filesystem + 'static,
{
    fn read(&mut self, buffer: &mut [u8]) -> io::Result<usize> {
        if let Some(error) = self.terminal_error.clone() {
            return Err(io::Error::other(error));
        }
        if self.eof || buffer.is_empty() {
            return Ok(0);
        }
        let read = match self.file.read(buffer) {
            Ok(read) => read,
            Err(error) => {
                return self.terminal(FilesystemScanError::io(
                    self.relative_path.clone(),
                    FilesystemOperation::ReadFile,
                    map_io_error(&error),
                ));
            }
        };
        let read_u64 = match u64::try_from(read) {
            Ok(read) => read,
            Err(_) => {
                return self.terminal(FilesystemScanError::FileChanged {
                    path: self.relative_path.clone(),
                    phase: FilesystemChangePhase::DuringRead,
                    reason: FilesystemChangeReason::SizeChanged,
                });
            }
        };
        self.bytes_read = match self.bytes_read.checked_add(read_u64) {
            Some(total) => total,
            None => {
                return self.terminal(FilesystemScanError::FileChanged {
                    path: self.relative_path.clone(),
                    phase: FilesystemChangePhase::DuringRead,
                    reason: FilesystemChangeReason::SizeChanged,
                });
            }
        };
        if self.bytes_read > self.expected.size() {
            return self.terminal(FilesystemScanError::FileChanged {
                path: self.relative_path.clone(),
                phase: FilesystemChangePhase::DuringRead,
                reason: FilesystemChangeReason::SizeChanged,
            });
        }
        if read == 0 {
            if self.bytes_read != self.expected.size() {
                return self.terminal(FilesystemScanError::FileChanged {
                    path: self.relative_path.clone(),
                    phase: FilesystemChangePhase::DuringRead,
                    reason: FilesystemChangeReason::SizeChanged,
                });
            }
            if let Err(error) = self.verify_after_read() {
                self.terminal_error = Some(error.clone());
                return Err(io::Error::other(error));
            }
            self.eof = true;
        }
        Ok(read)
    }
}

#[cfg(feature = "async")]
type AsyncFilesystemScanStep<F, C> = (
    FilesystemScan<F, C>,
    Option<Result<FilesystemEntry, FilesystemScanError>>,
);

/// An async stream that performs one bounded blocking scan step per poll.
#[cfg(feature = "async")]
pub struct AsyncFilesystemScan<F, C> {
    state: Option<FilesystemScan<F, C>>,
    pending: Option<tokio::task::JoinHandle<AsyncFilesystemScanStep<F, C>>>,
    finished: bool,
}

#[cfg(feature = "async")]
impl<F, C> AsyncFilesystemScan<F, C> {
    fn new(state: FilesystemScan<F, C>) -> Self {
        Self {
            state: Some(state),
            pending: None,
            finished: false,
        }
    }
}

#[cfg(feature = "async")]
impl<F, C> Unpin for AsyncFilesystemScan<F, C> {}

#[cfg(feature = "async")]
impl<F, C> futures_util::Stream for AsyncFilesystemScan<F, C>
where
    F: Filesystem + 'static,
    C: FilesystemClock + 'static,
{
    type Item = Result<FilesystemEntry, FilesystemScanError>;

    fn poll_next(
        self: std::pin::Pin<&mut Self>,
        context: &mut std::task::Context<'_>,
    ) -> std::task::Poll<Option<Self::Item>> {
        let this = self.get_mut();
        if this.finished {
            return std::task::Poll::Ready(None);
        }
        if this.pending.is_none() {
            let Some(mut state) = this.state.take() else {
                this.finished = true;
                return std::task::Poll::Ready(None);
            };
            this.pending = Some(tokio::task::spawn_blocking(move || {
                let item = state.next();
                (state, item)
            }));
        }
        let Some(pending) = this.pending.as_mut() else {
            this.finished = true;
            return std::task::Poll::Ready(None);
        };
        match std::future::Future::poll(std::pin::Pin::new(pending), context) {
            std::task::Poll::Pending => std::task::Poll::Pending,
            std::task::Poll::Ready(Ok((state, item))) => {
                this.pending = None;
                this.state = Some(state);
                if item.is_none() {
                    this.finished = true;
                }
                std::task::Poll::Ready(item)
            }
            std::task::Poll::Ready(Err(_)) => {
                this.pending = None;
                this.finished = true;
                std::task::Poll::Ready(Some(Err(FilesystemScanError::BlockingWorkerFailed)))
            }
        }
    }
}
