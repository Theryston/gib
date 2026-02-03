use async_trait::async_trait;

#[async_trait]
pub trait ClientStorage: Send + Sync {
    async fn read_file(&self, path: &str) -> Result<Vec<u8>, std::io::Error>;
    async fn write_file(&self, path: &str, data: &[u8]) -> Result<(), std::io::Error>;
    async fn list_files(&self, path: &str) -> Result<Vec<String>, std::io::Error>;
    async fn delete_file(&self, path: &str) -> Result<(), std::io::Error>;
}

