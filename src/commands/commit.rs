use crate::commands::config::Config;
use crate::fs::{FS, LocalFS, S3FS, S3FSConfig};
use crate::utils::{get_pwd_string, get_storage, list_files};
use clap::ArgMatches;
use dialoguer::{Input, Select};
use dirs::home_dir;
use indicatif::{ProgressBar, ProgressStyle};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::collections::HashMap;
use std::path::Path;
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

pub fn commit(matches: &ArgMatches) {
    let (key, message, root_path_string, storage) = get_params(matches);

    let pb = ProgressBar::new(100);
    pb.enable_steady_tick(Duration::from_millis(100));
    pb.set_style(ProgressStyle::with_template("{spinner:.green} {msg}").unwrap());

    pb.set_message("Loading commit metadata...");

    let home_dir = home_dir().unwrap();
    let config_path = home_dir.join(".gib").join("config.msgpack");
    let config_bytes = std::fs::read(config_path).unwrap();
    let config: Config = rmp_serde::from_slice(&config_bytes).unwrap();

    let storage = get_storage(&storage);

    let fs: Box<dyn FS> = match storage.storage_type {
        0 => Box::new(LocalFS::new(storage.path.unwrap())),
        1 => Box::new(S3FS::new(S3FSConfig {
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

    let commit_summaries = get_last_commit(&fs, &key);

    let new_commit = create_new_commit(&fs, &key, &message, &config.author, commit_summaries);

    let root_files = list_files(&root_path_string);

    pb.set_message(format!(
        "Committing {} files to {}...",
        root_files.len(),
        new_commit.hash[..8].to_string()
    ));

    println!("{:?}", root_files);

    // TODO: Load the indexes/chunk.msgpack in memory
    // TODO: for each root file:
    // - generate a hash of the file
    // - open the file and read chunks
    // - for each chunk:
    //   - generate a hash of the chunk
    //   - compress the chunk (if needed)
    //   - encrypt the chunk (if needed)
    //   - update in memory the indexes/chunk.msgpack for the chunk hash by incrementing the count by 1
    //   - if the chunk hash is NOT in the indexes/chunk.msgpack in memory:
    //     - save the bytes to the storage
    //   - if the chunk hash is IN the indexes/chunk.msgpack in memory:
    //     - DO NOT save the bytes to the storage (because it already exists)
    // - save the commit file with all the chunk hashes and tree
    // - save the new indexes/chunk.msgpack to the storage

    let elapsed = pb.elapsed();
    pb.set_style(ProgressStyle::with_template("{prefix:.green} {msg}").unwrap());
    pb.set_prefix("✓");
    pb.finish_with_message(format!("Committed files ({:.2?})", elapsed));
}

fn create_new_commit(
    fs: &Box<dyn FS>,
    key: &str,
    message: &str,
    author: &str,
    commit_sumaries: Vec<CommitSummary>,
) -> Commit {
    let last_commit_summary = commit_sumaries.first();

    if last_commit_summary.is_none() {
        let commit_hash = Sha256::digest(format!("{}:{}", message, author).as_bytes());

        return write_commit(
            fs,
            key,
            &Commit {
                message: message.to_string(),
                author: author.to_string(),
                timestamp: std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .unwrap()
                    .as_secs(),
                tree: std::collections::HashMap::new(),
                hash: format!("{:x}", commit_hash),
            },
            &mut commit_sumaries.to_owned(),
        );
    };

    let last_commit_hash = &last_commit_summary.unwrap().hash;
    let commit_path = format!("{}/commits/{}.msgpack", key, last_commit_hash);

    let last_commit_bytes = fs.read_file(&commit_path).unwrap_or_else(|_| Vec::new());

    let last_commit: Commit = if !last_commit_bytes.is_empty() {
        rmp_serde::from_slice(&last_commit_bytes).unwrap()
    } else {
        Commit {
            message: String::new(),
            author: String::new(),
            timestamp: 0,
            tree: std::collections::HashMap::new(),
            hash: String::new(),
        }
    };

    let new_commit = Commit {
        hash: format!(
            "{:x}",
            Sha256::digest(format!("{}:{}", message, author).as_bytes())
        ),
        message: message.to_string(),
        author: author.to_string(),
        timestamp: std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_secs(),
        tree: last_commit.tree,
    };

    write_commit(fs, key, &new_commit, &mut commit_sumaries.to_owned())
}

fn write_commit(
    fs: &Box<dyn FS>,
    key: &str,
    commit: &Commit,
    commit_sumaries: &mut Vec<CommitSummary>,
) -> Commit {
    let commit_bytes = rmp_serde::to_vec(commit).unwrap();

    fs.write_file(
        format!("{}/commits/{}.msgpack", key, commit.hash).as_str(),
        &commit_bytes,
    )
    .unwrap();

    let new_commit_summary = CommitSummary {
        message: commit.message.clone(),
        hash: commit.hash.clone(),
    };

    commit_sumaries.insert(0, new_commit_summary);

    let commit_sumaries_bytes = rmp_serde::to_vec(&commit_sumaries).unwrap();

    fs.write_file(
        format!("{}/indexes/commits.msgpack", key).as_str(),
        &commit_sumaries_bytes,
    )
    .unwrap();

    commit.clone()
}

fn get_last_commit(fs: &Box<dyn FS>, key: &str) -> Vec<CommitSummary> {
    let last_commit = fs
        .read_file(format!("{}/indexes/commits.msgpack", key).as_str())
        .unwrap_or_else(|_| Vec::new());

    let commit_summaries: Vec<CommitSummary> = if last_commit.is_empty() {
        Vec::new()
    } else {
        rmp_serde::from_slice(&last_commit).unwrap()
    };

    commit_summaries
}

fn get_params(matches: &ArgMatches) -> (String, String, String, String) {
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

    let exists = storages_names
        .iter()
        .any(|storage_name| storage_name == &storage);

    if !exists {
        eprintln!("Error: Storage '{}' not found", storage);
        std::process::exit(1);
    }

    (key, message, root_path_string, storage)
}
