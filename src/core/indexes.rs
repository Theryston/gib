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

pub(crate) async fn resolve_backup_reference(
    fs: Arc<dyn FS>,
    key: String,
    password: Option<String>,
    reference: &str,
) -> Result<String, String> {
    let summaries = list_backup_summaries(fs, key, password).await?;
    resolve_backup_reference_from_summaries(&summaries, reference)
}

fn resolve_backup_reference_from_summaries(
    summaries: &[BackupSummary],
    reference: &str,
) -> Result<String, String> {
    if reference.eq_ignore_ascii_case("latest") {
        return summaries
            .first()
            .map(|summary| summary.hash.clone())
            .ok_or_else(|| "No backups found in repository".to_string());
    }

    if reference.len() <= 8 {
        return summaries
            .iter()
            .find(|summary| summary.hash.starts_with(reference))
            .map(|summary| summary.hash.clone())
            .ok_or_else(|| format!("No backup found matching hash prefix: {}", reference));
    }

    Ok(reference.to_string())
}

pub(crate) fn create_new_backup(message: String, author: String) -> Backup {
    let now = std::time::SystemTime::now();
    let timestamp = now
        .duration_since(std::time::UNIX_EPOCH)
        .expect("system clock is before the Unix epoch");
    let backup_hash =
        Sha256::digest(format!("{}:{}:{}", message, author, timestamp.as_nanos()).as_bytes());

    Backup {
        message: message.to_string(),
        author: author.to_string(),
        timestamp: timestamp.as_secs(),
        tree: std::collections::HashMap::new(),
        hash: format!("{:x}", backup_hash),
    }
}

pub(crate) async fn add_backup_summary(
    fs: Arc<dyn FS>,
    key: String,
    backup: &Backup,
    compress: i32,
    password: Option<String>,
    written_bytes: &u64,
) -> Result<(), String> {
    let new_backup_summary = BackupSummary {
        message: backup.message.clone(),
        hash: backup.hash.clone(),
        timestamp: Some(backup.timestamp),
        size: Some(*written_bytes),
    };

    let mut backup_summaries =
        list_backup_summaries(Arc::clone(&fs), key.clone(), password.clone()).await?;

    backup_summaries.insert(0, new_backup_summary);

    let backup_summaries_bytes = rmp_serde::to_vec_named(&backup_summaries)
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

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::resolve_backup_reference_from_summaries;
    use crate::core::metadata::BackupSummary;

    fn summary(hash: &str) -> BackupSummary {
        BackupSummary {
            message: String::new(),
            hash: hash.to_string(),
            timestamp: None,
            size: None,
        }
    }

    #[test]
    fn resolves_latest_to_the_newest_backup() {
        let summaries = vec![summary("newest-backup"), summary("older-backup")];

        assert_eq!(
            resolve_backup_reference_from_summaries(&summaries, "latest").unwrap(),
            "newest-backup"
        );
    }

    #[test]
    fn resolves_latest_case_insensitively() {
        let summaries = vec![summary("newest-backup")];

        assert_eq!(
            resolve_backup_reference_from_summaries(&summaries, "LATEST").unwrap(),
            "newest-backup"
        );
    }

    #[test]
    fn keeps_prefix_resolution() {
        let summaries = vec![summary("abcdef123456")];

        assert_eq!(
            resolve_backup_reference_from_summaries(&summaries, "abcdef12").unwrap(),
            "abcdef123456"
        );
    }

    #[test]
    fn rejects_latest_when_repository_is_empty() {
        let error = resolve_backup_reference_from_summaries(&[], "latest").unwrap_err();

        assert_eq!(error, "No backups found in repository");
    }
}
