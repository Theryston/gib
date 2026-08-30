use super::catalog::index_backup_after_finalize;
use super::crypto::{encode_file_bytes, read_file_maybe_decrypt, write_file_maybe_encrypt};
use super::indexes::{
    add_backup_summary, advance_repository_head, create_new_backup,
    load_chunk_indexes_with_version, read_or_initialize_repository_head, set_repository_head,
    write_chunk_indexes_with_merge,
};
use super::metadata::{Backup, BackupObject, ChunkIndex, PendingBackup};
use super::permissions::get_file_permissions_with_path;
use crate::storage::FS;
use crate::utils::{compress_bytes, decompress_bytes};
use sha2::{Digest, Sha256};
use std::collections::{HashMap, HashSet};
use std::io::Read;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

#[derive(Clone)]
pub(crate) struct BackupInput {
    pub(crate) key: String,
    pub(crate) root: PathBuf,
    pub(crate) fs: Arc<dyn FS>,
    pub(crate) author: String,
    pub(crate) message: String,
    pub(crate) password: Option<String>,
    pub(crate) compression: i32,
    pub(crate) chunk_size: u64,
    pub(crate) ignore_patterns: Vec<String>,
    pub(crate) include_git: bool,
    pub(crate) concurrency: usize,
    pub(crate) parent: Option<String>,
    pub(crate) resume: Option<String>,
}

#[derive(Clone, Debug)]
pub(crate) struct CoreProgress {
    pub(crate) phase: String,
    pub(crate) processed: u64,
    pub(crate) total: Option<u64>,
    pub(crate) message: Option<String>,
}

#[derive(Clone, Debug)]
pub(crate) struct CoreWarning {
    pub(crate) code: String,
    pub(crate) message: String,
}

#[derive(Clone, Debug)]
pub(crate) struct BackupOutcome {
    pub(crate) backup: Backup,
    pub(crate) files_total: usize,
    pub(crate) written_bytes: u64,
    pub(crate) deduplicated_bytes: u64,
    pub(crate) elapsed_ms: u64,
    pub(crate) head_published: bool,
    pub(crate) warnings: Vec<CoreWarning>,
}

pub(crate) async fn run(
    input: BackupInput,
    progress: Option<Arc<dyn Fn(CoreProgress) + Send + Sync + 'static>>,
) -> Result<BackupOutcome, String> {
    validate_input(&input)?;
    let started = std::time::Instant::now();
    let BackupInput {
        key,
        root,
        fs,
        author,
        message,
        password,
        mut compression,
        mut chunk_size,
        mut ignore_patterns,
        include_git,
        mut concurrency,
        mut parent,
        resume,
    } = input;
    let mut warnings = Vec::new();

    report(
        &progress,
        "metadata",
        0,
        None,
        Some("Loading repository metadata...".to_string()),
    );
    let previous_backup_was_unencrypted = Arc::new(Mutex::new(false));
    let (mut chunk_indexes, initial_version) = load_chunk_indexes_with_version(
        Arc::clone(&fs),
        key.clone(),
        password.clone(),
        Arc::clone(&previous_backup_was_unencrypted),
    )
    .await?;
    if *previous_backup_was_unencrypted
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
    {
        warnings.push(CoreWarning {
            code: "unencrypted_chunks".to_string(),
            message: "The backup was not encrypted but you provided a password. Only new chunks will be encrypted; run 'gib encrypt' to encrypt existing chunks.".to_string(),
        });
    }
    let initial_chunk_indexes = chunk_indexes.clone();

    let mut backup = create_new_backup(message, author);

    let pending_path = format!("{key}/indexes/pending_{}", backup.hash);
    let mut pending = PendingBackup {
        message: backup.message.clone(),
        compress: compression,
        chunk_size,
        ignore_patterns: ignore_patterns.clone(),
        concurrency,
        parent: parent.clone(),
        processed_chunks: Vec::new(),
    };
    if let Some(resume_prefix) = resume.as_deref() {
        let Some((resumed, resumed_path)) =
            load_pending(&fs, &key, resume_prefix, password.as_deref()).await?
        else {
            return Err(format!("No pending backup found for '{resume_prefix}'"));
        };
        if resumed.compress != compression {
            warnings.push(CoreWarning {
                code: "pending_backup_reuse".to_string(),
                message: format!(
                    "Reusing pending backup compression level {} instead of requested level {}",
                    resumed.compress, compression
                ),
            });
        }
        if resumed.chunk_size != chunk_size {
            warnings.push(CoreWarning {
                code: "pending_backup_reuse".to_string(),
                message: format!(
                    "Reusing pending backup chunk size {} instead of requested size {}",
                    resumed.chunk_size, chunk_size
                ),
            });
        }
        if resumed.concurrency != concurrency {
            warnings.push(CoreWarning {
                code: "pending_backup_reuse".to_string(),
                message: format!(
                    "Reusing pending backup concurrency {} instead of requested value {}",
                    resumed.concurrency, concurrency
                ),
            });
        }
        if resumed.ignore_patterns != ignore_patterns {
            warnings.push(CoreWarning {
                code: "pending_backup_reuse".to_string(),
                message: "Reusing pending backup ignore rules".to_string(),
            });
        }
        compression = resumed.compress;
        chunk_size = resumed.chunk_size;
        ignore_patterns = resumed.ignore_patterns.clone();
        concurrency = resumed.concurrency;
        if parent.is_none() {
            parent = resumed.parent.clone();
        }
        pending = resumed;
        pending.compress = compression;
        pending.chunk_size = chunk_size;
        pending.ignore_patterns = ignore_patterns.clone();
        pending.concurrency = concurrency;
        pending.parent = parent.clone();
        if backup.message.is_empty() {
            backup.message = pending.message.clone();
        }
        let _ = fs.delete_file(&resumed_path).await;
    }

    if let Some(parent_reference) = parent.take() {
        parent = Some(
            super::indexes::resolve_backup_reference(
                Arc::clone(&fs),
                key.clone(),
                password.clone(),
                &parent_reference,
            )
            .await?,
        );
        pending.parent = parent.clone();
    }

    let parent_backup = match parent.as_deref() {
        Some(parent_hash) => Some(load_backup(&fs, &key, password.as_deref(), parent_hash).await?),
        None => None,
    };
    if let Some(parent_backup) = &parent_backup {
        backup.parents.push(parent_backup.hash.clone());
        increment_chunk_refcounts(&mut chunk_indexes, &parent_backup.tree);
        backup.tree = parent_backup.tree.clone();
    }

    let files = list_files(&root, &ignore_patterns, include_git)?;
    let current_paths = files
        .iter()
        .filter_map(|path| relative_path(path, &root))
        .collect::<HashSet<_>>();
    if parent_backup.is_some() {
        let stale = backup
            .tree
            .keys()
            .filter(|path| {
                !current_paths.contains(*path) && !is_ignored_relative(path, &ignore_patterns)
            })
            .cloned()
            .collect::<Vec<_>>();
        for path in stale {
            if let Some(object) = backup.tree.remove(&path) {
                decrement_chunk_refcounts(&mut chunk_indexes, &object);
            }
        }
        if !include_git {
            let stale_git = backup
                .tree
                .keys()
                .filter(|path| super::git::is_git_path(path))
                .cloned()
                .collect::<Vec<_>>();
            for path in stale_git {
                if let Some(object) = backup.tree.remove(&path) {
                    decrement_chunk_refcounts(&mut chunk_indexes, &object);
                }
            }
        }
    }

    write_pending(
        &fs,
        &pending_path,
        &pending,
        compression,
        password.as_deref(),
    )
    .await?;

    let resumed_chunks = pending
        .processed_chunks
        .iter()
        .cloned()
        .collect::<HashSet<_>>();

    let mut written_bytes = 0_u64;
    let mut deduplicated_bytes = 0_u64;
    let chunk_capacity = usize::try_from(chunk_size)
        .map_err(|_| "Chunk size is too large to process".to_string())?;
    for (index, file_path) in files.iter().enumerate() {
        let relative = relative_path(file_path, &root)
            .ok_or_else(|| format!("File '{}' is outside the backup root", file_path.display()))?;
        let metadata = std::fs::metadata(file_path).map_err(|error| {
            format!(
                "Failed to get metadata for '{}': {error}",
                file_path.display()
            )
        })?;
        let previous = backup.tree.get(&relative).cloned();
        let data = read_file(file_path)?;
        let file_hash = format!("{:x}", Sha256::digest(&data));
        if previous
            .as_ref()
            .is_some_and(|object| object.hash == file_hash)
        {
            report(
                &progress,
                "files",
                (index + 1) as u64,
                Some(files.len() as u64),
                Some(format!("Skipping unchanged file {relative}")),
            );
            continue;
        }

        let mut chunks = Vec::new();
        for chunk in data.chunks(chunk_capacity) {
            let chunk_hash = format!("{:x}", Sha256::digest(chunk));
            chunks.push(chunk_hash.clone());
            let indexed = chunk_indexes
                .entry(chunk_hash.clone())
                .or_insert(ChunkIndex { refcount: 0 });
            indexed.refcount = indexed.refcount.saturating_add(1);

            let chunk_path = chunk_path(&key, &chunk_hash)?;
            let was_resumed = resumed_chunks.contains(&chunk_hash);
            match fs.read_file(&chunk_path).await {
                Ok(_) => {
                    deduplicated_bytes = deduplicated_bytes.saturating_add(chunk.len() as u64);
                    if was_resumed {
                        report(
                            &progress,
                            "resume",
                            pending.processed_chunks.len() as u64,
                            None,
                            Some(format!("Reused pending chunk {chunk_hash}")),
                        );
                    }
                }
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                    let compressed = compress_bytes(chunk, compression);
                    write_file_maybe_encrypt(&fs, &chunk_path, &compressed, password.as_deref())
                        .await?;
                    written_bytes = written_bytes.saturating_add(chunk.len() as u64);
                }
                Err(error) => {
                    return Err(format!("Failed to read chunk '{chunk_hash}': {error}"));
                }
            }
            pending.processed_chunks.push(chunk_hash);
        }

        let object = BackupObject {
            hash: file_hash,
            size: metadata.len(),
            content_type: "application/octet-stream".to_string(),
            permissions: get_file_permissions_with_path(&metadata, &file_path.to_string_lossy()),
            chunks,
        };
        if let Some(previous) = previous {
            decrement_chunk_refcounts(&mut chunk_indexes, &previous);
        }
        backup.tree.insert(relative, object);
        write_pending(
            &fs,
            &pending_path,
            &pending,
            compression,
            password.as_deref(),
        )
        .await?;
        report(
            &progress,
            "files",
            (index + 1) as u64,
            Some(files.len() as u64),
            Some(format!("Processed file {}/{}", index + 1, files.len())),
        );
    }

    report(
        &progress,
        "indexes",
        0,
        None,
        Some("Saving repository indexes...".to_string()),
    );
    let backup_bytes = rmp_serde::to_vec_named(&backup)
        .map_err(|error| format!("Failed to serialize backup: {error}"))?;
    let backup_path = format!("{key}/backups/{}", backup.hash);
    let encoded_backup = encode_file_bytes(
        &compress_bytes(&backup_bytes, compression),
        password.as_deref(),
    )?;
    write_chunk_indexes_with_merge(
        Arc::clone(&fs),
        key.clone(),
        password.clone(),
        compression,
        chunk_indexes,
        initial_chunk_indexes,
        initial_version,
    )
    .await?;
    fs.write_file(&backup_path, &encoded_backup)
        .await
        .map_err(|error| format!("Failed to write backup file: {error}"))?;
    add_backup_summary(
        Arc::clone(&fs),
        key.clone(),
        &backup,
        compression,
        password.clone(),
        &written_bytes,
    )
    .await?;

    if let Err(error) = index_backup_after_finalize(
        Arc::clone(&fs),
        key.clone(),
        password.clone(),
        compression,
        &backup,
        parent.as_deref(),
        None,
    )
    .await
    {
        warnings.push(CoreWarning {
            code: "catalog_degraded".to_string(),
            message: format!(
                "Historical catalog update was deferred; the backup remains usable: {error}"
            ),
        });
    }

    let expected_parent = match parent.as_deref() {
        Some(parent) => Some(parent.to_string()),
        None => {
            read_or_initialize_repository_head(Arc::clone(&fs), key.clone(), password.clone())
                .await?
                .head
                .backup
        }
    };
    let mut head_published = advance_repository_head(
        Arc::clone(&fs),
        key.clone(),
        password.clone(),
        expected_parent.as_deref(),
        &backup.hash,
    )
    .await?;
    if !head_published {
        let snapshot =
            read_or_initialize_repository_head(Arc::clone(&fs), key.clone(), password.clone())
                .await?;
        head_published = set_repository_head(
            Arc::clone(&fs),
            key.clone(),
            password.clone(),
            &snapshot,
            Some(&backup.hash),
        )
        .await?;
    }
    let _ = fs.delete_file(&pending_path).await;
    report(
        &progress,
        "complete",
        files.len() as u64,
        Some(files.len() as u64),
        Some("Backup completed".to_string()),
    );

    Ok(BackupOutcome {
        backup,
        files_total: files.len(),
        written_bytes,
        deduplicated_bytes,
        elapsed_ms: started.elapsed().as_millis() as u64,
        head_published,
        warnings,
    })
}

fn validate_input(input: &BackupInput) -> Result<(), String> {
    if !input.root.is_dir() {
        return Err(format!(
            "Backup root '{}' is not an existing directory",
            input.root.display()
        ));
    }
    if !(1..=22).contains(&input.compression) {
        return Err("Compression level must be between 1 and 22".to_string());
    }
    if input.chunk_size == 0 || usize::try_from(input.chunk_size).is_err() {
        return Err("Chunk size must be greater than zero and fit in memory".to_string());
    }
    if input.concurrency == 0 {
        return Err("Concurrency must be greater than zero".to_string());
    }
    Ok(())
}

fn report(
    progress: &Option<Arc<dyn Fn(CoreProgress) + Send + Sync + 'static>>,
    phase: &str,
    processed: u64,
    total: Option<u64>,
    message: Option<String>,
) {
    if let Some(progress) = progress {
        progress(CoreProgress {
            phase: phase.to_string(),
            processed,
            total,
            message,
        });
    }
}

fn list_files(root: &Path, ignore: &[String], include_git: bool) -> Result<Vec<PathBuf>, String> {
    let mut files = Vec::new();
    let walker = walkdir::WalkDir::new(root)
        .into_iter()
        .filter_entry(|entry| {
            if entry.path() == root {
                return true;
            }
            let ignored = entry
                .path()
                .strip_prefix(root)
                .ok()
                .is_some_and(|relative| {
                    relative.components().any(|component| {
                        let name = component.as_os_str().to_string_lossy();
                        (!include_git && name == ".git")
                            || ignore.iter().any(|pattern| pattern == &name)
                    })
                });
            !ignored
        });
    for entry in walker {
        let entry = entry.map_err(|error| format!("Failed to list backup files: {error}"))?;
        if entry.file_type().is_file() {
            files.push(entry.into_path());
        }
    }
    files.sort();
    Ok(files)
}

fn relative_path(path: &Path, root: &Path) -> Option<String> {
    let relative = path.strip_prefix(root).ok()?;
    let value = relative.to_string_lossy().replace('\\', "/");
    let value = value.trim_matches('/');
    (!value.is_empty()).then(|| value.to_string())
}

fn is_ignored_relative(path: &str, ignore: &[String]) -> bool {
    path.split('/')
        .any(|component| ignore.iter().any(|pattern| pattern == component))
}

fn read_file(path: &Path) -> Result<Vec<u8>, String> {
    let mut file = std::fs::File::open(path)
        .map_err(|error| format!("Failed to open '{}': {error}", path.display()))?;
    let mut data = Vec::new();
    file.read_to_end(&mut data)
        .map_err(|error| format!("Failed to read '{}': {error}", path.display()))?;
    Ok(data)
}

fn chunk_path(key: &str, hash: &str) -> Result<String, String> {
    let (prefix, rest) = hash
        .split_at_checked(2)
        .ok_or_else(|| format!("Invalid chunk hash '{hash}'"))?;
    Ok(format!("{key}/chunks/{prefix}/{rest}"))
}

async fn load_backup(
    fs: &Arc<dyn FS>,
    key: &str,
    password: Option<&str>,
    hash: &str,
) -> Result<Backup, String> {
    let path = format!("{key}/backups/{hash}");
    let result = read_file_maybe_decrypt(
        fs,
        &path,
        password,
        "Backup is encrypted but no password provided",
    )
    .await?;
    if result.bytes.is_empty() {
        return Err(format!("Backup {hash} not found or is empty"));
    }
    let bytes = decompress_bytes(&result.bytes);
    rmp_serde::from_slice(&bytes).map_err(|error| format!("Failed to deserialize backup: {error}"))
}

async fn load_pending(
    fs: &Arc<dyn FS>,
    key: &str,
    prefix: &str,
    password: Option<&str>,
) -> Result<Option<(PendingBackup, String)>, String> {
    let paths = fs
        .list_files(&format!("{key}/indexes"))
        .await
        .map_err(|error| format!("Failed to list pending backups: {error}"))?;
    let marker = format!("{key}/indexes/pending_{prefix}");
    let Some(path) = paths
        .into_iter()
        .filter(|path| path.starts_with(&marker))
        .max()
    else {
        return Ok(None);
    };
    let result = read_file_maybe_decrypt(
        fs,
        &path,
        password,
        "Pending backup is encrypted but no password provided",
    )
    .await?;
    let pending = rmp_serde::from_slice(&decompress_bytes(&result.bytes))
        .map_err(|error| format!("Failed to deserialize pending backup '{path}': {error}"))?;
    Ok(Some((pending, path)))
}

async fn write_pending(
    fs: &Arc<dyn FS>,
    path: &str,
    pending: &PendingBackup,
    compression: i32,
    password: Option<&str>,
) -> Result<(), String> {
    let bytes = rmp_serde::to_vec_named(pending)
        .map_err(|error| format!("Failed to serialize pending backup: {error}"))?;
    write_file_maybe_encrypt(fs, path, &compress_bytes(&bytes, compression), password).await
}

fn increment_chunk_refcounts(
    indexes: &mut HashMap<String, ChunkIndex>,
    tree: &HashMap<String, BackupObject>,
) {
    for object in tree.values() {
        for hash in &object.chunks {
            let entry = indexes
                .entry(hash.clone())
                .or_insert(ChunkIndex { refcount: 0 });
            entry.refcount = entry.refcount.saturating_add(1);
        }
    }
}

fn decrement_chunk_refcounts(indexes: &mut HashMap<String, ChunkIndex>, object: &BackupObject) {
    for hash in &object.chunks {
        if let Some(entry) = indexes.get_mut(hash) {
            entry.refcount = entry.refcount.saturating_sub(1);
            if entry.refcount == 0 {
                indexes.remove(hash);
            }
        }
    }
}
