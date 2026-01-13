use crate::commands::config::Config;
use crate::fs::{FS, LocalFS, S3FS, S3FSConfig};
use crate::utils::{
    compress_bytes, decompress_bytes, decrypt_bytes, encrypt_bytes, get_pwd_string, get_storage,
    is_encrypted, list_files,
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
    let (key, message, root_path_string, storage, compress, password) = get_params(matches);

    let pb = ProgressBar::new(100);

    pb.enable_steady_tick(Duration::from_millis(100));
    pb.set_style(ProgressStyle::with_template("{spinner:.green} {msg}").unwrap());

    pb.set_message("Loading commit metadata...");

    let home_dir = home_dir().unwrap();
    let config_path = home_dir.join(".gib").join("config.msgpack");
    let config_bytes = std::fs::read(config_path).unwrap();
    let config: Config = rmp_serde::from_slice(&config_bytes).unwrap();

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
        _ => {
            eprintln!("Error: Invalid storage type");
            std::process::exit(1);
        }
    };

    pb.set_message("Generating new commit...");

    let (new_commit, root_files, file_indexes, chunk_indexes) = load_metadata(
        Arc::clone(&fs),
        key,
        message,
        config,
        root_path_string,
        compress,
        password,
    )
    .await;

    pb.set_message(format!(
        "Committing {} files to {}...",
        root_files.len(),
        new_commit.hash[..8].to_string()
    ));

    println!("{:?}", root_files);
    println!("{:?}", file_indexes);
    println!("{:?}", chunk_indexes);

    // TODO: Load the indexes/chunk.msgpack in memory
    // TODO: for each root file:
    // - generate a hash of the file
    // - open the file and read chunks
    // - for each chunk:
    //   - generate a hash of the chunk
    //   - update in memory the indexes/chunk.msgpack for the chunk hash by incrementing the count by 1
    //   - if the chunk hash is NOT in the indexes/chunk.msgpack in memory:
    //      - compress the chunk (if needed)
    //      - encrypt the chunk (if needed)
    //      - save the bytes to the storage
    // - save the commit file with all the chunk hashes and tree
    // - save the new indexes/chunk.msgpack to the storage

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
) -> (
    Commit,
    Vec<String>,
    HashMap<String, Vec<String>>,
    HashMap<String, ChunkIndex>,
) {
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

    let new_commit = match new_commit_result {
        Ok(commit) => commit,
        Err(e) => {
            eprintln!("Failed to create new commit: {e}");
            std::process::exit(1);
        }
    };

    let root_files = match root_files_result {
        Ok(files) => files,
        Err(e) => {
            eprintln!("Failed to list root files: {e}");
            std::process::exit(1);
        }
    };

    let file_indexes = match file_indexes_result {
        Ok(indexes) => indexes,
        Err(e) => {
            eprintln!("Failed to load file indexes: {e}");
            std::process::exit(1);
        }
    };

    let chunk_indexes = match chunk_indexes_result {
        Ok(indexes) => indexes,
        Err(e) => {
            eprintln!("Failed to load chunk indexes: {e}");
            std::process::exit(1);
        }
    };

    (new_commit, root_files, file_indexes, chunk_indexes)
}

async fn load_chunk_indexes(
    fs: Arc<dyn FS>,
    key: String,
    password: Option<String>,
) -> HashMap<String, ChunkIndex> {
    let chunk_index_bytes = fs
        .read_file(format!("{}/indexes/chunks.msgpack", key).as_str())
        .await
        .unwrap_or_else(|_| Vec::new());

    let chunk_indexes: HashMap<String, ChunkIndex> = if chunk_index_bytes.is_empty() {
        HashMap::new()
    } else {
        let is_encrypted = is_encrypted(&chunk_index_bytes);

        let decrypted_chunk_index_bytes = match password {
            Some(password) => {
                if is_encrypted {
                    decrypt_bytes(&chunk_index_bytes, password.as_bytes()).unwrap()
                } else {
                    chunk_index_bytes
                }
            }
            None => {
                if is_encrypted {
                    eprintln!("Chunk indexes are encrypted but no password provided");
                    std::process::exit(1);
                } else {
                    chunk_index_bytes
                }
            }
        };

        let decompressed_chunk_index_bytes = decompress_bytes(&decrypted_chunk_index_bytes);

        rmp_serde::from_slice(&decompressed_chunk_index_bytes).unwrap()
    };

    chunk_indexes
}

async fn load_file_indexes(
    fs: Arc<dyn FS>,
    key: String,
    password: Option<String>,
) -> HashMap<String, Vec<String>> {
    let file_index_bytes = fs
        .read_file(format!("{}/indexes/files.msgpack", key).as_str())
        .await
        .unwrap_or_else(|_| Vec::new());

    let file_indexes: HashMap<String, Vec<String>> = if file_index_bytes.is_empty() {
        HashMap::new()
    } else {
        let is_encrypted = is_encrypted(&file_index_bytes);

        let decrypted_file_index_bytes = match password {
            Some(password) => {
                if is_encrypted {
                    decrypt_bytes(&file_index_bytes, password.as_bytes()).unwrap()
                } else {
                    file_index_bytes
                }
            }
            None => {
                if is_encrypted {
                    eprintln!("File indexes are encrypted but no password provided");
                    std::process::exit(1);
                } else {
                    file_index_bytes
                }
            }
        };

        let decompressed_file_index_bytes = decompress_bytes(&decrypted_file_index_bytes);

        rmp_serde::from_slice(&decompressed_file_index_bytes).unwrap()
    };

    file_indexes
}

async fn create_new_commit(
    fs: Arc<dyn FS>,
    key: String,
    message: String,
    author: String,
    compress: i32,
    password: Option<String>,
) -> Commit {
    let commit_hash = Sha256::digest(format!("{}:{}", message, author).as_bytes());

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

    let commit_bytes = rmp_serde::to_vec(&commit).unwrap();
    let compressed_commit_bytes = compress_bytes(&commit_bytes, compress);

    let final_commit_bytes = match &password {
        Some(password) => encrypt_bytes(&compressed_commit_bytes, password.as_bytes()),
        None => compressed_commit_bytes,
    };

    let commit_path = format!("{}/commits/{}.msgpack", key, commit.hash);
    let write_commit_future = fs.write_file(&commit_path, &final_commit_bytes);

    let new_commit_summary = CommitSummary {
        message: commit.message.clone(),
        hash: commit.hash.clone(),
    };

    let mut commit_sumaries = list_commit_summaries(&fs, &key, password.clone()).await;

    commit_sumaries.insert(0, new_commit_summary);

    let commit_sumaries_bytes = rmp_serde::to_vec(&commit_sumaries).unwrap();
    let compressed_commit_sumaries_bytes = compress_bytes(&commit_sumaries_bytes, compress);

    let final_commit_sumaries_bytes = match password {
        Some(password) => encrypt_bytes(&compressed_commit_sumaries_bytes, password.as_bytes()),
        None => compressed_commit_sumaries_bytes,
    };

    let index_path = format!("{}/indexes/commits.msgpack", key);
    let write_index_future = fs.write_file(&index_path, &final_commit_sumaries_bytes);

    let (write_commit_result, write_index_result) =
        tokio::join!(write_commit_future, write_index_future);

    if write_commit_result.is_err() || write_index_result.is_err() {
        eprintln!("Error: Failed to write commit or index");
        std::process::exit(1);
    }

    commit
}

async fn list_commit_summaries(
    fs: &Arc<dyn FS>,
    key: &String,
    password: Option<String>,
) -> Vec<CommitSummary> {
    let commit_summaries_bytes = fs
        .read_file(format!("{}/indexes/commits.msgpack", key).as_str())
        .await
        .unwrap_or_else(|_| Vec::new());

    let commit_summaries: Vec<CommitSummary> = if commit_summaries_bytes.is_empty() {
        Vec::new()
    } else {
        let is_encrypted = is_encrypted(&commit_summaries_bytes);

        let decrypted_commit_summaries_bytes = match password {
            Some(password) => {
                if is_encrypted {
                    decrypt_bytes(&commit_summaries_bytes, password.as_bytes()).unwrap()
                } else {
                    commit_summaries_bytes
                }
            }
            None => {
                if is_encrypted {
                    eprintln!("Commit summaries are encrypted but no password provided");
                    std::process::exit(1);
                } else {
                    commit_summaries_bytes
                }
            }
        };

        let decompressed_commit_summaries_bytes =
            decompress_bytes(&decrypted_commit_summaries_bytes);

        rmp_serde::from_slice(&decompressed_commit_summaries_bytes).unwrap()
    };

    commit_summaries
}

fn get_params(matches: &ArgMatches) -> (String, String, String, String, i32, Option<String>) {
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

    let message = matches.get_one::<String>("message").map_or_else(
        || {
            let typed_message: String = Input::<String>::new()
                .with_prompt("Enter the commit message")
                .interact_text()
                .unwrap_or_else(|e| {
                    eprintln!("Error: {}", e);
                    std::process::exit(1);
                });
            typed_message
        },
        |message| message.to_string(),
    );

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

    let storage = matches.get_one::<String>("storage").map_or_else(
        || {
            let selected_index = Select::new()
                .with_prompt("Select the storage to use")
                .items(storages_names)
                .interact()
                .unwrap_or_else(|e| {
                    eprintln!("Error: {}", e);
                    std::process::exit(1);
                });

            let selected_storage = &storages_names[selected_index];

            selected_storage.to_string()
        },
        |storage| storage.to_string(),
    );

    let password = matches.get_one::<String>("password").map(|s| s.to_string());

    let compress: i32 = matches
        .get_one::<String>("compress")
        .map_or_else(|| 3, |compress| compress.parse().unwrap());

    let exists = storages_names
        .iter()
        .any(|storage_name| storage_name == &storage);

    if !exists {
        eprintln!("Error: Storage '{}' not found", storage);
        std::process::exit(1);
    }

    (key, message, root_path_string, storage, compress, password)
}
