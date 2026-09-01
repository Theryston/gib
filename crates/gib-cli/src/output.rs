use crate::input::OutputMode;
use gib::{
    AuthorIdentity, ConfigurationSource, ResolvedConfiguration, SnapshotReference,
    SnapshotSummaryPage, StorageAddResult, StorageBackend, StorageConfigurationMetadata,
    StorageListResult, StorageRemoveResult,
};
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

#[derive(Serialize)]
struct ConfigurationEventOutput {
    loaded: bool,
    source: &'static str,
    path: Option<String>,
}

pub fn render_configuration_source(configuration: &ResolvedConfiguration, mode: OutputMode) {
    let source = configuration.source();
    let source_name = match source {
        ConfigurationSource::Defaults => "defaults",
        ConfigurationSource::Discovered(_) => "discovered",
        ConfigurationSource::Explicit(_) => "explicit",
        ConfigurationSource::Disabled => "disabled",
        _ => "unknown",
    };
    let event = ConfigurationEventOutput {
        loaded: source.is_loaded(),
        source: source_name,
        path: source.path().map(|path| path.display().to_string()),
    };

    match mode {
        OutputMode::Interactive => match source {
            ConfigurationSource::Discovered(path) | ConfigurationSource::Explicit(path) => {
                println!("Loaded local config {}", path.display());
            }
            ConfigurationSource::Disabled => println!("Local config disabled."),
            ConfigurationSource::Defaults => {}
            _ => {}
        },
        OutputMode::Json => render_json("config", event, false),
    }
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

pub fn render_error(
    error: &dyn std::fmt::Display,
    code: &str,
    field: Option<&str>,
    mode: OutputMode,
) {
    match mode {
        OutputMode::Interactive => eprintln!("error: {error}"),
        OutputMode::Json => {
            render_json(
                "error",
                ErrorOutput {
                    message: error.to_string(),
                    code,
                    field,
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
    #[serde(skip_serializing_if = "Option::is_none")]
    field: Option<&'a str>,
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

#[derive(Serialize)]
struct StorageOutput {
    name: String,
    backend: String,
    storage_type: String,
    path: Option<String>,
    region: Option<String>,
    bucket: Option<String>,
    endpoint: Option<String>,
    url: Option<String>,
    credentials_configured: bool,
    health: String,
}

#[derive(Serialize)]
struct StorageAddOutput {
    action: &'static str,
    storage: StorageOutput,
    replaced_existing: bool,
}

#[derive(Serialize)]
struct StorageListOutput {
    action: &'static str,
    storages: Vec<StorageOutput>,
}

#[derive(Serialize)]
struct StorageRemoveOutput {
    action: &'static str,
    name: String,
    backend: String,
    credentials_removed: bool,
    repository_data_preserved: bool,
}

pub fn render_storage_add(result: &StorageAddResult, mode: OutputMode) {
    let storage = storage_output(result.metadata());
    match mode {
        OutputMode::Interactive => println!(
            "{} storage '{}' ({}) [{}]",
            if result.replaced_existing() {
                "Replaced"
            } else {
                "Added"
            },
            storage.name,
            storage.backend,
            storage.health
        ),
        OutputMode::Json => render_json(
            "storage",
            StorageAddOutput {
                action: if result.replaced_existing() {
                    "replaced"
                } else {
                    "added"
                },
                storage,
                replaced_existing: result.replaced_existing(),
            },
            false,
        ),
    }
}

pub fn render_storage_list(result: &StorageListResult, mode: OutputMode) {
    let storages = result
        .storages()
        .iter()
        .map(storage_output)
        .collect::<Vec<_>>();
    match mode {
        OutputMode::Interactive => {
            if storages.is_empty() {
                println!("No storages.");
                return;
            }
            println!("NAME\tBACKEND\tENDPOINT/PATH\tHEALTH\tCREDENTIALS");
            for storage in storages {
                let location = storage
                    .path
                    .as_deref()
                    .or(storage.endpoint.as_deref())
                    .or(storage.url.as_deref())
                    .unwrap_or("-");
                println!(
                    "{}\t{}\t{}\t{}\t{}",
                    storage.name,
                    storage.backend,
                    location,
                    storage.health,
                    if storage.credentials_configured {
                        "configured"
                    } else {
                        "not_configured"
                    }
                );
            }
        }
        OutputMode::Json => render_json(
            "storage",
            StorageListOutput {
                action: "listed",
                storages,
            },
            false,
        ),
    }
}

pub fn render_storage_remove(result: &StorageRemoveResult, mode: OutputMode) {
    let name = result.name().to_string();
    let backend = result.backend().to_string();
    match mode {
        OutputMode::Interactive => println!(
            "Removed storage '{}' ({}) and preserved repository data.",
            name, backend
        ),
        OutputMode::Json => render_json(
            "storage",
            StorageRemoveOutput {
                action: "removed",
                name,
                backend,
                credentials_removed: result.credentials_removed(),
                repository_data_preserved: result.repository_data_preserved(),
            },
            false,
        ),
    }
}

fn storage_output(metadata: &StorageConfigurationMetadata) -> StorageOutput {
    let (path, region, bucket, endpoint, url) = match metadata.backend() {
        StorageBackend::Local(settings) => (
            Some(settings.root().display().to_string()),
            None,
            None,
            None,
            None,
        ),
        StorageBackend::S3(settings) => (
            None,
            Some(settings.region().to_owned()),
            Some(settings.bucket().to_owned()),
            settings.endpoint().map(ToOwned::to_owned),
            None,
        ),
        StorageBackend::WebDav(settings) => (
            None,
            None,
            None,
            None,
            Some(settings.collection_url().to_owned()),
        ),
        _ => (None, None, None, None, None),
    };
    let backend = metadata.backend().kind().to_string();
    StorageOutput {
        name: metadata.name().to_string(),
        backend: backend.clone(),
        storage_type: backend,
        path,
        region,
        bucket,
        endpoint,
        url,
        credentials_configured: metadata.credentials_configured(),
        health: metadata.health().to_string(),
    }
}
