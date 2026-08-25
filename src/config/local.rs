use clap::ArgMatches;
use parse_size::parse_size;
use serde::Deserialize;
use std::fs;
use std::path::{Path, PathBuf};

pub(crate) const LOCAL_CONFIG_FILE_NAME: &str = "gib.toml";
const SUPPORTED_VERSION: u32 = 1;

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub(crate) struct LocalConfig {
    pub(crate) version: Option<u32>,
    pub(crate) repository: RepositoryConfig,
    pub(crate) backup: BackupConfig,
    pub(crate) live: LiveConfig,
    pub(crate) restore: RestoreConfig,
}

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub(crate) struct RepositoryConfig {
    pub(crate) storage: Option<String>,
    pub(crate) key: Option<String>,
}

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub(crate) struct BackupConfig {
    pub(crate) root_path: Option<String>,
    pub(crate) message: Option<String>,
    pub(crate) compress: Option<i32>,
    pub(crate) chunk_size: Option<String>,
    pub(crate) concurrency: Option<usize>,
    pub(crate) ignore: Vec<String>,
}

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub(crate) struct LiveConfig {
    pub(crate) message: Option<String>,
    pub(crate) debounce_ms: Option<u64>,
    pub(crate) poll_ms: Option<u64>,
}

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub(crate) struct RestoreConfig {
    pub(crate) target_path: Option<String>,
}

#[derive(Debug, Clone)]
pub(crate) struct LocalConfigContext {
    pub(crate) config: LocalConfig,
    pub(crate) path: Option<PathBuf>,
    pub(crate) base_dir: PathBuf,
}

impl LocalConfigContext {
    pub(crate) fn without_config(base_dir: PathBuf) -> Self {
        Self {
            config: LocalConfig::default(),
            path: None,
            base_dir,
        }
    }

    pub(crate) fn is_loaded(&self) -> bool {
        self.path.is_some()
    }
}

pub(crate) fn load_local_config(matches: &ArgMatches) -> Result<LocalConfigContext, String> {
    let current_dir = std::env::current_dir()
        .map_err(|error| format!("Failed to get the current directory: {}", error))?;

    if matches.get_flag("no-config") {
        return Ok(LocalConfigContext::without_config(current_dir));
    }

    let config_path = match matches.get_one::<String>("config") {
        Some(path) => Some(resolve_explicit_path(path, &current_dir)),
        None => discover_config(&current_dir),
    };

    let Some(config_path) = config_path else {
        return Ok(LocalConfigContext::without_config(current_dir));
    };

    load_config_file(&config_path)
}

pub(crate) fn discover_config(start_dir: &Path) -> Option<PathBuf> {
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

pub(crate) fn load_config_file(path: &Path) -> Result<LocalConfigContext, String> {
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

fn resolve_explicit_path(raw_path: &str, current_dir: &Path) -> PathBuf {
    let path = Path::new(raw_path);
    if path.is_absolute() {
        path.to_path_buf()
    } else {
        current_dir.join(path)
    }
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
        .is_some_and(|storage| storage.trim().is_empty())
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
        .is_some_and(|key| key.trim().is_empty())
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
        let parsed_chunk_size = parse_size(chunk_size).map_err(|_| {
            format!(
                "The backup.chunk_size value in '{}' must be a valid size",
                path.display()
            )
        })?;
        if parsed_chunk_size == 0 {
            return Err(format!(
                "The backup.chunk_size value in '{}' must be greater than zero",
                path.display()
            ));
        }
    }

    if config.live.debounce_ms == Some(0) {
        return Err(format!(
            "The live.debounce_ms value in '{}' must be greater than zero",
            path.display()
        ));
    }

    if config.live.poll_ms == Some(0) {
        return Err(format!(
            "The live.poll_ms value in '{}' must be greater than zero",
            path.display()
        ));
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::{SystemTime, UNIX_EPOCH};

    fn temporary_directory() -> PathBuf {
        let suffix = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("system clock should be after unix epoch")
            .as_nanos();
        let path = std::env::temp_dir().join(format!("gib-local-config-{suffix}"));
        fs::create_dir_all(&path).expect("temporary directory should be created");
        path
    }

    #[test]
    fn discovers_the_nearest_config_file() {
        let root = temporary_directory();
        let child = root.join("project").join("src");
        fs::create_dir_all(&child).expect("child directory should be created");
        fs::write(root.join(LOCAL_CONFIG_FILE_NAME), "version = 1\n")
            .expect("root config should be written");
        fs::write(
            child.parent().unwrap().join(LOCAL_CONFIG_FILE_NAME),
            "version = 1\n",
        )
        .expect("child config should be written");

        assert_eq!(
            discover_config(&child),
            Some(child.parent().unwrap().join(LOCAL_CONFIG_FILE_NAME))
        );

        fs::remove_dir_all(root).expect("temporary directory should be removed");
    }

    #[test]
    fn parses_optional_sections_and_rejects_unknown_keys() {
        let root = temporary_directory();
        let path = root.join(LOCAL_CONFIG_FILE_NAME);
        fs::write(
            &path,
            "[backup]\nignore = [\"node_modules\"]\n[restore]\ntarget_path = \"out\"\n",
        )
        .expect("config should be written");

        let context = load_config_file(&path).expect("optional config should parse");
        assert_eq!(context.config.backup.ignore, vec!["node_modules"]);
        assert_eq!(context.config.restore.target_path.as_deref(), Some("out"));

        fs::write(&path, "[backup]\nunknown = true\n").expect("invalid config should be written");
        let error = load_config_file(&path).expect_err("unknown keys should be rejected");
        assert!(error.contains("unknown"));

        fs::write(&path, "version = 2\n").expect("unsupported version should be written");
        let error = load_config_file(&path).expect_err("unsupported versions should be rejected");
        assert!(error.contains("Unsupported version"));

        fs::write(&path, "[backup]\nchunk_size = \"not-a-size\"\n")
            .expect("invalid size should be written");
        let error = load_config_file(&path).expect_err("invalid sizes should be rejected");
        assert!(error.contains("backup.chunk_size"));

        fs::remove_dir_all(root).expect("temporary directory should be removed");
    }
}
