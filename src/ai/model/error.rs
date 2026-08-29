use std::fmt;
use std::io;
use std::path::PathBuf;

#[derive(Debug)]
pub(crate) enum ModelError {
    MissingHomeDirectory,
    InvalidModelId(String),
    InvalidManifest(String),
    UnknownModel(String),
    ManifestIntegrityMissing {
        model_id: String,
        missing: Vec<&'static str>,
    },
    InvalidUrl(String),
    Io {
        operation: &'static str,
        path: PathBuf,
        source: io::Error,
    },
    Serialization {
        format: &'static str,
        message: String,
    },
    LockTimeout(PathBuf),
    LockLost(PathBuf),
    Http {
        url: String,
        message: String,
    },
    UnexpectedStatus {
        url: String,
        status: u16,
    },
    InvalidContentRange {
        header: String,
        expected_start: u64,
    },
    RangeNotSatisfiable {
        current: u64,
        expected: u64,
    },
    DownloadCancelled,
    DownloadInterrupted(String),
    SizeMismatch {
        expected: u64,
        actual: u64,
    },
    NotInstalled(String),
    MetadataMismatch(String),
    UnsafePath(PathBuf),
    ActiveModel(String),
    InvalidRuntime(String),
}

impl ModelError {
    pub(crate) fn io(operation: &'static str, path: impl Into<PathBuf>, source: io::Error) -> Self {
        Self::Io {
            operation,
            path: path.into(),
            source,
        }
    }

    pub(crate) fn serialization(format: &'static str, error: impl fmt::Display) -> Self {
        Self::Serialization {
            format,
            message: error.to_string(),
        }
    }
}

impl fmt::Display for ModelError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::MissingHomeDirectory => {
                write!(formatter, "Failed to determine the user home directory")
            }
            Self::InvalidModelId(id) => write!(formatter, "Invalid AI model identifier '{}'", id),
            Self::InvalidManifest(message) => {
                write!(formatter, "Invalid AI model manifest: {message}")
            }
            Self::UnknownModel(id) => write!(formatter, "AI model '{}' is not registered", id),
            Self::ManifestIntegrityMissing { model_id, missing } => write!(
                formatter,
                "AI model '{}' cannot be installed because its manifest is missing verified {}",
                model_id,
                missing.join(" and ")
            ),
            Self::InvalidUrl(url) => write!(formatter, "AI model URL is not allowed: {}", url),
            Self::Io {
                operation,
                path,
                source,
            } => write!(
                formatter,
                "Failed to {} '{}': {}",
                operation,
                path.display(),
                source
            ),
            Self::Serialization { format, message } => {
                write!(
                    formatter,
                    "Failed to serialize or parse {}: {}",
                    format, message
                )
            }
            Self::LockTimeout(path) => write!(
                formatter,
                "Timed out waiting for the AI model lock '{}'",
                path.display()
            ),
            Self::LockLost(path) => write!(
                formatter,
                "The AI model lock '{}' was lost while the operation was in progress",
                path.display()
            ),
            Self::Http { url, message } => {
                write!(
                    formatter,
                    "AI model download request to '{}' failed: {}",
                    url, message
                )
            }
            Self::UnexpectedStatus { url, status } => write!(
                formatter,
                "AI model download request to '{}' returned HTTP status {}",
                url, status
            ),
            Self::InvalidContentRange {
                header,
                expected_start,
            } => write!(
                formatter,
                "Invalid Content-Range '{}' for a resume starting at byte {}",
                header, expected_start
            ),
            Self::RangeNotSatisfiable { current, expected } => write!(
                formatter,
                "The partial AI model download is {} bytes but the manifest expects {} bytes",
                current, expected
            ),
            Self::DownloadCancelled => write!(
                formatter,
                "AI model download was cancelled; the partial download was preserved"
            ),
            Self::DownloadInterrupted(message) => {
                write!(formatter, "AI model download was interrupted: {}", message)
            }
            Self::SizeMismatch { expected, actual } => write!(
                formatter,
                "AI model size mismatch: expected {} bytes, received {} bytes",
                expected, actual
            ),
            Self::NotInstalled(id) => write!(formatter, "AI model '{}' is not installed", id),
            Self::MetadataMismatch(message) => {
                write!(
                    formatter,
                    "Installed AI model metadata is invalid: {}",
                    message
                )
            }
            Self::UnsafePath(path) => write!(
                formatter,
                "Refusing to use an unsafe AI model path '{}'",
                path.display()
            ),
            Self::ActiveModel(message) => write!(formatter, "Invalid active AI model: {}", message),
            Self::InvalidRuntime(message) => {
                write!(formatter, "Invalid AI runtime configuration: {}", message)
            }
        }
    }
}

impl std::error::Error for ModelError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Io { source, .. } => Some(source),
            _ => None,
        }
    }
}
