use clap::ArgMatches;
use console::style;
use dirs::home_dir;
use rmp_serde::Serializer;
use serde::Serialize;
use sha2::{Digest, Sha256};
use std::collections::{BTreeMap, HashSet};
use std::fs::{self, OpenOptions};
use std::io::Write;
use std::path::{Path, PathBuf};

use crate::commands::config::{Config, DEFAULT_AUTHOR};
use crate::commands::storage::add::Storage;
use crate::output::{emit_output, is_json_mode};
use crate::utils::handle_error;

const REPOSITORY_DIRECTORIES: [&str; 3] = ["backups", "chunks", "indexes"];

// Keep directory-name rules in one list so adding another development or build
// directory does not require changing the traversal algorithm.
const BLACKLISTED_DIRECTORY_NAMES: &[&str] = &[
    ".git",
    ".svn",
    ".hg",
    ".bzr",
    "node_modules",
    "bower_components",
    "jspm_packages",
    "vendor",
    "Pods",
    "Carthage",
    "__pycache__",
    ".venv",
    "venv",
    "env",
    ".tox",
    ".nox",
    ".mypy_cache",
    ".pytest_cache",
    ".ruff_cache",
    ".gradle",
    ".m2",
    ".npm",
    ".pnpm-store",
    ".yarn",
    ".cache",
    ".cargo",
    ".rustup",
    ".stack-work",
    ".dart_tool",
    ".pub-cache",
    ".terraform",
    ".terragrunt-cache",
    ".idea",
    ".vscode",
    ".vs",
    "target",
    "bin",
    "obj",
    "build",
    "dist",
    "out",
    ".next",
    ".nuxt",
    ".svelte-kit",
    ".angular",
    ".parcel-cache",
    ".turbo",
    "coverage",
];

#[cfg(target_os = "linux")]
const LINUX_SYSTEM_PATHS: &[&str] = &[
    "/proc",
    "/sys",
    "/dev",
    "/run",
    "/tmp",
    "/var/tmp",
    "/lost+found",
    "/boot",
    "/bin",
    "/sbin",
    "/lib",
    "/lib64",
    "/usr",
    "/opt",
    "/snap",
    "/nix",
];

#[cfg(target_os = "macos")]
const MACOS_SYSTEM_PATHS: &[&str] = &[
    "/System",
    "/Library",
    "/Applications",
    "/Volumes",
    "/Network",
    "/private",
    "/usr",
    "/bin",
    "/sbin",
    "/etc",
    "/var",
    "/tmp",
    "/dev",
];

#[cfg(target_os = "windows")]
const WINDOWS_SYSTEM_DIRECTORY_NAMES: &[&str] = &[
    "Windows",
    "Program Files",
    "Program Files (x86)",
    "ProgramData",
    "$Recycle.Bin",
    "System Volume Information",
    "Recovery",
    "Documents and Settings",
    "MSOCache",
    "PerfLogs",
    "Windows.old",
    "WindowsApps",
];

#[cfg(any(target_os = "macos", target_os = "windows"))]
const ADOBE_DIRECTORY_NAMES: &[&str] = &[
    "Adobe",
    "AdobeGCClient",
    "Adobe Creative Cloud",
    "Adobe Desktop Service",
    "Adobe Genuine Service",
    "Adobe Premiere Pro",
    "Adobe After Effects",
    "Adobe Photoshop",
    "Adobe Illustrator",
    "Adobe Media Encoder",
    "Adobe Camera Raw",
    "Adobe Common",
];

#[derive(Debug, Serialize, PartialEq, Eq)]
struct SkippedPath {
    path: String,
    reason: String,
}

#[derive(Debug, Serialize, PartialEq, Eq)]
struct SetupSummary {
    config_created: bool,
    detected_repositories: Vec<String>,
    configured_storages: Vec<String>,
    skipped: Vec<SkippedPath>,
}

#[derive(Default)]
struct Discovery {
    repositories: Vec<PathBuf>,
    skipped: Vec<SkippedPath>,
}

struct RegisteredStorage {
    name: String,
    storage: Storage,
}

pub fn setup(matches: &ArgMatches) {
    let root = std::env::current_dir().unwrap_or_else(|e| {
        handle_error(
            format!("Failed to determine current directory: {}", e),
            None,
        )
    });
    let home = home_dir()
        .unwrap_or_else(|| handle_error("Failed to determine home directory".to_string(), None));
    let gib_dir = home.join(".gib");
    let recursive = !matches.get_flag("no-recursive");

    let summary =
        perform_setup(&root, recursive, &gib_dir).unwrap_or_else(|e| handle_error(e, None));

    if is_json_mode() {
        emit_output(&summary);
    } else {
        display_summary(&summary, recursive);
    }
}

fn perform_setup(root: &Path, recursive: bool, gib_dir: &Path) -> Result<SetupSummary, String> {
    let config_created = ensure_default_config(&gib_dir.join("config.msgpack"))?;
    let storage_dir = gib_dir.join("storages");
    let mut registered_storages = load_registered_storages(&storage_dir)?;
    let discovery = discover_repositories(root, recursive)?;

    let mut summary = SetupSummary {
        config_created,
        detected_repositories: discovery
            .repositories
            .iter()
            .map(|path| path.to_string_lossy().to_string())
            .collect(),
        configured_storages: Vec::new(),
        skipped: discovery.skipped,
    };

    let mut occupied_names: HashSet<String> = registered_storages
        .iter()
        .map(|storage| storage_name_key(&storage.name))
        .collect();

    for repository in discovery.repositories {
        if registered_storages.iter().any(|registered| {
            registered.storage.storage_type == 0
                && registered
                    .storage
                    .path
                    .as_deref()
                    .is_some_and(|path| paths_equal(Path::new(path), &repository))
        }) {
            summary.skipped.push(SkippedPath {
                path: repository.to_string_lossy().to_string(),
                reason: "already configured".to_string(),
            });
            continue;
        }

        let name = allocate_storage_name(&repository, &occupied_names);
        let storage = local_storage_for(&repository);
        write_new_storage(&storage_dir, &name, &storage)?;

        occupied_names.insert(storage_name_key(&name));
        registered_storages.push(RegisteredStorage {
            name: name.clone(),
            storage,
        });
        summary.configured_storages.push(name);
    }

    Ok(summary)
}

fn ensure_default_config(config_path: &Path) -> Result<bool, String> {
    if config_path.exists() {
        return Ok(false);
    }

    let parent = config_path
        .parent()
        .ok_or_else(|| "Global config path has no parent directory".to_string())?;
    fs::create_dir_all(parent).map_err(|e| {
        format!(
            "Failed to create config directory '{}': {}",
            parent.display(),
            e
        )
    })?;

    let config = Config {
        author: DEFAULT_AUTHOR.to_string(),
    };
    let bytes = serialize(&config, "config")?;

    let mut file = match OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(config_path)
    {
        Ok(file) => file,
        Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => return Ok(false),
        Err(error) => {
            return Err(format!(
                "Failed to create global config '{}': {}",
                config_path.display(),
                error
            ));
        }
    };

    if let Err(error) = file.write_all(&bytes) {
        let _ = fs::remove_file(config_path);
        return Err(format!(
            "Failed to write global config '{}': {}",
            config_path.display(),
            error
        ));
    }

    Ok(true)
}

fn load_registered_storages(storage_dir: &Path) -> Result<Vec<RegisteredStorage>, String> {
    if !storage_dir.exists() {
        return Ok(Vec::new());
    }

    let entries = fs::read_dir(storage_dir).map_err(|e| {
        format!(
            "Failed to read storage directory '{}': {}",
            storage_dir.display(),
            e
        )
    })?;
    let mut registered = Vec::new();

    for entry in entries {
        let entry = entry.map_err(|e| format!("Failed to read storage entry: {}", e))?;
        let path = entry.path();
        if !entry
            .file_type()
            .map_err(|e| {
                format!(
                    "Failed to inspect storage entry '{}': {}",
                    path.display(),
                    e
                )
            })?
            .is_file()
        {
            continue;
        }

        if path.extension().and_then(|extension| extension.to_str()) != Some("msgpack") {
            continue;
        }

        let name = path
            .file_stem()
            .map(|stem| stem.to_string_lossy().to_string())
            .ok_or_else(|| format!("Storage file '{}' has no name", path.display()))?;
        let bytes =
            fs::read(&path).map_err(|e| format!("Failed to read storage '{}': {}", name, e))?;
        let storage: Storage = rmp_serde::from_slice(&bytes)
            .map_err(|e| format!("Failed to parse storage '{}': {}", name, e))?;

        registered.push(RegisteredStorage { name, storage });
    }

    registered.sort_by(|left, right| left.name.cmp(&right.name));
    Ok(registered)
}

fn discover_repositories(root: &Path, recursive: bool) -> Result<Discovery, String> {
    let root = normalize_existing_path(root);
    let mut discovery = Discovery::default();
    let mut visited = HashSet::new();
    discover_children(&root, recursive, &mut discovery, &mut visited)?;
    Ok(discovery)
}

fn discover_children(
    root: &Path,
    recursive: bool,
    discovery: &mut Discovery,
    visited: &mut HashSet<String>,
) -> Result<(), String> {
    let root_key = path_key(root);
    if !visited.insert(root_key) {
        return Ok(());
    }

    let entries = fs::read_dir(root)
        .map_err(|e| format!("Failed to read directory '{}': {}", root.display(), e))?;
    let mut directories = Vec::new();

    for entry in entries {
        let entry = entry.map_err(|e| format!("Failed to read directory entry: {}", e))?;
        let path = entry.path();
        if path.is_dir() {
            directories.push(normalize_existing_path(&path));
        }
    }

    directories.sort_by(|left, right| left.to_string_lossy().cmp(&right.to_string_lossy()));

    for directory in directories {
        if let Some(reason) = blacklist_reason(&directory) {
            discovery.skipped.push(SkippedPath {
                path: directory.to_string_lossy().to_string(),
                reason: reason.to_string(),
            });
            continue;
        }

        if is_valid_repository(&directory) {
            discovery.repositories.push(directory);
            continue;
        }

        if recursive {
            discover_children(&directory, recursive, discovery, visited)?;
        }
    }

    Ok(())
}

fn is_valid_repository(path: &Path) -> bool {
    REPOSITORY_DIRECTORIES
        .iter()
        .all(|directory| path.join(directory).is_dir())
}

fn blacklist_reason(path: &Path) -> Option<&'static str> {
    if is_blacklisted_directory_name(path)
        || is_blacklisted_system_path(path)
        || is_blacklisted_adobe_path(path)
    {
        Some("blacklisted directory")
    } else {
        None
    }
}

fn is_blacklisted_directory_name(path: &Path) -> bool {
    let Some(name) = path.file_name().and_then(|name| name.to_str()) else {
        return false;
    };

    BLACKLISTED_DIRECTORY_NAMES
        .iter()
        .any(|candidate| directory_name_matches(name, candidate))
}

fn directory_name_matches(actual: &str, expected: &str) -> bool {
    if is_case_insensitive_filesystem() {
        actual.eq_ignore_ascii_case(expected)
    } else {
        actual == expected
    }
}

const fn is_case_insensitive_filesystem() -> bool {
    cfg!(any(target_os = "windows", target_os = "macos"))
}

#[cfg(target_os = "linux")]
fn is_blacklisted_system_path(path: &Path) -> bool {
    LINUX_SYSTEM_PATHS
        .iter()
        .any(|root| paths_equal(path, Path::new(root)))
}

#[cfg(target_os = "macos")]
fn is_blacklisted_system_path(path: &Path) -> bool {
    MACOS_SYSTEM_PATHS
        .iter()
        .any(|root| paths_equal(path, Path::new(root)))
}

#[cfg(target_os = "windows")]
fn is_blacklisted_system_path(path: &Path) -> bool {
    WINDOWS_SYSTEM_DIRECTORY_NAMES
        .iter()
        .any(|root| windows_path_has_root_directory(path, root))
}

#[cfg(not(any(target_os = "linux", target_os = "macos", target_os = "windows")))]
fn is_blacklisted_system_path(_path: &Path) -> bool {
    false
}

#[cfg(target_os = "windows")]
fn is_blacklisted_adobe_path(path: &Path) -> bool {
    if !path
        .file_name()
        .and_then(|name| name.to_str())
        .is_some_and(|name| {
            ADOBE_DIRECTORY_NAMES
                .iter()
                .any(|candidate| name.eq_ignore_ascii_case(candidate))
        })
    {
        return false;
    }

    ["Program Files", "Program Files (x86)", "ProgramData"]
        .iter()
        .any(|root| windows_path_has_directory(path, root))
}

#[cfg(target_os = "macos")]
fn is_blacklisted_adobe_path(path: &Path) -> bool {
    if !path
        .file_name()
        .and_then(|name| name.to_str())
        .is_some_and(|name| {
            ADOBE_DIRECTORY_NAMES
                .iter()
                .any(|candidate| name.eq_ignore_ascii_case(candidate))
        })
    {
        return false;
    }

    [
        "/Applications",
        "/Library/Application Support",
        "/Library/Preferences",
        "/Users/Shared",
    ]
    .iter()
    .any(|root| path_is_within(path, Path::new(root)))
}

#[cfg(not(any(target_os = "macos", target_os = "windows")))]
fn is_blacklisted_adobe_path(_path: &Path) -> bool {
    false
}

#[cfg(target_os = "windows")]
fn windows_path_has_root_directory(path: &Path, expected: &str) -> bool {
    windows_path_components(path)
        .first()
        .filter(|_| windows_path_components(path).len() == 1)
        .is_some_and(|actual| actual.eq_ignore_ascii_case(expected))
}

#[cfg(target_os = "windows")]
fn windows_path_has_directory(path: &Path, expected: &str) -> bool {
    windows_path_components(path)
        .iter()
        .any(|actual| actual.eq_ignore_ascii_case(expected))
}

#[cfg(target_os = "windows")]
fn windows_path_components(path: &Path) -> Vec<String> {
    let mut path = path.to_string_lossy().replace('/', "\\");
    if let Some(stripped) = path.strip_prefix(r"\\?\") {
        path = stripped.to_string();
    }

    if path.len() < 3
        || !path.as_bytes()[0].is_ascii_alphabetic()
        || path.as_bytes()[1] != b':'
        || path.as_bytes()[2] != b'\\'
    {
        return Vec::new();
    }
    let rest = &path[3..];

    rest.split('\\')
        .filter(|component| !component.is_empty())
        .map(str::to_string)
        .collect()
}

#[cfg(target_os = "macos")]
fn path_is_within(path: &Path, root: &Path) -> bool {
    let path = normalize_existing_path(path);
    let root = normalize_existing_path(root);
    path == root || path.starts_with(root)
}

fn normalize_existing_path(path: &Path) -> PathBuf {
    let absolute = if path.is_absolute() {
        path.to_path_buf()
    } else {
        std::env::current_dir()
            .map(|current| current.join(path))
            .unwrap_or_else(|_| path.to_path_buf())
    };

    fs::canonicalize(&absolute).unwrap_or(absolute)
}

fn path_key(path: &Path) -> String {
    let path = normalize_existing_path(path);
    let value = path.to_string_lossy().replace('\\', "/");
    if is_case_insensitive_filesystem() {
        value.to_ascii_lowercase()
    } else {
        value
    }
}

fn paths_equal(left: &Path, right: &Path) -> bool {
    path_key(left) == path_key(right)
}

fn local_storage_for(repository: &Path) -> Storage {
    Storage {
        storage_type: 0,
        path: Some(repository.to_string_lossy().to_string()),
        region: None,
        bucket: None,
        access_key: None,
        secret_key: None,
        endpoint: None,
    }
}

fn allocate_storage_name(repository: &Path, occupied_names: &HashSet<String>) -> String {
    let preferred = sanitize_storage_name(
        repository
            .file_name()
            .and_then(|name| name.to_str())
            .unwrap_or("repository"),
    );

    if !occupied_names.contains(&storage_name_key(&preferred)) {
        return preferred;
    }

    let digest = Sha256::digest(path_key(repository).as_bytes());
    let suffix = format!("{:x}", digest);
    let base = format!("{}-{}", preferred, &suffix[..8]);
    if !occupied_names.contains(&storage_name_key(&base)) {
        return base;
    }

    let mut counter = 2;
    loop {
        let candidate = format!("{}-{}", base, counter);
        if !occupied_names.contains(&storage_name_key(&candidate)) {
            return candidate;
        }
        counter += 1;
    }
}

fn sanitize_storage_name(name: &str) -> String {
    let mut sanitized = String::new();
    for character in name.chars() {
        if character.is_ascii_alphanumeric() {
            sanitized.push(character);
        } else if !sanitized.ends_with('-') {
            sanitized.push('-');
        }
    }

    let sanitized = sanitized.trim_matches('-').to_string();
    if sanitized.is_empty() {
        "repository".to_string()
    } else {
        sanitized
    }
}

fn storage_name_key(name: &str) -> String {
    if is_case_insensitive_filesystem() {
        name.to_ascii_lowercase()
    } else {
        name.to_string()
    }
}

fn write_new_storage(storage_dir: &Path, name: &str, storage: &Storage) -> Result<(), String> {
    fs::create_dir_all(storage_dir).map_err(|e| {
        format!(
            "Failed to create storage directory '{}': {}",
            storage_dir.display(),
            e
        )
    })?;

    let path = storage_dir.join(format!("{}.msgpack", name));
    let bytes = serialize(storage, &format!("storage '{}'", name))?;
    let mut file = OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&path)
        .map_err(|e| format!("Failed to create storage '{}': {}", name, e))?;

    if let Err(error) = file.write_all(&bytes) {
        let _ = fs::remove_file(&path);
        return Err(format!("Failed to write storage '{}': {}", name, error));
    }

    Ok(())
}

fn serialize<T: Serialize>(value: &T, label: &str) -> Result<Vec<u8>, String> {
    let mut bytes = Vec::new();
    value
        .serialize(&mut Serializer::new(&mut bytes))
        .map_err(|e| format!("Failed to serialize {}: {}", label, e))?;
    Ok(bytes)
}

fn display_summary(summary: &SetupSummary, recursive: bool) {
    println!();
    println!("{}", style("GIB setup complete").cyan().bold());
    println!(
        "{} {}",
        style("Discovery").bold(),
        if recursive {
            style("recursive").green()
        } else {
            style("direct children only").green()
        }
    );
    println!();

    let config_status = if summary.config_created {
        style("created with the default identity").green()
    } else {
        style("already exists").dim()
    };
    println!("{} {}", style("Global config").bold(), config_status);
    println!(
        "{} {} detected, {} newly configured",
        style("Repositories").bold(),
        style(summary.detected_repositories.len()).cyan(),
        style(summary.configured_storages.len()).green()
    );

    if !summary.configured_storages.is_empty() {
        println!();
        println!("{}", style("New local storages").bold());

        const MAX_STORAGE_NAMES: usize = 10;
        for name in summary.configured_storages.iter().take(MAX_STORAGE_NAMES) {
            println!("  {} {}", style("✓").green(), style(name).white());
        }

        let remaining = summary
            .configured_storages
            .len()
            .saturating_sub(MAX_STORAGE_NAMES);
        if remaining > 0 {
            println!(
                "  {} {} more storages",
                style("…").dim(),
                style(remaining).dim()
            );
        }
    }

    println!();
    if summary.skipped.is_empty() {
        println!("{} {}", style("Skipped").bold(), style("none").green());
    } else {
        let skipped_counts = count_skipped_reasons(&summary.skipped);
        println!(
            "{} {} directories",
            style("Skipped").bold().yellow(),
            style(summary.skipped.len()).yellow()
        );
        for (reason, count) in skipped_counts {
            println!("  {} {:>5}  {}", style("•").yellow(), count, reason);
        }
    }
    println!();
}

fn count_skipped_reasons(skipped: &[SkippedPath]) -> BTreeMap<&str, usize> {
    let mut counts = BTreeMap::new();
    for entry in skipped {
        *counts.entry(entry.reason.as_str()).or_insert(0) += 1;
    }
    counts
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicU64, Ordering};

    static TEST_ID: AtomicU64 = AtomicU64::new(0);

    struct TestDirectory(PathBuf);

    impl TestDirectory {
        fn new() -> Self {
            let id = TEST_ID.fetch_add(1, Ordering::Relaxed);
            let path =
                std::env::temp_dir().join(format!("gib-setup-test-{}-{}", std::process::id(), id));
            fs::create_dir_all(&path).unwrap();
            Self(path)
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

    fn make_repository(path: &Path) {
        for directory in REPOSITORY_DIRECTORIES {
            fs::create_dir_all(path.join(directory)).unwrap();
        }
    }

    #[test]
    fn creates_and_then_preserves_the_default_config() {
        let fixture = TestDirectory::new();
        let gib_dir = fixture.path().join("gib");

        let first = perform_setup(fixture.path(), true, &gib_dir).unwrap();
        assert!(first.config_created);

        let config_bytes = fs::read(gib_dir.join("config.msgpack")).unwrap();
        let config: Config = rmp_serde::from_slice(&config_bytes).unwrap();
        assert_eq!(config.author, DEFAULT_AUTHOR);

        fs::write(gib_dir.join("config.msgpack"), b"preserve me").unwrap();
        let second = perform_setup(fixture.path(), true, &gib_dir).unwrap();
        assert!(!second.config_created);
        assert_eq!(
            fs::read(gib_dir.join("config.msgpack")).unwrap(),
            b"preserve me"
        );
    }

    #[test]
    fn discovers_direct_and_nested_repositories() {
        let fixture = TestDirectory::new();
        make_repository(&fixture.path().join("project-a"));
        make_repository(&fixture.path().join("archives").join("project-b"));

        let recursive = discover_repositories(fixture.path(), true).unwrap();
        assert_eq!(recursive.repositories.len(), 2);

        let direct = discover_repositories(fixture.path(), false).unwrap();
        assert_eq!(direct.repositories.len(), 1);
        assert_eq!(
            direct.repositories[0]
                .file_name()
                .and_then(|name| name.to_str()),
            Some("project-a")
        );
    }

    #[test]
    fn stops_at_a_repository_and_prunes_blacklisted_directories() {
        let fixture = TestDirectory::new();
        let repository = fixture.path().join("project");
        make_repository(&repository);
        make_repository(&repository.join("nested"));
        make_repository(&fixture.path().join(".git").join("nested"));

        let discovery = discover_repositories(fixture.path(), true).unwrap();
        assert_eq!(discovery.repositories.len(), 1);
        assert_eq!(
            discovery.repositories[0],
            normalize_existing_path(&repository)
        );
        assert_eq!(discovery.skipped.len(), 1);
        assert_eq!(discovery.skipped[0].reason, "blacklisted directory");
    }

    #[test]
    fn requires_the_complete_repository_layout() {
        let fixture = TestDirectory::new();
        fs::create_dir_all(fixture.path().join("incomplete").join("backups")).unwrap();

        let discovery = discover_repositories(fixture.path(), true).unwrap();
        assert!(discovery.repositories.is_empty());
    }

    #[test]
    fn setup_is_idempotent_and_uses_collision_safe_names() {
        let fixture = TestDirectory::new();
        make_repository(&fixture.path().join("one").join("repository"));
        make_repository(&fixture.path().join("two").join("repository"));
        let gib_dir = fixture.path().join("gib");

        let first = perform_setup(fixture.path(), true, &gib_dir).unwrap();
        assert_eq!(first.configured_storages.len(), 2);
        assert_ne!(first.configured_storages[0], first.configured_storages[1]);

        let second = perform_setup(fixture.path(), true, &gib_dir).unwrap();
        assert!(second.configured_storages.is_empty());
        assert_eq!(
            second
                .skipped
                .iter()
                .filter(|skipped| skipped.reason == "already configured")
                .count(),
            2
        );

        let storage_count = fs::read_dir(gib_dir.join("storages")).unwrap().count();
        assert_eq!(storage_count, 2);
    }
}
