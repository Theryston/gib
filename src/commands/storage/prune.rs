use std::sync::{Arc, Mutex};

use crate::core::crypto::get_password;
use crate::core::indexes::load_chunk_indexes;
use crate::output::{JsonProgress, emit_output, emit_progress_message, is_json_mode};
use crate::utils::{get_fs, get_pwd_string, get_storage, handle_error};
use clap::ArgMatches;
use dialoguer::Select;
use dirs::home_dir;
use futures::stream::{self, StreamExt};
use indicatif::{ProgressBar, ProgressStyle};
use std::path::Path;
use std::time::{Duration, Instant};
use tokio::sync::{Mutex as TokioMutex, Semaphore};
use tokio::task::JoinSet;

const MAX_CONCURRENT_CHUNKS: usize = 100;

pub async fn prune(matches: &ArgMatches) {
    let (key, storage, password) = match get_params(matches) {
        Ok(params) => params,
        Err(e) => handle_error(e, None),
    };

    let started_at = Instant::now();
    let auto_confirm = matches.get_flag("yes");

    let storage = get_storage(&storage);

    let fs = get_fs(&storage, None);

    let pb = if is_json_mode() {
        ProgressBar::hidden()
    } else {
        let pb = ProgressBar::new(100);
        pb.enable_steady_tick(Duration::from_millis(100));
        pb.set_style(ProgressStyle::with_template("{spinner:.green} {msg}").unwrap());
        pb.set_message("Loading chunk indexes...");
        pb
    };

    if is_json_mode() {
        emit_progress_message("Loading chunk indexes...");
    }

    let chunk_indexes = match load_chunk_indexes(
        Arc::clone(&fs),
        key.clone(),
        password.clone(),
        Arc::new(Mutex::new(false)),
    )
    .await
    {
        Ok(chunk_indexes) => chunk_indexes,
        Err(e) => handle_error(e, Some(&pb)),
    };

    pb.set_message("Loading all chunks in the repository...");
    if is_json_mode() {
        emit_progress_message("Loading all chunks in the repository...");
    }

    let chunks_folder = format!("{}/chunks", key);
    let indexes_folder = format!("{}/indexes", key);

    let chunks = match fs.list_files(&chunks_folder).await {
        Ok(chunks) => chunks,
        Err(e) => handle_error(e.to_string(), Some(&pb)),
    };

    let pending_backups = match fs.list_files(&indexes_folder).await {
        Ok(indexes) => indexes
            .iter()
            .filter(|index| {
                let last_part: &str = index.split('/').last().unwrap_or(&"");

                last_part.starts_with("pending_")
            })
            .cloned()
            .collect::<Vec<String>>(),
        Err(e) => handle_error(e.to_string(), Some(&pb)),
    };

    let chunks_to_prune = {
        let mut chunks_to_prune = chunks
            .iter()
            .filter(|chunk| {
                let parts: Vec<&str> = chunk.split('/').collect();
                let key = if parts.len() >= 2 {
                    format!("{}{}", parts[parts.len() - 2], parts[parts.len() - 1])
                } else {
                    chunk.to_string()
                };

                !chunk_indexes.contains_key(&key)
            })
            .cloned()
            .collect::<Vec<String>>();

        chunks_to_prune.extend(pending_backups);

        chunks_to_prune
    };

    pb.finish_and_clear();

    if chunks_to_prune.is_empty() {
        if is_json_mode() {
            #[derive(serde::Serialize)]
            struct PruneOutput {
                deleted_chunks: usize,
                elapsed_ms: u64,
            }

            let payload = PruneOutput {
                deleted_chunks: 0,
                elapsed_ms: started_at.elapsed().as_millis() as u64,
            };
            emit_output(&payload);
        } else {
            println!("No chunks to prune");
        }
        return;
    }

    if is_json_mode() && !auto_confirm {
        handle_error(
            "Confirmation required in --mode json. Re-run with --yes to delete unused chunks."
                .to_string(),
            None,
        );
    }

    let confirm = if auto_confirm {
        true
    } else {
        dialoguer::Confirm::new()
            .with_prompt(format!(
                "Seams like you have {} chunks that are not used in the repository. Are you sure you want to DELETE them?",
                chunks_to_prune.len()
            ))
            .interact()
            .unwrap_or_else(|e| handle_error(format!("Error: {}", e), None))
    };

    if !confirm {
        if is_json_mode() {
            #[derive(serde::Serialize)]
            struct PruneOutput {
                deleted_chunks: usize,
                aborted: bool,
            }

            let payload = PruneOutput {
                deleted_chunks: 0,
                aborted: true,
            };
            emit_output(&payload);
        } else {
            println!("Aborting...");
        }
        return;
    }

    let json_progress = if is_json_mode() {
        let progress = JsonProgress::new(chunks_to_prune.len() as u64);
        progress.set_message("Deleting chunks...");
        Some(progress)
    } else {
        None
    };

    let pb = if is_json_mode() {
        ProgressBar::hidden()
    } else {
        let pb = ProgressBar::new(chunks_to_prune.len() as u64);
        pb.enable_steady_tick(Duration::from_millis(100));
        pb.set_style(
            ProgressStyle::with_template(
                "[{elapsed_precise}] [{bar:40.cyan/blue}] {pos}/{len} {msg}",
            )
            .unwrap(),
        );
        pb.set_message("Deleting chunks...");
        pb
    };

    let chunks_set = Arc::new(TokioMutex::new(JoinSet::new()));
    let semaphore = Arc::new(Semaphore::new(MAX_CONCURRENT_CHUNKS));

    let chunks_stream = stream::iter(&chunks_to_prune);

    chunks_stream
        .for_each_concurrent(MAX_CONCURRENT_CHUNKS, |chunk| {
            let pb_clone = pb.clone();
            let fs_clone = Arc::clone(&fs);
            let chunk_clone = chunk.clone();
            let semaphore_clone = Arc::clone(&semaphore);
            let chunks_set_clone = Arc::clone(&chunks_set);
            let json_progress_clone = json_progress.clone();

            async move {
                let mut guard = chunks_set_clone.lock().await;
                guard.spawn(async move {
                    let _permit = semaphore_clone.acquire().await.expect("Semaphore closed");
                    let _ = fs_clone.delete_file(&chunk_clone).await;
                    if let Some(progress) = &json_progress_clone {
                        progress.inc_by(1);
                    } else {
                        pb_clone.inc(1);
                    }
                    Ok(())
                });
            }
        })
        .await;

    let mut failed_chunks = Vec::new();

    {
        let mut guard = chunks_set.lock().await;
        while let Some(file_process_result) = guard.join_next().await {
            match file_process_result {
                Ok(Ok(_)) => {}
                Ok(Err(e)) => failed_chunks.push(e),
                Err(e) => failed_chunks.push(e.to_string()),
            }
        }
    }

    if !failed_chunks.is_empty() {
        handle_error(
            format!(
                "Failed to process {} files:\n{}",
                failed_chunks.len(),
                failed_chunks
                    .iter()
                    .map(|f| format!("  - {}", f))
                    .collect::<Vec<String>>()
                    .join("\n")
            ),
            Some(&pb),
        );
    }

    if is_json_mode() {
        #[derive(serde::Serialize)]
        struct PruneOutput {
            deleted_chunks: usize,
            elapsed_ms: u64,
        }

        let payload = PruneOutput {
            deleted_chunks: chunks_to_prune.len(),
            elapsed_ms: started_at.elapsed().as_millis() as u64,
        };
        emit_output(&payload);
    } else {
        let elapsed = pb.elapsed();
        pb.set_style(ProgressStyle::with_template("{prefix:.green} {msg}").unwrap());
        pb.set_prefix("OK");
        pb.finish_with_message(format!(
            "Deleted {} chunks ({:.2?})",
            chunks_to_prune.len(),
            elapsed,
        ));
    }
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
        return Err("Seams like you didn't create any storage yet. Run 'gib storage add' to create a storage.".to_string());
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
        return Err("Seams like you didn't create any storage yet. Run 'gib storage add' to create a storage.".to_string());
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
