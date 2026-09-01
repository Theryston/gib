use super::CommandError;
use crate::input::{
    OutputMode, StorageAddCommand, StorageCommand, StorageListCommand, StorageRemoveCommand,
};
use crate::output;
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

fn run_add(request: StorageAddCommand, mode: OutputMode) -> Result<(), CommandError> {
    let request = build_add_request(&request, mode)?;
    let manager = storage_manager()?;
    let result = match manager.add(request.clone()) {
        Ok(result) => result,
        Err(error)
            if error.is_conflict()
                && mode == OutputMode::Interactive
                && !request.replaces_existing() =>
        {
            if !prompt_confirmation("Storage exists. Replace it?")? {
                return Err(CommandError::StorageConfiguration(error));
            }
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
    let manager = storage_manager()?;
    let request = StorageConfigurationListRequest::new().with_health_check(request.check_health);
    let result = manager
        .list(request)
        .map_err(CommandError::StorageConfiguration)?;
    output::render_storage_list(&result, mode);
    Ok(())
}

fn run_remove(request: StorageRemoveCommand, mode: OutputMode) -> Result<(), CommandError> {
    let name = match request.name {
        Some(name) => name,
        None if mode == OutputMode::Json => return Err(missing_field("name")),
        None => prompt_text("Storage name")?,
    };
    if mode == OutputMode::Json && !request.yes {
        return Err(missing_field("yes"));
    }
    if mode == OutputMode::Interactive
        && !request.yes
        && !prompt_confirmation("Remove this storage configuration?")?
    {
        return Err(CommandError::Sdk(SdkError::OperationCancelled {
            operation_id: None,
        }));
    }

    let request = StorageRemoveRequest::new(name).map_err(CommandError::StorageConfiguration)?;
    let manager = storage_manager()?;
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
            let path = argument_or_prompt_path(&command.path, "path", "Local storage path", mode)?;
            StorageAddRequest::local(name, path).map_err(CommandError::StorageConfiguration)?
        }
        "s3" => {
            let region = argument_or_prompt(&command.region, "region", "S3 region", mode)?;
            let bucket = argument_or_prompt(&command.bucket, "bucket", "S3 bucket", mode)?;
            let access_key =
                argument_or_prompt(&command.access_key, "access_key", "S3 access key", mode)?;
            let secret_key = argument_or_prompt_secret(
                &command.secret_key,
                "secret_key",
                "S3 secret key",
                mode,
            )?;
            let credentials = S3StorageCredentials::with_session_token(
                access_key,
                secret_key,
                match &command.session_token {
                    Some(token) => Some(token.clone()),
                    None if mode == OutputMode::Json => None,
                    None => optional_secret("S3 session token")?,
                },
            )
            .map_err(|_| invalid_storage_configuration())?;
            let mut settings = S3StorageSettings::new(region, bucket)
                .map_err(CommandError::StorageConfiguration)?
                .with_force_path_style(command.force_path_style);
            if let Some(endpoint) = &command.endpoint {
                settings = settings
                    .with_endpoint(endpoint)
                    .map_err(CommandError::StorageConfiguration)?;
            } else if mode == OutputMode::Interactive
                && let Some(endpoint) = optional_text("S3 endpoint")?
            {
                settings = settings
                    .with_endpoint(endpoint)
                    .map_err(CommandError::StorageConfiguration)?;
            }
            StorageAddRequest::s3(name, settings, credentials)
                .map_err(CommandError::StorageConfiguration)?
        }
        "webdav" => {
            let url = argument_or_prompt(&command.url, "url", "WebDAV collection URL", mode)?;
            let username =
                argument_or_prompt(&command.username, "username", "WebDAV username", mode)?;
            let password =
                argument_or_prompt_secret(&command.password, "password", "WebDAV password", mode)?;
            let settings = WebDavStorageSettings::new(url)
                .map_err(CommandError::StorageConfiguration)?
                .with_allow_insecure_http(command.allow_insecure_http);
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
        None => prompt_text(prompt),
    }
}

fn argument_or_prompt_path(
    value: &Option<std::path::PathBuf>,
    field: &'static str,
    prompt: &str,
    mode: OutputMode,
) -> Result<std::path::PathBuf, CommandError> {
    match value {
        Some(value) => Ok(value.clone()),
        None if mode == OutputMode::Json => Err(missing_field(field)),
        None => prompt_text(prompt).map(Into::into),
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
        None => prompt_secret(prompt),
    }
}

fn optional_text(prompt: &str) -> Result<Option<String>, CommandError> {
    let value = prompt_line(prompt)?;
    Ok((!value.is_empty()).then_some(value))
}

fn optional_secret(prompt: &str) -> Result<Option<String>, CommandError> {
    let value = prompt_secret(prompt)?;
    Ok((!value.is_empty()).then_some(value))
}

fn prompt_text(prompt: &str) -> Result<String, CommandError> {
    prompt_line(prompt)
}

fn prompt_secret(prompt: &str) -> Result<String, CommandError> {
    print_prompt(prompt)?;
    set_terminal_echo(false)?;
    let result = read_line();
    let restore_result = set_terminal_echo(true);
    println!();
    restore_result?;
    result
}

fn prompt_confirmation(prompt: &str) -> Result<bool, CommandError> {
    let answer = prompt_line(&format!("{prompt} [y/N]"))?;
    Ok(matches!(
        answer.trim().to_ascii_lowercase().as_str(),
        "y" | "yes"
    ))
}

fn prompt_backend() -> Result<String, CommandError> {
    prompt_text("Backend (local, s3, or webdav)")
}

fn argument_or_prompt_backend(
    value: &Option<String>,
    mode: OutputMode,
) -> Result<String, CommandError> {
    match value {
        Some(value) => Ok(value.clone()),
        None if mode == OutputMode::Json => Err(missing_field("backend")),
        None => prompt_backend(),
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

fn prompt_error() -> CommandError {
    CommandError::Sdk(SdkError::ConfigurationFailure {
        operation: "prompt",
    })
}

fn prompt_line(prompt: &str) -> Result<String, CommandError> {
    print_prompt(prompt)?;
    read_line()
}

fn print_prompt(prompt: &str) -> Result<(), CommandError> {
    use std::io::Write;

    print!("{prompt}: ");
    std::io::stdout().flush().map_err(|_| prompt_error())
}

fn read_line() -> Result<String, CommandError> {
    use std::io::BufRead;

    let mut value = String::new();
    std::io::stdin()
        .lock()
        .read_line(&mut value)
        .map_err(|_| prompt_error())?;
    while matches!(value.chars().last(), Some('\n' | '\r')) {
        let _ = value.pop();
    }
    Ok(value)
}

#[cfg(unix)]
fn set_terminal_echo(enabled: bool) -> Result<(), CommandError> {
    use std::process::Command;

    let argument = if enabled { "echo" } else { "-echo" };
    let status = Command::new("stty")
        .arg(argument)
        .status()
        .map_err(|_| prompt_error())?;
    if status.success() {
        Ok(())
    } else {
        Err(prompt_error())
    }
}

#[cfg(windows)]
fn set_terminal_echo(enabled: bool) -> Result<(), CommandError> {
    windows_console::set_echo(enabled).map_err(|_| prompt_error())
}

#[cfg(not(any(unix, windows)))]
fn set_terminal_echo(_enabled: bool) -> Result<(), CommandError> {
    Err(prompt_error())
}

#[cfg(windows)]
mod windows_console {
    use std::ffi::c_void;

    const STD_INPUT_HANDLE: u32 = 0xffff_fff6;
    const ENABLE_ECHO_INPUT: u32 = 0x0004;

    #[link(name = "Kernel32")]
    unsafe extern "system" {
        fn GetStdHandle(standard_handle: u32) -> *mut c_void;
        fn GetConsoleMode(console_handle: *mut c_void, mode: *mut u32) -> i32;
        fn SetConsoleMode(console_handle: *mut c_void, mode: u32) -> i32;
    }

    pub(super) fn set_echo(enabled: bool) -> Result<(), ()> {
        // SAFETY: the handle is obtained from the Windows console API and the
        // mode pointers refer to live local values for each call.
        let handle = unsafe { GetStdHandle(STD_INPUT_HANDLE) };
        let mut mode = 0;
        // SAFETY: `handle` is passed back to the matching Windows API and the
        // writable mode pointer is valid for the duration of the call.
        if unsafe { GetConsoleMode(handle, &mut mode) } == 0 {
            return Err(());
        }
        if enabled {
            mode |= ENABLE_ECHO_INPUT;
        } else {
            mode &= !ENABLE_ECHO_INPUT;
        }
        // SAFETY: `handle` and the mode flags were obtained from the console
        // API and remain valid for this synchronous call.
        if unsafe { SetConsoleMode(handle, mode) } == 0 {
            Err(())
        } else {
            Ok(())
        }
    }
}
