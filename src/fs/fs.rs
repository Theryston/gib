use async_trait::async_trait;
use sha2::{Digest, Sha256};

#[async_trait]
pub trait FS: Send + Sync {
    async fn read_file(&self, path: &str) -> Result<Vec<u8>, std::io::Error>;
    async fn write_file(&self, path: &str, data: &[u8]) -> Result<(), std::io::Error>;
    async fn list_files(&self, path: &str) -> Result<Vec<String>, std::io::Error>;
    async fn delete_file(&self, path: &str) -> Result<(), std::io::Error>;

    /// Reads an object and returns the provider version used for conditional writes.
    ///
    /// The default implementation uses a content hash. Providers with native object
    /// versions (such as S3 ETags) should override it so the compare-and-swap remains
    /// atomic on the provider.
    async fn read_file_with_version(
        &self,
        path: &str,
    ) -> Result<(Vec<u8>, String), std::io::Error> {
        let data = self.read_file(path).await?;
        Ok((data.clone(), content_version(&data)))
    }

    /// Writes an object only when its current version matches `expected_version`.
    /// `None` means that the object must not exist yet.
    async fn write_file_if_version(
        &self,
        path: &str,
        data: &[u8],
        expected_version: Option<&str>,
    ) -> Result<(), std::io::Error> {
        let current = self.read_file_with_version(path).await;
        match (expected_version, current) {
            (Some(expected), Ok((_, actual))) if expected == actual => {}
            (None, Err(error)) if error.kind() == std::io::ErrorKind::NotFound => {}
            (Some(_), Err(error)) if error.kind() == std::io::ErrorKind::NotFound => {
                return Err(std::io::Error::new(
                    std::io::ErrorKind::AlreadyExists,
                    "conditional write failed because the object was removed",
                ));
            }
            (None, Ok(_)) => {
                return Err(std::io::Error::new(
                    std::io::ErrorKind::AlreadyExists,
                    "conditional write failed because the object already exists",
                ));
            }
            (_, Err(error)) => return Err(error),
            (Some(_), Ok(_)) => {
                return Err(std::io::Error::new(
                    std::io::ErrorKind::AlreadyExists,
                    "conditional write failed because the object changed",
                ));
            }
        }

        self.write_file(path, data).await
    }
}

pub(crate) fn content_version(data: &[u8]) -> String {
    format!("sha256:{:x}", Sha256::digest(data))
}
