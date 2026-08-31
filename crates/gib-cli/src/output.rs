use crate::input::OutputMode;
use gib::{AuthorIdentity, SnapshotReference, SnapshotSummaryPage};
use serde::Serialize;

const CLI_OUTPUT_SCHEMA_VERSION: u16 = 1;

#[derive(Serialize)]
struct OutputEnvelope<'a, T: Serialize> {
    version: u16,
    #[serde(rename = "type")]
    kind: &'a str,
    data: T,
}

#[derive(Serialize)]
struct IdentityOutput<'a> {
    author: &'a str,
}

#[derive(Serialize)]
struct HistoryOutput {
    summaries: Vec<HistorySummaryOutput>,
    next_cursor: Option<String>,
}

#[derive(Serialize)]
struct HistorySummaryOutput {
    id: String,
    reference: String,
    parent: Option<String>,
    message: String,
    author: Option<String>,
    timestamp: Option<u64>,
    size: Option<u64>,
}

pub fn render_identity(identity: &AuthorIdentity, mode: OutputMode, action: &str) {
    match mode {
        OutputMode::Interactive => {
            if action == "configured" {
                println!("Configured author: {identity}");
            } else {
                println!("You are: {identity}");
            }
        }
        OutputMode::Json => {
            let envelope = OutputEnvelope {
                version: CLI_OUTPUT_SCHEMA_VERSION,
                kind: "output",
                data: IdentityOutput {
                    author: identity.as_str(),
                },
            };
            if let Ok(json) = serde_json::to_string(&envelope) {
                println!("{json}");
            }
        }
    }
}

pub fn render_history(page: &SnapshotSummaryPage, mode: OutputMode) {
    if mode == OutputMode::Json {
        let data = HistoryOutput {
            summaries: page
                .summaries()
                .iter()
                .map(|summary| HistorySummaryOutput {
                    id: summary.id().to_string(),
                    reference: summary.reference().to_string(),
                    parent: summary.parent().map(ToString::to_string),
                    message: summary.message().to_owned(),
                    author: summary.author().map(ToOwned::to_owned),
                    timestamp: summary.timestamp(),
                    size: summary.size(),
                })
                .collect(),
            next_cursor: page.next_cursor().map(ToString::to_string),
        };
        render_json("output", data, false);
        return;
    }

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

pub fn render_empty_history(mode: OutputMode) {
    if mode == OutputMode::Json {
        render_json(
            "output",
            HistoryOutput {
                summaries: Vec::new(),
                next_cursor: None,
            },
            false,
        );
    } else {
        println!("No snapshots.");
    }
}

pub fn render_resolved_reference(reference: &SnapshotReference, mode: OutputMode) {
    if mode == OutputMode::Json {
        render_json(
            "output",
            ResolvedReferenceOutput {
                reference: reference.to_string(),
            },
            false,
        );
    } else {
        println!("{reference}");
    }
}

pub fn render_error(error: &dyn std::fmt::Display, code: &str, mode: OutputMode) {
    match mode {
        OutputMode::Interactive => eprintln!("error: {error}"),
        OutputMode::Json => {
            render_json(
                "error",
                ErrorOutput {
                    message: error.to_string(),
                    code,
                },
                true,
            );
        }
    }
}

#[derive(Serialize)]
struct ErrorOutput<'a> {
    message: String,
    code: &'a str,
}

#[derive(Serialize)]
struct ResolvedReferenceOutput {
    reference: String,
}

fn render_json<T: Serialize>(kind: &'static str, data: T, to_stderr: bool) {
    let envelope = OutputEnvelope {
        version: CLI_OUTPUT_SCHEMA_VERSION,
        kind,
        data,
    };
    if let Ok(json) = serde_json::to_string(&envelope) {
        if to_stderr {
            eprintln!("{json}");
        } else {
            println!("{json}");
        }
    }
}
