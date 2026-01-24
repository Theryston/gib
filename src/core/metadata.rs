use serde::{Deserialize, Serialize};
use std::collections::HashMap;

#[derive(Debug, PartialEq, Deserialize, Serialize, Clone)]
pub(crate) struct BackupSummary {
    pub(crate) message: String,
    pub(crate) hash: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) timestamp: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) size: Option<u64>,
}

#[derive(Debug, PartialEq, Deserialize, Serialize, Clone)]
pub(crate) struct Backup {
    pub(crate) message: String,
    pub(crate) hash: String,
    pub(crate) timestamp: u64,
    pub(crate) author: String,
    pub(crate) tree: HashMap<String, BackupObject>,
}

#[derive(Debug, PartialEq, Deserialize, Serialize, Clone)]
pub(crate) struct BackupObject {
    pub(crate) hash: String,
    pub(crate) size: u64,
    pub(crate) content_type: String,
    pub(crate) permissions: u32,
    pub(crate) chunks: Vec<String>,
}

#[derive(Debug, PartialEq, Deserialize, Serialize, Clone)]
pub(crate) struct ChunkIndex {
    pub(crate) refcount: u32,
}
