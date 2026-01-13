use crate::commands::config::Config;
use crate::fs::{FS, LocalFS, S3FS, S3FSConfig};
use crate::utils::{
    compress_bytes, decompress_bytes, decrypt_bytes, encrypt_bytes, get_pwd_string, get_storage,
    handle_error, is_encrypted, list_files,
};
use clap::ArgMatches;
use dialoguer::{Input, Select};
use dirs::home_dir;
use indicatif::{ProgressBar, ProgressStyle};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::collections::HashMap;
use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

#[derive(Debug, PartialEq, Deserialize, Serialize, Clone)]
struct CommitSummary {
    message: String,
    hash: String,
}

#[derive(Debug, PartialEq, Deserialize, Serialize, Clone)]
struct Commit {
    message: String,
    hash: String,
    timestamp: u64,
    author: String,
    tree: HashMap<String, CommitObject>,
}

#[derive(Debug, PartialEq, Deserialize, Serialize, Clone)]
struct CommitObject {
    hash: String,
    size: u64,
    content_type: String,
    permissions: u32,
    chunks: Vec<String>,
}

#[derive(Debug, PartialEq, Deserialize, Serialize, Clone)]
struct ChunkIndex {
    refcount: u32,
}

pub async fn commit(matches: &ArgMatches) {
    let (key, message, root_path_string, storage, compress, password) = match get_params(matches) {
        Ok(params) => params,
        Err(e) => handle_error(e, None),
    };

    let pb = ProgressBar::new(100);

    pb.enable_steady_tick(Duration::from_millis(100));
    pb.set_style(ProgressStyle::with_template("{spinner:.green} {msg}").unwrap());

    pb.set_message("Loading commit metadata...");

    let home_dir = match home_dir() {
        Some(dir) => dir,
        None => handle_error("Failed to get home directory".to_string(), Some(&pb)),
    };
    let config_path = home_dir.join(".gib").join("config.msgpack");
    let config_bytes = match std::fs::read(&config_path) {
        Ok(bytes) => bytes,
        Err(e) => handle_error(format!("Failed to read config file: {}", e), Some(&pb)),
    };
    let config: Config = match rmp_serde::from_slice(&config_bytes) {
        Ok(config) => config,
        Err(e) => handle_error(format!("Failed to deserialize config: {}", e), Some(&pb)),
    };

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

    pb.set_message("Generating new commit...");

    let (new_commit, root_files, file_indexes, chunk_indexes) = match load_metadata(
        Arc::clone(&fs),
        key,
        message,
        config,
        root_path_string,
        compress,
        password,
    )
    .await
    {
        Ok(result) => result,
        Err(e) => handle_error(e, Some(&pb)),
    };

    pb.set_message(format!(
        "Committing {} files to {}...",
        root_files.len(),
        new_commit.hash[..8].to_string()
    ));

    println!("{:?}", root_files);
    println!("{:?}", file_indexes);
    println!("{:?}", chunk_indexes);

    // TODO: Load the indexes/chunks in memory
    // TODO: for each root file:
    // - generate a hash of the file
    // - open the file and read chunks
    // - for each chunk:
    //   - generate a hash of the chunk
    //   - update in memory the indexes/chunks for the chunk hash by incrementing the count by 1
    //   - if the chunk hash is NOT in the indexes/chunks in memory:
    //      - compress the chunk (if needed)
    //      - encrypt the chunk (if needed)
    //      - save the bytes to the storage
    // - save the commit file with all the chunk hashes and tree
    // - save the new indexes/chunks to the storage

    let elapsed = pb.elapsed();
    pb.set_style(ProgressStyle::with_template("{prefix:.green} {msg}").unwrap());
    pb.set_prefix("✓");
    pb.finish_with_message(format!("Committed files ({:.2?})", elapsed));
}

async fn load_metadata(
    fs: Arc<dyn FS>,
    key: String,
    message: String,
    config: Config,
    root_path_string: String,
    compress: i32,
    password: Option<String>,
) -> Result<
    (
        Commit,
        Vec<String>,
        HashMap<String, Vec<String>>,
        HashMap<String, ChunkIndex>,
    ),
    String,
> {
    let new_commit_future = tokio::spawn(create_new_commit(
        Arc::clone(&fs),
        key.clone(),
        message.clone(),
        config.author.clone(),
        compress.clone(),
        password.clone(),
    ));

    let root_files_future = tokio::spawn(async move { list_files(&root_path_string) });

    let file_indexes_future = tokio::spawn(load_file_indexes(
        Arc::clone(&fs),
        key.clone(),
        password.clone(),
    ));

    let chunk_indexes_future =
        tokio::spawn(load_chunk_indexes(Arc::clone(&fs), key.clone(), password));

    let (new_commit_result, root_files_result, file_indexes_result, chunk_indexes_result) = tokio::join!(
        new_commit_future,
        root_files_future,
        file_indexes_future,
        chunk_indexes_future
    );

    let new_commit = new_commit_result
        .map_err(|e| format!("Failed to create new commit: {}", e))?
        .map_err(|e| format!("Failed to create new commit: {}", e))?;

    let root_files = root_files_result.map_err(|e| format!("Failed to list root files: {}", e))?;

    let file_indexes = file_indexes_result
        .map_err(|e| format!("Failed to load file indexes: {}", e))?
        .map_err(|e| format!("Failed to load file indexes: {}", e))?;

    let chunk_indexes = chunk_indexes_result
        .map_err(|e| format!("Failed to load chunk indexes: {}", e))?
        .map_err(|e| format!("Failed to load chunk indexes: {}", e))?;

    Ok((new_commit, root_files, file_indexes, chunk_indexes))
}

async fn load_chunk_indexes(
    fs: Arc<dyn FS>,
    key: String,
    password: Option<String>,
) -> Result<HashMap<String, ChunkIndex>, String> {
    let chunk_index_bytes = fs
        .read_file(format!("{}/indexes/chunks", key).as_str())
        .await
        .unwrap_or_else(|_| Vec::new());

    let chunk_indexes: HashMap<String, ChunkIndex> = if chunk_index_bytes.is_empty() {
        HashMap::new()
    } else {
        let is_encrypted = is_encrypted(&chunk_index_bytes);

        let decrypted_chunk_index_bytes = match password {
            Some(password) => {
                if is_encrypted {
                    decrypt_bytes(&chunk_index_bytes, password.as_bytes())?
                } else {
                    chunk_index_bytes
                }
            }
            None => {
                if is_encrypted {
                    return Err("Chunk indexes are encrypted but no password provided".to_string());
                } else {
                    chunk_index_bytes
                }
            }
        };

        let decompressed_chunk_index_bytes = decompress_bytes(&decrypted_chunk_index_bytes);

        rmp_serde::from_slice(&decompressed_chunk_index_bytes)
            .map_err(|e| format!("Failed to deserialize chunk indexes: {}", e))?
    };

    Ok(chunk_indexes)
}

async fn load_file_indexes(
    fs: Arc<dyn FS>,
    key: String,
    password: Option<String>,
) -> Result<HashMap<String, Vec<String>>, String> {
    let file_index_bytes = fs
        .read_file(format!("{}/indexes/files", key).as_str())
        .await
        .unwrap_or_else(|_| Vec::new());

    let file_indexes: HashMap<String, Vec<String>> = if file_index_bytes.is_empty() {
        HashMap::new()
    } else {
        let is_encrypted = is_encrypted(&file_index_bytes);

        let decrypted_file_index_bytes = match password {
            Some(password) => {
                if is_encrypted {
                    decrypt_bytes(&file_index_bytes, password.as_bytes())?
                } else {
                    file_index_bytes
                }
            }
            None => {
                if is_encrypted {
                    return Err("File indexes are encrypted but no password provided".to_string());
                } else {
                    file_index_bytes
                }
            }
        };

        let decompressed_file_index_bytes = decompress_bytes(&decrypted_file_index_bytes);

        rmp_serde::from_slice(&decompressed_file_index_bytes)
            .map_err(|e| format!("Failed to deserialize file indexes: {}", e))?
    };

    Ok(file_indexes)
}

async fn create_new_commit(
    fs: Arc<dyn FS>,
    key: String,
    message: String,
    author: String,
    compress: i32,
    password: Option<String>,
) -> Result<Commit, String> {
    let commit_hash = Sha256::digest(
        format!(
            "{}:{}:{}",
            message,
            author,
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_secs()
        )
        .as_bytes(),
    );

    let commit = Commit {
        message: message.to_string(),
        author: author.to_string(),
        timestamp: std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_secs(),
        tree: std::collections::HashMap::new(),
        hash: format!("{:x}", commit_hash),
    };

    let commit_bytes =
        rmp_serde::to_vec(&commit).map_err(|e| format!("Failed to serialize commit: {}", e))?;
    let compressed_commit_bytes = compress_bytes(&commit_bytes, compress);

    let final_commit_bytes = match &password {
        Some(password) => encrypt_bytes(&compressed_commit_bytes, password.as_bytes())?,
        None => compressed_commit_bytes,
    };

    let commit_path = format!("{}/commits/{}", key, commit.hash);
    let write_commit_future = fs.write_file(&commit_path, &final_commit_bytes);

    let new_commit_summary = CommitSummary {
        message: commit.message.clone(),
        hash: commit.hash.clone(),
    };

    let mut commit_sumaries = list_commit_summaries(&fs, &key, password.clone()).await?;

    commit_sumaries.insert(0, new_commit_summary);

    let commit_sumaries_bytes = rmp_serde::to_vec(&commit_sumaries)
        .map_err(|e| format!("Failed to serialize commit summaries: {}", e))?;
    let compressed_commit_sumaries_bytes = compress_bytes(&commit_sumaries_bytes, compress);

    let final_commit_sumaries_bytes = match password {
        Some(password) => encrypt_bytes(&compressed_commit_sumaries_bytes, password.as_bytes())?,
        None => compressed_commit_sumaries_bytes,
    };

    let index_path = format!("{}/indexes/commits", key);
    let write_index_future = fs.write_file(&index_path, &final_commit_sumaries_bytes);

    let (write_commit_result, write_index_result) =
        tokio::join!(write_commit_future, write_index_future);

    if write_commit_result.is_err() || write_index_result.is_err() {
        return Err("Failed to write commit or index".to_string());
    }

    Ok(commit)
}

async fn list_commit_summaries(
    fs: &Arc<dyn FS>,
    key: &String,
    password: Option<String>,
) -> Result<Vec<CommitSummary>, String> {
    let commit_summaries_bytes = fs
        .read_file(format!("{}/indexes/commits", key).as_str())
        .await
        .unwrap_or_else(|_| Vec::new());

    let commit_summaries: Vec<CommitSummary> = if commit_summaries_bytes.is_empty() {
        Vec::new()
    } else {
        let is_encrypted = is_encrypted(&commit_summaries_bytes);

        let decrypted_commit_summaries_bytes = match password {
            Some(password) => {
                if is_encrypted {
                    decrypt_bytes(&commit_summaries_bytes, password.as_bytes())?
                } else {
                    commit_summaries_bytes
                }
            }
            None => {
                if is_encrypted {
                    return Err(
                        "Commit summaries are encrypted but no password provided".to_string()
                    );
                } else {
                    commit_summaries_bytes
                }
            }
        };

        let decompressed_commit_summaries_bytes =
            decompress_bytes(&decrypted_commit_summaries_bytes);

        rmp_serde::from_slice(&decompressed_commit_summaries_bytes)
            .map_err(|e| format!("Failed to deserialize commit summaries: {}", e))?
    };

    Ok(commit_summaries)
}

fn get_params(
    matches: &ArgMatches,
) -> Result<(String, String, String, String, i32, Option<String>), String> {
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
            .with_prompt("Enter the commit message")
            .interact_text()
            .map_err(|e| format!("{}", e))?,
    };

    let home_dir = home_dir().unwrap();
    let storage_path = home_dir.join(".gib").join("storages");

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

    let password = matches.get_one::<String>("password").map(|s| s.to_string());

    let compress: i32 = matches
        .get_one::<String>("compress")
        .map_or_else(|| 3, |compress| compress.parse().unwrap());

    let exists = storages_names
        .iter()
        .any(|storage_name| storage_name == &storage);

    if !exists {
        return Err(format!("Storage '{}' not found", storage));
    }

    Ok((key, message, root_path_string, storage, compress, password))
}
