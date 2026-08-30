use crate::commands::storage::add::{LOCAL_STORAGE_TYPE, S3_STORAGE_TYPE, WEBDAV_STORAGE_TYPE};
use crate::output::{emit_output, is_json_mode};
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

        let storage_type = match storage.storage_type {
            LOCAL_STORAGE_TYPE => "local",
            S3_STORAGE_TYPE => "s3",
            WEBDAV_STORAGE_TYPE => "webdav",
            _ => "unknown",
        };

        let details = match storage.storage_type {
            LOCAL_STORAGE_TYPE => format!("path: {}", storage.path.clone().unwrap_or_default()),
            S3_STORAGE_TYPE => format!(
                "region: {}, bucket: {}, access_key: {}, secret_key: {}, endpoint: {}",
                storage.region.clone().unwrap_or_default(),
                storage.bucket.clone().unwrap_or_default(),
                "********",
                "********",
                storage.endpoint.clone().unwrap_or_default()
            ),
            WEBDAV_STORAGE_TYPE => format!(
                "url: {}, username: {}, password: ********",
                storage.url.clone().unwrap_or_default(),
                storage.username.clone().unwrap_or_default(),
            ),
            _ => "unknown".to_string(),
        };

        rows.push(StorageRow {
            name: storage_name.to_string(),
            storage_type: storage_type.to_string(),
            details: details.clone(),
        });

        json_rows.push(StorageInfo {
            name: storage_name.to_string(),
            storage_type: storage_type.to_string(),
            path: storage.path,
            region: storage.region,
            bucket: storage.bucket,
            endpoint: storage.endpoint,
            url: storage.url,
            username: storage.username,
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
    path: Option<String>,
    region: Option<String>,
    bucket: Option<String>,
    endpoint: Option<String>,
    url: Option<String>,
    username: Option<String>,
}
