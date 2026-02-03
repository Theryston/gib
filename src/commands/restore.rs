use crate::core::crypto::get_password;
use crate::core::crypto::read_file_maybe_decrypt;
use crate::core::indexes::list_backup_summaries;
use crate::core::metadata::Backup;
use crate::core::only::OnlyRequest;
use crate::core::only::filter_only_paths;
use crate::core::only::parse_only_request;
use crate::core::only::select_only_paths_interactive;
use crate::core::permissions::set_file_permissions;
use crate::output::{JsonProgress, emit_output, emit_progress_message, emit_warning, is_json_mode};
use crate::storage_clients::ClientStorage;
use crate::utils::{
    decompress_bytes, get_pwd_string, get_storage, get_storage_client, handle_error,
};
use clap::ArgMatches;
use dialoguer::Select;
use dirs::home_dir;
use futures::stream::{self, StreamExt};
use indicatif::{ProgressBar, ProgressStyle};
use sha2::{Digest, Sha256};
use std::collections::HashSet;
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio::sync::{Mutex as TokioMutex, Semaphore};
use tokio::task::JoinSet;
use walkdir::WalkDir;

const MAX_CONCURRENT_FILES: usize = 100;

pub async fn restore(matches: &ArgMatches) {
    let (key, storage, password, backup_hash, target_path, prune_local, only_request) =
        match get_params(matches) {
            Ok(params) => params,
            Err(e) => handle_error(e, None),
        };

    let started_at = Instant::now();

    let storage = get_storage(&storage);

    let storage_client = get_storage_client(&storage, None);

    let full_backup_hash = match resolve_backup_hash(
        Arc::clone(&storage_client),
        key.clone(),
        password.clone(),
        backup_hash,
    )
    .await
    {
        Ok(hash) => hash,
        Err(e) => handle_error(e, None),
    };

    let pb = if is_json_mode() {
        ProgressBar::hidden()
    } else {
        let pb = ProgressBar::new(100);
        pb.enable_steady_tick(Duration::from_millis(100));
        pb.set_style(ProgressStyle::with_template("{spinner:.green} {msg}").unwrap());
        pb.set_message("Loading backup data...");
        pb
    };

    if is_json_mode() {
        emit_progress_message("Loading backup data...");
    }

    let backup = match load_backup(
        Arc::clone(&storage_client),
        key.clone(),
        password.clone(),
        &full_backup_hash,
    )
    .await
    {
        Ok(backup) => backup,
        Err(e) => handle_error(e, Some(&pb)),
    };

    pb.finish_and_clear();

    let files_to_restore = match only_request {
        OnlyRequest::None => backup
            .tree
            .iter()
            .map(|(k, v)| (k.clone(), v.clone()))
            .collect(),
        OnlyRequest::Paths(paths) => match filter_only_paths(&backup.tree, &paths) {
            Ok(files) => files,
            Err(e) => handle_error(e, None),
        },
        OnlyRequest::Interactive => {
            let selected_paths = match select_only_paths_interactive(&backup.tree) {
                Ok(paths) => paths,
                Err(e) => handle_error(e, None),
            };
            match filter_only_paths(&backup.tree, &selected_paths) {
                Ok(files) => files,
                Err(e) => handle_error(e, None),
            }
        }
    };

    let total_files = files_to_restore.len() as u64;

    let json_progress = if is_json_mode() {
        let progress = JsonProgress::new(total_files);
        progress.set_message(&format!(
            "Restoring files from {}...",
            full_backup_hash[..8.min(full_backup_hash.len())].to_string()
        ));
        Some(progress)
    } else {
        None
    };

    let pb = if is_json_mode() {
        ProgressBar::hidden()
    } else {
        let pb = ProgressBar::new(total_files);
        pb.enable_steady_tick(Duration::from_millis(100));
        pb.set_style(
            ProgressStyle::with_template(
                "[{elapsed_precise}] [{bar:40.cyan/blue}] {pos}/{len} {msg}",
            )
            .unwrap(),
        );
        pb.set_message(format!(
            "Restoring files from {}...",
            full_backup_hash[..8.min(full_backup_hash.len())].to_string()
        ));
        pb
    };

    let files_set = Arc::new(TokioMutex::new(JoinSet::new()));
    let restored_files = Arc::new(std::sync::Mutex::new(0u64));
    let skipped_files = Arc::new(std::sync::Mutex::new(0u64));
    let semaphore = Arc::new(Semaphore::new(MAX_CONCURRENT_FILES));

    let files_stream = stream::iter(files_to_restore);

    files_stream
        .for_each_concurrent(MAX_CONCURRENT_FILES, |(relative_path, backup_object)| {
            let pb_clone = pb.clone();
            let storage_client_clone = Arc::clone(&storage_client);
            let key_clone = key.clone();
            let password_clone = password.clone();
            let target_path_clone = target_path.clone();
            let relative_path_clone = relative_path.clone();
            let restored_files_clone = Arc::clone(&restored_files);
            let skipped_files_clone = Arc::clone(&skipped_files);
            let semaphore_clone = Arc::clone(&semaphore);
            let files_set_clone = Arc::clone(&files_set);
            let json_progress_clone = json_progress.clone();

            async move {
                let mut guard = files_set_clone.lock().await;
                guard.spawn(async move {
                    let _permit = semaphore_clone.acquire().await.expect("Semaphore closed");
                    let local_path = Path::new(&target_path_clone).join(&relative_path_clone);

                    let needs_restore = if local_path.exists() {
                        match calculate_file_hash(&local_path) {
                            Ok(local_hash) => local_hash != backup_object.hash,
                            Err(_) => true,
                        }
                    } else {
                        true
                    };

                    if !needs_restore {
                        {
                            let mut skipped = skipped_files_clone.lock().unwrap();
                            *skipped += 1;
                        }
                        if let Some(progress) = &json_progress_clone {
                            progress.inc_by(1);
                        } else {
                            pb_clone.inc(1);
                        }
                        return Ok(());
                    }

                    if let Some(parent) = local_path.parent() {
                        std::fs::create_dir_all(parent).map_err(|e| {
                            format!(
                                "Failed to create parent directory for {}: {}",
                                relative_path_clone, e
                            )
                        })?;
                    }

                    let mut file = std::fs::File::create(&local_path).map_err(|e| {
                        format!("Failed to create file {}: {}", relative_path_clone, e)
                    })?;

                    for chunk_hash in &backup_object.chunks {
                        let (prefix, rest) = chunk_hash.split_at(2);
                        let chunk_path = format!("{}/chunks/{}/{}", key_clone, prefix, rest);

                        let chunk_data = read_file_maybe_decrypt(
                            &storage_client_clone,
                            &chunk_path,
                            password_clone.as_deref(),
                            "Chunk is encrypted but no password provided",
                        )
                        .await
                        .map_err(|e| format!("Failed to read chunk {}: {}", chunk_hash, e))?;

                        let decompressed = decompress_bytes(&chunk_data.bytes);

                        file.write_all(&decompressed).map_err(|e| {
                            format!(
                                "Failed to write chunk {} to file {}: {}",
                                chunk_hash, relative_path_clone, e
                            )
                        })?;
                    }

                    set_file_permissions(&local_path, backup_object.permissions).map_err(|e| {
                        format!(
                            "Failed to set permissions for {}: {}",
                            relative_path_clone, e
                        )
                    })?;

                    {
                        let mut restored = restored_files_clone.lock().unwrap();
                        *restored += 1;
                    }

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

    let mut failed_files = Vec::new();

    {
        let mut guard = files_set.lock().await;
        while let Some(file_process_result) = guard.join_next().await {
            match file_process_result {
                Ok(Ok(_)) => {}
                Ok(Err(e)) => failed_files.push(e),
                Err(e) => failed_files.push(e.to_string()),
            }
        }
    }

    if !failed_files.is_empty() {
        handle_error(
            format!(
                "Failed to restore {} files:\n{}",
                failed_files.len(),
                failed_files
                    .iter()
                    .map(|f| format!("  - {}", f))
                    .collect::<Vec<String>>()
                    .join("\n")
            ),
            Some(&pb),
        );
    }

    let deleted_count = if prune_local {
        pb.set_message("Cleaning up files not in backup...");
        if is_json_mode() {
            emit_progress_message("Cleaning up files not in backup...");
        }
        match cleanup_extra_files(&target_path, &backup.tree) {
            Ok(count) => count,
            Err(e) => {
                emit_warning(
                    &format!("Failed to clean up extra files: {}", e),
                    "cleanup_failed",
                );
                0
            }
        }
    } else {
        0
    };

    let restored_count = *restored_files.lock().unwrap();
    let skipped_count = *skipped_files.lock().unwrap();

    if is_json_mode() {
        #[derive(serde::Serialize)]
        struct RestoreOutput {
            backup: String,
            backup_short: String,
            restored: u64,
            skipped: u64,
            deleted_local: u64,
            target_path: String,
            elapsed_ms: u64,
        }

        let payload = RestoreOutput {
            backup: full_backup_hash.clone(),
            backup_short: full_backup_hash[..8.min(full_backup_hash.len())].to_string(),
            restored: restored_count,
            skipped: skipped_count,
            deleted_local: deleted_count,
            target_path: target_path.clone(),
            elapsed_ms: started_at.elapsed().as_millis() as u64,
        };
        emit_output(&payload);
    } else {
        let elapsed = pb.elapsed();
        pb.set_style(ProgressStyle::with_template("{prefix:.green} {msg}").unwrap());
        pb.set_prefix("OK");

        if deleted_count > 0 {
            pb.finish_with_message(format!(
                "Restored {} files, skipped {} files, deleted {} files ({:.2?})",
                restored_count, skipped_count, deleted_count, elapsed
            ));
        } else {
            pb.finish_with_message(format!(
                "Restored {} files, skipped {} files ({:.2?})",
                restored_count, skipped_count, elapsed
            ));
        }
    }
}

async fn resolve_backup_hash(
    storage_client: Arc<dyn ClientStorage>,
    key: String,
    password: Option<String>,
    provided_hash: Option<String>,
) -> Result<String, String> {
    match provided_hash {
        Some(hash) => {
            if hash.len() <= 8 {
                let summaries = list_backup_summaries(storage_client, key, password).await?;

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
            if is_json_mode() {
                return Err(
                    "Missing required argument: --backup (required in --mode json)".to_string(),
                );
            }
            let summaries = list_backup_summaries(storage_client, key, password).await?;

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
                .with_prompt("Select a backup to restore")
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
    storage_client: Arc<dyn ClientStorage>,
    key: String,
    password: Option<String>,
    backup_hash: &str,
) -> Result<Backup, String> {
    let backup_path = format!("{}/backups/{}", key, backup_hash);

    let read_result = read_file_maybe_decrypt(
        &storage_client,
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

fn calculate_file_hash(path: &Path) -> Result<String, std::io::Error> {
    let mut file = std::fs::File::open(path)?;
    let mut hasher = Sha256::new();
    let mut buffer = vec![0u8; 8192];

    loop {
        let bytes_read = file.read(&mut buffer)?;
        if bytes_read == 0 {
            break;
        }
        hasher.update(&buffer[..bytes_read]);
    }

    Ok(format!("{:x}", hasher.finalize()))
}

fn cleanup_extra_files(
    target_path: &str,
    backup_tree: &std::collections::HashMap<String, crate::core::metadata::BackupObject>,
) -> Result<u64, String> {
    let target_path_buf = PathBuf::from(target_path);

    if !target_path_buf.exists() {
        return Ok(0);
    }

    let backup_paths: HashSet<String> = backup_tree.keys().map(|p| p.replace('\\', "/")).collect();

    let mut deleted_count = 0u64;
    let mut dirs_to_check = HashSet::new();

    for entry in WalkDir::new(&target_path_buf)
        .into_iter()
        .filter_map(|e| e.ok())
        .filter(|e| e.path().is_file())
    {
        let file_path = entry.path();

        let relative_path = match file_path.strip_prefix(&target_path_buf) {
            Ok(rel) => rel,
            Err(_) => continue,
        };

        let relative_path_str = relative_path.to_string_lossy().replace('\\', "/");

        if !backup_paths.contains(&relative_path_str) {
            match std::fs::remove_file(file_path) {
                Ok(_) => {
                    deleted_count += 1;
                    let mut current = file_path.parent();
                    while let Some(parent) = current {
                        if parent != target_path_buf {
                            dirs_to_check.insert(parent.to_path_buf());
                        }
                        current = parent.parent();
                    }
                }
                Err(e) => {
                    emit_warning(
                        &format!("Failed to delete {}: {}", relative_path_str, e),
                        "delete_failed",
                    );
                }
            }
        }
    }

    let mut dirs_vec: Vec<PathBuf> = dirs_to_check.into_iter().collect();
    dirs_vec.sort_by(|a, b| b.components().count().cmp(&a.components().count()));

    for dir in dirs_vec {
        if dir.exists() && dir != target_path_buf {
            if let Ok(mut entries) = std::fs::read_dir(&dir) {
                if entries.next().is_none() {
                    let _ = std::fs::remove_dir(&dir);
                }
            }
        }
    }

    Ok(deleted_count)
}

fn get_params(
    matches: &ArgMatches,
) -> Result<
    (
        String,
        String,
        Option<String>,
        Option<String>,
        String,
        bool,
        OnlyRequest,
    ),
    String,
> {
    let password: Option<String> = matches
        .get_one::<String>("password")
        .map(|s| s.to_string())
        .map_or_else(
            || get_password(false, true),
            |password| Some(password.to_string()),
        );

    let pwd_string = get_pwd_string();

    let target_path = matches.get_one::<String>("target-path").map_or_else(
        || pwd_string.clone(),
        |target_path| {
            Path::new(&pwd_string)
                .join(target_path)
                .to_string_lossy()
                .to_string()
        },
    );

    let default_key = Path::new(&pwd_string)
        .file_name()
        .unwrap()
        .to_string_lossy()
        .to_string();

    let key = matches
        .get_one::<String>("key")
        .map_or_else(|| default_key, |key| key.to_string());

    let prune_local = matches.get_flag("prune-local");
    let only_request = parse_only_request(matches, prune_local)?;

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

    let backup_hash = matches.get_one::<String>("backup").map(|s| s.to_string());

    Ok((
        key,
        storage,
        password,
        backup_hash,
        target_path,
        prune_local,
        only_request,
    ))
}
