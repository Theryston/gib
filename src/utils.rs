use crate::commands::storage::add::Storage;
use dirs::home_dir;
use walkdir;

pub fn get_pwd_string() -> String {
    std::env::current_dir()
        .unwrap()
        .to_string_lossy()
        .to_string()
}

pub fn get_storage(name: &str) -> Storage {
    let home_dir = home_dir().unwrap();
    let storage_path = home_dir
        .join(".gib")
        .join("storages")
        .join(format!("{}.msgpack", name));
    let contents = std::fs::read(storage_path).unwrap();

    rmp_serde::from_slice(&contents).unwrap()
}

pub fn list_files(path: &str) -> Vec<String> {
    let mut files = Vec::new();
    let walker = walkdir::WalkDir::new(path)
        .into_iter()
        .filter_map(|e| e.ok())
        .filter(|e| e.path().is_file());

    for entry in walker {
        files.push(entry.path().display().to_string());
    }

    files
}
