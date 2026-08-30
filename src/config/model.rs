use serde::{Deserialize, Serialize};
use std::fmt;
use std::path::PathBuf;

pub(crate) const LOCAL_CONFIG_FILE_NAME: &str = "gib.toml";
pub(crate) const DEFAULT_AUTHOR: &str = "anonymous <anonymous@trygib.org>";

#[derive(Debug, Clone, PartialEq, Eq, Deserialize, Serialize)]
pub(crate) struct GlobalConfig {
    pub(crate) author: String,
}

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub(crate) struct LocalConfig {
    pub(crate) version: Option<u32>,
    pub(crate) repository: RepositoryConfig,
    pub(crate) backup: BackupConfig,
    pub(crate) live: LiveConfig,
    pub(crate) restore: RestoreConfig,
}

#[derive(Debug, Clone, Default, Deserialize)]
pub(crate) struct RepositoryConfig {
    pub(crate) storage: Option<String>,
    pub(crate) key: Option<String>,
}

#[derive(Debug, Clone, Default, Deserialize)]
pub(crate) struct BackupConfig {
    pub(crate) root_path: Option<String>,
    pub(crate) message: Option<String>,
    pub(crate) compress: Option<i32>,
    pub(crate) chunk_size: Option<String>,
    pub(crate) concurrency: Option<usize>,
    pub(crate) ignore: Vec<String>,
}

#[derive(Debug, Clone, Default, Deserialize)]
pub(crate) struct LiveConfig {
    pub(crate) message: Option<String>,
    pub(crate) debounce_ms: Option<u64>,
    pub(crate) poll_ms: Option<u64>,
}

#[derive(Debug, Clone, Default, Deserialize)]
pub(crate) struct RestoreConfig {
    pub(crate) target_path: Option<String>,
}

#[derive(Debug, Clone)]
pub(crate) struct LocalConfigContext {
    pub(crate) config: LocalConfig,
    pub(crate) path: Option<PathBuf>,
    pub(crate) base_dir: PathBuf,
}

impl LocalConfigContext {
    pub(crate) fn without_config(base_dir: PathBuf) -> Self {
        Self {
            config: LocalConfig::default(),
            path: None,
            base_dir,
        }
    }
}

/// The on-disk storage record. Its shape intentionally remains compatible
/// with the original MessagePack representation, including the numeric type
/// discriminator and optional fields.
#[derive(Clone, PartialEq, Eq, Deserialize, Serialize)]
pub(crate) struct StorageRecord {
    pub(crate) storage_type: u8,
    pub(crate) path: Option<String>,
    pub(crate) region: Option<String>,
    pub(crate) bucket: Option<String>,
    pub(crate) access_key: Option<String>,
    pub(crate) secret_key: Option<String>,
    pub(crate) endpoint: Option<String>,
    #[serde(default)]
    pub(crate) url: Option<String>,
    #[serde(default)]
    pub(crate) username: Option<String>,
    #[serde(default)]
    pub(crate) password: Option<String>,
}

impl fmt::Debug for StorageRecord {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("StorageRecord")
            .field("storage_type", &self.storage_type)
            .field("path", &self.path)
            .field("region", &self.region)
            .field("bucket", &self.bucket)
            .field(
                "access_key",
                &self.access_key.as_ref().map(|_| "[redacted]"),
            )
            .field(
                "secret_key",
                &self.secret_key.as_ref().map(|_| "[redacted]"),
            )
            .field("endpoint", &self.endpoint)
            .field("url", &self.url)
            .field("username", &self.username)
            .field("password", &self.password.as_ref().map(|_| "[redacted]"))
            .finish()
    }
}
