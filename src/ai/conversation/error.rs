use serde::{Deserialize, Serialize};
use std::fmt;

/// Errors returned by conversation storage and active-conversation state.
///
/// Error variants intentionally avoid embedding file contents, full paths, or
/// parser diagnostics. Callers can expose `code()` in JSON mode while keeping
/// normal diagnostics safe for terminal output.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(tag = "code", content = "details", rename_all = "snake_case")]
pub(crate) enum ConversationError {
    MissingHomeDirectory,
    InvalidConversationId,
    ConversationNotFound {
        id: String,
    },
    ConversationAlreadyExists {
        id: String,
    },
    MalformedConversation {
        id: String,
    },
    FutureSchemaVersion {
        id: String,
        version: u32,
    },
    UnsupportedSchemaVersion {
        id: String,
        version: u32,
    },
    RevisionConflict {
        id: String,
        expected: u64,
        actual: u64,
    },
    RevisionOverflow,
    InvalidTitle,
    TitleTooLarge {
        limit: usize,
        actual: usize,
    },
    InvalidMessage,
    MessageTooLarge {
        limit: usize,
        actual: usize,
    },
    TooManyMessages {
        limit: usize,
    },
    ContextTooLarge {
        limit: usize,
        actual: usize,
    },
    ConversationTooLarge {
        limit: usize,
        actual: usize,
    },
    InvalidTimestamp,
    DuplicateMessageId,
    NoActiveConversation,
    ActiveConversationUnavailable {
        id: String,
    },
    LockTimeout {
        scope: String,
    },
    LockLost {
        scope: String,
    },
    UnsafePath,
    IoError {
        operation: String,
    },
    SerializationError {
        operation: String,
    },
    ConfigUnavailable,
    RegistryPoisoned,
}

impl ConversationError {
    pub(crate) fn code(&self) -> &'static str {
        match self {
            Self::MissingHomeDirectory => "missing_home_directory",
            Self::InvalidConversationId => "invalid_conversation_id",
            Self::ConversationNotFound { .. } => "conversation_not_found",
            Self::ConversationAlreadyExists { .. } => "conversation_already_exists",
            Self::MalformedConversation { .. } => "malformed_conversation",
            Self::FutureSchemaVersion { .. } => "future_schema_version",
            Self::UnsupportedSchemaVersion { .. } => "unsupported_schema_version",
            Self::RevisionConflict { .. } => "revision_conflict",
            Self::RevisionOverflow => "revision_overflow",
            Self::InvalidTitle => "invalid_title",
            Self::TitleTooLarge { .. } => "title_too_large",
            Self::InvalidMessage => "invalid_message",
            Self::MessageTooLarge { .. } => "message_too_large",
            Self::TooManyMessages { .. } => "too_many_messages",
            Self::ContextTooLarge { .. } => "context_too_large",
            Self::ConversationTooLarge { .. } => "conversation_too_large",
            Self::InvalidTimestamp => "invalid_timestamp",
            Self::DuplicateMessageId => "duplicate_message_id",
            Self::NoActiveConversation => "no_active_conversation",
            Self::ActiveConversationUnavailable { .. } => "active_conversation_unavailable",
            Self::LockTimeout { .. } => "lock_timeout",
            Self::LockLost { .. } => "lock_lost",
            Self::UnsafePath => "unsafe_path",
            Self::IoError { .. } => "io_error",
            Self::SerializationError { .. } => "serialization_error",
            Self::ConfigUnavailable => "config_unavailable",
            Self::RegistryPoisoned => "registry_poisoned",
        }
    }

    pub(crate) fn io(operation: impl Into<String>) -> Self {
        Self::IoError {
            operation: operation.into(),
        }
    }

    pub(crate) fn serialization(operation: impl Into<String>) -> Self {
        Self::SerializationError {
            operation: operation.into(),
        }
    }
}

impl fmt::Display for ConversationError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::MissingHomeDirectory => {
                formatter.write_str("the user home directory could not be determined")
            }
            Self::InvalidConversationId => formatter.write_str(
                "conversation ID must be a safe opaque path component without separators",
            ),
            Self::ConversationNotFound { id } => {
                write!(formatter, "conversation '{id}' was not found")
            }
            Self::ConversationAlreadyExists { id } => {
                write!(formatter, "conversation '{id}' already exists")
            }
            Self::MalformedConversation { id } => {
                write!(
                    formatter,
                    "conversation '{id}' is malformed or cannot be decoded"
                )
            }
            Self::FutureSchemaVersion { id, version } => write!(
                formatter,
                "conversation '{id}' uses schema version {version}, which is newer than this binary"
            ),
            Self::UnsupportedSchemaVersion { id, version } => write!(
                formatter,
                "conversation '{id}' uses unsupported schema version {version}"
            ),
            Self::RevisionConflict {
                id,
                expected,
                actual,
            } => write!(
                formatter,
                "conversation '{id}' changed from expected revision {expected} to revision {actual}"
            ),
            Self::RevisionOverflow => {
                formatter.write_str("conversation revision cannot be incremented")
            }
            Self::InvalidTitle => formatter.write_str("conversation title cannot be empty"),
            Self::TitleTooLarge { limit, actual } => write!(
                formatter,
                "conversation title is {actual} bytes; the limit is {limit} bytes"
            ),
            Self::InvalidMessage => formatter
                .write_str("conversation messages must contain visible user or assistant text"),
            Self::MessageTooLarge { limit, actual } => write!(
                formatter,
                "conversation message is {actual} bytes; the limit is {limit} bytes"
            ),
            Self::TooManyMessages { limit } => write!(
                formatter,
                "conversation exceeds the limit of {limit} messages"
            ),
            Self::ContextTooLarge { limit, actual } => write!(
                formatter,
                "durable conversation context is {actual} bytes; the limit is {limit} bytes"
            ),
            Self::ConversationTooLarge { limit, actual } => write!(
                formatter,
                "conversation document is {actual} bytes; the limit is {limit} bytes"
            ),
            Self::InvalidTimestamp => {
                formatter.write_str("conversation timestamps must be valid UTC RFC 3339 values")
            }
            Self::DuplicateMessageId => {
                formatter.write_str("conversation message IDs must be unique")
            }
            Self::NoActiveConversation => formatter.write_str("no active conversation is selected"),
            Self::ActiveConversationUnavailable { id } => write!(
                formatter,
                "the configured active conversation '{id}' is missing or malformed"
            ),
            Self::LockTimeout { scope } => {
                write!(formatter, "timed out waiting for the {scope} lock")
            }
            Self::LockLost { scope } => write!(formatter, "the {scope} lock was lost"),
            Self::UnsafePath => formatter.write_str("refusing to use an unsafe conversation path"),
            Self::IoError { operation } => {
                write!(formatter, "conversation storage failed to {operation}")
            }
            Self::SerializationError { operation } => {
                write!(formatter, "conversation storage failed to {operation}")
            }
            Self::ConfigUnavailable => {
                formatter.write_str("AI conversation configuration is unavailable")
            }
            Self::RegistryPoisoned => formatter.write_str("conversation storage is unavailable"),
        }
    }
}

impl std::error::Error for ConversationError {}
