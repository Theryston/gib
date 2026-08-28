use crate::ai::model::ModelError;
use serde::{Deserialize, Serialize};
use std::fmt;

/// Errors exposed by the llama runtime without leaking native paths, prompts,
/// credentials, or raw FFI diagnostics.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(tag = "code", content = "details", rename_all = "snake_case")]
pub(crate) enum AiBackendError {
    InvalidRequest(String),
    UnknownModel {
        model_id: String,
    },
    ModelNotInstalled {
        model_id: String,
    },
    InvalidGguf {
        model_id: String,
    },
    ModelIntegrity {
        model_id: String,
    },
    ModelResolutionFailed {
        model_id: String,
    },
    ModelAlreadyLoaded {
        loaded_model_id: String,
        requested_model_id: String,
    },
    ModelNotLoaded {
        model_id: String,
    },
    ModelMismatch {
        loaded_model_id: String,
        requested_model_id: String,
    },
    ModelLoadFailed {
        model_id: String,
    },
    ContextCreationFailed {
        model_id: String,
    },
    ContextExhausted {
        context_limit: u32,
        prompt_tokens: u32,
        requested_output_tokens: u32,
    },
    GenerationFailed {
        model_id: String,
    },
    GrammarInitializationFailed,
    Cancelled,
    RequestAlreadyActive {
        request_id: String,
    },
    RequestNotFound {
        request_id: String,
    },
    BackendInitializationFailed,
    UnsupportedFeature {
        feature: String,
    },
    CapabilityUnavailable {
        capability: String,
    },
    WorkerClosed,
}

impl AiBackendError {
    pub(crate) fn from_model_error(model_id: &str, error: ModelError) -> Self {
        match error {
            ModelError::UnknownModel(_) => Self::UnknownModel {
                model_id: model_id.to_string(),
            },
            ModelError::NotInstalled(_)
            | ModelError::MetadataMismatch(_)
            | ModelError::ChecksumMismatch { .. }
            | ModelError::SizeMismatch { .. } => Self::ModelNotInstalled {
                model_id: model_id.to_string(),
            },
            ModelError::InvalidManifest(_)
            | ModelError::ManifestIntegrityMissing { .. }
            | ModelError::UnsafePath(_) => Self::ModelIntegrity {
                model_id: model_id.to_string(),
            },
            ModelError::InvalidModelId(_) => Self::InvalidRequest(
                "model_id contains characters that are not allowed by the model registry"
                    .to_string(),
            ),
            _ => Self::ModelResolutionFailed {
                model_id: model_id.to_string(),
            },
        }
    }

    pub(crate) fn load_native(model_id: &str) -> Self {
        Self::ModelLoadFailed {
            model_id: model_id.to_string(),
        }
    }

    pub(crate) fn context_native(model_id: &str) -> Self {
        Self::ContextCreationFailed {
            model_id: model_id.to_string(),
        }
    }

    pub(crate) fn code(&self) -> &'static str {
        match self {
            Self::InvalidRequest(_) => "invalid_request",
            Self::UnknownModel { .. } => "unknown_model",
            Self::ModelNotInstalled { .. } => "model_not_installed",
            Self::InvalidGguf { .. } => "invalid_gguf",
            Self::ModelIntegrity { .. } => "model_integrity",
            Self::ModelResolutionFailed { .. } => "model_resolution_failed",
            Self::ModelAlreadyLoaded { .. } => "model_already_loaded",
            Self::ModelNotLoaded { .. } => "model_not_loaded",
            Self::ModelMismatch { .. } => "model_mismatch",
            Self::ModelLoadFailed { .. } => "model_load_failed",
            Self::ContextCreationFailed { .. } => "context_creation_failed",
            Self::ContextExhausted { .. } => "context_exhausted",
            Self::GenerationFailed { .. } => "generation_failed",
            Self::GrammarInitializationFailed => "grammar_initialization_failed",
            Self::Cancelled => "cancelled",
            Self::RequestAlreadyActive { .. } => "request_already_active",
            Self::RequestNotFound { .. } => "request_not_found",
            Self::BackendInitializationFailed => "backend_initialization_failed",
            Self::UnsupportedFeature { .. } => "unsupported_feature",
            Self::CapabilityUnavailable { .. } => "capability_unavailable",
            Self::WorkerClosed => "worker_closed",
        }
    }
}

impl fmt::Display for AiBackendError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::InvalidRequest(message) => {
                write!(formatter, "Invalid AI generation request: {message}")
            }
            Self::UnknownModel { model_id } => {
                write!(formatter, "AI model '{model_id}' is not registered")
            }
            Self::ModelNotInstalled { model_id } => write!(
                formatter,
                "AI model '{model_id}' is not installed or verified"
            ),
            Self::InvalidGguf { model_id } => write!(
                formatter,
                "AI model '{model_id}' is not a valid GGUF artifact"
            ),
            Self::ModelIntegrity { model_id } => write!(
                formatter,
                "AI model '{model_id}' failed installation integrity validation"
            ),
            Self::ModelResolutionFailed { model_id } => write!(
                formatter,
                "Unable to resolve the verified installation for AI model '{model_id}'"
            ),
            Self::ModelAlreadyLoaded {
                loaded_model_id,
                requested_model_id,
            } => write!(
                formatter,
                "AI model '{loaded_model_id}' is already loaded; cannot load '{requested_model_id}'"
            ),
            Self::ModelNotLoaded { model_id } => {
                write!(formatter, "AI model '{model_id}' is not loaded")
            }
            Self::ModelMismatch {
                loaded_model_id,
                requested_model_id,
            } => write!(
                formatter,
                "AI model '{requested_model_id}' was requested while '{loaded_model_id}' is loaded"
            ),
            Self::ModelLoadFailed { model_id } => write!(
                formatter,
                "llama.cpp could not load verified AI model '{model_id}'"
            ),
            Self::ContextCreationFailed { model_id } => write!(
                formatter,
                "llama.cpp could not create a context for AI model '{model_id}'"
            ),
            Self::ContextExhausted {
                context_limit,
                prompt_tokens,
                requested_output_tokens,
            } => write!(
                formatter,
                "The AI request needs {prompt_tokens} prompt tokens plus {requested_output_tokens} output tokens, exceeding the context limit of {context_limit}"
            ),
            Self::GenerationFailed { model_id } => write!(
                formatter,
                "llama.cpp generation failed for AI model '{model_id}'"
            ),
            Self::GrammarInitializationFailed => {
                formatter.write_str("llama.cpp could not initialize the requested output grammar")
            }
            Self::Cancelled => formatter.write_str("AI generation was cancelled"),
            Self::RequestAlreadyActive { request_id } => {
                write!(formatter, "AI request '{request_id}' is already active")
            }
            Self::RequestNotFound { request_id } => {
                write!(formatter, "AI request '{request_id}' is not active")
            }
            Self::BackendInitializationFailed => {
                formatter.write_str("The llama.cpp backend could not be initialized")
            }
            Self::UnsupportedFeature { feature } => write!(
                formatter,
                "The in-process AI runtime does not support {feature}"
            ),
            Self::CapabilityUnavailable { capability } => write!(
                formatter,
                "The requested AI runtime capability is unavailable: {capability}"
            ),
            Self::WorkerClosed => {
                formatter.write_str("The in-process AI runtime worker has stopped")
            }
        }
    }
}

impl std::error::Error for AiBackendError {}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn native_errors_do_not_expose_paths_or_native_text() {
        let error = AiBackendError::load_native("qwen3.5-4b-q8-0");
        let rendered = error.to_string();
        assert!(!rendered.contains("/tmp"));
        assert!(!rendered.contains("secret"));
        assert_eq!(error.code(), "model_load_failed");
    }

    #[test]
    fn model_integrity_errors_are_safe_to_serialize() {
        let encoded = serde_json::to_string(&AiBackendError::ModelIntegrity {
            model_id: "qwen3.5-4b-q8-0".to_string(),
        })
        .expect("error should serialize");
        assert!(encoded.contains("model_integrity"));
        assert!(!encoded.contains("/home"));
    }
}
