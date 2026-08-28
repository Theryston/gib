use crate::ai::prompts::{PromptContext, PromptError, PromptKey, PromptService, ReasoningPolicy};
use crate::ai::runtime::AiBackendError;
use crate::ai::runtime::{
    AiBackend, AiGenerationRequest as RuntimeGenerationRequest, AiGrammar, AiSamplingSettings,
};
use crate::ai::structured::{SchemaError, SchemaKey};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize, de::DeserializeOwned};
use serde_json::Value;
use std::fmt;
use std::sync::Arc;

const DEFAULT_MAX_ATTEMPTS: u8 = 3;
const MAX_ALLOWED_ATTEMPTS: u8 = 8;
const DEFAULT_MAX_OUTPUT_BYTES: usize = 64 * 1024;
const MAX_ALLOWED_OUTPUT_BYTES: usize = 4 * 1024 * 1024;
const MAX_ERROR_PATHS: usize = 8;
const MAX_FEEDBACK_BYTES: usize = 2 * 1024;
const MAX_DIAGNOSTIC_BYTES: usize = 256;

/// A request for one version-pinned typed generation operation.
#[derive(Debug, Clone)]
pub(crate) struct GenerationRequest {
    pub(crate) request_id: String,
    pub(crate) model_id: String,
    pub(crate) prompt: PromptKey,
    pub(crate) context: PromptContext,
    pub(crate) sampling: AiSamplingSettings,
    pub(crate) context_limit: u32,
    pub(crate) max_output_tokens: u32,
    pub(crate) stop_sequences: Vec<String>,
    pub(crate) max_attempts: u8,
    pub(crate) max_output_bytes: usize,
}

impl GenerationRequest {
    pub(crate) fn new(
        request_id: impl Into<String>,
        model_id: impl Into<String>,
        prompt: PromptKey,
        user_request: impl Into<String>,
    ) -> Self {
        Self {
            request_id: request_id.into(),
            model_id: model_id.into(),
            prompt,
            context: PromptContext::new(user_request),
            sampling: AiSamplingSettings::default(),
            context_limit: 4096,
            max_output_tokens: 512,
            stop_sequences: Vec::new(),
            max_attempts: DEFAULT_MAX_ATTEMPTS,
            max_output_bytes: DEFAULT_MAX_OUTPUT_BYTES,
        }
    }

    pub(crate) fn with_context(mut self, context: PromptContext) -> Self {
        self.context = context;
        self
    }

    pub(crate) fn with_sampling(mut self, sampling: AiSamplingSettings) -> Self {
        self.sampling = sampling;
        self
    }

    pub(crate) fn with_context_limit(mut self, context_limit: u32) -> Self {
        self.context_limit = context_limit;
        self
    }

    pub(crate) fn with_max_output_tokens(mut self, max_output_tokens: u32) -> Self {
        self.max_output_tokens = max_output_tokens;
        self
    }

    pub(crate) fn with_stop_sequences(mut self, stop_sequences: Vec<String>) -> Self {
        self.stop_sequences = stop_sequences;
        self
    }

    pub(crate) fn with_max_attempts(mut self, max_attempts: u8) -> Self {
        self.max_attempts = max_attempts;
        self
    }

    pub(crate) fn with_max_output_bytes(mut self, max_output_bytes: usize) -> Self {
        self.max_output_bytes = max_output_bytes;
        self
    }

    fn validate(&self) -> Result<(), StructuredGenerationError> {
        if self.request_id.trim().is_empty() || self.request_id.contains('\0') {
            return Err(StructuredGenerationError::InvalidRequest(
                "request_id must be non-empty and contain no NUL bytes".to_string(),
            ));
        }
        if self.model_id.trim().is_empty() || self.model_id.contains('\0') {
            return Err(StructuredGenerationError::InvalidRequest(
                "model_id must be non-empty and contain no NUL bytes".to_string(),
            ));
        }
        if self.context_limit == 0 {
            return Err(StructuredGenerationError::InvalidRequest(
                "context_limit must be greater than zero".to_string(),
            ));
        }
        if self.max_output_tokens == 0 {
            return Err(StructuredGenerationError::InvalidRequest(
                "max_output_tokens must be greater than zero".to_string(),
            ));
        }
        if self.max_attempts == 0 || self.max_attempts > MAX_ALLOWED_ATTEMPTS {
            return Err(StructuredGenerationError::InvalidRequest(format!(
                "max_attempts must be between one and {MAX_ALLOWED_ATTEMPTS}"
            )));
        }
        if self.max_output_bytes == 0 || self.max_output_bytes > MAX_ALLOWED_OUTPUT_BYTES {
            return Err(StructuredGenerationError::InvalidRequest(format!(
                "max_output_bytes must be between one and {MAX_ALLOWED_OUTPUT_BYTES}"
            )));
        }
        if self
            .stop_sequences
            .iter()
            .any(|stop| stop.is_empty() || stop.contains('\0'))
        {
            return Err(StructuredGenerationError::InvalidRequest(
                "stop sequences must be non-empty and contain no NUL bytes".to_string(),
            ));
        }
        Ok(())
    }
}

/// Trace-safe metadata for a successful typed generation.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub(crate) struct GenerationMetadata {
    pub(crate) request_id: String,
    pub(crate) model_id: String,
    pub(crate) prompt: PromptKey,
    pub(crate) output_schema: SchemaKey,
    pub(crate) reasoning_policy: ReasoningPolicy,
    pub(crate) attempts: u8,
    pub(crate) prompt_hashes: Vec<String>,
    pub(crate) schema_hash: String,
    pub(crate) output_bytes: usize,
}

/// A typed value and the trace metadata associated with its generation.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub(crate) struct GenerationOutcome<T> {
    pub(crate) value: T,
    pub(crate) metadata: GenerationMetadata,
}

/// Semantic validation runs after JSON Schema validation and typed
/// deserialization. It must not mutate the generated value.
pub(crate) trait SemanticValidator<T>: Send + Sync {
    fn validate(&self, value: &T) -> Result<(), String>;
}

impl<T, F> SemanticValidator<T> for F
where
    F: Fn(&T) -> Result<(), String> + Send + Sync,
{
    fn validate(&self, value: &T) -> Result<(), String> {
        self(value)
    }
}

struct NoSemanticValidation;

impl<T> SemanticValidator<T> for NoSemanticValidation {
    fn validate(&self, _value: &T) -> Result<(), String> {
        Ok(())
    }
}

/// Errors from prompt lookup, grammar construction, model generation, parsing,
/// schema validation, and typed semantic validation.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(tag = "code", content = "details", rename_all = "snake_case")]
pub(crate) enum StructuredGenerationError {
    InvalidRequest(String),
    Prompt(PromptError),
    Schema(SchemaError),
    InvalidJson {
        attempt: u8,
        line: usize,
        column: usize,
    },
    SchemaValidationFailed {
        attempt: u8,
        paths: Vec<String>,
    },
    DeserializationFailed {
        attempt: u8,
    },
    SemanticValidationFailed {
        attempt: u8,
        message: String,
    },
    OutputLimit {
        limit: usize,
        actual: usize,
    },
    Cancelled,
    Backend(AiBackendError),
    RetryExhausted {
        attempts: u8,
        last_error: Box<StructuredGenerationError>,
    },
}

impl StructuredGenerationError {
    pub(crate) fn code(&self) -> &'static str {
        match self {
            Self::InvalidRequest(_) => "invalid_request",
            Self::Prompt(error) => error.code(),
            Self::Schema(error) => error.code(),
            Self::InvalidJson { .. } => "invalid_json",
            Self::SchemaValidationFailed { .. } => "schema_validation_failed",
            Self::DeserializationFailed { .. } => "deserialization_failed",
            Self::SemanticValidationFailed { .. } => "semantic_validation_failed",
            Self::OutputLimit { .. } => "output_limit",
            Self::Cancelled => "cancelled",
            Self::Backend(error) => error.code(),
            Self::RetryExhausted { .. } => "retry_exhausted",
        }
    }

    fn retryable(&self) -> bool {
        matches!(
            self,
            Self::InvalidJson { .. }
                | Self::SchemaValidationFailed { .. }
                | Self::DeserializationFailed { .. }
                | Self::SemanticValidationFailed { .. }
                | Self::OutputLimit { .. }
        )
    }

    fn feedback(&self) -> String {
        let feedback = match self {
            Self::InvalidJson { .. } => {
                "The previous response was not valid JSON. Return exactly one complete JSON value and no Markdown or commentary."
                    .to_string()
            }
            Self::SchemaValidationFailed { paths, .. } => format!(
                "The previous JSON did not satisfy the output schema. Correct these field paths: {}. Return a complete JSON value.",
                paths.join(", ")
            ),
            Self::DeserializationFailed { .. } => {
                "The previous JSON matched the schema boundary but could not be deserialized into the typed response. Return only the exact declared JSON shape."
                    .to_string()
            }
            Self::SemanticValidationFailed { message, .. } => format!(
                "The previous response was structurally valid but failed a domain invariant: {message}. Return a corrected response without changing the original request."
            ),
            Self::OutputLimit { limit, .. } => format!(
                "The previous response exceeded the {limit}-byte output limit. Return a compact complete JSON value."
            ),
            _ => "Return a complete JSON value matching the pinned output contract.".to_string(),
        };
        truncate_diagnostic(&feedback, MAX_FEEDBACK_BYTES)
    }
}

impl fmt::Display for StructuredGenerationError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::InvalidRequest(message) => write!(
                formatter,
                "invalid structured generation request: {message}"
            ),
            Self::Prompt(error) => write!(formatter, "structured generation prompt error: {error}"),
            Self::Schema(error) => write!(formatter, "structured generation schema error: {error}"),
            Self::InvalidJson {
                attempt,
                line,
                column,
            } => write!(
                formatter,
                "model output on attempt {attempt} was not valid JSON at line {line}, column {column}"
            ),
            Self::SchemaValidationFailed { attempt, paths } => write!(
                formatter,
                "model output on attempt {attempt} failed schema validation at {}",
                paths.join(", ")
            ),
            Self::DeserializationFailed { attempt } => write!(
                formatter,
                "model output on attempt {attempt} could not be deserialized into the requested type"
            ),
            Self::SemanticValidationFailed { attempt, message } => write!(
                formatter,
                "typed output on attempt {attempt} failed semantic validation: {message}"
            ),
            Self::OutputLimit { limit, actual } => {
                write!(
                    formatter,
                    "model output is {actual} bytes; the limit is {limit}"
                )
            }
            Self::Cancelled => formatter.write_str("structured generation was cancelled"),
            Self::Backend(error) => write!(formatter, "AI backend generation failed: {error}"),
            Self::RetryExhausted {
                attempts,
                last_error,
            } => write!(
                formatter,
                "structured generation failed after {attempts} attempts: {last_error}"
            ),
        }
    }
}

impl std::error::Error for StructuredGenerationError {}

impl From<PromptError> for StructuredGenerationError {
    fn from(error: PromptError) -> Self {
        Self::Prompt(error)
    }
}

impl From<SchemaError> for StructuredGenerationError {
    fn from(error: SchemaError) -> Self {
        Self::Schema(error)
    }
}

/// High-level typed generation service. It is deliberately independent of
/// conversations and persistence so retries cannot create fake conversation
/// turns.
#[derive(Clone)]
pub(crate) struct StructuredGenerator {
    backend: Arc<dyn AiBackend>,
    prompts: PromptService,
}

impl fmt::Debug for StructuredGenerator {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("StructuredGenerator")
            .field("prompts", &self.prompts)
            .finish_non_exhaustive()
    }
}

impl StructuredGenerator {
    pub(crate) fn new(backend: Arc<dyn AiBackend>, prompts: PromptService) -> Self {
        Self { backend, prompts }
    }

    pub(crate) async fn generate<T>(
        &self,
        request: GenerationRequest,
    ) -> Result<GenerationOutcome<T>, StructuredGenerationError>
    where
        T: DeserializeOwned + JsonSchema,
    {
        self.generate_with_validator(request, NoSemanticValidation)
            .await
    }

    pub(crate) async fn generate_with_validator<T, V>(
        &self,
        request: GenerationRequest,
        validator: V,
    ) -> Result<GenerationOutcome<T>, StructuredGenerationError>
    where
        T: DeserializeOwned + JsonSchema,
        V: SemanticValidator<T>,
    {
        request.validate()?;
        let definition = self.prompts.get(&request.prompt)?;
        let output_schema = self.prompts.schemas().get(&definition.output_schema)?;
        let mut prompt_hashes = Vec::with_capacity(request.max_attempts as usize);
        let mut last_retryable_error: Option<StructuredGenerationError> = None;

        for attempt_index in 0..request.max_attempts {
            let attempt = attempt_index + 1;
            let context = match &last_retryable_error {
                Some(error) => request
                    .context
                    .clone()
                    .with_retry_feedback(error.feedback()),
                None => request.context.clone(),
            };
            let rendered = self.prompts.render(&request.prompt, &context)?;
            prompt_hashes.push(rendered.prompt_hash.clone());

            let runtime_request = RuntimeGenerationRequest {
                request_id: attempt_request_id(&request.request_id, attempt),
                model_id: request.model_id.clone(),
                messages: rendered.messages,
                sampling: sampling_for_attempt(request.sampling, attempt_index),
                context_limit: request.context_limit,
                max_output_tokens: request.max_output_tokens,
                stop_sequences: request.stop_sequences.clone(),
                grammar: Some(AiGrammar {
                    grammar: output_schema.grammar().to_string(),
                    root: "root".to_string(),
                }),
            };
            let result = self
                .backend
                .generate(runtime_request)
                .await
                .map_err(StructuredGenerationError::Backend)?
                .collect_result()
                .await
                .map_err(map_backend_error)?;

            match validate_output::<T, V>(
                &result.text,
                attempt,
                request.max_output_bytes,
                &output_schema,
                &validator,
            ) {
                Ok(value) => {
                    return Ok(GenerationOutcome {
                        value,
                        metadata: GenerationMetadata {
                            request_id: request.request_id,
                            model_id: request.model_id,
                            prompt: definition.key.clone(),
                            output_schema: output_schema.key().clone(),
                            reasoning_policy: definition.reasoning_policy,
                            attempts: attempt,
                            prompt_hashes,
                            schema_hash: output_schema.hash().to_string(),
                            output_bytes: result.text.len(),
                        },
                    });
                }
                Err(error) if error.retryable() && attempt < request.max_attempts => {
                    last_retryable_error = Some(error);
                }
                Err(error) if error.retryable() => {
                    return Err(StructuredGenerationError::RetryExhausted {
                        attempts: attempt,
                        last_error: Box::new(error),
                    });
                }
                Err(error) => return Err(error),
            }
        }

        Err(StructuredGenerationError::RetryExhausted {
            attempts: request.max_attempts,
            last_error: Box::new(StructuredGenerationError::InvalidRequest(
                "structured generation exhausted without a result".to_string(),
            )),
        })
    }
}

fn validate_output<T, V>(
    text: &str,
    attempt: u8,
    max_output_bytes: usize,
    schema: &crate::ai::structured::SchemaDefinition,
    semantic_validator: &V,
) -> Result<T, StructuredGenerationError>
where
    T: DeserializeOwned + JsonSchema,
    V: SemanticValidator<T>,
{
    let output_bytes = text.len();
    if output_bytes > max_output_bytes {
        return Err(StructuredGenerationError::OutputLimit {
            limit: max_output_bytes,
            actual: output_bytes,
        });
    }
    let value = serde_json::from_str::<Value>(text).map_err(|error| {
        StructuredGenerationError::InvalidJson {
            attempt,
            line: error.line(),
            column: error.column(),
        }
    })?;
    let paths = schema
        .validator()
        .iter_errors(&value)
        .take(MAX_ERROR_PATHS)
        .map(|error| {
            let path = error.instance_path().as_str();
            if path.is_empty() {
                "$".to_string()
            } else {
                truncate_diagnostic(path, MAX_DIAGNOSTIC_BYTES)
            }
        })
        .collect::<Vec<_>>();
    if !paths.is_empty() {
        return Err(StructuredGenerationError::SchemaValidationFailed { attempt, paths });
    }

    let typed = serde_json::from_value::<T>(value)
        .map_err(|_| StructuredGenerationError::DeserializationFailed { attempt })?;
    semantic_validator.validate(&typed).map_err(|message| {
        StructuredGenerationError::SemanticValidationFailed {
            attempt,
            message: truncate_diagnostic(&message, MAX_DIAGNOSTIC_BYTES),
        }
    })?;
    Ok(typed)
}

fn map_backend_error(error: AiBackendError) -> StructuredGenerationError {
    match error {
        AiBackendError::Cancelled => StructuredGenerationError::Cancelled,
        other => StructuredGenerationError::Backend(other),
    }
}

fn attempt_request_id(base: &str, attempt: u8) -> String {
    format!("{base}:structured-attempt-{attempt}")
}

fn sampling_for_attempt(settings: AiSamplingSettings, attempt_index: u8) -> AiSamplingSettings {
    AiSamplingSettings {
        seed: Some(
            settings
                .seed
                .unwrap_or(0)
                .wrapping_add(u32::from(attempt_index)),
        ),
        ..settings
    }
}

fn truncate_diagnostic(value: &str, max_bytes: usize) -> String {
    if value.len() <= max_bytes {
        return value.to_string();
    }
    let mut end = max_bytes;
    while end > 0 && !value.is_char_boundary(end) {
        end -= 1;
    }
    value[..end].to_string()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ai::prompts::{PromptDefinition, PromptKey};
    use crate::ai::structured::fake::ScriptedAiBackend;
    use schemars::JsonSchema;
    use serde::{Deserialize, Serialize};

    #[derive(Debug, Clone, Deserialize, JsonSchema, Serialize, PartialEq, Eq)]
    #[serde(deny_unknown_fields)]
    struct Decision {
        action: String,
        score: u32,
    }

    fn generator(scripts: Vec<&str>) -> (StructuredGenerator, Arc<ScriptedAiBackend>, PromptKey) {
        let schemas = crate::ai::structured::SchemaRegistry::default();
        let output_schema = schemas
            .register::<Decision>("decision", "1.0.0")
            .expect("schema should compile");
        let prompts = PromptService::new(schemas);
        let prompt_key = PromptKey::new("decision-prompt", "1.0.0").expect("key is valid");
        prompts
            .register(
                PromptDefinition::new(
                    prompt_key.clone(),
                    "test",
                    "test generation",
                    "You are a test model.",
                    "Return the typed decision.",
                    None,
                    output_schema,
                    ReasoningPolicy::Disabled,
                )
                .expect("prompt should be valid"),
            )
            .expect("prompt should register");
        let backend = Arc::new(ScriptedAiBackend::new(scripts));
        let generator = StructuredGenerator::new(backend.clone(), prompts);
        (generator, backend, prompt_key)
    }

    fn request(prompt: PromptKey) -> GenerationRequest {
        GenerationRequest::new("request", "fake-model", prompt, "choose")
            .with_max_output_tokens(64)
            .with_max_attempts(3)
    }

    #[tokio::test]
    async fn malformed_json_is_retried_without_becoming_a_conversation_turn() {
        let (generator, backend, prompt) =
            generator(vec!["not json", r#"{"action":"keep","score":7}"#]);
        let outcome = generator
            .generate::<Decision>(request(prompt))
            .await
            .expect("second scripted response should succeed");
        assert_eq!(outcome.value.action, "keep");
        assert_eq!(outcome.metadata.attempts, 2);
        assert_eq!(backend.attempts(), 2);
        assert_eq!(outcome.metadata.prompt_hashes.len(), 2);
    }

    #[tokio::test]
    async fn schema_invalid_output_is_retried_and_reports_field_paths() {
        let (generator, _, prompt) = generator(vec![
            r#"{"action":"keep","score":"bad"}"#,
            r#"{"action":"keep","score":1}"#,
        ]);
        let outcome = generator
            .generate::<Decision>(request(prompt))
            .await
            .expect("valid response should be returned");
        assert_eq!(outcome.value.score, 1);
        assert_eq!(outcome.metadata.attempts, 2);
    }

    #[tokio::test]
    async fn semantic_validation_is_after_schema_validation_and_is_bounded() {
        let (generator, backend, prompt) = generator(vec![
            r#"{"action":"keep","score":0}"#,
            r#"{"action":"keep","score":5}"#,
        ]);
        let outcome = generator
            .generate_with_validator(request(prompt), |decision: &Decision| {
                if decision.score == 0 {
                    Err("score must be positive".to_string())
                } else {
                    Ok(())
                }
            })
            .await
            .expect("semantic retry should succeed");
        assert_eq!(outcome.value.score, 5);
        assert_eq!(backend.attempts(), 2);
    }

    #[tokio::test]
    async fn exhausted_retries_return_typed_failure_without_raw_model_output() {
        let (generator, backend, prompt) = generator(vec![
            r#"{"action":"keep","score":"bad"}"#,
            r#"{"action":"keep","score":"still bad"}"#,
            r#"{"action":"keep","score":"secret-invalid-output"}"#,
        ]);
        let error = generator
            .generate::<Decision>(request(prompt))
            .await
            .expect_err("all scripted responses should fail");
        assert!(matches!(
            error,
            StructuredGenerationError::RetryExhausted {
                attempts: 3,
                last_error: _
            }
        ));
        assert!(!error.to_string().contains("secret-invalid-output"));
        assert_eq!(backend.attempts(), 3);
    }
}
