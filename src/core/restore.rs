use crate::core::crypto::read_file_maybe_decrypt;
use crate::core::metadata::BackupObject;
use crate::core::permissions::set_file_permissions;
use crate::fs::FS;
use futures::stream::{self, StreamExt};
use sha2::{Digest, Sha256};
use std::io::{Read, Write};
use std::path::Path;
use std::sync::Arc;

const MAX_CONCURRENT_FILES: usize = 100;

pub(crate) type RestoreProgressCallback = Arc<dyn Fn() + Send + Sync>;

#[derive(Debug, Default)]
pub(crate) struct RestoreStats {
    pub(crate) restored: u64,
    pub(crate) skipped: u64,
    pub(crate) failed: Vec<RestoreFailure>,
}

#[derive(Debug)]
pub(crate) struct RestoreFailure {
    pub(crate) path: String,
    pub(crate) message: String,
}

enum RestoreFileOutcome {
    Restored,
    Skipped,
}

pub(crate) async fn restore_files(
    fs: Arc<dyn FS>,
    key: String,
    password: Option<String>,
    target_path: String,
    files: Vec<(String, BackupObject)>,
    progress: Option<RestoreProgressCallback>,
) -> RestoreStats {
    let mut stats = RestoreStats::default();
    let mut results = stream::iter(files.into_iter().map(|(relative_path, backup_object)| {
        let fs = Arc::clone(&fs);
        let key = key.clone();
        let password = password.clone();
        let target_path = target_path.clone();
        let progress = progress.clone();

        async move {
            let result = restore_one_file(
                fs,
                key,
                password,
                target_path,
                relative_path.clone(),
                backup_object,
            )
            .await;
            if let Some(progress) = progress {
                progress();
            }
            (relative_path, result)
        }
    }))
    .buffer_unordered(MAX_CONCURRENT_FILES);

    while let Some((relative_path, result)) = results.next().await {
        match result {
            Ok(RestoreFileOutcome::Restored) => stats.restored += 1,
            Ok(RestoreFileOutcome::Skipped) => stats.skipped += 1,
            Err(message) => stats.failed.push(RestoreFailure {
                path: relative_path,
                message,
            }),
        }
    }

    stats
}

async fn restore_one_file(
    fs: Arc<dyn FS>,
    key: String,
    password: Option<String>,
    target_path: String,
    relative_path: String,
    backup_object: BackupObject,
) -> Result<RestoreFileOutcome, String> {
    let local_path = Path::new(&target_path).join(&relative_path);
    let needs_restore = if local_path.exists() {
        match calculate_file_hash(&local_path) {
            Ok(local_hash) => local_hash != backup_object.hash,
            Err(_) => true,
        }
    } else {
        true
    };

    if !needs_restore {
        return Ok(RestoreFileOutcome::Skipped);
    }

    if let Some(parent) = local_path.parent() {
        std::fs::create_dir_all(parent)
            .map_err(|error| format!("Failed to create parent directory: {}", error))?;
    }

    let mut file = std::fs::File::create(&local_path)
        .map_err(|error| format!("Failed to create file: {}", error))?;

    for chunk_hash in &backup_object.chunks {
        let Some((prefix, rest)) = chunk_hash.split_at_checked(2) else {
            return Err(format!("Invalid chunk hash '{}'", chunk_hash));
        };
        let chunk_path = format!("{}/chunks/{}/{}", key, prefix, rest);

        let chunk_data = read_file_maybe_decrypt(
            &fs,
            &chunk_path,
            password.as_deref(),
            "Chunk is encrypted but no password provided",
        )
        .await
        .map_err(|error| format!("Failed to read chunk {}: {}", chunk_hash, error))?;

        let decompressed = zstd::decode_all(chunk_data.bytes.as_slice())
            .map_err(|error| format!("Failed to decompress chunk {}: {}", chunk_hash, error))?;

        file.write_all(&decompressed)
            .map_err(|error| format!("Failed to write chunk {}: {}", chunk_hash, error))?;
    }

    set_file_permissions(&local_path, backup_object.permissions)
        .map_err(|error| format!("Failed to set file permissions: {}", error))?;

    Ok(RestoreFileOutcome::Restored)
}

fn calculate_file_hash(path: &Path) -> Result<String, std::io::Error> {
    let mut file = std::fs::File::open(path)?;
    let mut hasher = Sha256::new();
    let mut buffer = vec![0u8; 8192];

    loop {
        let bytes_read = file.read(&mut buffer)?;
        if bytes_read == 0 {
            break;
        }
        hasher.update(&buffer[..bytes_read]);
    }

    Ok(format!("{:x}", hasher.finalize()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::fs::{FS, LocalFS};
    use std::path::PathBuf;

    fn test_directory(label: &str) -> PathBuf {
        let suffix = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let path = std::env::temp_dir().join(format!("gib-restore-{label}-{suffix}"));
        std::fs::create_dir_all(&path).unwrap();
        path
    }

    #[tokio::test]
    async fn restores_multiple_files_and_skips_identical_files() {
        let storage_path = test_directory("storage");
        let target_path = test_directory("target");
        let fs: Arc<dyn FS> = Arc::new(LocalFS::new(&storage_path));
        let key = "project";
        let first_data: &[u8] = b"first file";
        let second_data: &[u8] = b"second file";
        let first_hash = format!("{:x}", Sha256::digest(first_data));
        let second_hash = format!("{:x}", Sha256::digest(second_data));
        let first_chunk = "aa1111";
        let second_chunk = "bb2222";

        for (chunk, data) in [(first_chunk, first_data), (second_chunk, second_data)] {
            let encoded = zstd::encode_all(data, 3).unwrap();
            fs.write_file(
                &format!("{key}/chunks/{}/{}", &chunk[..2], &chunk[2..]),
                &encoded,
            )
            .await
            .unwrap();
        }

        let files = vec![
            (
                "nested/first.txt".to_string(),
                BackupObject {
                    hash: first_hash,
                    size: first_data.len() as u64,
                    content_type: "text/plain".to_string(),
                    permissions: 0o644,
                    chunks: vec![first_chunk.to_string()],
                },
            ),
            (
                "second.txt".to_string(),
                BackupObject {
                    hash: second_hash,
                    size: second_data.len() as u64,
                    content_type: "text/plain".to_string(),
                    permissions: 0o600,
                    chunks: vec![second_chunk.to_string()],
                },
            ),
        ];
        let stats = restore_files(
            Arc::clone(&fs),
            key.to_string(),
            None,
            target_path.to_string_lossy().to_string(),
            files.clone(),
            None,
        )
        .await;
        assert_eq!(stats.restored, 2);
        assert!(stats.failed.is_empty());
        assert_eq!(
            std::fs::read(target_path.join("nested/first.txt")).unwrap(),
            first_data
        );
        assert_eq!(
            std::fs::read(target_path.join("second.txt")).unwrap(),
            second_data
        );

        let second_stats = restore_files(
            fs,
            key.to_string(),
            None,
            target_path.to_string_lossy().to_string(),
            files,
            None,
        )
        .await;
        assert_eq!(second_stats.skipped, 2);
        assert_eq!(second_stats.restored, 0);

        let _ = std::fs::remove_dir_all(storage_path);
        let _ = std::fs::remove_dir_all(target_path);
    }
}
