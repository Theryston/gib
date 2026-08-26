use crate::config::{
    PasswordPolicy, RepositoryOptions, load_and_report_local_config, load_local_config,
    resolve_repository,
};
use crate::core::catalog::{
    CatalogEntryScope, CatalogEntrySummary, CatalogState, lookup_entries_by_tokens, lookup_path,
    normalize_relative_path, path_tokens, read_catalog_status,
};
use crate::output::{emit_output, is_json_mode};
use crate::utils::handle_error;
use clap::ArgMatches;
use console::style;
use serde::Serialize;
use std::sync::Arc;

const DEFAULT_SEARCH_LIMIT: usize = 100;
const CATALOG_PAGE_SIZE: usize = 256;
const NO_INDEXED_BACKUPS_MESSAGE: &str = "No searchable backups yet. New backups are indexed automatically; existing older snapshots remain usable normally.";
const DEGRADED_INDEX_WARNING: &str = "The historical search catalog is degraded; search results may be incomplete until pending backups are indexed.";

#[derive(Debug, Clone, PartialEq, Eq)]
struct SearchRequest {
    query: String,
    tokens: Vec<String>,
    path_prefix: Option<String>,
    extension: Option<String>,
    limit: usize,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
enum SearchIndexStatus {
    Ready,
    Degraded,
    NoIndexedBackups,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
struct SearchResult {
    path: String,
    last_backup: String,
    restore_command: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
struct SearchResponse {
    query: String,
    index_status: SearchIndexStatus,
    results: Vec<SearchResult>,
    truncated: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    warning: Option<String>,
}

#[derive(Debug, Clone)]
struct RankedSearchResult {
    newest_revision_timestamp: u64,
    result: SearchResult,
}

pub async fn search(matches: &ArgMatches) {
    let request = match parse_search_request(matches) {
        Ok(request) => request,
        Err(error) => handle_error(error, None),
    };

    let repository = match get_params(matches) {
        Ok(repository) => repository,
        Err(error) => handle_error(error, None),
    };

    let response =
        match run_search(repository.fs, repository.key, repository.password, request).await {
            Ok(response) => response,
            Err(error) => handle_error(error, None),
        };

    render_response(&response);
}

fn get_params(matches: &ArgMatches) -> Result<RepositoryOptions, String> {
    let local_config = if is_json_mode() {
        // Search emits one stable result payload in JSON mode. The values still
        // come from the same resolver used by the other read-only commands.
        load_local_config(matches)?
    } else {
        load_and_report_local_config(matches)?
    };

    resolve_repository(
        matches,
        &local_config,
        PasswordPolicy {
            required: false,
            readonly: true,
        },
        None,
    )
}

fn parse_search_request(matches: &ArgMatches) -> Result<SearchRequest, String> {
    let query = matches
        .get_one::<String>("query")
        .map(|value| value.as_str())
        .unwrap_or_default();
    let path = matches
        .get_one::<String>("path")
        .map(|value| value.as_str());
    let extension = matches
        .get_one::<String>("extension")
        .map(|value| value.as_str());
    let limit = matches
        .get_one::<usize>("limit")
        .copied()
        .unwrap_or(DEFAULT_SEARCH_LIMIT);

    parse_search_values(query, path, extension, limit)
}

fn parse_search_values(
    query: &str,
    path: Option<&str>,
    extension: Option<&str>,
    limit: usize,
) -> Result<SearchRequest, String> {
    let query = query.trim();
    if query.is_empty() {
        return Err("Search query cannot be empty".to_string());
    }

    let tokens = path_tokens(query);
    if tokens.is_empty() {
        return Err("Search query must contain at least one searchable token".to_string());
    }

    if limit == 0 {
        return Err("--limit must be greater than zero".to_string());
    }

    let path_prefix = match path {
        Some(value) => parse_path_prefix(value)?,
        None => None,
    };
    let extension = match extension {
        Some(value) => Some(parse_extension(value)?),
        None => None,
    };

    Ok(SearchRequest {
        query: query.to_string(),
        tokens,
        path_prefix,
        extension,
        limit,
    })
}

fn parse_path_prefix(value: &str) -> Result<Option<String>, String> {
    let value = value.trim();
    if value.is_empty() {
        return Err("The --path prefix cannot be empty".to_string());
    }

    let normalized = normalize_relative_path(value)
        .map_err(|error| format!("Invalid --path prefix '{}': {}", value, error))?;
    Ok((!normalized.is_empty()).then_some(normalized))
}

fn parse_extension(value: &str) -> Result<String, String> {
    let extension = value.trim();
    if extension.is_empty()
        || extension.starts_with('.')
        || extension.contains('/')
        || extension.contains('\\')
        || extension.contains('\0')
        || extension.chars().any(char::is_whitespace)
    {
        return Err(format!(
            "Invalid --extension '{}': use a file extension without a leading dot or path separators",
            value
        ));
    }

    Ok(lookup_path(extension))
}

async fn run_search(
    fs: Arc<dyn crate::fs::FS>,
    key: String,
    password: Option<String>,
    request: SearchRequest,
) -> Result<SearchResponse, String> {
    let catalog_status =
        read_catalog_status(Arc::clone(&fs), key.clone(), password.clone()).await?;
    let Some(catalog_status) = catalog_status else {
        return Ok(empty_response(
            request.query,
            SearchIndexStatus::NoIndexedBackups,
            None,
        ));
    };

    let warning = (catalog_status.state == CatalogState::Degraded)
        .then(|| DEGRADED_INDEX_WARNING.to_string());
    let no_indexed_backups =
        catalog_status.indexed_backup_count == 0 || catalog_status.latest_indexed_backup.is_none();
    if no_indexed_backups {
        return Ok(empty_response(
            request.query,
            SearchIndexStatus::NoIndexedBackups,
            None,
        ));
    }

    let index_status = if catalog_status.state == CatalogState::Degraded {
        SearchIndexStatus::Degraded
    } else {
        SearchIndexStatus::Ready
    };

    let mut candidates = Vec::new();
    let mut cursor = None;
    loop {
        let page = lookup_entries_by_tokens(
            Arc::clone(&fs),
            key.clone(),
            password.clone(),
            &request.tokens,
            CatalogEntryScope::AllHistory,
            cursor.as_deref(),
            CATALOG_PAGE_SIZE,
        )
        .await?;
        let next_cursor = page.next_cursor.clone();
        candidates.extend(page.items);

        match next_cursor {
            Some(next) if cursor.as_deref() != Some(next.as_str()) => cursor = Some(next),
            _ => break,
        }
    }

    let (results, truncated) = filter_sort_and_limit(candidates, &request);
    Ok(SearchResponse {
        query: request.query,
        index_status,
        results,
        truncated,
        warning,
    })
}

fn empty_response(
    query: String,
    index_status: SearchIndexStatus,
    warning: Option<String>,
) -> SearchResponse {
    SearchResponse {
        query,
        index_status,
        results: Vec::new(),
        truncated: false,
        warning,
    }
}

fn filter_sort_and_limit(
    candidates: Vec<CatalogEntrySummary>,
    request: &SearchRequest,
) -> (Vec<SearchResult>, bool) {
    let mut ranked = candidates
        .into_iter()
        .filter_map(|summary| {
            let backup = summary
                .latest_restorable_backup
                .filter(|backup| !backup.is_empty())?;
            if !matches_path_prefix(&summary.path, request.path_prefix.as_deref())
                || !matches_extension(&summary.path, request.extension.as_deref())
            {
                return None;
            }

            let backup_short = short_backup_hash(&backup);
            Some(RankedSearchResult {
                newest_revision_timestamp: summary.newest_revision_timestamp,
                result: SearchResult {
                    restore_command: build_restore_command(&backup_short, &summary.path),
                    path: summary.path,
                    last_backup: backup_short,
                },
            })
        })
        .collect::<Vec<_>>();

    ranked.sort_by(|left, right| {
        right
            .newest_revision_timestamp
            .cmp(&left.newest_revision_timestamp)
            .then_with(|| left.result.path.cmp(&right.result.path))
    });

    let truncated = ranked.len() > request.limit;
    ranked.truncate(request.limit);
    (
        ranked.into_iter().map(|ranked| ranked.result).collect(),
        truncated,
    )
}

fn matches_path_prefix(path: &str, prefix: Option<&str>) -> bool {
    let Some(prefix) = prefix else {
        return true;
    };

    let path = lookup_path(path);
    let prefix = lookup_path(prefix);
    path == prefix || path.starts_with(&format!("{}/", prefix))
}

fn matches_extension(path: &str, extension: Option<&str>) -> bool {
    let Some(extension) = extension else {
        return true;
    };

    let name = path.rsplit('/').next().unwrap_or(path);
    let suffix = format!(".{}", extension);
    let name = lookup_path(name);
    name.len() > suffix.len() && name.ends_with(&suffix)
}

fn short_backup_hash(hash: &str) -> String {
    hash[..hash.len().min(8)].to_string()
}

fn build_restore_command(backup: &str, path: &str) -> String {
    format!(
        "gib restore --backup {} --only {}",
        shell_quote(backup),
        shell_quote(path)
    )
}

fn shell_quote(value: &str) -> String {
    if value.chars().all(|character| {
        character.is_ascii_alphanumeric() || matches!(character, '_' | '-' | '.' | '/')
    }) {
        return value.to_string();
    }

    format!("'{}'", value.replace('\'', "'\\''"))
}

fn render_response(response: &SearchResponse) {
    if is_json_mode() {
        emit_output(response);
        return;
    }

    if let Some(warning) = &response.warning {
        eprintln!("Warning: {}", warning);
    }

    if response.index_status == SearchIndexStatus::NoIndexedBackups {
        println!("{}", style(NO_INDEXED_BACKUPS_MESSAGE).yellow());
        return;
    }

    if response.results.is_empty() {
        println!(
            "{}",
            style(format!("No files found for query '{}'.", response.query)).yellow()
        );
        return;
    }

    for (index, result) in response.results.iter().enumerate() {
        println!("{}", style(&result.path).cyan().bold());
        println!("  last backup: {}", result.last_backup);
        println!("  restore: {}", result.restore_command);
        if index + 1 < response.results.len() {
            println!();
        }
    }

    if response.truncated {
        println!();
        println!(
            "{}",
            style(format!(
                "Results truncated to {} entries; use --limit to change the maximum.",
                response.results.len()
            ))
            .yellow()
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::core::catalog::{
        index_backup_after_finalize, mark_catalog_degraded_state, remove_backup_from_catalog,
    };
    use crate::core::crypto::encode_file_bytes;
    use crate::core::metadata::{Backup, BackupObject, BackupSummary, ChunkIndex};
    use crate::fs::{FS, LocalFS};
    use crate::utils::compress_bytes;
    use std::collections::HashMap;
    use std::path::PathBuf;

    fn summary(path: &str, timestamp: u64, backup: Option<&str>) -> CatalogEntrySummary {
        CatalogEntrySummary {
            entry_id: format!("id-{path}"),
            path: path.to_string(),
            exists_in_latest_indexed_snapshot: backup.is_some(),
            latest_restorable_backup: backup.map(ToString::to_string),
            newest_revision_timestamp: timestamp,
            revision_count: 1,
        }
    }

    #[test]
    fn rejects_empty_queries() {
        assert!(parse_search_values("   ", None, None, 100).is_err());
        assert!(parse_search_values("---", None, None, 100).is_err());
    }

    #[test]
    fn tokenizes_queries_case_insensitively_with_and_semantics() {
        let request = parse_search_values("Tax 2021.PDF", None, None, 100).unwrap();
        assert_eq!(request.tokens, vec!["2021", "pdf", "tax"]);
    }

    #[test]
    fn validates_and_normalizes_path_and_extension_filters() {
        let request =
            parse_search_values("invoice", Some("./Downloads\\Invoices"), Some("PDF"), 10).unwrap();
        assert_eq!(request.path_prefix.as_deref(), Some("Downloads/Invoices"));
        assert_eq!(request.extension.as_deref(), Some("pdf"));
        assert!(parse_search_values("invoice", Some("../downloads"), None, 10).is_err());
        assert!(parse_search_values("invoice", None, Some(".pdf"), 10).is_err());
        assert!(parse_search_values("invoice", None, Some("docs/pdf"), 10).is_err());
    }

    #[test]
    fn filters_sorts_and_truncates_deterministically() {
        let request = parse_search_values("invoice", Some("downloads"), Some("pdf"), 2).unwrap();
        let (results, truncated) = filter_sort_and_limit(
            vec![
                summary("downloads/invoices/old.pdf", 10, Some("old-backup")),
                summary("downloads/invoices/new.pdf", 30, Some("new-backup")),
                summary("downloads/invoices/tie.pdf", 30, Some("tie-backup")),
                summary("downloads/invoices/no-backup.pdf", 40, None),
                summary("other/invoice.pdf", 50, Some("other-backup")),
                summary("downloads/invoices/readme.txt", 60, Some("text-backup")),
            ],
            &request,
        );

        assert!(truncated);
        assert_eq!(
            results
                .iter()
                .map(|result| result.path.as_str())
                .collect::<Vec<_>>(),
            vec!["downloads/invoices/new.pdf", "downloads/invoices/tie.pdf"]
        );
        assert!(results.iter().all(|result| {
            result.restore_command.contains(&result.last_backup)
                && result.restore_command.contains(&result.path)
        }));
    }

    #[test]
    fn matches_path_prefixes_and_extensions_case_insensitively() {
        assert!(matches_path_prefix(
            "Downloads/Invoices/tax.PDF",
            Some("downloads")
        ));
        assert!(!matches_path_prefix(
            "Downloads/Invoice-old/tax.PDF",
            Some("downloads/invoices")
        ));
        assert!(matches_extension("tax.PDF", Some("pdf")));
        assert!(matches_extension("tax.final.PDF", Some("pdf")));
        assert!(!matches_extension(".pdf", Some("pdf")));
    }

    #[test]
    fn builds_shell_safe_restore_guidance() {
        assert_eq!(
            build_restore_command("abcdef12", "downloads/invoice.pdf"),
            "gib restore --backup abcdef12 --only downloads/invoice.pdf"
        );
        assert_eq!(
            build_restore_command("abcdef12", "downloads/my invoice.pdf"),
            "gib restore --backup abcdef12 --only 'downloads/my invoice.pdf'"
        );
    }

    #[test]
    fn response_serialization_keeps_result_objects_minimal() {
        let response = SearchResponse {
            query: "invoice".to_string(),
            index_status: SearchIndexStatus::Ready,
            results: vec![SearchResult {
                path: "invoice.pdf".to_string(),
                last_backup: "abcdef12".to_string(),
                restore_command: "gib restore --backup abcdef12 --only invoice.pdf".to_string(),
            }],
            truncated: false,
            warning: None,
        };
        let value = serde_json::to_value(response).unwrap();
        assert_eq!(value["index_status"], "ready");
        assert!(value["results"][0].get("revision_count").is_none());
        assert!(
            value["results"][0]
                .get("newest_revision_timestamp")
                .is_none()
        );
    }

    fn test_directory(label: &str) -> PathBuf {
        let suffix = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let directory = std::env::temp_dir().join(format!("gib-search-{label}-{suffix}"));
        std::fs::create_dir_all(&directory).unwrap();
        directory
    }

    fn backup(
        hash: &str,
        timestamp: u64,
        tree: HashMap<String, BackupObject>,
        parents: Vec<String>,
    ) -> Backup {
        Backup {
            message: hash.to_string(),
            hash: hash.to_string(),
            timestamp,
            author: "search-test".to_string(),
            parents,
            tree,
        }
    }

    fn object(hash: &str, chunk: &str) -> BackupObject {
        BackupObject {
            hash: hash.to_string(),
            size: 1,
            content_type: "application/octet-stream".to_string(),
            permissions: 0o644,
            chunks: vec![chunk.to_string()],
        }
    }

    async fn write_manifest(fs: &Arc<dyn FS>, key: &str, backup: &Backup, password: Option<&str>) {
        let bytes = rmp_serde::to_vec_named(backup).unwrap();
        let compressed = compress_bytes(&bytes, 3);
        let encoded = encode_file_bytes(&compressed, password).unwrap();
        fs.write_file(&format!("{key}/backups/{}", backup.hash), &encoded)
            .await
            .unwrap();
    }

    async fn index_backup(
        fs: &Arc<dyn FS>,
        key: &str,
        backup: &Backup,
        password: Option<&str>,
        parent: Option<&str>,
    ) {
        index_backup_after_finalize(
            Arc::clone(fs),
            key.to_string(),
            password.map(ToString::to_string),
            3,
            backup,
            parent,
            None,
        )
        .await
        .unwrap();
    }

    fn request(query: &str) -> SearchRequest {
        parse_search_values(query, None, None, DEFAULT_SEARCH_LIMIT).unwrap()
    }

    #[tokio::test]
    async fn missing_catalog_is_a_successful_empty_search() {
        let directory = test_directory("missing-catalog");
        let fs: Arc<dyn FS> = Arc::new(LocalFS::new(&directory));

        let response = run_search(fs, "project".to_string(), None, request("invoice"))
            .await
            .unwrap();

        assert_eq!(response.index_status, SearchIndexStatus::NoIndexedBackups);
        assert!(response.results.is_empty());
        assert!(!response.truncated);
        assert!(response.warning.is_none());

        let _ = std::fs::remove_dir_all(directory);
    }

    #[tokio::test]
    async fn catalog_without_indexed_snapshots_is_not_treated_as_an_error() {
        let directory = test_directory("no-index");
        let fs: Arc<dyn FS> = Arc::new(LocalFS::new(&directory));
        mark_catalog_degraded_state(&fs, "project", None, 3)
            .await
            .unwrap();

        let response = run_search(fs, "project".to_string(), None, request("invoice"))
            .await
            .unwrap();

        assert_eq!(response.index_status, SearchIndexStatus::NoIndexedBackups);
        assert!(response.results.is_empty());
        assert!(response.warning.is_none());

        let _ = std::fs::remove_dir_all(directory);
    }

    #[tokio::test]
    async fn searches_current_and_deleted_paths_from_catalog_metadata() {
        let directory = test_directory("history");
        let fs: Arc<dyn FS> = Arc::new(LocalFS::new(&directory));
        let key = "project";
        let first = backup(
            "backup-one",
            1,
            HashMap::from([
                (
                    "docs/invoice-2021.pdf".to_string(),
                    object("invoice-before", "chunk-invoice-before"),
                ),
                (
                    "docs/old-invoice.pdf".to_string(),
                    object("old-invoice", "chunk-old-invoice"),
                ),
            ]),
            Vec::new(),
        );
        let second = backup(
            "backup-two",
            2,
            HashMap::from([(
                "docs/invoice-2021.pdf".to_string(),
                object("invoice-after", "chunk-invoice-after"),
            )]),
            vec![first.hash.clone()],
        );
        write_manifest(&fs, key, &first, None).await;
        write_manifest(&fs, key, &second, None).await;
        index_backup(&fs, key, &first, None, None).await;
        index_backup(&fs, key, &second, None, Some(&first.hash)).await;

        let response = run_search(Arc::clone(&fs), key.to_string(), None, request("INVOICE"))
            .await
            .unwrap();

        assert_eq!(response.index_status, SearchIndexStatus::Ready);
        assert_eq!(
            response
                .results
                .iter()
                .map(|result| result.path.as_str())
                .collect::<Vec<_>>(),
            vec!["docs/invoice-2021.pdf", "docs/old-invoice.pdf"]
        );
        assert_eq!(response.results[0].last_backup, "backup-t");
        assert_eq!(response.results[1].last_backup, "backup-o");
        assert!(
            response.results[1]
                .restore_command
                .contains("--only docs/old-invoice.pdf")
        );

        let _ = std::fs::remove_dir_all(directory);
    }

    #[tokio::test]
    async fn deleting_an_old_backup_keeps_the_path_with_a_restorable_revision() {
        let directory = test_directory("delete-old");
        let fs: Arc<dyn FS> = Arc::new(LocalFS::new(&directory));
        let key = "project";
        let first = backup(
            "backup-one",
            1,
            HashMap::from([(
                "docs/invoice.pdf".to_string(),
                object("same-content", "chunk-invoice"),
            )]),
            Vec::new(),
        );
        let second = backup(
            "backup-two",
            2,
            HashMap::from([(
                "docs/invoice.pdf".to_string(),
                object("same-content", "chunk-invoice"),
            )]),
            vec![first.hash.clone()],
        );
        write_manifest(&fs, key, &first, None).await;
        write_manifest(&fs, key, &second, None).await;
        index_backup(&fs, key, &first, None, None).await;
        index_backup(&fs, key, &second, None, Some(&first.hash)).await;

        let remaining_summaries = vec![BackupSummary {
            message: second.message.clone(),
            hash: second.hash.clone(),
            timestamp: Some(second.timestamp),
            size: Some(1),
        }];
        let chunk_indexes =
            HashMap::from([("chunk-invoice".to_string(), ChunkIndex { refcount: 1 })]);
        remove_backup_from_catalog(
            Arc::clone(&fs),
            key.to_string(),
            None,
            3,
            &first,
            &remaining_summaries,
            &chunk_indexes,
        )
        .await
        .unwrap();

        let response = run_search(Arc::clone(&fs), key.to_string(), None, request("invoice"))
            .await
            .unwrap();
        assert_eq!(response.results.len(), 1);
        assert_eq!(response.results[0].last_backup, "backup-t");
        assert!(
            response.results[0]
                .restore_command
                .contains("gib restore --backup backup-t --only docs/invoice.pdf")
        );

        let _ = std::fs::remove_dir_all(directory);
    }

    #[tokio::test]
    async fn degraded_catalog_returns_results_with_a_warning() {
        let directory = test_directory("degraded");
        let fs: Arc<dyn FS> = Arc::new(LocalFS::new(&directory));
        let key = "project";
        let first = backup(
            "backup-one",
            1,
            HashMap::from([(
                "invoice.pdf".to_string(),
                object("invoice", "chunk-invoice"),
            )]),
            Vec::new(),
        );
        index_backup(&fs, key, &first, None, None).await;
        mark_catalog_degraded_state(&fs, key, None, 3)
            .await
            .unwrap();

        let response = run_search(Arc::clone(&fs), key.to_string(), None, request("invoice"))
            .await
            .unwrap();
        assert_eq!(response.index_status, SearchIndexStatus::Degraded);
        assert_eq!(response.results.len(), 1);
        assert!(response.warning.is_some());

        let _ = std::fs::remove_dir_all(directory);
    }

    #[tokio::test]
    async fn encrypted_catalog_search_uses_the_repository_password() {
        let directory = test_directory("encrypted");
        let fs: Arc<dyn FS> = Arc::new(LocalFS::new(&directory));
        let key = "project";
        let password = "search-secret";
        let first = backup(
            "backup-one",
            1,
            HashMap::from([(
                "private/invoice.pdf".to_string(),
                object("invoice", "chunk-invoice"),
            )]),
            Vec::new(),
        );
        write_manifest(&fs, key, &first, Some(password)).await;
        index_backup(&fs, key, &first, Some(password), None).await;

        let response = run_search(
            Arc::clone(&fs),
            key.to_string(),
            Some(password.to_string()),
            request("invoice"),
        )
        .await
        .unwrap();
        assert_eq!(response.results.len(), 1);
        assert!(
            run_search(
                fs,
                key.to_string(),
                Some("wrong-password".to_string()),
                request("invoice")
            )
            .await
            .is_err()
        );

        let _ = std::fs::remove_dir_all(directory);
    }
}
