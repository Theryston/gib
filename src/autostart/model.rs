use crate::commands::backup::LiveOverrides;
use serde::{Deserialize, Serialize};
use std::path::PathBuf;

pub(crate) const AUTOSTART_JOB_VERSION: u32 = 1;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub(crate) struct AutostartJob {
    pub(crate) version: u32,
    pub(crate) id: String,
    pub(crate) name: String,
    pub(crate) enabled: bool,
    pub(crate) root_path: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) config_path: Option<String>,
    pub(crate) created_at: String,
    pub(crate) updated_at: String,
    #[serde(default)]
    pub(crate) overrides: LiveJobOverrides,
    #[serde(default)]
    pub(crate) secrets: SecretReferences,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub(crate) struct LiveJobOverrides {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) storage: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) key: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) message: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) compress: Option<i32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) chunk_size: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) concurrency: Option<usize>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) ignore: Option<Vec<String>>,
    #[serde(default)]
    pub(crate) no_ignore_git: bool,
    #[serde(default = "default_conflict_policy")]
    pub(crate) conflict: String,
}

impl Default for LiveJobOverrides {
    fn default() -> Self {
        Self {
            storage: None,
            key: None,
            message: None,
            compress: None,
            chunk_size: None,
            concurrency: None,
            ignore: None,
            no_ignore_git: false,
            conflict: default_conflict_policy(),
        }
    }
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub(crate) struct SecretReferences {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) password_ref: Option<String>,
}

impl AutostartJob {
    pub(crate) fn live_overrides(&self, password: Option<String>) -> LiveOverrides {
        LiveOverrides {
            root_path: PathBuf::from(&self.root_path),
            config_path: self.config_path.as_ref().map(PathBuf::from),
            key: self.overrides.key.clone(),
            storage: self.overrides.storage.clone(),
            message: self.overrides.message.clone(),
            compress: self.overrides.compress,
            chunk_size: self.overrides.chunk_size.clone(),
            ignore: self.overrides.ignore.clone(),
            no_ignore_git: self.overrides.no_ignore_git,
            concurrency: self.overrides.concurrency,
            password,
        }
    }
}

pub(crate) fn default_conflict_policy() -> String {
    "local".to_string()
}

pub(crate) fn validate_name(name: &str) -> Result<(), String> {
    if name.is_empty() {
        return Err("The autostart job name cannot be empty".to_string());
    }
    if !name
        .chars()
        .all(|character| character.is_ascii_alphanumeric() || character == '-' || character == '_')
    {
        return Err(
            "The autostart job name can only contain ASCII letters, numbers, hyphens, and underscores"
                .to_string(),
        );
    }
    Ok(())
}

pub(crate) fn validate_job_id(id: &str) -> Result<(), String> {
    if id.is_empty()
        || !id
            .chars()
            .all(|character| character.is_ascii_alphanumeric() || character == '-')
    {
        return Err(format!("Invalid autostart job ID '{}'", id));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn validates_safe_job_names() {
        assert!(validate_name("code_sync-1").is_ok());
        assert!(validate_name("code sync").is_err());
        assert!(validate_name("../code").is_err());
    }

    #[test]
    fn serializes_without_a_password() {
        let job = AutostartJob {
            version: AUTOSTART_JOB_VERSION,
            id: "job-1".to_string(),
            name: "code".to_string(),
            enabled: true,
            root_path: "/tmp/code".to_string(),
            config_path: None,
            created_at: "now".to_string(),
            updated_at: "now".to_string(),
            overrides: LiveJobOverrides::default(),
            secrets: SecretReferences {
                password_ref: Some("gib/autostart/job-1/password".to_string()),
            },
        };
        let encoded = toml::to_string(&job).unwrap();
        assert!(!encoded.contains("password ="));
        assert!(encoded.contains("password_ref"));
    }
}
