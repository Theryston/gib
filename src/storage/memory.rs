//! An in-memory filesystem implementation for tests and embedded callers.

use super::{FS, content_version};
use async_trait::async_trait;
use std::collections::BTreeMap;
use std::io::{Error, ErrorKind};
use std::sync::{Arc, RwLock};

/// A small, cloneable storage backend that keeps repository objects in memory.
///
/// MemoryFS is useful for exercising the public API without creating a
/// temporary directory or contacting a remote provider. Clones share the same
/// object map.
#[derive(Clone, Default)]
pub struct MemoryFS {
    files: Arc<RwLock<BTreeMap<String, Vec<u8>>>>,
}

impl std::fmt::Debug for MemoryFS {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let count = self
            .files
            .read()
            .map(|files| files.len())
            .unwrap_or_default();
        formatter
            .debug_struct("MemoryFS")
            .field("file_count", &count)
            .finish()
    }
}

impl MemoryFS {
    /// Creates an empty in-memory backend.
    pub fn new() -> Self {
        Self::default()
    }

    /// Inserts or replaces an object synchronously.
    ///
    /// This helper is convenient for fixtures. Normal callers can use the
    /// asynchronous FS::write_file method instead.
    pub fn insert(&self, path: impl AsRef<str>, data: impl AsRef<[u8]>) -> Result<(), Error> {
        let path = normalize_path(path.as_ref())?;
        let mut files = self
            .files
            .write()
            .map_err(|_| Error::other("in-memory storage lock is poisoned"))?;
        files.insert(path, data.as_ref().to_vec());
        Ok(())
    }

    /// Returns a copy of all currently stored object paths.
    pub fn paths(&self) -> Result<Vec<String>, Error> {
        let files = self
            .files
            .read()
            .map_err(|_| Error::other("in-memory storage lock is poisoned"))?;
        Ok(files.keys().cloned().collect())
    }
}

#[async_trait]
impl FS for MemoryFS {
    async fn read_file(&self, path: &str) -> Result<Vec<u8>, Error> {
        let path = normalize_path(path)?;
        let files = self
            .files
            .read()
            .map_err(|_| Error::other("in-memory storage lock is poisoned"))?;
        files.get(&path).cloned().ok_or_else(|| not_found(&path))
    }

    async fn write_file(&self, path: &str, data: &[u8]) -> Result<(), Error> {
        let path = normalize_path(path)?;
        let mut files = self
            .files
            .write()
            .map_err(|_| Error::other("in-memory storage lock is poisoned"))?;
        files.insert(path, data.to_vec());
        Ok(())
    }

    async fn list_files(&self, path: &str) -> Result<Vec<String>, Error> {
        let path = normalize_prefix(path)?;
        let files = self
            .files
            .read()
            .map_err(|_| Error::other("in-memory storage lock is poisoned"))?;
        let mut paths = files
            .keys()
            .filter(|candidate| {
                path.is_empty() || *candidate == &path || candidate.starts_with(&format!("{path}/"))
            })
            .cloned()
            .collect::<Vec<_>>();
        paths.sort();
        Ok(paths)
    }

    async fn delete_file(&self, path: &str) -> Result<(), Error> {
        let path = normalize_path(path)?;
        let mut files = self
            .files
            .write()
            .map_err(|_| Error::other("in-memory storage lock is poisoned"))?;
        if files.remove(&path).is_none() {
            return Err(not_found(&path));
        }
        Ok(())
    }

    async fn read_file_with_version(&self, path: &str) -> Result<(Vec<u8>, String), Error> {
        let data = self.read_file(path).await?;
        let version = content_version(&data);
        Ok((data, version))
    }

    async fn write_file_if_version(
        &self,
        path: &str,
        data: &[u8],
        expected_version: Option<&str>,
    ) -> Result<(), Error> {
        let path = normalize_path(path)?;
        let mut files = self
            .files
            .write()
            .map_err(|_| Error::other("in-memory storage lock is poisoned"))?;
        match (expected_version, files.get(&path)) {
            (None, None) => {}
            (Some(expected), Some(current)) if expected == content_version(current) => {}
            (None, Some(_)) => {
                return Err(Error::new(
                    ErrorKind::AlreadyExists,
                    "conditional write failed because the object already exists",
                ));
            }
            (Some(_), None) => {
                return Err(Error::new(
                    ErrorKind::AlreadyExists,
                    "conditional write failed because the object was removed",
                ));
            }
            (Some(_), Some(_)) => {
                return Err(Error::new(
                    ErrorKind::AlreadyExists,
                    "conditional write failed because the object changed",
                ));
            }
        }
        files.insert(path, data.to_vec());
        Ok(())
    }
}

fn normalize_path(path: &str) -> Result<String, Error> {
    let normalized = path.replace('\\', "/").trim_matches('/').to_string();
    if normalized.is_empty()
        || normalized
            .split('/')
            .any(|part| part.is_empty() || part == "." || part == "..")
    {
        return Err(Error::new(
            ErrorKind::InvalidInput,
            "in-memory storage paths must be relative and non-empty",
        ));
    }
    Ok(normalized)
}

fn normalize_prefix(path: &str) -> Result<String, Error> {
    if path.trim().is_empty() {
        return Ok(String::new());
    }
    normalize_path(path)
}

fn not_found(path: &str) -> Error {
    Error::new(
        ErrorKind::NotFound,
        format!("in-memory storage object '{path}' was not found"),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn lists_objects_and_enforces_conditional_writes() {
        let fs = MemoryFS::new();
        fs.write_file("project/chunks/aa/bb", b"one")
            .await
            .expect("object should be written");
        fs.write_file("project/indexes/HEAD", b"head")
            .await
            .expect("head should be written");

        assert_eq!(
            fs.list_files("project/chunks")
                .await
                .expect("objects should be listed"),
            vec!["project/chunks/aa/bb"]
        );

        let (_, version) = fs
            .read_file_with_version("project/indexes/HEAD")
            .await
            .expect("head should be readable");
        fs.write_file_if_version("project/indexes/HEAD", b"next", Some(&version))
            .await
            .expect("matching version should be accepted");
        let error = fs
            .write_file_if_version("project/indexes/HEAD", b"stale", Some(&version))
            .await
            .expect_err("stale version should be rejected");
        assert_eq!(error.kind(), ErrorKind::AlreadyExists);
    }
}
