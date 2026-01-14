use crate::commands::config::Config;
use crate::core::crypto::get_password;
use crate::core::crypto::write_file_maybe_encrypt;
use crate::core::indexes::{create_new_commit, load_chunk_indexes};
use crate::core::metadata::{ChunkIndex, Commit, CommitObject};
use crate::core::permissions::get_file_permissions_with_path;
use crate::fs::{FS, LocalFS, S3FS, S3FSConfig};
use crate::utils::{compress_bytes, get_pwd_string, get_storage, handle_error, list_files};
use clap::ArgMatches;
use console::style;
use dialoguer::{Input, Select};
use dirs::home_dir;
use indicatif::{ProgressBar, ProgressStyle};
use parse_size::parse_size;
use sha2::{Digest, Sha256};
use std::collections::HashMap;
use std::io::Read;
use std::path::Path;
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tokio::task::JoinSet;

pub async fn backup(matches: &ArgMatches) {
    let (key, message, root_path_string, storage, compress, password, chunk_size) =
        match get_params(matches) {
            Ok(params) => params,
            Err(e) => handle_error(e, None),
        };

    let home_dir = match home_dir() {
        Some(dir) => dir,
        None => handle_error("Failed to get home directory".to_string(), None),
    };

    let config_path = home_dir.join(".gib").join("config.msgpack");

    if !config_path.exists() {
        handle_error("Seams like you didn't configure your backup tool yet. Run 'gib config' to configure your backup tool.".to_string(), None);
    }

    let config_bytes = match std::fs::read(&config_path) {
        Ok(bytes) => bytes,
        Err(e) => handle_error(format!("Failed to read config file: {}", e), None),
    };

    let config: Config = match rmp_serde::from_slice(&config_bytes) {
        Ok(config) => config,
        Err(e) => handle_error(format!("Failed to deserialize config: {}", e), None),
    };

    let pb = ProgressBar::new(100);

    pb.enable_steady_tick(Duration::from_millis(100));
    pb.set_style(ProgressStyle::with_template("{spinner:.green} {msg}").unwrap());

    pb.set_message("Loading metadata from the repository key...");

    let storage = get_storage(&storage);

    let fs: Arc<dyn FS> = match storage.storage_type {
        0 => Arc::new(LocalFS::new(storage.path.unwrap())),
        1 => Arc::new(S3FS::new(S3FSConfig {
            region: storage.region,
            bucket: storage.bucket,
            access_key: storage.access_key,
            secret_key: storage.secret_key,
            endpoint: storage.endpoint,
        })),
        _ => handle_error("Invalid storage type".to_string(), Some(&pb)),
    };

    pb.set_message("Generating new backup...");

    let prev_not_encrypted_but_now_yes = Arc::new(Mutex::new(false));

    let (new_commit, root_files, chunk_indexes) = match load_metadata(
        Arc::clone(&fs),
        key.clone(),
        message,
        config,
        root_path_string.clone(),
        compress,
        password.clone(),
        Arc::clone(&prev_not_encrypted_but_now_yes),
    )
    .await
    {
        Ok(result) => result,
        Err(e) => handle_error(e, Some(&pb)),
    };

    pb.finish_and_clear();

    if *prev_not_encrypted_but_now_yes.lock().unwrap() {
        println!("{}", style("The backup was not encrypted but you provided a password! Only new chunks will be encrypted, for old chunks run 'gib encrypt'").yellow());
    }

    let pb = ProgressBar::new(root_files.len() as u64);
    pb.enable_steady_tick(Duration::from_millis(100));

    pb.set_style(
        ProgressStyle::with_template("[{elapsed_precise}] [{bar:40.cyan/blue}] {pos}/{len} {msg}")
            .unwrap(),
    );

    pb.set_message(format!(
        "Backing up files to {}...",
        new_commit.hash[..8].to_string()
    ));

    let chunk_indexes: Arc<Mutex<HashMap<String, ChunkIndex>>> =
        Arc::new(Mutex::new(chunk_indexes));

    let new_commit: Arc<Mutex<Commit>> = Arc::new(Mutex::new(new_commit));

    let mut files_set: JoinSet<Result<(), String>> = JoinSet::new();

    for file_path in root_files {
        let pb_clone = pb.clone();
        let chunk_indexes_clone = Arc::clone(&chunk_indexes);
        let password_clone = password.clone();
        let key_clone = key.clone();
        let fs_clone = Arc::clone(&fs);
        let new_commit_clone = Arc::clone(&new_commit);
        let root_path_string_clone = root_path_string.clone();

        files_set.spawn(async move {
            let mut file = std::fs::File::open(file_path.clone())
                .map_err(|e| format!("Failed to open file: {}", e))?;
            let mut file_hasher = Sha256::new();
            let mut file_chunks = Vec::new();

            let file_metadata = file
                .metadata()
                .map_err(|e| format!("Failed to get file metadata: {}", e))?;

            let mut buffer = vec![0u8; chunk_size as usize];

            loop {
                let bytes_read = file
                    .read(&mut buffer)
                    .map_err(|e| format!("Failed to read file: {}", e))
                    .unwrap_or(0);

                if bytes_read == 0 {
                    break;
                }

                let chunk_bytes = &buffer[..bytes_read];

                file_hasher.update(chunk_bytes);

                let chunk_hash = format!("{:x}", Sha256::digest(chunk_bytes));
                file_chunks.push(chunk_hash.clone());

                let is_in_chunk_indexes = {
                    let mut chunk_indexes_guard = chunk_indexes_clone.lock().unwrap();
                    let entry = chunk_indexes_guard
                        .entry(chunk_hash.clone())
                        .or_insert(ChunkIndex { refcount: 0 });
                    entry.refcount += 1;

                    entry.refcount > 1
                };

                if is_in_chunk_indexes {
                    continue;
                }

                let compressed_chunk_bytes = compress_bytes(chunk_bytes, compress);

                let (chunk_hash_prefix, chunk_hash_rest) = chunk_hash.split_at(2);
                let chunk_path = format!(
                    "{}/chunks/{}/{}",
                    key_clone, chunk_hash_prefix, chunk_hash_rest
                );

                write_file_maybe_encrypt(
                    &fs_clone,
                    &chunk_path,
                    &compressed_chunk_bytes,
                    password_clone.as_deref(),
                )
                .await
                .map_err(|e| format!("Failed to write chunk: {}", e))?;
            }

            let file_hash = format!("{:x}", file_hasher.finalize());

            let relative_path = {
                let content = file_path
                    .strip_prefix(&root_path_string_clone)
                    .unwrap_or(&file_path);

                let mut content = content.replace('\\', "/");

                if content.starts_with('/') {
                    content = content[1..].to_string();
                }

                content
            };

            let file_permissions = get_file_permissions_with_path(&file_metadata, &file_path);

            {
                let mut new_commit_guard = new_commit_clone.lock().unwrap();

                new_commit_guard.tree.insert(
                    relative_path.to_string(),
                    CommitObject {
                        hash: file_hash.clone(),
                        size: file_metadata.len(),
                        content_type: "application/octet-stream".to_string(),
                        permissions: file_permissions,
                        chunks: file_chunks,
                    },
                );
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

    let chunk_indexes_bytes =
        rmp_serde::to_vec(&*chunk_indexes.lock().unwrap()).unwrap_or_else(|_| Vec::new());

    let compressed_chunk_indexes_bytes = compress_bytes(&chunk_indexes_bytes, compress);

    let chunk_index_path = format!("{}/indexes/chunks", key);

    let write_chunk_index_future = write_file_maybe_encrypt(
        &fs,
        &chunk_index_path,
        &compressed_chunk_indexes_bytes,
        password.as_deref(),
    );

    let commit_file_bytes =
        rmp_serde::to_vec(&*new_commit.lock().unwrap()).unwrap_or_else(|_| Vec::new());

    let compressed_commit_file_bytes = compress_bytes(&commit_file_bytes, compress);

    let commit_file_path = format!("{}/commits/{}", key, new_commit.lock().unwrap().hash);

    let write_commit_file_future = write_file_maybe_encrypt(
        &fs,
        &commit_file_path,
        &compressed_commit_file_bytes,
        password.as_deref(),
    );

    let (write_chunk_index_result, write_commit_file_result) =
        tokio::join!(write_chunk_index_future, write_commit_file_future);

    if write_chunk_index_result.is_err() {
        handle_error("Failed to write chunk indexes".to_string(), Some(&pb));
    }

    if write_commit_file_result.is_err() {
        handle_error("Failed to write backup file".to_string(), Some(&pb));
    }

    let elapsed = pb.elapsed();
    pb.set_style(ProgressStyle::with_template("{prefix:.green} {msg}").unwrap());
    pb.set_prefix("✓");
    pb.finish_with_message(format!("Backed up files ({:.2?})", elapsed));
}

async fn load_metadata(
    fs: Arc<dyn FS>,
    key: String,
    message: String,
    config: Config,
    root_path_string: String,
    compress: i32,
    password: Option<String>,
    prev_not_encrypted_but_now_yes: Arc<Mutex<bool>>,
) -> Result<(Commit, Vec<String>, HashMap<String, ChunkIndex>), String> {
    let new_commit_future = tokio::spawn(create_new_commit(
        Arc::clone(&fs),
        key.clone(),
        message.clone(),
        config.author.clone(),
        compress.clone(),
        password.clone(),
    ));

    let root_files_future = tokio::spawn(async move { list_files(&root_path_string) });

    let chunk_indexes_future = tokio::spawn(load_chunk_indexes(
        Arc::clone(&fs),
        key.clone(),
        password,
        prev_not_encrypted_but_now_yes,
    ));

    let (new_commit_result, root_files_result, chunk_indexes_result) =
        tokio::join!(new_commit_future, root_files_future, chunk_indexes_future);

    let new_commit = new_commit_result
        .map_err(|e| format!("Failed to create new backup: {}", e))?
        .map_err(|e| format!("Failed to create new backup: {}", e))?;

    let root_files = root_files_result.map_err(|e| format!("Failed to list root files: {}", e))?;

    let chunk_indexes = chunk_indexes_result
        .map_err(|e| format!("Failed to load chunk indexes: {}", e))?
        .map_err(|e| format!("Failed to load chunk indexes: {}", e))?;

    Ok((new_commit, root_files, chunk_indexes))
}

fn get_params(
    matches: &ArgMatches,
) -> Result<(String, String, String, String, i32, Option<String>, u64), String> {
    let password: Option<String> = matches
        .get_one::<String>("password")
        .map(|s| s.to_string())
        .map_or_else(|| get_password(), |password| Some(password.to_string()));

    let pwd_string = get_pwd_string();

    let root_path_string = matches.get_one::<String>("root-path").map_or_else(
        || pwd_string.clone(),
        |root_path| {
            Path::new(&pwd_string)
                .join(root_path)
                .to_string_lossy()
                .to_string()
        },
    );

    let default_key = Path::new(&root_path_string)
        .file_name()
        .unwrap()
        .to_string_lossy()
        .to_string();

    let key = matches
        .get_one::<String>("key")
        .map_or_else(|| default_key, |key| key.to_string());

    let message = match matches.get_one::<String>("message") {
        Some(message) => message.to_string(),
        None => Input::<String>::new()
            .with_prompt("Enter the backup message")
            .interact_text()
            .map_err(|e| format!("{}", e))?,
    };

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

    let compress: i32 = matches
        .get_one::<String>("compress")
        .map_or_else(|| 3, |compress| compress.parse().unwrap());

    let chunk_size: u64 = matches.get_one::<String>("chunk-size").map_or_else(
        || parse_size("5 MB").unwrap(),
        |chunk_size| parse_size(chunk_size).unwrap(),
    );

    let exists = storages_names
        .iter()
        .any(|storage_name| storage_name == &storage);

    if !exists {
        return Err(format!("Storage '{}' not found", storage));
    }

    Ok((
        key,
        message,
        root_path_string,
        storage,
        compress,
        password,
        chunk_size,
    ))
}
