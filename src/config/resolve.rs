use super::local::LocalConfigContext;
use crate::commands::storage::add::Storage;
use crate::core::crypto::get_password;
use crate::fs::FS;
use crate::output::{emit_named_event, is_json_mode};
use crate::utils::{get_fs, get_pwd_string};
use clap::ArgMatches;
use console::style;
use dialoguer::Select;
use dirs::home_dir;
use serde::Serialize;
use std::collections::BTreeSet;
use std::fs;
use std::path::Path;
use std::sync::Arc;

#[derive(Clone, Copy)]
pub(crate) struct PasswordPolicy {
    pub(crate) required: bool,
    pub(crate) readonly: bool,
}

#[derive(Clone)]
pub(crate) struct RepositoryOptions {
    pub(crate) key: String,
    pub(crate) storage: String,
    pub(crate) password: Option<String>,
    pub(crate) fs: Arc<dyn FS>,
}

#[derive(Serialize)]
struct LocalConfigEvent {
    loaded: bool,
    path: Option<String>,
}

pub(crate) fn load_and_report_local_config(
    matches: &ArgMatches,
) -> Result<LocalConfigContext, String> {
    let context = super::local::load_local_config(matches)?;
    report_local_config(&context);
    Ok(context)
}

pub(crate) fn report_local_config(context: &LocalConfigContext) {
    if is_json_mode() {
        emit_named_event(
            "config",
            &LocalConfigEvent {
                loaded: context.is_loaded(),
                path: context
                    .path
                    .as_ref()
                    .map(|path| path.to_string_lossy().to_string()),
            },
        );
    } else if let Some(path) = &context.path {
        println!(
            "{} {}",
            style("Loaded local config").cyan().bold(),
            path.display()
        );
    }
}

pub(crate) fn resolve_repository(
    matches: &ArgMatches,
    context: &LocalConfigContext,
    password_policy: PasswordPolicy,
    default_key: Option<String>,
) -> Result<RepositoryOptions, String> {
    let password = matches
        .get_one::<String>("password")
        .map(ToString::to_string)
        .or_else(|| get_password(password_policy.required, password_policy.readonly));

    let default_key = default_key.unwrap_or_else(|| {
        Path::new(&get_pwd_string())
            .file_name()
            .map(|name| name.to_string_lossy().to_string())
            .unwrap_or_else(|| "repository".to_string())
    });
    let key = matches
        .get_one::<String>("key")
        .map(ToString::to_string)
        .or_else(|| context.config.repository.key.clone())
        .unwrap_or(default_key);

    let storage_dir = home_dir()
        .ok_or_else(|| "Failed to get home directory".to_string())?
        .join(".gib")
        .join("storages");
    let storage_names = list_storage_names(&storage_dir)?;

    let storage = match matches
        .get_one::<String>("storage")
        .map(ToString::to_string)
        .or_else(|| context.config.repository.storage.clone())
    {
        Some(storage) => storage,
        None => {
            if is_json_mode() {
                return Err(
                    "Missing required argument: --storage (or repository.storage in gib.toml) (required in --mode json)".to_string(),
                );
            }
            let selected_index = Select::new()
                .with_prompt("Select the storage to use")
                .items(&storage_names)
                .default(0)
                .interact()
                .map_err(|error| error.to_string())?;
            storage_names[selected_index].clone()
        }
    };

    if !storage_names.iter().any(|name| name == &storage) {
        return Err(format!("Storage '{}' not found", storage));
    }

    let storage_config = load_storage_config(&storage_dir, &storage)?;
    if storage_config.storage_type == 0 && storage_config.path.is_none() {
        return Err(format!("Local storage '{}' has no path", storage));
    }
    if storage_config.storage_type > 1 {
        return Err(format!("Storage '{}' has an invalid storage type", storage));
    }

    let fs = get_fs(&storage_config, None);

    Ok(RepositoryOptions {
        key,
        storage,
        password,
        fs,
    })
}

pub(crate) fn resolve_path(
    cli_value: Option<&String>,
    config_value: Option<&String>,
    context: &LocalConfigContext,
) -> Result<String, String> {
    let current_dir = std::env::current_dir()
        .map_err(|error| format!("Failed to get the current directory: {}", error))?;
    let (value, base_dir) = match (cli_value, config_value) {
        (Some(value), _) => (value.as_str(), current_dir.as_path()),
        (None, Some(value)) => (value.as_str(), context.base_dir.as_path()),
        (None, None) => return Ok(current_dir.to_string_lossy().to_string()),
    };

    let path = Path::new(value);
    let resolved = if path.is_absolute() {
        path.to_path_buf()
    } else {
        base_dir.join(path)
    };

    Ok(resolved.to_string_lossy().to_string())
}

pub(crate) fn merge_ignore_patterns(
    config_values: &[String],
    cli_values: &[String],
) -> Vec<String> {
    config_values
        .iter()
        .chain(cli_values)
        .cloned()
        .collect::<BTreeSet<_>>()
        .into_iter()
        .collect()
}

fn list_storage_names(storage_dir: &Path) -> Result<Vec<String>, String> {
    if !storage_dir.exists() {
        return Err("Seems like you didn't create any storage yet. Run 'gib storage add' to create a storage.".to_string());
    }

    let mut names = fs::read_dir(storage_dir)
        .map_err(|error| format!("Failed to read storages: {}", error))?
        .map(|entry| {
            entry
                .map_err(|error| format!("Failed to read storage entry: {}", error))
                .and_then(|entry| {
                    entry
                        .path()
                        .file_stem()
                        .map(|stem| stem.to_string_lossy().to_string())
                        .ok_or_else(|| "Storage entry has no name".to_string())
                })
        })
        .collect::<Result<Vec<_>, _>>()?;

    names.sort();
    names.dedup();
    if names.is_empty() {
        return Err("Seems like you didn't create any storage yet. Run 'gib storage add' to create a storage.".to_string());
    }
    Ok(names)
}

fn load_storage_config(storage_dir: &Path, name: &str) -> Result<Storage, String> {
    let path = storage_dir.join(format!("{}.msgpack", name));
    let bytes =
        fs::read(&path).map_err(|error| format!("Failed to read storage '{}': {}", name, error))?;
    rmp_serde::from_slice(&bytes)
        .map_err(|error| format!("Failed to parse storage '{}': {}", name, error))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    #[test]
    fn merges_and_deduplicates_ignore_patterns_deterministically() {
        assert_eq!(
            merge_ignore_patterns(
                &["node_modules".to_string(), ".git".to_string()],
                &["coverage".to_string(), ".git".to_string()]
            ),
            vec![".git", "coverage", "node_modules"]
        );
    }

    #[test]
    fn resolves_config_paths_from_the_config_directory_and_cli_wins() {
        let context = LocalConfigContext {
            config: super::super::local::LocalConfig::default(),
            path: Some(PathBuf::from("/workspace/gib.toml")),
            base_dir: PathBuf::from("/workspace"),
        };
        let configured_path = "./restore-output".to_string();
        assert_eq!(
            resolve_path(None, Some(&configured_path), &context).expect("path should resolve"),
            "/workspace/./restore-output"
        );

        let cli_path = "/tmp/cli-restore".to_string();
        assert_eq!(
            resolve_path(Some(&cli_path), Some(&configured_path), &context)
                .expect("CLI path should win"),
            "/tmp/cli-restore"
        );
    }
}
