use crate::utils::get_storage;
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

    let files = std::fs::read_dir(storage_path).unwrap();

    let mut rows = Vec::new();

    for file in files {
        let file = file.unwrap();
        let path = file.path();
        let file_name = path.file_name().unwrap().to_string_lossy().to_string();
        let storage_name = file_name.split('.').next().unwrap();
        let storage = get_storage(storage_name);

        let storage_type = match storage.storage_type {
            0 => "local",
            1 => "s3",
            _ => "unknown",
        };

        rows.push(StorageRow {
            name: storage_name.to_string(),
            storage_type: storage_type.to_string(),
            details: match storage.storage_type {
                0 => format!("path: {}", storage.path.unwrap()),
                1 => format!(
                    "region: {}, bucket: {}, access_key: {}, secret_key: {}, endpoint: {}",
                    storage.region.unwrap(),
                    storage.bucket.unwrap(),
                    "********",
                    "********",
                    storage.endpoint.unwrap()
                ),
                _ => "unknown".to_string(),
            },
        });
    }

    let table = Table::new(rows).to_string();

    println!("{table}");
}
