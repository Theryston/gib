use serde::{Deserialize, Serialize};
use std::collections::HashMap;

#[derive(Debug, PartialEq, Deserialize, Serialize, Clone)]
pub(crate) struct CommitSummary {
    pub(crate) message: String,
    pub(crate) hash: String,
}

#[derive(Debug, PartialEq, Deserialize, Serialize, Clone)]
pub(crate) struct Commit {
    pub(crate) message: String,
    pub(crate) hash: String,
    pub(crate) timestamp: u64,
    pub(crate) author: String,
    pub(crate) tree: HashMap<String, CommitObject>,
}

#[derive(Debug, PartialEq, Deserialize, Serialize, Clone)]
pub(crate) struct CommitObject {
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
