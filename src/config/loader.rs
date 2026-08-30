use super::model::{
    GlobalConfig, LOCAL_CONFIG_FILE_NAME, LocalConfig, LocalConfigContext, StorageRecord,
};
use parse_size::parse_size;
use std::fs;
use std::path::{Path, PathBuf};

const SUPPORTED_VERSION: u32 = 1;

pub(crate) fn load_local_config(
    root: &Path,
    explicit_path: Option<&Path>,
    discover: bool,
) -> Result<LocalConfigContext, String> {
    let root = fs::canonicalize(root).unwrap_or_else(|_| root.to_path_buf());
    let config_path = explicit_path
        .map(|path| {
            if path.is_absolute() {
                path.to_path_buf()
            } else {
                root.join(path)
            }
        })
        .or_else(|| discover.then(|| discover_config(&root)).flatten());

    match config_path {
        Some(path) => load_config_file(&path),
        None => Ok(LocalConfigContext::without_config(root)),
    }
}

fn discover_config(start_dir: &Path) -> Option<PathBuf> {
    let mut directory = start_dir.to_path_buf();
    loop {
        let candidate = directory.join(LOCAL_CONFIG_FILE_NAME);
        if candidate.is_file() {
            return Some(candidate);
        }
        if !directory.pop() {
            return None;
        }
    }
}

fn load_config_file(path: &Path) -> Result<LocalConfigContext, String> {
    if !path.is_file() {
        return Err(format!(
            "Local config file '{}' does not exist or is not a file",
            path.display()
        ));
    }
    let contents = fs::read_to_string(path).map_err(|error| {
        format!(
            "Failed to read local config '{}': {}",
            path.display(),
            error
        )
    })?;
    let config: LocalConfig = toml::from_str(&contents).map_err(|error| {
        format!(
            "Failed to parse local config '{}': {}",
            path.display(),
            error
        )
    })?;
    validate_config(&config, path)?;

    let absolute_path = fs::canonicalize(path).unwrap_or_else(|_| path.to_path_buf());
    let base_dir = absolute_path
        .parent()
        .map(Path::to_path_buf)
        .ok_or_else(|| format!("Local config '{}' has no parent directory", path.display()))?;
    Ok(LocalConfigContext {
        config,
        path: Some(absolute_path),
        base_dir,
    })
}

fn validate_config(config: &LocalConfig, path: &Path) -> Result<(), String> {
    if let Some(version) = config.version
        && version != SUPPORTED_VERSION
    {
        return Err(format!(
            "Unsupported version {} in local config '{}'; supported version is {}",
            version,
            path.display(),
            SUPPORTED_VERSION
        ));
    }
    if config
        .repository
        .storage
        .as_deref()
        .is_some_and(|value| value.trim().is_empty())
    {
        return Err(format!(
            "The repository.storage value in '{}' cannot be empty",
            path.display()
        ));
    }
    if config
        .repository
        .key
        .as_deref()
        .is_some_and(|value| value.trim().is_empty())
    {
        return Err(format!(
            "The repository.key value in '{}' cannot be empty",
            path.display()
        ));
    }
    if config.backup.concurrency == Some(0) {
        return Err(format!(
            "The backup.concurrency value in '{}' must be greater than zero",
            path.display()
        ));
    }
    if let Some(compress) = config.backup.compress
        && !(1..=22).contains(&compress)
    {
        return Err(format!(
            "The backup.compress value in '{}' must be between 1 and 22",
            path.display()
        ));
    }
    if let Some(chunk_size) = &config.backup.chunk_size {
        let parsed = parse_size(chunk_size).map_err(|_| {
            format!(
                "The backup.chunk_size value in '{}' must be a valid size",
                path.display()
            )
        })?;
        if parsed == 0 {
            return Err(format!(
                "The backup.chunk_size value in '{}' must be greater than zero",
                path.display()
            ));
        }
    }
    if config.live.debounce_ms == Some(0) || config.live.poll_ms == Some(0) {
        return Err(format!(
            "The live timing values in '{}' must be greater than zero",
            path.display()
        ));
    }
    Ok(())
}

pub(crate) fn load_global_config(data_dir: &Path) -> Result<Option<GlobalConfig>, String> {
    let path = data_dir.join("config.msgpack");
    if !path.is_file() {
        return Ok(None);
    }
    let bytes = fs::read(&path)
        .map_err(|error| format!("Failed to read config '{}': {error}", path.display()))?;
    rmp_serde::from_slice(&bytes)
        .map(Some)
        .map_err(|error| format!("Failed to parse config '{}': {error}", path.display()))
}

pub(crate) fn save_global_config(data_dir: &Path, config: &GlobalConfig) -> Result<(), String> {
    fs::create_dir_all(data_dir).map_err(|error| {
        format!(
            "Failed to create config directory '{}': {error}",
            data_dir.display()
        )
    })?;
    let bytes = rmp_serde::to_vec_named(config)
        .map_err(|error| format!("Failed to serialize config: {error}"))?;
    fs::write(data_dir.join("config.msgpack"), bytes)
        .map_err(|error| format!("Failed to write config: {error}"))
}

pub(crate) fn list_storage_names(data_dir: &Path) -> Result<Vec<String>, String> {
    let storage_dir = data_dir.join("storages");
    if !storage_dir.exists() {
        return Ok(Vec::new());
    }
    let mut names = Vec::new();
    for entry in
        fs::read_dir(&storage_dir).map_err(|error| format!("Failed to read storages: {error}"))?
    {
        let entry = entry.map_err(|error| format!("Failed to read storage entry: {error}"))?;
        let path = entry.path();
        if path.extension().and_then(|value| value.to_str()) != Some("msgpack") {
            continue;
        }
        if let Some(stem) = path.file_stem() {
            names.push(stem.to_string_lossy().to_string());
        }
    }
    names.sort();
    names.dedup();
    Ok(names)
}

pub(crate) fn load_storage(data_dir: &Path, name: &str) -> Result<StorageRecord, String> {
    let path = data_dir.join("storages").join(format!("{name}.msgpack"));
    let bytes =
        fs::read(&path).map_err(|error| format!("Failed to read storage '{name}': {error}"))?;
    rmp_serde::from_slice(&bytes)
        .map_err(|error| format!("Failed to parse storage '{name}': {error}"))
}

pub(crate) fn save_storage(
    data_dir: &Path,
    name: &str,
    storage: &StorageRecord,
) -> Result<(), String> {
    let storage_dir = data_dir.join("storages");
    fs::create_dir_all(&storage_dir).map_err(|error| {
        format!(
            "Failed to create storage directory '{}': {error}",
            storage_dir.display()
        )
    })?;
    let bytes = rmp_serde::to_vec_named(storage)
        .map_err(|error| format!("Failed to serialize storage '{name}': {error}"))?;
    fs::write(storage_dir.join(format!("{name}.msgpack")), bytes)
        .map_err(|error| format!("Failed to write storage '{name}': {error}"))
}

pub(crate) fn remove_storage(data_dir: &Path, name: &str) -> Result<bool, String> {
    let path = data_dir.join("storages").join(format!("{name}.msgpack"));
    if !path.exists() {
        return Ok(false);
    }
    fs::remove_file(&path)
        .map_err(|error| format!("Failed to remove storage '{name}': {error}"))?;
    Ok(true)
}
