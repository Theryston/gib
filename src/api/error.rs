use serde::Serialize;
use std::collections::BTreeMap;
use std::fmt;

/// Stable machine-readable categories returned by the public API.
#[non_exhaustive]
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ErrorCode {
    ConfigurationNotFound,
    InvalidConfiguration,
    StorageNotFound,
    InvalidStorageConfiguration,
    PasswordRequired,
    InvalidPassword,
    BackupNotFound,
    RepositoryConflict,
    CatalogDegraded,
    PermissionDenied,
    Io,
    Serialization,
    Encryption,
    InvalidRequest,
    Cancelled,
    Unsupported,
    Internal,
}

/// An error with a stable code and optional structured context.
///
/// Secret-bearing values are never accepted into the context by the library,
/// and display text is sanitized before it is stored.
#[derive(Clone, PartialEq, Eq, Serialize)]
pub struct GibError {
    pub code: ErrorCode,
    pub message: String,
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub context: BTreeMap<String, String>,
}

impl GibError {
    pub fn new(code: ErrorCode, message: impl Into<String>) -> Self {
        Self {
            code,
            message: sanitize_message(&message.into()),
            context: BTreeMap::new(),
        }
    }

    pub fn with_context(mut self, key: impl Into<String>, value: impl Into<String>) -> Self {
        let key = key.into();
        if !is_sensitive_key(&key) {
            self.context.insert(key, sanitize_message(&value.into()));
        }
        self
    }

    pub fn code(&self) -> ErrorCode {
        self.code
    }

    pub fn message(&self) -> &str {
        &self.message
    }

    pub fn context(&self) -> &BTreeMap<String, String> {
        &self.context
    }

    pub(crate) fn from_message(message: impl Into<String>) -> Self {
        let message = message.into();
        let lower = message.to_ascii_lowercase();
        let code = if lower.contains("password") && lower.contains("required") {
            ErrorCode::PasswordRequired
        } else if lower.contains("invalid password") {
            ErrorCode::InvalidPassword
        } else if lower.contains("not found") {
            ErrorCode::BackupNotFound
        } else if lower.contains("serialize") || lower.contains("deserialize") {
            ErrorCode::Serialization
        } else if lower.contains("encrypt") || lower.contains("decrypt") {
            ErrorCode::Encryption
        } else if lower.contains("permission denied") {
            ErrorCode::PermissionDenied
        } else {
            ErrorCode::Internal
        };
        Self::new(code, message)
    }
}

impl fmt::Debug for GibError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("GibError")
            .field("code", &self.code)
            .field("message", &self.message)
            .field("context", &self.context)
            .finish()
    }
}

impl fmt::Display for GibError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.message)
    }
}

impl std::error::Error for GibError {}

impl From<std::io::Error> for GibError {
    fn from(error: std::io::Error) -> Self {
        Self::new(ErrorCode::Io, error.to_string())
    }
}

pub(crate) fn map_error(message: impl Into<String>) -> GibError {
    GibError::from_message(message)
}

fn is_sensitive_key(key: &str) -> bool {
    let key = key.to_ascii_lowercase();
    [
        "password",
        "secret",
        "token",
        "credential",
        "access_key",
        "secret_key",
    ]
    .iter()
    .any(|part| key.contains(part))
}

fn sanitize_message(message: &str) -> String {
    // Error messages in the library are constructed from paths, operation
    // names, and provider errors. Replace common credential-shaped fields so
    // an accidental provider message cannot turn into a public secret leak.
    let mut sanitized = message.to_string();
    for label in ["password", "secret_key", "access_key", "token"] {
        let mut search_from = 0;
        loop {
            let lower = sanitized.to_ascii_lowercase();
            let Some(relative_start) = lower[search_from..].find(label) else {
                break;
            };
            let start = search_from + relative_start;
            let after_label = start + label.len();
            let Some(separator_offset) = sanitized[after_label..]
                .find(['=', ':'])
                .filter(|offset| *offset < 16)
            else {
                search_from = after_label;
                continue;
            };
            let mut value_start = after_label + separator_offset + 1;
            while let Some(character) = sanitized[value_start..].chars().next() {
                if !character.is_whitespace() {
                    break;
                }
                value_start += character.len_utf8();
            }
            if value_start >= sanitized.len() {
                break;
            }
            let quoted = matches!(sanitized.as_bytes()[value_start], b'\'' | b'"');
            if quoted {
                value_start += 1;
            }
            let value_end = if quoted {
                sanitized[value_start..]
                    .find(['\'', '"'])
                    .map(|offset| value_start + offset)
                    .unwrap_or(sanitized.len())
            } else {
                sanitized[value_start..]
                    .find([',', ' ', ';', ')', ']', '}'])
                    .map(|offset| value_start + offset)
                    .unwrap_or(sanitized.len())
            };
            sanitized.replace_range(value_start..value_end, "[redacted]");
            search_from = value_start + "[redacted]".len();
        }
    }
    sanitized
}
