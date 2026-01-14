use crate::core::crypto::get_password;
use crate::core::indexes::list_commit_summaries;
use crate::core::metadata::CommitSummary;
use crate::utils::{get_fs, get_storage, handle_error};
use clap::ArgMatches;
use console::{Term, style};
use crossterm::event::{self, Event, KeyCode, KeyEvent, KeyEventKind};
use crossterm::execute;
use crossterm::terminal::{Clear, ClearType, disable_raw_mode, enable_raw_mode};
use dialoguer::{Input, Select};
use dirs::home_dir;
use std::io;
use std::sync::Arc;

pub async fn log(matches: &ArgMatches) {
    let (key, storage, password) = match get_params(matches) {
        Ok(params) => params,
        Err(e) => handle_error(e, None),
    };

    let storage = get_storage(&storage);

    let fs = get_fs(&storage, None);

    let commit_summaries =
        match list_commit_summaries(Arc::clone(&fs), key.clone(), password.clone()).await {
            Ok(summaries) => summaries,
            Err(e) => handle_error(e, None),
        };

    if commit_summaries.is_empty() {
        println!(
            "{}",
            style("No backups found for this repository.").yellow()
        );
        return;
    }

    display_paginated_commits(&commit_summaries);
}

fn get_params(matches: &ArgMatches) -> Result<(String, String, Option<String>), String> {
    let password: Option<String> = matches
        .get_one::<String>("password")
        .map(|s| s.to_string())
        .map_or_else(
            || get_password(false, true),
            |password| Some(password.to_string()),
        );

    let key = matches.get_one::<String>("key").map_or_else(
        || {
            let typed_key: String = Input::<String>::new()
                .with_prompt("Enter the key of the repository")
                .interact_text()
                .unwrap_or_else(|e| {
                    eprintln!("Error: {}", e);
                    std::process::exit(1);
                });

            typed_key
        },
        |key| key.to_string(),
    );

    let home_dir = home_dir().unwrap();
    let storage_path = home_dir.join(".gib").join("storages");

    if !storage_path.exists() {
        return Err("Seams like you didn't create any storage yet. Run 'gib storage add' to create a storage.".to_string());
    }

    let files = std::fs::read_dir(&storage_path).unwrap();

    let storages_names = &files
        .map(|file| {
            file.unwrap()
                .file_name()
                .to_string_lossy()
                .to_string()
                .split('.')
                .next()
                .unwrap()
                .to_string()
        })
        .collect::<Vec<String>>();

    if storages_names.is_empty() {
        return Err("Seams like you didn't create any storage yet. Run 'gib storage add' to create a storage.".to_string());
    }

    let storage = match matches.get_one::<String>("storage") {
        Some(storage) => storage.to_string(),
        None => {
            let selected_index = Select::new()
                .with_prompt("Select the storage to use")
                .items(storages_names)
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

    Ok((key, storage, password))
}

const COMMITS_PER_PAGE: usize = 10;

fn display_paginated_commits(commit_summaries: &[CommitSummary]) {
    let total_commits = commit_summaries.len();
    let total_pages = (total_commits + COMMITS_PER_PAGE - 1) / COMMITS_PER_PAGE;
    let mut current_page = 0;

    let term = Term::stdout();
    enable_raw_mode().unwrap_or(());

    loop {
        execute!(io::stdout(), Clear(ClearType::All)).unwrap_or(());
        term.clear_screen().unwrap_or(());

        let start_idx = current_page * COMMITS_PER_PAGE;
        let end_idx = (start_idx + COMMITS_PER_PAGE).min(total_commits);
        let page_commits = &commit_summaries[start_idx..end_idx];

        for (idx, commit) in page_commits.iter().enumerate() {
            let hash_short = &commit.hash[..8.min(commit.hash.len())];
            println!(
                "{} {}",
                style(format!("commit {}", hash_short)).cyan().bold(),
                style(&commit.message).white()
            );

            if idx < page_commits.len() - 1 {
                println!();
            }
        }

        println!();
        println!(
            "{}",
            style(format!(
                "Page {}/{} ({} commits) | Press 'n' for next, 'p' for previous, 'q' to quit",
                current_page + 1,
                total_pages,
                total_commits
            ))
            .dim()
        );

        match event::read() {
            Ok(Event::Key(KeyEvent {
                code,
                kind: KeyEventKind::Press,
                ..
            })) => match code {
                KeyCode::Char('q') | KeyCode::Esc => {
                    break;
                }
                KeyCode::Char('n') | KeyCode::Right | KeyCode::Char(' ') => {
                    if current_page < total_pages - 1 {
                        current_page += 1;
                    }
                }
                KeyCode::Char('p') | KeyCode::Left => {
                    if current_page > 0 {
                        current_page -= 1;
                    }
                }
                KeyCode::Home => {
                    current_page = 0;
                }
                KeyCode::End => {
                    current_page = total_pages - 1;
                }
                _ => {}
            },
            Ok(Event::Resize(_, _)) => {}
            Err(_) => break,
            _ => {}
        }
    }

    disable_raw_mode().unwrap_or(());
    term.clear_screen().unwrap_or(());
}
