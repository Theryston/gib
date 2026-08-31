use super::{CommandError, open_repository};
use crate::input::OutputMode;
use crate::input::ResolveRequest;
use crate::output;

pub fn run(request: ResolveRequest, mode: OutputMode) -> Result<(), CommandError> {
    let repository = open_repository(request.repository_path())?;
    let reference = repository
        .resolve_snapshot_reference(&request.reference)
        .map_err(CommandError::Sdk)?;
    output::render_resolved_reference(&reference, mode);
    Ok(())
}
