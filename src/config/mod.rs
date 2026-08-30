//! Configuration loading and deterministic value resolution.
//!
//! This module deliberately contains no terminal or command-line concerns.

mod loader;
mod model;
mod resolver;

pub(crate) use loader::{
    list_storage_names, load_global_config, load_local_config, load_storage, remove_storage,
    save_global_config, save_storage,
};
pub(crate) use model::{DEFAULT_AUTHOR, GlobalConfig, LocalConfigContext, StorageRecord};
pub(crate) use resolver::{merge_ignore_patterns, resolve_path};
