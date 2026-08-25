use crate::config::{
    PasswordPolicy, RepositoryOptions, load_and_report_local_config, resolve_repository,
};
use crate::core::crypto::{read_file_maybe_decrypt, write_file_maybe_encrypt};
use crate::core::indexes::list_backup_summaries;
use crate::core::indexes::load_chunk_indexes;
use crate::core::metadata::{BackupSummary, ChunkIndex};
use crate::fs::FS;
use crate::output::{JsonProgress, emit_output, emit_progress_message, is_json_mode};
use crate::utils::handle_error;
use clap::ArgMatches;
use console::style;
use futures::stream::{self, StreamExt};
use indicatif::{ProgressBar, ProgressStyle};
use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tokio::sync::{Mutex as TokioMutex, Semaphore};
use tokio::task::JoinSet;

const MAX_CONCURRENT_FILES: usize = 100;

pub async fn encrypt(matches: &ArgMatches) {
    let repository = match get_params(matches) {
        Ok(params) => params,
        Err(e) => handle_error(e, None),
    };

    let key = repository.key;
    let password = repository.password;
    let fs = repository.fs;

    if password.is_none() {
        handle_error("Password is required".to_string(), None);
    }

    let pb = if is_json_mode() {
        ProgressBar::hidden()
    } else {
        let pb = ProgressBar::new(100);
        pb.enable_steady_tick(Duration::from_millis(100));
        pb.set_style(ProgressStyle::with_template("{spinner:.green} {msg}").unwrap());
        pb.set_message("Loading metadata from the repository key...");
        pb
    };

    if is_json_mode() {
        emit_progress_message("Loading metadata from the repository key...");
    }

    let prev_not_encrypted_but_now_yes = Arc::new(Mutex::new(false));

    let (chunk_indexes, backup_summaries) = match load_metadata(
        Arc::clone(&fs),
        key.clone(),
        password.clone(),
        Arc::clone(&prev_not_encrypted_but_now_yes),
    )
    .await
    {
        Ok(result) => result,
        Err(e) => handle_error(e, Some(&pb)),
    };

    let mut files_to_encrypt = Vec::new();

    files_to_encrypt.push(format!("{}/indexes/chunks", key));
    files_to_encrypt.push(format!("{}/indexes/backups", key));
    let head_path = format!("{}/indexes/HEAD", key);
    if fs.read_file(&head_path).await.is_ok() {
        files_to_encrypt.push(head_path);
    }

    for (chunk_hash, _) in chunk_indexes.iter() {
        let (chunk_hash_prefix, chunk_hash_rest) = chunk_hash.split_at(2);
        let chunk_path = format!("{}/chunks/{}/{}", &key, chunk_hash_prefix, chunk_hash_rest);
        files_to_encrypt.push(chunk_path);
    }

    for backup_summary in backup_summaries.iter() {
        let backup_file_path = format!("{}/backups/{}", key, backup_summary.hash);
        files_to_encrypt.push(backup_file_path);
    }

    pb.finish_and_clear();

    if !is_json_mode() {
        if *prev_not_encrypted_but_now_yes.lock().unwrap() {
            println!(
                "{}",
                style("Encrypting all chunks of the repository...").green()
            );
        } else {
            println!(
                "{}",
                style("Some chunks are already encrypted, encrypting all the other chunks now...")
                    .green()
            );
        }
    }

    let json_progress = if is_json_mode() {
        let progress = JsonProgress::new(files_to_encrypt.len() as u64);
        progress.set_message("Encrypting chunks...");
        Some(progress)
    } else {
        None
    };

    let pb = if is_json_mode() {
        ProgressBar::hidden()
    } else {
        let pb = ProgressBar::new(files_to_encrypt.len() as u64);
        pb.enable_steady_tick(Duration::from_millis(100));
        pb.set_style(
            ProgressStyle::with_template(
                "[{elapsed_precise}] [{bar:40.cyan/blue}] {pos}/{len} {msg}",
            )
            .unwrap(),
        );
        pb.set_message("Encrypting chunks...");
        pb
    };

    let encrypted_amount = Arc::new(Mutex::new(0));
    let already_encrypted_amount = Arc::new(Mutex::new(0));
    let files_set = Arc::new(TokioMutex::new(JoinSet::new()));
    let semaphore = Arc::new(Semaphore::new(MAX_CONCURRENT_FILES));

    let files_stream = stream::iter(files_to_encrypt);

    files_stream
        .for_each_concurrent(MAX_CONCURRENT_FILES, |file_path| {
            let pb_clone = pb.clone();
            let password_clone = password.clone();
            let fs_clone = Arc::clone(&fs);
            let file_path_clone = file_path.clone();
            let already_encrypted_amount_clone = Arc::clone(&already_encrypted_amount);
            let encrypted_amount_clone = Arc::clone(&encrypted_amount);
            let semaphore_clone = Arc::clone(&semaphore);
            let files_set_clone = Arc::clone(&files_set);
            let json_progress_clone = json_progress.clone();

            async move {
                let mut guard = files_set_clone.lock().await;
                guard.spawn(async move {
                    let _permit = semaphore_clone.acquire().await.expect("Semaphore closed");
                    let read_result = read_file_maybe_decrypt(
                        &fs_clone,
                        &file_path_clone,
                        password_clone.as_deref(),
                        "File is encrypted but no password provided",
                    )
                    .await?;

                    if read_result.was_encrypted {
                        {
                            let mut already_encrypted_amount_guard =
                                already_encrypted_amount_clone.lock().unwrap();
                            *already_encrypted_amount_guard += 1;
                        }

                        if let Some(progress) = &json_progress_clone {
                            progress.inc_by(1);
                        } else {
                            pb_clone.inc(1);
                        }
                        return Ok(());
                    }

                    write_file_maybe_encrypt(
                        &fs_clone,
                        &file_path_clone,
                        &read_result.bytes,
                        password_clone.as_deref(),
                    )
                    .await?;

                    {
                        let mut encrypted_amount_guard = encrypted_amount_clone.lock().unwrap();
                        *encrypted_amount_guard += 1;
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
                "Failed to process {} files:\n{}",
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

    let encrypted_amount = encrypted_amount.lock().unwrap();
    let already_encrypted_amount = already_encrypted_amount.lock().unwrap();

    if is_json_mode() {
        #[derive(serde::Serialize)]
        struct EncryptOutput {
            encrypted: u64,
            already_encrypted: u64,
        }

        let payload = EncryptOutput {
            encrypted: *encrypted_amount,
            already_encrypted: *already_encrypted_amount,
        };
        emit_output(&payload);
    } else {
        pb.set_style(ProgressStyle::with_template("{prefix:.green} {msg}").unwrap());
        pb.set_prefix("OK");

        if *already_encrypted_amount > 0 {
            pb.finish_with_message(format!(
                "Encrypted {} chunks ({} were already encrypted)",
                encrypted_amount, already_encrypted_amount
            ));
        } else {
            pb.finish_with_message(format!("Encrypted {} chunks", encrypted_amount));
        }
    }
}

async fn load_metadata(
    fs: Arc<dyn FS>,
    key: String,
    password: Option<String>,
    prev_not_encrypted_but_now_yes: Arc<Mutex<bool>>,
) -> Result<(HashMap<String, ChunkIndex>, Vec<BackupSummary>), String> {
    let chunk_indexes_future = tokio::spawn(load_chunk_indexes(
        Arc::clone(&fs),
        key.clone(),
        password.clone(),
        Arc::clone(&prev_not_encrypted_but_now_yes),
    ));

    let backup_summaries_future = tokio::spawn(list_backup_summaries(
        Arc::clone(&fs),
        key.clone(),
        password.clone(),
    ));

    let (chunk_indexes_result, backup_summaries_result) =
        tokio::join!(chunk_indexes_future, backup_summaries_future);

    let chunk_indexes = chunk_indexes_result
        .map_err(|e| format!("Failed to load chunk indexes: {}", e))?
        .map_err(|e| format!("Failed to load chunk indexes: {}", e))?;

    let backup_summaries = backup_summaries_result
        .map_err(|e| format!("Failed to load backup summaries: {}", e))?
        .map_err(|e| format!("Failed to load backup summaries: {}", e))?;

    Ok((chunk_indexes, backup_summaries))
}

fn get_params(matches: &ArgMatches) -> Result<RepositoryOptions, String> {
    let local_config = load_and_report_local_config(matches)?;
    resolve_repository(
        matches,
        &local_config,
        PasswordPolicy {
            required: true,
            readonly: false,
        },
        None,
    )
}
