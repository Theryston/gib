use crate::ai::conversation::{
    Conversation, ConversationError, ConversationMessageRole, ConversationMessageStatus,
    ConversationService,
};
use crate::ai::runtime::{
    AiBackend, AiBackendError, AiFinishReason, AiGenerationRequest, AiMessage, AiMessageRole,
    AiSamplingSettings, AiStreamEvent, AiUsage,
};
use futures::StreamExt;
use rand_core::{OsRng, TryRngCore};
use serde::{Deserialize, Serialize};
use std::fmt;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use tokio::sync::Notify;

const MAX_INTERRUPTED_MESSAGE_BYTES: usize = 64 * 1024;
const DIRECT_CHAT_SYSTEM_MESSAGE: &str = "You are the GIB local AI assistant. Answer the user directly and clearly. Repository investigation, backup history, and restore actions are not available in this initial chat mode. Do not claim to have performed actions or accessed data that you cannot access.";

/// The input needed to execute one persisted direct-chat turn.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct AiTurnRequest {
    pub(crate) conversation_id: Option<String>,
    pub(crate) message: String,
    pub(crate) turn_id: String,
}

impl AiTurnRequest {
    pub(crate) fn new(
        conversation_id: Option<String>,
        message: impl Into<String>,
    ) -> Result<Self, AiTurnError> {
        Self::with_turn_id(conversation_id, message, generate_id("turn")?)
    }

    pub(crate) fn with_turn_id(
        conversation_id: Option<String>,
        message: impl Into<String>,
        turn_id: impl Into<String>,
    ) -> Result<Self, AiTurnError> {
        let turn_id = turn_id.into();
        crate::ai::conversation::validate_conversation_id(&turn_id)
            .map_err(AiTurnError::Conversation)?;
        let message = message.into();
        if message.trim().is_empty() {
            return Err(AiTurnError::InvalidMessage);
        }
        Ok(Self {
            conversation_id,
            message,
            turn_id,
        })
    }
}

/// Errors at the boundary between a command, durable conversation state, and
/// the local model runtime. All variants are safe to expose in JSON mode.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(tag = "code", content = "details", rename_all = "snake_case")]
pub(crate) enum AiTurnError {
    InvalidMessage,
    Conversation(ConversationError),
    Backend(AiBackendError),
    Cancelled,
    StreamClosed,
    StreamProtocol,
    TurnAlreadyRecorded { turn_id: String },
    PendingTurn { turn_id: String },
    EmptyResponse,
}

impl AiTurnError {
    pub(crate) fn code(&self) -> &'static str {
        match self {
            Self::InvalidMessage => "invalid_message",
            Self::Conversation(error) => error.code(),
            Self::Backend(error) => error.code(),
            Self::Cancelled => "cancelled",
            Self::StreamClosed => "stream_closed",
            Self::StreamProtocol => "stream_protocol_error",
            Self::TurnAlreadyRecorded { .. } => "turn_already_recorded",
            Self::PendingTurn { .. } => "pending_turn",
            Self::EmptyResponse => "empty_response",
        }
    }

    pub(crate) fn is_cancelled(&self) -> bool {
        matches!(self, Self::Cancelled)
    }
}

impl fmt::Display for AiTurnError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::InvalidMessage => formatter.write_str("the AI message cannot be empty"),
            Self::Conversation(error) => write!(formatter, "{error}"),
            Self::Backend(error) => write!(formatter, "{error}"),
            Self::Cancelled => formatter.write_str("AI generation was cancelled"),
            Self::StreamClosed => {
                formatter.write_str("the AI generation stream closed unexpectedly")
            }
            Self::StreamProtocol => {
                formatter.write_str("the AI generation stream returned an invalid event sequence")
            }
            Self::TurnAlreadyRecorded { turn_id } => {
                write!(formatter, "AI turn '{turn_id}' has already been recorded")
            }
            Self::PendingTurn { turn_id } => write!(
                formatter,
                "AI turn '{turn_id}' is still pending; retry the same message to resume it"
            ),
            Self::EmptyResponse => {
                formatter.write_str("the local model returned no visible assistant response")
            }
        }
    }
}

impl std::error::Error for AiTurnError {}

impl From<ConversationError> for AiTurnError {
    fn from(error: ConversationError) -> Self {
        Self::Conversation(error)
    }
}

impl From<AiBackendError> for AiTurnError {
    fn from(error: AiBackendError) -> Self {
        Self::Backend(error)
    }
}

/// The stable final payload returned for a completed turn.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub(crate) struct AiTurnResponse {
    pub(crate) conversation_id: String,
    pub(crate) turn_id: String,
    pub(crate) user_message_id: String,
    pub(crate) assistant_message_id: String,
    pub(crate) model_id: String,
    pub(crate) text: String,
    pub(crate) finish_reason: AiFinishReason,
    pub(crate) usage: AiUsage,
}

/// Service-level events consumed by both the terminal and JSON adapters.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(tag = "event", rename_all = "snake_case")]
pub(crate) enum AiTurnEvent {
    Started {
        conversation_id: String,
        turn_id: String,
        user_message_id: String,
        model_id: String,
    },
    Progress {
        conversation_id: String,
        turn_id: String,
        usage: AiUsage,
    },
    #[serde(rename = "delta")]
    Delta {
        conversation_id: String,
        turn_id: String,
        text: String,
    },
    Finished {
        response: AiTurnResponse,
    },
    Cancelled {
        conversation_id: String,
        turn_id: String,
        partial_text: String,
        usage: AiUsage,
    },
    Failed {
        conversation_id: String,
        turn_id: String,
        error: AiTurnError,
    },
}

impl AiTurnEvent {
    #[cfg(test)]
    pub(crate) fn is_terminal(&self) -> bool {
        matches!(
            self,
            Self::Finished { .. } | Self::Cancelled { .. } | Self::Failed { .. }
        )
    }
}

pub(crate) type AiTurnEventSink = Arc<dyn Fn(&AiTurnEvent) + Send + Sync>;

/// Cooperative cancellation shared by a command adapter and the turn
/// service. It does not own a signal handler, which keeps the service usable
/// in JSON automation and in tests.
#[derive(Clone, Default)]
pub(crate) struct AiCancellation {
    cancelled: Arc<AtomicBool>,
    notification: Arc<Notify>,
}

impl AiCancellation {
    pub(crate) fn new() -> Self {
        Self::default()
    }

    pub(crate) fn cancel(&self) {
        self.cancelled.store(true, Ordering::Release);
        // `notify_one` retains a permit when cancellation wins the small
        // race before the async waiter is polled; `notify_waiters` would not.
        self.notification.notify_one();
    }

    pub(crate) fn is_cancelled(&self) -> bool {
        self.cancelled.load(Ordering::Acquire)
    }

    async fn cancelled(&self) {
        if self.is_cancelled() {
            return;
        }
        let notified = self.notification.notified();
        if self.is_cancelled() {
            return;
        }
        notified.await;
    }
}

/// The initial direct-chat prompt policy. It deliberately has no tools,
/// structured output, hidden reasoning, or agent instructions.
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct AiPromptPolicy {
    pub(crate) system_message: Option<String>,
    pub(crate) sampling: AiSamplingSettings,
    pub(crate) context_limit: u32,
    pub(crate) max_output_tokens: u32,
}

impl Default for AiPromptPolicy {
    fn default() -> Self {
        Self {
            system_message: Some(DIRECT_CHAT_SYSTEM_MESSAGE.to_string()),
            sampling: AiSamplingSettings::default(),
            context_limit: 4096,
            max_output_tokens: 256,
        }
    }
}

impl AiPromptPolicy {
    fn messages(&self, conversation: &Conversation) -> Vec<AiMessage> {
        let mut messages = Vec::with_capacity(
            conversation.messages.len() + usize::from(self.system_message.is_some()),
        );
        if let Some(system_message) = &self.system_message {
            messages.push(AiMessage::new(
                AiMessageRole::System,
                system_message.clone(),
            ));
        }
        messages.extend(conversation.messages.iter().map(|message| {
            let role = match message.role {
                ConversationMessageRole::User => AiMessageRole::User,
                ConversationMessageRole::Assistant => AiMessageRole::Assistant,
            };
            AiMessage::new(role, message.content.clone())
        }));
        messages
    }
}

/// The single persistence and runtime lifecycle used by both command modes.
#[derive(Clone)]
pub(crate) struct AiTurnService {
    conversations: ConversationService,
    backend: Arc<dyn AiBackend>,
    model_id: String,
    prompt_policy: AiPromptPolicy,
}

impl AiTurnService {
    pub(crate) fn new(
        conversations: ConversationService,
        backend: Arc<dyn AiBackend>,
        model_id: impl Into<String>,
        prompt_policy: AiPromptPolicy,
    ) -> Self {
        Self {
            conversations,
            backend,
            model_id: model_id.into(),
            prompt_policy,
        }
    }

    pub(crate) async fn run_turn(
        &self,
        request: AiTurnRequest,
        cancellation: AiCancellation,
        sink: Option<AiTurnEventSink>,
    ) -> Result<AiTurnResponse, AiTurnError> {
        let mut request = request;
        let conversation = self
            .resolve_conversation(request.conversation_id.clone())
            .await?;
        let pending = conversation
            .messages
            .iter()
            .rev()
            .find(|message| {
                message.role == ConversationMessageRole::User
                    && message.status == ConversationMessageStatus::Pending
            })
            .map(|message| {
                (
                    message.message_id.clone(),
                    message.content.clone(),
                    message.turn_id.clone(),
                )
            });
        let (conversation, user_message_id) = if let Some((message_id, content, turn_id)) = pending
        {
            if content != request.message {
                return Err(AiTurnError::PendingTurn {
                    turn_id: turn_id.unwrap_or_else(|| "unknown".to_string()),
                });
            }
            let Some(turn_id) = turn_id else {
                return Err(AiTurnError::StreamProtocol);
            };
            request.turn_id = turn_id;
            (conversation, message_id)
        } else {
            if conversation
                .messages
                .iter()
                .any(|message| message.turn_id.as_deref() == Some(request.turn_id.as_str()))
            {
                return Err(AiTurnError::TurnAlreadyRecorded {
                    turn_id: request.turn_id,
                });
            }
            self.append_user_with_retry(conversation, &request).await?
        };
        emit_event(
            sink.as_ref(),
            AiTurnEvent::Started {
                conversation_id: conversation.conversation_id.clone(),
                turn_id: request.turn_id.clone(),
                user_message_id: user_message_id.clone(),
                model_id: self.model_id.clone(),
            },
        );

        if cancellation.is_cancelled() {
            return self
                .finish_interrupted(
                    &conversation,
                    &request,
                    &user_message_id,
                    String::new(),
                    AiUsage::default(),
                    AiTurnError::Cancelled,
                    sink.as_ref(),
                )
                .await;
        }

        let generation_request = AiGenerationRequest {
            request_id: request.turn_id.clone(),
            model_id: self.model_id.clone(),
            messages: self.prompt_policy.messages(&conversation),
            sampling: self.prompt_policy.sampling,
            context_limit: self.prompt_policy.context_limit,
            max_output_tokens: self.prompt_policy.max_output_tokens,
            stop_sequences: Vec::new(),
            grammar: None,
        };

        let mut stream = match self.backend.generate(generation_request).await {
            Ok(stream) => stream,
            Err(error) => {
                return self
                    .finish_interrupted(
                        &conversation,
                        &request,
                        &user_message_id,
                        String::new(),
                        AiUsage::default(),
                        AiTurnError::Backend(error),
                        sink.as_ref(),
                    )
                    .await;
            }
        };

        let runtime_request_id = stream.request_id().to_string();
        let mut text = String::new();
        let mut usage = AiUsage::default();
        let terminal = loop {
            tokio::select! {
                event = stream.next() => {
                    let Some(event) = event else {
                        break TerminalEvent::Failed(AiTurnError::StreamClosed);
                    };
                    if event.request_id() != runtime_request_id {
                        break TerminalEvent::Failed(AiTurnError::StreamProtocol);
                    }
                    match event {
                        AiStreamEvent::Started { .. } => {}
                        AiStreamEvent::TextDelta { text: delta, .. } => {
                            text.push_str(&delta);
                            emit_event(
                                sink.as_ref(),
                                AiTurnEvent::Delta {
                                    conversation_id: conversation.conversation_id.clone(),
                                    turn_id: request.turn_id.clone(),
                                    text: delta,
                                },
                            );
                        }
                        AiStreamEvent::Usage { usage: current, .. } => {
                            usage = current;
                            emit_event(
                                sink.as_ref(),
                                AiTurnEvent::Progress {
                                    conversation_id: conversation.conversation_id.clone(),
                                    turn_id: request.turn_id.clone(),
                                    usage,
                                },
                            );
                        }
                        AiStreamEvent::Finished { result } => {
                            break TerminalEvent::Finished(result);
                        }
                        AiStreamEvent::Cancelled { usage: current, .. } => {
                            usage = current;
                            break TerminalEvent::Cancelled;
                        }
                        AiStreamEvent::Failed { error, .. } => {
                            break TerminalEvent::Failed(AiTurnError::Backend(error));
                        }
                    }
                }
                _ = cancellation.cancelled() => {
                    stream.cancel();
                    let _ = self.backend.cancel(&runtime_request_id);
                    break TerminalEvent::Cancelled;
                }
            }
        };

        match terminal {
            TerminalEvent::Finished(result) => {
                if result.request_id != request.turn_id
                    || result.model_id != self.model_id
                    || result.text != text
                {
                    return self
                        .finish_interrupted(
                            &conversation,
                            &request,
                            &user_message_id,
                            text,
                            usage,
                            AiTurnError::StreamProtocol,
                            sink.as_ref(),
                        )
                        .await;
                }
                if result.text.trim().is_empty() {
                    return self
                        .finish_interrupted(
                            &conversation,
                            &request,
                            &user_message_id,
                            result.text,
                            usage,
                            AiTurnError::EmptyResponse,
                            sink.as_ref(),
                        )
                        .await;
                }
                usage = result.usage;
                let updated = match self
                    .conversations
                    .finish_turn(
                        conversation.conversation_id.clone(),
                        conversation.revision,
                        user_message_id.clone(),
                        request.turn_id.clone(),
                        result.text.clone(),
                        ConversationMessageStatus::Complete,
                    )
                    .await
                {
                    Ok(updated) => updated,
                    Err(error) => {
                        let error = AiTurnError::Conversation(error);
                        emit_event(
                            sink.as_ref(),
                            AiTurnEvent::Failed {
                                conversation_id: conversation.conversation_id.clone(),
                                turn_id: request.turn_id.clone(),
                                error: error.clone(),
                            },
                        );
                        return Err(error);
                    }
                };
                let Some(assistant_message_id) =
                    updated.messages.iter().rev().find_map(|message| {
                        (message.turn_id.as_deref() == Some(request.turn_id.as_str())
                            && message.role == ConversationMessageRole::Assistant
                            && message.status == ConversationMessageStatus::Complete)
                            .then(|| message.message_id.clone())
                    })
                else {
                    return self
                        .finish_interrupted(
                            &conversation,
                            &request,
                            &user_message_id,
                            result.text,
                            usage,
                            AiTurnError::StreamProtocol,
                            sink.as_ref(),
                        )
                        .await;
                };
                let response = AiTurnResponse {
                    conversation_id: conversation.conversation_id.clone(),
                    turn_id: request.turn_id,
                    user_message_id,
                    assistant_message_id,
                    model_id: self.model_id.clone(),
                    text: result.text,
                    finish_reason: result.finish_reason,
                    usage,
                };
                emit_event(
                    sink.as_ref(),
                    AiTurnEvent::Finished {
                        response: response.clone(),
                    },
                );
                Ok(response)
            }
            TerminalEvent::Cancelled => {
                self.finish_interrupted(
                    &conversation,
                    &request,
                    &user_message_id,
                    text,
                    usage,
                    AiTurnError::Cancelled,
                    sink.as_ref(),
                )
                .await
            }
            TerminalEvent::Failed(error) => {
                self.finish_interrupted(
                    &conversation,
                    &request,
                    &user_message_id,
                    text,
                    usage,
                    error,
                    sink.as_ref(),
                )
                .await
            }
        }
    }

    async fn resolve_conversation(
        &self,
        explicit_id: Option<String>,
    ) -> Result<Conversation, AiTurnError> {
        match self.conversations.resolve(explicit_id.clone()).await {
            Ok(conversation) => Ok(conversation),
            Err(ConversationError::NoActiveConversation) if explicit_id.is_none() => {
                let created = self.conversations.create(None).await?;
                Ok(self
                    .conversations
                    .select_active(created.conversation_id)
                    .await?)
            }
            Err(error) => Err(error.into()),
        }
    }

    async fn append_user_with_retry(
        &self,
        mut conversation: Conversation,
        request: &AiTurnRequest,
    ) -> Result<(Conversation, String), AiTurnError> {
        for attempt in 0..2 {
            match self
                .conversations
                .append_message_with_status_and_turn_id(
                    conversation.conversation_id.clone(),
                    conversation.revision,
                    ConversationMessageRole::User,
                    request.message.clone(),
                    ConversationMessageStatus::Pending,
                    Some(request.turn_id.clone()),
                )
                .await
            {
                Ok(updated) => {
                    let Some(message_id) = updated.messages.iter().rev().find_map(|message| {
                        (message.turn_id.as_deref() == Some(request.turn_id.as_str())
                            && message.role == ConversationMessageRole::User)
                            .then(|| message.message_id.clone())
                    }) else {
                        return Err(AiTurnError::StreamProtocol);
                    };
                    return Ok((updated, message_id));
                }
                Err(error)
                    if attempt == 0
                        && matches!(&error, ConversationError::RevisionConflict { .. }) =>
                {
                    conversation = self
                        .conversations
                        .load(conversation.conversation_id.clone())
                        .await?;
                    if let Some(pending) = conversation.messages.iter().rev().find(|message| {
                        message.role == ConversationMessageRole::User
                            && message.status == ConversationMessageStatus::Pending
                    }) {
                        return Err(AiTurnError::PendingTurn {
                            turn_id: pending
                                .turn_id
                                .clone()
                                .unwrap_or_else(|| "unknown".to_string()),
                        });
                    }
                    if conversation
                        .messages
                        .iter()
                        .any(|message| message.turn_id.as_deref() == Some(request.turn_id.as_str()))
                    {
                        return Err(AiTurnError::TurnAlreadyRecorded {
                            turn_id: request.turn_id.clone(),
                        });
                    }
                }
                Err(error) => return Err(error.into()),
            }
        }
        Err(AiTurnError::StreamProtocol)
    }

    async fn finish_interrupted(
        &self,
        conversation: &Conversation,
        request: &AiTurnRequest,
        user_message_id: &str,
        partial_text: String,
        usage: AiUsage,
        error: AiTurnError,
        sink: Option<&AiTurnEventSink>,
    ) -> Result<AiTurnResponse, AiTurnError> {
        let content = interrupted_content(&partial_text, &error);
        if let Err(persist_error) = self
            .conversations
            .finish_turn(
                conversation.conversation_id.clone(),
                conversation.revision,
                user_message_id.to_string(),
                request.turn_id.clone(),
                content,
                ConversationMessageStatus::Interrupted,
            )
            .await
        {
            let persist_error = AiTurnError::Conversation(persist_error);
            emit_event(
                sink,
                AiTurnEvent::Failed {
                    conversation_id: conversation.conversation_id.clone(),
                    turn_id: request.turn_id.clone(),
                    error: persist_error.clone(),
                },
            );
            return Err(persist_error);
        }

        if error.is_cancelled() {
            emit_event(
                sink,
                AiTurnEvent::Cancelled {
                    conversation_id: conversation.conversation_id.clone(),
                    turn_id: request.turn_id.clone(),
                    partial_text,
                    usage,
                },
            );
        } else {
            emit_event(
                sink,
                AiTurnEvent::Failed {
                    conversation_id: conversation.conversation_id.clone(),
                    turn_id: request.turn_id.clone(),
                    error: error.clone(),
                },
            );
        }
        Err(error)
    }
}

enum TerminalEvent {
    Finished(crate::ai::runtime::AiGenerationResult),
    Cancelled,
    Failed(AiTurnError),
}

fn emit_event(sink: Option<&AiTurnEventSink>, event: AiTurnEvent) {
    if let Some(sink) = sink {
        sink(&event);
    }
}

fn interrupted_content(partial_text: &str, error: &AiTurnError) -> String {
    if !partial_text.is_empty() {
        return truncate_visible_text(partial_text);
    }
    if error.is_cancelled() {
        "[AI response cancelled]".to_string()
    } else {
        format!("[AI response interrupted: {}]", error.code())
    }
}

fn truncate_visible_text(value: &str) -> String {
    let sanitized = value.replace('\0', "�");
    if sanitized.len() <= MAX_INTERRUPTED_MESSAGE_BYTES {
        return sanitized;
    }
    let mut end = MAX_INTERRUPTED_MESSAGE_BYTES;
    while !sanitized.is_char_boundary(end) {
        end -= 1;
    }
    let mut result = sanitized[..end].to_string();
    result.push_str("… [response interrupted]");
    result
}

fn generate_id(prefix: &str) -> Result<String, AiTurnError> {
    let mut random = [0_u8; 16];
    OsRng
        .try_fill_bytes(&mut random)
        .map_err(|_| AiTurnError::Conversation(ConversationError::io("generate AI turn ID")))?;
    let mut encoded = String::with_capacity(prefix.len() + 1 + random.len() * 2);
    encoded.push_str(prefix);
    encoded.push('-');
    for byte in random {
        encoded.push(hex_digit(byte >> 4));
        encoded.push(hex_digit(byte & 0x0f));
    }
    Ok(encoded)
}

fn hex_digit(value: u8) -> char {
    match value {
        0..=9 => char::from(b'0' + value),
        10..=15 => char::from(b'a' + value - 10),
        _ => unreachable!("hex digit input is limited to four bits"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ai::conversation::ConversationStore;
    use crate::ai::runtime::{AiBackend, FakeAiBackend};
    use std::path::PathBuf;
    use std::sync::Mutex;
    use std::time::{SystemTime, UNIX_EPOCH};

    fn temporary_root(name: &str) -> PathBuf {
        let stamp = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("clock should be after unix epoch")
            .as_nanos();
        let path =
            std::env::temp_dir().join(format!("gib-ai-chat-{name}-{}-{stamp}", std::process::id()));
        std::fs::create_dir_all(&path).expect("temporary root should be created");
        path
    }

    async fn make_service(name: &str) -> (AiTurnService, ConversationService, Arc<FakeAiBackend>) {
        let conversations =
            ConversationService::new(ConversationStore::from_root(temporary_root(name)));
        let backend = Arc::new(FakeAiBackend::new());
        backend
            .load_model("fake-model")
            .await
            .expect("fake model should load");
        let service = AiTurnService::new(
            conversations.clone(),
            backend.clone(),
            "fake-model",
            AiPromptPolicy::default(),
        );
        (service, conversations, backend)
    }

    fn collecting_sink() -> (AiTurnEventSink, Arc<Mutex<Vec<AiTurnEvent>>>) {
        let events = Arc::new(Mutex::new(Vec::new()));
        let captured = events.clone();
        let sink: AiTurnEventSink = Arc::new(move |event| {
            captured
                .lock()
                .expect("event lock should not be poisoned")
                .push(event.clone());
        });
        (sink, events)
    }

    #[tokio::test]
    async fn first_turn_creates_active_conversation_and_persists_streamed_response() {
        let (service, conversations, _backend) = make_service("success").await;
        let (sink, events) = collecting_sink();
        let response = service
            .run_turn(
                AiTurnRequest::with_turn_id(None, "hello", "turn-success")
                    .expect("request should be valid"),
                AiCancellation::new(),
                Some(sink),
            )
            .await
            .expect("turn should succeed");

        assert_eq!(response.text, "hello world");
        let conversation = conversations
            .active()
            .await
            .expect("active conversation should load")
            .expect("first turn should select a conversation");
        assert_eq!(conversation.messages.len(), 2);
        assert_eq!(conversation.messages[0].role, ConversationMessageRole::User);
        assert_eq!(
            conversation.messages[1].role,
            ConversationMessageRole::Assistant
        );
        assert_eq!(
            conversation.messages[0].turn_id,
            Some("turn-success".to_string())
        );
        assert_eq!(
            conversation.messages[1].turn_id,
            Some("turn-success".to_string())
        );
        assert!(
            conversation
                .messages
                .iter()
                .all(|message| message.status == ConversationMessageStatus::Complete)
        );

        let events = events.lock().expect("event lock should not be poisoned");
        assert_eq!(events.iter().filter(|event| event.is_terminal()).count(), 1);
        assert!(
            events
                .iter()
                .any(|event| matches!(event, AiTurnEvent::Delta { .. }))
        );
        let streamed = events
            .iter()
            .filter_map(|event| match event {
                AiTurnEvent::Delta { text, .. } => Some(text.as_str()),
                _ => None,
            })
            .collect::<String>();
        assert_eq!(streamed, response.text);
    }

    #[tokio::test]
    async fn explicit_conversation_does_not_change_active_conversation() {
        let (service, conversations, _backend) = make_service("explicit").await;
        let active = conversations
            .create(Some("Active".to_string()))
            .await
            .expect("active conversation should be created");
        conversations
            .select_active(active.conversation_id.clone())
            .await
            .expect("active conversation should be selected");
        let explicit = conversations
            .create(Some("Explicit".to_string()))
            .await
            .expect("explicit conversation should be created");

        service
            .run_turn(
                AiTurnRequest::with_turn_id(
                    Some(explicit.conversation_id.clone()),
                    "hello",
                    "turn-explicit",
                )
                .expect("request should be valid"),
                AiCancellation::new(),
                None,
            )
            .await
            .expect("explicit turn should succeed");

        assert_eq!(
            conversations
                .active_conversation_id()
                .await
                .expect("active ID should load"),
            Some(active.conversation_id)
        );
        assert_eq!(
            conversations
                .load(explicit.conversation_id)
                .await
                .expect("explicit conversation should load")
                .messages
                .len(),
            2
        );
    }

    #[tokio::test]
    async fn cancellation_persists_an_interrupted_assistant_message() {
        let (service, conversations, _backend) = make_service("cancel").await;
        let cancellation = AiCancellation::new();
        cancellation.cancel();
        let result = service
            .run_turn(
                AiTurnRequest::with_turn_id(None, "hello", "turn-cancel")
                    .expect("request should be valid"),
                cancellation,
                None,
            )
            .await;
        assert!(matches!(result, Err(AiTurnError::Cancelled)));

        let conversation = conversations
            .active()
            .await
            .expect("conversation should load")
            .expect("conversation should be active");
        assert_eq!(conversation.messages.len(), 2);
        assert_eq!(
            conversation.messages[0].status,
            ConversationMessageStatus::Complete
        );
        assert_eq!(
            conversation.messages[1].status,
            ConversationMessageStatus::Interrupted
        );
        assert!(conversation.messages[1].content.contains("cancelled"));
    }

    #[tokio::test]
    async fn backend_failure_is_persisted_as_an_interrupted_turn() {
        let conversations =
            ConversationService::new(ConversationStore::from_root(temporary_root("failure")));
        let backend = Arc::new(FakeAiBackend::new());
        let service = AiTurnService::new(
            conversations.clone(),
            backend,
            "fake-model",
            AiPromptPolicy::default(),
        );
        let result = service
            .run_turn(
                AiTurnRequest::with_turn_id(None, "hello", "turn-failure")
                    .expect("request should be valid"),
                AiCancellation::new(),
                None,
            )
            .await;
        assert!(matches!(
            result,
            Err(AiTurnError::Backend(AiBackendError::ModelNotLoaded { .. }))
        ));

        let conversation = conversations
            .active()
            .await
            .expect("conversation should load")
            .expect("conversation should be active");
        assert_eq!(conversation.messages.len(), 2);
        assert_eq!(
            conversation.messages[1].status,
            ConversationMessageStatus::Interrupted
        );
        assert!(
            conversation.messages[1]
                .content
                .contains("model_not_loaded")
        );
    }

    #[tokio::test]
    async fn request_id_prevents_duplicate_persistence_on_retry() {
        let (service, conversations, _backend) = make_service("idempotency").await;
        let request = AiTurnRequest::with_turn_id(None, "hello", "turn-retry")
            .expect("request should be valid");
        service
            .run_turn(request.clone(), AiCancellation::new(), None)
            .await
            .expect("first turn should succeed");
        let retry = service.run_turn(request, AiCancellation::new(), None).await;
        assert!(matches!(
            retry,
            Err(AiTurnError::TurnAlreadyRecorded { .. })
        ));
        let conversation = conversations
            .active()
            .await
            .expect("conversation should load")
            .expect("conversation should be active");
        assert_eq!(conversation.messages.len(), 2);
    }

    #[tokio::test]
    async fn retry_after_an_interrupted_process_reuses_the_pending_user_message() {
        let (service, conversations, _backend) = make_service("pending-recovery").await;
        let conversation = conversations
            .create(Some("Recovery".to_string()))
            .await
            .expect("conversation should be created");
        let pending = conversations
            .append_message_with_status_and_turn_id(
                conversation.conversation_id.clone(),
                conversation.revision,
                ConversationMessageRole::User,
                "hello".to_string(),
                ConversationMessageStatus::Pending,
                Some("turn-crashed".to_string()),
            )
            .await
            .expect("pending user message should be persisted");

        let response = service
            .run_turn(
                AiTurnRequest::with_turn_id(
                    Some(pending.conversation_id.clone()),
                    "hello",
                    "new-process-request",
                )
                .expect("retry request should be valid"),
                AiCancellation::new(),
                None,
            )
            .await
            .expect("the pending turn should resume");

        assert_eq!(response.turn_id, "turn-crashed");
        let recovered = conversations
            .load(pending.conversation_id)
            .await
            .expect("recovered conversation should load");
        assert_eq!(recovered.messages.len(), 2);
        assert_eq!(
            recovered.messages[0].status,
            ConversationMessageStatus::Complete
        );
        assert_eq!(
            recovered.messages[1].status,
            ConversationMessageStatus::Complete
        );
    }

    #[test]
    fn prompt_policy_maps_only_persisted_visible_roles() {
        let conversation = Conversation::new(
            "conv-test".to_string(),
            "Test".to_string(),
            "2026-08-28T00:00:00Z".to_string(),
        );
        let messages = AiPromptPolicy::default().messages(&conversation);
        assert_eq!(messages[0].role, AiMessageRole::System);
        assert_eq!(messages.len(), 1);
        assert!(!messages[0].content.contains("chain-of-thought"));
    }
}
