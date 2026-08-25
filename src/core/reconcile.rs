use crate::core::crypto::read_file_maybe_decrypt;
use crate::core::metadata::{Backup, BackupObject};
use crate::core::permissions::{get_file_permissions_with_path, set_file_permissions};
use crate::fs::FS;
use sha2::{Digest, Sha256};
use std::collections::{BTreeSet, HashMap};
use std::io::{Read, Write};
use std::path::{Component, Path, PathBuf};
use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};
use walkdir::WalkDir;

const MAX_TEXT_MERGE_BYTES: u64 = 512 * 1024;
const MAX_TEXT_MERGE_LINES: usize = 2_048;

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct LocalFile {
    pub(crate) hash: String,
    pub(crate) size: u64,
    pub(crate) permissions: u32,
}

#[derive(Clone, Debug)]
pub(crate) struct ReconcileConflict {
    pub(crate) path: String,
    pub(crate) reason: String,
    pub(crate) remote: Option<BackupObject>,
}

#[derive(Debug, Default)]
pub(crate) struct ReconciliationResult {
    pub(crate) applied_remote: usize,
    pub(crate) merged_text: usize,
    pub(crate) conflicts: Vec<ReconcileConflict>,
}

pub(crate) fn scan_worktree(
    root: &Path,
    ignore_patterns: &[String],
) -> Result<HashMap<String, LocalFile>, String> {
    let mut files = HashMap::new();
    let walker = WalkDir::new(root)
        .into_iter()
        .filter_entry(|entry| !is_ignored_path(entry.path(), root, ignore_patterns));

    for entry in walker {
        let entry = entry.map_err(|error| {
            format!("Failed to scan watch root '{}': {}", root.display(), error)
        })?;
        if !entry.file_type().is_file() {
            continue;
        }

        let relative = entry
            .path()
            .strip_prefix(root)
            .map_err(|error| format!("Failed to derive a relative path: {}", error))?
            .to_string_lossy()
            .replace('\\', "/");
        let metadata = entry
            .metadata()
            .map_err(|error| format!("Failed to inspect '{}': {}", relative, error))?;
        let hash = hash_file(entry.path())
            .map_err(|error| format!("Failed to hash '{}': {}", relative, error))?;

        files.insert(
            relative,
            LocalFile {
                hash,
                size: metadata.len(),
                permissions: get_file_permissions_with_path(
                    &metadata,
                    entry.path().to_string_lossy().as_ref(),
                ),
            },
        );
    }

    Ok(files)
}

pub(crate) async fn reconcile_worktree(
    root: &Path,
    ignore_patterns: &[String],
    base: Option<&Backup>,
    remote: &Backup,
    fs: Arc<dyn FS>,
    key: &str,
    password: Option<&str>,
) -> Result<ReconciliationResult, String> {
    let local = scan_worktree(root, ignore_patterns)?;
    let base_tree = base.map(|backup| &backup.tree);
    let remote_tree = &remote.tree;
    let mut paths = BTreeSet::new();

    for path in local.keys() {
        if !is_ignored_relative(path, ignore_patterns) {
            paths.insert(path.clone());
        }
    }
    if let Some(tree) = base_tree {
        for path in tree.keys() {
            if !is_ignored_relative(path, ignore_patterns) {
                paths.insert(path.clone());
            }
        }
    }
    for path in remote_tree.keys() {
        if !is_ignored_relative(path, ignore_patterns) {
            paths.insert(path.clone());
        }
    }

    let mut result = ReconciliationResult::default();

    for path in paths {
        let target = safe_target_path(root, &path)?;
        let local_file = local.get(&path);
        let base_file = base_tree.and_then(|tree| tree.get(&path));
        let remote_file = remote_tree.get(&path);

        if local_matches_backup(local_file, remote_file) {
            continue;
        }

        if local_matches_backup(local_file, base_file) {
            apply_remote_change(root, &path, remote_file, fs.clone(), key, password).await?;
            result.applied_remote += 1;
            continue;
        }

        if backup_matches_backup(remote_file, base_file) {
            continue;
        }

        if let (Some(local_file), Some(remote_file), Some(base_file)) =
            (local_file, remote_file, base_file)
        {
            if local_file.hash == remote_file.hash {
                if local_file.permissions != remote_file.permissions {
                    apply_remote_change(root, &path, Some(remote_file), fs.clone(), key, password)
                        .await?;
                    result.applied_remote += 1;
                }
                continue;
            }

            if local_file.size <= MAX_TEXT_MERGE_BYTES
                && base_file.size <= MAX_TEXT_MERGE_BYTES
                && remote_file.size <= MAX_TEXT_MERGE_BYTES
            {
                let local_bytes = std::fs::read(&target)
                    .map_err(|error| format!("Failed to read local file '{}': {}", path, error))?;
                let base_bytes =
                    read_backup_object_bytes(&fs, key, password, base_file, MAX_TEXT_MERGE_BYTES)
                        .await?;
                let remote_bytes =
                    read_backup_object_bytes(&fs, key, password, remote_file, MAX_TEXT_MERGE_BYTES)
                        .await?;

                if is_text_bytes(&local_bytes)
                    && is_text_bytes(&base_bytes)
                    && is_text_bytes(&remote_bytes)
                {
                    if let Some(merged) = three_way_merge(&base_bytes, &local_bytes, &remote_bytes)
                    {
                        write_bytes_atomically(root, &path, &merged, local_file.permissions)?;
                        result.merged_text += 1;
                        continue;
                    }
                }
            }
        }

        result.conflicts.push(ReconcileConflict {
            path,
            reason: conflict_reason(local_file, base_file, remote_file),
            remote: remote_file.cloned(),
        });
    }

    Ok(result)
}

pub(crate) async fn apply_remote_change(
    root: &Path,
    relative_path: &str,
    remote: Option<&BackupObject>,
    fs: Arc<dyn FS>,
    key: &str,
    password: Option<&str>,
) -> Result<(), String> {
    let target = safe_target_path(root, relative_path)?;
    match remote {
        Some(remote) => {
            let parent = target
                .parent()
                .ok_or_else(|| format!("Path '{}' has no parent directory", relative_path))?;
            std::fs::create_dir_all(parent).map_err(|error| {
                format!(
                    "Failed to create parent directory for '{}': {}",
                    relative_path, error
                )
            })?;

            let temporary = temporary_path(&target);
            let mut file = std::fs::File::create(&temporary).map_err(|error| {
                format!(
                    "Failed to create temporary file for '{}': {}",
                    relative_path, error
                )
            })?;
            let mut hasher = Sha256::new();
            let mut written = 0u64;

            for chunk_hash in &remote.chunks {
                let chunk_path = chunk_path(key, chunk_hash);
                let chunk_data = read_file_maybe_decrypt(
                    &fs,
                    &chunk_path,
                    password,
                    "Chunk is encrypted but no password provided",
                )
                .await?;
                let decompressed = decompress_chunk(&chunk_data.bytes, relative_path)?;
                written = written.saturating_add(decompressed.len() as u64);
                hasher.update(&decompressed);
                file.write_all(&decompressed)
                    .map_err(|error| format!("Failed to write '{}': {}", relative_path, error))?;
            }

            file.flush().map_err(|error| {
                format!(
                    "Failed to flush restored file '{}': {}",
                    relative_path, error
                )
            })?;
            drop(file);

            if written != remote.size || format!("{:x}", hasher.finalize()) != remote.hash {
                let _ = std::fs::remove_file(&temporary);
                return Err(format!(
                    "Remote file '{}' failed its integrity check",
                    relative_path
                ));
            }

            set_file_permissions(&temporary, remote.permissions).map_err(|error| {
                format!(
                    "Failed to set permissions for '{}': {}",
                    relative_path, error
                )
            })?;
            replace_file(&temporary, &target).map_err(|error| {
                format!("Failed to apply remote file '{}': {}", relative_path, error)
            })?;
        }
        None => {
            if target.exists() {
                std::fs::remove_file(&target)
                    .map_err(|error| format!("Failed to remove '{}': {}", relative_path, error))?;
                cleanup_empty_parents(&target, root);
            }
        }
    }

    Ok(())
}

pub(crate) fn worktree_matches_backup(
    root: &Path,
    ignore_patterns: &[String],
    backup: &Backup,
) -> Result<bool, String> {
    let local = scan_worktree(root, ignore_patterns)?;
    let expected: HashMap<String, &BackupObject> = backup
        .tree
        .iter()
        .filter(|(path, _)| !is_ignored_relative(path, ignore_patterns))
        .map(|(path, object)| (path.clone(), object))
        .collect();

    if local.len() != expected.len() {
        return Ok(false);
    }

    Ok(local.iter().all(|(path, local_file)| {
        expected
            .get(path)
            .is_some_and(|remote| local_matches_backup(Some(local_file), Some(remote)))
    }))
}

fn local_matches_backup(local: Option<&LocalFile>, backup: Option<&BackupObject>) -> bool {
    match (local, backup) {
        (None, None) => true,
        (Some(local), Some(backup)) => {
            local.hash == backup.hash
                && local.size == backup.size
                && local.permissions == backup.permissions
        }
        _ => false,
    }
}

fn backup_matches_backup(left: Option<&BackupObject>, right: Option<&BackupObject>) -> bool {
    match (left, right) {
        (None, None) => true,
        (Some(left), Some(right)) => {
            left.hash == right.hash
                && left.size == right.size
                && left.permissions == right.permissions
        }
        _ => false,
    }
}

fn conflict_reason(
    local: Option<&LocalFile>,
    base: Option<&BackupObject>,
    remote: Option<&BackupObject>,
) -> String {
    match (local.is_some(), base.is_some(), remote.is_some()) {
        (true, true, false) => "the file was changed locally while it was deleted remotely",
        (false, true, true) => "the file was deleted locally while it was changed remotely",
        (true, false, true) => "the file was created independently on both machines",
        _ => "both machines changed the same file",
    }
    .to_string()
}

async fn read_backup_object_bytes(
    fs: &Arc<dyn FS>,
    key: &str,
    password: Option<&str>,
    object: &BackupObject,
    max_bytes: u64,
) -> Result<Vec<u8>, String> {
    if object.size > max_bytes {
        return Err("file exceeds the in-memory text merge limit".to_string());
    }

    let mut bytes = Vec::with_capacity(object.size as usize);
    for chunk_hash in &object.chunks {
        let data = read_file_maybe_decrypt(
            fs,
            &chunk_path(key, chunk_hash),
            password,
            "Chunk is encrypted but no password provided",
        )
        .await?;
        let decompressed = decompress_chunk(&data.bytes, "text merge")?;
        if bytes.len().saturating_add(decompressed.len()) > max_bytes as usize {
            return Err("file exceeds the in-memory text merge limit".to_string());
        }
        bytes.extend_from_slice(&decompressed);
    }
    Ok(bytes)
}

fn chunk_path(key: &str, chunk_hash: &str) -> String {
    let (prefix, rest) = chunk_hash.split_at(2);
    format!("{}/chunks/{}/{}", key, prefix, rest)
}

fn decompress_chunk(bytes: &[u8], context: &str) -> Result<Vec<u8>, String> {
    zstd::decode_all(bytes).map_err(|error| {
        format!(
            "Failed to decompress repository chunk while processing '{}': {}",
            context, error
        )
    })
}

fn hash_file(path: &Path) -> Result<String, std::io::Error> {
    let mut file = std::fs::File::open(path)?;
    let mut hasher = Sha256::new();
    let mut buffer = vec![0u8; 64 * 1024];

    loop {
        let read = file.read(&mut buffer)?;
        if read == 0 {
            break;
        }
        hasher.update(&buffer[..read]);
    }

    Ok(format!("{:x}", hasher.finalize()))
}

fn is_text_bytes(bytes: &[u8]) -> bool {
    !bytes.contains(&0) && std::str::from_utf8(bytes).is_ok()
}

fn three_way_merge(base: &[u8], local: &[u8], remote: &[u8]) -> Option<Vec<u8>> {
    let base = std::str::from_utf8(base).ok()?;
    let local = std::str::from_utf8(local).ok()?;
    let remote = std::str::from_utf8(remote).ok()?;
    let base_lines = split_lines(base);
    let local_lines = split_lines(local);
    let remote_lines = split_lines(remote);

    if base_lines.len() > MAX_TEXT_MERGE_LINES
        || local_lines.len() > MAX_TEXT_MERGE_LINES
        || remote_lines.len() > MAX_TEXT_MERGE_LINES
    {
        return None;
    }

    let local_edits = diff_hunks(&base_lines, &local_lines);
    let remote_edits = diff_hunks(&base_lines, &remote_lines);
    merge_hunks(&base_lines, &local_edits, &remote_edits)
}

#[derive(Clone, Debug)]
struct Hunk {
    start: usize,
    end: usize,
    replacement: Vec<String>,
}

fn split_lines(value: &str) -> Vec<String> {
    value
        .split_inclusive('\n')
        .map(ToString::to_string)
        .collect()
}

fn diff_hunks(base: &[String], variant: &[String]) -> Vec<Hunk> {
    let rows = base.len() + 1;
    let columns = variant.len() + 1;
    let mut lcs = vec![vec![0usize; columns]; rows];

    for base_index in (0..base.len()).rev() {
        for variant_index in (0..variant.len()).rev() {
            lcs[base_index][variant_index] = if base[base_index] == variant[variant_index] {
                lcs[base_index + 1][variant_index + 1] + 1
            } else {
                lcs[base_index + 1][variant_index].max(lcs[base_index][variant_index + 1])
            };
        }
    }

    let mut hunks = Vec::new();
    let mut base_index = 0;
    let mut variant_index = 0;

    while base_index < base.len() || variant_index < variant.len() {
        if base_index < base.len()
            && variant_index < variant.len()
            && base[base_index] == variant[variant_index]
        {
            base_index += 1;
            variant_index += 1;
            continue;
        }

        let start = base_index;
        let mut replacement = Vec::new();
        while base_index < base.len() || variant_index < variant.len() {
            if base_index < base.len()
                && variant_index < variant.len()
                && base[base_index] == variant[variant_index]
            {
                break;
            }

            let can_insert = variant_index < variant.len()
                && (base_index == base.len()
                    || lcs[base_index][variant_index + 1] >= lcs[base_index + 1][variant_index]);
            if can_insert {
                replacement.push(variant[variant_index].clone());
                variant_index += 1;
            } else {
                base_index += 1;
            }
        }

        hunks.push(Hunk {
            start,
            end: base_index,
            replacement,
        });
    }

    hunks
}

fn merge_hunks(base: &[String], local: &[Hunk], remote: &[Hunk]) -> Option<Vec<u8>> {
    let mut output = String::new();
    let mut local_index = 0;
    let mut remote_index = 0;
    let mut cursor = 0;

    while local_index < local.len() || remote_index < remote.len() {
        let next_local = local.get(local_index);
        let next_remote = remote.get(remote_index);
        let next_start = match (next_local, next_remote) {
            (Some(local), Some(remote)) => local.start.min(remote.start),
            (Some(local), None) => local.start,
            (None, Some(remote)) => remote.start,
            (None, None) => break,
        };

        output.push_str(&base[cursor..next_start].concat());
        cursor = next_start;

        match (next_local, next_remote) {
            (Some(local), Some(remote)) if hunks_overlap(local, remote) => {
                if local.start == remote.start
                    && local.end == remote.end
                    && local.replacement == remote.replacement
                {
                    output.push_str(&local.replacement.concat());
                    cursor = local.end.max(remote.end);
                    local_index += 1;
                    remote_index += 1;
                } else {
                    return None;
                }
            }
            (Some(local), Some(remote)) if local.start <= remote.start => {
                output.push_str(&local.replacement.concat());
                cursor = local.end;
                local_index += 1;
            }
            (Some(_), Some(remote)) => {
                output.push_str(&remote.replacement.concat());
                cursor = remote.end;
                remote_index += 1;
            }
            (Some(local), None) => {
                output.push_str(&local.replacement.concat());
                cursor = local.end;
                local_index += 1;
            }
            (None, Some(remote)) => {
                output.push_str(&remote.replacement.concat());
                cursor = remote.end;
                remote_index += 1;
            }
            (None, None) => break,
        }
    }

    output.push_str(&base[cursor..].concat());
    Some(output.into_bytes())
}

fn hunks_overlap(left: &Hunk, right: &Hunk) -> bool {
    if left.start == left.end && right.start == right.end {
        return left.start == right.start;
    }
    if left.start == left.end {
        return left.start >= right.start && left.start <= right.end;
    }
    if right.start == right.end {
        return right.start >= left.start && right.start <= left.end;
    }
    left.start < right.end && right.start < left.end
}

fn safe_target_path(root: &Path, relative_path: &str) -> Result<PathBuf, String> {
    let relative = Path::new(relative_path);
    if relative.is_absolute()
        || relative.components().any(|component| {
            matches!(
                component,
                Component::ParentDir | Component::RootDir | Component::Prefix(_)
            )
        })
    {
        return Err(format!(
            "Refusing to reconcile unsafe repository path '{}'",
            relative_path
        ));
    }
    Ok(root.join(relative))
}

fn temporary_path(target: &Path) -> PathBuf {
    let stamp = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_nanos())
        .unwrap_or_default();
    let name = target
        .file_name()
        .map(|name| name.to_string_lossy())
        .unwrap_or_else(|| std::borrow::Cow::Borrowed("file"));
    target.with_file_name(format!(".{}.gib-{}-{}", name, std::process::id(), stamp))
}

fn write_bytes_atomically(
    root: &Path,
    relative_path: &str,
    bytes: &[u8],
    permissions: u32,
) -> Result<(), String> {
    let target = safe_target_path(root, relative_path)?;
    let parent = target
        .parent()
        .ok_or_else(|| format!("Path '{}' has no parent directory", relative_path))?;
    std::fs::create_dir_all(parent)
        .map_err(|error| format!("Failed to create parent directory: {}", error))?;
    let temporary = temporary_path(&target);
    std::fs::write(&temporary, bytes)
        .map_err(|error| format!("Failed to write merged file '{}': {}", relative_path, error))?;
    set_file_permissions(&temporary, permissions)
        .map_err(|error| format!("Failed to set merged file permissions: {}", error))?;
    replace_file(&temporary, &target)
        .map_err(|error| format!("Failed to apply merged file '{}': {}", relative_path, error))
}

fn replace_file(temporary: &Path, target: &Path) -> Result<(), std::io::Error> {
    match std::fs::rename(temporary, target) {
        Ok(()) => Ok(()),
        Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {
            std::fs::remove_file(target)?;
            std::fs::rename(temporary, target)
        }
        Err(error) => Err(error),
    }
}

fn cleanup_empty_parents(path: &Path, root: &Path) {
    let mut current = path.parent();
    while let Some(directory) = current {
        if directory == root {
            break;
        }
        let empty = std::fs::read_dir(directory)
            .map(|mut entries| entries.next().is_none())
            .unwrap_or(false);
        if !empty || std::fs::remove_dir(directory).is_err() {
            break;
        }
        current = directory.parent();
    }
}

fn is_ignored_path(path: &Path, root: &Path, ignore_patterns: &[String]) -> bool {
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

fn is_ignored_relative(path: &str, ignore_patterns: &[String]) -> bool {
    path.split('/')
        .any(|component| ignore_patterns.iter().any(|pattern| pattern == component))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn merges_non_overlapping_text_changes() {
        let merged = three_way_merge(
            b"one\ntwo\nthree\n",
            b"ONE\ntwo\nthree\n",
            b"one\ntwo\nTHREE\n",
        )
        .unwrap();
        assert_eq!(merged, b"ONE\ntwo\nTHREE\n");
    }

    #[test]
    fn rejects_overlapping_text_changes() {
        assert!(three_way_merge(b"one\ntwo\n", b"one\nlocal\n", b"one\nremote\n").is_none());
    }

    #[test]
    fn treats_identical_edits_as_clean() {
        assert_eq!(
            three_way_merge(b"one\n", b"ONE\n", b"ONE\n").unwrap(),
            b"ONE\n"
        );
    }
}
