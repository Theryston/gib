use crate::storage_clients::ClientStorage;
use async_trait::async_trait;
use walkdir::WalkDir;

pub struct LocalClientStorage {
    path: std::path::PathBuf,
}

impl LocalClientStorage {
    pub fn new(path: impl Into<std::path::PathBuf>) -> Self {
        Self { path: path.into() }
    }
}

#[async_trait]
impl ClientStorage for LocalClientStorage {
    async fn read_file(&self, path: &str) -> Result<Vec<u8>, std::io::Error> {
        std::fs::read(&self.path.join(path))
    }

    async fn write_file(&self, path: &str, data: &[u8]) -> Result<(), std::io::Error> {
        let path = self.path.join(path);
        let parent_dir = path.parent().unwrap();

        if !parent_dir.exists() {
            std::fs::create_dir_all(parent_dir).unwrap();
        }

        std::fs::write(path, data)
    }

    async fn list_files(&self, path: &str) -> Result<Vec<String>, std::io::Error> {
        let mut files = Vec::new();

        let full_path = self.path.join(path);

        if !full_path.exists() {
            return Ok(files);
        }

        for entry in WalkDir::new(full_path) {
            let entry = entry?;
            if entry.file_type().is_file() {
                let path_str = entry
                    .path()
                    .strip_prefix(&self.path)
                    .unwrap()
                    .to_string_lossy()
                    .replace('\\', "/");
                files.push(path_str);
            }
        }

        Ok(files)
    }

    async fn delete_file(&self, path: &str) -> Result<(), std::io::Error> {
        let full_path = self.path.join(path);

        std::fs::remove_file(&full_path)?;

        if let Some(folder) = full_path.parent() {
            if let Ok(mut it) = folder.read_dir() {
                if it.next().is_none() {
                    let _ = std::fs::remove_dir(folder);
                }
            }
        }

        Ok(())
    }
}
