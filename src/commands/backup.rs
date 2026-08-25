use crate::commands::config::Config;
use crate::core::crypto::get_password;
use crate::core::crypto::read_file_maybe_decrypt;
use crate::core::crypto::write_file_maybe_encrypt;
use crate::core::indexes::{
    add_backup_summary, create_new_backup, list_backup_summaries, load_chunk_indexes,
};
use crate::core::metadata::PendingBackup;
use crate::core::metadata::{Backup, BackupObject, ChunkIndex};
use crate::core::permissions::get_file_permissions_with_path;
use crate::fs::FS;
use crate::output::{JsonProgress, emit_output, emit_progress_message, emit_warning, is_json_mode};
use crate::utils::decompress_bytes;
use crate::utils::{compress_bytes, get_fs, get_pwd_string, handle_error};
use bytesize::ByteSize;
use clap::ArgMatches;
use console::style;
use dialoguer::{Input, Select};
use dirs::home_dir;
use futures::stream::{self, StreamExt};
use indicatif::{ProgressBar, ProgressStyle};
use parse_size::parse_size;
use sha2::{Digest, Sha256};
use std::collections::{HashMap, HashSet};
use std::io::Read;
use std::path::Path;
use std::sync::atomic::AtomicBool;
use std::sync::atomic::Ordering;
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::Duration;
use tokio::sync::{Mutex as TokioMutex, Semaphore};
use tokio::task::JoinSet;

#[derive(Clone)]
pub(crate) struct BackupOptions {
    pub(crate) key: String,
    pub(crate) root_path_string: String,
    pub(crate) storage: String,
    pub(crate) fs: Arc<dyn FS>,
    pub(crate) author: String,
    pub(crate) compress: i32,
    pub(crate) password: Option<String>,
    pub(crate) chunk_size: u64,
    pub(crate) ignore_patterns: Vec<String>,
    pub(crate) concurrency: usize,
}

pub(crate) struct ResolvedBackup {
    pub(crate) options: BackupOptions,
    pub(crate) message: String,
    pub(crate) parent_hash: Option<String>,
    pub(crate) pending_backup: Option<PendingBackupMatch>,
}

#[derive(Clone, Copy, PartialEq, Eq)]
pub(crate) enum BackupMode {
    Manual,
    Watch,
}

pub(crate) struct BackupResult {
    pub(crate) backup: Backup,
    pub(crate) files_total: usize,
    pub(crate) written_bytes: u64,
    pub(crate) deduplicated_bytes: u64,
    pub(crate) elapsed_ms: u64,
}

struct PendingBackupWatcherGuard(Arc<AtomicBool>);

impl Drop for PendingBackupWatcherGuard {
    fn drop(&mut self) {
        self.0.store(true, Ordering::SeqCst);
    }
}

pub async fn backup(matches: &ArgMatches) {
    let resolved = match resolve_backup(matches, BackupMode::Manual).await {
        Ok(resolved) => resolved,
        Err(e) => handle_error(e, None),
    };

    if let Err(e) = run_backup(
        resolved.options,
        resolved.message,
        resolved.parent_hash,
        resolved.pending_backup,
    )
    .await
    {
        handle_error(e, None);
    }
}

pub(crate) async fn run_backup(
    options: BackupOptions,
    message: String,
    parent_hash: Option<String>,
    received_pending_backup: Option<PendingBackupMatch>,
) -> Result<BackupResult, String> {
    let BackupOptions {
        key,
        root_path_string,
        storage: _storage,
        fs,
        author,
        compress,
        password,
        chunk_size,
        ignore_patterns,
        concurrency,
    } = options;

    let received_pending_backup = Arc::new(Mutex::new(received_pending_backup));
    let config = Config { author };

    let pb = if is_json_mode() {
        ProgressBar::hidden()
    } else {
        let pb = ProgressBar::new(100);
        pb.enable_steady_tick(Duration::from_millis(100));
        pb.set_style(ProgressStyle::with_template("{spinner:.green} {msg}").unwrap());
        pb.set_message("Loading metadata from the repository key...");
        pb
    };

    if is_json_mode() {
        emit_progress_message("Loading metadata from the repository key...");
    }

    pb.set_message("Generating new backup...");
    if is_json_mode() {
        emit_progress_message("Generating new backup...");
    }

    let prev_not_encrypted_but_now_yes = Arc::new(Mutex::new(false));

    let (new_backup, root_files, chunk_indexes) = match load_metadata(
        Arc::clone(&fs),
        key.clone(),
        message,
        config,
        root_path_string.clone(),
        password.clone(),
        Arc::clone(&prev_not_encrypted_but_now_yes),
        ignore_patterns.clone(),
        parent_hash.clone(),
    )
    .await
    {
        Ok(result) => result,
        Err(e) => {
            pb.finish_and_clear();
            return Err(e);
        }
    };

    let continue_error_message = format!(
        "Continue from the place where the backup was interrupted by running: gib backup --continue {}",
        new_backup.hash[..8].to_string()
    );

    let total_files = root_files.len();

    pb.finish_and_clear();

    if *prev_not_encrypted_but_now_yes.lock().unwrap() {
        let warning = "The backup was not encrypted but you provided a password. Only new chunks will be encrypted; run 'gib encrypt' to encrypt existing chunks.";
        if is_json_mode() {
            emit_warning(warning, "unencrypted_chunks");
        } else {
            println!("{}", style(warning).yellow());
        }
    }

    let json_progress = if is_json_mode() {
        let progress = JsonProgress::new(root_files.len() as u64);
        progress.set_message(&format!(
            "Backing up files to {}...",
            new_backup.hash[..8].to_string()
        ));
        Some(progress)
    } else {
        None
    };

    let pb = if is_json_mode() {
        ProgressBar::hidden()
    } else {
        let pb = ProgressBar::new(root_files.len() as u64);
        pb.enable_steady_tick(Duration::from_millis(100));
        pb.set_style(
            ProgressStyle::with_template(
                "[{elapsed_precise}] [{bar:40.cyan/blue}] {pos}/{len} {msg}",
            )
            .unwrap(),
        );
        pb.set_message(format!(
            "Backing up files to {}...",
            new_backup.hash[..8].to_string()
        ));
        pb
    };

    let chunk_indexes: Arc<Mutex<HashMap<String, ChunkIndex>>> =
        Arc::new(Mutex::new(chunk_indexes));

    let new_backup: Arc<Mutex<Backup>> = Arc::new(Mutex::new(new_backup));

    let files_set = Arc::new(TokioMutex::new(JoinSet::new()));
    let written_bytes = Arc::new(Mutex::new(0));
    let deduplicated_bytes = Arc::new(Mutex::new(0));
    let semaphore = Arc::new(Semaphore::new(concurrency));

    let pending_backup = Arc::new(Mutex::new(PendingBackup {
        message: new_backup.lock().unwrap().message.clone(),
        compress,
        chunk_size,
        concurrency,
        ignore_patterns: ignore_patterns.clone(),
        parent: parent_hash,
        processed_chunks: Vec::new(),
    }));
    let pending_backup_path = Arc::new(format!(
        "{}/indexes/pending_{}",
        key,
        new_backup.lock().unwrap().hash
    ));

    let pending_backup_watcher_stop = Arc::new(AtomicBool::new(false));

    {
        let fs_clone = Arc::clone(&fs);
        let pending_backup_clone = Arc::clone(&pending_backup);
        let pending_backup_path_clone = pending_backup_path.clone();
        let pending_backup_watcher_stop_clone = pending_backup_watcher_stop.clone();
        let password_clone = password.clone();

        thread::spawn(move || {
            let runtime = tokio::runtime::Runtime::new().unwrap();
            runtime.block_on(watch_pending_backup(
                pending_backup_clone,
                pending_backup_path_clone,
                fs_clone,
                pending_backup_watcher_stop_clone,
                password_clone,
            ));
        });
    };
    let _pending_backup_watcher_guard =
        PendingBackupWatcherGuard(Arc::clone(&pending_backup_watcher_stop));

    let files_stream = stream::iter(root_files);

    files_stream
        .for_each_concurrent(concurrency, |file_path| {
            let pb_clone = pb.clone();
            let chunk_indexes_clone = Arc::clone(&chunk_indexes);
            let password_clone = password.clone();
            let key_clone = key.clone();
            let fs_clone = Arc::clone(&fs);
            let new_backup_clone = Arc::clone(&new_backup);
            let root_path_string_clone = root_path_string.clone();
            let written_bytes_clone = Arc::clone(&written_bytes);
            let deduplicated_bytes_clone = Arc::clone(&deduplicated_bytes);
            let semaphore_clone = Arc::clone(&semaphore);
            let files_set_clone = Arc::clone(&files_set);
            let json_progress_clone = json_progress.clone();
            let pending_backup_clone = Arc::clone(&pending_backup);
            let received_pending_backup_clone = Arc::clone(&received_pending_backup);

            async move {
                let mut guard = files_set_clone.lock().await;
                guard.spawn(async move {
                    let _permit = semaphore_clone.acquire().await.expect("Semaphore closed");
                    backup_file(
                        file_path,
                        pb_clone,
                        chunk_indexes_clone,
                        password_clone,
                        key_clone,
                        fs_clone,
                        new_backup_clone,
                        root_path_string_clone,
                        written_bytes_clone,
                        deduplicated_bytes_clone,
                        chunk_size,
                        compress,
                        json_progress_clone,
                        pending_backup_clone,
                        received_pending_backup_clone,
                    )
                    .await
                });
            }
        })
        .await;

    let mut failed_files = Vec::new();

    {
        let mut guard = files_set.lock().await;
        while let Some(file_process_result) = guard.join_next().await {
            match file_process_result {
                Ok(Ok(_)) => {}
                Ok(Err(e)) => failed_files.push(e),
                Err(e) => failed_files.push(e.to_string()),
            }
        }
    }

    pending_backup_watcher_stop.store(true, Ordering::SeqCst);

    if !failed_files.is_empty() {
        pb.finish_and_clear();
        return Err(format!(
            "Failed to process {} files:\n{}\n\n{}",
            failed_files.len(),
            failed_files
                .iter()
                .map(|f| format!("  - {}", f))
                .collect::<Vec<String>>()
                .join("\n"),
            &continue_error_message
        ));
    }

    let chunk_indexes_bytes = match rmp_serde::to_vec_named(&*chunk_indexes.lock().unwrap()) {
        Ok(bytes) => bytes,
        Err(error) => {
            pb.finish_and_clear();
            return Err(format!("Failed to serialize chunk indexes: {}", error));
        }
    };

    let compressed_chunk_indexes_bytes = compress_bytes(&chunk_indexes_bytes, compress);

    let chunk_index_path = format!("{}/indexes/chunks", key);

    let write_chunk_index_future = write_file_maybe_encrypt(
        &fs,
        &chunk_index_path,
        &compressed_chunk_indexes_bytes,
        password.as_deref(),
    );

    let backup_file_bytes = match rmp_serde::to_vec_named(&*new_backup.lock().unwrap()) {
        Ok(bytes) => bytes,
        Err(error) => {
            pb.finish_and_clear();
            return Err(format!("Failed to serialize backup: {}", error));
        }
    };

    let compressed_backup_file_bytes = compress_bytes(&backup_file_bytes, compress);

    let backup_file_path = format!("{}/backups/{}", key, new_backup.lock().unwrap().hash);

    let write_backup_file_future = write_file_maybe_encrypt(
        &fs,
        &backup_file_path,
        &compressed_backup_file_bytes,
        password.as_deref(),
    );

    let (write_chunk_index_result, write_backup_file_result) =
        tokio::join!(write_chunk_index_future, write_backup_file_future);

    if let Err(error) = write_chunk_index_result {
        pb.finish_and_clear();
        return Err(format!(
            "Failed to write chunk indexes: {}\n\n{}",
            error, &continue_error_message
        ));
    }

    if let Err(error) = write_backup_file_result {
        pb.finish_and_clear();
        return Err(format!(
            "Failed to write backup file: {}\n\n{}",
            error, &continue_error_message
        ));
    }

    let written_bytes = *written_bytes.lock().unwrap();
    let deduplicated_bytes = *deduplicated_bytes.lock().unwrap();

    {
        let backup_guard = new_backup.lock().unwrap();
        if let Err(e) = add_backup_summary(
            Arc::clone(&fs),
            key.clone(),
            &backup_guard,
            compress,
            password.clone(),
            &written_bytes,
        )
        .await
        {
            pb.finish_and_clear();
            return Err(format!(
                "Failed to save backup summary: {}\n\n{}",
                &e, &continue_error_message
            ));
        }
    }

    let _ = fs.delete_file(&pending_backup_path).await;

    {
        match received_pending_backup.lock().unwrap().take() {
            Some(pending_backup) => {
                let _ = fs.delete_file(&pending_backup.path).await;
            }
            None => {}
        };
    }

    if is_json_mode() {
        #[derive(serde::Serialize)]
        struct BackupOutput {
            backup: String,
            backup_short: String,
            message: String,
            author: String,
            timestamp_unix: u64,
            files_total: usize,
            written_bytes: u64,
            deduplicated_bytes: u64,
            elapsed_ms: u64,
        }

        let backup_guard = new_backup.lock().unwrap();
        let elapsed_ms = pb.elapsed().as_millis() as u64;
        let payload = BackupOutput {
            backup: backup_guard.hash.clone(),
            backup_short: backup_guard.hash[..8.min(backup_guard.hash.len())].to_string(),
            message: backup_guard.message.clone(),
            author: backup_guard.author.clone(),
            timestamp_unix: backup_guard.timestamp,
            files_total: total_files,
            written_bytes,
            deduplicated_bytes,
            elapsed_ms,
        };
        emit_output(&payload);
    } else {
        let elapsed = pb.elapsed();
        pb.set_style(ProgressStyle::with_template("{prefix:.green} {msg}").unwrap());
        pb.set_prefix("OK");
        pb.finish_with_message(format!(
            "Backed up files ({:.2?}) - {} written, {} deduplicated",
            elapsed,
            ByteSize(written_bytes),
            ByteSize(deduplicated_bytes),
        ));
    }

    Ok(BackupResult {
        backup: new_backup.lock().unwrap().clone(),
        files_total: total_files,
        written_bytes,
        deduplicated_bytes,
        elapsed_ms: pb.elapsed().as_millis() as u64,
    })
}

async fn watch_pending_backup(
    pending_backup: Arc<Mutex<PendingBackup>>,
    pending_backup_path: Arc<String>,
    fs: Arc<dyn FS>,
    pending_backup_watcher_stop: Arc<AtomicBool>,
    password: Option<String>,
) {
    let mut interval = tokio::time::interval(Duration::from_secs(1));

    loop {
        interval.tick().await;

        if pending_backup_watcher_stop.load(Ordering::SeqCst) {
            break;
        }

        let bytes_to_write = {
            let pending_backup_guard = pending_backup.lock().unwrap();
            rmp_serde::to_vec_named(&*pending_backup_guard).unwrap_or_else(|_| Vec::new())
        };

        let compressed_bytes = compress_bytes(&bytes_to_write, 3);

        let _ = write_file_maybe_encrypt(
            &fs,
            pending_backup_path.as_str(),
            &compressed_bytes,
            password.as_deref(),
        )
        .await;
    }
}

async fn backup_file(
    file_path: String,
    pb: ProgressBar,
    chunk_indexes: Arc<Mutex<HashMap<String, ChunkIndex>>>,
    password: Option<String>,
    key: String,
    fs: Arc<dyn FS>,
    new_backup: Arc<Mutex<Backup>>,
    root_path_string: String,
    written_bytes: Arc<Mutex<u64>>,
    deduplicated_bytes: Arc<Mutex<u64>>,
    chunk_size: u64,
    compress: i32,
    json_progress: Option<Arc<JsonProgress>>,
    pending_backup: Arc<Mutex<PendingBackup>>,
    received_pending_backup: Arc<Mutex<Option<PendingBackupMatch>>>,
) -> Result<(), String> {
    let mut file = std::fs::File::open(file_path.clone())
        .map_err(|e| format!("Failed to open file: {}", e))?;
    let mut file_hasher = Sha256::new();
    let mut file_chunks = Vec::new();

    let file_metadata = file
        .metadata()
        .map_err(|e| format!("Failed to get file metadata: {}", e))?;

    let mut buffer = vec![0u8; chunk_size as usize];

    loop {
        let bytes_read = file
            .read(&mut buffer)
            .map_err(|e| format!("Failed to read file: {}", e))
            .unwrap_or(0);

        if bytes_read == 0 {
            break;
        }

        let chunk_bytes = &buffer[..bytes_read];

        file_hasher.update(chunk_bytes);

        let chunk_hash = format!("{:x}", Sha256::digest(chunk_bytes));
        file_chunks.push(chunk_hash.clone());

        let is_in_chunk_indexes = {
            let mut chunk_indexes_guard = chunk_indexes.lock().unwrap();
            let entry = chunk_indexes_guard
                .entry(chunk_hash.clone())
                .or_insert(ChunkIndex { refcount: 0 });
            entry.refcount += 1;

            entry.refcount > 1
        };

        if is_in_chunk_indexes {
            let mut deduplicated_bytes_guard = deduplicated_bytes.lock().unwrap();
            *deduplicated_bytes_guard += chunk_bytes.len() as u64;
            continue;
        }

        {
            let received_pending_backup_guard = received_pending_backup.lock().unwrap();

            let exists = match received_pending_backup_guard.as_ref() {
                Some(pending_backup) => {
                    pending_backup.backup.processed_chunks.contains(&chunk_hash)
                }
                None => false,
            };

            if exists {
                let mut written_bytes_guard = written_bytes.lock().unwrap();
                *written_bytes_guard += chunk_bytes.len() as u64;
                continue;
            }
        }

        let compressed_chunk_bytes = compress_bytes(chunk_bytes, compress);

        let (chunk_hash_prefix, chunk_hash_rest) = chunk_hash.split_at(2);
        let chunk_path = format!("{}/chunks/{}/{}", key, chunk_hash_prefix, chunk_hash_rest);

        let mut last_error = String::new();
        let mut success = false;

        for attempt in 1..=3 {
            match write_file_maybe_encrypt(
                &fs,
                &chunk_path,
                &compressed_chunk_bytes,
                password.as_deref(),
            )
            .await
            {
                Ok(_) => {
                    success = true;
                    break;
                }
                Err(e) => {
                    last_error = format!("Failed to write chunk (attempt {}/3): {}", attempt, e);
                    if attempt < 3 {
                        tokio::time::sleep(Duration::from_millis(100 * attempt as u64)).await;
                    }
                }
            }
        }

        if !success {
            return Err(last_error);
        }

        {
            let mut written_bytes_guard = written_bytes.lock().unwrap();
            *written_bytes_guard += chunk_bytes.len() as u64;
        }

        {
            let mut pending_backup_guard = pending_backup.lock().unwrap();
            pending_backup_guard
                .processed_chunks
                .push(chunk_hash.clone());
        }
    }

    let file_hash = format!("{:x}", file_hasher.finalize());

    let relative_path = relative_path(&file_path, &root_path_string);

    let file_permissions = get_file_permissions_with_path(&file_metadata, &file_path);

    let replaced_backup_object = {
        let mut new_backup_guard = new_backup.lock().unwrap();

        new_backup_guard.tree.insert(
            relative_path.to_string(),
            BackupObject {
                hash: file_hash.clone(),
                size: file_metadata.len(),
                content_type: "application/octet-stream".to_string(),
                permissions: file_permissions,
                chunks: file_chunks,
            },
        )
    };

    if let Some(previous_backup_object) = replaced_backup_object {
        decrement_chunk_refcounts(&chunk_indexes, &previous_backup_object);
    }

    if let Some(progress) = &json_progress {
        progress.inc_by(1);
    } else {
        pb.inc(1);
    }
    Ok(())
}

fn list_files(path: &str, ignore_patterns: &[String]) -> Vec<String> {
    let mut files = Vec::new();
    let root = Path::new(path);

    let walker = walkdir::WalkDir::new(path)
        .into_iter()
        .filter_entry(|entry| !is_ignored_path(entry.path(), root, ignore_patterns));

    for entry in walker.filter_map(|e| e.ok()).filter(|e| e.path().is_file()) {
        files.push(entry.path().display().to_string());
    }

    files
}

pub(crate) fn is_ignored_path(path: &Path, root: &Path, ignore_patterns: &[String]) -> bool {
    if ignore_patterns.is_empty() {
        return false;
    }

    let components = if path == root {
        root.file_name()
            .map(|name| vec![name.to_string_lossy().to_string()])
            .unwrap_or_default()
    } else {
        path.strip_prefix(root)
            .map(|relative| {
                relative
                    .components()
                    .map(|component| component.as_os_str().to_string_lossy().to_string())
                    .collect::<Vec<_>>()
            })
            .unwrap_or_default()
    };

    components
        .iter()
        .any(|component| ignore_patterns.iter().any(|pattern| pattern == component))
}

fn relative_path(file_path: &str, root_path: &str) -> String {
    let content = file_path.strip_prefix(root_path).unwrap_or(file_path);
    let mut content = content.replace('\\', "/");

    if content.starts_with('/') {
        content = content[1..].to_string();
    }

    content
}

fn remove_missing_backup_objects(
    tree: &mut HashMap<String, BackupObject>,
    chunk_indexes: &mut HashMap<String, ChunkIndex>,
    root_files: &[String],
    root_path: &str,
    ignore_patterns: &[String],
) {
    let current_paths: HashSet<String> = root_files
        .iter()
        .map(|file_path| relative_path(file_path, root_path))
        .collect();
    let stale_paths: Vec<String> = tree
        .keys()
        .filter(|path| {
            !current_paths.contains(*path)
                && !path
                    .split('/')
                    .any(|component| ignore_patterns.iter().any(|pattern| pattern == component))
        })
        .cloned()
        .collect();

    for stale_path in stale_paths {
        if let Some(backup_object) = tree.remove(&stale_path) {
            decrement_chunk_refcounts_from_map(chunk_indexes, &backup_object);
        }
    }
}

async fn load_metadata(
    fs: Arc<dyn FS>,
    key: String,
    message: String,
    config: Config,
    root_path_string: String,
    password: Option<String>,
    prev_not_encrypted_but_now_yes: Arc<Mutex<bool>>,
    ignore_patterns: Vec<String>,
    parent_hash: Option<String>,
) -> Result<(Backup, Vec<String>, HashMap<String, ChunkIndex>), String> {
    let mut new_backup = create_new_backup(message, config.author);

    let root_path_for_listing = root_path_string.clone();
    let ignore_patterns_for_listing = ignore_patterns.clone();
    let root_files_future = tokio::spawn(async move {
        list_files(&root_path_for_listing, &ignore_patterns_for_listing)
    });

    let parent_backup_future = tokio::spawn({
        let fs = Arc::clone(&fs);
        let key = key.clone();
        let password = password.clone();
        async move {
            match parent_hash {
                Some(parent_hash) => load_backup(fs, key, password, &parent_hash).await.map(Some),
                None => Ok(None),
            }
        }
    });

    let chunk_indexes_future = tokio::spawn(load_chunk_indexes(
        Arc::clone(&fs),
        key.clone(),
        password,
        prev_not_encrypted_but_now_yes,
    ));

    let (root_files_result, chunk_indexes_result, parent_backup_result) =
        tokio::join!(root_files_future, chunk_indexes_future, parent_backup_future);

    let root_files = root_files_result.map_err(|e| format!("Failed to list root files: {}", e))?;

    let mut chunk_indexes = chunk_indexes_result
        .map_err(|e| format!("Failed to load chunk indexes: {}", e))?
        .map_err(|e| format!("Failed to load chunk indexes: {}", e))?;

    if let Some(parent_backup) = parent_backup_result
        .map_err(|e| format!("Failed to load parent backup: {}", e))?
        .map_err(|e| format!("Failed to load parent backup: {}", e))?
    {
        increment_chunk_refcounts(&mut chunk_indexes, &parent_backup.tree);
        new_backup.tree = parent_backup.tree;

        remove_missing_backup_objects(
            &mut new_backup.tree,
            &mut chunk_indexes,
            &root_files,
            &root_path_string,
            &ignore_patterns,
        );
    }

    Ok((new_backup, root_files, chunk_indexes))
}

pub(crate) struct PendingBackupMatch {
    backup: PendingBackup,
    path: String,
}

struct BackupSummaryDisplay {
    hash: String,
    message: String,
}

async fn load_pending_backup(
    fs: Arc<dyn FS>,
    key: &str,
    continue_prefix: &str,
    password: &Option<String>,
) -> Result<PendingBackupMatch, String> {
    let indexes_path = format!("{}/indexes", key);
    let files = fs
        .list_files(&indexes_path)
        .await
        .map_err(|e| format!("Failed to list indexes in '{}': {}", indexes_path, e))?;

    let pending_prefix = format!("{}/indexes/pending_{}", key, continue_prefix);
    let mut matches: Vec<String> = files
        .into_iter()
        .filter(|path| path.starts_with(&pending_prefix))
        .collect();

    matches.sort();
    matches.dedup();

    if matches.is_empty() {
        return Err(format!("No pending backup found for '{}'", continue_prefix));
    }

    let pending_path = matches
        .pop()
        .ok_or_else(|| "Pending backup match missing".to_string())?;

    let pending_result = read_file_maybe_decrypt(
        &fs,
        &pending_path,
        password.as_deref(),
        "The pending backup is encrypted. Please enter the password to decrypt it.",
    )
    .await?;

    let decompressed_bytes = decompress_bytes(&pending_result.bytes);

    let pending_backup: PendingBackup =
        rmp_serde::from_slice(&decompressed_bytes).map_err(|e| {
            format!(
                "Failed to deserialize pending backup '{}': {}",
                pending_path, e
            )
        })?;

    Ok(PendingBackupMatch {
        backup: pending_backup,
        path: pending_path,
    })
}

pub(crate) async fn resolve_backup(
    matches: &ArgMatches,
    mode: BackupMode,
) -> Result<ResolvedBackup, String> {
    let parent_requested = matches.contains_id("parent");
    let continue_requested = matches.contains_id("continue");

    if mode == BackupMode::Watch && (parent_requested || continue_requested) {
        return Err(
            "--parent and --continue cannot be used with gib watch; watch selects the latest completed backup automatically".to_string(),
        );
    }

    if parent_requested
        && matches.get_one::<String>("parent").is_none()
        && is_json_mode()
    {
        return Err("--parent requires a backup hash when used in JSON mode".to_string());
    }

    if continue_requested && parent_requested {
        return Err("--parent cannot be used together with --continue".to_string());
    }

    let password = matches
        .get_one::<String>("password")
        .map(ToString::to_string)
        .or_else(|| get_password(false, false));

    let pwd_string = get_pwd_string();
    let root_path_string = matches.get_one::<String>("root-path").map_or_else(
        || pwd_string.clone(),
        |root_path| Path::new(&pwd_string).join(root_path).to_string_lossy().to_string(),
    );

    let default_key = Path::new(&root_path_string)
        .file_name()
        .ok_or_else(|| "The backup root path must have a valid directory name".to_string())?
        .to_string_lossy()
        .to_string();

    let key = matches
        .get_one::<String>("key")
        .map_or(default_key, ToString::to_string);

    let home_dir = home_dir().ok_or_else(|| "Failed to get home directory".to_string())?;
    let gib_dir = home_dir.join(".gib");
    let config = load_config(&gib_dir.join("config.msgpack"))?;
    let storage_dir = gib_dir.join("storages");
    let storage_names = list_storage_names(&storage_dir)?;

    let storage = match matches.get_one::<String>("storage") {
        Some(storage) => storage.to_string(),
        None => {
            if is_json_mode() {
                return Err(
                    "Missing required argument: --storage (required in --mode json)".to_string(),
                );
            }
            let selected_index = Select::new()
                .with_prompt("Select the storage to use")
                .items(&storage_names)
                .default(0)
                .interact()
                .map_err(|e| e.to_string())?;
            storage_names[selected_index].clone()
        }
    };

    if !storage_names.iter().any(|name| name == &storage) {
        return Err(format!("Storage '{}' not found", storage));
    }

    let storage_config = load_storage_config(&storage_dir, &storage)?;
    if storage_config.storage_type == 0 && storage_config.path.is_none() {
        return Err(format!("Local storage '{}' has no path", storage));
    }
    if storage_config.storage_type > 1 {
        return Err(format!("Storage '{}' has an invalid storage type", storage));
    }
    let fs = get_fs(&storage_config, None);

    let pending_backup = if mode == BackupMode::Manual {
        match matches.get_one::<String>("continue") {
            Some(continue_prefix) => {
                Some(load_pending_backup(Arc::clone(&fs), &key, continue_prefix, &password).await?)
            }
            None => None,
        }
    } else {
        None
    };

    let mut reused_data = Vec::new();
    if let Some(pending) = &pending_backup
        && !pending.backup.processed_chunks.is_empty()
    {
        reused_data.push("uploaded chunks".to_string());
    }

    let parent_hash = if mode == BackupMode::Watch {
        None
    } else {
        match pending_backup.as_ref() {
            Some(pending) => {
                if pending.backup.parent.is_some() {
                    reused_data.push("parent".to_string());
                }
                pending.backup.parent.clone()
            }
            None => {
                resolve_parent_hash(Arc::clone(&fs), key.clone(), password.clone(), matches)
                    .await?
            }
        }
    };

    let message = if mode == BackupMode::Watch {
        matches
            .get_one::<String>("message")
            .map(ToString::to_string)
            .unwrap_or_default()
    } else {
        match matches.get_one::<String>("message") {
            Some(message) => message.to_string(),
            None => {
                if let Some(pending) = &pending_backup
                    && !pending.backup.message.is_empty()
                {
                    reused_data.push("message".to_string());
                    pending.backup.message.clone()
                } else {
                    if is_json_mode() {
                        return Err(
                            "Missing required argument: --message (required in --mode json)"
                                .to_string(),
                        );
                    }
                    Input::<String>::new()
                        .with_prompt("Enter the backup message")
                        .interact_text()
                        .map_err(|e| e.to_string())?
                }
            }
        }
    };

    let default_compress = 3;
    let compress = match matches.get_one::<String>("compress") {
        Some(value) => value
            .parse()
            .map_err(|_| format!("Invalid compression level '{}'", value))?,
        None => pending_backup
            .as_ref()
            .map(|pending| {
                if pending.backup.compress != default_compress {
                    reused_data.push("compress".to_string());
                }
                pending.backup.compress
            })
            .unwrap_or(default_compress),
    };

    let default_chunk_size = parse_size("5 MB").expect("default chunk size is valid");
    let chunk_size = match matches.get_one::<String>("chunk-size") {
        Some(value) => parse_size(value)
            .map_err(|_| format!("Invalid chunk size '{}'", value))?,
        None => pending_backup
            .as_ref()
            .map(|pending| {
                if pending.backup.chunk_size != default_chunk_size {
                    reused_data.push("chunk size".to_string());
                }
                pending.backup.chunk_size
            })
            .unwrap_or(default_chunk_size),
    };

    let ignore_patterns = match matches.get_many::<String>("ignore") {
        Some(values) => values.map(ToString::to_string).collect(),
        None => pending_backup
            .as_ref()
            .map(|pending| {
                if !pending.backup.ignore_patterns.is_empty() {
                    reused_data.push("ignored files".to_string());
                }
                pending.backup.ignore_patterns.clone()
            })
            .unwrap_or_default(),
    };

    let default_concurrency = num_cpus::get() * 2;
    let concurrency = match matches.get_one::<String>("concurrency") {
        Some(value) => value
            .parse()
            .map_err(|_| format!("Invalid concurrency '{}'", value))?,
        None => pending_backup
            .as_ref()
            .map(|pending| {
                if pending.backup.concurrency != default_concurrency {
                    reused_data.push("concurrency".to_string());
                }
                pending.backup.concurrency
            })
            .unwrap_or(default_concurrency),
    };

    if !reused_data.is_empty() {
        let pending_name = pending_backup
            .as_ref()
            .and_then(|pending| pending.path.rsplit('/').next())
            .map_or("pending backup".to_string(), |pending| {
                let hash = pending.replace("pending_", "");
                hash[..8.min(hash.len())].to_string()
            });
        let warning = format!("Reusing from {}: {}", pending_name, reused_data.join(", "));

        if is_json_mode() {
            emit_warning(&warning, "pending_backup_reuse");
        } else {
            println!("{}", style(warning).yellow());
        }
    }

    Ok(ResolvedBackup {
        options: BackupOptions {
            key,
            root_path_string,
            storage,
            fs,
            author: config.author,
            compress,
            password,
            chunk_size,
            ignore_patterns,
            concurrency,
        },
        message,
        parent_hash,
        pending_backup,
    })
}

fn load_config(config_path: &Path) -> Result<Config, String> {
    if !config_path.exists() {
        return Err(
            "Seems like you didn't configure your backup tool yet. Run 'gib config' to configure your backup tool."
                .to_string(),
        );
    }

    let config_bytes = std::fs::read(config_path)
        .map_err(|e| format!("Failed to read config file: {}", e))?;
    rmp_serde::from_slice(&config_bytes)
        .map_err(|e| format!("Failed to deserialize config: {}", e))
}

fn list_storage_names(storage_dir: &Path) -> Result<Vec<String>, String> {
    if !storage_dir.exists() {
        return Err("Seems like you didn't create any storage yet. Run 'gib storage add' to create a storage.".to_string());
    }

    let mut names = std::fs::read_dir(storage_dir)
        .map_err(|e| format!("Failed to read storages: {}", e))?
        .map(|entry| {
            entry
                .map_err(|e| format!("Failed to read storage entry: {}", e))
                .map(|entry| {
                    entry
                        .path()
                        .file_stem()
                        .map(|stem| stem.to_string_lossy().to_string())
                        .ok_or_else(|| "Storage entry has no name".to_string())
                })
                .and_then(|name| name)
        })
        .collect::<Result<Vec<_>, _>>()?;

    names.sort();
    names.dedup();
    if names.is_empty() {
        return Err("Seems like you didn't create any storage yet. Run 'gib storage add' to create a storage.".to_string());
    }
    Ok(names)
}

fn load_storage_config(storage_dir: &Path, name: &str) -> Result<crate::commands::storage::add::Storage, String> {
    let path = storage_dir.join(format!("{}.msgpack", name));
    let bytes = std::fs::read(&path)
        .map_err(|e| format!("Failed to read storage '{}': {}", name, e))?;
    rmp_serde::from_slice(&bytes)
        .map_err(|e| format!("Failed to parse storage '{}': {}", name, e))
}

pub(crate) async fn latest_backup_hash(
    fs: Arc<dyn FS>,
    key: String,
    password: Option<String>,
) -> Result<Option<String>, String> {
    let summaries = list_backup_summaries(fs, key, password).await?;
    Ok(summaries.first().map(|summary| summary.hash.clone()))
}

async fn resolve_parent_hash(
    fs: Arc<dyn FS>,
    key: String,
    password: Option<String>,
    matches: &ArgMatches,
) -> Result<Option<String>, String> {
    if !matches.contains_id("parent") {
        return Ok(None);
    }

    match matches.get_one::<String>("parent") {
        Some(hash) => resolve_backup_hash(fs, key, password, Some(hash.to_string()))
            .await
            .map(Some),
        None => resolve_backup_hash(fs, key, password, None).await.map(Some),
    }
}

async fn resolve_backup_hash(
    fs: Arc<dyn FS>,
    key: String,
    password: Option<String>,
    provided_hash: Option<String>,
) -> Result<String, String> {
    match provided_hash {
        Some(hash) => {
            if hash.len() <= 8 {
                let summaries = list_backup_summaries(fs, key, password).await?;

                for summary in summaries {
                    if summary.hash.starts_with(&hash) {
                        return Ok(summary.hash);
                    }
                }

                Err(format!("No backup found matching hash prefix: {}", hash))
            } else {
                Ok(hash)
            }
        }
        None => {
            let summaries = list_backup_summaries(fs, key, password).await?;

            if summaries.is_empty() {
                return Err("No backups found in repository".to_string());
            }

            let recent_backups: Vec<BackupSummaryDisplay> = summaries
                .iter()
                .take(10)
                .map(|s| BackupSummaryDisplay {
                    hash: s.hash.clone(),
                    message: s.message.clone(),
                })
                .collect();

            if recent_backups.is_empty() {
                return Err("No backups found in repository".to_string());
            }

            let items: Vec<String> = recent_backups
                .iter()
                .map(|c| format!("{} {}", &c.hash[..8.min(c.hash.len())], &c.message))
                .collect();

            let selected_index = Select::new()
                .with_prompt("Select a parent backup")
                .items(&items)
                .default(0)
                .interact()
                .map_err(|e| format!("Failed to select parent backup: {}", e))?;

            Ok(recent_backups[selected_index].hash.clone())
        }
    }
}

async fn load_backup(
    fs: Arc<dyn FS>,
    key: String,
    password: Option<String>,
    backup_hash: &str,
) -> Result<Backup, String> {
    let backup_path = format!("{}/backups/{}", key, backup_hash);

    let read_result = read_file_maybe_decrypt(
        &fs,
        &backup_path,
        password.as_deref(),
        "Backup is encrypted but no password provided",
    )
    .await?;

    if read_result.bytes.is_empty() {
        return Err(format!("Backup {} not found or is empty", backup_hash));
    }

    let decompressed_bytes = decompress_bytes(&read_result.bytes);

    let backup: Backup = rmp_serde::from_slice(&decompressed_bytes)
        .map_err(|e| format!("Failed to deserialize backup: {}", e))?;

    Ok(backup)
}

fn increment_chunk_refcounts(
    chunk_indexes: &mut HashMap<String, ChunkIndex>,
    tree: &HashMap<String, BackupObject>,
) {
    for backup_object in tree.values() {
        for chunk_hash in &backup_object.chunks {
            let entry = chunk_indexes
                .entry(chunk_hash.clone())
                .or_insert(ChunkIndex { refcount: 0 });
            entry.refcount += 1;
        }
    }
}

fn decrement_chunk_refcounts(
    chunk_indexes: &Arc<Mutex<HashMap<String, ChunkIndex>>>,
    backup_object: &BackupObject,
) {
    let mut chunk_indexes_guard = chunk_indexes.lock().unwrap();
    decrement_chunk_refcounts_from_map(&mut chunk_indexes_guard, backup_object);
}

fn decrement_chunk_refcounts_from_map(
    chunk_indexes: &mut HashMap<String, ChunkIndex>,
    backup_object: &BackupObject,
) {
    for chunk_hash in &backup_object.chunks {
        if let Some(entry) = chunk_indexes.get_mut(chunk_hash)
            && entry.refcount > 0
        {
            entry.refcount -= 1;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::fs::LocalFS;
    use std::path::PathBuf;

    fn backup_object(chunk: &str) -> BackupObject {
        BackupObject {
            hash: format!("file-{chunk}"),
            size: 1,
            content_type: "application/octet-stream".to_string(),
            permissions: 0o644,
            chunks: vec![chunk.to_string()],
        }
    }

    fn test_directory() -> PathBuf {
        let timestamp = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let path = std::env::temp_dir().join(format!("gib-backup-test-{}", timestamp));
        std::fs::create_dir_all(&path).unwrap();
        path
    }

    #[test]
    fn incremental_snapshots_remove_files_missing_from_the_current_tree() {
        let mut tree = HashMap::from([
            ("kept.txt".to_string(), backup_object("kept-chunk")),
            ("deleted.txt".to_string(), backup_object("deleted-chunk")),
        ]);
        let mut chunk_indexes = HashMap::from([
            ("kept-chunk".to_string(), ChunkIndex { refcount: 1 }),
            ("deleted-chunk".to_string(), ChunkIndex { refcount: 1 }),
        ]);

        remove_missing_backup_objects(
            &mut tree,
            &mut chunk_indexes,
            &["/workspace/kept.txt".to_string()],
            "/workspace",
            &[],
        );

        assert!(tree.contains_key("kept.txt"));
        assert!(!tree.contains_key("deleted.txt"));
        assert_eq!(chunk_indexes["kept-chunk"].refcount, 1);
        assert_eq!(chunk_indexes["deleted-chunk"].refcount, 0);
    }

    #[test]
    fn incremental_snapshots_preserve_ignored_parent_objects() {
        let mut tree = HashMap::from([(
            "ignored/file.txt".to_string(),
            backup_object("ignored-chunk"),
        )]);
        let mut chunk_indexes = HashMap::from([(
            "ignored-chunk".to_string(),
            ChunkIndex { refcount: 1 },
        )]);

        remove_missing_backup_objects(
            &mut tree,
            &mut chunk_indexes,
            &[],
            "/workspace",
            &["ignored".to_string()],
        );

        assert!(tree.contains_key("ignored/file.txt"));
        assert_eq!(chunk_indexes["ignored-chunk"].refcount, 1);
    }

    #[tokio::test]
    async fn run_backup_applies_deletions_to_the_next_snapshot() {
        let fixture = test_directory();
        let source = fixture.join("source");
        let storage = fixture.join("storage");
        std::fs::create_dir_all(&source).unwrap();
        std::fs::write(source.join("kept.txt"), b"kept").unwrap();
        std::fs::write(source.join("deleted.txt"), b"deleted").unwrap();

        let options = BackupOptions {
            key: "project".to_string(),
            root_path_string: source.to_string_lossy().to_string(),
            storage: "test".to_string(),
            fs: Arc::new(LocalFS::new(&storage)),
            author: "tester <tester@example.com>".to_string(),
            compress: 3,
            password: None,
            chunk_size: 1024,
            ignore_patterns: Vec::new(),
            concurrency: 1,
        };

        let first = run_backup(
            options.clone(),
            "[WATCH] first".to_string(),
            None,
            None,
        )
        .await
        .unwrap();
        assert!(first.backup.tree.contains_key("kept.txt"));
        assert!(first.backup.tree.contains_key("deleted.txt"));

        std::fs::remove_file(source.join("deleted.txt")).unwrap();

        let second = run_backup(
            options,
            "[WATCH] deleted".to_string(),
            Some(first.backup.hash),
            None,
        )
        .await
        .unwrap();

        assert!(second.backup.tree.contains_key("kept.txt"));
        assert!(!second.backup.tree.contains_key("deleted.txt"));

        let _ = std::fs::remove_dir_all(fixture);
    }
}
