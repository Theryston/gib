use super::CommandError;
use crate::input::{OutputMode, WhoamiRequest};
use crate::output;
use gib::Client;

pub fn run(_request: WhoamiRequest, mode: OutputMode) -> Result<(), CommandError> {
    let identity = Client::default()
        .get_global_identity()
        .map_err(CommandError::Sdk)?;
    output::render_identity(&identity, mode, "current");
    Ok(())
}
