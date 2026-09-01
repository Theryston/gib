use super::CommandError;
use crate::input::{ConfigRequest, OutputMode};
use crate::interactive;
use crate::output;
use gib::{Client, SdkError};

pub fn run(request: ConfigRequest, mode: OutputMode) -> Result<(), CommandError> {
    if mode == OutputMode::Interactive && request.author.is_none() {
        output::render_identity_prompt_start();
    }
    let author = match request.author {
        Some(author) => author,
        None if mode == OutputMode::Json => {
            return Err(CommandError::Sdk(SdkError::InvalidRequest {
                field: "author",
                reason: "author is required",
            }));
        }
        None => interactive::text("Author identity (Name <email>)").map_err(CommandError::Sdk)?,
    };

    let identity = Client::default()
        .set_global_identity(author)
        .map_err(CommandError::Sdk)?;
    output::render_identity(&identity, mode, "configured");
    Ok(())
}
