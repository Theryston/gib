use gib::{
    CURRENT_CONFIGURATION_VERSION, Configuration, MAX_CONFIGURATION_BYTES,
    ProjectConfigurationErrorKind,
};
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::time::Duration;

const MINIMAL: &str = include_str!("../../../tests/fixtures/configuration/minimal.toml");
const COMPLETE: &str = include_str!("../../../tests/fixtures/configuration/complete.toml");
const MALFORMED: &str = include_str!("../../../tests/fixtures/configuration/malformed.toml");
const UNKNOWN_FIELD: &str =
    include_str!("../../../tests/fixtures/configuration/unknown-field.toml");
const UNSUPPORTED_VERSION: &str =
    include_str!("../../../tests/fixtures/configuration/unsupported-version.toml");

static CURRENT_DIRECTORY_LOCK: Mutex<()> = Mutex::new(());

#[test]
fn minimal_configuration_loads_with_all_optional_values_absent() {
    let configuration = Configuration::parse(MINIMAL, Path::new("/project"))
        .expect("minimal configuration should parse");

    assert_eq!(configuration.version(), CURRENT_CONFIGURATION_VERSION);
    assert!(configuration.repository().storage().is_none());
    assert!(configuration.repository().key().is_none());
    assert!(configuration.backup().root_path().is_none());
    assert!(configuration.backup().message().is_none());
    assert!(configuration.backup().compress().is_none());
    assert!(configuration.backup().chunk_size().is_none());
    assert!(configuration.backup().concurrency().is_none());
    assert!(configuration.backup().ignore().is_empty());
    assert!(configuration.live().message().is_none());
    assert!(configuration.live().debounce().is_none());
    assert!(configuration.live().poll().is_none());
    assert!(configuration.restore().target_path().is_none());
}

#[test]
fn complete_configuration_validates_every_section_and_resolves_paths() {
    let base = Path::new("/workspace/project");
    let configuration =
        Configuration::parse(COMPLETE, base).expect("complete configuration should parse");

    assert_eq!(configuration.repository().storage(), Some("mybackups"));
    assert_eq!(configuration.repository().key(), Some("my-project"));
    assert_eq!(
        configuration.backup().root_path(),
        Some(base.join("./source").as_path())
    );
    assert_eq!(configuration.backup().message(), Some("Project backup"));
    assert_eq!(configuration.backup().compress(), Some(3));
    assert_eq!(
        configuration.backup().chunk_size().map(|size| size.bytes()),
        Some(4096)
    );
    assert_eq!(configuration.backup().concurrency(), Some(8));
    assert_eq!(
        configuration.backup().ignore(),
        [String::from("node_modules"), String::from("dist")].as_slice()
    );
    assert_eq!(
        configuration.live().message(),
        Some("Project live synchronization")
    );
    assert_eq!(
        configuration.live().debounce(),
        Some(Duration::from_millis(1500))
    );
    assert_eq!(configuration.live().poll_ms(), Some(2000));
    assert_eq!(
        configuration.restore().target_path(),
        Some(base.join("./.gib-restore").as_path())
    );
}

#[test]
fn file_loading_reports_exact_file_and_field_context() -> Result<(), Box<dyn std::error::Error>> {
    let directory = TestDirectory::new();
    let unknown_path = directory.path().join("gib.toml");
    fs::write(&unknown_path, UNKNOWN_FIELD)?;
    let unknown =
        Configuration::from_file(&unknown_path).expect_err("unknown fields should be rejected");
    assert_eq!(unknown.kind(), ProjectConfigurationErrorKind::UnknownField);
    assert_eq!(unknown.field(), Some("backup.unknown"));
    assert_eq!(unknown.file(), Some(unknown_path.as_path()));
    assert!(unknown.to_string().contains("backup.unknown"));
    assert!(
        unknown
            .to_string()
            .contains(&unknown_path.display().to_string())
    );

    fs::write(&unknown_path, UNSUPPORTED_VERSION)?;
    let unsupported = Configuration::from_file(&unknown_path)
        .expect_err("unsupported versions should be rejected");
    assert_eq!(
        unsupported.kind(),
        ProjectConfigurationErrorKind::UnsupportedVersion
    );
    assert_eq!(unsupported.field(), Some("version"));
    assert_eq!(unsupported.version(), Some(2));
    assert_eq!(unsupported.file(), Some(unknown_path.as_path()));
    Ok(())
}

#[test]
fn malformed_files_and_invalid_values_are_typed() -> Result<(), Box<dyn std::error::Error>> {
    let directory = TestDirectory::new();
    let path = directory.path().join("gib.toml");
    fs::write(&path, MALFORMED)?;
    let malformed = Configuration::from_file(&path).expect_err("malformed TOML should fail");
    assert_eq!(malformed.kind(), ProjectConfigurationErrorKind::Parse);
    assert_eq!(malformed.file(), Some(path.as_path()));

    fs::write(&path, "[backup]\nmessage = \"missing version\"\n")?;
    let missing = Configuration::from_file(&path).expect_err("version should be required");
    assert_eq!(missing.kind(), ProjectConfigurationErrorKind::MissingField);
    assert_eq!(missing.field(), Some("version"));
    assert_eq!(missing.file(), Some(path.as_path()));

    for (contents, field) in [
        (
            "version = 1\n[repository]\nstorage = \"\"\n",
            "repository.storage",
        ),
        ("version = 1\n[repository]\nkey = \"\"\n", "repository.key"),
        (
            "version = 1\n[backup]\nroot_path = \"\"\n",
            "backup.root_path",
        ),
        ("version = 1\n[backup]\ncompress = 0\n", "backup.compress"),
        (
            "version = 1\n[backup]\nchunk_size = \"not-a-size\"\n",
            "backup.chunk_size",
        ),
        (
            "version = 1\n[backup]\nconcurrency = 0\n",
            "backup.concurrency",
        ),
        (
            "version = 1\n[backup]\nignore = [\"\"]\n",
            "backup.ignore[0]",
        ),
        ("version = 1\n[live]\ndebounce_ms = 0\n", "live.debounce_ms"),
        ("version = 1\n[live]\npoll_ms = 0\n", "live.poll_ms"),
        (
            "version = 1\n[restore]\ntarget_path = \"\"\n",
            "restore.target_path",
        ),
    ] {
        fs::write(&path, contents)?;
        let error = Configuration::from_file(&path).expect_err("invalid value should fail");
        assert_eq!(error.kind(), ProjectConfigurationErrorKind::InvalidValue);
        assert_eq!(error.field(), Some(field));
        assert_eq!(error.file(), Some(path.as_path()));
    }

    let long_message = "x".repeat(513);
    fs::write(
        &path,
        format!("version = 1\n[backup]\nmessage = \"{long_message}\"\n"),
    )?;
    let error = Configuration::from_file(&path).expect_err("long messages should fail");
    assert_eq!(error.kind(), ProjectConfigurationErrorKind::InvalidValue);
    assert_eq!(error.field(), Some("backup.message"));

    let long_live_message = "x".repeat(513);
    fs::write(
        &path,
        format!("version = 1\n[live]\nmessage = \"{long_live_message}\"\n"),
    )?;
    let error = Configuration::from_file(&path).expect_err("long Live messages should fail");
    assert_eq!(error.kind(), ProjectConfigurationErrorKind::InvalidValue);
    assert_eq!(error.field(), Some("live.message"));
    Ok(())
}

#[test]
fn relative_paths_use_the_config_directory_after_the_process_changes_directory()
-> Result<(), Box<dyn std::error::Error>> {
    let _lock = CURRENT_DIRECTORY_LOCK
        .lock()
        .map_err(|_| "current-directory test lock was poisoned")?;
    let directory = TestDirectory::new();
    let nested = directory.path().join("nested");
    fs::create_dir(&nested)?;
    let config_path = directory.path().join("gib.toml");
    fs::write(
        &config_path,
        "version = 1\n[backup]\nroot_path = \"source\"\n[restore]\ntarget_path = \"restore\"\n",
    )?;

    let original = std::env::current_dir()?;
    let _restore_directory = CurrentDirectoryGuard { path: original };
    std::env::set_current_dir(&nested)?;
    let configuration = Configuration::from_file(&config_path)?;

    assert_eq!(
        configuration.backup().root_path(),
        Some(directory.path().join("source").as_path())
    );
    assert_eq!(
        configuration.restore().target_path(),
        Some(directory.path().join("restore").as_path())
    );
    Ok(())
}

#[test]
fn parsing_does_not_create_files_or_directories() {
    let directory = TestDirectory::new();
    let before = entries(directory.path());
    let _configuration = Configuration::parse(
        "version = 1\n[backup]\nroot_path = \"source\"\n",
        directory.path(),
    )
    .expect("configuration should parse");
    assert_eq!(entries(directory.path()), before);
}

#[test]
fn oversized_documents_are_rejected_before_toml_allocation() {
    let contents = format!("version = 1\n#{}", "x".repeat(MAX_CONFIGURATION_BYTES));
    let error = Configuration::parse(&contents, Path::new("/project"))
        .expect_err("oversized configuration should fail");
    assert_eq!(error.kind(), ProjectConfigurationErrorKind::InputTooLarge);
    assert!(error.field().is_none());
}

fn entries(path: &Path) -> Vec<PathBuf> {
    let mut entries = fs::read_dir(path)
        .expect("test directory should be readable")
        .map(|entry| entry.expect("test entry should be readable").path())
        .collect::<Vec<_>>();
    entries.sort();
    entries
}

struct CurrentDirectoryGuard {
    path: PathBuf,
}

impl Drop for CurrentDirectoryGuard {
    fn drop(&mut self) {
        let _ = std::env::set_current_dir(&self.path);
    }
}

struct TestDirectory {
    path: PathBuf,
}

impl TestDirectory {
    fn new() -> Self {
        let path = std::env::temp_dir().join(format!(
            "gib-project-configuration-test-{}-{}",
            std::process::id(),
            UNIQUE_DIRECTORY.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
        ));
        fs::create_dir_all(&path).expect("test directory should be created");
        Self { path }
    }

    fn path(&self) -> &Path {
        &self.path
    }
}

impl Drop for TestDirectory {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.path);
    }
}

static UNIQUE_DIRECTORY: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(1);
