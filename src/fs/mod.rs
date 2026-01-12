mod fs;
mod local;
mod s3;

pub use fs::FS;
pub use local::LocalFS;
pub use s3::{S3FS, S3FSConfig};
