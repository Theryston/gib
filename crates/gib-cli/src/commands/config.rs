use super::CommandError;
use crate::input::{ConfigRequest, OutputMode};
use crate::output;
use gib::{Client, SdkError};
use std::io::{self, Write};

pub fn run(request: ConfigRequest, mode: OutputMode) -> Result<(), CommandError> {
    let author = match request.author {
        Some(author) => author,
        None if mode == OutputMode::Json => {
            return Err(CommandError::Sdk(SdkError::InvalidRequest {
                field: "author",
                reason: "author is required",
            }));
        }
        None => prompt_for_author().map_err(CommandError::Sdk)?,
    };

    let identity = Client::default()
        .set_global_identity(author)
        .map_err(CommandError::Sdk)?;
    output::render_identity(&identity, mode, "configured");
    Ok(())
}

fn prompt_for_author() -> Result<String, SdkError> {
    print!("Enter your author (e.g. 'Jane Doe <jane@example.com>'): ");
    io::stdout()
        .flush()
        .map_err(|_| SdkError::ConfigurationFailure {
            operation: "prompt",
        })?;
    let mut author = String::new();
    io::stdin()
        .read_line(&mut author)
        .map_err(|_| SdkError::ConfigurationFailure {
            operation: "prompt",
        })?;
    while matches!(author.chars().last(), Some('\n' | '\r')) {
        let _ = author.pop();
    }
    Ok(author)
}
