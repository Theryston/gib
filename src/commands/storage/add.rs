use clap::ArgMatches;
use dialoguer::{Input, Select};
use dirs::home_dir;
use indicatif::{ProgressBar, ProgressStyle};
use rmp_serde::Serializer;
use serde::Serialize;
use std::collections::BTreeMap;
use std::time::Duration;

use crate::output::{JsonProgress, emit_output, is_json_mode};
use crate::storage_clients::{
    public_fields, storage_definition, storage_definitions, storage_type_values, StorageConfig,
    StorageDefinition, StorageField, StorageFields,
};
use crate::utils::handle_error;

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

    let storage_definition = select_storage_definition(matches);

    let mut fields = StorageFields::new();
    for field in storage_definition.fields {
        if let Some(value) = resolve_field_value(field, matches, &fields) {
            fields.insert(field.key.to_string(), value);
        }
    }

    let mut storage = StorageConfig {
        storage_type: storage_definition.id.to_string(),
        fields,
    };

    if let Some(prepare) = storage_definition.prepare {
        prepare(&mut storage).unwrap_or_else(|e| handle_error(e, None));
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
            fields: BTreeMap<String, String>,
            path: Option<String>,
            region: Option<String>,
            bucket: Option<String>,
            endpoint: Option<String>,
        }

        let fields = public_fields(&storage);
        let payload = StorageOutput {
            name,
            storage_type: storage.storage_type.clone(),
            path: fields.get("path").cloned(),
            region: fields.get("region").cloned(),
            bucket: fields.get("bucket").cloned(),
            endpoint: fields.get("endpoint").cloned(),
            fields,
        };
        emit_output(&payload);
    } else {
        let elapsed = pb.elapsed();

        pb.set_style(ProgressStyle::with_template("{prefix:.green} {msg}").unwrap());
        pb.set_prefix("OK");
        pb.finish_with_message(format!("Storage written ({:.2?})", elapsed));
    }
}

fn select_storage_definition(matches: &ArgMatches) -> &'static StorageDefinition {
    let storage_type = matches.get_one::<String>("type").map(|value| value.to_string());

    if let Some(storage_type) = storage_type {
        return storage_definition(&storage_type).unwrap_or_else(|| {
            let available = storage_type_values().join(", ");
            handle_error(
                format!(
                    "Unknown storage type '{}'. Available types: {}",
                    storage_type, available
                ),
                None,
            );
        });
    }

    if is_json_mode() {
        let available = storage_type_values().join(", ");
        handle_error(
            format!(
                "Missing required argument: --type (required in --mode json). Available types: {}",
                available
            ),
            None,
        );
    }

    let definitions = storage_definitions();
    let items: Vec<&str> = definitions.iter().map(|definition| definition.label).collect();

    let selected_storage_type: usize = Select::new()
        .with_prompt("Enter the type of the storage")
        .default(0)
        .items(&items)
        .interact()
        .unwrap_or_else(|e| {
            handle_error(format!("Error: {}", e), None);
        });

    &definitions[selected_storage_type]
}

fn resolve_field_value(
    field: &StorageField,
    matches: &ArgMatches,
    fields: &StorageFields,
) -> Option<String> {
    if let Some(value) = matches.get_one::<String>(field.arg_name) {
        return Some(value.to_string());
    }

    let default_value = field.default_value.map(|default| default(fields));

    if is_json_mode() {
        if field.required && default_value.is_none() {
            handle_error(
                format!(
                    "Missing required argument: --{} (required in --mode json)",
                    field.arg_name
                ),
                None,
            );
        }

        return default_value;
    }

    if !field.required && default_value.is_none() {
        return None;
    }

    let mut input = Input::<String>::new().with_prompt(field.prompt);
    if let Some(default) = &default_value {
        input = input.default(default.clone()).show_default(true);
    }

    let typed_value: String = input.interact_text().unwrap_or_else(|e| {
        handle_error(format!("Error: {}", e), None);
    });

    if field.required && typed_value.trim().is_empty() {
        handle_error(format!("Field '{}' is required", field.arg_name), None);
    }

    if typed_value.is_empty() {
        return None;
    }

    Some(typed_value)
}
