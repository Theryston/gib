use crate::core::metadata::{ChunkIndex, Commit, CommitSummary};
use crate::fs::FS;
use crate::utils::{compress_bytes, decompress_bytes, decrypt_bytes, encrypt_bytes, is_encrypted};
use sha2::{Digest, Sha256};
use std::collections::HashMap;
use std::sync::{Arc, Mutex};

pub(crate) async fn load_chunk_indexes(
    fs: Arc<dyn FS>,
    key: String,
    password: Option<String>,
    prev_not_encrypted_but_now_yes: Arc<Mutex<bool>>,
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
                    let mut prev_not_encrypted_guard =
                        prev_not_encrypted_but_now_yes.lock().unwrap();
                    *prev_not_encrypted_guard = true;
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

pub(crate) async fn list_commit_summaries(
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
                        "Backup summaries are encrypted but no password provided".to_string()
                    );
                } else {
                    commit_summaries_bytes
                }
            }
        };

        let decompressed_commit_summaries_bytes =
            decompress_bytes(&decrypted_commit_summaries_bytes);

        rmp_serde::from_slice(&decompressed_commit_summaries_bytes)
            .map_err(|e| format!("Failed to deserialize backup summaries: {}", e))?
    };

    Ok(commit_summaries)
}

pub(crate) async fn create_new_commit(
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

    let new_commit_summary = CommitSummary {
        message: commit.message.clone(),
        hash: commit.hash.clone(),
    };

    let mut commit_sumaries = list_commit_summaries(&fs, &key, password.clone()).await?;

    commit_sumaries.insert(0, new_commit_summary);

    let commit_sumaries_bytes = rmp_serde::to_vec(&commit_sumaries)
        .map_err(|e| format!("Failed to serialize backup summaries: {}", e))?;
    let compressed_commit_sumaries_bytes = compress_bytes(&commit_sumaries_bytes, compress);

    let final_commit_sumaries_bytes = match password {
        Some(password) => encrypt_bytes(&compressed_commit_sumaries_bytes, password.as_bytes())?,
        None => compressed_commit_sumaries_bytes,
    };

    let index_path = format!("{}/indexes/commits", key);
    fs.write_file(&index_path, &final_commit_sumaries_bytes)
        .await
        .map_err(|e| format!("Failed to write backup index: {}", e))?;

    Ok(commit)
}
