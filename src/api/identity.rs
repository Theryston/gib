use super::client::Gib;
use super::error::{ErrorCode, GibError};
use super::event::{GibEvent, OperationKind, OperationStarted, ProgressEvent};
use crate::config::{self, DEFAULT_AUTHOR, GlobalConfig};
use regex::Regex;
use serde::Serialize;
use sha2::{Digest, Sha256};
use std::collections::HashSet;
use std::path::{Path, PathBuf};

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SetIdentityRequest {
    pub author: String,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct Identity {
    pub author: String,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct IdentityChange {
    pub identity: Identity,
    pub path: PathBuf,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SetupRequest {
    pub root: PathBuf,
    pub recursive: bool,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct SetupSkippedPath {
    pub path: PathBuf,
    pub reason: String,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct SetupResult {
    pub config_created: bool,
    pub detected_storages: Vec<PathBuf>,
    pub configured_storages: Vec<String>,
    pub skipped: Vec<SetupSkippedPath>,
}

impl Gib {
    pub fn get_identity(&self) -> Result<Identity, GibError> {
        self.events()
            .emit(GibEvent::OperationStarted(OperationStarted {
                operation: OperationKind::Identity,
            }));
        let config = config::load_global_config(&self.inner.context.data_dir)
            .map_err(super::error::map_error)?
            .ok_or_else(|| {
                GibError::new(
                    ErrorCode::ConfigurationNotFound,
                    "GIB identity is not configured; call set_identity first",
                )
            })?;
        Ok(Identity {
            author: config.author,
        })
    }

    pub fn set_identity(&self, request: SetIdentityRequest) -> Result<IdentityChange, GibError> {
        self.events()
            .emit(GibEvent::OperationStarted(OperationStarted {
                operation: OperationKind::Identity,
            }));
        if !is_valid_author(&request.author) {
            return Err(GibError::new(
                ErrorCode::InvalidRequest,
                "The author must be in the format 'Firstname Lastname <email>'",
            ));
        }
        self.events().emit(GibEvent::Progress(ProgressEvent {
            operation: OperationKind::Identity,
            phase: "write".to_string(),
            processed: 0,
            total: Some(1),
            percentage: Some(0),
            message: Some("Writing config...".to_string()),
        }));
        let path = self.inner.context.data_dir.join("config.msgpack");
        config::save_global_config(
            &self.inner.context.data_dir,
            &GlobalConfig {
                author: request.author.clone(),
            },
        )
        .map_err(super::error::map_error)?;
        self.events().emit(GibEvent::Progress(ProgressEvent {
            operation: OperationKind::Identity,
            phase: "write".to_string(),
            processed: 1,
            total: Some(1),
            percentage: Some(100),
            message: Some("Config written".to_string()),
        }));
        Ok(IdentityChange {
            identity: Identity {
                author: request.author,
            },
            path,
        })
    }

    pub fn setup(&self, request: SetupRequest) -> Result<SetupResult, GibError> {
        self.events()
            .emit(GibEvent::OperationStarted(OperationStarted {
                operation: OperationKind::Setup,
            }));
        let root = super::client::path_from_context(&self.inner.context, &request.root);
        if !root.is_dir() {
            return Err(GibError::new(
                ErrorCode::InvalidRequest,
                format!("Setup root '{}' is not a directory", root.display()),
            ));
        }
        let config_created = if config::load_global_config(&self.inner.context.data_dir)
            .map_err(super::error::map_error)?
            .is_none()
        {
            config::save_global_config(
                &self.inner.context.data_dir,
                &GlobalConfig {
                    author: DEFAULT_AUTHOR.to_string(),
                },
            )
            .map_err(super::error::map_error)?;
            true
        } else {
            false
        };
        let (detected, mut skipped) = discover_storages(&root, request.recursive)?;
        let existing = self.list_storages()?.into_iter().collect::<Vec<_>>();
        let mut configured = Vec::new();
        for path in &detected {
            if existing.iter().any(|storage| {
                storage
                    .path
                    .as_ref()
                    .is_some_and(|existing| existing == path)
            }) {
                skipped.push(SetupSkippedPath {
                    path: path.clone(),
                    reason: "already configured".to_string(),
                });
                continue;
            }
            let base = path
                .file_name()
                .map(|name| name.to_string_lossy().to_string())
                .unwrap_or_else(|| "storage".to_string());
            let name = unique_storage_name(&base, path, &configured, &existing);
            let storage_config =
                super::storage::StorageConfig::Local(super::storage::LocalStorageConfig {
                    path: path.clone(),
                });
            let record = super::storage::record_from_config(&storage_config)?;
            config::save_storage(&self.inner.context.data_dir, &name, &record)
                .map_err(super::error::map_error)?;
            configured.push(name);
        }
        Ok(SetupResult {
            config_created,
            detected_storages: detected,
            configured_storages: configured,
            skipped,
        })
    }
}

pub(crate) fn is_valid_author(author: &str) -> bool {
    Regex::new(r"^[A-Za-z]+(?: [A-Za-z]+)*(?: )?<[^@ ]+@[^@ ]+\.[^@ >]+>$")
        .map(|pattern| pattern.is_match(author))
        .unwrap_or(false)
}

fn unique_storage_name(
    base: &str,
    path: &Path,
    configured: &[String],
    existing: &[super::storage::StorageInfo],
) -> String {
    let sanitized = base
        .chars()
        .map(|character| {
            if character.is_ascii_alphanumeric() || matches!(character, '-' | '_') {
                character
            } else {
                '-'
            }
        })
        .collect::<String>();
    let sanitized = if sanitized.is_empty() {
        "storage".to_string()
    } else {
        sanitized
    };
    if !configured.iter().any(|name| name == &sanitized)
        && !existing.iter().any(|storage| storage.name == sanitized)
    {
        return sanitized;
    }
    let digest = Sha256::digest(path_key(path).as_bytes());
    let candidate = format!("{sanitized}-{digest:x}");
    let candidate = &candidate[..candidate.len().min(sanitized.len() + 1 + 8)];
    if !configured.iter().any(|name| name == candidate)
        && !existing.iter().any(|storage| storage.name == candidate)
    {
        return candidate.to_string();
    }
    for index in 2.. {
        let candidate = format!("{candidate}-{index}");
        if !configured.iter().any(|name| name == &candidate)
            && !existing.iter().any(|storage| storage.name == candidate)
        {
            return candidate;
        }
    }
    "storage".to_string()
}

const STORAGE_KEY_DIRECTORIES: [&str; 3] = ["backups", "chunks", "indexes"];

const BLACKLISTED_DIRECTORY_NAMES: &[&str] = &[
    ".git",
    ".cache",
    ".cargo",
    ".codex",
    ".idea",
    ".next",
    ".npm",
    ".nuxt",
    ".rustup",
    ".terraform",
    ".tox",
    ".venv",
    ".vscode",
    "build",
    "coverage",
    "dist",
    "node_modules",
    "out",
    "target",
    "tmp",
    "vendor",
];

fn discover_storages(
    root: &Path,
    recursive: bool,
) -> Result<(Vec<PathBuf>, Vec<SetupSkippedPath>), GibError> {
    let root = std::fs::canonicalize(root).unwrap_or_else(|_| root.to_path_buf());
    let mut detected = Vec::new();
    let mut skipped = Vec::new();
    if is_valid_storage(&root)? {
        detected.push(root);
        return Ok((detected, skipped));
    }

    let mut visited = HashSet::new();
    discover_storage_children(&root, recursive, &mut detected, &mut skipped, &mut visited)?;
    detected.sort();
    detected.dedup();
    Ok((detected, skipped))
}

fn discover_storage_children(
    root: &Path,
    recursive: bool,
    detected: &mut Vec<PathBuf>,
    skipped: &mut Vec<SetupSkippedPath>,
    visited: &mut HashSet<String>,
) -> Result<(), GibError> {
    if !visited.insert(path_key(root)) {
        return Ok(());
    }
    let mut directories = Vec::new();
    for entry in std::fs::read_dir(root).map_err(|error| {
        GibError::new(
            ErrorCode::Io,
            format!("Failed to inspect setup path '{}': {error}", root.display()),
        )
    })? {
        let entry = entry.map_err(|error| GibError::new(ErrorCode::Io, error.to_string()))?;
        if entry.path().is_dir() {
            directories.push(std::fs::canonicalize(entry.path()).unwrap_or_else(|_| entry.path()));
        }
    }
    directories.sort();

    for directory in directories {
        if is_blacklisted_directory(&directory) {
            skipped.push(SetupSkippedPath {
                path: directory,
                reason: "blacklisted directory".to_string(),
            });
            continue;
        }
        if is_valid_storage(&directory)? {
            detected.push(directory);
        } else if recursive {
            discover_storage_children(&directory, recursive, detected, skipped, visited)?;
        }
    }
    Ok(())
}

fn is_valid_storage(path: &Path) -> Result<bool, GibError> {
    for entry in
        std::fs::read_dir(path).map_err(|error| GibError::new(ErrorCode::Io, error.to_string()))?
    {
        let entry = entry.map_err(|error| GibError::new(ErrorCode::Io, error.to_string()))?;
        let candidate = entry.path();
        if candidate.is_dir() && !is_blacklisted_directory(&candidate) && is_storage_key(&candidate)
        {
            return Ok(true);
        }
    }
    Ok(false)
}

fn is_storage_key(path: &Path) -> bool {
    STORAGE_KEY_DIRECTORIES
        .iter()
        .all(|directory| path.join(directory).is_dir())
}

fn is_blacklisted_directory(path: &Path) -> bool {
    let name_blacklisted = path
        .file_name()
        .and_then(|name| name.to_str())
        .is_some_and(|name| {
            BLACKLISTED_DIRECTORY_NAMES
                .iter()
                .any(|candidate| directory_name_matches(name, candidate))
        });
    name_blacklisted || is_blacklisted_system_path(path)
}

fn directory_name_matches(actual: &str, expected: &str) -> bool {
    if cfg!(any(target_os = "windows", target_os = "macos")) {
        actual.eq_ignore_ascii_case(expected)
    } else {
        actual == expected
    }
}

fn is_blacklisted_system_path(path: &Path) -> bool {
    #[cfg(target_os = "linux")]
    {
        return [
            "/proc",
            "/sys",
            "/dev",
            "/run",
            "/tmp",
            "/var/tmp",
            "/lost+found",
        ]
        .iter()
        .any(|root| path == Path::new(root));
    }
    #[cfg(target_os = "macos")]
    {
        return [
            "/System",
            "/Library",
            "/Applications",
            "/Volumes",
            "/Network",
        ]
        .iter()
        .any(|root| path == Path::new(root));
    }
    #[cfg(target_os = "windows")]
    {
        return path.components().any(|component| {
            let value = component.as_os_str().to_string_lossy();
            ["Windows", "Program Files", "ProgramData", "$Recycle.Bin"]
                .iter()
                .any(|root| value.eq_ignore_ascii_case(root))
        });
    }
    #[cfg(not(any(target_os = "linux", target_os = "macos", target_os = "windows")))]
    {
        let _ = path;
        false
    }
}

fn path_key(path: &Path) -> String {
    let value = path.to_string_lossy().replace('\\', "/");
    if cfg!(any(target_os = "windows", target_os = "macos")) {
        value.to_ascii_lowercase()
    } else {
        value
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::{SystemTime, UNIX_EPOCH};

    #[test]
    fn setup_discovers_storage_roots_instead_of_repository_keys() {
        let suffix = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("system clock should be after the Unix epoch")
            .as_nanos();
        let root = std::env::temp_dir().join(format!("gib-setup-{suffix}"));
        let key = root.join("repository-storage").join("project");
        for directory in ["backups", "chunks", "indexes"] {
            std::fs::create_dir_all(key.join(directory)).expect("storage directory should exist");
        }

        let (detected, skipped) = discover_storages(&root, false).expect("discovery should work");
        assert!(skipped.is_empty());
        assert_eq!(detected, vec![root.join("repository-storage")]);

        std::fs::remove_dir_all(root).expect("temporary setup tree should be removed");
    }
}
