use serde_json::{Map, Value};
use sha2::{Digest, Sha256};
use std::collections::BTreeSet;
use std::sync::{Arc, Mutex};

const MAX_SAFE_TEXT_BYTES: usize = 512;
const REDACTED: &str = "[REDACTED]";

/// Policy for the optional diagnostic detail facility. Normal session
/// records never retain the detail passed to it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct DebugDetailPolicy {
    pub(crate) enabled: bool,
    pub(crate) max_bytes: usize,
}

impl Default for DebugDetailPolicy {
    fn default() -> Self {
        Self {
            enabled: false,
            max_bytes: 64 * 1024,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum RedactionError {
    Disabled,
    TooLarge,
    InvalidDiagnosticId,
    Poisoned,
}

/// Explicit opt-in storage for native diagnostics. It is deliberately not a
/// field of `AttemptLog` or `TraceEvent`, so the normal JSON contract cannot
/// accidentally serialize prompt bodies or native output.
#[derive(Debug, Clone)]
pub(crate) struct DebugDetailStore {
    policy: DebugDetailPolicy,
    details: Arc<Mutex<std::collections::BTreeMap<String, String>>>,
}

impl DebugDetailStore {
    pub(crate) fn new(policy: DebugDetailPolicy) -> Self {
        Self {
            policy,
            details: Arc::new(Mutex::new(std::collections::BTreeMap::new())),
        }
    }

    pub(crate) fn record(
        &self,
        diagnostic_id: &str,
        detail: impl Into<String>,
    ) -> Result<(), RedactionError> {
        if !self.policy.enabled {
            return Err(RedactionError::Disabled);
        }
        if !is_safe_identifier(diagnostic_id) {
            return Err(RedactionError::InvalidDiagnosticId);
        }
        let detail = detail.into();
        if detail.len() > self.policy.max_bytes {
            return Err(RedactionError::TooLarge);
        }
        self.details
            .lock()
            .map_err(|_| RedactionError::Poisoned)?
            .insert(diagnostic_id.to_string(), detail);
        Ok(())
    }

    pub(crate) fn get(&self, diagnostic_id: &str) -> Result<Option<String>, RedactionError> {
        self.details
            .lock()
            .map_err(|_| RedactionError::Poisoned)
            .map(|details| details.get(diagnostic_id).cloned())
    }
}

/// Redact a JSON value before it is hashed, persisted, or exposed to a
/// frontend. Object keys are compared case-insensitively and all strings are
/// passed through the small text redactor as a second line of defence.
pub(crate) fn redact_json(value: &Value) -> Value {
    redact_json_at(value, None)
}

fn redact_json_at(value: &Value, key: Option<&str>) -> Value {
    match value {
        Value::Object(object) => {
            let mut redacted = Map::new();
            for (name, value) in object {
                let normalized = normalize_key(name);
                let value = if is_secret_key(&normalized) {
                    Value::String(REDACTED.to_string())
                } else if is_path_key(&normalized) {
                    redact_path_value(value)
                } else {
                    redact_json_at(value, Some(&normalized))
                };
                redacted.insert(name.clone(), value);
            }
            Value::Object(redacted)
        }
        Value::Array(values) => Value::Array(
            values
                .iter()
                .map(|value| redact_json_at(value, key))
                .collect(),
        ),
        Value::String(text) => {
            if key.is_some_and(is_path_key) {
                redact_path_value(value)
            } else if key.is_some_and(is_secret_key) {
                Value::String(REDACTED.to_string())
            } else {
                Value::String(redact_text(text))
            }
        }
        _ => value.clone(),
    }
}

/// Canonical JSON used by fingerprints and content hashes. It sorts object
/// keys, trims strings, normalizes RFC 3339 timestamps, and sorts only fields
/// that are explicitly set-like. Ordered arrays retain their order.
pub(crate) fn canonical_json(value: &Value) -> Value {
    canonical_json_at(&redact_json(value), None)
}

fn canonical_json_at(value: &Value, key: Option<&str>) -> Value {
    match value {
        Value::Object(object) => {
            let mut names = object.keys().cloned().collect::<Vec<_>>();
            names.sort();
            let mut canonical = Map::new();
            for name in names {
                let Some(value) = object.get(&name) else {
                    continue;
                };
                // Nulls are omitted because they represent an unspecified
                // optional argument in the canonical request contract.
                if value.is_null() {
                    continue;
                }
                canonical.insert(
                    name.clone(),
                    canonical_json_at(value, Some(&normalize_key(&name))),
                );
            }
            Value::Object(canonical)
        }
        Value::Array(values) => {
            let mut values = values
                .iter()
                .map(|value| canonical_json_at(value, key))
                .collect::<Vec<_>>();
            if key.is_some_and(is_set_like_key) {
                values.sort_by(|left, right| canonical_bytes(left).cmp(&canonical_bytes(right)));
                values.dedup_by(|left, right| canonical_bytes(left) == canonical_bytes(right));
            }
            Value::Array(values)
        }
        Value::String(text) => Value::String(normalize_string(key, text)),
        _ => value.clone(),
    }
}

pub(crate) fn canonical_bytes(value: &Value) -> Vec<u8> {
    serde_json::to_vec(&canonical_json(value)).unwrap_or_else(|_| b"null".to_vec())
}

pub(crate) fn hash_bytes(prefix: &str, bytes: &[u8]) -> String {
    format!("{prefix}{:x}", Sha256::digest(bytes))
}

pub(crate) fn safe_diagnostic_id(error_code: &str, fingerprint: &str) -> String {
    let mut input = Vec::with_capacity(error_code.len() + fingerprint.len() + 1);
    input.extend_from_slice(error_code.as_bytes());
    input.push(b'\n');
    input.extend_from_slice(fingerprint.as_bytes());
    hash_bytes("diag-", &input)
}

pub(crate) fn redact_text(value: &str) -> String {
    let mut result = value.to_string();
    for marker in [
        "password=",
        "secret=",
        "token=",
        "api_key=",
        "apikey=",
        "storage_key=",
        "authorization=",
        "bearer ",
    ] {
        let mut offset = 0;
        while let Some(relative) = result.to_ascii_lowercase()[offset..].find(marker) {
            let start = offset + relative;
            let value_start = start + marker.len();
            let end = result[value_start..]
                .char_indices()
                .find_map(|(index, character)| {
                    (character.is_whitespace() || matches!(character, ',' | ';' | '}' | ']'))
                        .then_some(value_start + index)
                })
                .unwrap_or(result.len());
            result.replace_range(value_start..end, REDACTED);
            offset = value_start + REDACTED.len();
        }
    }

    let lowercase = result.to_ascii_lowercase();
    if [
        "\"password\"",
        "'password'",
        "password:",
        "\"secret\"",
        "'secret'",
        "secret:",
        "\"token\"",
        "'token'",
        "token:",
        "\"api_key\"",
        "'api_key'",
        "api_key:",
        "\"storage_key\"",
        "'storage_key'",
        "storage_key:",
        "\"authorization\"",
        "'authorization'",
        "authorization:",
        "\"prompt_body\"",
        "'prompt_body'",
        "\"prompt_content\"",
        "'prompt_content'",
        "\"message_content\"",
        "'message_content'",
        "\"full_message\"",
        "'full_message'",
        "\"native_diagnostic\"",
        "'native_diagnostic'",
        "\"native_log\"",
        "'native_log'",
        "\"hidden_reasoning\"",
        "'hidden_reasoning'",
        "\"chain_of_thought\"",
        "'chain_of_thought'",
    ]
    .iter()
    .any(|marker| lowercase.contains(marker))
        || lowercase.contains("chain-of-thought")
        || lowercase.contains("hidden reasoning")
        || lowercase.contains("native diagnostic")
        || lowercase.contains("native log")
        || lowercase.contains("credential")
    {
        return REDACTED.to_string();
    }

    let sanitized = result
        .chars()
        .filter(|character| !character.is_control() || matches!(character, '\n' | '\t'))
        .collect::<String>();
    truncate_text(&sanitized, MAX_SAFE_TEXT_BYTES)
}

pub(crate) fn truncate_text(value: &str, max_bytes: usize) -> String {
    if value.len() <= max_bytes {
        return value.to_string();
    }
    let suffix = "…[truncated]";
    if max_bytes <= suffix.len() {
        let mut end = max_bytes;
        while end > 0 && !value.is_char_boundary(end) {
            end -= 1;
        }
        return value[..end].to_string();
    }
    let mut end = max_bytes - suffix.len();
    while end > 0 && !value.is_char_boundary(end) {
        end -= 1;
    }
    format!("{}{suffix}", &value[..end])
}

pub(crate) fn is_safe_identifier(value: &str) -> bool {
    !value.is_empty()
        && value.len() <= 128
        && value != "."
        && value != ".."
        && value.chars().all(|character| {
            character.is_ascii_alphanumeric() || matches!(character, '-' | '_' | '.')
        })
}

fn normalize_key(value: &str) -> String {
    value.to_ascii_lowercase().replace('-', "_")
}

fn redact_path_value(value: &Value) -> Value {
    match value {
        Value::String(path) => {
            let normalized = path.replace('\\', "/").trim_start_matches("./").to_string();
            Value::String(hash_bytes("path:sha256:", normalized.as_bytes()))
        }
        Value::Array(paths) => Value::Array(paths.iter().map(redact_path_value).collect()),
        _ => Value::String(hash_bytes("path:sha256:", value.to_string().as_bytes())),
    }
}

fn is_secret_key(value: &str) -> bool {
    [
        "password",
        "secret",
        "credential",
        "token",
        "access_token",
        "refresh_token",
        "session_token",
        "api_key",
        "apikey",
        "access_key",
        "storage_key",
        "encryption_key",
        "signing_key",
        "private_key",
        "authorization",
        "prompt",
        "prompt_body",
        "prompt_content",
        "message_content",
        "full_message",
        "hidden_reasoning",
        "chain_of_thought",
        "native_diagnostic",
        "native_log",
        "raw_output",
        "scratchpad",
    ]
    .iter()
    .any(|name| value == *name || value.ends_with(&format!("_{name}")))
        || value.contains("credential")
        || value.contains("reasoning")
}

fn is_path_key(value: &str) -> bool {
    matches!(
        value,
        "path" | "lookup_path" | "target_path" | "file_path" | "paths"
    )
}

fn is_case_insensitive_key(value: &str) -> bool {
    matches!(
        value,
        "action"
            | "action_type"
            | "operation"
            | "tool"
            | "kind"
            | "storage"
            | "extension"
            | "extensions"
            | "content_type"
            | "status"
            | "scope"
    )
}

fn is_set_like_key(value: &str) -> bool {
    value.ends_with("_set")
        || matches!(
            value,
            "tags" | "extensions" | "entry_ids" | "include" | "exclude" | "ignore"
        )
}

fn normalize_string(key: Option<&str>, value: &str) -> String {
    let value = value.trim();
    let key = key.map(normalize_key);
    if key.as_deref().is_some_and(is_case_insensitive_key) {
        return value.to_ascii_lowercase();
    }
    if key.as_deref().is_some_and(is_path_key) {
        return value
            .replace('\\', "/")
            .trim_start_matches("./")
            .to_string();
    }
    if let Ok(timestamp) = chrono::DateTime::parse_from_rfc3339(value) {
        return timestamp
            .with_timezone(&chrono::Utc)
            .to_rfc3339_opts(chrono::SecondsFormat::Millis, true);
    }
    value.to_string()
}

#[allow(dead_code)]
fn _keep_set_type_available(_: BTreeSet<String>) {}
