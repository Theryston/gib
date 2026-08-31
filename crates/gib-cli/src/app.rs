use crate::commands::{self, CommandError};
use crate::input::{self, Command};
use crate::output;
use std::process::ExitCode;

pub fn run() -> ExitCode {
    let cli = input::parse();
    let Some(command) = cli.command else {
        input::print_help();
        return ExitCode::SUCCESS;
    };

    match command {
        Command::Log(request) => match commands::log::run(request) {
            Ok(()) => ExitCode::SUCCESS,
            Err(error) => {
                output::render_error(&error);
                ExitCode::from(error.exit_code())
            }
        },
        Command::Resolve(request) => match commands::resolve::run(request) {
            Ok(()) => ExitCode::SUCCESS,
            Err(error) => {
                output::render_error(&error);
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
                | gib::SdkError::InvalidRequest { .. } => 2,
                _ => 1,
            },
        }
    }
}
