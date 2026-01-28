use clap::ArgMatches;
use dialoguer::Select;
use dirs::home_dir;
use indicatif::{ProgressBar, ProgressStyle};
use std::time::Duration;

use crate::output::{JsonProgress, emit_output, is_json_mode};
use crate::utils::handle_error;

pub fn remove(matches: &ArgMatches) {
    let home_dir = home_dir().unwrap();
    let storage_path = home_dir.join(".gib").join("storages");

    if !storage_path.exists() {
        handle_error("No storages found".to_string(), None);
    }

    let files = std::fs::read_dir(&storage_path)
        .unwrap_or_else(|e| handle_error(format!("Failed to read storages: {}", e), None));

    let storages_names = &files
        .map(|file| {
            file.unwrap_or_else(|e| {
                handle_error(format!("Failed to read storage entry: {}", e), None)
            })
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
        handle_error("No storages found".to_string(), None);
    }

    let name = matches.get_one::<String>("name").map_or_else(
        || {
            if is_json_mode() {
                handle_error(
                    "Missing required argument: --name (required in --mode json)".to_string(),
                    None,
                );
            }
            let selected_index = Select::new()
                .with_prompt("Select the storage to remove")
                .items(storages_names)
                .default(0)
                .interact()
                .unwrap_or_else(|e| {
                    handle_error(format!("Error: {}", e), None);
                });

            let selected_storage = &storages_names[selected_index];

            selected_storage.to_string()
        },
        |name| name.to_string(),
    );

    let exists = storages_names
        .iter()
        .any(|storage_name| storage_name == &name);

    if !exists {
        handle_error(format!("Storage '{}' not found", name), None);
    }

    let json_progress = if is_json_mode() {
        let progress = JsonProgress::new(1);
        progress.set_message(&format!("Removing storage '{}'...", name));
        Some(progress)
    } else {
        None
    };

    let pb = if is_json_mode() {
        ProgressBar::hidden()
    } else {
        let pb = ProgressBar::new(100);
        pb.enable_steady_tick(Duration::from_millis(100));
        pb.set_style(ProgressStyle::with_template("{spinner:.green} {msg}").unwrap());
        pb.set_message(format!("Removing storage '{}'...", name));
        pb
    };

    let storage_path = storage_path.join(format!("{}.msgpack", name));

    std::fs::remove_file(&storage_path)
        .unwrap_or_else(|e| handle_error(format!("Failed to remove storage: {}", e), None));

    if let Some(progress) = &json_progress {
        progress.inc_by(1);
    }

    if is_json_mode() {
        #[derive(serde::Serialize)]
        struct RemoveOutput {
            name: String,
            removed: bool,
        }

        let payload = RemoveOutput {
            name,
            removed: true,
        };
        emit_output(&payload);
    } else {
        let elapsed = pb.elapsed();

        pb.set_style(ProgressStyle::with_template("{prefix:.green} {msg}").unwrap());
        pb.set_prefix("OK");
        pb.finish_with_message(format!("Storage removed ({:.2?})", elapsed));
    }
}
