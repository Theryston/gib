use super::error::ModelError;
use super::lock::FileLock;
use super::paths::{ModelPaths, ensure_regular_or_missing, validate_path_component};
use super::storage::write_atomic;
use serde::{Deserialize, Serialize};
use std::time::Duration;

pub(crate) const AI_CONFIG_VERSION: u32 = 1;

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(default, deny_unknown_fields)]
pub(crate) struct AiConfig {
    pub(crate) version: u32,
    pub(crate) model: ModelConfig,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(default, deny_unknown_fields)]
pub(crate) struct ModelConfig {
    pub(crate) active: Option<String>,
}

impl AiConfig {
    pub(crate) fn current() -> Self {
        Self {
            version: AI_CONFIG_VERSION,
            model: ModelConfig::default(),
        }
    }

    fn validate(&self) -> Result<(), ModelError> {
        if self.version != AI_CONFIG_VERSION {
            return Err(ModelError::ActiveModel(format!(
                "unsupported AI config version {}",
                self.version
            )));
        }
        if let Some(active) = &self.model.active {
            validate_path_component(active)?;
        }
        Ok(())
    }
}

#[derive(Debug, Clone)]
pub(crate) struct AiConfigStore {
    paths: ModelPaths,
    lock_timeout: Duration,
}

impl AiConfigStore {
    pub(crate) fn new(paths: ModelPaths) -> Self {
        Self {
            paths,
            lock_timeout: Duration::from_secs(30),
        }
    }

    pub(crate) fn with_lock_timeout(mut self, timeout: Duration) -> Self {
        self.lock_timeout = timeout;
        self
    }

    pub(crate) fn load(&self) -> Result<AiConfig, ModelError> {
        self.paths.ensure_root()?;
        let path = self.paths.config_path();
        ensure_regular_or_missing(path)?;
        let contents = match std::fs::read_to_string(path) {
            Ok(contents) => contents,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                return Ok(AiConfig::current());
            }
            Err(error) => return Err(ModelError::io("read AI config", path, error)),
        };
        let config: AiConfig = toml::from_str(&contents)
            .map_err(|error| ModelError::serialization("AI TOML config", error))?;
        config.validate()?;
        Ok(config)
    }

    pub(crate) fn active_model_id(&self) -> Result<Option<String>, ModelError> {
        Ok(self.load()?.model.active)
    }

    pub(crate) async fn set_active_model(&self, model_id: &str) -> Result<(), ModelError> {
        validate_path_component(model_id)?;
        self.paths.ensure_root()?;
        let lock_path = self.paths.root().join(".config.lock");
        let _lock = FileLock::acquire(&lock_path, self.lock_timeout).await?;
        let mut config = self.load()?;
        config.model.active = Some(model_id.to_string());
        config.version = AI_CONFIG_VERSION;
        let encoded = toml::to_string_pretty(&config)
            .map_err(|error| ModelError::serialization("AI TOML config", error))?;
        write_atomic(self.paths.config_path(), encoded.as_bytes(), Some(0o600))
    }
}
