use super::error::AiBackendError;
use async_trait::async_trait;
use futures::Stream;
use serde::{Deserialize, Serialize};
use std::pin::Pin;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::task::{Context, Poll};
use tokio::sync::mpsc;

/// A role that can participate in a model prompt.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub(crate) enum AiMessageRole {
    System,
    Developer,
    User,
    Assistant,
}

impl AiMessageRole {
    pub(crate) fn as_str(self) -> &'static str {
        match self {
            Self::System => "system",
            Self::Developer => "developer",
            Self::User => "user",
            Self::Assistant => "assistant",
        }
    }
}

/// A single typed message supplied to the prompt renderer.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub(crate) struct AiMessage {
    pub(crate) role: AiMessageRole,
    pub(crate) content: String,
}

impl AiMessage {
    pub(crate) fn new(role: AiMessageRole, content: impl Into<String>) -> Self {
        Self {
            role,
            content: content.into(),
        }
    }
}

/// Sampling values accepted by the runtime boundary.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub(crate) struct AiSamplingSettings {
    pub(crate) temperature: f32,
    pub(crate) top_k: u32,
    pub(crate) top_p: f32,
    pub(crate) min_p: f32,
    pub(crate) seed: Option<u32>,
}

impl Default for AiSamplingSettings {
    fn default() -> Self {
        Self {
            temperature: 0.7,
            top_k: 40,
            top_p: 0.95,
            min_p: 0.05,
            seed: None,
        }
    }
}

/// A grammar payload reserved for the structured-generation layer.
///
/// Task 02 keeps the field at the backend boundary so callers do not need a
/// breaking API change when grammar-constrained generation is added. The
/// initial llama backend deliberately rejects it; Task 03 owns its semantics.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub(crate) struct AiGrammar {
    pub(crate) grammar: String,
    pub(crate) root: String,
}

/// A complete, bounded generation request.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub(crate) struct AiGenerationRequest {
    pub(crate) request_id: String,
    pub(crate) model_id: String,
    pub(crate) messages: Vec<AiMessage>,
    pub(crate) sampling: AiSamplingSettings,
    /// The maximum prompt-plus-output context requested for this turn.
    pub(crate) context_limit: u32,
    pub(crate) max_output_tokens: u32,
    pub(crate) stop_sequences: Vec<String>,
    pub(crate) grammar: Option<AiGrammar>,
}

impl AiGenerationRequest {
    pub(crate) fn new(
        request_id: impl Into<String>,
        model_id: impl Into<String>,
        user_message: impl Into<String>,
    ) -> Self {
        Self {
            request_id: request_id.into(),
            model_id: model_id.into(),
            messages: vec![AiMessage::new(AiMessageRole::User, user_message)],
            sampling: AiSamplingSettings::default(),
            context_limit: 4096,
            max_output_tokens: 256,
            stop_sequences: Vec::new(),
            grammar: None,
        }
    }

    pub(crate) fn validate(&self) -> Result<(), AiBackendError> {
        if self.request_id.trim().is_empty() {
            return Err(AiBackendError::InvalidRequest(
                "request_id cannot be empty".to_string(),
            ));
        }
        if self.model_id.trim().is_empty() {
            return Err(AiBackendError::InvalidRequest(
                "model_id cannot be empty".to_string(),
            ));
        }
        if self.messages.is_empty() {
            return Err(AiBackendError::InvalidRequest(
                "at least one prompt message is required".to_string(),
            ));
        }
        if !self
            .messages
            .iter()
            .any(|message| message.role == AiMessageRole::User)
        {
            return Err(AiBackendError::InvalidRequest(
                "at least one user message is required".to_string(),
            ));
        }
        if self
            .messages
            .iter()
            .any(|message| message.content.contains('\0'))
        {
            return Err(AiBackendError::InvalidRequest(
                "prompt messages cannot contain NUL bytes".to_string(),
            ));
        }
        if self.context_limit == 0 {
            return Err(AiBackendError::InvalidRequest(
                "context_limit must be greater than zero".to_string(),
            ));
        }
        if self.max_output_tokens == 0 {
            return Err(AiBackendError::InvalidRequest(
                "max_output_tokens must be greater than zero".to_string(),
            ));
        }
        if !self.sampling.temperature.is_finite() || self.sampling.temperature < 0.0 {
            return Err(AiBackendError::InvalidRequest(
                "temperature must be a finite non-negative number".to_string(),
            ));
        }
        if !self.sampling.top_p.is_finite()
            || !(0.0..=1.0).contains(&self.sampling.top_p)
            || self.sampling.top_p == 0.0
        {
            return Err(AiBackendError::InvalidRequest(
                "top_p must be greater than zero and at most one".to_string(),
            ));
        }
        if !self.sampling.min_p.is_finite() || !(0.0..=1.0).contains(&self.sampling.min_p) {
            return Err(AiBackendError::InvalidRequest(
                "min_p must be between zero and one".to_string(),
            ));
        }
        for stop in &self.stop_sequences {
            if stop.is_empty() || stop.contains('\0') {
                return Err(AiBackendError::InvalidRequest(
                    "stop sequences cannot be empty or contain NUL bytes".to_string(),
                ));
            }
        }
        if let Some(grammar) = &self.grammar
            && (grammar.grammar.is_empty()
                || grammar.root.is_empty()
                || grammar.grammar.contains('\0')
                || grammar.root.contains('\0'))
        {
            return Err(AiBackendError::InvalidRequest(
                "grammar and grammar root must be non-empty and contain no NUL bytes".to_string(),
            ));
        }
        Ok(())
    }
}

/// Why a generation ended.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub(crate) enum AiFinishReason {
    EndOfGeneration,
    StopSequence,
    MaxOutputTokens,
}

/// Safe, aggregate token accounting for a generation.
#[derive(Debug, Clone, Copy, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub(crate) struct AiUsage {
    pub(crate) prompt_tokens: u32,
    pub(crate) completion_tokens: u32,
    pub(crate) total_tokens: u32,
}

/// The terminal result of a successful generation.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub(crate) struct AiGenerationResult {
    pub(crate) request_id: String,
    pub(crate) model_id: String,
    pub(crate) text: String,
    pub(crate) finish_reason: AiFinishReason,
    pub(crate) usage: AiUsage,
    /// Wall-clock duration rounded to milliseconds; no host paths or prompts
    /// are included in timing data.
    pub(crate) duration_ms: u64,
}

/// Information returned after a model is loaded or reused.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub(crate) struct AiLoadedModel {
    pub(crate) model_id: String,
    pub(crate) version: String,
    pub(crate) context_size: u32,
    pub(crate) gpu_layers: u32,
}

/// Stable events emitted for one generation request.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(tag = "event", rename_all = "snake_case")]
pub(crate) enum AiStreamEvent {
    Started {
        request_id: String,
        model_id: String,
    },
    TextDelta {
        request_id: String,
        text: String,
    },
    Usage {
        request_id: String,
        usage: AiUsage,
    },
    Finished {
        result: AiGenerationResult,
    },
    Cancelled {
        request_id: String,
        usage: AiUsage,
    },
    Failed {
        request_id: String,
        error: AiBackendError,
    },
}

impl AiStreamEvent {
    pub(crate) fn request_id(&self) -> &str {
        match self {
            Self::Started { request_id, .. }
            | Self::TextDelta { request_id, .. }
            | Self::Usage { request_id, .. }
            | Self::Cancelled { request_id, .. }
            | Self::Failed { request_id, .. } => request_id,
            Self::Finished { result } => &result.request_id,
        }
    }

    pub(crate) fn is_terminal(&self) -> bool {
        matches!(
            self,
            Self::Finished { .. } | Self::Cancelled { .. } | Self::Failed { .. }
        )
    }
}

/// A bounded stream backed by the runtime worker's event channel.
pub(crate) struct AiGenerationStream {
    request_id: String,
    receiver: mpsc::Receiver<AiStreamEvent>,
    cancellation: Arc<AtomicBool>,
}

impl AiGenerationStream {
    pub(crate) fn new(
        request_id: String,
        receiver: mpsc::Receiver<AiStreamEvent>,
        cancellation: Arc<AtomicBool>,
    ) -> Self {
        Self {
            request_id,
            receiver,
            cancellation,
        }
    }

    pub(crate) fn request_id(&self) -> &str {
        &self.request_id
    }

    pub(crate) fn cancel(&self) {
        self.cancellation.store(true, Ordering::Release);
    }

    /// Consume the stream and return its successful terminal result.
    pub(crate) async fn collect_result(mut self) -> Result<AiGenerationResult, AiBackendError> {
        while let Some(event) = self.receiver.recv().await {
            match event {
                AiStreamEvent::Finished { result } => return Ok(result),
                AiStreamEvent::Cancelled { .. } => return Err(AiBackendError::Cancelled),
                AiStreamEvent::Failed { error, .. } => return Err(error),
                AiStreamEvent::Started { .. }
                | AiStreamEvent::TextDelta { .. }
                | AiStreamEvent::Usage { .. } => {}
            }
        }
        Err(AiBackendError::WorkerClosed)
    }
}

impl Stream for AiGenerationStream {
    type Item = AiStreamEvent;

    fn poll_next(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        let this = self.get_mut();
        Pin::new(&mut this.receiver).poll_recv(cx)
    }
}

impl Drop for AiGenerationStream {
    fn drop(&mut self) {
        self.cancellation.store(true, Ordering::Release);
    }
}

/// Stable, object-safe boundary used by commands, conversations, and later
/// harness layers. No llama-cpp type crosses this trait.
#[async_trait]
pub(crate) trait AiBackend: Send + Sync {
    async fn load_model(&self, model_id: &str) -> Result<AiLoadedModel, AiBackendError>;
    async fn unload_model(&self, model_id: Option<&str>) -> Result<(), AiBackendError>;
    async fn generate(
        &self,
        request: AiGenerationRequest,
    ) -> Result<AiGenerationStream, AiBackendError>;
    fn cancel(&self, request_id: &str) -> Result<(), AiBackendError>;
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn request_validation_rejects_empty_limits_and_malformed_sampling() {
        let mut request = AiGenerationRequest::new("request", "model", "hello");
        request.max_output_tokens = 0;
        assert!(matches!(
            request.validate(),
            Err(AiBackendError::InvalidRequest(_))
        ));

        request.max_output_tokens = 1;
        request.sampling.top_p = 0.0;
        assert!(matches!(
            request.validate(),
            Err(AiBackendError::InvalidRequest(_))
        ));
    }

    #[test]
    fn request_validation_rejects_nul_and_empty_stop_values() {
        let mut request = AiGenerationRequest::new("request", "model", "hello\0world");
        assert!(matches!(
            request.validate(),
            Err(AiBackendError::InvalidRequest(_))
        ));

        request.messages[0].content = "hello".to_string();
        request.stop_sequences.push(String::new());
        assert!(matches!(
            request.validate(),
            Err(AiBackendError::InvalidRequest(_))
        ));
    }

    #[test]
    fn terminal_event_helpers_cover_finished_cancelled_and_failed() {
        let usage = AiUsage::default();
        let result = AiGenerationResult {
            request_id: "request".to_string(),
            model_id: "model".to_string(),
            text: String::new(),
            finish_reason: AiFinishReason::EndOfGeneration,
            usage,
            duration_ms: 0,
        };
        let events = [
            AiStreamEvent::Finished { result },
            AiStreamEvent::Cancelled {
                request_id: "request".to_string(),
                usage,
            },
            AiStreamEvent::Failed {
                request_id: "request".to_string(),
                error: AiBackendError::Cancelled,
            },
        ];
        assert!(events.iter().all(AiStreamEvent::is_terminal));
        assert!(events.iter().all(|event| event.request_id() == "request"));
    }
}
