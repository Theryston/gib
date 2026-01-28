use clap::ArgMatches;
use dialoguer::{Input, Select};
use dirs::home_dir;
use indicatif::{ProgressBar, ProgressStyle};
use rmp_serde::Serializer;
use serde::{Deserialize, Serialize};
use std::path::Path;
use std::time::Duration;

use crate::output::{JsonProgress, emit_output, is_json_mode};
use crate::utils::handle_error;

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
            if is_json_mode() {
                handle_error(
                    "Missing required argument: --name (required in --mode json)".to_string(),
                    None,
                );
            }
            let typed_name: String = Input::<String>::new()
                .with_prompt("Enter the name of the storage")
                .default("default".to_string())
                .interact_text()
                .unwrap_or_else(|e| {
                    handle_error(format!("Error: {}", e), None);
                });
            typed_name
        },
        |name| name.to_string(),
    );

    if !name
        .chars()
        .all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-')
    {
        handle_error(
            "The storage name can only contain letters, numbers, underscores (_), or hyphens (-)."
                .to_string(),
            None,
        );
    }

    let storage_type: u8 = matches.get_one::<String>("type").map_or_else(
        || {
            if is_json_mode() {
                handle_error(
                    "Missing required argument: --type (required in --mode json)".to_string(),
                    None,
                );
            }
            let selected_storage_type: u8 = Select::new()
                .with_prompt("Enter the type of the storage")
                .default(0)
                .items(&["local", "s3"])
                .interact()
                .unwrap_or_else(|e| {
                    handle_error(format!("Error: {}", e), None);
                }) as u8;
            selected_storage_type
        },
        |storage_type| match storage_type.as_str() {
            "local" => 0u8,
            "s3" => 1u8,
            _ => {
                handle_error(format!("Unknown storage type '{}'", storage_type), None);
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
                if is_json_mode() {
                    handle_error(
                        "Missing required argument: --path (required in --mode json)".to_string(),
                        None,
                    );
                }
                let typed_path: String = Input::<String>::new()
                    .with_prompt("Enter the path for local storage")
                    .interact_text()
                    .unwrap_or_else(|e| {
                        handle_error(format!("Error: {}", e), None);
                    });
                typed_path
            },
            |path| path.to_string(),
        );

        if !Path::new(&path).exists() {
            std::fs::create_dir_all(&path)
                .unwrap_or_else(|e| handle_error(format!("Failed to create path: {}", e), None));
        }

        storage.path = Some(path);
    } else {
        let region = matches.get_one::<String>("region").map_or_else(
            || {
                if is_json_mode() {
                    handle_error(
                        "Missing required argument: --region (required in --mode json)".to_string(),
                        None,
                    );
                }
                let typed_region: String = Input::<String>::new()
                    .with_prompt("Enter the S3 region")
                    .interact_text()
                    .unwrap_or_else(|e| {
                        handle_error(format!("Error: {}", e), None);
                    });
                typed_region
            },
            |region| region.to_string(),
        );

        let bucket = matches.get_one::<String>("bucket").map_or_else(
            || {
                if is_json_mode() {
                    handle_error(
                        "Missing required argument: --bucket (required in --mode json)".to_string(),
                        None,
                    );
                }
                let typed_bucket: String = Input::<String>::new()
                    .with_prompt("Enter the S3 bucket")
                    .interact_text()
                    .unwrap_or_else(|e| {
                        handle_error(format!("Error: {}", e), None);
                    });
                typed_bucket
            },
            |bucket| bucket.to_string(),
        );

        let access_key = matches.get_one::<String>("access-key").map_or_else(
            || {
                if is_json_mode() {
                    handle_error(
                        "Missing required argument: --access-key (required in --mode json)"
                            .to_string(),
                        None,
                    );
                }
                let typed_access_key: String = Input::<String>::new()
                    .with_prompt("Enter the S3 access key")
                    .interact_text()
                    .unwrap_or_else(|e| {
                        handle_error(format!("Error: {}", e), None);
                    });
                typed_access_key
            },
            |access_key| access_key.to_string(),
        );

        let secret_key = matches.get_one::<String>("secret-key").map_or_else(
            || {
                if is_json_mode() {
                    handle_error(
                        "Missing required argument: --secret-key (required in --mode json)"
                            .to_string(),
                        None,
                    );
                }
                let typed_secret_key: String = Input::<String>::new()
                    .with_prompt("Enter the S3 secret key")
                    .interact_text()
                    .unwrap_or_else(|e| {
                        handle_error(format!("Error: {}", e), None);
                    });
                typed_secret_key
            },
            |secret_key| secret_key.to_string(),
        );

        let endpoint = matches.get_one::<String>("endpoint").map_or_else(
            || {
                if is_json_mode() {
                    return format!("https://s3.{}.amazonaws.com", region);
                }
                let typed_endpoint: String = Input::<String>::new()
                    .with_prompt("Enter the S3 endpoint")
                    .default(format!("https://s3.{}.amazonaws.com", region))
                    .show_default(true)
                    .interact_text()
                    .unwrap_or_else(|e| {
                        handle_error(format!("Error: {}", e), None);
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

    let json_progress = if is_json_mode() {
        let progress = JsonProgress::new(1);
        progress.set_message(&format!("Writing storage '{}'...", name));
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
        pb.set_message(format!("Writing storage '{}'...", name));
        pb
    };

    let home_dir = home_dir().unwrap();

    let mut storage_path = home_dir.join(".gib").join("storages");

    if !storage_path.exists() {
        std::fs::create_dir_all(&storage_path).unwrap_or_else(|e| {
            handle_error(format!("Failed to create storage directory: {}", e), None)
        });
    }

    storage_path.push(format!("{}.msgpack", name));

    let mut buf = Vec::new();
    storage
        .serialize(&mut Serializer::new(&mut buf))
        .unwrap_or_else(|e| handle_error(format!("Failed to serialize storage: {}", e), None));

    std::fs::write(&storage_path, buf)
        .unwrap_or_else(|e| handle_error(format!("Failed to write storage: {}", e), None));

    if let Some(progress) = &json_progress {
        progress.inc_by(1);
    }

    if is_json_mode() {
        #[derive(Serialize)]
        struct StorageOutput {
            name: String,
            storage_type: String,
            path: Option<String>,
            region: Option<String>,
            bucket: Option<String>,
            endpoint: Option<String>,
        }

        let storage_type_label = match storage.storage_type {
            0 => "local",
            1 => "s3",
            _ => "unknown",
        };

        let payload = StorageOutput {
            name,
            storage_type: storage_type_label.to_string(),
            path: storage.path,
            region: storage.region,
            bucket: storage.bucket,
            endpoint: storage.endpoint,
        };
        emit_output(&payload);
    } else {
        let elapsed = pb.elapsed();

        pb.set_style(ProgressStyle::with_template("{prefix:.green} {msg}").unwrap());
        pb.set_prefix("OK");
        pb.finish_with_message(format!("Storage written ({:.2?})", elapsed));
    }
}
