use super::error::ModelError;
use super::lock::FileLock;
use super::paths::{ModelPaths, ensure_regular_or_missing, validate_path_component};
use super::storage::write_atomic;
use serde::{Deserialize, Serialize};
use std::time::Duration;

pub(crate) const AI_CONFIG_VERSION: u32 = 1;

fn default_ai_config_version() -> u32 {
    AI_CONFIG_VERSION
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(default, deny_unknown_fields)]
pub(crate) struct AiConfig {
    #[serde(
        default = "default_ai_config_version",
        rename = "schema_version",
        alias = "version"
    )]
    pub(crate) version: u32,
    #[serde(default)]
    pub(crate) active_conversation_id: Option<String>,
    #[serde(default)]
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
            active_conversation_id: None,
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
        if let Some(active) = &self.active_conversation_id {
            validate_path_component(active)?;
        }
        Ok(())
    }
}

impl Default for AiConfig {
    fn default() -> Self {
        Self::current()
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

    pub(crate) fn active_conversation_id(&self) -> Result<Option<String>, ModelError> {
        Ok(self.load()?.active_conversation_id)
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

    pub(crate) async fn set_active_conversation_id(
        &self,
        conversation_id: Option<&str>,
    ) -> Result<(), ModelError> {
        if let Some(conversation_id) = conversation_id {
            validate_path_component(conversation_id)?;
        }
        self.paths.ensure_root()?;
        let lock_path = self.paths.root().join(".config.lock");
        let _lock = FileLock::acquire(&lock_path, self.lock_timeout).await?;
        let mut config = self.load()?;
        config.active_conversation_id = conversation_id.map(ToString::to_string);
        config.version = AI_CONFIG_VERSION;
        let encoded = toml::to_string_pretty(&config)
            .map_err(|error| ModelError::serialization("AI TOML config", error))?;
        write_atomic(self.paths.config_path(), encoded.as_bytes(), Some(0o600))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;
    use std::time::{SystemTime, UNIX_EPOCH};

    fn temporary_root(name: &str) -> PathBuf {
        let stamp = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("system clock should be after unix epoch")
            .as_nanos();
        let root = std::env::temp_dir().join(format!(
            "gib-ai-config-{name}-{}-{stamp}",
            std::process::id()
        ));
        std::fs::create_dir_all(&root).expect("temporary root should be created");
        root
    }

    #[test]
    fn legacy_version_key_loads_and_current_serialization_uses_schema_version() {
        let root = temporary_root("migration");
        let paths = ModelPaths::from_root(root.clone());
        paths.ensure_root().expect("AI root should be created");
        std::fs::write(
            paths.config_path(),
            "version = 1\n[model]\nactive = \"qwen3.5-4b-q8-0\"\n",
        )
        .expect("legacy config should be written");

        let config = AiConfigStore::new(paths.clone())
            .load()
            .expect("legacy config should load");
        assert_eq!(config.version, AI_CONFIG_VERSION);
        assert_eq!(config.active_conversation_id, None);
        let encoded = toml::to_string_pretty(&config).expect("config should serialize");
        assert!(encoded.contains("schema_version = 1"));
        assert!(!encoded.contains("\nversion = 1"));

        std::fs::remove_dir_all(root).expect("temporary state should be removed");
    }

    #[tokio::test]
    async fn active_conversation_updates_preserve_the_active_model() {
        let root = temporary_root("active-conversation");
        let paths = ModelPaths::from_root(root.clone());
        let store = AiConfigStore::new(paths.clone());
        store
            .set_active_model("qwen3.5-4b-q8-0")
            .await
            .expect("active model should be written");
        store
            .set_active_conversation_id(Some("conv-example"))
            .await
            .expect("active conversation should be written");

        let config = store.load().expect("config should load");
        assert_eq!(config.model.active.as_deref(), Some("qwen3.5-4b-q8-0"));
        assert_eq!(
            config.active_conversation_id.as_deref(),
            Some("conv-example")
        );
        let encoded =
            std::fs::read_to_string(paths.config_path()).expect("config should be readable");
        assert!(encoded.contains("schema_version = 1"));
        assert!(encoded.contains("active_conversation_id = \"conv-example\""));

        store
            .set_active_conversation_id(None)
            .await
            .expect("active conversation should be cleared");
        assert_eq!(store.active_conversation_id().unwrap(), None);
        std::fs::remove_dir_all(root).expect("temporary state should be removed");
    }
}
