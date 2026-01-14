use crate::core::crypto::{read_file_maybe_decrypt, write_file_maybe_encrypt};
use crate::core::metadata::{Backup, BackupSummary, ChunkIndex};
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

pub(crate) async fn list_backup_summaries(
    fs: Arc<dyn FS>,
    key: String,
    password: Option<String>,
) -> Result<Vec<BackupSummary>, String> {
    let read_result = read_file_maybe_decrypt(
        &fs,
        format!("{}/indexes/backups", key).as_str(),
        password.as_deref(),
        "Backup summaries are encrypted but no password provided",
    )
    .await?;

    let backup_summaries: Vec<BackupSummary> = if read_result.bytes.is_empty() {
        Vec::new()
    } else {
        let decompressed_backup_summaries_bytes = decompress_bytes(&read_result.bytes);

        rmp_serde::from_slice(&decompressed_backup_summaries_bytes)
            .map_err(|e| format!("Failed to deserialize backup summaries: {}", e))?
    };

    Ok(backup_summaries)
}

pub(crate) async fn create_new_backup(
    fs: Arc<dyn FS>,
    key: String,
    message: String,
    author: String,
    compress: i32,
    password: Option<String>,
) -> Result<Backup, String> {
    let backup_hash = Sha256::digest(
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

    let backup = Backup {
        message: message.to_string(),
        author: author.to_string(),
        timestamp: std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_secs(),
        tree: std::collections::HashMap::new(),
        hash: format!("{:x}", backup_hash),
    };

    let new_backup_summary = BackupSummary {
        message: backup.message.clone(),
        hash: backup.hash.clone(),
    };

    let mut backup_summaries =
        list_backup_summaries(Arc::clone(&fs), key.clone(), password.clone()).await?;

    backup_summaries.insert(0, new_backup_summary);

    let backup_summaries_bytes = rmp_serde::to_vec(&backup_summaries)
        .map_err(|e| format!("Failed to serialize backup summaries: {}", e))?;
    let compressed_backup_summaries_bytes = compress_bytes(&backup_summaries_bytes, compress);

    let index_path = format!("{}/indexes/backups", key);
    write_file_maybe_encrypt(
        &fs,
        &index_path,
        &compressed_backup_summaries_bytes,
        password.as_deref(),
    )
    .await
    .map_err(|e| format!("Failed to write backup index: {}", e))?;

    Ok(backup)
}
