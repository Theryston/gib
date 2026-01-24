use crate::core::crypto::get_password;
use crate::core::crypto::read_file_maybe_decrypt;
use crate::core::crypto::write_file_maybe_encrypt;
use crate::core::indexes::{list_backup_summaries, load_chunk_indexes};
use crate::core::metadata::Backup;
use crate::fs::FS;
use crate::utils::{
    compress_bytes, decompress_bytes, get_fs, get_pwd_string, get_storage, handle_error,
};
use clap::ArgMatches;
use dialoguer::Select;
use dirs::home_dir;
use futures::stream::{self, StreamExt};
use indicatif::{ProgressBar, ProgressStyle};
use std::path::Path;
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tokio::sync::{Mutex as TokioMutex, Semaphore};
use tokio::task::JoinSet;

const MAX_CONCURRENT_CHUNKS: usize = 100;

pub async fn delete(matches: &ArgMatches) {
    let (key, storage, password, backup_hash) = match get_params(matches) {
        Ok(params) => params,
        Err(e) => handle_error(e, None),
    };

    let storage = get_storage(&storage);

    let fs = get_fs(&storage, None);

    let full_backup_hash = match resolve_backup_hash(
        Arc::clone(&fs),
        key.clone(),
        password.clone(),
        backup_hash,
    )
    .await
    {
        Ok(hash) => hash,
        Err(e) => handle_error(e, None),
    };

    let pb = ProgressBar::new(100);
    pb.enable_steady_tick(Duration::from_millis(100));
    pb.set_style(ProgressStyle::with_template("{spinner:.green} {msg}").unwrap());

    pb.set_message("Loading backup data and indexes...");

    let full_backup_hash_clone = full_backup_hash.clone();
    let backup_future = tokio::spawn(load_backup(
        Arc::clone(&fs),
        key.clone(),
        password.clone(),
        full_backup_hash_clone,
    ));

    let chunk_indexes_future = tokio::spawn(load_chunk_indexes(
        Arc::clone(&fs),
        key.clone(),
        password.clone(),
        Arc::new(Mutex::new(false)),
    ));

    let backup_summaries_future = tokio::spawn(list_backup_summaries(
        Arc::clone(&fs),
        key.clone(),
        password.clone(),
    ));

    let (backup_result, chunk_indexes_result, backup_summaries_result) =
        tokio::join!(backup_future, chunk_indexes_future, backup_summaries_future);

    let backup = match backup_result {
        Ok(Ok(backup)) => backup,
        Ok(Err(e)) => handle_error(format!("Failed to load backup: {}", e), Some(&pb)),
        Err(e) => handle_error(format!("Failed to load backup: {}", e), Some(&pb)),
    };

    let mut chunk_indexes = match chunk_indexes_result {
        Ok(Ok(indexes)) => indexes,
        Ok(Err(e)) => handle_error(format!("Failed to load chunk indexes: {}", e), Some(&pb)),
        Err(e) => handle_error(format!("Failed to load chunk indexes: {}", e), Some(&pb)),
    };

    let mut backup_summaries = match backup_summaries_result {
        Ok(Ok(summaries)) => summaries,
        Ok(Err(e)) => handle_error(format!("Failed to load backup summaries: {}", e), Some(&pb)),
        Err(e) => handle_error(format!("Failed to load backup summaries: {}", e), Some(&pb)),
    };

    pb.set_message("Processing chunks...");

    backup_summaries.retain(|summary| summary.hash != full_backup_hash);

    let chunks_to_delete = Arc::new(Mutex::new(Vec::<String>::new()));

    for (_relative_path, backup_object) in backup.tree.iter() {
        for chunk_hash in &backup_object.chunks {
            if let Some(chunk_index) = chunk_indexes.get_mut(chunk_hash) {
                if chunk_index.refcount > 0 {
                    chunk_index.refcount -= 1;

                    if chunk_index.refcount == 0 {
                        chunks_to_delete.lock().unwrap().push(chunk_hash.clone());
                    }
                }
            }
        }
    }

    let chunks_to_delete_vec = chunks_to_delete.lock().unwrap().clone();
    for chunk_hash in &chunks_to_delete_vec {
        chunk_indexes.remove(chunk_hash);
    }

    pb.set_message("Writing updated indexes...");

    let chunk_indexes_bytes = match rmp_serde::to_vec_named(&chunk_indexes) {
        Ok(bytes) => bytes,
        Err(e) => handle_error(
            format!("Failed to serialize chunk indexes: {}", e),
            Some(&pb),
        ),
    };
    let compressed_chunk_indexes_bytes = compress_bytes(&chunk_indexes_bytes, 3);

    let chunk_index_path = format!("{}/indexes/chunks", key);
    let write_chunk_index_future = write_file_maybe_encrypt(
        &fs,
        &chunk_index_path,
        &compressed_chunk_indexes_bytes,
        password.as_deref(),
    );

    let backup_summaries_bytes = match rmp_serde::to_vec_named(&backup_summaries) {
        Ok(bytes) => bytes,
        Err(e) => handle_error(
            format!("Failed to serialize backup summaries: {}", e),
            Some(&pb),
        ),
    };
    let compressed_backup_summaries_bytes = compress_bytes(&backup_summaries_bytes, 3);

    let backup_index_path = format!("{}/indexes/backups", key);
    let write_backup_index_future = write_file_maybe_encrypt(
        &fs,
        &backup_index_path,
        &compressed_backup_summaries_bytes,
        password.as_deref(),
    );

    let (write_chunk_index_result, write_backup_index_result) =
        tokio::join!(write_chunk_index_future, write_backup_index_future);

    if write_chunk_index_result.is_err() {
        handle_error("Failed to write chunk indexes".to_string(), Some(&pb));
    }

    if write_backup_index_result.is_err() {
        handle_error("Failed to write backup index".to_string(), Some(&pb));
    }

    pb.set_message("Deleting backup file...");

    let backup_file_path = format!("{}/backups/{}", key, full_backup_hash);
    if let Err(e) = fs.delete_file(&backup_file_path).await {
        handle_error(format!("Failed to delete backup file: {}", e), Some(&pb));
    }

    pb.finish_and_clear();

    if !chunks_to_delete_vec.is_empty() {
        let pb = ProgressBar::new(chunks_to_delete_vec.len() as u64);
        pb.enable_steady_tick(Duration::from_millis(100));
        pb.set_style(
            ProgressStyle::with_template(
                "[{elapsed_precise}] [{bar:40.cyan/blue}] {pos}/{len} {msg}",
            )
            .unwrap(),
        );
        pb.set_message("Deleting orphaned chunks...");

        let chunks_set = Arc::new(TokioMutex::new(JoinSet::new()));
        let semaphore = Arc::new(Semaphore::new(MAX_CONCURRENT_CHUNKS));
        let chunks_stream = stream::iter(&chunks_to_delete_vec);

        chunks_stream
            .for_each_concurrent(MAX_CONCURRENT_CHUNKS, |chunk_hash| {
                let pb_clone = pb.clone();
                let fs_clone = Arc::clone(&fs);
                let key_clone = key.clone();
                let chunk_hash_clone = chunk_hash.clone();
                let semaphore_clone = Arc::clone(&semaphore);
                let chunks_set_clone = Arc::clone(&chunks_set);

                async move {
                    let mut guard = chunks_set_clone.lock().await;
                    guard.spawn(async move {
                        let _permit = semaphore_clone.acquire().await.expect("Semaphore closed");
                        let (prefix, rest) = chunk_hash_clone.split_at(2);
                        let chunk_path = format!("{}/chunks/{}/{}", key_clone, prefix, rest);

                        if let Err(e) = fs_clone.delete_file(&chunk_path).await {
                            return Err(format!(
                                "Failed to delete chunk {}: {}",
                                chunk_hash_clone, e
                            ));
                        }

                        pb_clone.inc(1);
                        Ok(())
                    });
                }
            })
            .await;

        let mut failed_chunks = Vec::new();

        {
            let mut guard = chunks_set.lock().await;
            while let Some(chunk_process_result) = guard.join_next().await {
                match chunk_process_result {
                    Ok(Ok(_)) => {}
                    Ok(Err(e)) => failed_chunks.push(e),
                    Err(e) => failed_chunks.push(e.to_string()),
                }
            }
        }

        if !failed_chunks.is_empty() {
            handle_error(
                format!(
                    "Failed to delete {} chunks:\n{}",
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
            chunks_to_delete_vec.len(),
            elapsed
        ));
    }
}

async fn resolve_backup_hash(
    fs: Arc<dyn FS>,
    key: String,
    password: Option<String>,
    provided_hash: Option<String>,
) -> Result<String, String> {
    match provided_hash {
        Some(hash) => {
            if hash.len() <= 8 {
                let summaries = list_backup_summaries(fs, key, password).await?;

                for summary in summaries {
                    if summary.hash.starts_with(&hash) {
                        return Ok(summary.hash);
                    }
                }

                Err(format!("No backup found matching hash prefix: {}", hash))
            } else {
                Ok(hash)
            }
        }
        None => {
            let summaries = list_backup_summaries(fs, key, password).await?;

            if summaries.is_empty() {
                return Err("No backups found in repository".to_string());
            }

            let recent_backups: Vec<BackupSummaryDisplay> = summaries
                .iter()
                .take(10)
                .map(|s| BackupSummaryDisplay {
                    hash: s.hash.clone(),
                    message: s.message.clone(),
                })
                .collect();

            if recent_backups.is_empty() {
                return Err("No backups found in repository".to_string());
            }

            let items: Vec<String> = recent_backups
                .iter()
                .map(|c| format!("{} {}", &c.hash[..8.min(c.hash.len())], &c.message))
                .collect();

            let selected_index = Select::new()
                .with_prompt("Select a backup to delete")
                .items(&items)
                .default(0)
                .interact()
                .map_err(|e| format!("Failed to select backup: {}", e))?;

            Ok(recent_backups[selected_index].hash.clone())
        }
    }
}

struct BackupSummaryDisplay {
    hash: String,
    message: String,
}

async fn load_backup(
    fs: Arc<dyn FS>,
    key: String,
    password: Option<String>,
    backup_hash: String,
) -> Result<Backup, String> {
    let backup_path = format!("{}/backups/{}", key, backup_hash);

    let read_result = read_file_maybe_decrypt(
        &fs,
        &backup_path,
        password.as_deref(),
        "Backup is encrypted but no password provided",
    )
    .await?;

    if read_result.bytes.is_empty() {
        return Err(format!("Backup {} not found or is empty", backup_hash));
    }

    let decompressed_bytes = decompress_bytes(&read_result.bytes);

    let backup: Backup = rmp_serde::from_slice(&decompressed_bytes)
        .map_err(|e| format!("Failed to deserialize backup: {}", e))?;

    Ok(backup)
}

fn get_params(
    matches: &ArgMatches,
) -> Result<(String, String, Option<String>, Option<String>), String> {
    let password: Option<String> = matches
        .get_one::<String>("password")
        .map(|s| s.to_string())
        .map_or_else(
            || get_password(false, false),
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

    let backup_hash = matches.get_one::<String>("backup").map(|s| s.to_string());

    Ok((key, storage, password, backup_hash))
}
