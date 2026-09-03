use crate::application::ports::{
    Filesystem, FilesystemClock, FilesystemDirectory, FilesystemDirectoryEntry, FilesystemFile,
};
use crate::domain::{FilePermissions, FilesystemEntryKind, FilesystemIdentity, FilesystemMetadata};
use std::fs::{self, File, OpenOptions, ReadDir};
use std::io::{self, Read};
use std::path::Path;
use std::time::{SystemTime, UNIX_EPOCH};

/// Filesystem adapter backed by the host operating system.
#[derive(Clone, Copy, Debug, Default)]
pub struct LocalFilesystem;

impl Filesystem for LocalFilesystem {
    fn symlink_metadata(&self, path: &Path) -> io::Result<FilesystemMetadata> {
        fs::symlink_metadata(path).map(|metadata| metadata_to_portable(&metadata))
    }

    fn read_dir(&self, path: &Path) -> io::Result<Box<dyn FilesystemDirectory>> {
        Ok(Box::new(LocalDirectory {
            path: path.to_path_buf(),
            entries: fs::read_dir(path)?,
        }))
    }

    fn read_link(&self, path: &Path) -> io::Result<Vec<u8>> {
        let target = fs::read_link(path)?;
        Ok(os_path_bytes(&target))
    }

    fn open_file(&self, path: &Path) -> io::Result<Box<dyn FilesystemFile>> {
        let mut options = OpenOptions::new();
        options.read(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;

            options.custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK);
        }
        #[cfg(windows)]
        {
            use std::os::windows::fs::OpenOptionsExt;

            const FILE_FLAG_OPEN_REPARSE_POINT: u32 = 0x0020_0000;
            options.custom_flags(FILE_FLAG_OPEN_REPARSE_POINT);
        }
        Ok(Box::new(LocalFile {
            file: options.open(path)?,
        }))
    }
}

/// Host clock used by the default scanner.
#[derive(Clone, Copy, Debug, Default)]
pub struct SystemClock;

impl FilesystemClock for SystemClock {
    fn now_unix_nanos(&self) -> i64 {
        let now = SystemTime::now();
        match now.duration_since(UNIX_EPOCH) {
            Ok(duration) => i64::try_from(duration.as_nanos()).unwrap_or(i64::MAX),
            Err(error) => i64::try_from(error.duration().as_nanos())
                .ok()
                .map_or(i64::MIN, |nanos| nanos.saturating_neg()),
        }
    }
}

struct LocalDirectory {
    path: std::path::PathBuf,
    entries: ReadDir,
}

impl FilesystemDirectory for LocalDirectory {
    fn next_entry(&mut self) -> io::Result<Option<FilesystemDirectoryEntry>> {
        self.entries
            .next()
            .transpose()
            .map(|entry| entry.map(|entry| FilesystemDirectoryEntry::new(entry.file_name())))
    }

    fn metadata(&self) -> io::Result<FilesystemMetadata> {
        fs::symlink_metadata(&self.path).map(|metadata| metadata_to_portable(&metadata))
    }
}

struct LocalFile {
    file: File,
}

impl Read for LocalFile {
    fn read(&mut self, buffer: &mut [u8]) -> io::Result<usize> {
        self.file.read(buffer)
    }
}

impl FilesystemFile for LocalFile {
    fn metadata(&self) -> io::Result<FilesystemMetadata> {
        self.file
            .metadata()
            .map(|metadata| metadata_to_portable(&metadata))
    }
}

fn metadata_to_portable(metadata: &fs::Metadata) -> FilesystemMetadata {
    let kind = classify_file_type(metadata);
    let mut portable = FilesystemMetadata::new(kind, metadata.len());
    if let Some(permissions) = portable_permissions(metadata) {
        portable = portable.with_permissions(permissions);
    }
    if let Some(timestamp) = metadata.modified().ok().and_then(system_time_to_nanos) {
        portable = portable.with_modified_at(timestamp);
    }
    if let Some(timestamp) = metadata.created().ok().and_then(system_time_to_nanos) {
        portable = portable.with_created_at(timestamp);
    }
    if let Some(timestamp) = metadata.accessed().ok().and_then(system_time_to_nanos) {
        portable = portable.with_accessed_at(timestamp);
    }
    portable.with_identity(filesystem_identity(metadata))
}

fn classify_file_type(metadata: &fs::Metadata) -> FilesystemEntryKind {
    let file_type = metadata.file_type();
    if file_type.is_symlink() || is_reparse_point(metadata) {
        FilesystemEntryKind::SymbolicLink
    } else if file_type.is_dir() {
        FilesystemEntryKind::Directory
    } else if file_type.is_file() {
        FilesystemEntryKind::RegularFile
    } else {
        FilesystemEntryKind::Other
    }
}

fn portable_permissions(metadata: &fs::Metadata) -> Option<FilePermissions> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;

        FilePermissions::new(metadata.permissions().mode() & FilePermissions::MAX_MODE).ok()
    }
    #[cfg(not(unix))]
    {
        let _ = metadata;
        None
    }
}

fn filesystem_identity(metadata: &fs::Metadata) -> FilesystemIdentity {
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;

        FilesystemIdentity::new(Some(metadata.dev()), Some(metadata.ino()))
    }
    #[cfg(windows)]
    {
        use std::os::windows::fs::MetadataExt;

        FilesystemIdentity::new(
            metadata.volume_serial_number().map(u64::from),
            metadata.file_index(),
        )
    }
    #[cfg(not(any(unix, windows)))]
    {
        let _ = metadata;
        FilesystemIdentity::default()
    }
}

fn is_reparse_point(metadata: &fs::Metadata) -> bool {
    #[cfg(windows)]
    {
        use std::os::windows::fs::MetadataExt;

        metadata.file_attributes() & 0x0400 != 0
    }
    #[cfg(not(windows))]
    {
        let _ = metadata;
        false
    }
}

fn os_path_bytes(path: &Path) -> Vec<u8> {
    #[cfg(unix)]
    {
        use std::os::unix::ffi::OsStrExt;

        path.as_os_str().as_bytes().to_vec()
    }
    #[cfg(not(unix))]
    {
        path.to_string_lossy().into_owned().into_bytes()
    }
}

fn system_time_to_nanos(time: SystemTime) -> Option<i64> {
    match time.duration_since(UNIX_EPOCH) {
        Ok(duration) => i64::try_from(duration.as_nanos()).ok(),
        Err(error) => i64::try_from(error.duration().as_nanos())
            .ok()
            .map(|nanos| nanos.saturating_neg()),
    }
}
