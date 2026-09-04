use crate::input::OutputMode;
use crate::interactive;
use gib::{
    AuthorIdentity, ConfigurationSource, ResolvedConfiguration, SnapshotReference,
    SnapshotSummaryPage, StorageAddRequest, StorageAddResult, StorageBackend,
    StorageConfigurationMetadata, StorageListResult, StorageRemoveResult,
};
use serde::Serialize;
use std::path::Path;
use time::{OffsetDateTime, format_description::BorrowedFormatItem};

const CLI_OUTPUT_SCHEMA_VERSION: u16 = 1;
const HISTORY_TABLE_HEADERS: [&str; 5] = ["SNAPSHOT", "SIZE", "AUTHOR", "TIME", "MESSAGE"];
const HISTORY_TIMESTAMP_FORMAT: &[BorrowedFormatItem<'static>] =
    time::macros::format_description!("[month repr:short] [day], [year] [hour]:[minute] UTC");

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
                interactive::info(&format!("Using local configuration · {}", path.display()));
            }
            ConfigurationSource::Disabled => {
                interactive::warning("Local configuration discovery is disabled for this command.")
            }
            ConfigurationSource::Defaults => {}
            _ => {}
        },
        OutputMode::Json => render_json("config", event, false),
    }
}

pub fn render_identity(identity: &AuthorIdentity, mode: OutputMode, action: &str) {
    match mode {
        OutputMode::Interactive => {
            let configured = action == "configured";
            interactive::banner(
                if configured {
                    "Identity configured"
                } else {
                    "Your identity"
                },
                if configured {
                    "This author will be attached to your snapshots."
                } else {
                    "The author currently associated with your snapshots."
                },
            );
            interactive::card("Author profile", &[("Author", identity.to_string())]);
            interactive::success_value(
                if configured {
                    "Configured author"
                } else {
                    "You are"
                },
                identity.as_str(),
            );
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

pub fn render_identity_prompt_start() {
    interactive::section(
        "Set your author identity",
        Some("Use the format Name <email>; this is saved in your global profile"),
    );
}

pub fn render_history_start(repository: &Path, page_size: usize, mode: OutputMode) {
    if mode != OutputMode::Interactive {
        return;
    }
    interactive::banner(
        "Snapshot history",
        "A calm, newest-first view of your repository timeline.",
    );
    interactive::card(
        "Query",
        &[
            ("Repository", repository.display().to_string()),
            ("Page size", page_size.to_string()),
        ],
    );
    println!();
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

    let rows = page
        .summaries()
        .iter()
        .map(|summary| {
            vec![
                shorten(summary.id().as_ref(), 16),
                summary
                    .size()
                    .map_or_else(|| String::from("-"), format_size),
                summary
                    .author()
                    .map_or_else(|| String::from("-"), |value| shorten(value, 18)),
                summary
                    .timestamp()
                    .map_or_else(|| String::from("-"), format_timestamp),
                shorten(summary.message(), 42),
            ]
        })
        .collect::<Vec<_>>();
    interactive::table(&HISTORY_TABLE_HEADERS, &rows);
}

pub fn render_history_complete(count: usize, mode: OutputMode) {
    if mode == OutputMode::Interactive {
        interactive::success(
            &format!(
                "{count} snapshot{} shown",
                if count == 1 { "" } else { "s" }
            ),
            Some("Use `gib resolve latest` to inspect the newest snapshot."),
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
        interactive::info("No snapshots yet. Run a backup to create your first checkpoint.");
    }
}

pub fn render_resolved_reference(requested: &str, reference: &SnapshotReference, mode: OutputMode) {
    if mode == OutputMode::Json {
        render_json(
            "output",
            ResolvedReferenceOutput {
                reference: reference.to_string(),
            },
            false,
        );
    } else {
        interactive::banner(
            "Snapshot resolved",
            "The reference points to an immutable snapshot.",
        );
        interactive::card(
            "Resolution",
            &[
                ("Requested", requested.to_owned()),
                ("Snapshot", reference.to_string()),
            ],
        );
        interactive::success_line("Snapshot reference is ready to use.");
    }
}

pub fn render_error(
    error: &dyn std::fmt::Display,
    code: &str,
    field: Option<&str>,
    mode: OutputMode,
) {
    match mode {
        OutputMode::Interactive => {
            interactive::error("Command failed", &error.to_string(), code, field);
            if code == "storage_credential_store_failure" {
                render_credential_store_guidance();
            }
        }
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

struct CredentialStoreGuidance {
    summary: &'static str,
    commands: Option<&'static [&'static str]>,
    steps: &'static [&'static str],
}

fn render_credential_store_guidance() {
    let Some(guidance) = platform_credential_store_guidance() else {
        return;
    };
    interactive::warning(guidance.summary);
    interactive::newline();
    if let Some(commands) = guidance.commands {
        interactive::code_block("Install it with:", commands);
    }
    interactive::steps("Next steps", guidance.steps);
    interactive::newline();
    interactive::warning("Plaintext credential files are intentionally unsupported.");
}

fn platform_credential_store_guidance() -> Option<CredentialStoreGuidance> {
    #[cfg(target_os = "linux")]
    {
        Some(CredentialStoreGuidance {
            summary: "Gib could not access the Linux secure credential store: the `secret-tool` command is required.",
            commands: Some(&[
                "sudo apt update",
                "sudo apt install libsecret-tools gnome-keyring",
            ]),
            steps: &[
                "Ensure GNOME Keyring or another Secret Service is running.",
                "Retry `gib storage add`.",
            ],
        })
    }
    #[cfg(target_os = "macos")]
    {
        Some(CredentialStoreGuidance {
            summary: "Gib could not access the macOS Keychain.",
            commands: None,
            steps: &[
                "Open Keychain Access from Applications → Utilities.",
                "Select the login keychain and unlock it.",
                "Retry `gib storage add` and allow Gib to access the keychain.",
            ],
        })
    }
    #[cfg(windows)]
    {
        Some(CredentialStoreGuidance {
            summary: "Gib could not access Windows Credential Manager.",
            commands: None,
            steps: &[
                "Open Start → Credential Manager.",
                "Open Windows Credentials and verify that the credential service is available.",
                "Retry `gib storage add` and approve any security prompt.",
            ],
        })
    }
    #[cfg(not(any(target_os = "linux", target_os = "macos", windows)))]
    {
        Some(CredentialStoreGuidance {
            summary: "This operating system has no supported secure credential-store integration.",
            commands: None,
            steps: &[
                "Run Gib on Linux, macOS, or Windows with its secure credential store available.",
            ],
        })
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

pub fn render_storage_add_start(name: Option<&str>) {
    interactive::banner(
        "Add a storage",
        "Connect a local folder, an S3 bucket, or a WebDAV collection.",
    );
    if let Some(name) = name {
        interactive::info(&format!("Configuring `{}`", one_line(name, 54)));
    }
}

pub fn render_storage_add_review(request: &StorageAddRequest) {
    let backend = request.configuration().backend();
    let mut fields = vec![
        ("Name", request.name().to_string()),
        ("Backend", backend.kind().to_string()),
    ];
    match backend {
        StorageBackend::Local(settings) => {
            fields.push(("Folder", settings.root().display().to_string()));
        }
        StorageBackend::S3(settings) => {
            fields.push(("Region", settings.region().to_owned()));
            fields.push(("Bucket", settings.bucket().to_owned()));
            fields.push((
                "Endpoint",
                settings
                    .endpoint()
                    .map_or_else(|| "AWS default".to_owned(), ToOwned::to_owned),
            ));
            fields.push((
                "Addressing",
                if settings.force_path_style() {
                    "Path style".to_owned()
                } else {
                    "Virtual host".to_owned()
                },
            ));
        }
        StorageBackend::WebDav(settings) => {
            fields.push(("Collection", settings.collection_url().to_owned()));
            fields.push((
                "Transport",
                if settings.allow_insecure_http() {
                    "HTTP (explicitly allowed)".to_owned()
                } else {
                    "HTTPS".to_owned()
                },
            ));
        }
        _ => {}
    }
    fields.push((
        "Credentials",
        if request.configuration().credentials().is_some() {
            "configured".to_owned()
        } else {
            "not required".to_owned()
        },
    ));
    interactive::section(
        "Check the details",
        Some("nothing is saved until you confirm"),
    );
    interactive::card("Review before saving", &fields);
}

pub fn render_storage_checking(request: &StorageAddRequest) {
    interactive::info(&format!(
        "Checking {} connectivity before saving…",
        request.configuration().backend().kind()
    ));
}

pub fn render_storage_conflict(name: &str) {
    interactive::warning(&format!(
        "A storage named `{}` already exists.",
        one_line(name, 54)
    ));
}

pub fn render_storage_add(result: &StorageAddResult, mode: OutputMode) {
    let storage = storage_output(result.metadata());
    match mode {
        OutputMode::Interactive => {
            interactive::success_line(&format!(
                "{} storage '{}' ({})",
                if result.replaced_existing() {
                    "Replaced"
                } else {
                    "Added"
                },
                storage.name,
                storage.backend
            ));
            interactive::card("Stored configuration", &storage_card_fields(&storage));
        }
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

pub fn render_storage_list_start(check_health: bool) {
    interactive::banner(
        "Storage spaces",
        if check_health {
            "Reviewing configured backends and checking their health."
        } else {
            "Your named storage destinations at a glance."
        },
    );
}

pub fn render_storage_health_check_start() {
    interactive::info("Running read-only connectivity checks…");
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
                interactive::info("No storages configured yet.");
                return;
            }
            let rows = storages
                .iter()
                .map(|storage| {
                    let location = storage_location(storage);
                    let health = if storage.health == "healthy" {
                        "● healthy".to_owned()
                    } else {
                        "not checked".to_owned()
                    };
                    vec![
                        shorten(&storage.name, 18),
                        storage.backend.clone(),
                        shorten(&location, 34),
                        health,
                        storage_credentials_label(storage).to_owned(),
                    ]
                })
                .collect::<Vec<_>>();
            interactive::table(
                &["NAME", "BACKEND", "LOCATION", "HEALTH", "CREDENTIALS"],
                &rows,
            );
            interactive::success(
                &format!(
                    "{} storage{} configured",
                    storages.len(),
                    if storages.len() == 1 { "" } else { "s" }
                ),
                None,
            );
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

pub fn render_storage_remove_start() {
    interactive::banner(
        "Remove a storage",
        "This removes only Gib’s configuration and credentials.",
    );
}

pub fn render_storage_remove_review(metadata: &StorageConfigurationMetadata) {
    let storage = storage_output(metadata);
    interactive::section("Confirm removal", Some("repository data remains untouched"));
    interactive::card("Removal preview", &storage_card_fields(&storage));
    interactive::warning("Repository contents will not be deleted.");
}

pub fn render_storage_remove(result: &StorageRemoveResult, mode: OutputMode) {
    let name = result.name().to_string();
    let backend = result.backend().to_string();
    match mode {
        OutputMode::Interactive => {
            interactive::success_line(&format!("Removed storage '{}' ({})", name, backend));
            interactive::info(
                "Repository data was preserved; only the configuration and credential were removed.",
            );
        }
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

fn storage_location(storage: &StorageOutput) -> String {
    storage
        .path
        .as_deref()
        .or(storage.endpoint.as_deref())
        .or(storage.url.as_deref())
        .or(storage.bucket.as_deref())
        .unwrap_or("-")
        .to_owned()
}

fn storage_card_fields(storage: &StorageOutput) -> Vec<(&'static str, String)> {
    let mut fields = vec![
        ("Name", storage.name.clone()),
        ("Backend", storage.backend.clone()),
        ("Location", storage_location(storage)),
    ];
    if let Some(region) = &storage.region {
        fields.push(("Region", region.clone()));
    }
    if let Some(bucket) = &storage.bucket {
        fields.push(("Bucket", bucket.clone()));
    }
    fields.push(("Health", storage_health_label(storage)));
    fields.push(("Credentials", storage_credentials_label(storage).to_owned()));
    fields
}

fn storage_health_label(storage: &StorageOutput) -> String {
    match storage.health.as_str() {
        "healthy" => "● healthy".to_owned(),
        _ => "not checked".to_owned(),
    }
}

fn storage_credentials_label(storage: &StorageOutput) -> &'static str {
    if storage.credentials_configured {
        "configured"
    } else {
        "not required"
    }
}

fn format_size(size: u64) -> String {
    const UNITS: [&str; 4] = ["B", "KiB", "MiB", "GiB"];
    let mut value = size as f64;
    let mut unit = 0;
    while value >= 1024.0 && unit < UNITS.len() - 1 {
        value /= 1024.0;
        unit += 1;
    }
    if unit == 0 {
        format!("{size} B")
    } else {
        format!("{value:.1} {}", UNITS[unit])
    }
}

fn format_timestamp(timestamp: u64) -> String {
    let Ok(timestamp) = i64::try_from(timestamp) else {
        return String::from("-");
    };
    let Ok(date_time) = OffsetDateTime::from_unix_timestamp(timestamp) else {
        return String::from("-");
    };
    date_time
        .format(HISTORY_TIMESTAMP_FORMAT)
        .unwrap_or_else(|_| String::from("-"))
}

fn shorten(value: &str, max_chars: usize) -> String {
    let value = value
        .chars()
        .map(|character| {
            if character.is_control() {
                ' '
            } else {
                character
            }
        })
        .collect::<String>();
    if value.chars().count() <= max_chars {
        return value;
    }
    let mut shortened = value
        .chars()
        .take(max_chars.saturating_sub(1))
        .collect::<String>();
    shortened.push('…');
    shortened
}

fn one_line(value: &str, max_chars: usize) -> String {
    value
        .chars()
        .map(|character| {
            if character.is_control() {
                ' '
            } else {
                character
            }
        })
        .take(max_chars)
        .collect()
}

#[cfg(test)]
mod tests {
    use super::{format_timestamp, platform_credential_store_guidance};

    #[test]
    fn credential_store_failure_has_platform_guidance() {
        let guidance = platform_credential_store_guidance();
        assert!(guidance.is_some());

        #[cfg(target_os = "linux")]
        assert!(guidance.is_some_and(|guidance| {
            guidance.summary.contains("secret-tool")
                && guidance.commands.is_some_and(|commands| {
                    commands
                        .iter()
                        .any(|command| command.contains("libsecret-tools"))
                })
        }));
    }

    #[test]
    fn history_timestamp_is_human_readable_and_uses_utc() {
        assert_eq!(format_timestamp(0), "Jan 01, 1970 00:00 UTC");
        assert_eq!(format_timestamp(u64::MAX), "-");
    }
}
