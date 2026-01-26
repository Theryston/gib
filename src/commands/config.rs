use clap::ArgMatches;
use dialoguer::Input;
use dirs::home_dir;
use indicatif::{ProgressBar, ProgressStyle};
use rmp_serde::Serializer;
use serde::{Deserialize, Serialize};
use std::time::Duration;

use crate::output::{emit_output, is_json_mode, JsonProgress};
use crate::utils::handle_error;

#[derive(Debug, PartialEq, Deserialize, Serialize)]
pub struct Config {
    pub author: String,
}

pub fn config(matches: &ArgMatches) {
    let author = matches.get_one::<String>("author").map_or_else(
        || {
            if is_json_mode() {
                handle_error(
                    "Missing required argument: --author (required in --mode json)"
                        .to_string(),
                    None,
                );
            }
            let typed_author: String = Input::<String>::new()
                .with_prompt("Enter your author (e.g. 'John Doe <john.doe@example.com>')")
                .interact_text()
                .unwrap_or_else(|e| {
                    handle_error(format!("Error: {}", e), None);
                });

            typed_author
        },
        |author| author.to_string(),
    );

    let author_pattern =
        regex::Regex::new(r"^[A-Za-z]+(?: [A-Za-z]+)+(?: )?<[^@ ]+@[^@ ]+\.[^@ >]+>$").unwrap();

    if !author_pattern.is_match(&author) {
        handle_error(
            "The author must be in the format 'Firstname Lastname <email>'".to_string(),
            None,
        );
    }

    let json_progress = if is_json_mode() {
        let progress = JsonProgress::new(1);
        progress.set_message("Writing config...");
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
        pb.set_message("Writing config...");
        pb
    };

    let config = Config { author };

    let mut buf = Vec::new();
    config
        .serialize(&mut Serializer::new(&mut buf))
        .unwrap_or_else(|e| handle_error(format!("Failed to serialize config: {}", e), None));

    let home_dir = home_dir().unwrap();

    let mut config_path = home_dir.join(".gib");

    if !config_path.exists() {
        std::fs::create_dir_all(&config_path)
            .unwrap_or_else(|e| handle_error(format!("Failed to create config directory: {}", e), None));
    }

    config_path.push("config.msgpack");

    std::fs::write(&config_path, buf)
        .unwrap_or_else(|e| handle_error(format!("Failed to write config: {}", e), None));

    if let Some(progress) = &json_progress {
        progress.inc_by(1);
    }

    if is_json_mode() {
        #[derive(Serialize)]
        struct ConfigOutput {
            author: String,
            path: String,
        }

        let payload = ConfigOutput {
            author: config.author,
            path: config_path.to_string_lossy().to_string(),
        };
        emit_output(&payload);
    } else {
        let elapsed = pb.elapsed();

        pb.set_style(ProgressStyle::with_template("{prefix:.green} {msg}").unwrap());
        pb.set_prefix("OK");
        pb.finish_with_message(format!("Config written ({:.2?})", elapsed));
    }
}
