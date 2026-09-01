use gib::{
    CONTENT_DEFINED_CHUNKING_ALGORITHM, CURRENT_CHUNKING_VERSION, CURRENT_CONFIGURATION_VERSION,
    ChunkingConfiguration, Configuration, ConfigurationFileMetadata, ConfigurationFileSystem,
    ConfigurationOverrides, ConfigurationResolutionRequest, ConfigurationResolver,
    ConfigurationSource, LocalConfigurationFileSystem, MAX_CONFIGURATION_BYTES,
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
const CONTENT_DEFINED: &str =
    include_str!("../../../tests/fixtures/configuration/content-defined.toml");

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
    assert_eq!(
        configuration.backup().chunking().version(),
        CURRENT_CHUNKING_VERSION
    );
    assert_eq!(
        configuration.backup().chunking().algorithm(),
        CONTENT_DEFINED_CHUNKING_ALGORITHM
    );
    assert!(configuration.backup().concurrency().is_none());
    assert!(configuration.backup().ignore().is_empty());
    assert!(configuration.live().message().is_none());
    assert!(configuration.live().debounce().is_none());
    assert!(configuration.live().poll().is_none());
    assert!(configuration.restore().target_path().is_none());
}

#[test]
fn content_defined_configuration_is_validated_and_exposed_as_policy_metadata() {
    let configuration = Configuration::parse(CONTENT_DEFINED, Path::new("/project"))
        .expect("content-defined configuration should parse");
    let chunking = configuration.backup().chunking();
    assert_eq!(chunking.version(), 1);
    assert_eq!(chunking.algorithm(), "buzhash");
    assert_eq!(chunking.window_size(), 64);
    assert_eq!(chunking.min_size(), 64 * 1024);
    assert_eq!(chunking.target_size(), 128 * 1024);
    assert_eq!(chunking.max_size(), 256 * 1024);
    assert_eq!(chunking.canonical_policy_bytes(), {
        let mut expected = b"GIB chunking policy\0".to_vec();
        expected.extend_from_slice(&7_u16.to_be_bytes());
        expected.extend_from_slice(b"buzhash");
        expected.extend_from_slice(&1_u16.to_be_bytes());
        expected.extend_from_slice(&64_u16.to_be_bytes());
        expected.extend_from_slice(&0x4752_4942_4344_4331_u64.to_be_bytes());
        expected.extend_from_slice(&(64_u64 * 1024).to_be_bytes());
        expected.extend_from_slice(&(128_u64 * 1024).to_be_bytes());
        expected.extend_from_slice(&(256_u64 * 1024).to_be_bytes());
        expected
    });
}

#[test]
fn content_defined_configuration_rejects_unknown_and_invalid_policy_values() {
    for (contents, field, kind) in [
        (
            "version = 1\n[backup.chunking]\nalgorithm = \"other\"\n",
            "backup.chunking",
            ProjectConfigurationErrorKind::InvalidValue,
        ),
        (
            "version = 1\n[backup.chunking]\nversion = 2\n",
            "backup.chunking",
            ProjectConfigurationErrorKind::InvalidValue,
        ),
        (
            "version = 1\n[backup.chunking]\nmin_size = \"8 MiB\"\ntarget_size = \"4 MiB\"\nmax_size = \"16 MiB\"\n",
            "backup.chunking",
            ProjectConfigurationErrorKind::InvalidValue,
        ),
        (
            "version = 1\n[backup.chunking]\nunknown = true\n",
            "backup.chunking.unknown",
            ProjectConfigurationErrorKind::UnknownField,
        ),
    ] {
        let error = Configuration::parse(contents, Path::new("/project"))
            .expect_err("invalid content-defined policy should fail");
        assert_eq!(error.kind(), kind);
        assert_eq!(error.field(), Some(field));
    }
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

#[test]
fn nearest_ancestor_configuration_wins_and_reports_its_canonical_source()
-> Result<(), Box<dyn std::error::Error>> {
    let directory = TestDirectory::new();
    let project = directory.path().join("project");
    let nested = project.join("src").join("module");
    fs::create_dir_all(&nested)?;
    fs::write(
        directory.path().join("gib.toml"),
        "version = 1\n[backup]\nmessage = \"root\"\n",
    )?;
    let project_config = project.join("gib.toml");
    fs::write(
        &project_config,
        "version = 1\n[backup]\nmessage = \"nearest\"\n",
    )?;

    let resolved =
        ConfigurationResolver::default().resolve(ConfigurationResolutionRequest::new(&nested))?;

    assert_eq!(resolved.configuration().backup().message(), Some("nearest"));
    assert_eq!(
        resolved.source(),
        &ConfigurationSource::Discovered(fs::canonicalize(project_config)?)
    );
    let event = resolved.source_event();
    assert!(event.loaded());
    assert_eq!(event.path(), resolved.path());
    Ok(())
}

#[test]
fn discovery_terminates_at_the_filesystem_root_without_a_configuration_file()
-> Result<(), Box<dyn std::error::Error>> {
    let directory = TestDirectory::new();
    let nested = directory.path().join("one").join("two");
    fs::create_dir_all(&nested)?;

    let discovered = ConfigurationResolver::default().discover(&nested)?;

    assert!(discovered.is_none());
    Ok(())
}

#[test]
fn explicit_paths_are_canonicalized_and_invalid_targets_are_rejected()
-> Result<(), Box<dyn std::error::Error>> {
    let directory = TestDirectory::new();
    let project = directory.path().join("project");
    let nested = project.join("src");
    fs::create_dir_all(&nested)?;
    let config_path = project.join("gib.toml");
    fs::write(
        &config_path,
        "version = 1\n[repository]\nstorage = \"explicit\"\n",
    )?;

    let resolved = ConfigurationResolver::default()
        .resolve(ConfigurationResolutionRequest::new(&nested).with_config_path("../gib.toml"))?;
    assert_eq!(
        resolved.source(),
        &ConfigurationSource::Explicit(fs::canonicalize(config_path)?)
    );
    assert_eq!(
        resolved.configuration().repository().storage(),
        Some("explicit")
    );

    let missing = ConfigurationResolver::default()
        .resolve(ConfigurationResolutionRequest::new(&nested).with_config_path("missing.toml"))
        .expect_err("missing explicit paths should fail");
    assert_eq!(missing.kind(), ProjectConfigurationErrorKind::InvalidPath);

    let directory_target = ConfigurationResolver::default()
        .resolve(ConfigurationResolutionRequest::new(&nested).with_config_path("."))
        .expect_err("directory explicit paths should fail");
    assert_eq!(
        directory_target.kind(),
        ProjectConfigurationErrorKind::InvalidPath
    );
    Ok(())
}

#[test]
fn disabled_discovery_uses_defaults_without_reading_an_invalid_file()
-> Result<(), Box<dyn std::error::Error>> {
    let directory = TestDirectory::new();
    let nested = directory.path().join("child");
    fs::create_dir(&nested)?;
    fs::write(
        directory.path().join("gib.toml"),
        "this is not valid configuration",
    )?;

    let resolved = ConfigurationResolver::default()
        .resolve(ConfigurationResolutionRequest::new(&nested).without_config())?;

    assert_eq!(resolved.source(), &ConfigurationSource::Disabled);
    assert!(resolved.configuration().backup().message().is_none());
    assert!(resolved.source_event().path().is_none());
    Ok(())
}

#[test]
fn cli_overrides_win_for_every_field_without_discarding_file_values()
-> Result<(), Box<dyn std::error::Error>> {
    let directory = TestDirectory::new();
    let invocation = directory.path().join("invocation");
    fs::create_dir(&invocation)?;
    let config_path = directory.path().join("gib.toml");
    fs::write(
        &config_path,
        r#"version = 1

[repository]
storage = "file-storage"
key = "file-key"

[backup]
root_path = "file-source"
message = "file-backup"
compress = 3
chunk_size = "4096 B"
concurrency = 2
ignore = ["z-file", "shared"]

[live]
message = "file-live"
debounce_ms = 100
poll_ms = 200

[restore]
target_path = "file-restore"
"#,
    )?;
    let overrides = ConfigurationOverrides::new()
        .with_repository_storage("cli-storage")
        .with_repository_key("cli-key")
        .with_backup_root_path("cli-source")
        .with_backup_message("cli-backup")
        .with_backup_compress(22)
        .with_backup_chunk_size("8192 B")
        .with_backup_chunking(
            ChunkingConfiguration::new(32 * 1024, 64 * 1024, 128 * 1024)
                .expect("override policy should be valid"),
        )
        .with_backup_concurrency(8)
        .with_ignore_rules(["cli", "shared"])
        .with_live_message("cli-live")
        .with_live_debounce_ms(300)
        .with_live_poll_ms(400)
        .with_restore_target_path("cli-restore");

    let resolved = ConfigurationResolver::default().resolve(
        ConfigurationResolutionRequest::new(&invocation)
            .with_config_path(&config_path)
            .with_overrides(overrides),
    )?;
    let configuration = resolved.configuration();

    assert_eq!(configuration.repository().storage(), Some("cli-storage"));
    assert_eq!(configuration.repository().key(), Some("cli-key"));
    assert_eq!(
        configuration.backup().root_path(),
        Some(invocation.join("cli-source").as_path())
    );
    assert_eq!(configuration.backup().message(), Some("cli-backup"));
    assert_eq!(configuration.backup().compress(), Some(22));
    assert_eq!(
        configuration.backup().chunk_size().map(|size| size.bytes()),
        Some(8192)
    );
    assert_eq!(configuration.backup().chunking().min_size(), 32 * 1024);
    assert_eq!(configuration.backup().chunking().target_size(), 64 * 1024);
    assert_eq!(configuration.backup().chunking().max_size(), 128 * 1024);
    assert_eq!(configuration.backup().concurrency(), Some(8));
    assert_eq!(
        configuration.backup().ignore(),
        [
            String::from("cli"),
            String::from("shared"),
            String::from("z-file")
        ]
        .as_slice()
    );
    assert_eq!(configuration.live().message(), Some("cli-live"));
    assert_eq!(configuration.live().debounce_ms(), Some(300));
    assert_eq!(configuration.live().poll_ms(), Some(400));
    assert_eq!(
        configuration.restore().target_path(),
        Some(invocation.join("cli-restore").as_path())
    );
    Ok(())
}

#[test]
fn ignore_rules_are_sorted_and_deduplicated_across_sources() {
    let merged = gib::merge_ignore_rules(
        &[String::from("node_modules"), String::from(".git")],
        &[
            String::from("coverage"),
            String::from(".git"),
            String::from("node_modules"),
        ],
    );
    assert_eq!(merged, [".git", "coverage", "node_modules"]);
}

#[test]
fn discovery_uses_the_injected_filesystem_adapter() -> Result<(), Box<dyn std::error::Error>> {
    let directory = TestDirectory::new();
    let nested = directory.path().join("nested");
    fs::create_dir(&nested)?;
    let config_path = directory.path().join("gib.toml");
    fs::write(&config_path, "version = 1\n")?;
    let adapter = RecordingFileSystem {
        calls: Mutex::new(Vec::new()),
    };

    let discovered = ConfigurationResolver::new(adapter).discover(&nested)?;

    assert_eq!(discovered, Some(fs::canonicalize(config_path)?));
    Ok(())
}

struct RecordingFileSystem {
    calls: Mutex<Vec<PathBuf>>,
}

impl ConfigurationFileSystem for RecordingFileSystem {
    fn canonicalize(&self, path: &Path) -> std::io::Result<PathBuf> {
        self.calls
            .lock()
            .expect("recording lock should not be poisoned")
            .push(path.to_path_buf());
        LocalConfigurationFileSystem.canonicalize(path)
    }

    fn metadata(&self, path: &Path) -> std::io::Result<ConfigurationFileMetadata> {
        LocalConfigurationFileSystem.metadata(path)
    }

    fn read(&self, path: &Path) -> std::io::Result<Vec<u8>> {
        LocalConfigurationFileSystem.read(path)
    }
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
