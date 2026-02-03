mod local;
mod s3;
mod storage_client;

use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, HashMap};
use std::sync::Arc;

pub use storage_client::ClientStorage;

pub type StorageFields = HashMap<String, String>;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StorageConfig {
    pub storage_type: String,
    pub fields: StorageFields,
}

pub struct StorageField {
    pub key: &'static str,
    pub arg_name: &'static str,
    pub value_name: &'static str,
    pub short: Option<char>,
    pub help: &'static str,
    pub prompt: &'static str,
    pub required: bool,
    pub secret: bool,
    pub default_value: Option<fn(&StorageFields) -> String>,
}

pub struct StorageDefinition {
    pub id: &'static str,
    pub label: &'static str,
    pub legacy_type: Option<u8>,
    pub fields: &'static [StorageField],
    pub build_client: fn(&StorageFields) -> Result<Arc<dyn ClientStorage>, String>,
    pub prepare: Option<fn(&mut StorageConfig) -> Result<(), String>>,
}

#[derive(Debug, Deserialize)]
struct LegacyStorage {
    storage_type: u8,
    path: Option<String>,
    region: Option<String>,
    bucket: Option<String>,
    access_key: Option<String>,
    secret_key: Option<String>,
    endpoint: Option<String>,
}

const STORAGE_DEFINITIONS: &[StorageDefinition] = &[local::DEFINITION, s3::DEFINITION];

pub fn storage_definitions() -> &'static [StorageDefinition] {
    STORAGE_DEFINITIONS
}

pub fn storage_definition(storage_type: &str) -> Option<&'static StorageDefinition> {
    STORAGE_DEFINITIONS
        .iter()
        .find(|definition| definition.id == storage_type)
}

fn storage_definition_by_legacy_type(legacy_type: u8) -> Option<&'static StorageDefinition> {
    STORAGE_DEFINITIONS
        .iter()
        .find(|definition| definition.legacy_type == Some(legacy_type))
}

pub fn storage_type_values() -> Vec<&'static str> {
    STORAGE_DEFINITIONS
        .iter()
        .map(|definition| definition.id)
        .collect()
}

pub fn storage_add_fields() -> Vec<&'static StorageField> {
    let mut fields: Vec<&'static StorageField> = Vec::new();

    for definition in STORAGE_DEFINITIONS {
        for field in definition.fields {
            if fields
                .iter()
                .any(|existing| existing.arg_name == field.arg_name)
            {
                continue;
            }
            fields.push(field);
        }
    }

    fields
}

pub fn build_storage_client(storage: &StorageConfig) -> Result<Arc<dyn ClientStorage>, String> {
    let definition = storage_definition(&storage.storage_type)
        .ok_or_else(|| format!("Unknown storage type '{}'", storage.storage_type))?;

    (definition.build_client)(&storage.fields)
}

pub fn storage_details(storage: &StorageConfig) -> String {
    let Some(definition) = storage_definition(&storage.storage_type) else {
        return "unknown".to_string();
    };

    let mut parts = Vec::new();
    for field in definition.fields {
        if let Some(value) = storage.fields.get(field.key) {
            let display_value = if field.secret {
                "********".to_string()
            } else {
                value.clone()
            };
            parts.push(format!("{}: {}", field.key, display_value));
        }
    }

    if parts.is_empty() {
        "no details".to_string()
    } else {
        parts.join(", ")
    }
}

pub fn public_fields(storage: &StorageConfig) -> BTreeMap<String, String> {
    let mut fields = BTreeMap::new();

    if let Some(definition) = storage_definition(&storage.storage_type) {
        for field in definition.fields {
            if field.secret {
                continue;
            }
            if let Some(value) = storage.fields.get(field.key) {
                fields.insert(field.key.to_string(), value.clone());
            }
        }
        return fields;
    }

    for (key, value) in &storage.fields {
        fields.insert(key.clone(), value.clone());
    }

    fields
}

pub fn parse_storage_config(bytes: &[u8]) -> Result<StorageConfig, String> {
    if let Ok(storage) = rmp_serde::from_slice::<StorageConfig>(bytes) {
        return Ok(storage);
    }

    let legacy = rmp_serde::from_slice::<LegacyStorage>(bytes)
        .map_err(|e| format!("Failed to parse storage: {}", e))?;

    let definition = storage_definition_by_legacy_type(legacy.storage_type)
        .ok_or_else(|| format!("Unknown storage type '{}'", legacy.storage_type))?;

    let mut fields = StorageFields::new();
    if let Some(path) = legacy.path {
        fields.insert("path".to_string(), path);
    }
    if let Some(region) = legacy.region {
        fields.insert("region".to_string(), region);
    }
    if let Some(bucket) = legacy.bucket {
        fields.insert("bucket".to_string(), bucket);
    }
    if let Some(access_key) = legacy.access_key {
        fields.insert("access_key".to_string(), access_key);
    }
    if let Some(secret_key) = legacy.secret_key {
        fields.insert("secret_key".to_string(), secret_key);
    }
    if let Some(endpoint) = legacy.endpoint {
        fields.insert("endpoint".to_string(), endpoint);
    }

    Ok(StorageConfig {
        storage_type: definition.id.to_string(),
        fields,
    })
}
