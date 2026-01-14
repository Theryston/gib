use clap::ArgMatches;
use dialoguer::Select;
use dirs::home_dir;
use indicatif::{ProgressBar, ProgressStyle};
use std::time::Duration;

pub fn remove(matches: &ArgMatches) {
    let home_dir = home_dir().unwrap();
    let storage_path = home_dir.join(".gib").join("storages");

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

    let name = matches.get_one::<String>("name").map_or_else(
        || {
            let selected_index = Select::new()
                .with_prompt("Select the storage to remove")
                .items(storages_names)
                .default(0)
                .interact()
                .unwrap_or_else(|e| {
                    eprintln!("Error: {}", e);
                    std::process::exit(1);
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
        eprintln!("Error: Storage '{}' not found", name);
        std::process::exit(1);
    }

    let pb = ProgressBar::new(100);
    pb.enable_steady_tick(Duration::from_millis(100));
    pb.set_style(ProgressStyle::with_template("{spinner:.green} {msg}").unwrap());

    pb.set_message(format!("Removing storage '{}'...", name));

    let storage_path = storage_path.join(format!("{}.msgpack", name));

    std::fs::remove_file(storage_path).unwrap();

    let elapsed = pb.elapsed();

    pb.set_style(ProgressStyle::with_template("{prefix:.green} {msg}").unwrap());
    pb.set_prefix("✓");
    pb.finish_with_message(format!("Storage removed ({:.2?})", elapsed));
}
