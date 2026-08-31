use super::{CommandError, open_repository};
use crate::input::LogRequest;
use crate::input::OutputMode;
use crate::output;
use gib::SnapshotListRequest;

pub fn run(request: LogRequest, mode: OutputMode) -> Result<(), CommandError> {
    let repository = open_repository(request.repository_path())?;
    let mut cursor = request.after;
    let mut emitted = false;

    loop {
        let page_request = SnapshotListRequest::new().with_limit(request.page_size);
        let page_request = match cursor.take() {
            Some(cursor) => page_request.after(cursor),
            None => page_request,
        };
        let page = repository
            .list_history(page_request)
            .map_err(CommandError::Sdk)?;
        emitted |= !page.is_empty();
        output::render_history(&page, mode);
        cursor = page.next_cursor().cloned();
        if cursor.is_none() {
            break;
        }
    }

    if !emitted {
        output::render_empty_history(mode);
    }
    Ok(())
}
