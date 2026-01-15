use std::sync::{Arc, Mutex};

use crate::core::crypto::get_password;
use crate::core::indexes::load_chunk_indexes;
use crate::utils::{get_fs, get_pwd_string, get_storage, handle_error};
use clap::ArgMatches;
use dialoguer::Select;
use dirs::home_dir;
use futures::stream::{self, StreamExt};
use indicatif::{ProgressBar, ProgressStyle};
use std::path::Path;
use std::time::Duration;
use tokio::sync::{Mutex as TokioMutex, Semaphore};
use tokio::task::JoinSet;

const MAX_CONCURRENT_CHUNKS: usize = 100;

pub async fn prune(matches: &ArgMatches) {
    let (key, storage, password) = match get_params(matches) {
        Ok(params) => params,
        Err(e) => handle_error(e, None),
    };

    let storage = get_storage(&storage);

    let fs = get_fs(&storage, None);

    let pb = ProgressBar::new(100);
    pb.enable_steady_tick(Duration::from_millis(100));
    pb.set_style(ProgressStyle::with_template("{spinner:.green} {msg}").unwrap());

    pb.set_message(format!("Loading chunk indexes..."));

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

    pb.set_message(format!("Loading all chunks in the repository..."));

    let chunks_folder = format!("{}/chunks", key);

    let chunks = match fs.list_files(&chunks_folder).await {
        Ok(chunks) => chunks,
        Err(e) => handle_error(e.to_string(), Some(&pb)),
    };

    let chunks_to_prune = chunks
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

    pb.finish_and_clear();

    if chunks_to_prune.is_empty() {
        println!("No chunks to prune");
        std::process::exit(0);
    }

    let confirm = dialoguer::Confirm::new()
        .with_prompt(format!("Seams like you have {} chunks that are not used in the repository. Are you sure you want to DELETE them?", chunks_to_prune.len()))
        .interact()
        .unwrap_or_else(|e| {
            eprintln!("Error: {}", e);
            std::process::exit(1);
        });

    if !confirm {
        println!("Aborting...");
        std::process::exit(0);
    }

    let pb = ProgressBar::new(chunks_to_prune.len() as u64);
    pb.enable_steady_tick(Duration::from_millis(100));

    pb.set_style(
        ProgressStyle::with_template("[{elapsed_precise}] [{bar:40.cyan/blue}] {pos}/{len} {msg}")
            .unwrap(),
    );

    pb.set_message("Deleting chunks...");

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

            async move {
                let _permit = semaphore_clone.acquire().await.expect("Semaphore closed");

                let mut guard = chunks_set_clone.lock().await;
                guard.spawn(async move {
                    let _ = fs_clone.delete_file(&chunk_clone).await;
                    pb_clone.inc(1);
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

    let elapsed = pb.elapsed();
    pb.set_style(ProgressStyle::with_template("{prefix:.green} {msg}").unwrap());
    pb.set_prefix("✓");
    pb.finish_with_message(format!(
        "Deleted {} chunks ({:.2?})",
        chunks_to_prune.len(),
        elapsed,
    ));
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

    let files = std::fs::read_dir(&storage_path).unwrap();

    let storages_names = &files
        .map(|file| {
            file.unwrap()
                .file_name()
                .to_string_lossy()
                .to_string()
                .split('.')
                .next()
                .unwrap()
                .to_string()
        })
        .collect::<Vec<String>>();

    if storages_names.is_empty() {
        return Err("Seams like you didn't create any storage yet. Run 'gib storage add' to create a storage.".to_string());
    }

    let storage = match matches.get_one::<String>("storage") {
        Some(storage) => storage.to_string(),
        None => {
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
