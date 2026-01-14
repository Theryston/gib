use crate::core::crypto::get_password;
use crate::core::crypto::read_file_maybe_decrypt;
use crate::core::indexes::list_backup_summaries;
use crate::core::metadata::Backup;
use crate::core::permissions::set_file_permissions;
use crate::fs::FS;
use crate::utils::{decompress_bytes, get_fs, get_pwd_string, get_storage, handle_error};
use clap::ArgMatches;
use dialoguer::Select;
use dirs::home_dir;
use indicatif::{ProgressBar, ProgressStyle};
use sha2::{Digest, Sha256};
use std::collections::HashSet;
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;
use tokio::task::JoinSet;
use walkdir::WalkDir;

pub async fn restore(matches: &ArgMatches) {
    let (key, storage, password, backup_hash, target_path) = match get_params(matches) {
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
    pb.set_message("Loading backup data...");

    let backup = match load_backup(
        Arc::clone(&fs),
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

    let pb = ProgressBar::new(backup.tree.len() as u64);
    pb.enable_steady_tick(Duration::from_millis(100));
    pb.set_style(
        ProgressStyle::with_template("[{elapsed_precise}] [{bar:40.cyan/blue}] {pos}/{len} {msg}")
            .unwrap(),
    );
    pb.set_message(format!(
        "Restoring files from {}...",
        full_backup_hash[..8.min(full_backup_hash.len())].to_string()
    ));

    let mut files_set: JoinSet<Result<(), String>> = JoinSet::new();
    let restored_files = Arc::new(std::sync::Mutex::new(0u64));
    let skipped_files = Arc::new(std::sync::Mutex::new(0u64));

    for (relative_path, backup_object) in backup.tree.iter() {
        let pb_clone = pb.clone();
        let fs_clone = Arc::clone(&fs);
        let key_clone = key.clone();
        let password_clone = password.clone();
        let target_path_clone = target_path.clone();
        let relative_path_clone = relative_path.clone();
        let backup_object_clone = backup_object.clone();
        let restored_files_clone = Arc::clone(&restored_files);
        let skipped_files_clone = Arc::clone(&skipped_files);

        files_set.spawn(async move {
            let local_path = Path::new(&target_path_clone).join(&relative_path_clone);

            let needs_restore = if local_path.exists() {
                match calculate_file_hash(&local_path) {
                    Ok(local_hash) => local_hash != backup_object_clone.hash,
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
                pb_clone.inc(1);
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

            let mut file = std::fs::File::create(&local_path)
                .map_err(|e| format!("Failed to create file {}: {}", relative_path_clone, e))?;

            for chunk_hash in &backup_object_clone.chunks {
                let (prefix, rest) = chunk_hash.split_at(2);
                let chunk_path = format!("{}/chunks/{}/{}", key_clone, prefix, rest);

                let chunk_data = read_file_maybe_decrypt(
                    &fs_clone,
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

            set_file_permissions(&local_path, backup_object_clone.permissions).map_err(|e| {
                format!(
                    "Failed to set permissions for {}: {}",
                    relative_path_clone, e
                )
            })?;

            {
                let mut restored = restored_files_clone.lock().unwrap();
                *restored += 1;
            }

            pb_clone.inc(1);
            Ok(())
        });
    }

    let mut failed_files = Vec::new();

    while let Some(file_process_result) = files_set.join_next().await {
        match file_process_result {
            Ok(Ok(_)) => {}
            Ok(Err(e)) => failed_files.push(e),
            Err(e) => failed_files.push(e.to_string()),
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

    pb.set_message("Cleaning up files not in backup...");
    let deleted_count = match cleanup_extra_files(&target_path, &backup.tree) {
        Ok(count) => count,
        Err(e) => {
            eprintln!("Warning: Failed to clean up extra files: {}", e);
            0
        }
    };

    let restored_count = *restored_files.lock().unwrap();
    let skipped_count = *skipped_files.lock().unwrap();

    let elapsed = pb.elapsed();
    pb.set_style(ProgressStyle::with_template("{prefix:.green} {msg}").unwrap());
    pb.set_prefix("✓");
    pb.finish_with_message(format!(
        "Restored {} files, skipped {} files, deleted {} files ({:.2?})",
        restored_count, skipped_count, deleted_count, elapsed
    ));
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
                .with_prompt("Select a backup to restore")
                .items(&items)
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
    backup_hash: &str,
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
                    eprintln!("Warning: Failed to delete {}: {}", relative_path_str, e);
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
) -> Result<(String, String, Option<String>, Option<String>, String), String> {
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

    Ok((key, storage, password, backup_hash, target_path))
}
