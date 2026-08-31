use crate::commands::{self, CommandError};
use crate::input::{self, Command};
use crate::output;
use std::process::ExitCode;

pub fn run() -> ExitCode {
    let cli = input::parse();
    let mode = cli.mode;
    let Some(command) = cli.command else {
        input::print_help();
        return ExitCode::SUCCESS;
    };

    match command {
        Command::Config(request) => match commands::config::run(request, mode) {
            Ok(()) => ExitCode::SUCCESS,
            Err(error) => {
                output::render_error(&error, error.code(), mode);
                ExitCode::from(error.exit_code())
            }
        },
        Command::Log(request) => match commands::log::run(request, mode) {
            Ok(()) => ExitCode::SUCCESS,
            Err(error) => {
                output::render_error(&error, error.code(), mode);
                ExitCode::from(error.exit_code())
            }
        },
        Command::Resolve(request) => match commands::resolve::run(request, mode) {
            Ok(()) => ExitCode::SUCCESS,
            Err(error) => {
                output::render_error(&error, error.code(), mode);
                ExitCode::from(error.exit_code())
            }
        },
        Command::Whoami(request) => match commands::whoami::run(request, mode) {
            Ok(()) => ExitCode::SUCCESS,
            Err(error) => {
                output::render_error(&error, error.code(), mode);
                ExitCode::from(error.exit_code())
            }
        },
    }
}

impl CommandError {
    fn exit_code(&self) -> u8 {
        match self {
            Self::Storage(_) => 1,
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
        }
    }
}
