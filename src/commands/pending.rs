use crate::core::crypto::{get_password, read_file_maybe_decrypt};
use crate::core::metadata::PendingBackup;
use crate::output::{emit_output, emit_progress_message, is_json_mode};
use crate::utils::{
    decompress_bytes, get_pwd_string, get_storage, get_storage_client, handle_error,
};
use bytesize::ByteSize;
use clap::ArgMatches;
use console::{Term, style};
use crossterm::event::{self, Event, KeyCode, KeyEvent, KeyEventKind};
use crossterm::execute;
use crossterm::terminal::{Clear, ClearType, disable_raw_mode, enable_raw_mode};
use dialoguer::Select;
use dirs::home_dir;
use futures::stream::{self, StreamExt};
use indicatif::{ProgressBar, ProgressStyle};
use std::io;
use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

const PENDING_PER_PAGE: usize = 10;

#[derive(serde::Serialize, Clone)]
struct PendingBackupEntry {
    backup: String,
    backup_short: String,
    message: String,
    uploaded_chunks: usize,
    chunk_size_bytes: u64,
    compress: i32,
    concurrency: usize,
    ignored_entries: usize,
}

pub async fn pending(matches: &ArgMatches) {
    let (key, storage, password) = match get_params(matches) {
        Ok(params) => params,
        Err(e) => handle_error(e, None),
    };

    let storage = get_storage(&storage);
    let storage_client = get_storage_client(&storage, None);

    let pending_paths = match list_pending_backup_paths(Arc::clone(&storage_client), &key).await {
        Ok(paths) => paths,
        Err(e) => handle_error(e, None),
    };

    if pending_paths.is_empty() {
        if is_json_mode() {
            let empty: Vec<PendingBackupEntry> = Vec::new();
            emit_output(&empty);
        } else {
            println!(
                "{}",
                style("No pending backups found for this repository.").yellow()
            );
        }
        return;
    }

    let pb = if is_json_mode() {
        ProgressBar::hidden()
    } else {
        let pb = ProgressBar::new_spinner();
        pb.enable_steady_tick(Duration::from_millis(100));
        pb.set_style(ProgressStyle::with_template("{spinner:.green} {msg}").unwrap());
        pb.set_message("Loading pending backups...");
        pb
    };

    if is_json_mode() {
        emit_progress_message("Loading pending backups...");
    }

    let concurrency = num_cpus::get() * 2;
    let mut entries = Vec::with_capacity(pending_paths.len());
    let mut errors = Vec::new();

    let mut stream = stream::iter(pending_paths.into_iter().map(|path| {
        let storage_client = Arc::clone(&storage_client);
        let password = password.clone();
        async move { load_pending_backup_entry(storage_client, path, password).await }
    }))
    .buffer_unordered(concurrency);

    while let Some(result) = stream.next().await {
        match result {
            Ok(entry) => entries.push(entry),
            Err(e) => errors.push(e),
        }
    }

    pb.finish_and_clear();

    if !errors.is_empty() {
        handle_error(
            format!(
                "Failed to load {} pending backups:\n{}",
                errors.len(),
                errors
                    .iter()
                    .map(|e| format!("  - {}", e))
                    .collect::<Vec<String>>()
                    .join("\n")
            ),
            None,
        );
    }

    entries.sort_by(|a, b| a.backup.cmp(&b.backup));

    if is_json_mode() {
        emit_output(&entries);
    } else {
        display_paginated_pending_backups(&entries);
    }
}

async fn list_pending_backup_paths(
    storage_client: Arc<dyn crate::storage_clients::ClientStorage>,
    key: &str,
) -> Result<Vec<String>, String> {
    let indexes_path = format!("{}/indexes", key);
    let files = storage_client
        .list_files(&indexes_path)
        .await
        .map_err(|e| format!("Failed to list indexes in '{}': {}", indexes_path, e))?;

    let pending_prefix = format!("{}/indexes/pending_", key);
    let mut matches: Vec<String> = files
        .into_iter()
        .filter(|path| path.starts_with(&pending_prefix))
        .collect();

    matches.sort();
    matches.dedup();
    Ok(matches)
}

async fn load_pending_backup_entry(
    storage_client: Arc<dyn crate::storage_clients::ClientStorage>,
    pending_path: String,
    password: Option<String>,
) -> Result<PendingBackupEntry, String> {
    let pending_result = read_file_maybe_decrypt(
        &storage_client,
        &pending_path,
        password.as_deref(),
        "Pending backup is encrypted but no password provided",
    )
    .await?;

    if pending_result.bytes.is_empty() {
        return Err(format!("Pending backup '{}' is empty", pending_path));
    }

    let decompressed_bytes = decompress_bytes(&pending_result.bytes);

    let pending_backup: PendingBackup =
        rmp_serde::from_slice(&decompressed_bytes).map_err(|e| {
            format!(
                "Failed to deserialize pending backup '{}': {}",
                pending_path, e
            )
        })?;

    let backup_hash = extract_pending_hash(&pending_path)?;
    let backup_short = backup_hash[..8.min(backup_hash.len())].to_string();

    Ok(PendingBackupEntry {
        backup: backup_hash,
        backup_short,
        message: pending_backup.message,
        uploaded_chunks: pending_backup.processed_chunks.len(),
        chunk_size_bytes: pending_backup.chunk_size,
        compress: pending_backup.compress,
        concurrency: pending_backup.concurrency,
        ignored_entries: pending_backup.ignore_patterns.len(),
    })
}

fn extract_pending_hash(path: &str) -> Result<String, String> {
    let file_name = path
        .rsplit('/')
        .next()
        .ok_or_else(|| format!("Failed to parse pending backup name from '{}'", path))?;
    let hash = file_name.strip_prefix("pending_").unwrap_or(file_name);

    if hash.is_empty() {
        return Err(format!("Pending backup name is empty for '{}'", path));
    }

    Ok(hash.to_string())
}

fn display_paginated_pending_backups(entries: &[PendingBackupEntry]) {
    let total_backups = entries.len();
    let total_pages = (total_backups + PENDING_PER_PAGE - 1) / PENDING_PER_PAGE;
    let mut current_page = 0;

    let term = Term::stdout();
    enable_raw_mode().unwrap_or(());

    loop {
        execute!(io::stdout(), Clear(ClearType::All)).unwrap_or(());
        term.clear_screen().unwrap_or(());

        let start_idx = current_page * PENDING_PER_PAGE;
        let end_idx = (start_idx + PENDING_PER_PAGE).min(total_backups);
        let page_backups = &entries[start_idx..end_idx];

        for (idx, backup) in page_backups.iter().enumerate() {
            let hash_short = &backup.backup_short;
            print!("\r");

            let mut parts = vec![
                style(format!("{}", hash_short)).cyan().bold(),
                style(backup.message.clone()).white(),
            ];

            let details = format!(
                "Uploaded chunks: {} | Chunk size: {} | Compress: {} | Concurrency: {} | Ignored: {}",
                backup.uploaded_chunks,
                ByteSize(backup.chunk_size_bytes),
                backup.compress,
                backup.concurrency,
                backup.ignored_entries
            );
            parts.push(style(format!("\r\n{}", details)).dim());

            let line = parts
                .iter()
                .map(|p| p.to_string())
                .collect::<Vec<String>>()
                .join(" ");
            println!("{}", line);

            if idx < page_backups.len() - 1 {
                println!();
            }
        }

        println!();
        print!("\r");
        println!(
            "{}",
            style(format!(
                "Page {}/{} ({} pending) | Press 'n' for next, 'p' for previous, 'q' to quit",
                current_page + 1,
                total_pages,
                total_backups
            ))
            .dim()
        );

        match event::read() {
            Ok(Event::Key(KeyEvent {
                code,
                kind: KeyEventKind::Press,
                ..
            })) => match code {
                KeyCode::Char('q') | KeyCode::Esc => {
                    break;
                }
                KeyCode::Char('n') | KeyCode::Right | KeyCode::Char(' ') => {
                    if current_page < total_pages - 1 {
                        current_page += 1;
                    }
                }
                KeyCode::Char('p') | KeyCode::Left => {
                    if current_page > 0 {
                        current_page -= 1;
                    }
                }
                KeyCode::Home => {
                    current_page = 0;
                }
                KeyCode::End => {
                    current_page = total_pages - 1;
                }
                _ => {}
            },
            Ok(Event::Resize(_, _)) => {}
            Err(_) => break,
            _ => {}
        }
    }

    disable_raw_mode().unwrap_or(());
    term.clear_screen().unwrap_or(());
}

fn get_params(matches: &ArgMatches) -> Result<(String, String, Option<String>), String> {
    let password: Option<String> = matches
        .get_one::<String>("password")
        .map(|s| s.to_string())
        .map_or_else(
            || get_password(false, true),
            |password| Some(password.to_string()),
        );

    let pwd_string = get_pwd_string();

    let default_key = Path::new(&pwd_string)
        .file_name()
        .unwrap()
        .to_string_lossy()
        .to_string();

    let key = matches
        .get_one::<String>("key")
        .map_or_else(|| default_key, |key| key.to_string());

    let home_dir = home_dir().unwrap();
    let storage_path = home_dir.join(".gib").join("storages");

    if !storage_path.exists() {
        return Err("Seems like you didn't create any storage yet. Run 'gib storage add' to create a storage.".to_string());
    }

    let files =
        std::fs::read_dir(&storage_path).map_err(|e| format!("Failed to read storages: {}", e))?;

    let storages_names = &files
        .map(|file| {
            file.map_err(|e| format!("Failed to read storage entry: {}", e))
                .map(|file| {
                    file.file_name()
                        .to_string_lossy()
                        .to_string()
                        .split('.')
                        .next()
                        .unwrap()
                        .to_string()
                })
        })
        .collect::<Result<Vec<String>, String>>()?;

    if storages_names.is_empty() {
        return Err("Seems like you didn't create any storage yet. Run 'gib storage add' to create a storage.".to_string());
    }

    let storage = match matches.get_one::<String>("storage") {
        Some(storage) => storage.to_string(),
        None => {
            if is_json_mode() {
                return Err(
                    "Missing required argument: --storage (required in --mode json)".to_string(),
                );
            }
            let selected_index = Select::new()
                .with_prompt("Select the storage to use")
                .items(storages_names)
                .default(0)
                .interact()
                .map_err(|e| format!("{}", e))?;

            storages_names[selected_index].clone()
        }
    };

    let exists = storages_names
        .iter()
        .any(|storage_name| storage_name == &storage);

    if !exists {
        return Err(format!("Storage '{}' not found", storage));
    }

    Ok((key, storage, password))
}
