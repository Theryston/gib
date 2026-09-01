use crate::commands::{self, CommandError};
use crate::input::{self, Command};
use crate::output;
use std::process::ExitCode;

pub fn run() -> ExitCode {
    let mut cli = input::parse();
    let mode = cli.mode;
    if cli.command.is_none() {
        input::print_help();
        return ExitCode::SUCCESS;
    }

    if matches!(&cli.command, Some(Command::Storage(_)))
        && let Some(Command::Storage(request)) = cli.command.take()
    {
        return match commands::storage::run(*request, mode) {
            Ok(()) => ExitCode::SUCCESS,
            Err(error) => {
                output::render_error(&error, error.code(), error.field(), mode);
                ExitCode::from(error.exit_code())
            }
        };
    }

    let current_directory = match std::env::current_dir() {
        Ok(path) => path,
        Err(_) => {
            let error = CommandError::Sdk(gib::SdkError::InvalidRequest {
                field: "starting_directory",
                reason: "could not determine the current directory",
            });
            output::render_error(&error, error.code(), error.field(), mode);
            return ExitCode::from(error.exit_code());
        }
    };
    let configuration =
        match commands::resolve_configuration(cli.configuration_request(current_directory)) {
            Ok(configuration) => configuration,
            Err(error) => {
                output::render_error(&error, error.code(), error.field(), mode);
                return ExitCode::from(error.exit_code());
            }
        };
    if !configuration.source().is_default() {
        output::render_configuration_source(&configuration, mode);
    }

    let Some(command) = cli.command else {
        return ExitCode::SUCCESS;
    };

    match command {
        Command::Config(request) => match commands::config::run(request, mode) {
            Ok(()) => ExitCode::SUCCESS,
            Err(error) => {
                output::render_error(&error, error.code(), error.field(), mode);
                ExitCode::from(error.exit_code())
            }
        },
        Command::Log(request) => match commands::log::run(request, mode) {
            Ok(()) => ExitCode::SUCCESS,
            Err(error) => {
                output::render_error(&error, error.code(), error.field(), mode);
                ExitCode::from(error.exit_code())
            }
        },
        Command::Resolve(request) => match commands::resolve::run(request, mode) {
            Ok(()) => ExitCode::SUCCESS,
            Err(error) => {
                output::render_error(&error, error.code(), error.field(), mode);
                ExitCode::from(error.exit_code())
            }
        },
        Command::Whoami(request) => match commands::whoami::run(request, mode) {
            Ok(()) => ExitCode::SUCCESS,
            Err(error) => {
                output::render_error(&error, error.code(), error.field(), mode);
                ExitCode::from(error.exit_code())
            }
        },
        Command::Storage(request) => match commands::storage::run(*request, mode) {
            Ok(()) => ExitCode::SUCCESS,
            Err(error) => {
                output::render_error(&error, error.code(), error.field(), mode);
                ExitCode::from(error.exit_code())
            }
        },
    }
}

impl CommandError {
    fn exit_code(&self) -> u8 {
        match self {
            Self::Storage(_) => 1,
            Self::StorageConfiguration(error) => {
                if error.is_conflict() {
                    3
                } else if error.is_input_error() {
                    2
                } else {
                    1
                }
            }
            Self::Sdk(error) => match error {
                gib::SdkError::SnapshotReferenceEmpty
                | gib::SdkError::SnapshotReferenceMalformed
                | gib::SdkError::SnapshotReferenceNotFound
                | gib::SdkError::SnapshotReferenceAmbiguous
                | gib::SdkError::RepositoryNoSnapshots
                | gib::SdkError::IdentityNotConfigured
                | gib::SdkError::InvalidRequest { .. } => 2,
                _ => 1,
            },
            Self::Configuration(_) => 2,
        }
    }
}
