#![allow(dead_code)]

mod api;
mod error;
mod llama;
mod options;
mod prompt;

#[cfg(test)]
pub(crate) mod fake;

#[allow(unused_imports)]
pub(crate) use api::{
    AiBackend, AiFinishReason, AiGenerationRequest, AiGenerationResult, AiGenerationStream,
    AiGrammar, AiLoadedModel, AiMessage, AiMessageRole, AiSamplingSettings, AiStreamEvent, AiUsage,
};
#[allow(unused_imports)]
pub(crate) use error::AiBackendError;
#[cfg(test)]
#[allow(unused_imports)]
pub(crate) use fake::FakeAiBackend;
#[allow(unused_imports)]
pub(crate) use llama::{AiBackendFactory, AiRuntime};
#[allow(unused_imports)]
pub(crate) use options::{AiRuntimeCapabilities, AiRuntimeOptions};
