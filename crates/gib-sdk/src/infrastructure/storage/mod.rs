mod local;
mod memory;
#[cfg(feature = "s3")]
mod s3;

pub use local::{LocalStorage, LocalStorageOperation};
pub use memory::{MemoryStorage, MemoryStorageOperation};
#[cfg(feature = "s3")]
pub use s3::{
    DEFAULT_S3_MAX_CONCURRENCY, DEFAULT_S3_MULTIPART_PART_SIZE, DEFAULT_S3_MULTIPART_THRESHOLD,
    MAX_S3_MULTIPART_PART_SIZE, MAX_S3_MULTIPART_THRESHOLD, MAX_S3_MULTIPART_UPLOAD_PARTS,
    MIN_S3_MULTIPART_PART_SIZE, S3Storage, S3StorageConfig,
};
