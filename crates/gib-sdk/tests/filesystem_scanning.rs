use gib::{
    EntryName, Filesystem, FilesystemClock, FilesystemDirectory, FilesystemDirectoryEntry,
    FilesystemEntry, FilesystemEntryKind, FilesystemFile, FilesystemMetadata,
    FilesystemPermissionPolicy, FilesystemScanError, FilesystemScanOptions, FilesystemScanner,
    IgnorePolicy, LocalFilesystem, RelativePath, SymlinkTarget,
};
use std::ffi::OsString;
use std::fs::{self, File};
use std::io::{self, Cursor, Read};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

static NEXT_TEMP_DIRECTORY: AtomicU64 = AtomicU64::new(0);

struct TestDirectory(PathBuf);

impl TestDirectory {
    fn new() -> io::Result<Self> {
        let base = std::env::temp_dir();
        loop {
            let sequence = NEXT_TEMP_DIRECTORY.fetch_add(1, Ordering::Relaxed);
            let path = base.join(format!(
                "gib-filesystem-scan-{}-{sequence}",
                std::process::id()
            ));
            match fs::create_dir(&path) {
                Ok(()) => return Ok(Self(path)),
                Err(error) if error.kind() == io::ErrorKind::AlreadyExists => continue,
                Err(error) => return Err(error),
            }
        }
    }

    fn path(&self) -> &Path {
        &self.0
    }
}

impl Drop for TestDirectory {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

#[derive(Clone, Copy)]
struct FixedClock(i64);

impl FilesystemClock for FixedClock {
    fn now_unix_nanos(&self) -> i64 {
        self.0
    }
}

fn scan_entries<F, C>(
    scanner: &FilesystemScanner<F, C>,
    root: &Path,
) -> io::Result<Vec<FilesystemEntry>>
where
    F: Filesystem + 'static,
    C: FilesystemClock + 'static,
{
    scanner
        .scan(root)
        .map_err(io::Error::other)?
        .collect::<Result<Vec<_>, _>>()
        .map_err(io::Error::other)
}

fn find_entry<F, C>(
    scan: &mut gib::FilesystemScan<F, C>,
    path: &str,
) -> Result<FilesystemEntry, Box<dyn std::error::Error>>
where
    F: Filesystem + 'static,
    C: FilesystemClock + 'static,
{
    for item in scan {
        let entry = item?;
        if entry.path().as_str() == path {
            return Ok(entry);
        }
    }
    Err(format!("entry '{path}' was not found").into())
}

#[test]
fn portable_paths_reject_traversal_and_accept_unicode_without_rewriting_it()
-> Result<(), Box<dyn std::error::Error>> {
    for value in [
        "../outside",
        "/absolute",
        "a/../../outside",
        "a//b",
        "a\\b",
        "C:drive-relative",
        "CON",
        "name.",
        "name ",
    ] {
        assert!(
            RelativePath::new(value).is_err(),
            "path should be rejected: {value}"
        );
    }
    assert_eq!(
        RelativePath::new("café/данные/🙂.txt")?.as_str(),
        "café/данные/🙂.txt"
    );
    assert!(EntryName::new("é").is_ok());
    Ok(())
}

#[test]
fn scanner_rejects_untrusted_directory_names_before_path_operations()
-> Result<(), Box<dyn std::error::Error>> {
    let scanner = FilesystemScanner::new(MaliciousNameFilesystem, FixedClock(3));
    let mut scan = scanner.scan(Path::new("/virtual/source"))?;
    assert!(scan.next().ok_or("root item missing")?.is_ok());
    let error = scan
        .next()
        .ok_or("invalid-name error missing")?
        .expect_err("traversal name should fail closed");
    assert!(matches!(
        error,
        FilesystemScanError::InvalidEntryName {
            reason: gib::EntryNameError::Traversal,
            ..
        }
    ));
    assert!(scan.next().is_none());
    Ok(())
}

#[cfg(unix)]
#[test]
fn a_symbolic_link_selected_as_the_root_is_rejected() -> Result<(), Box<dyn std::error::Error>> {
    let directory = TestDirectory::new()?;
    let target = directory.path().join("target");
    fs::create_dir(&target)?;
    let link = directory.path().join("root-link");
    std::os::unix::fs::symlink(&target, &link)?;

    let scanner = FilesystemScanner::new(LocalFilesystem, FixedClock(4));
    let error = match scanner.scan(&link) {
        Ok(_) => return Err("symbolic-link root was accepted".into()),
        Err(error) => error,
    };
    assert!(matches!(error, FilesystemScanError::RootIsSymbolicLink));
    Ok(())
}

#[test]
fn local_scan_preserves_links_and_never_descends_into_them()
-> Result<(), Box<dyn std::error::Error>> {
    let directory = TestDirectory::new()?;
    let root = directory.path().join("source");
    let nested = root.join("nested");
    let outside = directory.path().join("outside");
    fs::create_dir_all(&nested)?;
    fs::create_dir_all(&outside)?;
    fs::write(root.join("café-🙂.txt"), b"portable unicode")?;
    fs::write(nested.join("inside.txt"), b"inside")?;
    fs::write(outside.join("secret.txt"), b"must not be scanned")?;

    #[cfg(unix)]
    {
        std::os::unix::fs::symlink(&outside, root.join("outside-link"))?;
        std::os::unix::fs::symlink(".", nested.join("loop"))?;
    }

    let scanner = FilesystemScanner::new(LocalFilesystem, FixedClock(42));
    let entries = scan_entries(&scanner, &root)?;
    let paths: Vec<_> = entries.iter().map(|entry| entry.path().as_str()).collect();

    assert!(paths.contains(&""));
    assert!(paths.contains(&"nested"));
    assert!(paths.contains(&"nested/inside.txt"));
    assert!(paths.contains(&"café-🙂.txt"));
    #[cfg(unix)]
    {
        let link = entries
            .iter()
            .find(|entry| entry.path().as_str() == "outside-link")
            .ok_or("outside link was not emitted")?;
        assert_eq!(link.kind(), FilesystemEntryKind::SymbolicLink);
        assert_eq!(
            link.symlink_target().and_then(SymlinkTarget::as_str),
            Some(outside.to_str().ok_or("outside path is not UTF-8")?)
        );
        assert!(!paths.iter().any(|path| path.starts_with("outside-link/")));

        let loop_link = entries
            .iter()
            .find(|entry| entry.path().as_str() == "nested/loop")
            .ok_or("loop link was not emitted")?;
        assert_eq!(loop_link.kind(), FilesystemEntryKind::SymbolicLink);
        assert!(!paths.iter().any(|path| path.starts_with("nested/loop/")));
    }
    assert!(!paths.iter().any(|path| path.contains("secret.txt")));
    Ok(())
}

#[test]
fn nested_git_paths_are_excluded_by_default_and_included_by_explicit_opt_in()
-> Result<(), Box<dyn std::error::Error>> {
    let directory = TestDirectory::new()?;
    let root = directory.path().join("source");
    fs::create_dir_all(root.join("nested/.git"))?;
    fs::create_dir_all(root.join("nested/git"))?;
    fs::write(root.join("nested/.git/HEAD"), b"head")?;
    fs::write(root.join("nested/git/HEAD"), b"not metadata")?;
    fs::write(root.join("nested/.gitignore"), b"ignore file")?;
    fs::write(root.join("nested/gitignore"), b"similar name")?;

    let default_entries = scan_entries(
        &FilesystemScanner::new(LocalFilesystem, FixedClock(23)),
        &root,
    )?;
    let default_paths: Vec<_> = default_entries
        .iter()
        .map(|entry| entry.path().as_str())
        .collect();
    assert!(!default_paths.iter().any(|path| {
        path.split('/')
            .any(|component| component.eq_ignore_ascii_case(".git"))
    }));
    assert!(default_paths.contains(&"nested/.gitignore"));
    assert!(default_paths.contains(&"nested/git/HEAD"));
    assert!(default_paths.contains(&"nested/gitignore"));

    let policy = IgnorePolicy::default().with_no_ignore_git();
    let included_entries = scan_entries(
        &FilesystemScanner::new(LocalFilesystem, FixedClock(23)).with_ignore_policy(policy),
        &root,
    )?;
    let included_paths: Vec<_> = included_entries
        .iter()
        .map(|entry| entry.path().as_str())
        .collect();
    assert!(included_paths.contains(&"nested/.git"));
    assert!(included_paths.contains(&"nested/.git/HEAD"));
    Ok(())
}

#[test]
fn ignored_directories_are_pruned_before_their_entries_are_inspected()
-> Result<(), Box<dyn std::error::Error>> {
    let directory = TestDirectory::new()?;
    let root = directory.path().join("source");
    let ignored = root.join("ignored");
    let kept = root.join("kept");
    fs::create_dir_all(&ignored)?;
    fs::create_dir_all(&kept)?;
    for index in 0..5_000 {
        fs::write(ignored.join(format!("entry-{index:05}")), b"ignored")?;
    }
    fs::write(kept.join("visible"), b"visible")?;

    let read_directories = Arc::new(Mutex::new(Vec::new()));
    let filesystem = CountingFilesystem {
        read_directories: Arc::clone(&read_directories),
    };
    let scanner =
        FilesystemScanner::new(filesystem, FixedClock(29)).with_ignore_patterns(["ignored"])?;
    let entries = scan_entries(&scanner, &root)?;
    let paths: Vec<_> = entries.iter().map(|entry| entry.path().as_str()).collect();

    assert!(paths.contains(&"kept"));
    assert!(paths.contains(&"kept/visible"));
    assert!(!paths.iter().any(|path| path.starts_with("ignored")));
    let read_directories = read_directories
        .lock()
        .map_err(|_| "read directory test lock was poisoned")?;
    assert_eq!(read_directories.len(), 2);
    assert!(read_directories.iter().any(|path| path == &root));
    assert!(read_directories.iter().any(|path| path == &kept));
    assert!(!read_directories.iter().any(|path| path == &ignored));
    Ok(())
}

#[test]
fn backup_and_live_scans_can_share_the_same_decision_policy()
-> Result<(), Box<dyn std::error::Error>> {
    let policy = IgnorePolicy::new(["cache", "src/generated", "**/*.tmp"])?;
    let backup_scanner =
        FilesystemScanner::new(LocalFilesystem, FixedClock(31)).with_ignore_policy(policy.clone());
    let live_scanner =
        FilesystemScanner::new(LocalFilesystem, FixedClock(31)).with_ignore_policy(policy);

    for path in [
        "cache/data.bin",
        "src/generated/code.rs",
        "work/file.tmp",
        "work/file.rs",
        "src/other.rs",
    ] {
        assert_eq!(
            backup_scanner.ignore_decision(path)?,
            live_scanner.ignore_decision(path)?,
            "decision mismatch for {path}"
        );
    }
    Ok(())
}

#[test]
fn local_metadata_captures_supported_size_permissions_timestamps_and_identity()
-> Result<(), Box<dyn std::error::Error>> {
    let directory = TestDirectory::new()?;
    let root = directory.path().join("source");
    fs::create_dir(&root)?;
    let file = root.join("data.bin");
    fs::write(&file, b"12345")?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;

        fs::set_permissions(&file, fs::Permissions::from_mode(0o644))?;
    }

    let scanner = FilesystemScanner::new(LocalFilesystem, FixedClock(123));
    let mut scan = scanner.scan(&root)?;
    assert_eq!(scan.next().ok_or("root item missing")??.observed_at(), 123);
    let entry = find_entry(&mut scan, "data.bin")?;
    assert_eq!(entry.kind(), FilesystemEntryKind::RegularFile);
    assert_eq!(entry.metadata().size(), 5);
    assert!(entry.metadata().modified_at().is_some());
    assert!(entry.metadata().created_at().is_some());
    assert!(entry.metadata().identity().is_complete());
    #[cfg(unix)]
    assert_eq!(
        entry
            .metadata()
            .permissions()
            .ok_or("POSIX permissions missing")?
            .mode(),
        0o644
    );
    #[cfg(not(unix))]
    assert!(entry.metadata().permissions().is_none());
    Ok(())
}

#[test]
fn permission_warn_yields_the_failure_and_continues_without_omitting_it()
-> Result<(), Box<dyn std::error::Error>> {
    let scanner = FilesystemScanner::new(PermissionFilesystem, FixedClock(7)).with_options(
        FilesystemScanOptions::new().with_permission_policy(FilesystemPermissionPolicy::Warn),
    );
    let scan = scanner.scan(Path::new("/virtual/source"))?;
    let mut paths = Vec::new();
    let mut permission_errors = 0;
    for item in scan {
        match item {
            Ok(entry) => paths.push(entry.path().as_str().to_owned()),
            Err(error) if error.is_permission_denied() => {
                permission_errors += 1;
                assert_eq!(error.path().map(RelativePath::as_str), Some("blocked"));
            }
            Err(error) => return Err(error.into()),
        }
    }
    assert_eq!(permission_errors, 1);
    assert!(paths.contains(&"ok.txt".to_owned()));
    assert!(!paths.contains(&"blocked".to_owned()));
    Ok(())
}

#[test]
fn permission_fail_yields_the_failure_and_stops_before_later_entries()
-> Result<(), Box<dyn std::error::Error>> {
    let scanner = FilesystemScanner::new(PermissionFilesystem, FixedClock(7));
    let mut scan = scanner.scan(Path::new("/virtual/source"))?;
    assert_eq!(scan.next().ok_or("root item missing")??.path().as_str(), "");
    let error = scan
        .next()
        .ok_or("permission item missing")?
        .expect_err("blocked entry should fail");
    assert!(error.is_permission_denied());
    assert!(scan.next().is_none());
    Ok(())
}

#[cfg(unix)]
#[test]
fn non_utf8_names_are_reported_instead_of_silently_omitted()
-> Result<(), Box<dyn std::error::Error>> {
    use std::os::unix::ffi::OsStringExt;

    let directory = TestDirectory::new()?;
    let root = directory.path().join("source");
    fs::create_dir(&root)?;
    let invalid_name = OsString::from_vec(vec![b'i', b'n', b'v', b'a', b'l', b'i', b'd', 0x80]);
    File::create(root.join(invalid_name))?;

    let scanner = FilesystemScanner::new(LocalFilesystem, FixedClock(1));
    let mut scan = scanner.scan(&root)?;
    assert!(scan.next().ok_or("root item missing")?.is_ok());
    let error = scan
        .next()
        .ok_or("invalid-name error missing")?
        .expect_err("invalid name should fail");
    assert!(matches!(
        error,
        FilesystemScanError::NonUtf8EntryName { .. }
    ));
    Ok(())
}

#[test]
fn deep_paths_remain_incremental_and_the_directory_limit_is_visible()
-> Result<(), Box<dyn std::error::Error>> {
    let directory = TestDirectory::new()?;
    let root = directory.path().join("source");
    fs::create_dir(&root)?;
    let mut current = root.clone();
    for index in 0..128 {
        current = current.join(format!("d{index:03}"));
        fs::create_dir(&current)?;
    }
    fs::write(current.join("leaf"), b"leaf")?;

    let scanner = FilesystemScanner::new(LocalFilesystem, FixedClock(9));
    let entries = scan_entries(&scanner, &root)?;
    assert!(
        entries
            .iter()
            .any(|entry| entry.path().as_str().ends_with("/leaf"))
    );

    let limited = FilesystemScanner::new(LocalFilesystem, FixedClock(9))
        .with_options(FilesystemScanOptions::new().with_max_open_directories(1));
    let mut scan = limited.scan(&root)?;
    let mut saw_limit = false;
    for item in &mut scan {
        match item {
            Ok(_) => {}
            Err(FilesystemScanError::OpenDirectoryLimit { limit: 1, .. }) => {
                saw_limit = true;
                break;
            }
            Err(error) => return Err(error.into()),
        }
    }
    assert!(saw_limit);
    Ok(())
}

#[test]
fn a_large_flat_tree_is_streamed_entry_by_entry() -> Result<(), Box<dyn std::error::Error>> {
    let directory = TestDirectory::new()?;
    let root = directory.path().join("source");
    fs::create_dir(&root)?;
    for index in 0..5_000 {
        File::create(root.join(format!("entry-{index:05}")))?;
    }

    let scanner = FilesystemScanner::new(LocalFilesystem, FixedClock(11));
    let mut scan = scanner.scan(&root)?;
    assert!(scan.next().ok_or("root item missing")?.is_ok());
    let first = scan.next().ok_or("first child missing")??;
    assert!(first.path().as_str().starts_with("entry-"));
    let remaining = scan.count();
    assert_eq!(remaining, 4_999);
    Ok(())
}

#[cfg(unix)]
#[test]
fn replacing_a_file_before_open_is_reported_as_a_race() -> Result<(), Box<dyn std::error::Error>> {
    let directory = TestDirectory::new()?;
    let root = directory.path().join("source");
    fs::create_dir(&root)?;
    let file = root.join("race.txt");
    fs::write(&file, b"old")?;
    let replacement = root.join("replacement.txt");
    fs::write(&replacement, b"new")?;

    let scanner = FilesystemScanner::new(LocalFilesystem, FixedClock(13));
    let mut scan = scanner.scan(&root)?;
    let _ = scan.next();
    let entry = find_entry(&mut scan, "race.txt")?;
    fs::rename(&replacement, &file)?;
    let result = scan.open_file(&entry);
    let error = match result {
        Ok(_) => return Err("replacement race was not detected".into()),
        Err(error) => error,
    };
    assert!(matches!(error, FilesystemScanError::FileChanged { .. }));
    assert!(error.is_race());
    assert!(error.is_retryable());
    Ok(())
}

#[cfg(unix)]
#[test]
fn replacing_a_file_during_read_is_reported_when_finalizing()
-> Result<(), Box<dyn std::error::Error>> {
    let directory = TestDirectory::new()?;
    let root = directory.path().join("source");
    fs::create_dir(&root)?;
    let file = root.join("race.txt");
    fs::write(&file, vec![b'a'; 128 * 1024])?;
    let replacement = root.join("replacement.txt");
    fs::write(&replacement, vec![b'b'; 128 * 1024])?;

    let scanner = FilesystemScanner::new(LocalFilesystem, FixedClock(17));
    let mut scan = scanner.scan(&root)?;
    let _ = scan.next();
    let entry = find_entry(&mut scan, "race.txt")?;
    let mut reader = scan.open_file(&entry)?;
    let mut first_chunk = [0_u8; 4096];
    reader.read_exact(&mut first_chunk)?;
    fs::rename(&replacement, &file)?;
    let mut rest = Vec::new();
    let read_result = reader.read_to_end(&mut rest);
    assert!(read_result.is_err());
    let error = reader
        .finish()
        .expect_err("finalization should retain the race error");
    assert!(matches!(error, FilesystemScanError::FileChanged { .. }));
    assert!(error.is_race());
    assert!(error.is_retryable());
    Ok(())
}

#[cfg(feature = "async")]
#[tokio::test(flavor = "current_thread")]
async fn async_scan_moves_each_blocking_step_to_the_blocking_pool()
-> Result<(), Box<dyn std::error::Error>> {
    use futures_util::StreamExt;

    let directory = TestDirectory::new()?;
    let root = directory.path().join("source");
    fs::create_dir(&root)?;
    fs::write(root.join("file"), b"file")?;
    let scanner = FilesystemScanner::new(LocalFilesystem, FixedClock(19));
    let mut stream = scanner.scan_async(root).await?;
    let mut paths = Vec::new();
    while let Some(item) = stream.next().await {
        paths.push(item?.path().as_str().to_owned());
    }
    assert!(paths.contains(&"".to_owned()));
    assert!(paths.contains(&"file".to_owned()));
    Ok(())
}

#[derive(Clone, Copy)]
struct PermissionFilesystem;

impl Filesystem for PermissionFilesystem {
    fn symlink_metadata(&self, path: &Path) -> io::Result<FilesystemMetadata> {
        if path == Path::new("/virtual/source") {
            return Ok(FilesystemMetadata::new(FilesystemEntryKind::Directory, 0));
        }
        if path.file_name().and_then(|name| name.to_str()) == Some("blocked") {
            return Err(io::Error::new(io::ErrorKind::PermissionDenied, "blocked"));
        }
        if path.file_name().and_then(|name| name.to_str()) == Some("ok.txt") {
            return Ok(FilesystemMetadata::new(FilesystemEntryKind::RegularFile, 2));
        }
        Err(io::Error::new(io::ErrorKind::NotFound, "missing"))
    }

    fn read_dir(&self, path: &Path) -> io::Result<Box<dyn FilesystemDirectory>> {
        if path == Path::new("/virtual/source") {
            return Ok(Box::new(PermissionDirectory {
                index: 0,
                names: vec!["blocked".into(), "ok.txt".into()],
            }));
        }
        Err(io::Error::new(
            io::ErrorKind::NotADirectory,
            "not a directory",
        ))
    }

    fn read_link(&self, _path: &Path) -> io::Result<Vec<u8>> {
        Err(io::Error::new(io::ErrorKind::Unsupported, "not a link"))
    }

    fn open_file(&self, _path: &Path) -> io::Result<Box<dyn FilesystemFile>> {
        Ok(Box::new(PermissionFile {
            content: Cursor::new(b"ok".to_vec()),
        }))
    }
}

struct PermissionDirectory {
    index: usize,
    names: Vec<OsString>,
}

impl FilesystemDirectory for PermissionDirectory {
    fn next_entry(&mut self) -> io::Result<Option<FilesystemDirectoryEntry>> {
        let Some(name) = self.names.get(self.index).cloned() else {
            return Ok(None);
        };
        self.index += 1;
        Ok(Some(FilesystemDirectoryEntry::new(name)))
    }

    fn metadata(&self) -> io::Result<FilesystemMetadata> {
        Ok(FilesystemMetadata::new(FilesystemEntryKind::Directory, 0))
    }
}

struct PermissionFile {
    content: Cursor<Vec<u8>>,
}

impl Read for PermissionFile {
    fn read(&mut self, buffer: &mut [u8]) -> io::Result<usize> {
        self.content.read(buffer)
    }
}

impl FilesystemFile for PermissionFile {
    fn metadata(&self) -> io::Result<FilesystemMetadata> {
        Ok(FilesystemMetadata::new(FilesystemEntryKind::RegularFile, 2))
    }
}

struct CountingFilesystem {
    read_directories: Arc<Mutex<Vec<PathBuf>>>,
}

impl Filesystem for CountingFilesystem {
    fn symlink_metadata(&self, path: &Path) -> io::Result<FilesystemMetadata> {
        LocalFilesystem.symlink_metadata(path)
    }

    fn read_dir(&self, path: &Path) -> io::Result<Box<dyn FilesystemDirectory>> {
        self.read_directories
            .lock()
            .map_err(|_| io::Error::other("read directory test lock was poisoned"))?
            .push(path.to_path_buf());
        LocalFilesystem.read_dir(path).map(|reader| {
            Box::new(CountingDirectory { inner: reader }) as Box<dyn FilesystemDirectory>
        })
    }

    fn read_link(&self, path: &Path) -> io::Result<Vec<u8>> {
        LocalFilesystem.read_link(path)
    }

    fn open_file(&self, path: &Path) -> io::Result<Box<dyn FilesystemFile>> {
        LocalFilesystem.open_file(path)
    }
}

struct CountingDirectory {
    inner: Box<dyn FilesystemDirectory>,
}

impl FilesystemDirectory for CountingDirectory {
    fn next_entry(&mut self) -> io::Result<Option<FilesystemDirectoryEntry>> {
        self.inner.next_entry()
    }

    fn metadata(&self) -> io::Result<FilesystemMetadata> {
        self.inner.metadata()
    }
}

#[derive(Clone, Copy)]
struct MaliciousNameFilesystem;

impl Filesystem for MaliciousNameFilesystem {
    fn symlink_metadata(&self, path: &Path) -> io::Result<FilesystemMetadata> {
        if path == Path::new("/virtual/source") {
            return Ok(FilesystemMetadata::new(FilesystemEntryKind::Directory, 0));
        }
        if path.file_name().and_then(|name| name.to_str()) == Some("ok") {
            return Ok(FilesystemMetadata::new(FilesystemEntryKind::RegularFile, 0));
        }
        Err(io::Error::new(io::ErrorKind::NotFound, "missing"))
    }

    fn read_dir(&self, path: &Path) -> io::Result<Box<dyn FilesystemDirectory>> {
        if path == Path::new("/virtual/source") {
            return Ok(Box::new(PermissionDirectory {
                index: 0,
                names: vec![
                    OsString::from(".."),
                    OsString::from("nested/escape"),
                    "ok".into(),
                ],
            }));
        }
        Err(io::Error::new(
            io::ErrorKind::NotADirectory,
            "not a directory",
        ))
    }

    fn read_link(&self, _path: &Path) -> io::Result<Vec<u8>> {
        Err(io::Error::new(io::ErrorKind::Unsupported, "not a link"))
    }

    fn open_file(&self, _path: &Path) -> io::Result<Box<dyn FilesystemFile>> {
        Ok(Box::new(PermissionFile {
            content: Cursor::new(Vec::new()),
        }))
    }
}
