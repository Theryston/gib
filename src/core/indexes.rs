use crate::core::crypto::{encode_file_bytes, read_file_maybe_decrypt};
use crate::core::metadata::{Backup, BackupSummary, ChunkIndex, RepositoryHead};
use crate::fs::FS;
use crate::utils::{compress_bytes, decompress_bytes};
use crate::utils::{decrypt_bytes, is_encrypted};
use sha2::{Digest, Sha256};
use std::collections::HashMap;
use std::sync::{Arc, Mutex};

const HEAD_FILE_NAME: &str = "HEAD";

#[derive(Debug, Clone)]
pub(crate) struct RepositoryHeadSnapshot {
    pub(crate) head: RepositoryHead,
    pub(crate) version: Option<String>,
}

pub(crate) async fn load_chunk_indexes(
    fs: Arc<dyn FS>,
    key: String,
    password: Option<String>,
    prev_not_encrypted_but_now_yes: Arc<Mutex<bool>>,
) -> Result<HashMap<String, ChunkIndex>, String> {
    Ok(
        load_chunk_indexes_with_version(fs, key, password, prev_not_encrypted_but_now_yes)
            .await?
            .0,
    )
}

pub(crate) async fn load_chunk_indexes_with_version(
    fs: Arc<dyn FS>,
    key: String,
    password: Option<String>,
    prev_not_encrypted_but_now_yes: Arc<Mutex<bool>>,
) -> Result<(HashMap<String, ChunkIndex>, Option<String>), String> {
    let index_path = format!("{}/indexes/chunks", key);
    let (raw_bytes, version) = match fs.read_file_with_version(&index_path).await {
        Ok((bytes, version)) => (bytes, Some(version)),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => (Vec::new(), None),
        Err(error) => return Err(format!("Failed to read chunk indexes: {}", error)),
    };
    let was_encrypted = is_encrypted(&raw_bytes);
    let bytes = if was_encrypted {
        let password = password
            .as_deref()
            .ok_or_else(|| "Chunk indexes are encrypted but no password provided".to_string())?;
        decrypt_bytes(&raw_bytes, password.as_bytes())?
    } else {
        raw_bytes.clone()
    };

    if password.is_some() && !was_encrypted && !bytes.is_empty() {
        let mut prev_not_encrypted_guard = prev_not_encrypted_but_now_yes.lock().unwrap();
        *prev_not_encrypted_guard = true;
    }

    let chunk_indexes = if bytes.is_empty() {
        HashMap::new()
    } else {
        let decompressed_chunk_index_bytes = decompress_bytes(&bytes);
        rmp_serde::from_slice(&decompressed_chunk_index_bytes)
            .map_err(|e| format!("Failed to deserialize chunk indexes: {}", e))?
    };

    Ok((chunk_indexes, version))
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
    if reference.eq_ignore_ascii_case("latest") {
        let head = read_or_initialize_repository_head(fs, key, password).await?;
        return head
            .head
            .backup
            .ok_or_else(|| "No backups found in repository".to_string());
    }

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
        parents: Vec::new(),
        tree: std::collections::HashMap::new(),
        hash: format!("{:x}", backup_hash),
    }
}

fn head_path(key: &str) -> String {
    format!("{}/indexes/{}", key, HEAD_FILE_NAME)
}

fn decode_head(data: &[u8], password: Option<&str>) -> Result<RepositoryHead, String> {
    let data = if is_encrypted(data) {
        let password = password
            .ok_or_else(|| "Repository HEAD is encrypted but no password provided".to_string())?;
        decrypt_bytes(data, password.as_bytes())?
    } else {
        data.to_vec()
    };

    rmp_serde::from_slice(&data)
        .map_err(|e| format!("Failed to deserialize repository HEAD: {}", e))
}

fn encode_head(head: &RepositoryHead, password: Option<&str>) -> Result<Vec<u8>, String> {
    let bytes = rmp_serde::to_vec_named(head)
        .map_err(|e| format!("Failed to serialize repository HEAD: {}", e))?;
    encode_file_bytes(&bytes, password)
}

pub(crate) async fn read_repository_head(
    fs: Arc<dyn FS>,
    key: String,
    password: Option<String>,
) -> Result<Option<RepositoryHeadSnapshot>, String> {
    let path = head_path(&key);
    let (data, version) = match fs.read_file_with_version(&path).await {
        Ok(result) => result,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(format!("Failed to read repository HEAD: {}", error)),
    };

    if data.is_empty() {
        return Ok(None);
    }

    let head = decode_head(&data, password.as_deref())?;
    Ok(Some(RepositoryHeadSnapshot {
        head,
        version: Some(version),
    }))
}

pub(crate) async fn read_or_initialize_repository_head(
    fs: Arc<dyn FS>,
    key: String,
    password: Option<String>,
) -> Result<RepositoryHeadSnapshot, String> {
    if let Some(snapshot) =
        read_repository_head(Arc::clone(&fs), key.clone(), password.clone()).await?
    {
        return Ok(snapshot);
    }

    let backup = list_backup_summaries(Arc::clone(&fs), key.clone(), password.clone())
        .await?
        .first()
        .map(|summary| summary.hash.clone());
    let initial_head = RepositoryHead {
        generation: 0,
        backup,
    };
    let encoded = encode_head(&initial_head, password.as_deref())?;

    match fs
        .write_file_if_version(&head_path(&key), &encoded, None)
        .await
    {
        Ok(()) => {}
        Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {}
        Err(error) => return Err(format!("Failed to initialize repository HEAD: {}", error)),
    }

    read_repository_head(fs, key, password)
        .await?
        .ok_or_else(|| "Repository HEAD disappeared after initialization".to_string())
}

pub(crate) async fn advance_repository_head(
    fs: Arc<dyn FS>,
    key: String,
    password: Option<String>,
    expected_parent: Option<&str>,
    backup_hash: &str,
) -> Result<bool, String> {
    let snapshot =
        read_or_initialize_repository_head(Arc::clone(&fs), key.clone(), password.clone()).await?;

    if snapshot.head.backup.as_deref() == Some(backup_hash) {
        return Ok(true);
    }

    if snapshot.head.backup.as_deref() != expected_parent {
        return Ok(false);
    }

    set_repository_head(fs, key, password, &snapshot, Some(backup_hash)).await
}

pub(crate) async fn set_repository_head(
    fs: Arc<dyn FS>,
    key: String,
    password: Option<String>,
    snapshot: &RepositoryHeadSnapshot,
    backup_hash: Option<&str>,
) -> Result<bool, String> {
    let next_head = RepositoryHead {
        generation: snapshot.head.generation.saturating_add(1),
        backup: backup_hash.map(ToString::to_string),
    };
    let encoded = encode_head(&next_head, password.as_deref())?;

    match fs
        .write_file_if_version(&head_path(&key), &encoded, snapshot.version.as_deref())
        .await
    {
        Ok(()) => Ok(true),
        Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => Ok(false),
        Err(error) => Err(format!("Failed to publish repository HEAD: {}", error)),
    }
}

pub(crate) async fn write_chunk_indexes_with_merge(
    fs: Arc<dyn FS>,
    key: String,
    password: Option<String>,
    compress: i32,
    final_indexes: HashMap<String, ChunkIndex>,
    initial_indexes: HashMap<String, ChunkIndex>,
    initial_version: Option<String>,
) -> Result<(), String> {
    let mut deltas = HashMap::<String, i64>::new();
    let mut keys = std::collections::HashSet::new();
    keys.extend(initial_indexes.keys().cloned());
    keys.extend(final_indexes.keys().cloned());
    for key in keys {
        let initial = initial_indexes.get(&key).map_or(0, |index| index.refcount) as i64;
        let final_value = final_indexes.get(&key).map_or(0, |index| index.refcount) as i64;
        let delta = final_value - initial;
        if delta != 0 {
            deltas.insert(key, delta);
        }
    }

    let index_path = format!("{}/indexes/chunks", key);
    for attempt in 0..5 {
        let current = match fs.read_file_with_version(&index_path).await {
            Ok((data, version)) => {
                let indexes = decode_chunk_indexes(&data, password.as_deref())?;
                Some((indexes, version))
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => None,
            Err(error) => return Err(format!("Failed to read chunk indexes: {}", error)),
        };

        let use_final_indexes = match (&current, &initial_version) {
            (None, None) => true,
            (Some((_, version)), Some(initial_version)) => version == initial_version,
            _ => false,
        };
        let indexes = if use_final_indexes {
            final_indexes.clone()
        } else if attempt == 0 && current.is_none() && initial_version.is_none() {
            final_indexes.clone()
        } else {
            let mut merged = current
                .as_ref()
                .map(|(indexes, _)| indexes.clone())
                .unwrap_or_default();
            for (chunk_hash, delta) in &deltas {
                let current_refcount = merged.get(chunk_hash).map_or(0, |index| index.refcount);
                let next_refcount = (current_refcount as i64 + delta).max(0) as u32;
                if next_refcount == 0 {
                    merged.remove(chunk_hash);
                } else {
                    merged.insert(
                        chunk_hash.clone(),
                        ChunkIndex {
                            refcount: next_refcount,
                        },
                    );
                }
            }
            merged
        };

        let bytes = rmp_serde::to_vec_named(&indexes)
            .map_err(|error| format!("Failed to serialize chunk indexes: {}", error))?;
        let compressed = compress_bytes(&bytes, compress);
        let encoded = encode_file_bytes(&compressed, password.as_deref())?;
        let expected_version = current.as_ref().map(|(_, version)| version.as_str());

        match fs
            .write_file_if_version(&index_path, &encoded, expected_version)
            .await
        {
            Ok(()) => return Ok(()),
            Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => continue,
            Err(error) => return Err(format!("Failed to write chunk indexes: {}", error)),
        }
    }

    Err("Failed to update chunk indexes after several concurrent changes".to_string())
}

fn decode_chunk_indexes(
    raw_bytes: &[u8],
    password: Option<&str>,
) -> Result<HashMap<String, ChunkIndex>, String> {
    if raw_bytes.is_empty() {
        return Ok(HashMap::new());
    }
    let bytes = if is_encrypted(raw_bytes) {
        let password = password
            .ok_or_else(|| "Chunk indexes are encrypted but no password provided".to_string())?;
        decrypt_bytes(raw_bytes, password.as_bytes())?
    } else {
        raw_bytes.to_vec()
    };
    if bytes.is_empty() {
        return Ok(HashMap::new());
    }
    let decompressed = decompress_bytes(&bytes);
    rmp_serde::from_slice(&decompressed)
        .map_err(|error| format!("Failed to deserialize chunk indexes: {}", error))
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

    let index_path = format!("{}/indexes/backups", key);
    for _attempt in 0..5 {
        let current = match fs.read_file_with_version(&index_path).await {
            Ok((data, version)) => Some((data, version)),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => None,
            Err(error) => return Err(format!("Failed to read backup index: {}", error)),
        };

        let mut backup_summaries = match current.as_ref() {
            Some((data, _)) if !data.is_empty() => {
                let decoded = if is_encrypted(data) {
                    let password = password.as_deref().ok_or_else(|| {
                        "Backup summaries are encrypted but no password provided".to_string()
                    })?;
                    decrypt_bytes(data, password.as_bytes())?
                } else {
                    data.clone()
                };
                let decompressed = decompress_bytes(&decoded);
                rmp_serde::from_slice(&decompressed)
                    .map_err(|e| format!("Failed to deserialize backup summaries: {}", e))?
            }
            _ => Vec::new(),
        };

        backup_summaries.insert(0, new_backup_summary.clone());
        let backup_summaries_bytes = rmp_serde::to_vec_named(&backup_summaries)
            .map_err(|e| format!("Failed to serialize backup summaries: {}", e))?;
        let compressed_backup_summaries_bytes = compress_bytes(&backup_summaries_bytes, compress);
        let encoded = encode_file_bytes(&compressed_backup_summaries_bytes, password.as_deref())?;
        let expected_version = current.as_ref().map(|(_, version)| version.as_str());

        match fs
            .write_file_if_version(&index_path, &encoded, expected_version)
            .await
        {
            Ok(()) => return Ok(()),
            Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => continue,
            Err(error) => return Err(format!("Failed to write backup index: {}", error)),
        }
    }

    Err("Failed to update backup index after several concurrent changes".to_string())
}

#[cfg(test)]
mod tests {
    use super::resolve_backup_reference_from_summaries;
    use super::{load_chunk_indexes, write_chunk_indexes_with_merge};
    use crate::core::metadata::{BackupSummary, ChunkIndex};
    use crate::fs::{FS, LocalFS};
    use std::collections::HashMap;
    use std::sync::{Arc, Mutex};
    use std::time::{SystemTime, UNIX_EPOCH};

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

    #[tokio::test]
    async fn composes_concurrent_chunk_index_deltas() {
        let suffix = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let directory = std::env::temp_dir().join(format!("gib-index-test-{suffix}"));
        std::fs::create_dir_all(&directory).unwrap();
        let fs: Arc<dyn FS> = Arc::new(LocalFS::new(&directory));
        let first = HashMap::from([("chunk-a".to_string(), ChunkIndex { refcount: 1 })]);
        let second = HashMap::from([("chunk-b".to_string(), ChunkIndex { refcount: 1 })]);

        write_chunk_indexes_with_merge(
            Arc::clone(&fs),
            "project".to_string(),
            None,
            3,
            first,
            HashMap::new(),
            None,
        )
        .await
        .unwrap();
        write_chunk_indexes_with_merge(
            Arc::clone(&fs),
            "project".to_string(),
            None,
            3,
            second,
            HashMap::new(),
            None,
        )
        .await
        .unwrap();

        let indexes =
            load_chunk_indexes(fs, "project".to_string(), None, Arc::new(Mutex::new(false)))
                .await
                .unwrap();
        assert_eq!(indexes.get("chunk-a").map(|index| index.refcount), Some(1));
        assert_eq!(indexes.get("chunk-b").map(|index| index.refcount), Some(1));

        let _ = std::fs::remove_dir_all(directory);
    }
}
