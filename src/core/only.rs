use std::cmp::Ordering;
use std::collections::{HashMap, HashSet};
use std::io::Write;

use clap::ArgMatches;
use crossterm::{
    cursor,
    event::{self, Event, KeyCode, KeyEventKind, KeyModifiers},
    execute, queue,
    terminal::{self, ClearType},
};

use crate::output::is_json_mode;

pub enum OnlyRequest {
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

struct FileEntry {
    path: String,
    path_lower: String,
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

pub fn parse_only_request(matches: &ArgMatches, prune_local: bool) -> Result<OnlyRequest, String> {
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

pub fn filter_only_paths(
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

pub fn select_only_paths_interactive(
    backup_tree: &HashMap<String, crate::core::metadata::BackupObject>,
) -> Result<Vec<String>, String> {
    if backup_tree.is_empty() {
        return Err("Backup contains no files to restore".to_string());
    }

    let root = build_tree(backup_tree);
    let file_index = build_file_index(backup_tree);
    let _guard = TerminalGuard::new()?;

    let mut expanded: HashSet<String> = HashSet::new();
    let mut selected: HashSet<String> = HashSet::new();
    let mut cursor_index: usize = 0;
    let mut scroll_offset: usize = 0;
    let mut status_message: Option<String> = None;
    let mut search_input = false;
    let mut search_query = String::new();

    loop {
        let search_active = !search_query.is_empty();
        let visible = if search_active {
            visible_search_nodes(&file_index, &search_query)
        } else {
            visible_nodes(&root, &expanded)
        };

        if visible.is_empty() {
            if search_active {
                status_message = Some("No matches".to_string());
            } else {
                return Err("Backup contains no files to restore".to_string());
            }
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
            search_input,
            search_active,
            &search_query,
        )?;
        status_message = None;

        let event = event::read().map_err(|e| format!("Failed to read input: {}", e))?;
        let Event::Key(key) = event else {
            continue;
        };

        if key.kind == KeyEventKind::Release {
            continue;
        }

        if key.modifiers.contains(KeyModifiers::CONTROL) && matches!(key.code, KeyCode::Char('c')) {
            return Err("Selection cancelled".to_string());
        }

        let search_active = !search_query.is_empty();

        if search_input {
            match key.code {
                KeyCode::Esc => {
                    search_input = false;
                    search_query.clear();
                    cursor_index = 0;
                    scroll_offset = 0;
                }
                KeyCode::Enter => {
                    search_input = false;
                    cursor_index = 0;
                    scroll_offset = 0;
                }
                KeyCode::Backspace => {
                    if search_query.pop().is_some() {
                        cursor_index = 0;
                        scroll_offset = 0;
                    }
                }
                KeyCode::Char(c) => {
                    if !key.modifiers.contains(KeyModifiers::CONTROL) && !c.is_control() {
                        search_query.push(c);
                        cursor_index = 0;
                        scroll_offset = 0;
                    }
                }
                _ => {}
            }
            continue;
        }

        match key.code {
            KeyCode::Esc => {
                if search_active {
                    search_query.clear();
                    cursor_index = 0;
                    scroll_offset = 0;
                } else {
                    return Err("Selection cancelled".to_string());
                }
            }
            KeyCode::Char('s') | KeyCode::Char('S') => {
                search_input = true;
                cursor_index = 0;
                scroll_offset = 0;
            }
            KeyCode::Up => {
                if cursor_index > 0 && !visible.is_empty() {
                    cursor_index -= 1;
                }
            }
            KeyCode::Down => {
                if cursor_index + 1 < visible.len() && !visible.is_empty() {
                    cursor_index += 1;
                }
            }
            KeyCode::PageUp => {
                if !visible.is_empty() {
                    let page_size = current_page_size()?;
                    cursor_index = cursor_index.saturating_sub(page_size);
                    scroll_offset = page_start_for(cursor_index, page_size);
                }
            }
            KeyCode::PageDown => {
                if !visible.is_empty() {
                    let page_size = current_page_size()?;
                    let next = cursor_index.saturating_add(page_size);
                    cursor_index = next.min(visible.len().saturating_sub(1));
                    scroll_offset = page_start_for(cursor_index, page_size);
                }
            }
            KeyCode::BackTab => {
                expanded.clear();
            }
            KeyCode::Tab => {
                let Some(current) = visible.get(cursor_index) else {
                    continue;
                };
                if current.is_dir {
                    if expanded.contains(&current.path) {
                        expanded.remove(&current.path);
                    } else {
                        expanded.insert(current.path.clone());
                    }
                }
            }
            KeyCode::Char(' ') => {
                let Some(current) = visible.get(cursor_index) else {
                    continue;
                };
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
    search_input: bool,
    search_active: bool,
    search_query: &str,
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

    let header = if search_input {
        "Search mode: type to filter, Enter=apply, Esc=exit-search"
    } else if search_active {
        "Keys: Tab=toggle Shift+Tab=collapse-all Space=select Enter=ok Esc=clear-search S=edit-search"
    } else {
        "Keys: Tab=toggle Shift+Tab=collapse-all Space=select Enter=ok S=search Esc=cancel"
    };
    write_line(&mut stdout, header, width, true)?;

    let selection_count = selected.len();
    let summary = if search_active {
        format!(
            "Selected files: {} | Matches: {} | Page {}/{}",
            selection_count,
            visible.len(),
            current_page,
            total_pages
        )
    } else {
        format!(
            "Selected files: {} | Page {}/{} | Items: {}",
            selection_count,
            current_page,
            total_pages,
            visible.len()
        )
    };
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

        let (indent, expand_marker, display_name) = if search_active {
            ("".to_string(), "", item.name.clone())
        } else {
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
            (indent, expand_marker, display_name)
        };

        let line = format!(
            "{} {} {}{} {}",
            cursor_marker, selection_marker, indent, expand_marker, display_name
        );
        write_line(&mut stdout, &line, width, true)?;
    }

    let footer = if search_input {
        match status_message {
            Some(message) => format!("Search: {} | {}", search_query, message),
            None => format!("Search: {}", search_query),
        }
    } else if search_active {
        match status_message {
            Some(message) => format!("Filter: {} | {}", search_query, message),
            None => format!("Filter: {}", search_query),
        }
    } else {
        status_message.unwrap_or("").to_string()
    };
    write_line(&mut stdout, &footer, width, false)?;

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

fn build_file_index(
    backup_tree: &HashMap<String, crate::core::metadata::BackupObject>,
) -> Vec<FileEntry> {
    let mut entries: Vec<FileEntry> = backup_tree
        .keys()
        .map(|path| {
            let path_lower = path.to_lowercase();
            FileEntry {
                path: path.clone(),
                path_lower,
            }
        })
        .collect();
    entries.sort_by(|a, b| a.path.cmp(&b.path));
    entries
}

fn visible_search_nodes(entries: &[FileEntry], query: &str) -> Vec<VisibleNode> {
    let needle = query.to_lowercase();
    let mut out = Vec::new();

    for entry in entries {
        if needle.is_empty() || entry.path_lower.contains(&needle) {
            let display_path = if entry.path.starts_with('/') {
                entry.path.clone()
            } else {
                format!("/{}", entry.path)
            };
            out.push(VisibleNode {
                path: entry.path.clone(),
                name: display_path,
                is_dir: false,
                depth: 0,
            });
        }
    }

    out
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
