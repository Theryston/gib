use super::error::ConversationError;
use super::paths::validate_conversation_id;
use chrono::{DateTime, FixedOffset, SecondsFormat, Utc};
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, BTreeSet};

pub(crate) const CONVERSATION_SCHEMA_VERSION: u32 = 1;
pub(crate) const DEFAULT_CONVERSATION_TITLE: &str = "New conversation";

/// Resource limits applied both while loading and before persisting a
/// conversation document.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct ConversationLimits {
    pub(crate) max_id_bytes: usize,
    pub(crate) max_title_bytes: usize,
    pub(crate) max_message_bytes: usize,
    pub(crate) max_messages: usize,
    pub(crate) max_context_bytes: usize,
    pub(crate) max_file_bytes: usize,
}

impl Default for ConversationLimits {
    fn default() -> Self {
        Self {
            max_id_bytes: 96,
            max_title_bytes: 256,
            max_message_bytes: 256 * 1024,
            max_messages: 4_096,
            max_context_bytes: 256 * 1024,
            max_file_bytes: 8 * 1024 * 1024,
        }
    }
}

impl ConversationLimits {
    #[cfg(test)]
    pub(crate) fn with_file_limit(mut self, limit: usize) -> Self {
        self.max_file_bytes = limit;
        self
    }

    #[cfg(test)]
    pub(crate) fn with_message_limit(mut self, limit: usize) -> Self {
        self.max_message_bytes = limit;
        self
    }
}

/// A persisted conversation containing only user-visible dialogue and
/// explicit durable context.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub(crate) struct Conversation {
    pub(crate) schema_version: u32,
    pub(crate) conversation_id: String,
    pub(crate) title: String,
    pub(crate) created_at: String,
    pub(crate) updated_at: String,
    pub(crate) revision: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) model: Option<ConversationModelMetadata>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) prompt: Option<ConversationPromptMetadata>,
    #[serde(default)]
    pub(crate) messages: Vec<ConversationMessage>,
    #[serde(default)]
    pub(crate) durable_context: DurableContext,
    #[serde(default, skip_serializing_if = "is_false")]
    pub(crate) archived: bool,
}

impl Conversation {
    pub(crate) fn new(conversation_id: String, title: String, timestamp: String) -> Self {
        Self {
            schema_version: CONVERSATION_SCHEMA_VERSION,
            conversation_id,
            title,
            created_at: timestamp.clone(),
            updated_at: timestamp,
            revision: 0,
            model: None,
            prompt: None,
            messages: Vec::new(),
            durable_context: DurableContext::default(),
            archived: false,
        }
    }

    pub(crate) fn validate(&self, limits: ConversationLimits) -> Result<(), ConversationError> {
        if self.schema_version != CONVERSATION_SCHEMA_VERSION {
            return Err(ConversationError::UnsupportedSchemaVersion {
                id: self.conversation_id.clone(),
                version: self.schema_version,
            });
        }
        validate_conversation_id_with_limit(&self.conversation_id, limits.max_id_bytes)?;
        validate_title(&self.title, limits.max_title_bytes)?;
        let created_at = parse_utc_timestamp(&self.created_at)?;
        let updated_at = parse_utc_timestamp(&self.updated_at)?;
        if updated_at < created_at {
            return Err(ConversationError::InvalidTimestamp);
        }

        if self.messages.len() > limits.max_messages {
            return Err(ConversationError::TooManyMessages {
                limit: limits.max_messages,
            });
        }
        let mut message_ids = BTreeSet::new();
        for message in &self.messages {
            message.validate(limits)?;
            if !message_ids.insert(message.message_id.as_str()) {
                return Err(ConversationError::DuplicateMessageId);
            }
        }

        self.durable_context.validate(limits)?;
        if let Some(model) = &self.model {
            validate_metadata_id(&model.model_id, limits.max_id_bytes)?;
        }
        if let Some(prompt) = &self.prompt {
            validate_metadata_id(&prompt.id, limits.max_id_bytes)?;
            if prompt.version.is_empty()
                || prompt.version.len() > limits.max_id_bytes
                || prompt.version.contains('\0')
            {
                return Err(ConversationError::InvalidMessage);
            }
        }
        Ok(())
    }

    pub(crate) fn summary(&self) -> ConversationSummary {
        ConversationSummary {
            conversation_id: self.conversation_id.clone(),
            title: self.title.clone(),
            created_at: self.created_at.clone(),
            updated_at: self.updated_at.clone(),
            revision: self.revision,
            message_count: self.messages.len(),
            archived: self.archived,
        }
    }
}

/// Optional model metadata retained with a conversation without storing a
/// prompt or runtime payload.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub(crate) struct ConversationModelMetadata {
    pub(crate) model_id: String,
}

/// Optional prompt identity retained with a conversation.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub(crate) struct ConversationPromptMetadata {
    pub(crate) id: String,
    pub(crate) version: String,
}

/// A user-visible message. There is intentionally no system, tool, or hidden
/// reasoning role in the persisted conversation contract.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub(crate) struct ConversationMessage {
    pub(crate) message_id: String,
    pub(crate) role: ConversationMessageRole,
    pub(crate) timestamp: String,
    pub(crate) content: String,
    pub(crate) status: ConversationMessageStatus,
    /// An opaque, non-user-visible turn identifier used to make a turn
    /// append idempotent across retries. It is never sent to the model.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) turn_id: Option<String>,
}

impl ConversationMessage {
    pub(crate) fn new(
        message_id: String,
        role: ConversationMessageRole,
        timestamp: String,
        content: String,
    ) -> Self {
        Self {
            message_id,
            role,
            timestamp,
            content,
            status: ConversationMessageStatus::Complete,
            turn_id: None,
        }
    }

    pub(crate) fn with_status_and_turn_id(
        message_id: String,
        role: ConversationMessageRole,
        timestamp: String,
        content: String,
        status: ConversationMessageStatus,
        turn_id: Option<String>,
    ) -> Self {
        Self {
            message_id,
            role,
            timestamp,
            content,
            status,
            turn_id,
        }
    }

    fn validate(&self, limits: ConversationLimits) -> Result<(), ConversationError> {
        validate_conversation_id_with_limit(&self.message_id, limits.max_id_bytes)?;
        parse_utc_timestamp(&self.timestamp)?;
        if self.content.is_empty() || self.content.contains('\0') {
            return Err(ConversationError::InvalidMessage);
        }
        if self.content.len() > limits.max_message_bytes {
            return Err(ConversationError::MessageTooLarge {
                limit: limits.max_message_bytes,
                actual: self.content.len(),
            });
        }
        if let Some(turn_id) = &self.turn_id {
            validate_conversation_id_with_limit(turn_id, limits.max_id_bytes)?;
        }
        if self.status == ConversationMessageStatus::Pending
            && self.role != ConversationMessageRole::User
        {
            return Err(ConversationError::InvalidMessage);
        }
        Ok(())
    }
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub(crate) enum ConversationMessageRole {
    User,
    Assistant,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub(crate) enum ConversationMessageStatus {
    Complete,
    Interrupted,
    /// A user message whose generation has not reached a durable terminal
    /// state yet. It allows the next process invocation to resume the same
    /// turn instead of appending the user's message a second time.
    Pending,
}

/// Explicit context that may survive turns. It has no field for raw tools,
/// hidden reasoning, prompt expansions, or native runtime diagnostics.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub(crate) struct DurableContext {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) summary: Option<String>,
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub(crate) user_preferences: BTreeMap<String, String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub(crate) artifact_refs: Vec<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub(crate) evidence_refs: Vec<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub(crate) facts: Vec<String>,
}

impl DurableContext {
    fn validate(&self, limits: ConversationLimits) -> Result<(), ConversationError> {
        let encoded = serde_json::to_vec(self)
            .map_err(|_| ConversationError::serialization("encode durable context"))?;
        if encoded.len() > limits.max_context_bytes {
            return Err(ConversationError::ContextTooLarge {
                limit: limits.max_context_bytes,
                actual: encoded.len(),
            });
        }

        if self
            .summary
            .as_deref()
            .is_some_and(|value| value.is_empty() || value.contains('\0'))
        {
            return Err(ConversationError::InvalidMessage);
        }
        for (key, value) in &self.user_preferences {
            if key.is_empty() || value.is_empty() || key.contains('\0') || value.contains('\0') {
                return Err(ConversationError::InvalidMessage);
            }
        }
        for value in self
            .artifact_refs
            .iter()
            .chain(self.evidence_refs.iter())
            .chain(self.facts.iter())
        {
            if value.is_empty() || value.contains('\0') {
                return Err(ConversationError::InvalidMessage);
            }
        }
        Ok(())
    }
}

/// The bounded data needed for deterministic conversation listings.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub(crate) struct ConversationSummary {
    pub(crate) conversation_id: String,
    pub(crate) title: String,
    pub(crate) created_at: String,
    pub(crate) updated_at: String,
    pub(crate) revision: u64,
    pub(crate) message_count: usize,
    pub(crate) archived: bool,
}

/// A structured warning returned when one conversation file cannot be listed.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub(crate) struct ConversationWarning {
    pub(crate) conversation_id: String,
    pub(crate) code: String,
    pub(crate) message: String,
}

/// A listing can contain valid summaries and warnings for isolated bad files.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub(crate) struct ConversationList {
    pub(crate) conversations: Vec<ConversationSummary>,
    pub(crate) warnings: Vec<ConversationWarning>,
}

pub(crate) fn current_timestamp() -> String {
    Utc::now().to_rfc3339_opts(SecondsFormat::Nanos, true)
}

fn parse_utc_timestamp(value: &str) -> Result<DateTime<FixedOffset>, ConversationError> {
    let parsed =
        DateTime::parse_from_rfc3339(value).map_err(|_| ConversationError::InvalidTimestamp)?;
    if parsed.offset().local_minus_utc() != 0 {
        return Err(ConversationError::InvalidTimestamp);
    }
    Ok(parsed)
}

fn validate_title(value: &str, max_bytes: usize) -> Result<(), ConversationError> {
    if value.trim().is_empty() || value.chars().any(char::is_control) {
        return Err(ConversationError::InvalidTitle);
    }
    if value.len() > max_bytes {
        return Err(ConversationError::TitleTooLarge {
            limit: max_bytes,
            actual: value.len(),
        });
    }
    Ok(())
}

fn validate_metadata_id(value: &str, max_bytes: usize) -> Result<(), ConversationError> {
    if value.is_empty()
        || value.len() > max_bytes
        || value.contains('\0')
        || value.chars().any(char::is_control)
    {
        return Err(ConversationError::InvalidMessage);
    }
    Ok(())
}

fn validate_conversation_id_with_limit(
    value: &str,
    max_bytes: usize,
) -> Result<(), ConversationError> {
    if value.len() > max_bytes {
        return Err(ConversationError::InvalidConversationId);
    }
    validate_conversation_id(value)
}

fn is_false(value: &bool) -> bool {
    !value
}

#[cfg(test)]
mod tests {
    use super::*;

    fn conversation() -> Conversation {
        Conversation::new(
            "conv-test".to_string(),
            "A test conversation".to_string(),
            "2026-08-28T12:00:00Z".to_string(),
        )
    }

    #[test]
    fn new_conversations_are_current_and_user_visible_only() {
        let conversation = conversation();
        conversation
            .validate(ConversationLimits::default())
            .expect("new conversation should validate");
        assert!(conversation.messages.is_empty());
        assert!(conversation.durable_context.summary.is_none());
        let encoded = serde_json::to_string(&conversation).expect("conversation should encode");
        assert!(!encoded.contains("tool"));
        assert!(!encoded.contains("reasoning"));
    }

    #[test]
    fn message_roles_are_limited_to_user_visible_values() {
        let encoded =
            serde_json::to_string(&ConversationMessageRole::Assistant).expect("role should encode");
        assert_eq!(encoded, "\"assistant\"");
        assert!(serde_json::from_str::<ConversationMessageRole>("\"tool\"").is_err());
    }

    #[test]
    fn utc_timestamps_and_limits_are_enforced() {
        let mut value = conversation();
        value.title = "x".repeat(20);
        assert!(matches!(
            value.validate(ConversationLimits {
                max_title_bytes: 4,
                ..ConversationLimits::default()
            }),
            Err(ConversationError::TitleTooLarge { .. })
        ));
        value.title = "valid".to_string();
        value.created_at = "2026-08-28T12:00:00-01:00".to_string();
        assert!(matches!(
            value.validate(ConversationLimits::default()),
            Err(ConversationError::InvalidTimestamp)
        ));
    }
}
