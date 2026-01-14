use clap::ArgMatches;
use dialoguer::{Input, Select};
use dirs::home_dir;
use indicatif::{ProgressBar, ProgressStyle};
use rmp_serde::Serializer;
use serde::{Deserialize, Serialize};
use std::path::Path;
use std::time::Duration;

#[derive(Debug, PartialEq, Deserialize, Serialize)]
pub struct Storage {
    pub storage_type: u8,
    pub path: Option<String>,
    pub region: Option<String>,
    pub bucket: Option<String>,
    pub access_key: Option<String>,
    pub secret_key: Option<String>,
    pub endpoint: Option<String>,
}

pub fn add(matches: &ArgMatches) {
    let name = matches.get_one::<String>("name").map_or_else(
        || {
            let typed_name: String = Input::<String>::new()
                .with_prompt("Enter the name of the storage")
                .default("default".to_string())
                .interact_text()
                .unwrap_or_else(|e| {
                    eprintln!("Error: {}", e);
                    std::process::exit(1);
                });
            typed_name
        },
        |name| name.to_string(),
    );

    if !name
        .chars()
        .all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-')
    {
        eprintln!(
            "Error: The storage name can only contain letters, numbers, underscores (_), or hyphens (-)."
        );
        std::process::exit(1);
    }

    let storage_type: u8 = matches.get_one::<String>("type").map_or_else(
        || {
            let selected_storage_type: u8 = Select::new()
                .with_prompt("Enter the type of the storage")
                .default(0)
                .items(&["local", "s3"])
                .interact()
                .unwrap_or_else(|e| {
                    eprintln!("Error: {}", e);
                    std::process::exit(1);
                }) as u8;
            selected_storage_type
        },
        |storage_type| match storage_type.as_str() {
            "local" => 0u8,
            "s3" => 1u8,
            _ => {
                eprintln!("Error: Unknown storage type '{}'", storage_type);
                std::process::exit(1);
            }
        },
    );

    let mut storage = Storage {
        storage_type,
        path: None,
        region: None,
        bucket: None,
        access_key: None,
        secret_key: None,
        endpoint: None,
    };

    if storage_type == 0 {
        let path = matches.get_one::<String>("path").map_or_else(
            || {
                let typed_path: String = Input::<String>::new()
                    .with_prompt("Enter the path for local storage")
                    .interact_text()
                    .unwrap_or_else(|e| {
                        eprintln!("Error: {}", e);
                        std::process::exit(1);
                    });
                typed_path
            },
            |path| path.to_string(),
        );

        if !Path::new(&path).exists() {
            std::fs::create_dir_all(&path).unwrap();
        }

        storage.path = Some(path);
    } else {
        let region = matches.get_one::<String>("region").map_or_else(
            || {
                let typed_region: String = Input::<String>::new()
                    .with_prompt("Enter the S3 region")
                    .interact_text()
                    .unwrap_or_else(|e| {
                        eprintln!("Error: {}", e);
                        std::process::exit(1);
                    });
                typed_region
            },
            |region| region.to_string(),
        );

        let bucket = matches.get_one::<String>("bucket").map_or_else(
            || {
                let typed_bucket: String = Input::<String>::new()
                    .with_prompt("Enter the S3 bucket")
                    .interact_text()
                    .unwrap_or_else(|e| {
                        eprintln!("Error: {}", e);
                        std::process::exit(1);
                    });
                typed_bucket
            },
            |bucket| bucket.to_string(),
        );

        let access_key = matches.get_one::<String>("access-key").map_or_else(
            || {
                let typed_access_key: String = Input::<String>::new()
                    .with_prompt("Enter the S3 access key")
                    .interact_text()
                    .unwrap_or_else(|e| {
                        eprintln!("Error: {}", e);
                        std::process::exit(1);
                    });
                typed_access_key
            },
            |access_key| access_key.to_string(),
        );

        let secret_key = matches.get_one::<String>("secret-key").map_or_else(
            || {
                let typed_secret_key: String = Input::<String>::new()
                    .with_prompt("Enter the S3 secret key")
                    .interact_text()
                    .unwrap_or_else(|e| {
                        eprintln!("Error: {}", e);
                        std::process::exit(1);
                    });
                typed_secret_key
            },
            |secret_key| secret_key.to_string(),
        );

        let endpoint = matches.get_one::<String>("endpoint").map_or_else(
            || {
                let typed_endpoint: String = Input::<String>::new()
                    .with_prompt("Enter the S3 endpoint")
                    .default(format!("https://s3.{}.amazonaws.com", region))
                    .show_default(true)
                    .interact_text()
                    .unwrap_or_else(|e| {
                        eprintln!("Error: {}", e);
                        std::process::exit(1);
                    });
                typed_endpoint
            },
            |endpoint| endpoint.to_string(),
        );

        storage.region = Some(region);
        storage.bucket = Some(bucket);
        storage.access_key = Some(access_key);
        storage.secret_key = Some(secret_key);
        storage.endpoint = Some(endpoint);
    }

    let pb = ProgressBar::new(100);
    pb.enable_steady_tick(Duration::from_millis(100));
    pb.set_style(ProgressStyle::with_template("{spinner:.green} {msg}").unwrap());

    pb.set_message(format!("Writing storage '{}'...", name));

    let home_dir = home_dir().unwrap();

    let mut storage_path = home_dir.join(".gib").join("storages");

    if !storage_path.exists() {
        std::fs::create_dir_all(&storage_path).unwrap();
    }

    storage_path.push(format!("{}.msgpack", name));

    let mut buf = Vec::new();
    storage.serialize(&mut Serializer::new(&mut buf)).unwrap();

    std::fs::write(storage_path, buf).unwrap();

    let elapsed = pb.elapsed();

    pb.set_style(ProgressStyle::with_template("{prefix:.green} {msg}").unwrap());
    pb.set_prefix("✓");
    pb.finish_with_message(format!("Storage written ({:.2?})", elapsed));
}
