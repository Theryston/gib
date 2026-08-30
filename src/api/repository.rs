use serde::ser::SerializeStruct;
use std::fmt;

use super::error::{ErrorCode, GibError};

/// Explicit repository selection shared by all repository operations.
#[derive(Clone, PartialEq, Eq)]
pub struct RepositoryRequest {
    pub key: String,
    pub storage: String,
    pub password: Option<String>,
}

impl RepositoryRequest {
    pub fn new(key: impl Into<String>, storage: impl Into<String>) -> Self {
        Self {
            key: key.into(),
            storage: storage.into(),
            password: None,
        }
    }

    pub fn with_password(mut self, password: impl Into<String>) -> Self {
        self.password = Some(password.into());
        self
    }

    pub(crate) fn validate(&self) -> Result<(), GibError> {
        if !is_safe_component(&self.key) || !is_safe_component(&self.storage) {
            return Err(GibError::new(
                ErrorCode::InvalidRequest,
                "Repository key and storage must be non-empty path components",
            ));
        }
        Ok(())
    }
}

fn is_safe_component(value: &str) -> bool {
    !value.trim().is_empty()
        && value != "."
        && value != ".."
        && !value.contains(['/', '\\'])
        && !value.chars().any(char::is_control)
}

impl fmt::Debug for RepositoryRequest {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("RepositoryRequest")
            .field("key", &self.key)
            .field("storage", &self.storage)
            .field("password", &self.password.as_ref().map(|_| "[redacted]"))
            .finish()
    }
}

impl serde::Serialize for RepositoryRequest {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: serde::Serializer,
    {
        let mut value = serializer.serialize_struct("RepositoryRequest", 2)?;
        value.serialize_field("key", &self.key)?;
        value.serialize_field("storage", &self.storage)?;
        value.end()
    }
}
