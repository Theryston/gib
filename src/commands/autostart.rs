use crate::autostart::model::{
    AUTOSTART_JOB_VERSION, AutostartJob, LiveJobOverrides, SecretReferences, validate_name,
};
use crate::autostart::platform::{self, PlatformStatus};
use crate::autostart::registry::{
    RegistryPaths, ensure_registry, find_job_by_name, generate_job_id, list_jobs, registry_paths,
    remove_job, touch_updated_at, write_job,
};
use crate::autostart::runner;
use crate::commands::backup::{LiveOverrides, resolve_live_overrides};
use crate::config::load_local_config_for_root;
use crate::core::secrets;
use crate::output::{emit_named_event, is_json_mode};
use crate::utils::handle_error;
use chrono::Utc;
use clap::ArgMatches;
use console::style;
use dialoguer::{Confirm, Input, Password, Select};
use serde_json::{Value, json};
use std::env;
use std::path::{Path, PathBuf};

pub async fn autostart(matches: &ArgMatches) {
    let result = match matches.subcommand() {
        Some(("add", matches)) => add(matches).await,
        Some(("update", matches)) => update(matches).await,
        Some(("list", matches)) => list(matches),
        Some(("status", matches)) => status(matches),
        Some(("enable", matches)) => enable(matches),
        Some(("disable", matches)) => disable(matches),
        Some(("remove", matches)) => remove(matches),
        Some(("run", matches)) => match matches.get_one::<String>("job-id") {
            Some(job_id) => runner::run(job_id).await,
            None => Err("Missing required autostart job ID".to_string()),
        },
        _ => Err(
            "Invalid autostart command. Run 'gib autostart --help' for more information."
                .to_string(),
        ),
    };

    if let Err(error) = result {
        handle_error(error, None);
    }
}

async fn add(matches: &ArgMatches) -> Result<(), String> {
    let root = root_path(matches, None)?;
    let name = job_name(matches, &root, None)?;
    validate_name(&name)?;
    let config_path = config_path(matches, &root, None)?;
    let password = password_for_add(matches)?;
    let mut overrides = live_overrides(matches, None)?;
    let resolved = resolve_live_overrides(LiveOverrides {
        root_path: root.clone(),
        config_path: config_path.clone(),
        key: overrides.key.clone(),
        storage: overrides.storage.clone(),
        message: overrides.message.clone(),
        compress: overrides.compress,
        chunk_size: overrides.chunk_size.clone(),
        ignore: overrides.ignore.clone(),
        concurrency: overrides.concurrency,
        password: password.clone(),
    })
    .await?;
    persist_effective_repository_values(&mut overrides, &resolved);

    let paths = registry_paths()?;
    ensure_registry(&paths)?;
    let previous = find_job_by_name(&paths, &name)?;
    if previous.is_some() && !matches.get_flag("replace") {
        return Err(format!(
            "An autostart job named '{}' already exists; use --replace to update it",
            name
        ));
    }

    let (id, created_at, previous_job) = match previous {
        Some(job) => (job.id.clone(), job.created_at.clone(), Some(job)),
        None => (generate_job_id(&name, &root), Utc::now().to_rfc3339(), None),
    };
    let enabled = true;
    let identity = identity_from_resolved(&resolved);
    ensure_unique_identity(&paths, &identity, Some(&id), enabled)?;

    let password_ref = match password.as_deref() {
        Some(password) => {
            let reference = previous_job
                .as_ref()
                .and_then(|job| job.secrets.password_ref.clone())
                .unwrap_or_else(|| secrets::password_reference(&id));
            secrets::store_password(&reference, password)?;
            Some(reference)
        }
        None => previous_job
            .as_ref()
            .and_then(|job| job.secrets.password_ref.clone()),
    };

    let job = AutostartJob {
        version: AUTOSTART_JOB_VERSION,
        id,
        name,
        enabled,
        root_path: resolved.options.root_path_string.clone(),
        config_path: config_path.map(|path| path.to_string_lossy().to_string()),
        created_at,
        updated_at: Utc::now().to_rfc3339(),
        overrides,
        secrets: SecretReferences { password_ref },
    };

    let start_now = matches.get_flag("start-now");
    if let Err(error) = install_job(&paths, &job, previous_job.as_ref(), start_now) {
        if password.is_some()
            && previous_job
                .as_ref()
                .and_then(|old| old.secrets.password_ref.as_ref())
                != job.secrets.password_ref.as_ref()
            && let Some(reference) = job.secrets.password_ref.as_deref()
        {
            let _ = secrets::delete_password(reference);
        }
        return Err(error);
    }

    emit_changed("registered", &job, start_now);
    Ok(())
}

async fn update(matches: &ArgMatches) -> Result<(), String> {
    let paths = registry_paths()?;
    ensure_registry(&paths)?;
    let name = matches
        .get_one::<String>("name")
        .ok_or_else(|| "Missing required autostart job name".to_string())?;
    let previous = find_job_by_name(&paths, name)?
        .ok_or_else(|| format!("Autostart job '{}' was not found", name))?;
    let root = root_path(matches, Some(&previous))?;
    let config_path = config_path(matches, &root, Some(&previous))?;
    let password = matches
        .get_one::<String>("password")
        .map(ToString::to_string);
    let mut overrides = live_overrides(matches, Some(&previous))?;
    let resolved = resolve_live_overrides(LiveOverrides {
        root_path: root,
        config_path: config_path.clone(),
        key: overrides.key.clone(),
        storage: overrides.storage.clone(),
        message: overrides.message.clone(),
        compress: overrides.compress,
        chunk_size: overrides.chunk_size.clone(),
        ignore: overrides.ignore.clone(),
        concurrency: overrides.concurrency,
        password: password.clone(),
    })
    .await?;
    persist_effective_repository_values(&mut overrides, &resolved);

    let start_now = previous.enabled || matches.get_flag("start-now");
    let enabled = start_now;
    ensure_unique_identity(
        &paths,
        &identity_from_resolved(&resolved),
        Some(&previous.id),
        enabled,
    )?;

    let password_ref = match password.as_deref() {
        Some(password) => {
            let reference = previous
                .secrets
                .password_ref
                .clone()
                .unwrap_or_else(|| secrets::password_reference(&previous.id));
            secrets::store_password(&reference, password)?;
            Some(reference)
        }
        None => previous.secrets.password_ref.clone(),
    };

    overrides.conflict = conflict_policy(matches, Some(&previous.overrides.conflict))?;
    let mut job = AutostartJob {
        version: AUTOSTART_JOB_VERSION,
        id: previous.id.clone(),
        name: previous.name.clone(),
        enabled,
        root_path: resolved.options.root_path_string.clone(),
        config_path: config_path.map(|path| path.to_string_lossy().to_string()),
        created_at: previous.created_at.clone(),
        updated_at: Utc::now().to_rfc3339(),
        overrides,
        secrets: SecretReferences { password_ref },
    };
    touch_updated_at(&mut job);

    if let Err(error) = install_job(&paths, &job, Some(&previous), start_now) {
        if password.is_some()
            && previous.secrets.password_ref != job.secrets.password_ref
            && let Some(reference) = job.secrets.password_ref.as_deref()
        {
            let _ = secrets::delete_password(reference);
        }
        return Err(error);
    }

    emit_changed("updated", &job, start_now);
    Ok(())
}

fn list(matches: &ArgMatches) -> Result<(), String> {
    let _ = matches;
    let paths = registry_paths()?;
    ensure_registry(&paths)?;
    let jobs = list_jobs(&paths)?;
    let summaries = jobs
        .iter()
        .map(|job| job_summary(job, &paths))
        .collect::<Vec<_>>();

    if is_json_mode() {
        emit_named_event(
            "autostart",
            &json!({
                "event": "listed",
                "jobs": summaries,
            }),
        );
    } else if jobs.is_empty() {
        println!("No autostart jobs configured.");
    } else {
        for summary in summaries {
            print_interactive_summary(&summary);
        }
    }
    Ok(())
}

fn status(matches: &ArgMatches) -> Result<(), String> {
    let paths = registry_paths()?;
    ensure_registry(&paths)?;
    let jobs = if let Some(name) = matches.get_one::<String>("name") {
        vec![
            find_job_by_name(&paths, name)?
                .ok_or_else(|| format!("Autostart job '{}' was not found", name))?,
        ]
    } else {
        list_jobs(&paths)?
    };
    let summaries = jobs
        .iter()
        .map(|job| job_summary(job, &paths))
        .collect::<Vec<_>>();

    if is_json_mode() {
        emit_named_event(
            "autostart",
            &json!({
                "event": "status",
                "jobs": summaries,
            }),
        );
    } else if jobs.is_empty() {
        println!("No autostart jobs configured.");
    } else {
        for summary in summaries {
            print_interactive_summary(&summary);
        }
    }
    Ok(())
}

fn enable(matches: &ArgMatches) -> Result<(), String> {
    let paths = registry_paths()?;
    ensure_registry(&paths)?;
    let name = required_name(matches)?;
    let previous = find_job_by_name(&paths, &name)?
        .ok_or_else(|| format!("Autostart job '{}' was not found", name))?;
    let mut job = previous.clone();
    job.enabled = true;
    touch_updated_at(&mut job);
    install_job(&paths, &job, Some(&previous), true)?;
    emit_changed("enabled", &job, true);
    Ok(())
}

fn disable(matches: &ArgMatches) -> Result<(), String> {
    let paths = registry_paths()?;
    ensure_registry(&paths)?;
    let name = required_name(matches)?;
    let previous = find_job_by_name(&paths, &name)?
        .ok_or_else(|| format!("Autostart job '{}' was not found", name))?;
    let mut job = previous.clone();
    job.enabled = false;
    touch_updated_at(&mut job);
    install_job(&paths, &job, Some(&previous), false)?;
    emit_changed("disabled", &job, false);
    Ok(())
}

fn remove(matches: &ArgMatches) -> Result<(), String> {
    let paths = registry_paths()?;
    ensure_registry(&paths)?;
    let name = required_name(matches)?;
    let job = find_job_by_name(&paths, &name)?
        .ok_or_else(|| format!("Autostart job '{}' was not found", name))?;

    if is_json_mode() && !matches.get_flag("yes") {
        return Err(
            "Confirmation required in --mode json. Re-run with --yes to remove the autostart job."
                .to_string(),
        );
    }
    if !is_json_mode()
        && !Confirm::new()
            .with_prompt(format!(
                "Remove autostart job '{}' without deleting its projects, config, or backups?",
                job.name
            ))
            .default(false)
            .interact()
            .map_err(|error| error.to_string())?
    {
        println!("Autostart removal cancelled.");
        return Ok(());
    }

    platform::remove(&paths, &job)?;
    if let Some(reference) = job.secrets.password_ref.as_deref() {
        secrets::delete_password(reference)?;
    }
    remove_job(&paths, &job.id)?;

    if is_json_mode() {
        emit_named_event(
            "autostart",
            &json!({
                "event": "removed",
                "id": job.id,
                "name": job.name,
                "root_path": job.root_path,
            }),
        );
    } else {
        println!(
            "{} Removed autostart job '{}'.",
            style("OK").green(),
            job.name
        );
    }
    Ok(())
}

fn root_path(matches: &ArgMatches, existing: Option<&AutostartJob>) -> Result<PathBuf, String> {
    let value = match matches.get_one::<String>("root-path") {
        Some(value) => value.to_string(),
        None => match existing {
            Some(job) => job.root_path.clone(),
            None if is_json_mode() => {
                return Err(
                    "Missing required argument: --root-path (required in --mode json)".to_string(),
                );
            }
            None => {
                let current = env::current_dir().map_err(|error| error.to_string())?;
                Input::<String>::new()
                    .with_prompt("Directory to keep backed up and synchronized")
                    .default(current.to_string_lossy().to_string())
                    .interact_text()
                    .map_err(|error| error.to_string())?
            }
        },
    };
    let path = PathBuf::from(value);
    let path = if path.is_absolute() {
        path
    } else {
        env::current_dir()
            .map_err(|error| format!("Failed to get the current directory: {}", error))?
            .join(path)
    };
    let canonical = std::fs::canonicalize(&path).map_err(|error| {
        format!(
            "Failed to resolve live root '{}': {}",
            path.display(),
            error
        )
    })?;
    if !canonical.is_dir() {
        return Err(format!(
            "Live root '{}' is not an existing directory",
            canonical.display()
        ));
    }
    Ok(canonical)
}

fn job_name(
    matches: &ArgMatches,
    root: &Path,
    existing: Option<&AutostartJob>,
) -> Result<String, String> {
    if let Some(value) = matches.get_one::<String>("name") {
        return Ok(value.to_string());
    }
    if let Some(job) = existing {
        return Ok(job.name.clone());
    }
    if is_json_mode() {
        return Err("Missing required argument: --name (required in --mode json)".to_string());
    }
    let default = root
        .file_name()
        .map(|value| value.to_string_lossy().to_string())
        .unwrap_or_else(|| "live".to_string());
    Input::<String>::new()
        .with_prompt("Autostart job name")
        .default(default)
        .interact_text()
        .map_err(|error| error.to_string())
}

fn required_name(matches: &ArgMatches) -> Result<String, String> {
    matches
        .get_one::<String>("name")
        .map(ToString::to_string)
        .ok_or_else(|| "Missing required autostart job name".to_string())
}

fn config_path(
    matches: &ArgMatches,
    _root: &Path,
    existing: Option<&AutostartJob>,
) -> Result<Option<PathBuf>, String> {
    let value = matches
        .get_one::<String>("config")
        .map(ToString::to_string)
        .or_else(|| existing.and_then(|job| job.config_path.clone()));
    let Some(value) = value else {
        return Ok(None);
    };
    let path = PathBuf::from(value);
    if path.is_absolute() {
        Ok(Some(path))
    } else {
        Ok(Some(
            env::current_dir()
                .map_err(|error| format!("Failed to get the current directory: {}", error))?
                .join(path),
        ))
    }
}

fn live_overrides(
    matches: &ArgMatches,
    existing: Option<&AutostartJob>,
) -> Result<LiveJobOverrides, String> {
    let current = existing
        .map(|job| job.overrides.clone())
        .unwrap_or_default();
    let conflict = conflict_policy(matches, existing.map(|job| &job.overrides.conflict))?;
    Ok(LiveJobOverrides {
        storage: matches
            .get_one::<String>("storage")
            .map(ToString::to_string)
            .or(current.storage),
        key: matches
            .get_one::<String>("key")
            .map(ToString::to_string)
            .or(current.key),
        message: matches
            .get_one::<String>("message")
            .map(ToString::to_string)
            .or(current.message),
        compress: parse_optional_i32(matches, "compress")?.or(current.compress),
        chunk_size: matches
            .get_one::<String>("chunk-size")
            .map(ToString::to_string)
            .or(current.chunk_size),
        concurrency: parse_optional_usize(matches, "concurrency")?.or(current.concurrency),
        ignore: matches
            .get_many::<String>("ignore")
            .map(|values| values.map(ToString::to_string).collect())
            .or(current.ignore),
        conflict,
    })
}

fn conflict_policy(matches: &ArgMatches, existing: Option<&String>) -> Result<String, String> {
    if let Some(value) = matches.get_one::<String>("conflict") {
        if value == "local" || value == "remote" {
            return Ok(value.to_string());
        }
        return Err(format!(
            "Unsupported conflict policy '{}'; use 'local' or 'remote'",
            value
        ));
    }
    if let Some(value) = existing {
        return Ok(value.to_string());
    }
    if is_json_mode() {
        return Err("Missing required argument: --conflict (required in --mode json)".to_string());
    }
    let selected = Select::new()
        .with_prompt("Conflict policy for the background live job")
        .items(["local", "remote"])
        .default(0)
        .interact()
        .map_err(|error| error.to_string())?;
    Ok(["local", "remote"][selected].to_string())
}

fn parse_optional_i32(matches: &ArgMatches, id: &str) -> Result<Option<i32>, String> {
    matches
        .get_one::<String>(id)
        .map(|value| {
            value
                .parse::<i32>()
                .map_err(|_| format!("Invalid {} '{}'", id, value))
        })
        .transpose()
}

fn parse_optional_usize(matches: &ArgMatches, id: &str) -> Result<Option<usize>, String> {
    matches
        .get_one::<String>(id)
        .map(|value| {
            value
                .parse::<usize>()
                .map_err(|_| format!("Invalid {} '{}'", id, value))
        })
        .transpose()
}

fn password_for_add(matches: &ArgMatches) -> Result<Option<String>, String> {
    if let Some(password) = matches.get_one::<String>("password") {
        return Ok(Some(password.to_string()));
    }
    if is_json_mode() {
        return Ok(None);
    }
    let should_store = Confirm::new()
        .with_prompt("Store a repository password in the user credential store?")
        .default(false)
        .interact()
        .map_err(|error| error.to_string())?;
    if !should_store {
        return Ok(None);
    }
    Password::new()
        .with_prompt("Repository password")
        .interact()
        .map(Some)
        .map_err(|error| error.to_string())
}

fn identity_from_resolved(
    resolved: &crate::commands::backup::ResolvedBackup,
) -> (String, String, String) {
    (
        resolved.options.root_path_string.clone(),
        resolved.options.storage.clone(),
        resolved.options.key.clone(),
    )
}

fn persist_effective_repository_values(
    overrides: &mut LiveJobOverrides,
    resolved: &crate::commands::backup::ResolvedBackup,
) {
    overrides.storage = Some(resolved.options.storage.clone());
    overrides.key = Some(resolved.options.key.clone());
}

fn identity_from_job(job: &AutostartJob) -> Result<(String, String, String), String> {
    let root = PathBuf::from(&job.root_path);
    let canonical = std::fs::canonicalize(&root).unwrap_or(root.clone());
    let context = load_local_config_for_root(&canonical, job.config_path.as_ref().map(Path::new))?;
    let key = job
        .overrides
        .key
        .clone()
        .or(context.config.repository.key)
        .or_else(|| {
            canonical
                .file_name()
                .map(|name| name.to_string_lossy().to_string())
        })
        .unwrap_or_else(|| "repository".to_string());
    let storage = job
        .overrides
        .storage
        .clone()
        .or(context.config.repository.storage)
        .ok_or_else(|| format!("Autostart job '{}' has no configured storage", job.name))?;
    Ok((canonical.to_string_lossy().to_string(), storage, key))
}

fn ensure_unique_identity(
    paths: &RegistryPaths,
    identity: &(String, String, String),
    excluded_id: Option<&str>,
    enabled: bool,
) -> Result<(), String> {
    if !enabled {
        return Ok(());
    }
    for job in list_jobs(paths)? {
        if !job.enabled || excluded_id.is_some_and(|id| id == job.id) {
            continue;
        }
        if identity_from_job(&job)?.eq(identity) {
            return Err(format!(
                "An enabled autostart job already watches root '{}', storage '{}', and key '{}' (job '{}')",
                identity.0, identity.1, identity.2, job.name
            ));
        }
    }
    Ok(())
}

fn install_job(
    paths: &RegistryPaths,
    job: &AutostartJob,
    previous: Option<&AutostartJob>,
    start_now: bool,
) -> Result<(), String> {
    write_job(paths, job)?;
    let result = if job.enabled {
        match platform::executable_path() {
            Ok(executable) => platform::enable(paths, job, &executable, start_now),
            Err(error) => Err(error),
        }
    } else {
        platform::disable(paths, job)
    };
    if let Err(error) = result {
        if let Some(previous) = previous {
            let _ = write_job(paths, previous);
            if previous.enabled {
                if let Ok(executable) = platform::executable_path() {
                    let _ = platform::enable(paths, previous, &executable, false);
                }
            } else {
                let _ = platform::disable(paths, previous);
            }
        } else {
            let _ = platform::remove(paths, job);
            let _ = remove_job(paths, &job.id);
        }
        return Err(error);
    }
    Ok(())
}

fn job_summary(job: &AutostartJob, paths: &RegistryPaths) -> Value {
    let status: PlatformStatus = platform::status(paths, job);
    let identity = identity_from_job(job).ok();
    json!({
        "id": job.id,
        "name": job.name,
        "root_path": job.root_path,
        "config_path": job.config_path,
        "storage": identity.as_ref().map(|value| value.1.clone()),
        "key": identity.as_ref().map(|value| value.2.clone()),
        "enabled": job.enabled,
        "platform": status.platform,
        "platform_enabled": status.enabled,
        "running": status.running,
        "created_at": job.created_at,
        "updated_at": job.updated_at,
    })
}

fn print_interactive_summary(summary: &Value) {
    let name = summary.get("name").and_then(Value::as_str).unwrap_or("?");
    let id = summary.get("id").and_then(Value::as_str).unwrap_or("?");
    let root = summary
        .get("root_path")
        .and_then(Value::as_str)
        .unwrap_or("?");
    let enabled = summary
        .get("enabled")
        .and_then(Value::as_bool)
        .unwrap_or(false);
    let running = summary
        .get("running")
        .and_then(Value::as_bool)
        .map_or("unknown", |value| if value { "running" } else { "stopped" });
    let state = if enabled { "enabled" } else { "disabled" };
    println!(
        "{} {} ({}) — {} — {} — {}",
        style(name).bold(),
        style(id).dim(),
        state,
        running,
        summary
            .get("platform")
            .and_then(Value::as_str)
            .unwrap_or("unsupported"),
        root
    );
}

fn emit_changed(event: &str, job: &AutostartJob, start_now: bool) {
    if is_json_mode() {
        emit_named_event(
            "autostart",
            &json!({
                "event": event,
                "id": job.id,
                "name": job.name,
                "root_path": job.root_path,
                "enabled": job.enabled,
                "start_now": start_now,
                "platform": platform::platform_name(),
            }),
        );
    } else {
        println!(
            "{} Autostart job '{}' is {}.",
            style("OK").green(),
            job.name,
            if job.enabled { "enabled" } else { "disabled" }
        );
    }
}
