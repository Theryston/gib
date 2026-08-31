use gib::{SnapshotReference, SnapshotSummaryPage};

pub fn render_history(page: &SnapshotSummaryPage) {
    for summary in page.summaries() {
        let timestamp = summary
            .timestamp()
            .map_or_else(|| String::from("-"), |value| value.to_string());
        let size = summary
            .size()
            .map_or_else(|| String::from("-"), |value| value.to_string());
        println!(
            "{}\t{}\t{}\t{}",
            summary.id(),
            timestamp,
            size,
            summary.message()
        );
    }
}

pub fn render_empty_history() {
    println!("No snapshots.");
}

pub fn render_resolved_reference(reference: &SnapshotReference) {
    println!("{reference}");
}

pub fn render_error(error: &dyn std::fmt::Display) {
    eprintln!("error: {error}");
}
