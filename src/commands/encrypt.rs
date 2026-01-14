use crate::core::crypto::{read_file_maybe_decrypt, write_file_maybe_encrypt};
use crate::core::indexes::list_commit_summaries;
use crate::core::metadata::{ChunkIndex, CommitSummary};
use crate::core::{crypto::get_password, indexes::load_chunk_indexes};
use crate::fs::FS;
use crate::utils::{get_fs, get_storage, handle_error};
use clap::ArgMatches;
use console::style;
use dialoguer::{Input, Select};
use dirs::home_dir;
use indicatif::{ProgressBar, ProgressStyle};
use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tokio::task::JoinSet;

pub async fn encrypt(matches: &ArgMatches) {
    let (key, storage, password) = match get_params(matches) {
        Ok(params) => params,
        Err(e) => handle_error(e, None),
    };

    if password.is_none() {
        handle_error("Password is required".to_string(), None);
    }

    let storage = get_storage(&storage);

    let pb = ProgressBar::new(100);

    pb.enable_steady_tick(Duration::from_millis(100));
    pb.set_style(ProgressStyle::with_template("{spinner:.green} {msg}").unwrap());

    pb.set_message("Loading metadata from the repository key...");

    let fs = get_fs(&storage, Some(&pb));

    let prev_not_encrypted_but_now_yes = Arc::new(Mutex::new(false));

    let (chunk_indexes, commit_sumaries) = match load_metadata(
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
    files_to_encrypt.push(format!("{}/indexes/commits", key));

    for (chunk_hash, _) in chunk_indexes.iter() {
        let (chunk_hash_prefix, chunk_hash_rest) = chunk_hash.split_at(2);
        let chunk_path = format!("{}/chunks/{}/{}", &key, chunk_hash_prefix, chunk_hash_rest);
        files_to_encrypt.push(chunk_path);
    }

    for commit_summary in commit_sumaries.iter() {
        let commit_file_path = format!("{}/commits/{}", key, commit_summary.hash);
        files_to_encrypt.push(commit_file_path);
    }

    pb.finish_and_clear();

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

    let pb = ProgressBar::new(files_to_encrypt.len() as u64);
    pb.enable_steady_tick(Duration::from_millis(100));

    pb.set_style(
        ProgressStyle::with_template("[{elapsed_precise}] [{bar:40.cyan/blue}] {pos}/{len} {msg}")
            .unwrap(),
    );

    pb.set_message(format!("Encrypting chunks..."));

    let encrypted_amount = Arc::new(Mutex::new(0));
    let already_encrypted_amount = Arc::new(Mutex::new(0));
    let mut files_set: JoinSet<Result<(), String>> = JoinSet::new();

    for file_path in files_to_encrypt {
        let pb_clone = pb.clone();
        let password_clone = password.clone();
        let fs_clone = Arc::clone(&fs);
        let file_path_clone = file_path.clone();
        let already_encrypted_amount_clone = Arc::clone(&already_encrypted_amount);
        let encrypted_amount_clone = Arc::clone(&encrypted_amount);

        files_set.spawn(async move {
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

                pb_clone.inc(1);
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

    pb.set_style(ProgressStyle::with_template("{prefix:.green} {msg}").unwrap());
    pb.set_prefix("✓");

    let encrypted_amount = encrypted_amount.lock().unwrap();
    let already_encrypted_amount = already_encrypted_amount.lock().unwrap();

    if *already_encrypted_amount > 0 {
        pb.finish_with_message(format!(
            "Encrypted {} chunks ({} were already encrypted)",
            encrypted_amount, already_encrypted_amount
        ));
    } else {
        pb.finish_with_message(format!("Encrypted {} chunks", encrypted_amount));
    }
}

async fn load_metadata(
    fs: Arc<dyn FS>,
    key: String,
    password: Option<String>,
    prev_not_encrypted_but_now_yes: Arc<Mutex<bool>>,
) -> Result<(HashMap<String, ChunkIndex>, Vec<CommitSummary>), String> {
    let chunk_indexes_future = tokio::spawn(load_chunk_indexes(
        Arc::clone(&fs),
        key.clone(),
        password.clone(),
        Arc::clone(&prev_not_encrypted_but_now_yes),
    ));

    let commit_sumaries_future = tokio::spawn(list_commit_summaries(
        Arc::clone(&fs),
        key.clone(),
        password.clone(),
    ));

    let (chunk_indexes_result, commit_sumaries_result) =
        tokio::join!(chunk_indexes_future, commit_sumaries_future);

    let chunk_indexes = chunk_indexes_result
        .map_err(|e| format!("Failed to load chunk indexes: {}", e))?
        .map_err(|e| format!("Failed to load chunk indexes: {}", e))?;

    let commit_sumaries = commit_sumaries_result
        .map_err(|e| format!("Failed to load commit summaries: {}", e))?
        .map_err(|e| format!("Failed to load commit summaries: {}", e))?;

    Ok((chunk_indexes, commit_sumaries))
}

fn get_params(matches: &ArgMatches) -> Result<(String, String, Option<String>), String> {
    let password: Option<String> = matches
        .get_one::<String>("password")
        .map(|s| s.to_string())
        .map_or_else(|| get_password(true), |password| Some(password.to_string()));

    let key = matches.get_one::<String>("key").map_or_else(
        || {
            let typed_key: String = Input::<String>::new()
                .with_prompt("Enter the key of the repository")
                .interact_text()
                .unwrap_or_else(|e| {
                    eprintln!("Error: {}", e);
                    std::process::exit(1);
                });
            typed_key
        },
        |key| key.to_string(),
    );

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

    Ok((key, storage, password))
}
