use crate::fs::FS;
use async_trait::async_trait;
use walkdir::WalkDir;

pub struct LocalFS {
    path: std::path::PathBuf,
}

impl LocalFS {
    pub fn new(path: impl Into<std::path::PathBuf>) -> Self {
        Self { path: path.into() }
    }
}

#[async_trait]
impl FS for LocalFS {
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

        for entry in WalkDir::new(self.path.join(path)) {
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
        std::fs::remove_file(self.path.join(path))
    }
}
