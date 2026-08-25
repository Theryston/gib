use crate::config::{
    PasswordPolicy, RepositoryOptions, load_and_report_local_config, resolve_repository,
};
use crate::core::indexes::list_backup_summaries;
use crate::core::metadata::BackupSummary;
use crate::output::{emit_output, is_json_mode};
use crate::utils::handle_error;
use bytesize::ByteSize;
use chrono::{DateTime, Local, SecondsFormat, Utc};
use clap::ArgMatches;
use console::{Term, style};
use crossterm::event::{self, Event, KeyCode, KeyEvent, KeyEventKind};
use crossterm::execute;
use crossterm::terminal::{Clear, ClearType, disable_raw_mode, enable_raw_mode};
use std::io;
use std::sync::Arc;

pub async fn log(matches: &ArgMatches) {
    let repository = match get_params(matches) {
        Ok(params) => params,
        Err(e) => handle_error(e, None),
    };

    let backup_summaries = match list_backup_summaries(
        Arc::clone(&repository.fs),
        repository.key.clone(),
        repository.password.clone(),
    )
    .await
    {
        Ok(summaries) => summaries,
        Err(e) => handle_error(e, None),
    };

    if backup_summaries.is_empty() {
        if is_json_mode() {
            let empty: Vec<LogEntry> = Vec::new();
            emit_output(&empty);
        } else {
            println!(
                "{}",
                style("No backups found for this repository.").yellow()
            );
        }
        return;
    }

    if is_json_mode() {
        let entries = backup_summaries
            .iter()
            .map(|backup| LogEntry::from_summary(backup))
            .collect::<Vec<LogEntry>>();
        emit_output(&entries);
    } else {
        display_paginated_backups(&backup_summaries);
    }
}

fn get_params(matches: &ArgMatches) -> Result<RepositoryOptions, String> {
    let local_config = load_and_report_local_config(matches)?;
    resolve_repository(
        matches,
        &local_config,
        PasswordPolicy {
            required: false,
            readonly: true,
        },
        None,
    )
}

const BACKUPS_PER_PAGE: usize = 10;

#[derive(serde::Serialize)]
struct LogEntry {
    backup: String,
    backup_short: String,
    message: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    timestamp: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    timestamp_unix: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    size_bytes: Option<u64>,
}

impl LogEntry {
    fn from_summary(summary: &BackupSummary) -> Self {
        let timestamp = summary.timestamp.and_then(|ts| {
            DateTime::<Utc>::from_timestamp_secs(ts as i64)
                .map(|dt| dt.to_rfc3339_opts(SecondsFormat::Secs, true))
        });

        LogEntry {
            backup: summary.hash.clone(),
            backup_short: summary.hash[..8.min(summary.hash.len())].to_string(),
            message: summary.message.clone(),
            timestamp,
            timestamp_unix: summary.timestamp,
            size_bytes: summary.size,
        }
    }
}

fn display_paginated_backups(backup_summaries: &[BackupSummary]) {
    let total_backups = backup_summaries.len();
    let total_pages = (total_backups + BACKUPS_PER_PAGE - 1) / BACKUPS_PER_PAGE;
    let mut current_page = 0;

    let term = Term::stdout();
    enable_raw_mode().unwrap_or(());

    loop {
        execute!(io::stdout(), Clear(ClearType::All)).unwrap_or(());
        term.clear_screen().unwrap_or(());

        let start_idx = current_page * BACKUPS_PER_PAGE;
        let end_idx = (start_idx + BACKUPS_PER_PAGE).min(total_backups);
        let page_backups = &backup_summaries[start_idx..end_idx];

        for (idx, backup) in page_backups.iter().enumerate() {
            let hash_short = &backup.hash[..8.min(backup.hash.len())];
            print!("\r");

            let mut parts = vec![
                style(format!("{}", hash_short)).cyan().bold(),
                style(backup.message.clone()).white(),
            ];

            if let Some(timestamp) = backup.timestamp {
                let timestamp = DateTime::<Utc>::from_timestamp_secs(timestamp as i64)
                    .expect("Error parsing timestamp");
                let timestamp = timestamp.with_timezone(&Local).format("%Y-%m-%d %H:%M:%S");
                parts.push(style(format!("\r\nCreated at: {}", timestamp)).dim());
            }

            if let Some(size) = backup.size {
                parts.push(style(format!("Size: {}", ByteSize(size))).dim());
            }

            let line = parts
                .iter()
                .map(|p| p.to_string())
                .collect::<Vec<String>>()
                .join(" ");
            println!("{}", line);

            if idx < page_backups.len() - 1 {
                println!();
            }
        }

        println!();
        print!("\r");
        println!(
            "{}",
            style(format!(
                "Page {}/{} ({} backups) | Press 'n' for next, 'p' for previous, 'q' to quit",
                current_page + 1,
                total_pages,
                total_backups
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
