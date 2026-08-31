mod local;
mod memory;
#[cfg(feature = "s3")]
mod s3;
#[cfg(feature = "webdav")]
mod webdav;

pub use local::{LocalStorage, LocalStorageOperation};
pub use memory::{MemoryStorage, MemoryStorageOperation};
#[cfg(feature = "s3")]
pub use s3::{
    DEFAULT_S3_CAPABILITY_CACHE_FILE_NAME, DEFAULT_S3_CAPABILITY_CACHE_TTL_SECONDS,
    DEFAULT_S3_MAX_CONCURRENCY, DEFAULT_S3_MULTIPART_PART_SIZE, DEFAULT_S3_MULTIPART_THRESHOLD,
    MAX_S3_MULTIPART_PART_SIZE, MAX_S3_MULTIPART_THRESHOLD, MAX_S3_MULTIPART_UPLOAD_PARTS,
    MIN_S3_MULTIPART_PART_SIZE, S3ConditionalWriteCapabilities, S3ConditionalWriteStatus,
    S3Storage, S3StorageConfig,
};
#[cfg(feature = "webdav")]
pub use webdav::{
    DEFAULT_WEBDAV_MAX_CONCURRENCY, DEFAULT_WEBDAV_REQUEST_TIMEOUT,
    DEFAULT_WEBDAV_TRANSFER_BUFFER_SIZE, MAX_WEBDAV_MAX_CONCURRENCY, WebDavStorage,
    WebDavStorageConfig,
};
