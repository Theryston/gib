use crate::config::{
    PasswordPolicy, load_and_report_local_config, resolve_path, resolve_repository,
};
use crate::core::crypto::read_file_maybe_decrypt;
use crate::core::indexes::{list_backup_summaries, resolve_backup_reference};
use crate::core::metadata::Backup;
use crate::core::only::OnlyRequest;
use crate::core::only::filter_only_paths;
use crate::core::only::parse_only_request;
use crate::core::only::select_only_paths_interactive;
use crate::core::restore::{RestoreProgressCallback, RestoreStats, restore_files};
use crate::fs::FS;
use crate::output::{JsonProgress, emit_output, emit_progress_message, emit_warning, is_json_mode};
use crate::utils::{decompress_bytes, handle_error};
use clap::ArgMatches;
use dialoguer::Select;
use indicatif::{ProgressBar, ProgressStyle};
use std::collections::HashSet;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::{Duration, Instant};
use walkdir::WalkDir;

pub async fn restore(matches: &ArgMatches) {
    let (key, fs, password, backup_hash, target_path, prune_local, only_request) =
        match get_params(matches) {
            Ok(params) => params,
            Err(e) => handle_error(e, None),
        };

    let started_at = Instant::now();

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

    let progress: RestoreProgressCallback = if let Some(json_progress) = json_progress.clone() {
        Arc::new(move || json_progress.inc_by(1))
    } else {
        let pb_clone = pb.clone();
        Arc::new(move || pb_clone.inc(1))
    };
    let stats = restore_files(
        Arc::clone(&fs),
        key.clone(),
        password.clone(),
        target_path.clone(),
        files_to_restore,
        Some(progress),
    )
    .await;

    if !stats.failed.is_empty() {
        handle_error(
            format!(
                "Failed to restore {} files:\n{}",
                stats.failed.len(),
                stats
                    .failed
                    .iter()
                    .map(|failure| format!("  - {}: {}", failure.path, failure.message))
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

    let restored_count = stats.restored;
    let skipped_count = stats.skipped;

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
    fs: Arc<dyn FS>,
    key: String,
    password: Option<String>,
    provided_hash: Option<String>,
) -> Result<String, String> {
    match provided_hash {
        Some(hash) => resolve_backup_reference(fs, key, password, &hash).await,
        None => {
            if is_json_mode() {
                return Err(
                    "Missing required argument: --backup (required in --mode json)".to_string(),
                );
            }
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

pub(crate) struct SelectedRestoreResult {
    pub(crate) stats: RestoreStats,
    pub(crate) unavailable: Vec<String>,
}

pub(crate) async fn restore_selected_paths(
    fs: Arc<dyn FS>,
    key: String,
    password: Option<String>,
    target_path: String,
    backup_hash: String,
    paths: &[String],
    progress: Option<RestoreProgressCallback>,
) -> Result<SelectedRestoreResult, String> {
    let backup = load_backup(Arc::clone(&fs), key.clone(), password.clone(), &backup_hash).await?;

    let mut files = Vec::with_capacity(paths.len());
    let mut unavailable = Vec::new();
    for path in paths {
        let object = backup.tree.get(path).or_else(|| {
            backup.tree.iter().find_map(|(backup_path, object)| {
                crate::core::catalog::normalize_file_path(backup_path)
                    .ok()
                    .filter(|normalized| normalized == path)
                    .map(|_| object)
            })
        });
        if let Some(object) = object {
            files.push((path.clone(), object.clone()));
        } else {
            unavailable.push(path.clone());
        }
    }

    let stats = restore_files(fs, key, password, target_path, files, progress).await;
    Ok(SelectedRestoreResult { stats, unavailable })
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
        Arc<dyn FS>,
        Option<String>,
        Option<String>,
        String,
        bool,
        OnlyRequest,
    ),
    String,
> {
    let local_config = load_and_report_local_config(matches)?;
    let repository = resolve_repository(
        matches,
        &local_config,
        PasswordPolicy {
            required: false,
            readonly: true,
        },
        None,
    )?;
    let target_path = resolve_path(
        matches.get_one::<String>("target-path"),
        local_config.config.restore.target_path.as_ref(),
        &local_config,
    )?;
    let prune_local = matches.get_flag("prune-local");
    let only_request = parse_only_request(matches, prune_local)?;

    let backup_hash = matches.get_one::<String>("backup").map(|s| s.to_string());

    Ok((
        repository.key,
        repository.fs,
        repository.password,
        backup_hash,
        target_path,
        prune_local,
        only_request,
    ))
}
