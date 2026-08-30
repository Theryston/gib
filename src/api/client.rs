use super::error::{ErrorCode, GibError};
use super::event::{EventCallback, EventDispatcher};
use crate::config::{self, LocalConfigContext, StorageRecord};
use crate::storage::{FS, LocalFS, S3FS, S3FSConfig, WebDavFS, WebDavFSConfig};
use serde::Serialize;
use std::fmt;
use std::path::{Path, PathBuf};
use std::sync::Arc;

/// Explicit filesystem/configuration context used by a [`Gib`] client.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct GibContext {
    pub data_dir: PathBuf,
    pub working_dir: PathBuf,
    pub config_path: Option<PathBuf>,
    pub discover_config: bool,
}

/// Non-secret values resolved from the optional `gib.toml` file.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize)]
pub struct ConfigDefaults {
    pub config_path: Option<PathBuf>,
    pub repository_storage: Option<String>,
    pub repository_key: Option<String>,
    pub backup_root: Option<PathBuf>,
    pub backup_message: Option<String>,
    pub compression: Option<i32>,
    pub chunk_size: Option<u64>,
    pub concurrency: Option<usize>,
    pub ignore_patterns: Vec<String>,
    pub include_git: bool,
    pub live_message: Option<String>,
    pub live_debounce_ms: Option<u64>,
    pub live_poll_ms: Option<u64>,
    pub restore_target: Option<PathBuf>,
}

impl ConfigDefaults {
    /// Combines configured and explicit ignore names with deterministic
    /// de-duplication. CLI adapters can use this without depending on the
    /// private configuration model.
    pub fn merged_ignore_patterns(&self, explicit: &[String]) -> Vec<String> {
        crate::config::merge_ignore_patterns(&self.ignore_patterns, explicit)
    }
}

impl GibContext {
    pub(crate) fn local_config(&self) -> Result<LocalConfigContext, GibError> {
        config::load_local_config(
            &self.working_dir,
            self.config_path.as_deref(),
            self.discover_config,
        )
        .map_err(super::error::map_error)
    }
}

pub(crate) struct ClientInner {
    pub(crate) context: GibContext,
    pub(crate) events: EventDispatcher,
    pub(crate) injected_backend: Option<Arc<dyn FS>>,
}

/// Reusable, cheaply cloneable GIB service facade.
#[derive(Clone)]
pub struct Gib {
    pub(crate) inner: Arc<ClientInner>,
}

impl fmt::Debug for Gib {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("Gib")
            .field("context", &self.inner.context)
            .field("has_event_callback", &self.inner.events.has_callback())
            .field(
                "has_injected_backend",
                &self.inner.injected_backend.is_some(),
            )
            .finish()
    }
}

/// Builder for a silent, explicitly configured [`Gib`] client.
pub struct GibBuilder {
    context: GibContext,
    callback: Option<EventCallback>,
    injected_backend: Option<Arc<dyn FS>>,
}

impl fmt::Debug for GibBuilder {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("GibBuilder")
            .field("context", &self.context)
            .field("has_event_callback", &self.callback.is_some())
            .field("has_injected_backend", &self.injected_backend.is_some())
            .finish()
    }
}

impl GibBuilder {
    pub fn new() -> Result<Self, GibError> {
        let working_dir = std::env::current_dir().map_err(|error| {
            GibError::new(
                ErrorCode::Io,
                format!("Failed to determine the working directory: {error}"),
            )
        })?;
        let data_dir = dirs::home_dir()
            .map(|path| path.join(".gib"))
            .ok_or_else(|| {
                GibError::new(
                    ErrorCode::ConfigurationNotFound,
                    "Failed to determine the home directory",
                )
            })?;
        Ok(Self {
            context: GibContext {
                data_dir,
                working_dir,
                config_path: None,
                discover_config: true,
            },
            callback: None,
            injected_backend: None,
        })
    }

    pub fn data_dir(mut self, path: impl Into<PathBuf>) -> Self {
        self.context.data_dir = path.into();
        self
    }

    pub fn working_dir(mut self, path: impl Into<PathBuf>) -> Self {
        self.context.working_dir = path.into();
        self
    }

    pub fn config_path(mut self, path: impl Into<PathBuf>) -> Self {
        self.context.config_path = Some(path.into());
        self
    }

    pub fn discover_config(mut self, discover: bool) -> Self {
        self.context.discover_config = discover;
        self
    }

    pub fn on_event<F>(mut self, callback: F) -> Self
    where
        F: Fn(super::event::GibEvent) + Send + Sync + 'static,
    {
        self.callback = Some(Arc::new(callback));
        self
    }

    pub fn with_event_callback(mut self, callback: Option<EventCallback>) -> Self {
        self.callback = callback;
        self
    }

    /// Inject a storage backend for tests or embedding applications.
    pub fn storage_backend(mut self, backend: Arc<dyn FS>) -> Self {
        self.injected_backend = Some(backend);
        self
    }

    pub fn build(self) -> Result<Gib, GibError> {
        Ok(Gib {
            inner: Arc::new(ClientInner {
                context: self.context,
                events: EventDispatcher::new(self.callback),
                injected_backend: self.injected_backend,
            }),
        })
    }
}

impl Gib {
    pub fn builder() -> GibBuilder {
        GibBuilder::new().unwrap_or_else(|_| GibBuilder {
            context: GibContext {
                data_dir: PathBuf::from(".gib"),
                working_dir: PathBuf::from("."),
                config_path: None,
                discover_config: true,
            },
            callback: None,
            injected_backend: None,
        })
    }

    pub fn from_default_environment() -> Result<Self, GibError> {
        GibBuilder::new()?.build()
    }

    pub fn context(&self) -> &GibContext {
        &self.inner.context
    }

    /// Resolves non-interactive local configuration for a caller that wants
    /// to apply the same defaults as the command line client.
    pub fn config_defaults(&self) -> Result<ConfigDefaults, GibError> {
        let local = self.inner.context.local_config()?;
        let config = &local.config;
        let resolve = |value: Option<&str>| {
            value.map(|value| {
                crate::config::resolve_path(
                    None,
                    Some(value),
                    &local,
                    &self.inner.context.working_dir,
                )
            })
        };
        let chunk_size = config
            .backup
            .chunk_size
            .as_deref()
            .map(parse_size::parse_size)
            .transpose()
            .map_err(|_| {
                GibError::new(
                    ErrorCode::InvalidConfiguration,
                    "Invalid configured chunk size",
                )
            })?;
        Ok(ConfigDefaults {
            config_path: local.path.clone(),
            repository_storage: config.repository.storage.clone(),
            repository_key: config.repository.key.clone(),
            backup_root: resolve(config.backup.root_path.as_deref()),
            backup_message: config.backup.message.clone(),
            compression: config.backup.compress,
            chunk_size,
            concurrency: config.backup.concurrency,
            ignore_patterns: config.backup.ignore.clone(),
            include_git: false,
            live_message: config.live.message.clone(),
            live_debounce_ms: config.live.debounce_ms,
            live_poll_ms: config.live.poll_ms,
            restore_target: resolve(config.restore.target_path.as_deref()),
        })
    }

    pub(crate) fn events(&self) -> &EventDispatcher {
        &self.inner.events
    }

    pub(crate) fn with_config_path(&self, config_path: Option<PathBuf>) -> Self {
        let mut context = self.inner.context.clone();
        if config_path.is_some() {
            context.discover_config = false;
        }
        context.config_path = config_path;
        Self {
            inner: Arc::new(ClientInner {
                context,
                events: self.inner.events.clone(),
                injected_backend: self.inner.injected_backend.clone(),
            }),
        }
    }

    pub(crate) fn backend(&self, storage_name: &str) -> Result<Arc<dyn FS>, GibError> {
        if let Some(backend) = &self.inner.injected_backend {
            return Ok(Arc::clone(backend));
        }
        let mut record =
            config::load_storage(&self.inner.context.data_dir, storage_name).map_err(|error| {
                if error.contains("No such file") || error.contains("not found") {
                    GibError::new(ErrorCode::StorageNotFound, error)
                } else {
                    GibError::new(ErrorCode::InvalidStorageConfiguration, error)
                }
            })?;
        if let Some(path) = record.path.as_deref() {
            record.path = Some(
                path_from_context(&self.inner.context, Path::new(path))
                    .to_string_lossy()
                    .into_owned(),
            );
        }
        backend_from_record(record)
    }
}

pub(crate) fn backend_from_record(record: StorageRecord) -> Result<Arc<dyn FS>, GibError> {
    match record.storage_type {
        0 => Ok(Arc::new(LocalFS::new(record.path.ok_or_else(|| {
            GibError::new(
                ErrorCode::InvalidStorageConfiguration,
                "Local storage has no path",
            )
        })?))),
        1 => Ok(Arc::new(
            S3FS::new(S3FSConfig {
                region: record.region,
                bucket: record.bucket,
                access_key: record.access_key,
                secret_key: record.secret_key,
                endpoint: record.endpoint,
            })
            .map_err(|error| GibError::new(ErrorCode::InvalidStorageConfiguration, error))?,
        )),
        2 => Ok(Arc::new(
            WebDavFS::new(WebDavFSConfig {
                url: record.url.ok_or_else(|| {
                    GibError::new(
                        ErrorCode::InvalidStorageConfiguration,
                        "WebDAV storage has no URL",
                    )
                })?,
                username: record.username.ok_or_else(|| {
                    GibError::new(
                        ErrorCode::InvalidStorageConfiguration,
                        "WebDAV storage has no username",
                    )
                })?,
                password: record.password.ok_or_else(|| {
                    GibError::new(
                        ErrorCode::InvalidStorageConfiguration,
                        "WebDAV storage has no password",
                    )
                })?,
            })
            .map_err(|error| GibError::new(ErrorCode::InvalidStorageConfiguration, error))?,
        )),
        value => Err(GibError::new(
            ErrorCode::InvalidStorageConfiguration,
            format!("Unknown storage type {value}"),
        )),
    }
}

pub(crate) fn path_from_context(context: &GibContext, value: &Path) -> PathBuf {
    if value.is_absolute() {
        value.to_path_buf()
    } else {
        context.working_dir.join(value)
    }
}
