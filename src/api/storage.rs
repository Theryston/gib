use super::client::Gib;
use super::error::{ErrorCode, GibError};
use super::event::{GibEvent, OperationKind, OperationStarted};
use crate::config::{self, StorageRecord};
use crate::storage::{WebDavFS, WebDavFSConfig};
use serde::Serialize;
use std::fmt;
use std::path::PathBuf;

/// Supported repository storage backends.
#[non_exhaustive]
#[derive(Clone, PartialEq, Eq)]
pub enum StorageConfig {
    Local(LocalStorageConfig),
    S3(S3StorageConfig),
    WebDav(WebDavStorageConfig),
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct LocalStorageConfig {
    pub path: PathBuf,
}

#[derive(Clone, PartialEq, Eq)]
pub struct S3StorageConfig {
    pub region: String,
    pub bucket: String,
    pub access_key: String,
    pub secret_key: String,
    pub endpoint: Option<String>,
}

#[derive(Clone, PartialEq, Eq)]
pub struct WebDavStorageConfig {
    pub url: String,
    pub username: String,
    pub password: String,
}

impl fmt::Debug for StorageConfig {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Local(config) => formatter.debug_tuple("Local").field(config).finish(),
            Self::S3(config) => formatter.debug_tuple("S3").field(config).finish(),
            Self::WebDav(config) => formatter.debug_tuple("WebDav").field(config).finish(),
        }
    }
}

impl fmt::Debug for S3StorageConfig {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("S3StorageConfig")
            .field("region", &self.region)
            .field("bucket", &self.bucket)
            .field("access_key", &"[redacted]")
            .field("secret_key", &"[redacted]")
            .field("endpoint", &self.endpoint)
            .finish()
    }
}

impl fmt::Debug for WebDavStorageConfig {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("WebDavStorageConfig")
            .field("url", &self.url)
            .field("username", &self.username)
            .field("password", &"[redacted]")
            .finish()
    }
}

#[derive(Debug)]
pub struct AddStorageRequest {
    pub name: String,
    pub config: StorageConfig,
    pub validate_remote: bool,
}

impl AddStorageRequest {
    pub fn local(name: impl Into<String>, path: impl Into<PathBuf>) -> Self {
        Self {
            name: name.into(),
            config: StorageConfig::Local(LocalStorageConfig { path: path.into() }),
            validate_remote: true,
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct StorageInfo {
    pub name: String,
    pub storage_type: String,
    pub path: Option<PathBuf>,
    pub region: Option<String>,
    pub bucket: Option<String>,
    pub endpoint: Option<String>,
    pub url: Option<String>,
    pub username: Option<String>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct StorageChange {
    pub name: String,
    pub replaced: bool,
}

impl Gib {
    pub async fn add_storage(&self, request: AddStorageRequest) -> Result<StorageChange, GibError> {
        self.events()
            .emit(GibEvent::OperationStarted(OperationStarted {
                operation: OperationKind::Storage,
            }));
        validate_name(&request.name)?;
        let config = match request.config {
            StorageConfig::Local(mut config) => {
                config.path = super::client::path_from_context(&self.inner.context, &config.path);
                StorageConfig::Local(config)
            }
            config => config,
        };
        let record = record_from_config(&config)?;
        if let StorageConfig::Local(config) = &config {
            std::fs::create_dir_all(&config.path).map_err(|error| {
                GibError::new(
                    ErrorCode::Io,
                    format!(
                        "Failed to create local storage '{}': {error}",
                        config.path.display()
                    ),
                )
            })?;
        }
        if request.validate_remote
            && let StorageConfig::WebDav(config) = &config
        {
            let webdav = WebDavFS::new(WebDavFSConfig {
                url: config.url.clone(),
                username: config.username.clone(),
                password: config.password.clone(),
            })
            .map_err(|error| GibError::new(ErrorCode::InvalidStorageConfiguration, error))?;
            webdav.validate_root().await.map_err(|error| {
                GibError::new(
                    ErrorCode::InvalidStorageConfiguration,
                    format!("Failed to validate WebDAV storage: {error}"),
                )
            })?;
        }

        let replaced = config::list_storage_names(&self.inner.context.data_dir)
            .map_err(super::error::map_error)?
            .iter()
            .any(|name| name == &request.name);
        config::save_storage(&self.inner.context.data_dir, &request.name, &record)
            .map_err(super::error::map_error)?;
        Ok(StorageChange {
            name: request.name,
            replaced,
        })
    }

    pub fn list_storages(&self) -> Result<Vec<StorageInfo>, GibError> {
        let names = config::list_storage_names(&self.inner.context.data_dir)
            .map_err(super::error::map_error)?;
        names
            .into_iter()
            .map(|name| {
                let record = config::load_storage(&self.inner.context.data_dir, &name)
                    .map_err(super::error::map_error)?;
                info_from_record(name, record, &self.inner.context)
            })
            .collect()
    }

    pub fn remove_storage(&self, name: &str) -> Result<bool, GibError> {
        validate_name(name)?;
        self.events()
            .emit(GibEvent::OperationStarted(OperationStarted {
                operation: OperationKind::Storage,
            }));
        let removed = config::remove_storage(&self.inner.context.data_dir, name)
            .map_err(super::error::map_error)?;
        Ok(removed)
    }
}

pub(crate) fn record_from_config(config: &StorageConfig) -> Result<StorageRecord, GibError> {
    let record = match config {
        StorageConfig::Local(config) => StorageRecord {
            storage_type: 0,
            path: Some(config.path.to_string_lossy().to_string()),
            region: None,
            bucket: None,
            access_key: None,
            secret_key: None,
            endpoint: None,
            url: None,
            username: None,
            password: None,
        },
        StorageConfig::S3(config) => StorageRecord {
            storage_type: 1,
            path: None,
            region: Some(config.region.clone()),
            bucket: Some(config.bucket.clone()),
            access_key: Some(config.access_key.clone()),
            secret_key: Some(config.secret_key.clone()),
            endpoint: config.endpoint.clone(),
            url: None,
            username: None,
            password: None,
        },
        StorageConfig::WebDav(config) => StorageRecord {
            storage_type: 2,
            path: None,
            region: None,
            bucket: None,
            access_key: None,
            secret_key: None,
            endpoint: None,
            url: Some(config.url.clone()),
            username: Some(config.username.clone()),
            password: Some(config.password.clone()),
        },
    };
    validate_record(&record)?;
    Ok(record)
}

pub(crate) fn info_from_record(
    name: String,
    record: StorageRecord,
    context: &super::client::GibContext,
) -> Result<StorageInfo, GibError> {
    let storage_type = match record.storage_type {
        0 => "local",
        1 => "s3",
        2 => "webdav",
        value => {
            return Err(GibError::new(
                ErrorCode::InvalidStorageConfiguration,
                format!("Storage '{name}' has invalid type {value}"),
            ));
        }
    };
    Ok(StorageInfo {
        name,
        storage_type: storage_type.to_string(),
        path: record
            .path
            .map(|path| super::client::path_from_context(context, &PathBuf::from(path))),
        region: record.region,
        bucket: record.bucket,
        endpoint: record.endpoint,
        url: record.url,
        username: record.username,
    })
}

pub(crate) fn validate_record(record: &StorageRecord) -> Result<(), GibError> {
    match record.storage_type {
        0 if record
            .path
            .as_deref()
            .is_some_and(|path| !path.trim().is_empty()) =>
        {
            Ok(())
        }
        1 if record
            .region
            .as_deref()
            .is_some_and(|value| !value.trim().is_empty())
            && record
                .bucket
                .as_deref()
                .is_some_and(|value| !value.trim().is_empty())
            && record
                .access_key
                .as_deref()
                .is_some_and(|value| !value.trim().is_empty())
            && record
                .secret_key
                .as_deref()
                .is_some_and(|value| !value.trim().is_empty()) =>
        {
            Ok(())
        }
        2 if record
            .url
            .as_deref()
            .is_some_and(|value| !value.trim().is_empty())
            && record
                .username
                .as_deref()
                .is_some_and(|value| !value.trim().is_empty())
            && record
                .password
                .as_deref()
                .is_some_and(|value| !value.trim().is_empty()) =>
        {
            Ok(())
        }
        0..=2 => Err(GibError::new(
            ErrorCode::InvalidStorageConfiguration,
            "Storage configuration is missing required fields",
        )),
        value => Err(GibError::new(
            ErrorCode::InvalidStorageConfiguration,
            format!("Unknown storage type {value}"),
        )),
    }
}

fn validate_name(name: &str) -> Result<(), GibError> {
    if name.is_empty()
        || !name
            .chars()
            .all(|character| character.is_ascii_alphanumeric() || matches!(character, '-' | '_'))
    {
        return Err(GibError::new(
            ErrorCode::InvalidStorageConfiguration,
            "Storage names may contain only ASCII letters, numbers, hyphens, and underscores",
        ));
    }
    Ok(())
}
