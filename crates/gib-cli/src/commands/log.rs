use super::{CommandError, open_repository};
use crate::input::LogRequest;
use crate::input::OutputMode;
use crate::output;
use gib::SnapshotListRequest;

pub fn run(request: LogRequest, mode: OutputMode) -> Result<(), CommandError> {
    output::render_history_start(request.repository_path(), request.page_size, mode);
    let repository = open_repository(request.repository_path())?;
    let mut cursor = request.after;
    let mut emitted = false;
    let mut count = 0_usize;

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
        count = count.saturating_add(page.summaries().len());
        output::render_history(&page, mode);
        cursor = page.next_cursor().cloned();
        if cursor.is_none() {
            break;
        }
    }

    if !emitted {
        output::render_empty_history(mode);
    } else {
        output::render_history_complete(count, mode);
    }
    Ok(())
}
