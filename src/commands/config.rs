use clap::ArgMatches;
use dialoguer::Input;
use dirs::home_dir;
use indicatif::{ProgressBar, ProgressStyle};
use rmp_serde::Serializer;
use serde::{Deserialize, Serialize};
use std::time::Duration;

#[derive(Debug, PartialEq, Deserialize, Serialize)]
pub struct Config {
    pub author: String,
}

pub fn config(matches: &ArgMatches) {
    let author = matches.get_one::<String>("author").map_or_else(
        || {
            let typed_author: String = Input::<String>::new()
                .with_prompt("Enter your author (e.g. 'John Doe <john.doe@example.com>')")
                .interact_text()
                .unwrap_or_else(|e| {
                    eprintln!("Error: {}", e);
                    std::process::exit(1);
                });

            typed_author
        },
        |author| author.to_string(),
    );

    let author_pattern =
        regex::Regex::new(r"^[A-Za-z]+(?: [A-Za-z]+)+(?: )?<[^@ ]+@[^@ ]+\.[^@ >]+>$").unwrap();

    if !author_pattern.is_match(&author) {
        eprintln!("Error: The author must be in the format 'Firstname Lastname <email>'");
        std::process::exit(1);
    }

    let pb = ProgressBar::new(100);
    pb.enable_steady_tick(Duration::from_millis(100));
    pb.set_style(ProgressStyle::with_template("{spinner:.green} {msg}").unwrap());

    pb.set_message("Writing config...");

    let config = Config { author };

    let mut buf = Vec::new();
    config.serialize(&mut Serializer::new(&mut buf)).unwrap();

    let home_dir = home_dir().unwrap();

    let mut config_path = home_dir.join(".gib");

    if !config_path.exists() {
        std::fs::create_dir_all(&config_path).unwrap();
    }

    config_path.push("config.msgpack");

    std::fs::write(config_path, buf).unwrap();

    let elapsed = pb.elapsed();

    pb.set_style(ProgressStyle::with_template("{prefix:.green} {msg}").unwrap());
    pb.set_prefix("✓");
    pb.finish_with_message(format!("Config written ({:.2?})", elapsed));
}
