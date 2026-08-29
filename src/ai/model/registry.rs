use super::error::ModelError;
use super::paths::validate_path_component;
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use url::Url;

pub(crate) const MODEL_MANIFEST_SCHEMA_VERSION: u32 = 1;
pub(crate) const MODEL_REGISTRY_VERSION: u32 = 1;
pub(crate) const DEFAULT_MODEL_ID: &str = "qwen3.5-4b-q8-0";
pub(crate) const DEFAULT_MODEL_URL: &str =
    "https://public.trygib.org/ai/models/Qwen3.5-4B-Q8_0.gguf";

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub(crate) struct ModelManifest {
    pub(crate) schema_version: u32,
    pub(crate) id: String,
    pub(crate) display_name: String,
    pub(crate) version: String,
    pub(crate) family: String,
    pub(crate) parameter_count: Option<String>,
    pub(crate) quantization: String,
    pub(crate) format: String,
    pub(crate) intended_use: String,
    pub(crate) download_url: String,
    pub(crate) source: String,
    pub(crate) license: String,
    pub(crate) license_url: Option<String>,
    // Retained for manifest compatibility and provenance; installation uses
    // the expected byte size as its artifact check.
    pub(crate) sha256: Option<String>,
    pub(crate) expected_size: Option<u64>,
    pub(crate) min_ram_bytes: Option<u64>,
    #[serde(default)]
    pub(crate) runtime_features: Vec<String>,
}

impl ModelManifest {
    pub(crate) fn validate(&self) -> Result<(), ModelError> {
        if self.schema_version != MODEL_MANIFEST_SCHEMA_VERSION {
            return Err(ModelError::InvalidManifest(format!(
                "unsupported schema version {}",
                self.schema_version
            )));
        }
        validate_path_component(&self.id)?;
        validate_path_component(&self.version)?;
        for (label, value) in [
            ("display name", self.display_name.as_str()),
            ("family", self.family.as_str()),
            ("quantization", self.quantization.as_str()),
            ("format", self.format.as_str()),
            ("intended use", self.intended_use.as_str()),
            ("source", self.source.as_str()),
            ("license", self.license.as_str()),
        ] {
            if value.trim().is_empty() {
                return Err(ModelError::InvalidManifest(format!(
                    "{} cannot be empty",
                    label
                )));
            }
        }

        if self
            .parameter_count
            .as_deref()
            .is_some_and(|value| value.trim().is_empty())
        {
            return Err(ModelError::InvalidManifest(
                "parameter_count cannot be empty when present".to_string(),
            ));
        }
        if self
            .runtime_features
            .iter()
            .any(|feature| feature.trim().is_empty())
        {
            return Err(ModelError::InvalidManifest(
                "runtime_features cannot contain empty values".to_string(),
            ));
        }

        let url = Url::parse(&self.download_url).map_err(|error| {
            ModelError::InvalidUrl(format!("{} ({})", self.download_url, error))
        })?;
        let allowed_http = matches!(url.scheme(), "http")
            && url
                .host_str()
                .is_some_and(|host| matches!(host, "localhost" | "127.0.0.1" | "::1"));
        if url.scheme() != "https" && !allowed_http {
            return Err(ModelError::InvalidUrl(self.download_url.clone()));
        }
        if url.host_str().is_none() {
            return Err(ModelError::InvalidUrl(self.download_url.clone()));
        }

        if let Some(license_url) = &self.license_url {
            let license_url = Url::parse(license_url).map_err(|error| {
                ModelError::InvalidManifest(format!("license_url is invalid ({error})"))
            })?;
            if license_url.scheme() != "https" || license_url.host_str().is_none() {
                return Err(ModelError::InvalidManifest(
                    "license_url must be an HTTPS URL".to_string(),
                ));
            }
        }

        if self.expected_size.is_none() {
            return Err(ModelError::InvalidManifest(
                "expected_size is required for model installation".to_string(),
            ));
        }
        if self.expected_size == Some(0) {
            return Err(ModelError::InvalidManifest(
                "expected_size must be greater than zero".to_string(),
            ));
        }
        Ok(())
    }

    pub(crate) fn require_size(&self) -> Result<u64, ModelError> {
        match self.expected_size {
            Some(size) if size > 0 => Ok(size),
            Some(_) => Err(ModelError::InvalidManifest(
                "expected_size must be greater than zero".to_string(),
            )),
            None => Err(ModelError::ManifestIntegrityMissing {
                model_id: self.id.clone(),
                missing: vec!["expected size"],
            }),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub(crate) struct ModelRegistry {
    pub(crate) schema_version: u32,
    pub(crate) models: BTreeMap<String, ModelManifest>,
}

impl Default for ModelRegistry {
    fn default() -> Self {
        Self::built_in()
    }
}

impl ModelRegistry {
    pub(crate) fn built_in() -> Self {
        let manifest = ModelManifest {
            schema_version: MODEL_MANIFEST_SCHEMA_VERSION,
            id: DEFAULT_MODEL_ID.to_string(),
            display_name: "Qwen3.5 4B Q8_0".to_string(),
            version: "v1".to_string(),
            family: "Qwen3.5".to_string(),
            parameter_count: Some("4B".to_string()),
            quantization: "Q8_0".to_string(),
            format: "GGUF".to_string(),
            intended_use: "Local GIB assistant inference".to_string(),
            download_url: DEFAULT_MODEL_URL.to_string(),
            source: "GIB public model bucket".to_string(),
            license: "Apache-2.0".to_string(),
            license_url: Some("https://www.apache.org/licenses/LICENSE-2.0".to_string()),
            sha256: Some(
                "c3fc7bcaf6f75b8f7ceeead9a769f5a7a9f86a8180af1cfb2b72958dcad8e028".to_string(),
            ),
            expected_size: Some(4_482_402_656),
            min_ram_bytes: Some(8 * 1024 * 1024 * 1024),
            runtime_features: vec!["text-generation".to_string(), "chat".to_string()],
        };
        let mut models = BTreeMap::new();
        models.insert(manifest.id.clone(), manifest);
        Self {
            schema_version: MODEL_REGISTRY_VERSION,
            models,
        }
    }

    pub(crate) fn from_models(
        models: impl IntoIterator<Item = ModelManifest>,
    ) -> Result<Self, ModelError> {
        let mut entries = BTreeMap::new();
        for manifest in models {
            manifest.validate()?;
            if entries.insert(manifest.id.clone(), manifest).is_some() {
                return Err(ModelError::InvalidManifest(
                    "duplicate model identifier".to_string(),
                ));
            }
        }
        Ok(Self {
            schema_version: MODEL_REGISTRY_VERSION,
            models: entries,
        })
    }

    pub(crate) fn validate(&self) -> Result<(), ModelError> {
        if self.schema_version != MODEL_REGISTRY_VERSION {
            return Err(ModelError::InvalidManifest(format!(
                "unsupported registry version {}",
                self.schema_version
            )));
        }
        for (id, manifest) in &self.models {
            manifest.validate()?;
            if id != &manifest.id {
                return Err(ModelError::InvalidManifest(format!(
                    "registry key '{}' does not match manifest ID '{}'",
                    id, manifest.id
                )));
            }
        }
        Ok(())
    }

    pub(crate) fn get(&self, id: &str) -> Result<&ModelManifest, ModelError> {
        self.models
            .get(id)
            .ok_or_else(|| ModelError::UnknownModel(id.to_string()))
    }

    pub(crate) fn iter(&self) -> impl Iterator<Item = &ModelManifest> {
        self.models.values()
    }
}
