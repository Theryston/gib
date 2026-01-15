use crate::commands::config::Config;
use crate::core::crypto::get_password;
use crate::core::crypto::write_file_maybe_encrypt;
use crate::core::indexes::{add_backup_summary, create_new_backup, load_chunk_indexes};
use crate::core::metadata::{Backup, BackupObject, ChunkIndex};
use crate::core::permissions::get_file_permissions_with_path;
use crate::fs::FS;
use crate::utils::{compress_bytes, get_fs, get_pwd_string, get_storage, handle_error, list_files};
use bytesize::ByteSize;
use clap::ArgMatches;
use console::style;
use dialoguer::{Input, Select};
use dirs::home_dir;
use futures::stream::{self, StreamExt};
use indicatif::{ProgressBar, ProgressStyle};
use parse_size::parse_size;
use sha2::{Digest, Sha256};
use std::collections::HashMap;
use std::io::Read;
use std::path::Path;
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tokio::sync::{Mutex as TokioMutex, Semaphore};
use tokio::task::JoinSet;

const MAX_CONCURRENT_FILES: usize = 100;

pub async fn backup(matches: &ArgMatches) {
    let (key, message, root_path_string, storage, compress, password, chunk_size, ignore_patterns) =
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

    let fs = get_fs(&storage, Some(&pb));

    pb.set_message("Generating new backup...");

    let prev_not_encrypted_but_now_yes = Arc::new(Mutex::new(false));

    let (new_backup, root_files, chunk_indexes) = match load_metadata(
        Arc::clone(&fs),
        key.clone(),
        message,
        config,
        root_path_string.clone(),
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

    let root_files: Vec<String> = if ignore_patterns.is_empty() {
        root_files
    } else {
        root_files
            .into_iter()
            .filter(|file_path| {
                let normalized_path = file_path.replace('\\', "/");
                !ignore_patterns.iter().any(|pattern| {
                    normalized_path
                        .split('/')
                        .any(|component| component == pattern)
                })
            })
            .collect()
    };

    let pb = ProgressBar::new(root_files.len() as u64);
    pb.enable_steady_tick(Duration::from_millis(100));

    pb.set_style(
        ProgressStyle::with_template("[{elapsed_precise}] [{bar:40.cyan/blue}] {pos}/{len} {msg}")
            .unwrap(),
    );

    pb.set_message(format!(
        "Backing up files to {}...",
        new_backup.hash[..8].to_string()
    ));

    let chunk_indexes: Arc<Mutex<HashMap<String, ChunkIndex>>> =
        Arc::new(Mutex::new(chunk_indexes));

    let new_backup: Arc<Mutex<Backup>> = Arc::new(Mutex::new(new_backup));

    let files_set = Arc::new(TokioMutex::new(JoinSet::new()));
    let written_bytes = Arc::new(Mutex::new(0));
    let deduplicated_bytes = Arc::new(Mutex::new(0));
    let semaphore = Arc::new(Semaphore::new(MAX_CONCURRENT_FILES));

    let files_stream = stream::iter(root_files);

    files_stream
        .for_each_concurrent(MAX_CONCURRENT_FILES, |file_path| {
            let pb_clone = pb.clone();
            let chunk_indexes_clone = Arc::clone(&chunk_indexes);
            let password_clone = password.clone();
            let key_clone = key.clone();
            let fs_clone = Arc::clone(&fs);
            let new_backup_clone = Arc::clone(&new_backup);
            let root_path_string_clone = root_path_string.clone();
            let written_bytes_clone = Arc::clone(&written_bytes);
            let deduplicated_bytes_clone = Arc::clone(&deduplicated_bytes);
            let semaphore_clone = Arc::clone(&semaphore);
            let files_set_clone = Arc::clone(&files_set);

            async move {
                let _permit = semaphore_clone.acquire().await.expect("Semaphore closed");

                let mut guard = files_set_clone.lock().await;
                guard.spawn(async move {
                    backup_file(
                        file_path,
                        pb_clone,
                        chunk_indexes_clone,
                        password_clone,
                        key_clone,
                        fs_clone,
                        new_backup_clone,
                        root_path_string_clone,
                        written_bytes_clone,
                        deduplicated_bytes_clone,
                        chunk_size,
                        compress,
                    )
                    .await
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

    let backup_file_bytes =
        rmp_serde::to_vec(&*new_backup.lock().unwrap()).unwrap_or_else(|_| Vec::new());

    let compressed_backup_file_bytes = compress_bytes(&backup_file_bytes, compress);

    let backup_file_path = format!("{}/backups/{}", key, new_backup.lock().unwrap().hash);

    let write_backup_file_future = write_file_maybe_encrypt(
        &fs,
        &backup_file_path,
        &compressed_backup_file_bytes,
        password.as_deref(),
    );

    let (write_chunk_index_result, write_backup_file_result) =
        tokio::join!(write_chunk_index_future, write_backup_file_future);

    if write_chunk_index_result.is_err() {
        handle_error("Failed to write chunk indexes".to_string(), Some(&pb));
    }

    if write_backup_file_result.is_err() {
        handle_error("Failed to write backup file".to_string(), Some(&pb));
    }

    {
        let backup_guard = new_backup.lock().unwrap();
        if let Err(e) = add_backup_summary(
            Arc::clone(&fs),
            key.clone(),
            &backup_guard,
            compress,
            password.clone(),
        )
        .await
        {
            handle_error(format!("Failed to save backup summary: {}", e), Some(&pb));
        }
    }

    let written_bytes = *written_bytes.lock().unwrap();
    let deduplicated_bytes = *deduplicated_bytes.lock().unwrap();

    let elapsed = pb.elapsed();
    pb.set_style(ProgressStyle::with_template("{prefix:.green} {msg}").unwrap());
    pb.set_prefix("✓");
    pb.finish_with_message(format!(
        "Backed up files ({:.2?}) - {} written, {} deduplicated",
        elapsed,
        ByteSize(written_bytes),
        ByteSize(deduplicated_bytes),
    ));
}

async fn backup_file(
    file_path: String,
    pb: ProgressBar,
    chunk_indexes: Arc<Mutex<HashMap<String, ChunkIndex>>>,
    password: Option<String>,
    key: String,
    fs: Arc<dyn FS>,
    new_backup: Arc<Mutex<Backup>>,
    root_path_string: String,
    written_bytes: Arc<Mutex<u64>>,
    deduplicated_bytes: Arc<Mutex<u64>>,
    chunk_size: u64,
    compress: i32,
) -> Result<(), String> {
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
            let mut chunk_indexes_guard = chunk_indexes.lock().unwrap();
            let entry = chunk_indexes_guard
                .entry(chunk_hash.clone())
                .or_insert(ChunkIndex { refcount: 0 });
            entry.refcount += 1;

            entry.refcount > 1
        };

        if is_in_chunk_indexes {
            {
                let mut deduplicated_bytes_guard = deduplicated_bytes.lock().unwrap();
                *deduplicated_bytes_guard += chunk_bytes.len() as u64;
            }
            continue;
        }

        let compressed_chunk_bytes = compress_bytes(chunk_bytes, compress);

        let (chunk_hash_prefix, chunk_hash_rest) = chunk_hash.split_at(2);
        let chunk_path = format!("{}/chunks/{}/{}", key, chunk_hash_prefix, chunk_hash_rest);

        let mut last_error = String::new();
        let mut success = false;

        for attempt in 1..=3 {
            match write_file_maybe_encrypt(
                &fs,
                &chunk_path,
                &compressed_chunk_bytes,
                password.as_deref(),
            )
            .await
            {
                Ok(_) => {
                    success = true;
                    break;
                }
                Err(e) => {
                    last_error = format!("Failed to write chunk (attempt {}/3): {}", attempt, e);
                    if attempt < 3 {
                        tokio::time::sleep(Duration::from_millis(100 * attempt as u64)).await;
                    }
                }
            }
        }

        if !success {
            return Err(last_error);
        }

        {
            let mut written_bytes_guard = written_bytes.lock().unwrap();
            *written_bytes_guard += chunk_bytes.len() as u64;
        }
    }

    let file_hash = format!("{:x}", file_hasher.finalize());

    let relative_path = {
        let content = file_path
            .strip_prefix(&root_path_string)
            .unwrap_or(&file_path);

        let mut content = content.replace('\\', "/");

        if content.starts_with('/') {
            content = content[1..].to_string();
        }

        content
    };

    let file_permissions = get_file_permissions_with_path(&file_metadata, &file_path);

    {
        let mut new_backup_guard = new_backup.lock().unwrap();

        new_backup_guard.tree.insert(
            relative_path.to_string(),
            BackupObject {
                hash: file_hash.clone(),
                size: file_metadata.len(),
                content_type: "application/octet-stream".to_string(),
                permissions: file_permissions,
                chunks: file_chunks,
            },
        );
    }

    pb.inc(1);
    Ok(())
}

async fn load_metadata(
    fs: Arc<dyn FS>,
    key: String,
    message: String,
    config: Config,
    root_path_string: String,
    password: Option<String>,
    prev_not_encrypted_but_now_yes: Arc<Mutex<bool>>,
) -> Result<(Backup, Vec<String>, HashMap<String, ChunkIndex>), String> {
    let new_backup = create_new_backup(message, config.author);

    let root_files_future = tokio::spawn(async move { list_files(&root_path_string) });

    let chunk_indexes_future = tokio::spawn(load_chunk_indexes(
        Arc::clone(&fs),
        key.clone(),
        password,
        prev_not_encrypted_but_now_yes,
    ));

    let (root_files_result, chunk_indexes_result) =
        tokio::join!(root_files_future, chunk_indexes_future);

    let root_files = root_files_result.map_err(|e| format!("Failed to list root files: {}", e))?;

    let chunk_indexes = chunk_indexes_result
        .map_err(|e| format!("Failed to load chunk indexes: {}", e))?
        .map_err(|e| format!("Failed to load chunk indexes: {}", e))?;

    Ok((new_backup, root_files, chunk_indexes))
}

fn get_params(
    matches: &ArgMatches,
) -> Result<
    (
        String,
        String,
        String,
        String,
        i32,
        Option<String>,
        u64,
        Vec<String>,
    ),
    String,
> {
    let password: Option<String> = matches
        .get_one::<String>("password")
        .map(|s| s.to_string())
        .map_or_else(
            || get_password(false, false),
            |password| Some(password.to_string()),
        );

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
                .default(0)
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

    let ignore_patterns: Vec<String> = matches
        .get_many::<String>("ignore")
        .map(|values| values.map(|s| s.to_string()).collect())
        .unwrap_or_default();

    Ok((
        key,
        message,
        root_path_string,
        storage,
        compress,
        password,
        chunk_size,
        ignore_patterns,
    ))
}
