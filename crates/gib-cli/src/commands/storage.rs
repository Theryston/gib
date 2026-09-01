use super::CommandError;
use crate::input::{
    OutputMode, StorageAddCommand, StorageCommand, StorageListCommand, StorageRemoveCommand,
};
use crate::{interactive, output};
use gib::{
    S3StorageCredentials, S3StorageSettings, SdkError, StorageAddRequest,
    StorageConfigurationError, StorageConfigurationListRequest, StorageManager,
    StorageRemoveRequest, WebDavStorageCredentials, WebDavStorageSettings,
};

pub fn run(request: StorageCommand, mode: OutputMode) -> Result<(), CommandError> {
    match request {
        StorageCommand::Add(request) => run_add(*request, mode),
        StorageCommand::List(request) => run_list(request, mode),
        StorageCommand::Remove(request) => run_remove(request, mode),
    }
}

fn run_add(command: StorageAddCommand, mode: OutputMode) -> Result<(), CommandError> {
    if mode == OutputMode::Interactive {
        output::render_storage_add_start(command.name.as_deref());
    }
    let request = build_add_request(&command, mode)?;
    if mode == OutputMode::Interactive {
        output::render_storage_add_review(&request);
        if !interactive::confirm("Save this storage configuration?", true)
            .map_err(CommandError::Sdk)?
        {
            return Err(cancelled());
        }
    }

    let manager = storage_manager()?;
    if mode == OutputMode::Interactive {
        output::render_storage_checking(&request);
    }
    let result = match manager.add(request.clone()) {
        Ok(result) => result,
        Err(error)
            if error.is_conflict()
                && mode == OutputMode::Interactive
                && !request.replaces_existing() =>
        {
            output::render_storage_conflict(request.name().as_str());
            if !interactive::confirm("Replace the existing configuration?", false)
                .map_err(CommandError::Sdk)?
            {
                return Err(cancelled());
            }
            output::render_storage_checking(&request);
            manager
                .add(request.replace_existing())
                .map_err(CommandError::StorageConfiguration)?
        }
        Err(error) => return Err(CommandError::StorageConfiguration(error)),
    };
    output::render_storage_add(&result, mode);
    Ok(())
}

fn run_list(request: StorageListCommand, mode: OutputMode) -> Result<(), CommandError> {
    if mode == OutputMode::Interactive {
        output::render_storage_list_start(request.check_health);
    }
    let manager = storage_manager()?;
    if mode == OutputMode::Interactive && request.check_health {
        output::render_storage_health_check_start();
    }
    let request = StorageConfigurationListRequest::new().with_health_check(request.check_health);
    let result = manager
        .list(request)
        .map_err(CommandError::StorageConfiguration)?;
    output::render_storage_list(&result, mode);
    Ok(())
}

fn run_remove(command: StorageRemoveCommand, mode: OutputMode) -> Result<(), CommandError> {
    if mode == OutputMode::Json {
        let name = command.name.ok_or_else(|| missing_field("name"))?;
        if !command.yes {
            return Err(missing_field("yes"));
        }
        let request =
            StorageRemoveRequest::new(name).map_err(CommandError::StorageConfiguration)?;
        let manager = storage_manager()?;
        let result = manager
            .remove(request)
            .map_err(CommandError::StorageConfiguration)?;
        output::render_storage_remove(&result, mode);
        return Ok(());
    }

    output::render_storage_remove_start();
    let manager = storage_manager()?;
    let name = match command.name {
        Some(name) => name,
        None => {
            let result = manager
                .list(StorageConfigurationListRequest::new())
                .map_err(CommandError::StorageConfiguration)?;
            let options = result
                .storages()
                .iter()
                .map(|storage| format!("{} — {}", storage.name(), storage.backend().kind()))
                .collect::<Vec<_>>();
            if options.is_empty() {
                return Err(CommandError::StorageConfiguration(
                    StorageConfigurationError::NotFound,
                ));
            }
            let selected = interactive::select("Choose a storage to remove", &options, 0)
                .map_err(CommandError::Sdk)?;
            result.storages()[selected].name().to_string()
        }
    };
    let request = StorageRemoveRequest::new(name).map_err(CommandError::StorageConfiguration)?;
    let metadata = manager
        .store()
        .describe(request.name().as_str())
        .map_err(CommandError::StorageConfiguration)?;
    output::render_storage_remove_review(&metadata);
    if !command.yes
        && !interactive::confirm(
            "Remove this configuration? Repository data will stay untouched.",
            false,
        )
        .map_err(CommandError::Sdk)?
    {
        return Err(cancelled());
    }
    let result = manager
        .remove(request)
        .map_err(CommandError::StorageConfiguration)?;
    output::render_storage_remove(&result, mode);
    Ok(())
}

fn storage_manager() -> Result<StorageManager, CommandError> {
    StorageManager::global().map_err(CommandError::StorageConfiguration)
}

fn build_add_request(
    command: &StorageAddCommand,
    mode: OutputMode,
) -> Result<StorageAddRequest, CommandError> {
    let name = argument_or_prompt(&command.name, "name", "Storage name", mode)?;
    let backend = argument_or_prompt_backend(&command.backend, mode)?;
    let backend = backend.to_ascii_lowercase();
    let request = match backend.as_str() {
        "local" => {
            let path = argument_or_prompt_path(&command.path, "path", mode)?;
            StorageAddRequest::local(name, path).map_err(CommandError::StorageConfiguration)?
        }
        "s3" => {
            let region = argument_or_prompt(&command.region, "region", "AWS region", mode)?;
            let bucket = argument_or_prompt(&command.bucket, "bucket", "Bucket name", mode)?;
            let access_key =
                argument_or_prompt(&command.access_key, "access_key", "Access key", mode)?;
            let secret_key =
                argument_or_prompt_secret(&command.secret_key, "secret_key", "Secret key", mode)?;
            let session_token = match &command.session_token {
                Some(token) => Some(token.clone()),
                None if mode == OutputMode::Json => None,
                None => optional_secret("Session token (optional)")?,
            };
            let credentials =
                S3StorageCredentials::with_session_token(access_key, secret_key, session_token)
                    .map_err(|_| invalid_storage_configuration())?;
            let endpoint = match &command.endpoint {
                Some(endpoint) => Some(endpoint.clone()),
                None if mode == OutputMode::Json => None,
                None => optional_text("S3-compatible endpoint (optional)")?,
            };
            let force_path_style = if command.force_path_style {
                true
            } else if mode == OutputMode::Interactive && endpoint.is_some() {
                interactive::confirm("Use path-style addressing for this endpoint?", true)
                    .map_err(CommandError::Sdk)?
            } else {
                false
            };
            let mut settings = S3StorageSettings::new(region, bucket)
                .map_err(CommandError::StorageConfiguration)?
                .with_force_path_style(force_path_style);
            if let Some(endpoint) = endpoint {
                settings = settings
                    .with_endpoint(endpoint)
                    .map_err(CommandError::StorageConfiguration)?;
            }
            StorageAddRequest::s3(name, settings, credentials)
                .map_err(CommandError::StorageConfiguration)?
        }
        "webdav" => {
            let url = argument_or_prompt(&command.url, "url", "Collection URL", mode)?;
            let username = argument_or_prompt(&command.username, "username", "Username", mode)?;
            let password =
                argument_or_prompt_secret(&command.password, "password", "Password", mode)?;
            let allow_insecure_http = if command.allow_insecure_http {
                true
            } else if mode == OutputMode::Interactive
                && url.trim_start().to_ascii_lowercase().starts_with("http://")
            {
                interactive::confirm("Allow insecure HTTP for this WebDAV endpoint?", false)
                    .map_err(CommandError::Sdk)?
            } else {
                false
            };
            let settings = WebDavStorageSettings::new(url)
                .map_err(CommandError::StorageConfiguration)?
                .with_allow_insecure_http(allow_insecure_http);
            let credentials = WebDavStorageCredentials::new(username, password)
                .map_err(|_| invalid_storage_configuration())?;
            StorageAddRequest::webdav(name, settings, credentials)
                .map_err(CommandError::StorageConfiguration)?
        }
        _ => return Err(invalid_field("backend", "must be local, s3, or webdav")),
    };
    Ok(request.with_replacement(command.replace))
}

fn argument_or_prompt(
    value: &Option<String>,
    field: &'static str,
    prompt: &str,
    mode: OutputMode,
) -> Result<String, CommandError> {
    match value {
        Some(value) => Ok(value.clone()),
        None if mode == OutputMode::Json => Err(missing_field(field)),
        None => interactive::text(prompt).map_err(CommandError::Sdk),
    }
}

fn argument_or_prompt_path(
    value: &Option<std::path::PathBuf>,
    field: &'static str,
    mode: OutputMode,
) -> Result<std::path::PathBuf, CommandError> {
    match value {
        Some(value) => Ok(value.clone()),
        None if mode == OutputMode::Json => Err(missing_field(field)),
        None => interactive::text_with_default("Local folder", Some("."), false)
            .map(Into::into)
            .map_err(CommandError::Sdk),
    }
}

fn argument_or_prompt_secret(
    value: &Option<String>,
    field: &'static str,
    prompt: &str,
    mode: OutputMode,
) -> Result<String, CommandError> {
    match value {
        Some(value) => Ok(value.clone()),
        None if mode == OutputMode::Json => Err(missing_field(field)),
        None => interactive::secret(prompt, false).map_err(CommandError::Sdk),
    }
}

fn optional_text(prompt: &str) -> Result<Option<String>, CommandError> {
    interactive::text_with_default(prompt, None, true)
        .map(|value| (!value.is_empty()).then_some(value))
        .map_err(CommandError::Sdk)
}

fn optional_secret(prompt: &str) -> Result<Option<String>, CommandError> {
    interactive::secret(prompt, true)
        .map(|value| (!value.is_empty()).then_some(value))
        .map_err(CommandError::Sdk)
}

fn argument_or_prompt_backend(
    value: &Option<String>,
    mode: OutputMode,
) -> Result<String, CommandError> {
    match value {
        Some(value) => Ok(value.clone()),
        None if mode == OutputMode::Json => Err(missing_field("backend")),
        None => {
            let options = vec![
                "local — folder on this machine".to_owned(),
                "s3 — bucket or compatible service".to_owned(),
                "webdav — remote collection".to_owned(),
            ];
            let index = interactive::select("What kind of storage is this?", &options, 0)
                .map_err(CommandError::Sdk)?;
            Ok(["local", "s3", "webdav"][index].to_owned())
        }
    }
}

fn missing_field(field: &'static str) -> CommandError {
    invalid_field(field, "is required")
}

fn invalid_field(field: &'static str, reason: &'static str) -> CommandError {
    CommandError::Sdk(SdkError::InvalidRequest { field, reason })
}

fn invalid_storage_configuration() -> CommandError {
    CommandError::StorageConfiguration(StorageConfigurationError::InvalidConfiguration)
}

fn cancelled() -> CommandError {
    CommandError::Sdk(SdkError::OperationCancelled { operation_id: None })
}
