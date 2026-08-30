use super::autostart_platform;
use super::autostart_secrets;
use super::render::CliOutput;
use clap::ArgMatches;
use dialoguer::{Input, Password, Select};
use gib::api::{
    AddAutostartRequest, AddStorageRequest, BackupRequest, ConflictPolicy, DeleteBackupRequest,
    EncryptRepositoryRequest, ExploreDirectoryRequest, ExploreHistoryRequest,
    ExploreRestoreRequest, ExploreScope, ExploreSearchRequest, ExploreSelection, ExploreSort, Gib,
    GibError, ListBackupsRequest, ListPendingBackupsRequest, LocalStorageConfig, PruneRequest,
    RepositoryRequest, RestoreRequest, S3StorageConfig, SearchRequest, SetIdentityRequest,
    SetupRequest, StorageConfig, UpdateAutostartRequest, WebDavStorageConfig,
};
use parse_size::parse_size;
use serde::Serialize;
use std::path::PathBuf;
use std::time::Instant;
use tokio::signal;

pub(crate) fn client(matches: &ArgMatches, output: &CliOutput) -> Result<Gib, GibError> {
    let working_dir = std::env::current_dir().map_err(|error| {
        GibError::new(
            gib::api::ErrorCode::Io,
            format!("Failed to determine the working directory: {error}"),
        )
    })?;
    let mut builder = Gib::builder().working_dir(working_dir);
    if let Some(path) = matches.get_one::<String>("data-dir") {
        builder = builder.data_dir(PathBuf::from(path));
    }
    if let Some(path) = matches.get_one::<String>("config") {
        builder = builder.config_path(PathBuf::from(path));
    }
    builder = builder.discover_config(!matches.get_flag("no-config"));
    let output = output.clone();
    builder.on_event(move |event| output.event(event)).build()
}

pub(crate) async fn dispatch(
    client: &Gib,
    matches: &ArgMatches,
    output: &CliOutput,
) -> Result<(), GibError> {
    let Some((name, command)) = matches.subcommand() else {
        return Ok(());
    };
    match name {
        "config" => configure_identity(client, command, output),
        "whoami" => {
            output.result(&client.get_identity()?);
            Ok(())
        }
        "setup" => {
            let result = client.setup(SetupRequest {
                root: client.context().working_dir.clone(),
                recursive: !command.get_flag("no-recursive"),
            })?;
            output.result(&result);
            Ok(())
        }
        "storage" => dispatch_storage(client, command, output).await,
        "backup" => dispatch_backup(client, command, output).await,
        "restore" => restore(client, command, output).await,
        "encrypt" => encrypt(client, command, output).await,
        "log" => list_backups(client, command, output).await,
        "search" => search(client, command, output).await,
        "explore" => explore(client, command, output).await,
        "live" => live(client, command, output).await,
        "autostart" => dispatch_autostart(client, command, output).await,
        _ => Err(GibError::new(
            gib::api::ErrorCode::InvalidRequest,
            format!("Unknown command '{name}'"),
        )),
    }
}

fn configure_identity(
    client: &Gib,
    matches: &ArgMatches,
    output: &CliOutput,
) -> Result<(), GibError> {
    let author = text_value(
        matches,
        "author",
        "Enter your author (e.g. 'John Doe <john.doe@example.com>')",
        output,
    )?;
    let result = client.set_identity(SetIdentityRequest { author })?;
    if output.is_json() {
        #[derive(Serialize)]
        struct ConfigOutput {
            author: String,
            path: String,
        }

        output.result(&ConfigOutput {
            author: result.identity.author,
            path: result.path.to_string_lossy().into_owned(),
        });
    } else {
        output.message("Config written");
    }
    Ok(())
}

async fn dispatch_storage(
    client: &Gib,
    matches: &ArgMatches,
    output: &CliOutput,
) -> Result<(), GibError> {
    let Some((name, command)) = matches.subcommand() else {
        return Err(GibError::new(
            gib::api::ErrorCode::InvalidRequest,
            "A storage subcommand is required",
        ));
    };
    match name {
        "add" => {
            let storage_name =
                text_value(command, "name", "Enter the name of the storage", output)?;
            let storage_type = select_value(
                command,
                "type",
                &["local", "s3", "webdav"],
                "Enter the type of the storage",
                output,
            )?;
            let config = match storage_type.as_str() {
                "local" => {
                    let path =
                        text_value(command, "path", "Enter the path for local storage", output)?;
                    StorageConfig::Local(LocalStorageConfig {
                        path: PathBuf::from(path),
                    })
                }
                "s3" => StorageConfig::S3(S3StorageConfig {
                    region: text_value(command, "region", "Enter the S3 region", output)?,
                    bucket: text_value(command, "bucket", "Enter the S3 bucket", output)?,
                    access_key: text_value(
                        command,
                        "access-key",
                        "Enter the S3 access key",
                        output,
                    )?,
                    secret_key: secret_value(
                        command,
                        "secret-key",
                        "Enter the S3 secret key",
                        output,
                    )?,
                    endpoint: command.get_one::<String>("endpoint").cloned(),
                }),
                "webdav" => StorageConfig::WebDav(WebDavStorageConfig {
                    url: text_value(command, "url", "Enter the WebDAV URL", output)?,
                    username: text_value(command, "username", "Enter the WebDAV username", output)?,
                    password: secret_value(
                        command,
                        "password",
                        "Enter the WebDAV password",
                        output,
                    )?,
                }),
                value => {
                    return Err(GibError::new(
                        gib::api::ErrorCode::InvalidRequest,
                        format!("Unknown storage type '{value}'"),
                    ));
                }
            };
            let result = client
                .add_storage(AddStorageRequest {
                    name: storage_name,
                    config,
                    validate_remote: true,
                })
                .await?;
            let info = client
                .list_storages()?
                .into_iter()
                .find(|storage| storage.name == result.name);
            output.storage_added(&result, info.as_ref());
            Ok(())
        }
        "list" => {
            output.result(&client.list_storages()?);
            Ok(())
        }
        "remove" => {
            let name = text_value(command, "name", "Enter the name of the storage", output)?;
            let result = client.remove_storage(&name)?;
            output.result(&serde_json::json!({ "name": name, "removed": result }));
            Ok(())
        }
        "prune" => prune(client, command, output).await,
        _ => Err(GibError::new(
            gib::api::ErrorCode::InvalidRequest,
            format!("Unknown storage command '{name}'"),
        )),
    }
}

async fn dispatch_backup(
    client: &Gib,
    matches: &ArgMatches,
    output: &CliOutput,
) -> Result<(), GibError> {
    if let Some((name, command)) = matches.subcommand() {
        return match name {
            "pending" => {
                let repository = repository(client, command, output)?;
                let result = client
                    .list_pending_backups(ListPendingBackupsRequest { repository })
                    .await?;
                output.pending_result(&result);
                Ok(())
            }
            "delete" => {
                let repository = repository(client, command, output)?;
                let backup =
                    delete_backup_reference(client, repository.clone(), command, output).await?;
                let started = Instant::now();
                let result = client
                    .delete_backup(DeleteBackupRequest { repository, backup })
                    .await?;
                output.delete_result(&result, started.elapsed().as_millis() as u64);
                Ok(())
            }
            _ => Err(GibError::new(
                gib::api::ErrorCode::InvalidRequest,
                format!("Unknown backup command '{name}'"),
            )),
        };
    }

    let defaults = client.config_defaults()?;
    let root_path = path_value(
        matches,
        "root-path",
        defaults
            .backup_root
            .clone()
            .unwrap_or_else(|| client.context().working_dir.clone()),
        output,
    )?;
    let repository = repository_for_root(client, matches, output, &root_path)?;
    let resume = matches.get_one::<String>("continue").cloned();
    let pending_message =
        if matches.get_one::<String>("message").is_none() && defaults.backup_message.is_none() {
            if let Some(prefix) = resume.as_deref() {
                let pending = client
                    .list_pending_backups(ListPendingBackupsRequest {
                        repository: repository.clone(),
                    })
                    .await?;
                pending
                    .pending
                    .into_iter()
                    .find(|entry| entry.backup == prefix || entry.backup.starts_with(prefix))
                    .map(|entry| entry.message)
            } else {
                None
            }
        } else {
            None
        };
    let message = match (
        matches.get_one::<String>("message").cloned(),
        defaults.backup_message.clone(),
    ) {
        (Some(message), _) => message,
        (None, Some(message)) => message,
        (None, None) if pending_message.is_some() => pending_message.unwrap_or_default(),
        (None, None) if output.is_json() => {
            return Err(GibError::new(
                gib::api::ErrorCode::InvalidRequest,
                "Missing required argument: --message (or backup.message in gib.toml)",
            ));
        }
        (None, None) => text_value(matches, "message", "Backup message", output)?,
    };
    let author = client
        .get_identity()
        .map(|identity| identity.author)
        .unwrap_or_else(|_| "anonymous <anonymous@trygib.org>".to_string());
    let mut request = BackupRequest::new(repository, root_path, message, author);
    if let Some(compression) = defaults.compression {
        request.compression = compression;
    }
    if let Some(chunk_size) = defaults.chunk_size {
        request.chunk_size = chunk_size;
    }
    if let Some(concurrency) = defaults.concurrency {
        request.concurrency = concurrency;
    }
    request.ignore_patterns = defaults.ignore_patterns.clone();
    request.include_git = defaults.include_git;
    if let Some(value) = matches.get_one::<String>("compress") {
        request.compression = parse_i32(value, "compression")?;
    }
    if let Some(value) = matches.get_one::<String>("chunk-size") {
        request.chunk_size = parse_size_value(value, "chunk size")?;
    }
    if let Some(values) = matches.get_many::<String>("ignore") {
        request.ignore_patterns =
            defaults.merged_ignore_patterns(&values.cloned().collect::<Vec<_>>());
    }
    if matches.get_flag("no-ignore-git") {
        request.include_git = true;
    }
    if let Some(value) = matches.get_one::<String>("concurrency") {
        request.concurrency = parse_usize(value, "concurrency")?;
    }
    request.resume = resume;
    request.parent = match (
        matches.value_source("parent"),
        matches.get_one::<String>("parent").cloned(),
    ) {
        (Some(_), Some(parent)) => Some(parent),
        (Some(_), None) if output.is_json() => {
            return Err(GibError::new(
                gib::api::ErrorCode::InvalidRequest,
                "--parent requires a backup hash in JSON mode",
            ));
        }
        (Some(_), None) => Some("latest".to_string()),
        (None, _) => None,
    };
    output.backup_result(&client.backup(request).await?);
    Ok(())
}

async fn list_backups(
    client: &Gib,
    matches: &ArgMatches,
    output: &CliOutput,
) -> Result<(), GibError> {
    let repository = repository(client, matches, output)?;
    output.log_result(
        &client
            .list_backups(ListBackupsRequest { repository })
            .await?,
    );
    Ok(())
}

async fn search(client: &Gib, matches: &ArgMatches, output: &CliOutput) -> Result<(), GibError> {
    let repository = repository(client, matches, output)?;
    let query = text_value(matches, "query", "Search query", output)?;
    let mut request = SearchRequest::new(repository, query)?;
    if let Some(value) = matches.get_one::<String>("path") {
        request = request.with_path_prefix(value.clone())?;
    }
    if let Some(value) = matches.get_one::<String>("extension") {
        request = request.with_extension(value.clone())?;
    }
    if let Some(value) = matches.get_one::<usize>("limit") {
        request = request.with_limit(*value)?;
    }
    output.result(&client.search(request).await?);
    Ok(())
}

async fn explore(client: &Gib, matches: &ArgMatches, output: &CliOutput) -> Result<(), GibError> {
    let repository = repository(client, matches, output)?;
    let path = matches
        .get_one::<String>("path")
        .cloned()
        .unwrap_or_default();
    if matches.get_flag("restore") {
        let selected_paths = matches
            .get_many::<String>("select")
            .map(|values| values.cloned().collect::<Vec<_>>())
            .unwrap_or_default();
        let mut selections = selected_paths
            .into_iter()
            .map(|path| ExploreSelection { path, backup: None })
            .collect::<Vec<_>>();
        if selections.is_empty() && !path.is_empty() {
            selections.push(ExploreSelection {
                path: path.clone(),
                backup: None,
            });
        }
        for revision in matches.get_many::<String>("revision").into_iter().flatten() {
            let (revision_path, backup) =
                if let Some((revision_path, backup)) = revision.split_once('=') {
                    (revision_path.to_string(), backup.to_string())
                } else if !path.is_empty() {
                    (path.clone(), revision.clone())
                } else {
                    return Err(GibError::new(
                        gib::api::ErrorCode::InvalidRequest,
                        "A revision without PATH=BACKUP requires --path",
                    ));
                };
            if revision_path.trim().is_empty() || backup.trim().is_empty() {
                return Err(GibError::new(
                    gib::api::ErrorCode::InvalidRequest,
                    format!("Invalid revision '{revision}'"),
                ));
            }
            if let Some(selection) = selections
                .iter_mut()
                .find(|selection| selection.path == revision_path)
            {
                selection.backup = Some(backup);
            } else {
                selections.push(ExploreSelection {
                    path: revision_path,
                    backup: Some(backup),
                });
            }
        }
        if selections.is_empty() {
            return Err(GibError::new(
                gib::api::ErrorCode::InvalidRequest,
                "At least one path or revision is required for Explore restore",
            ));
        }
        let defaults = client.config_defaults()?;
        let target_path = path_value(
            matches,
            "target-path",
            defaults
                .restore_target
                .unwrap_or_else(|| client.context().working_dir.clone()),
            output,
        )?;
        output.result(
            &client
                .restore_explore_selection(ExploreRestoreRequest {
                    repository,
                    target_path,
                    selections,
                    prune_local: false,
                })
                .await?,
        );
        return Ok(());
    }
    let scope = match matches.get_one::<String>("scope").map(String::as_str) {
        Some("current") => ExploreScope::Current,
        _ => ExploreScope::AllHistory,
    };
    if matches.get_flag("history") {
        output.result(
            &client
                .explore_history(ExploreHistoryRequest { repository, path })
                .await?,
        );
    } else if let Some(query) = matches.get_one::<String>("query") {
        output.result(
            &client
                .explore_search(ExploreSearchRequest {
                    repository,
                    query: query.clone(),
                    scope,
                    limit: *matches.get_one::<usize>("limit").unwrap_or(&100),
                })
                .await?,
        );
    } else {
        let sort = match matches.get_one::<String>("sort").map(String::as_str) {
            Some("size") => ExploreSort::Size,
            Some("status") => ExploreSort::Status,
            Some("recent") => ExploreSort::Recent,
            _ => ExploreSort::Name,
        };
        output.result(
            &client
                .explore_directory(ExploreDirectoryRequest {
                    repository,
                    path,
                    scope,
                    cursor: matches.get_one::<String>("cursor").cloned(),
                    limit: *matches.get_one::<usize>("limit").unwrap_or(&100),
                    sort,
                })
                .await?,
        );
    }
    Ok(())
}

async fn restore(client: &Gib, matches: &ArgMatches, output: &CliOutput) -> Result<(), GibError> {
    let repository = repository(client, matches, output)?;
    let backup = restore_backup_reference(client, repository.clone(), matches, output).await?;
    let defaults = client.config_defaults()?;
    let target_path = path_value(
        matches,
        "target-path",
        defaults
            .restore_target
            .unwrap_or_else(|| client.context().working_dir.clone()),
        output,
    )?;
    let mut only: Vec<String> = matches
        .get_many::<String>("only")
        .map(|values| values.cloned().filter(|value| !value.is_empty()).collect())
        .unwrap_or_default();
    if matches.contains_id("only") && only.is_empty() && output.is_json() {
        return Err(GibError::new(
            gib::api::ErrorCode::InvalidRequest,
            "--only requires a path in JSON mode",
        ));
    }
    if matches.contains_id("only") && only.is_empty() && !output.is_json() {
        let selected = Input::<String>::new()
            .with_prompt("Path to restore (leave empty for the complete backup)")
            .allow_empty(true)
            .interact_text()
            .map_err(|error| {
                GibError::new(gib::api::ErrorCode::InvalidRequest, error.to_string())
            })?;
        if !selected.trim().is_empty() {
            only.push(selected);
        }
    }
    let started = Instant::now();
    let result = client
        .restore(RestoreRequest {
            repository,
            backup,
            target_path,
            only,
            prune_local: matches.get_flag("prune-local"),
        })
        .await?;
    output.restore_result(&result, started.elapsed().as_millis() as u64);
    Ok(())
}

async fn restore_backup_reference(
    client: &Gib,
    repository: RepositoryRequest,
    matches: &ArgMatches,
    output: &CliOutput,
) -> Result<String, GibError> {
    if let Some(backup) = matches.get_one::<String>("backup") {
        return Ok(backup.clone());
    }
    if output.is_json() {
        return Err(GibError::new(
            gib::api::ErrorCode::InvalidRequest,
            "Missing required argument: --backup (required in --mode json)",
        ));
    }
    let response = client
        .list_backups(ListBackupsRequest { repository })
        .await?;
    if response.backups.is_empty() {
        return Err(GibError::new(
            gib::api::ErrorCode::BackupNotFound,
            "No backups found in repository",
        ));
    }
    let choices = response
        .backups
        .iter()
        .map(|backup| {
            format!(
                "{} {}",
                &backup.hash[..8.min(backup.hash.len())],
                backup.message
            )
        })
        .collect::<Vec<_>>();
    let selected = Select::new()
        .with_prompt("Select a backup to restore")
        .items(&choices)
        .default(0)
        .interact()
        .map_err(|error| GibError::new(gib::api::ErrorCode::InvalidRequest, error.to_string()))?;
    Ok(response.backups[selected].hash.clone())
}

async fn delete_backup_reference(
    client: &Gib,
    repository: RepositoryRequest,
    matches: &ArgMatches,
    output: &CliOutput,
) -> Result<String, GibError> {
    if let Some(backup) = matches.get_one::<String>("backup") {
        return Ok(backup.clone());
    }
    if output.is_json() {
        return Err(GibError::new(
            gib::api::ErrorCode::InvalidRequest,
            "Missing required argument: --backup (required in --mode json)",
        ));
    }
    let response = client
        .list_backups(ListBackupsRequest { repository })
        .await?;
    if response.backups.is_empty() {
        return Err(GibError::new(
            gib::api::ErrorCode::BackupNotFound,
            "No backups found in repository",
        ));
    }
    let choices = response
        .backups
        .iter()
        .take(10)
        .map(|backup| {
            format!(
                "{} {}",
                &backup.hash[..8.min(backup.hash.len())],
                backup.message
            )
        })
        .collect::<Vec<_>>();
    let selected = Select::new()
        .with_prompt("Select a backup to delete")
        .items(&choices)
        .default(0)
        .interact()
        .map_err(|error| GibError::new(gib::api::ErrorCode::InvalidRequest, error.to_string()))?;
    Ok(response.backups[selected].hash.clone())
}

async fn encrypt(client: &Gib, matches: &ArgMatches, output: &CliOutput) -> Result<(), GibError> {
    let repository = repository(client, matches, output)?;
    output.encrypt_result(
        &client
            .encrypt_repository(EncryptRepositoryRequest { repository })
            .await?,
    );
    Ok(())
}

async fn live(client: &Gib, matches: &ArgMatches, output: &CliOutput) -> Result<(), GibError> {
    if matches.get_raw("continue").is_some() || matches.get_raw("parent").is_some() {
        return Err(GibError::new(
            gib::api::ErrorCode::InvalidRequest,
            "--parent and --continue cannot be used with gib live; live manages its synchronized base automatically",
        ));
    }
    let defaults = client.config_defaults()?;
    let root_path = path_value(
        matches,
        "root-path",
        defaults
            .backup_root
            .clone()
            .unwrap_or_else(|| client.context().working_dir.clone()),
        output,
    )?;
    let repository = repository_for_root(client, matches, output, &root_path)?;
    let conflict = match matches.get_one::<String>("conflict").map(String::as_str) {
        Some("local") => ConflictPolicy::Local,
        Some("remote") => ConflictPolicy::Remote,
        _ if output.is_json() => {
            return Err(GibError::new(
                gib::api::ErrorCode::InvalidRequest,
                "--conflict is required in JSON mode",
            ));
        }
        _ => ConflictPolicy::Local,
    };
    let mut request = gib::api::LiveRequest::new(repository, root_path);
    request.message = matches
        .get_one::<String>("message")
        .cloned()
        .or(defaults.live_message.clone())
        .or(defaults.backup_message.clone());
    if let Some(compression) = defaults.compression {
        request.compression = compression;
    }
    if let Some(chunk_size) = defaults.chunk_size {
        request.chunk_size = chunk_size;
    }
    if let Some(concurrency) = defaults.concurrency {
        request.concurrency = concurrency;
    }
    request.ignore_patterns = defaults.ignore_patterns.clone();
    request.conflict = conflict;
    apply_tuning(matches, &mut request)?;
    if let Some(values) = matches.get_many::<String>("ignore") {
        request.ignore_patterns =
            defaults.merged_ignore_patterns(&values.cloned().collect::<Vec<_>>());
    }
    if let Some(milliseconds) = defaults.live_debounce_ms {
        request.debounce = std::time::Duration::from_millis(milliseconds);
    }
    if let Some(milliseconds) = defaults.live_poll_ms {
        request.poll_interval = std::time::Duration::from_millis(milliseconds);
    }
    let handle = client.start_live(request).await?;
    wait_for_handle(handle, output).await
}

async fn wait_for_handle(handle: gib::api::LiveHandle, output: &CliOutput) -> Result<(), GibError> {
    tokio::select! {
        result = handle.wait() => output.result(&result?),
        signal_result = signal::ctrl_c() => {
            signal_result.map_err(|error| GibError::new(gib::api::ErrorCode::Io, error.to_string()))?;
            handle.stop().await?;
            output.result(&handle.wait().await?);
        }
    }
    Ok(())
}

async fn prune(client: &Gib, matches: &ArgMatches, output: &CliOutput) -> Result<(), GibError> {
    let repository = repository(client, matches, output)?;
    let started = Instant::now();
    let plan = client.plan_prune(PruneRequest { repository }).await?;
    if plan.items.is_empty() {
        output.result(&serde_json::json!({ "deleted_items": 0, "failures": [] }));
        return Ok(());
    }
    if output.is_json() && !matches.get_flag("yes") {
        return Err(GibError::new(
            gib::api::ErrorCode::InvalidRequest,
            "Confirmation required in --mode json; re-run with --yes",
        ));
    }
    if !matches.get_flag("yes") {
        let confirmed = dialoguer::Confirm::new()
            .with_prompt(format!(
                "Delete {} unused repository items?",
                plan.items.len()
            ))
            .interact()
            .map_err(|error| {
                GibError::new(gib::api::ErrorCode::InvalidRequest, error.to_string())
            })?;
        if !confirmed {
            output.result(&serde_json::json!({ "deleted_items": 0, "aborted": true }));
            return Ok(());
        }
    }
    let result = client.execute_prune(plan).await?;
    output.prune_result(&result, started.elapsed().as_millis() as u64);
    Ok(())
}

async fn dispatch_autostart(
    client: &Gib,
    matches: &ArgMatches,
    output: &CliOutput,
) -> Result<(), GibError> {
    let Some((name, command)) = matches.subcommand() else {
        return Err(GibError::new(
            gib::api::ErrorCode::InvalidRequest,
            "An autostart subcommand is required",
        ));
    };
    match name {
        "add" => {
            let job_name = text_value(command, "name", "Job name", output)?;
            let root_path = path_value(
                command,
                "root-path",
                client.context().working_dir.clone(),
                output,
            )?;
            let repository = Some(repository_for_root(client, command, output, &root_path)?);
            let config_path = client.config_defaults()?.config_path;
            let request = AddAutostartRequest {
                name: job_name,
                root_path,
                config_path,
                repository,
                message: command.get_one::<String>("message").cloned(),
                compression: parse_optional_i32(command, "compress")?,
                chunk_size: parse_optional_size(command, "chunk-size")?,
                ignore_patterns: command
                    .get_many::<String>("ignore")
                    .map(|values| values.cloned().collect()),
                include_git: command.get_flag("no-ignore-git"),
                concurrency: parse_optional_usize(command, "concurrency")?,
                conflict: parse_conflict(command, output)?,
                password: command.get_one::<String>("password").cloned(),
                start_now: !command.get_flag("no-start"),
            };
            if command.get_flag("replace") {
                if let Some(previous) = client
                    .list_autostart_jobs()?
                    .into_iter()
                    .find(|job| job.name == request.name)
                {
                    autostart_platform::remove(client, &previous)?;
                    client.remove_autostart(&previous.name)?;
                }
            }
            let result = client.add_autostart(request)?;
            if let Err(error) =
                autostart_platform::enable(client, &result.job, !command.get_flag("no-start"))
            {
                let _ = client.remove_autostart(&result.job.name);
                return Err(error);
            }
            let platform = autostart_platform::status(client, &result.job).platform;
            output.autostart_changed(
                "registered",
                &result.job,
                !command.get_flag("no-start"),
                &platform,
            );
            Ok(())
        }
        "update" => {
            let name = text_value(command, "name", "Job name", output)?;
            let previous = client
                .list_autostart_jobs()?
                .into_iter()
                .find(|job| job.name == name || job.id == name);
            let has_repository = command.get_one::<String>("key").is_some()
                || command.get_one::<String>("storage").is_some()
                || command.get_one::<String>("password").is_some();
            let repository = has_repository
                .then(|| repository(client, command, output))
                .transpose()?;
            let result = client.update_autostart(UpdateAutostartRequest {
                name: name.clone(),
                root_path: command.get_one::<String>("root-path").map(PathBuf::from),
                // `None` means “preserve the job's existing config path”; an
                // explicitly supplied global --config is already stored in
                // the client context and replaces it.
                config_path: client.context().config_path.clone(),
                repository,
                message: command.get_one::<String>("message").cloned(),
                compression: parse_optional_i32(command, "compress")?,
                chunk_size: parse_optional_size(command, "chunk-size")?,
                ignore_patterns: command
                    .get_many::<String>("ignore")
                    .map(|values| values.cloned().collect()),
                include_git: command.get_flag("no-ignore-git").then_some(true),
                concurrency: parse_optional_usize(command, "concurrency")?,
                conflict: command
                    .get_one::<String>("conflict")
                    .map(|_| parse_conflict(command, output))
                    .transpose()?,
                password: command.get_one::<String>("password").cloned(),
                start_now: if command.get_flag("no-start") {
                    Some(false)
                } else if command.get_flag("start-now") {
                    Some(true)
                } else {
                    None
                },
            })?;
            let start_now = !command.get_flag("no-start")
                && (command.get_flag("start-now")
                    || previous.as_ref().is_some_and(|job| job.enabled));
            if result.job.enabled {
                autostart_platform::enable(client, &result.job, start_now)?;
            } else {
                autostart_platform::disable(client, &result.job)?;
            }
            let platform = autostart_platform::status(client, &result.job).platform;
            output.autostart_changed("updated", &result.job, start_now, &platform);
            Ok(())
        }
        "list" => {
            output.autostart_list(&autostart_summaries(client, None)?, false);
            Ok(())
        }
        "status" => {
            output.autostart_list(
                &autostart_summaries(
                    client,
                    command.get_one::<String>("name").map(String::as_str),
                )?,
                true,
            );
            Ok(())
        }
        "enable" => {
            let name = text_value(command, "name", "Job name", output)?;
            let result = client.enable_autostart(&name)?;
            autostart_platform::enable(client, &result.job, true)?;
            let platform = autostart_platform::status(client, &result.job).platform;
            output.autostart_changed("enabled", &result.job, true, &platform);
            Ok(())
        }
        "disable" => {
            let name = text_value(command, "name", "Job name", output)?;
            let result = client.disable_autostart(&name)?;
            autostart_platform::disable(client, &result.job)?;
            let platform = autostart_platform::status(client, &result.job).platform;
            output.autostart_changed("disabled", &result.job, false, &platform);
            Ok(())
        }
        "remove" => {
            let name = text_value(command, "name", "Job name", output)?;
            if !command.get_flag("yes") && !output.is_json() {
                let confirmed = dialoguer::Confirm::new()
                    .with_prompt(format!("Remove autostart job '{name}'?"))
                    .interact()
                    .map_err(|error| {
                        GibError::new(gib::api::ErrorCode::InvalidRequest, error.to_string())
                    })?;
                if !confirmed {
                    output.result(&serde_json::json!({ "removed": false, "aborted": true }));
                    return Ok(());
                }
            }
            let previous = client
                .list_autostart_jobs()?
                .into_iter()
                .find(|job| job.name == name || job.id == name)
                .ok_or_else(|| {
                    GibError::new(
                        gib::api::ErrorCode::StorageNotFound,
                        format!("Autostart job '{name}' was not found"),
                    )
                })?;
            autostart_platform::remove(client, &previous)?;
            let reference = previous.password_reference().map(str::to_owned);
            let result = client.remove_autostart(&name)?;
            if let Some(reference) = reference {
                let _ = autostart_secrets::remove(&reference);
            }
            output.autostart_removed(&result.job);
            Ok(())
        }
        "logs" => {
            let name = text_value(command, "name", "Job name", output)?;
            follow_autostart_logs(client, &name, output).await
        }
        "run" => {
            let name = text_value(command, "job-id", "Job id", output)?;
            output.set_json_log(client.autostart_log_path(&name)?)?;
            let password = client
                .autostart_password_reference(&name)?
                .and_then(|reference| autostart_secrets::read(&reference).ok().flatten());
            let run = client.run_autostart_with_password(&name, password).await?;
            wait_for_handle(run.handle, output).await
        }
        _ => Err(GibError::new(
            gib::api::ErrorCode::InvalidRequest,
            format!("Unknown autostart command '{name}'"),
        )),
    }
}

async fn follow_autostart_logs(
    client: &Gib,
    name_or_id: &str,
    output: &CliOutput,
) -> Result<(), GibError> {
    let job = client
        .list_autostart_jobs()?
        .into_iter()
        .find(|job| job.name == name_or_id || job.id == name_or_id)
        .ok_or_else(|| {
            GibError::new(
                gib::api::ErrorCode::StorageNotFound,
                format!("Autostart job '{name_or_id}' was not found"),
            )
        })?;
    let log_path = client.autostart_log_path(&job.id)?;
    output.autostart_log_following(&job, &log_path);

    let mut offset = 0_usize;
    loop {
        let lines = client.follow_autostart_events(&job.id)?;
        if offset > lines.len() {
            offset = 0;
        }
        for line in lines.iter().skip(offset) {
            output.autostart_log_entry(&job, &log_path, line);
        }
        offset = lines.len();

        tokio::select! {
            signal_result = signal::ctrl_c() => {
                signal_result.map_err(|error| GibError::new(gib::api::ErrorCode::Io, error.to_string()))?;
                output.autostart_log_stopped(&job, &log_path);
                return Ok(());
            }
            _ = tokio::time::sleep(std::time::Duration::from_millis(200)) => {}
        }
    }
}

fn autostart_summaries(
    client: &Gib,
    name: Option<&str>,
) -> Result<Vec<serde_json::Value>, GibError> {
    let defaults = client.config_defaults().ok();
    client
        .list_autostart_jobs()?
        .into_iter()
        .filter(|job| name.is_none_or(|name| job.name == name || job.id == name))
        .map(|job| {
            let status = autostart_platform::status(client, &job);
            let storage = job.overrides.storage.clone().or_else(|| {
                defaults
                    .as_ref()
                    .and_then(|value| value.repository_storage.clone())
            });
            let key = job.overrides.key.clone().or_else(|| {
                defaults
                    .as_ref()
                    .and_then(|value| value.repository_key.clone())
            });
            Ok(serde_json::json!({
                "id": job.id,
                "name": job.name,
                "root_path": job.root_path,
                "config_path": job.config_path,
                "storage": storage,
                "key": key,
                "enabled": job.enabled,
                "platform": status.platform,
                "platform_enabled": status.enabled,
                "running": status.running,
                "created_at": job.created_at,
                "updated_at": job.updated_at,
            }))
        })
        .collect()
}

fn repository(
    client: &Gib,
    matches: &ArgMatches,
    output: &CliOutput,
) -> Result<RepositoryRequest, GibError> {
    // Commands without a root argument use the configured working directory.
    repository_with_default_root(client, matches, output, None)
}

fn repository_for_root(
    client: &Gib,
    matches: &ArgMatches,
    output: &CliOutput,
    root: &std::path::Path,
) -> Result<RepositoryRequest, GibError> {
    repository_with_default_root(client, matches, output, Some(root))
}

fn repository_with_default_root(
    client: &Gib,
    matches: &ArgMatches,
    output: &CliOutput,
    default_root: Option<&std::path::Path>,
) -> Result<RepositoryRequest, GibError> {
    let defaults = client.config_defaults()?;
    let default_key = default_root
        .map(PathBuf::from)
        .unwrap_or_else(|| client.context().working_dir.clone())
        .file_name()
        .map(|name| name.to_string_lossy().into_owned())
        .filter(|name| !name.is_empty() && name != "." && name != "..")
        .unwrap_or_else(|| "repository".to_string());
    let key = text_value_default(
        matches,
        "key",
        defaults.repository_key.unwrap_or(default_key),
        "Repository key",
        output,
    )?;
    let storage = if let Some(storage) = matches.get_one::<String>("storage") {
        storage.clone()
    } else if let Some(storage) = defaults.repository_storage {
        storage
    } else {
        let storages = client.list_storages()?;
        if storages.len() == 1 {
            storages[0].name.clone()
        } else if output.is_json() {
            return Err(GibError::new(
                gib::api::ErrorCode::InvalidRequest,
                "Repository storage is required",
            ));
        } else if storages.is_empty() {
            text_value(matches, "storage", "Storage name", output)?
        } else {
            let names = storages
                .iter()
                .map(|storage| storage.name.clone())
                .collect::<Vec<_>>();
            let index = Select::new()
                .with_prompt("Storage name")
                .items(&names)
                .default(0)
                .interact()
                .map_err(|error| {
                    GibError::new(gib::api::ErrorCode::InvalidRequest, error.to_string())
                })?;
            names[index].clone()
        }
    };
    Ok(RepositoryRequest {
        key,
        storage,
        password: matches.get_one::<String>("password").cloned(),
    })
}

fn text_value(
    matches: &ArgMatches,
    id: &str,
    prompt: &str,
    output: &CliOutput,
) -> Result<String, GibError> {
    if let Some(value) = matches.get_one::<String>(id) {
        return Ok(value.clone());
    }
    if output.is_json() {
        return Err(GibError::new(
            gib::api::ErrorCode::InvalidRequest,
            format!("Missing required argument: --{id}"),
        ));
    }
    Input::<String>::new()
        .with_prompt(prompt)
        .interact_text()
        .map_err(|error| GibError::new(gib::api::ErrorCode::InvalidRequest, error.to_string()))
}

fn text_value_default(
    matches: &ArgMatches,
    id: &str,
    default: String,
    prompt: &str,
    output: &CliOutput,
) -> Result<String, GibError> {
    if let Some(value) = matches.get_one::<String>(id) {
        return Ok(value.clone());
    }
    if !default.is_empty() {
        return Ok(default);
    }
    text_value(matches, id, prompt, output)
}

fn secret_value(
    matches: &ArgMatches,
    id: &str,
    prompt: &str,
    output: &CliOutput,
) -> Result<String, GibError> {
    if let Some(value) = matches.get_one::<String>(id) {
        return Ok(value.clone());
    }
    if output.is_json() {
        return Err(GibError::new(
            gib::api::ErrorCode::InvalidRequest,
            format!("Missing required argument: --{id}"),
        ));
    }
    Password::new()
        .with_prompt(prompt)
        .interact()
        .map_err(|error| GibError::new(gib::api::ErrorCode::InvalidRequest, error.to_string()))
}

fn select_value(
    matches: &ArgMatches,
    id: &str,
    values: &[&str],
    prompt: &str,
    output: &CliOutput,
) -> Result<String, GibError> {
    if let Some(value) = matches.get_one::<String>(id) {
        return Ok(value.clone());
    }
    if output.is_json() {
        return Err(GibError::new(
            gib::api::ErrorCode::InvalidRequest,
            format!("Missing required argument: --{id}"),
        ));
    }
    let selected = Select::new()
        .with_prompt(prompt)
        .items(values)
        .default(0)
        .interact()
        .map_err(|error| GibError::new(gib::api::ErrorCode::InvalidRequest, error.to_string()))?;
    Ok(values[selected].to_string())
}

fn path_value(
    matches: &ArgMatches,
    id: &str,
    default: PathBuf,
    _output: &CliOutput,
) -> Result<PathBuf, GibError> {
    Ok(matches
        .get_one::<String>(id)
        .map(PathBuf::from)
        .unwrap_or(default))
}

fn apply_tuning(matches: &ArgMatches, request: &mut gib::api::LiveRequest) -> Result<(), GibError> {
    if let Some(value) = matches.get_one::<String>("compress") {
        request.compression = parse_i32(value, "compression")?;
    }
    if let Some(value) = matches.get_one::<String>("chunk-size") {
        request.chunk_size = parse_size_value(value, "chunk size")?;
    }
    if let Some(values) = matches.get_many::<String>("ignore") {
        request.ignore_patterns = values.cloned().collect();
    }
    request.include_git = matches.get_flag("no-ignore-git");
    if let Some(value) = matches.get_one::<String>("concurrency") {
        request.concurrency = parse_usize(value, "concurrency")?;
    }
    Ok(())
}

fn parse_i32(value: &str, label: &str) -> Result<i32, GibError> {
    value.parse().map_err(|_| {
        GibError::new(
            gib::api::ErrorCode::InvalidRequest,
            format!("Invalid {label} '{value}'"),
        )
    })
}

fn parse_usize(value: &str, label: &str) -> Result<usize, GibError> {
    value.parse().map_err(|_| {
        GibError::new(
            gib::api::ErrorCode::InvalidRequest,
            format!("Invalid {label} '{value}'"),
        )
    })
}

fn parse_size_value(value: &str, label: &str) -> Result<u64, GibError> {
    parse_size(value).map_err(|_| {
        GibError::new(
            gib::api::ErrorCode::InvalidRequest,
            format!("Invalid {label} '{value}'"),
        )
    })
}

fn parse_optional_i32(matches: &ArgMatches, id: &str) -> Result<Option<i32>, GibError> {
    matches
        .get_one::<String>(id)
        .map(|value| parse_i32(value, id))
        .transpose()
}

fn parse_optional_usize(matches: &ArgMatches, id: &str) -> Result<Option<usize>, GibError> {
    matches
        .get_one::<String>(id)
        .map(|value| parse_usize(value, id))
        .transpose()
}

fn parse_optional_size(matches: &ArgMatches, id: &str) -> Result<Option<u64>, GibError> {
    matches
        .get_one::<String>(id)
        .map(|value| parse_size_value(value, id))
        .transpose()
}

fn parse_conflict(matches: &ArgMatches, output: &CliOutput) -> Result<ConflictPolicy, GibError> {
    match matches.get_one::<String>("conflict").map(String::as_str) {
        Some("remote") => Ok(ConflictPolicy::Remote),
        Some("local") => Ok(ConflictPolicy::Local),
        None if !output.is_json() => Ok(ConflictPolicy::Local),
        None => Err(GibError::new(
            gib::api::ErrorCode::InvalidRequest,
            "Conflict policy is required in JSON mode",
        )),
        Some(value) => Err(GibError::new(
            gib::api::ErrorCode::InvalidRequest,
            format!("Invalid conflict policy '{value}'"),
        )),
    }
}
