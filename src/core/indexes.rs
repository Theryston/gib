use crate::core::crypto::{read_file_maybe_decrypt, write_file_maybe_encrypt};
use crate::core::metadata::{ChunkIndex, Commit, CommitSummary};
use crate::fs::FS;
use crate::utils::{compress_bytes, decompress_bytes};
use sha2::{Digest, Sha256};
use std::collections::HashMap;
use std::sync::{Arc, Mutex};

pub(crate) async fn load_chunk_indexes(
    fs: Arc<dyn FS>,
    key: String,
    password: Option<String>,
    prev_not_encrypted_but_now_yes: Arc<Mutex<bool>>,
) -> Result<HashMap<String, ChunkIndex>, String> {
    let read_result = read_file_maybe_decrypt(
        &fs,
        format!("{}/indexes/chunks", key).as_str(),
        password.as_deref(),
        "Chunk indexes are encrypted but no password provided",
    )
    .await?;

    // Handle the case where password was provided but file was not encrypted
    if password.is_some() && !read_result.was_encrypted && !read_result.bytes.is_empty() {
        let mut prev_not_encrypted_guard = prev_not_encrypted_but_now_yes.lock().unwrap();
        *prev_not_encrypted_guard = true;
    }

    let chunk_indexes: HashMap<String, ChunkIndex> = if read_result.bytes.is_empty() {
        HashMap::new()
    } else {
        let decompressed_chunk_index_bytes = decompress_bytes(&read_result.bytes);

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
    let read_result = read_file_maybe_decrypt(
        fs,
        format!("{}/indexes/commits", key).as_str(),
        password.as_deref(),
        "Backup summaries are encrypted but no password provided",
    )
    .await?;

    let commit_summaries: Vec<CommitSummary> = if read_result.bytes.is_empty() {
        Vec::new()
    } else {
        let decompressed_commit_summaries_bytes = decompress_bytes(&read_result.bytes);

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

    let index_path = format!("{}/indexes/commits", key);
    write_file_maybe_encrypt(
        &fs,
        &index_path,
        &compressed_commit_sumaries_bytes,
        password.as_deref(),
    )
    .await
    .map_err(|e| format!("Failed to write backup index: {}", e))?;

    Ok(commit)
}
