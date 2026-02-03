use crate::output::{emit_output, is_json_mode};
use crate::storage_clients::{public_fields, storage_definition, storage_details};
use crate::utils::{get_storage, handle_error};
use dirs::home_dir;
use tabled::{Table, Tabled};

#[derive(Tabled)]
struct StorageRow {
    name: String,
    storage_type: String,
    details: String,
}

pub fn list() {
    let home_dir = home_dir().unwrap();
    let storage_path = home_dir.join(".gib").join("storages");

    if !storage_path.exists() {
        if is_json_mode() {
            let empty: Vec<StorageInfo> = Vec::new();
            emit_output(&empty);
        } else {
            println!("No storages found.");
        }
        return;
    }

    let files = std::fs::read_dir(&storage_path)
        .unwrap_or_else(|e| handle_error(format!("Failed to read storages: {}", e), None));

    let mut rows = Vec::new();
    let mut json_rows = Vec::new();

    for file in files {
        let file = file
            .unwrap_or_else(|e| handle_error(format!("Failed to read storage entry: {}", e), None));
        let path = file.path();
        let file_name = path.file_name().unwrap().to_string_lossy().to_string();
        let storage_name = file_name.split('.').next().unwrap();
        let storage = get_storage(storage_name);

        let storage_type = storage_definition(&storage.storage_type)
            .map(|definition| definition.label)
            .unwrap_or("unknown");

        let details = storage_details(&storage);

        rows.push(StorageRow {
            name: storage_name.to_string(),
            storage_type: storage_type.to_string(),
            details: details.clone(),
        });

        let fields = public_fields(&storage);

        json_rows.push(StorageInfo {
            name: storage_name.to_string(),
            storage_type: storage_type.to_string(),
            path: fields.get("path").cloned(),
            region: fields.get("region").cloned(),
            bucket: fields.get("bucket").cloned(),
            endpoint: fields.get("endpoint").cloned(),
            fields,
        });
    }

    if is_json_mode() {
        emit_output(&json_rows);
    } else {
        let table = Table::new(rows).to_string();
        println!("{table}");
    }
}

#[derive(serde::Serialize)]
struct StorageInfo {
    name: String,
    storage_type: String,
    fields: std::collections::BTreeMap<String, String>,
    path: Option<String>,
    region: Option<String>,
    bucket: Option<String>,
    endpoint: Option<String>,
}
