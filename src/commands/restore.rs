use crate::core::crypto::get_password;
use crate::core::crypto::read_file_maybe_decrypt;
use crate::core::indexes::list_backup_summaries;
use crate::core::metadata::Backup;
use crate::core::permissions::set_file_permissions;
use crate::fs::FS;
use crate::output::{JsonProgress, emit_output, emit_progress_message, emit_warning, is_json_mode};
use crate::utils::{decompress_bytes, get_fs, get_pwd_string, get_storage, handle_error};
use clap::ArgMatches;
use crossterm::{
    cursor,
    event::{self, Event, KeyCode, KeyEventKind, KeyModifiers},
    execute, queue,
    terminal::{self, ClearType},
};
use dialoguer::Select;
use dirs::home_dir;
use futures::stream::{self, StreamExt};
use indicatif::{ProgressBar, ProgressStyle};
use sha2::{Digest, Sha256};
use std::cmp::Ordering;
use std::collections::{HashMap, HashSet};
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio::sync::{Mutex as TokioMutex, Semaphore};
use tokio::task::JoinSet;
use walkdir::WalkDir;

const MAX_CONCURRENT_FILES: usize = 100;

pub async fn restore(matches: &ArgMatches) {
    let (key, storage, password, backup_hash, target_path, prune_local, only_request) =
        match get_params(matches) {
            Ok(params) => params,
            Err(e) => handle_error(e, None),
        };

    let started_at = Instant::now();

    let storage = get_storage(&storage);

    let fs = get_fs(&storage, None);

    let full_backup_hash = match resolve_backup_hash(
        Arc::clone(&fs),
        key.clone(),
        password.clone(),
        backup_hash,
    )
    .await
    {
        Ok(hash) => hash,
        Err(e) => handle_error(e, None),
    };

    let pb = if is_json_mode() {
        ProgressBar::hidden()
    } else {
        let pb = ProgressBar::new(100);
        pb.enable_steady_tick(Duration::from_millis(100));
        pb.set_style(ProgressStyle::with_template("{spinner:.green} {msg}").unwrap());
        pb.set_message("Loading backup data...");
        pb
    };

    if is_json_mode() {
        emit_progress_message("Loading backup data...");
    }

    let backup = match load_backup(
        Arc::clone(&fs),
        key.clone(),
        password.clone(),
        &full_backup_hash,
    )
    .await
    {
        Ok(backup) => backup,
        Err(e) => handle_error(e, Some(&pb)),
    };

    pb.finish_and_clear();

    let files_to_restore = match only_request {
        OnlyRequest::None => backup
            .tree
            .iter()
            .map(|(k, v)| (k.clone(), v.clone()))
            .collect(),
        OnlyRequest::Paths(paths) => match filter_only_paths(&backup.tree, &paths) {
            Ok(files) => files,
            Err(e) => handle_error(e, None),
        },
        OnlyRequest::Interactive => {
            let selected_paths = match select_only_paths_interactive(&backup.tree) {
                Ok(paths) => paths,
                Err(e) => handle_error(e, None),
            };
            match filter_only_paths(&backup.tree, &selected_paths) {
                Ok(files) => files,
                Err(e) => handle_error(e, None),
            }
        }
    };

    let total_files = files_to_restore.len() as u64;

    let json_progress = if is_json_mode() {
        let progress = JsonProgress::new(total_files);
        progress.set_message(&format!(
            "Restoring files from {}...",
            full_backup_hash[..8.min(full_backup_hash.len())].to_string()
        ));
        Some(progress)
    } else {
        None
    };

    let pb = if is_json_mode() {
        ProgressBar::hidden()
    } else {
        let pb = ProgressBar::new(total_files);
        pb.enable_steady_tick(Duration::from_millis(100));
        pb.set_style(
            ProgressStyle::with_template(
                "[{elapsed_precise}] [{bar:40.cyan/blue}] {pos}/{len} {msg}",
            )
            .unwrap(),
        );
        pb.set_message(format!(
            "Restoring files from {}...",
            full_backup_hash[..8.min(full_backup_hash.len())].to_string()
        ));
        pb
    };

    let files_set = Arc::new(TokioMutex::new(JoinSet::new()));
    let restored_files = Arc::new(std::sync::Mutex::new(0u64));
    let skipped_files = Arc::new(std::sync::Mutex::new(0u64));
    let semaphore = Arc::new(Semaphore::new(MAX_CONCURRENT_FILES));

    let files_stream = stream::iter(files_to_restore);

    files_stream
        .for_each_concurrent(MAX_CONCURRENT_FILES, |(relative_path, backup_object)| {
            let pb_clone = pb.clone();
            let fs_clone = Arc::clone(&fs);
            let key_clone = key.clone();
            let password_clone = password.clone();
            let target_path_clone = target_path.clone();
            let relative_path_clone = relative_path.clone();
            let restored_files_clone = Arc::clone(&restored_files);
            let skipped_files_clone = Arc::clone(&skipped_files);
            let semaphore_clone = Arc::clone(&semaphore);
            let files_set_clone = Arc::clone(&files_set);
            let json_progress_clone = json_progress.clone();

            async move {
                let mut guard = files_set_clone.lock().await;
                guard.spawn(async move {
                    let _permit = semaphore_clone.acquire().await.expect("Semaphore closed");
                    let local_path = Path::new(&target_path_clone).join(&relative_path_clone);

                    let needs_restore = if local_path.exists() {
                        match calculate_file_hash(&local_path) {
                            Ok(local_hash) => local_hash != backup_object.hash,
                            Err(_) => true,
                        }
                    } else {
                        true
                    };

                    if !needs_restore {
                        {
                            let mut skipped = skipped_files_clone.lock().unwrap();
                            *skipped += 1;
                        }
                        if let Some(progress) = &json_progress_clone {
                            progress.inc_by(1);
                        } else {
                            pb_clone.inc(1);
                        }
                        return Ok(());
                    }

                    if let Some(parent) = local_path.parent() {
                        std::fs::create_dir_all(parent).map_err(|e| {
                            format!(
                                "Failed to create parent directory for {}: {}",
                                relative_path_clone, e
                            )
                        })?;
                    }

                    let mut file = std::fs::File::create(&local_path).map_err(|e| {
                        format!("Failed to create file {}: {}", relative_path_clone, e)
                    })?;

                    for chunk_hash in &backup_object.chunks {
                        let (prefix, rest) = chunk_hash.split_at(2);
                        let chunk_path = format!("{}/chunks/{}/{}", key_clone, prefix, rest);

                        let chunk_data = read_file_maybe_decrypt(
                            &fs_clone,
                            &chunk_path,
                            password_clone.as_deref(),
                            "Chunk is encrypted but no password provided",
                        )
                        .await
                        .map_err(|e| format!("Failed to read chunk {}: {}", chunk_hash, e))?;

                        let decompressed = decompress_bytes(&chunk_data.bytes);

                        file.write_all(&decompressed).map_err(|e| {
                            format!(
                                "Failed to write chunk {} to file {}: {}",
                                chunk_hash, relative_path_clone, e
                            )
                        })?;
                    }

                    set_file_permissions(&local_path, backup_object.permissions).map_err(|e| {
                        format!(
                            "Failed to set permissions for {}: {}",
                            relative_path_clone, e
                        )
                    })?;

                    {
                        let mut restored = restored_files_clone.lock().unwrap();
                        *restored += 1;
                    }

                    if let Some(progress) = &json_progress_clone {
                        progress.inc_by(1);
                    } else {
                        pb_clone.inc(1);
                    }
                    Ok(())
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

    if !failed_files.is_empty() {
        handle_error(
            format!(
                "Failed to restore {} files:\n{}",
                failed_files.len(),
                failed_files
                    .iter()
                    .map(|f| format!("  - {}", f))
                    .collect::<Vec<String>>()
                    .join("\n")
            ),
            Some(&pb),
        );
    }

    let deleted_count = if prune_local {
        pb.set_message("Cleaning up files not in backup...");
        if is_json_mode() {
            emit_progress_message("Cleaning up files not in backup...");
        }
        match cleanup_extra_files(&target_path, &backup.tree) {
            Ok(count) => count,
            Err(e) => {
                emit_warning(
                    &format!("Failed to clean up extra files: {}", e),
                    "cleanup_failed",
                );
                0
            }
        }
    } else {
        0
    };

    let restored_count = *restored_files.lock().unwrap();
    let skipped_count = *skipped_files.lock().unwrap();

    if is_json_mode() {
        #[derive(serde::Serialize)]
        struct RestoreOutput {
            backup: String,
            backup_short: String,
            restored: u64,
            skipped: u64,
            deleted_local: u64,
            target_path: String,
            elapsed_ms: u64,
        }

        let payload = RestoreOutput {
            backup: full_backup_hash.clone(),
            backup_short: full_backup_hash[..8.min(full_backup_hash.len())].to_string(),
            restored: restored_count,
            skipped: skipped_count,
            deleted_local: deleted_count,
            target_path: target_path.clone(),
            elapsed_ms: started_at.elapsed().as_millis() as u64,
        };
        emit_output(&payload);
    } else {
        let elapsed = pb.elapsed();
        pb.set_style(ProgressStyle::with_template("{prefix:.green} {msg}").unwrap());
        pb.set_prefix("OK");

        if deleted_count > 0 {
            pb.finish_with_message(format!(
                "Restored {} files, skipped {} files, deleted {} files ({:.2?})",
                restored_count, skipped_count, deleted_count, elapsed
            ));
        } else {
            pb.finish_with_message(format!(
                "Restored {} files, skipped {} files ({:.2?})",
                restored_count, skipped_count, elapsed
            ));
        }
    }
}

enum OnlyRequest {
    None,
    Paths(Vec<String>),
    Interactive,
}

struct TreeNode {
    name: String,
    path: String,
    is_dir: bool,
    children: Vec<TreeNode>,
}

impl TreeNode {
    fn new(name: String, path: String, is_dir: bool) -> Self {
        Self {
            name,
            path,
            is_dir,
            children: Vec::new(),
        }
    }
}

struct VisibleNode {
    path: String,
    name: String,
    is_dir: bool,
    depth: usize,
}

#[derive(Copy, Clone)]
enum SelectionState {
    None,
    Partial,
    Selected,
}

struct TerminalGuard {
    raw_mode: bool,
}

impl TerminalGuard {
    fn new() -> Result<Self, String> {
        terminal::enable_raw_mode().map_err(|e| format!("Failed to enable raw mode: {}", e))?;
        let mut stdout = std::io::stdout();
        execute!(stdout, terminal::EnterAlternateScreen, cursor::Hide)
            .map_err(|e| format!("Failed to initialize terminal: {}", e))?;
        Ok(Self { raw_mode: true })
    }
}

impl Drop for TerminalGuard {
    fn drop(&mut self) {
        let mut stdout = std::io::stdout();
        let _ = execute!(stdout, terminal::LeaveAlternateScreen, cursor::Show);
        if self.raw_mode {
            let _ = terminal::disable_raw_mode();
        }
    }
}

fn parse_only_request(matches: &ArgMatches, prune_local: bool) -> Result<OnlyRequest, String> {
    if !matches.contains_id("only") {
        return Ok(OnlyRequest::None);
    }

    if prune_local {
        return Err("--only cannot be used together with --prune-local".to_string());
    }

    let values: Vec<String> = matches
        .get_many::<String>("only")
        .map(|vals| vals.map(|v| v.to_string()).collect())
        .unwrap_or_default();

    if values.is_empty() {
        if is_json_mode() {
            return Err("--only requires a path value when used in JSON mode".to_string());
        }
        return Ok(OnlyRequest::Interactive);
    }

    let mut normalized = Vec::with_capacity(values.len());
    for value in values {
        normalized.push(normalize_only_path(&value)?);
    }

    Ok(OnlyRequest::Paths(normalized))
}

fn normalize_only_path(value: &str) -> Result<String, String> {
    let trimmed = value.trim();
    if trimmed.is_empty() {
        return Err("Invalid --only path: empty".to_string());
    }

    let mut path = trimmed.replace('\\', "/");

    while path.starts_with("./") {
        path = path[2..].to_string();
    }

    if path.starts_with('/') {
        path = path.trim_start_matches('/').to_string();
    }

    while path.ends_with('/') {
        path.pop();
    }

    if path.is_empty() {
        return Err(format!("Invalid --only path: {}", value));
    }

    Ok(path)
}

fn filter_only_paths(
    backup_tree: &HashMap<String, crate::core::metadata::BackupObject>,
    only_paths: &[String],
) -> Result<Vec<(String, crate::core::metadata::BackupObject)>, String> {
    if only_paths.is_empty() {
        return Err("No paths selected".to_string());
    }

    let mut matched_paths = HashSet::new();

    for path in only_paths {
        if backup_tree.contains_key(path) {
            matched_paths.insert(path.clone());
            continue;
        }

        let mut found = false;
        let prefix = format!("{}/", path);
        for key in backup_tree.keys() {
            if key.starts_with(&prefix) {
                matched_paths.insert(key.clone());
                found = true;
            }
        }

        if !found {
            return Err(format!("No files found for path: {}", path));
        }
    }

    let mut files: Vec<(String, crate::core::metadata::BackupObject)> = matched_paths
        .into_iter()
        .filter_map(|path| backup_tree.get(&path).map(|obj| (path, obj.clone())))
        .collect();
    files.sort_by(|a, b| a.0.cmp(&b.0));

    Ok(files)
}

fn select_only_paths_interactive(
    backup_tree: &HashMap<String, crate::core::metadata::BackupObject>,
) -> Result<Vec<String>, String> {
    if backup_tree.is_empty() {
        return Err("Backup contains no files to restore".to_string());
    }

    let root = build_tree(backup_tree);
    let _guard = TerminalGuard::new()?;

    let mut expanded: HashSet<String> = HashSet::new();
    let mut selected: HashSet<String> = HashSet::new();
    let mut cursor_index: usize = 0;
    let mut scroll_offset: usize = 0;
    let mut status_message: Option<String> = None;

    loop {
        let visible = visible_nodes(&root, &expanded);
        if visible.is_empty() {
            return Err("Backup contains no files to restore".to_string());
        }

        if cursor_index >= visible.len() {
            cursor_index = visible.len().saturating_sub(1);
        }

        let selection_states = selection_states(&root, &selected);
        render_selector(
            &visible,
            &expanded,
            &selected,
            &selection_states,
            cursor_index,
            &mut scroll_offset,
            status_message.as_deref(),
        )?;
        status_message = None;

        let event = event::read().map_err(|e| format!("Failed to read input: {}", e))?;
        let Event::Key(key) = event else {
            continue;
        };

        if key.kind == KeyEventKind::Release {
            continue;
        }

        match key.code {
            KeyCode::Char('q') | KeyCode::Esc => {
                return Err("Selection cancelled".to_string());
            }
            KeyCode::Char('c') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                return Err("Selection cancelled".to_string());
            }
            KeyCode::Up => {
                if cursor_index > 0 {
                    cursor_index -= 1;
                }
            }
            KeyCode::Down => {
                if cursor_index + 1 < visible.len() {
                    cursor_index += 1;
                }
            }
            KeyCode::PageUp => {
                let page_size = current_page_size()?;
                cursor_index = cursor_index.saturating_sub(page_size);
                scroll_offset = page_start_for(cursor_index, page_size);
            }
            KeyCode::PageDown => {
                let page_size = current_page_size()?;
                let next = cursor_index.saturating_add(page_size);
                cursor_index = next.min(visible.len().saturating_sub(1));
                scroll_offset = page_start_for(cursor_index, page_size);
            }
            KeyCode::BackTab => {
                expanded.clear();
            }
            KeyCode::Tab => {
                let current = &visible[cursor_index];
                if current.is_dir {
                    if expanded.contains(&current.path) {
                        expanded.remove(&current.path);
                    } else {
                        expanded.insert(current.path.clone());
                    }
                }
            }
            KeyCode::Char(' ') => {
                let current = &visible[cursor_index];
                if current.is_dir {
                    if let Some(node) = find_node_by_path(&root, &current.path) {
                        let mut files = Vec::new();
                        collect_file_paths(node, &mut files);
                        let all_selected = files.iter().all(|path| selected.contains(path));
                        if all_selected {
                            for path in files {
                                selected.remove(&path);
                            }
                        } else {
                            for path in files {
                                selected.insert(path);
                            }
                        }
                    }
                } else if selected.contains(&current.path) {
                    selected.remove(&current.path);
                } else {
                    selected.insert(current.path.clone());
                }
            }
            KeyCode::Enter => {
                if selected.is_empty() {
                    status_message = Some("Select at least one path using space.".to_string());
                } else {
                    let mut result: Vec<String> = selected.into_iter().collect();
                    result.sort();
                    return Ok(result);
                }
            }
            _ => {}
        }
    }
}

fn current_page_size() -> Result<usize, String> {
    let (_, height) =
        terminal::size().map_err(|e| format!("Failed to read terminal size: {}", e))?;
    let header_lines = 2u16;
    let footer_lines = 1u16;
    let available = height.saturating_sub(header_lines + footer_lines);
    let page_size = available as usize;
    if page_size == 0 {
        return Err("Terminal window too small to render selector".to_string());
    }
    Ok(page_size)
}

fn page_start_for(cursor_index: usize, page_size: usize) -> usize {
    if page_size == 0 {
        return 0;
    }
    (cursor_index / page_size) * page_size
}

fn render_selector(
    visible: &[VisibleNode],
    expanded: &HashSet<String>,
    selected: &HashSet<String>,
    selection_states: &HashMap<String, SelectionState>,
    cursor_index: usize,
    scroll_offset: &mut usize,
    status_message: Option<&str>,
) -> Result<(), String> {
    let (width, height) =
        terminal::size().map_err(|e| format!("Failed to read terminal size: {}", e))?;
    let header_lines = 2usize;
    let footer_lines = 1usize;
    let view_height = height.saturating_sub((header_lines + footer_lines) as u16) as usize;

    if view_height == 0 {
        return Err("Terminal window too small to render selector".to_string());
    }

    if cursor_index < *scroll_offset {
        *scroll_offset = cursor_index;
    } else if cursor_index >= *scroll_offset + view_height {
        *scroll_offset = cursor_index + 1 - view_height;
    }

    let total_pages = (visible.len().saturating_sub(1) / view_height) + 1;
    let current_page = (cursor_index / view_height) + 1;

    let mut stdout = std::io::stdout();
    queue!(
        stdout,
        terminal::Clear(ClearType::All),
        cursor::MoveTo(0, 0)
    )
    .map_err(|e| format!("Failed to render selector: {}", e))?;

    let header = "Keys: Up/Down, Tab=expand/collapse, Shift+Tab=collapse all, Space=select, Enter=confirm, Q=cancel";
    write_line(&mut stdout, header, width, true)?;

    let selection_count = selected.len();
    let summary = format!(
        "Selected files: {} | Page {}/{} | Items: {}",
        selection_count,
        current_page,
        total_pages,
        visible.len()
    );
    write_line(&mut stdout, &summary, width, true)?;

    for line_index in 0..view_height {
        let item_index = *scroll_offset + line_index;
        if item_index >= visible.len() {
            write_line(&mut stdout, "", width, true)?;
            continue;
        }

        let item = &visible[item_index];
        let cursor_marker = if item_index == cursor_index { ">" } else { " " };
        let selection_state = selection_states
            .get(&item.path)
            .copied()
            .unwrap_or(SelectionState::None);
        let selection_marker = match selection_state {
            SelectionState::None => "[ ]",
            SelectionState::Partial => "[~]",
            SelectionState::Selected => "[x]",
        };

        let indent = "  ".repeat(item.depth);
        let expand_marker = if item.is_dir {
            if expanded.contains(&item.path) {
                "-"
            } else {
                "+"
            }
        } else {
            " "
        };
        let display_name = if item.is_dir {
            format!("{}/", item.name)
        } else {
            item.name.clone()
        };

        let line = format!(
            "{} {} {}{} {}",
            cursor_marker, selection_marker, indent, expand_marker, display_name
        );
        write_line(&mut stdout, &line, width, true)?;
    }

    let footer = status_message.unwrap_or("");
    write_line(&mut stdout, footer, width, false)?;

    stdout
        .flush()
        .map_err(|e| format!("Failed to render selector: {}", e))?;
    Ok(())
}

fn write_line(
    stdout: &mut std::io::Stdout,
    text: &str,
    width: u16,
    newline: bool,
) -> Result<(), String> {
    let mut line = String::new();
    let mut count = 0usize;
    for ch in text.chars() {
        if count >= width as usize {
            break;
        }
        line.push(ch);
        count += 1;
    }
    if newline {
        line.push_str("\r\n");
    } else {
        line.push('\r');
    }
    stdout
        .write_all(line.as_bytes())
        .map_err(|e| format!("Failed to write output: {}", e))?;
    Ok(())
}

fn visible_nodes(root: &TreeNode, expanded: &HashSet<String>) -> Vec<VisibleNode> {
    let mut out = Vec::new();
    collect_visible_nodes(root, expanded, 0, &mut out);
    out
}

fn collect_visible_nodes(
    node: &TreeNode,
    expanded: &HashSet<String>,
    depth: usize,
    out: &mut Vec<VisibleNode>,
) {
    for child in &node.children {
        out.push(VisibleNode {
            path: child.path.clone(),
            name: child.name.clone(),
            is_dir: child.is_dir,
            depth,
        });
        if child.is_dir && expanded.contains(&child.path) {
            collect_visible_nodes(child, expanded, depth + 1, out);
        }
    }
}

fn selection_states(
    root: &TreeNode,
    selected: &HashSet<String>,
) -> HashMap<String, SelectionState> {
    let mut states = HashMap::new();
    fill_selection_states(root, selected, &mut states);
    states
}

fn fill_selection_states(
    node: &TreeNode,
    selected: &HashSet<String>,
    states: &mut HashMap<String, SelectionState>,
) -> SelectionState {
    if node.is_dir {
        let mut any_selected = false;
        let mut all_selected = true;

        for child in &node.children {
            let child_state = fill_selection_states(child, selected, states);
            match child_state {
                SelectionState::Selected => {
                    any_selected = true;
                }
                SelectionState::Partial => {
                    any_selected = true;
                    all_selected = false;
                }
                SelectionState::None => {
                    all_selected = false;
                }
            }
        }

        let state = if node.children.is_empty() {
            SelectionState::None
        } else if all_selected {
            SelectionState::Selected
        } else if any_selected {
            SelectionState::Partial
        } else {
            SelectionState::None
        };

        if !node.path.is_empty() {
            states.insert(node.path.clone(), state);
        }

        state
    } else {
        let state = if selected.contains(&node.path) {
            SelectionState::Selected
        } else {
            SelectionState::None
        };
        if !node.path.is_empty() {
            states.insert(node.path.clone(), state);
        }
        state
    }
}

fn build_tree(backup_tree: &HashMap<String, crate::core::metadata::BackupObject>) -> TreeNode {
    let mut root = TreeNode::new(String::new(), String::new(), true);

    for path in backup_tree.keys() {
        let parts: Vec<&str> = path.split('/').collect();
        let mut current = &mut root;
        let mut current_path = String::new();

        for (index, part) in parts.iter().enumerate() {
            if !current_path.is_empty() {
                current_path.push('/');
            }
            current_path.push_str(part);

            let is_last = index == parts.len().saturating_sub(1);
            if is_last {
                if current
                    .children
                    .iter()
                    .any(|child| child.path == current_path)
                {
                    continue;
                }
                current
                    .children
                    .push(TreeNode::new(part.to_string(), current_path.clone(), false));
            } else {
                let next_index = current
                    .children
                    .iter()
                    .position(|child| child.is_dir && child.name == *part);
                match next_index {
                    Some(index) => current = &mut current.children[index],
                    None => {
                        current.children.push(TreeNode::new(
                            part.to_string(),
                            current_path.clone(),
                            true,
                        ));
                        let len = current.children.len();
                        current = &mut current.children[len - 1];
                    }
                }
            }
        }
    }

    sort_tree(&mut root);
    root
}

fn sort_tree(node: &mut TreeNode) {
    node.children.sort_by(|a, b| match (a.is_dir, b.is_dir) {
        (true, false) => Ordering::Less,
        (false, true) => Ordering::Greater,
        _ => a.name.cmp(&b.name),
    });
    for child in &mut node.children {
        if child.is_dir {
            sort_tree(child);
        }
    }
}

fn find_node_by_path<'a>(node: &'a TreeNode, path: &str) -> Option<&'a TreeNode> {
    if node.path == path {
        return Some(node);
    }

    for child in &node.children {
        if let Some(found) = find_node_by_path(child, path) {
            return Some(found);
        }
    }

    None
}

fn collect_file_paths(node: &TreeNode, out: &mut Vec<String>) {
    if node.is_dir {
        for child in &node.children {
            collect_file_paths(child, out);
        }
    } else if !node.path.is_empty() {
        out.push(node.path.clone());
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
            if is_json_mode() {
                return Err(
                    "Missing required argument: --backup (required in --mode json)".to_string(),
                );
            }
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
                .with_prompt("Select a backup to restore")
                .items(&items)
                .default(0)
                .interact()
                .map_err(|e| format!("Failed to select backup: {}", e))?;

            Ok(recent_backups[selected_index].hash.clone())
        }
    }
}

struct BackupSummaryDisplay {
    hash: String,
    message: String,
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

fn cleanup_extra_files(
    target_path: &str,
    backup_tree: &std::collections::HashMap<String, crate::core::metadata::BackupObject>,
) -> Result<u64, String> {
    let target_path_buf = PathBuf::from(target_path);

    if !target_path_buf.exists() {
        return Ok(0);
    }

    let backup_paths: HashSet<String> = backup_tree.keys().map(|p| p.replace('\\', "/")).collect();

    let mut deleted_count = 0u64;
    let mut dirs_to_check = HashSet::new();

    for entry in WalkDir::new(&target_path_buf)
        .into_iter()
        .filter_map(|e| e.ok())
        .filter(|e| e.path().is_file())
    {
        let file_path = entry.path();

        let relative_path = match file_path.strip_prefix(&target_path_buf) {
            Ok(rel) => rel,
            Err(_) => continue,
        };

        let relative_path_str = relative_path.to_string_lossy().replace('\\', "/");

        if !backup_paths.contains(&relative_path_str) {
            match std::fs::remove_file(file_path) {
                Ok(_) => {
                    deleted_count += 1;
                    let mut current = file_path.parent();
                    while let Some(parent) = current {
                        if parent != target_path_buf {
                            dirs_to_check.insert(parent.to_path_buf());
                        }
                        current = parent.parent();
                    }
                }
                Err(e) => {
                    emit_warning(
                        &format!("Failed to delete {}: {}", relative_path_str, e),
                        "delete_failed",
                    );
                }
            }
        }
    }

    let mut dirs_vec: Vec<PathBuf> = dirs_to_check.into_iter().collect();
    dirs_vec.sort_by(|a, b| b.components().count().cmp(&a.components().count()));

    for dir in dirs_vec {
        if dir.exists() && dir != target_path_buf {
            if let Ok(mut entries) = std::fs::read_dir(&dir) {
                if entries.next().is_none() {
                    let _ = std::fs::remove_dir(&dir);
                }
            }
        }
    }

    Ok(deleted_count)
}

fn get_params(
    matches: &ArgMatches,
) -> Result<
    (
        String,
        String,
        Option<String>,
        Option<String>,
        String,
        bool,
        OnlyRequest,
    ),
    String,
> {
    let password: Option<String> = matches
        .get_one::<String>("password")
        .map(|s| s.to_string())
        .map_or_else(
            || get_password(false, true),
            |password| Some(password.to_string()),
        );

    let pwd_string = get_pwd_string();

    let target_path = matches.get_one::<String>("target-path").map_or_else(
        || pwd_string.clone(),
        |target_path| {
            Path::new(&pwd_string)
                .join(target_path)
                .to_string_lossy()
                .to_string()
        },
    );

    let default_key = Path::new(&pwd_string)
        .file_name()
        .unwrap()
        .to_string_lossy()
        .to_string();

    let key = matches
        .get_one::<String>("key")
        .map_or_else(|| default_key, |key| key.to_string());

    let prune_local = matches.get_flag("prune-local");
    let only_request = parse_only_request(matches, prune_local)?;

    let home_dir = home_dir().unwrap();
    let storage_path = home_dir.join(".gib").join("storages");

    if !storage_path.exists() {
        return Err("Seems like you didn't create any storage yet. Run 'gib storage add' to create a storage.".to_string());
    }

    let files =
        std::fs::read_dir(&storage_path).map_err(|e| format!("Failed to read storages: {}", e))?;

    let storages_names = &files
        .map(|file| {
            file.map_err(|e| format!("Failed to read storage entry: {}", e))
                .map(|file| {
                    file.file_name()
                        .to_string_lossy()
                        .to_string()
                        .split('.')
                        .next()
                        .unwrap()
                        .to_string()
                })
        })
        .collect::<Result<Vec<String>, String>>()?;

    if storages_names.is_empty() {
        return Err("Seems like you didn't create any storage yet. Run 'gib storage add' to create a storage.".to_string());
    }

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
                .items(storages_names)
                .default(0)
                .interact()
                .map_err(|e| format!("{}", e))?;

            storages_names[selected_index].clone()
        }
    };

    let exists = storages_names
        .iter()
        .any(|storage_name| storage_name == &storage);

    if !exists {
        return Err(format!("Storage '{}' not found", storage));
    }

    let backup_hash = matches.get_one::<String>("backup").map(|s| s.to_string());

    Ok((
        key,
        storage,
        password,
        backup_hash,
        target_path,
        prune_local,
        only_request,
    ))
}
