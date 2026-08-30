//! Storage backends used by the GIB library.
//!
//! The filesystem implementations live behind this library-owned boundary;
//! callers can use the built-in configurations or inject their own [`FS`].

mod fs;
mod local;
mod memory;
mod s3;
mod webdav;

pub use fs::FS;
pub use local::LocalFS;
pub use memory::MemoryFS;
pub use s3::{S3FS, S3FSConfig};
pub use webdav::{WebDavFS, WebDavFSConfig};

pub(crate) use fs::content_version;
