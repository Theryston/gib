use crate::ai::conversation::{
    CONVERSATION_SCHEMA_VERSION, Conversation, ConversationError, ConversationMessage,
    ConversationMessageRole, ConversationMessageStatus, ConversationService,
};
use crate::output::{emit_error, emit_named_event, is_json_mode};
use crate::utils::handle_error;
use clap::ArgMatches;
use dialoguer::Confirm;
use serde::Serialize;
use std::fmt;

const RESPONSE_SCHEMA_VERSION: u32 = 1;
const DEFAULT_SHOW_LIMIT: usize = 128;
const DEFAULT_SHOW_MAX_BYTES: usize = 128 * 1024;
const MAX_SHOW_LIMIT: usize = 4_096;
const MAX_SHOW_BYTES: usize = 1024 * 1024;

/// Run one of the persistent conversation-management operations. This module
/// deliberately talks to ConversationService only; it never opens or edits
/// conversation files itself and never initializes the model runtime.
pub(crate) async fn run(matches: &ArgMatches) {
    match execute(matches).await {
        Ok(response) => render_response(&response),
        Err(error) => report_failure(error),
    }
}

#[derive(Debug)]
enum ConversationCommandError {
    Input(String),
    Conversation(ConversationError),
    Confirmation(String),
}

impl ConversationCommandError {
    fn code(&self) -> &'static str {
        match self {
            Self::Input(_) => "invalid_request",
            Self::Conversation(error) => management_error_code(error),
            Self::Confirmation(_) => "confirmation_failed",
        }
    }

    fn json_message(&self) -> String {
        match self {
            Self::Input(message) | Self::Confirmation(message) => message.clone(),
            Self::Conversation(error) => error.to_string(),
        }
    }
}

impl fmt::Display for ConversationCommandError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Input(message) | Self::Confirmation(message) => formatter.write_str(message),
            Self::Conversation(error) => write!(formatter, "{error}"),
        }
    }
}

impl From<ConversationError> for ConversationCommandError {
    fn from(error: ConversationError) -> Self {
        Self::Conversation(error)
    }
}

#[derive(Debug, Serialize)]
struct ConversationResponse {
    schema_version: u32,
    operation: &'static str,
    #[serde(skip_serializing_if = "Option::is_none")]
    conversation: Option<ConversationDto>,
    #[serde(skip_serializing_if = "Option::is_none")]
    conversations: Option<Vec<ConversationDto>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    messages: Option<Vec<ConversationMessageDto>>,
    active_conversation_id: Option<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    warnings: Vec<ConversationWarningDto>,
    #[serde(skip_serializing_if = "Option::is_none")]
    truncated: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    confirmation_required: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    cancelled: Option<bool>,
}

/// Stable command-facing conversation data. In particular, revision numbers,
/// turn IDs, lock paths, and absolute storage paths are intentionally absent.
#[derive(Debug, Serialize)]
struct ConversationDto {
    conversation_id: String,
    title: String,
    created_at: String,
    updated_at: String,
    schema_version: u32,
    message_count: usize,
    last_role: Option<ConversationMessageRole>,
    active: bool,
    archived: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    model_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    prompt_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    prompt_version: Option<String>,
}

#[derive(Debug, Serialize)]
struct ConversationMessageDto {
    message_id: String,
    role: ConversationMessageRole,
    timestamp: String,
    content: String,
    status: ConversationMessageStatus,
}

#[derive(Debug, Serialize)]
struct ConversationWarningDto {
    conversation_id: String,
    code: String,
    message: String,
}

async fn execute(matches: &ArgMatches) -> Result<ConversationResponse, ConversationCommandError> {
    let Some((operation, operation_matches)) = matches.subcommand() else {
        return Err(ConversationCommandError::Input(
            "Missing conversation operation. Run 'gib ai conversation --help' for more information."
                .to_string(),
        ));
    };

    let service = ConversationService::default_store()?;
    match operation {
        "new" => create_conversation(&service, operation_matches).await,
        "list" => list_conversations(&service).await,
        "select" => select_conversation(&service, operation_matches).await,
        "show" => show_conversation(&service, operation_matches).await,
        "rename" => rename_conversation(&service, operation_matches).await,
        "delete" => delete_conversation(&service, operation_matches).await,
        _ => Err(ConversationCommandError::Input(format!(
            "Unknown conversation operation '{operation}'. Run 'gib ai conversation --help' for more information."
        ))),
    }
}

async fn create_conversation(
    service: &ConversationService,
    matches: &ArgMatches,
) -> Result<ConversationResponse, ConversationCommandError> {
    let title = matches.get_one::<String>("title").cloned();
    let conversation = service.create_and_select(title).await?;
    let active_id = service.active_conversation_id().await?;
    Ok(ConversationResponse {
        schema_version: RESPONSE_SCHEMA_VERSION,
        operation: "new",
        conversation: Some(conversation_dto(&conversation, active_id.as_deref())),
        conversations: None,
        messages: None,
        active_conversation_id: active_id,
        warnings: Vec::new(),
        truncated: None,
        confirmation_required: None,
        cancelled: None,
    })
}

async fn list_conversations(
    service: &ConversationService,
) -> Result<ConversationResponse, ConversationCommandError> {
    let listing = service.list().await?;
    let active_id = service.active_conversation_id().await?;
    let conversations = listing
        .conversations
        .iter()
        .map(|summary| ConversationDto {
            conversation_id: summary.conversation_id.clone(),
            title: summary.title.clone(),
            created_at: summary.created_at.clone(),
            updated_at: summary.updated_at.clone(),
            schema_version: CONVERSATION_SCHEMA_VERSION,
            message_count: summary.message_count,
            last_role: summary.last_role,
            active: active_id.as_deref() == Some(summary.conversation_id.as_str()),
            archived: summary.archived,
            model_id: None,
            prompt_id: None,
            prompt_version: None,
        })
        .collect();
    let warnings = listing
        .warnings
        .iter()
        .map(conversation_warning_dto)
        .collect();

    Ok(ConversationResponse {
        schema_version: RESPONSE_SCHEMA_VERSION,
        operation: "list",
        conversation: None,
        conversations: Some(conversations),
        messages: None,
        active_conversation_id: active_id,
        warnings,
        truncated: None,
        confirmation_required: None,
        cancelled: None,
    })
}

async fn select_conversation(
    service: &ConversationService,
    matches: &ArgMatches,
) -> Result<ConversationResponse, ConversationCommandError> {
    let conversation_id = required_argument(matches, "id")?;
    let conversation = service.select_active(conversation_id).await?;
    let active_id = service.active_conversation_id().await?;
    Ok(ConversationResponse {
        schema_version: RESPONSE_SCHEMA_VERSION,
        operation: "select",
        conversation: Some(conversation_dto(&conversation, active_id.as_deref())),
        conversations: None,
        messages: None,
        active_conversation_id: active_id,
        warnings: Vec::new(),
        truncated: None,
        confirmation_required: None,
        cancelled: None,
    })
}

async fn show_conversation(
    service: &ConversationService,
    matches: &ArgMatches,
) -> Result<ConversationResponse, ConversationCommandError> {
    let conversation_id = required_argument(matches, "id")?;
    let limit = matches
        .get_one::<usize>("limit")
        .copied()
        .unwrap_or(DEFAULT_SHOW_LIMIT);
    let max_bytes = matches
        .get_one::<usize>("max-bytes")
        .copied()
        .unwrap_or(DEFAULT_SHOW_MAX_BYTES);
    validate_show_bounds(limit, max_bytes)?;

    let conversation = service.load(conversation_id).await?;
    let active_id = service.active_conversation_id().await?;
    let (messages, truncated) = bounded_messages(&conversation, limit, max_bytes);
    Ok(ConversationResponse {
        schema_version: RESPONSE_SCHEMA_VERSION,
        operation: "show",
        conversation: Some(conversation_dto(&conversation, active_id.as_deref())),
        conversations: None,
        messages: Some(messages),
        active_conversation_id: active_id,
        warnings: Vec::new(),
        truncated: Some(truncated),
        confirmation_required: None,
        cancelled: None,
    })
}

async fn rename_conversation(
    service: &ConversationService,
    matches: &ArgMatches,
) -> Result<ConversationResponse, ConversationCommandError> {
    let conversation_id = required_argument(matches, "id")?;
    let title = required_argument(matches, "title")?;
    let current = service.load(conversation_id.clone()).await?;
    let conversation = service
        .rename(conversation_id, current.revision, title)
        .await?;
    let active_id = service.active_conversation_id().await?;
    Ok(ConversationResponse {
        schema_version: RESPONSE_SCHEMA_VERSION,
        operation: "rename",
        conversation: Some(conversation_dto(&conversation, active_id.as_deref())),
        conversations: None,
        messages: None,
        active_conversation_id: active_id,
        warnings: Vec::new(),
        truncated: None,
        confirmation_required: None,
        cancelled: None,
    })
}

async fn delete_conversation(
    service: &ConversationService,
    matches: &ArgMatches,
) -> Result<ConversationResponse, ConversationCommandError> {
    let conversation_id = required_argument(matches, "id")?;
    let current = service.load(conversation_id.clone()).await?;
    let active_before = service.active_conversation_id().await?;
    let confirmed = matches.get_flag("yes");

    if is_json_mode() && !confirmed {
        return Ok(ConversationResponse {
            schema_version: RESPONSE_SCHEMA_VERSION,
            operation: "delete",
            conversation: Some(conversation_dto(&current, active_before.as_deref())),
            conversations: None,
            messages: None,
            active_conversation_id: active_before,
            warnings: Vec::new(),
            truncated: None,
            confirmation_required: Some(true),
            cancelled: None,
        });
    }

    if !confirmed
        && !Confirm::new()
            .with_prompt(format!(
                "Delete conversation '{}' ({})?",
                current.title, current.conversation_id
            ))
            .default(false)
            .interact()
            .map_err(|error| {
                ConversationCommandError::Confirmation(format!(
                    "failed to read deletion confirmation: {error}"
                ))
            })?
    {
        return Ok(ConversationResponse {
            schema_version: RESPONSE_SCHEMA_VERSION,
            operation: "delete",
            conversation: Some(conversation_dto(&current, active_before.as_deref())),
            conversations: None,
            messages: None,
            active_conversation_id: active_before,
            warnings: Vec::new(),
            truncated: None,
            confirmation_required: None,
            cancelled: Some(true),
        });
    }

    let deleted = service.delete(conversation_id, None).await?;
    let active_after = service.active_conversation_id().await?;
    Ok(ConversationResponse {
        schema_version: RESPONSE_SCHEMA_VERSION,
        operation: "delete",
        conversation: Some(conversation_dto(&deleted, active_after.as_deref())),
        conversations: None,
        messages: None,
        active_conversation_id: active_after,
        warnings: Vec::new(),
        truncated: None,
        confirmation_required: None,
        cancelled: None,
    })
}

fn required_argument(matches: &ArgMatches, name: &str) -> Result<String, ConversationCommandError> {
    matches.get_one::<String>(name).cloned().ok_or_else(|| {
        ConversationCommandError::Input(format!("Missing required argument: {name}"))
    })
}

fn validate_show_bounds(limit: usize, max_bytes: usize) -> Result<(), ConversationCommandError> {
    if limit == 0 || limit > MAX_SHOW_LIMIT {
        return Err(ConversationCommandError::Input(format!(
            "--limit must be between 1 and {MAX_SHOW_LIMIT}"
        )));
    }
    if max_bytes == 0 || max_bytes > MAX_SHOW_BYTES {
        return Err(ConversationCommandError::Input(format!(
            "--max-bytes must be between 1 and {MAX_SHOW_BYTES}"
        )));
    }
    Ok(())
}

fn conversation_dto(conversation: &Conversation, active_id: Option<&str>) -> ConversationDto {
    ConversationDto {
        conversation_id: conversation.conversation_id.clone(),
        title: conversation.title.clone(),
        created_at: conversation.created_at.clone(),
        updated_at: conversation.updated_at.clone(),
        schema_version: conversation.schema_version,
        message_count: conversation.messages.len(),
        last_role: conversation.messages.last().map(|message| message.role),
        active: active_id == Some(conversation.conversation_id.as_str()),
        archived: conversation.archived,
        model_id: conversation
            .model
            .as_ref()
            .map(|model| model.model_id.clone()),
        prompt_id: conversation.prompt.as_ref().map(|prompt| prompt.id.clone()),
        prompt_version: conversation
            .prompt
            .as_ref()
            .map(|prompt| prompt.version.clone()),
    }
}

fn conversation_warning_dto(
    warning: &crate::ai::conversation::ConversationWarning,
) -> ConversationWarningDto {
    ConversationWarningDto {
        conversation_id: warning.conversation_id.clone(),
        code: management_warning_code(&warning.code).to_string(),
        message: warning.message.clone(),
    }
}

fn bounded_messages(
    conversation: &Conversation,
    limit: usize,
    max_bytes: usize,
) -> (Vec<ConversationMessageDto>, bool) {
    let mut messages = Vec::new();
    let mut serialized_bytes = 0usize;
    let mut truncated = false;

    for message in &conversation.messages {
        if messages.len() >= limit {
            truncated = true;
            break;
        }

        let dto = message_dto(message);
        let message_bytes = serde_json::to_vec(&dto)
            .map(|encoded| encoded.len())
            .unwrap_or(usize::MAX);
        if serialized_bytes.saturating_add(message_bytes) > max_bytes {
            truncated = true;
            break;
        }
        serialized_bytes = serialized_bytes.saturating_add(message_bytes);
        messages.push(dto);
    }

    (messages, truncated)
}

fn message_dto(message: &ConversationMessage) -> ConversationMessageDto {
    ConversationMessageDto {
        message_id: message.message_id.clone(),
        role: message.role,
        timestamp: message.timestamp.clone(),
        content: message.content.clone(),
        status: message.status,
    }
}

fn management_error_code(error: &ConversationError) -> &'static str {
    match error {
        ConversationError::ActiveConversationUnavailable { .. } => "active_selection_conflict",
        ConversationError::FutureSchemaVersion { .. }
        | ConversationError::UnsupportedSchemaVersion { .. } => "newer_schema",
        ConversationError::LockTimeout { .. } | ConversationError::LockLost { .. } => "locked",
        ConversationError::IoError { .. }
        | ConversationError::SerializationError { .. }
        | ConversationError::ConfigUnavailable
        | ConversationError::RegistryPoisoned => "persistence_failure",
        _ => error.code(),
    }
}

fn management_warning_code(code: &str) -> &str {
    match code {
        "future_schema_version" | "unsupported_schema_version" => "newer_schema",
        "lock_timeout" | "lock_lost" => "locked",
        "io_error" | "serialization_error" | "config_unavailable" => "persistence_failure",
        other => other,
    }
}

fn render_response(response: &ConversationResponse) {
    if is_json_mode() {
        emit_named_event("ai_conversation", response);
        return;
    }

    match response.operation {
        "new" => {
            if let Some(conversation) = &response.conversation {
                println!(
                    "Created conversation '{}' ({}) and selected it.",
                    conversation.title, conversation.conversation_id
                );
            }
        }
        "list" => render_list(response),
        "select" => {
            if let Some(conversation) = &response.conversation {
                println!(
                    "Selected conversation '{}' ({}).",
                    conversation.title, conversation.conversation_id
                );
            }
        }
        "show" => render_show(response),
        "rename" => {
            if let Some(conversation) = &response.conversation {
                println!(
                    "Renamed conversation {} to '{}'.",
                    conversation.conversation_id, conversation.title
                );
            }
        }
        "delete" => {
            if response.confirmation_required == Some(true) {
                println!("Confirmation is required before deleting this conversation.");
            } else if response.cancelled == Some(true) {
                println!("Conversation deletion cancelled.");
            } else if let Some(conversation) = &response.conversation {
                println!(
                    "Deleted conversation '{}' ({}).",
                    conversation.title, conversation.conversation_id
                );
            }
        }
        _ => {}
    }

    for warning in &response.warnings {
        eprintln!(
            "Warning [{}] for conversation '{}': {}",
            warning.code, warning.conversation_id, warning.message
        );
    }
}

fn render_list(response: &ConversationResponse) {
    let Some(conversations) = &response.conversations else {
        println!("No conversations found.");
        return;
    };
    if conversations.is_empty() {
        println!("No conversations found.");
        return;
    }
    for conversation in conversations {
        let marker = if conversation.active { "*" } else { " " };
        let last_role = conversation.last_role.map(role_label).unwrap_or("none");
        println!(
            "{marker} {} — {} | {} messages | last: {} | updated: {}",
            conversation.conversation_id,
            conversation.title,
            conversation.message_count,
            last_role,
            conversation.updated_at
        );
    }
}

fn render_show(response: &ConversationResponse) {
    let Some(conversation) = &response.conversation else {
        return;
    };
    println!(
        "Conversation '{}' ({})",
        conversation.title, conversation.conversation_id
    );
    println!(
        "Created: {} | Updated: {} | Messages: {} | Active: {}",
        conversation.created_at,
        conversation.updated_at,
        conversation.message_count,
        conversation.active
    );
    if let Some(messages) = &response.messages {
        for message in messages {
            println!(
                "\n{} [{}] {}",
                role_label(message.role),
                status_label(message.status),
                message.timestamp
            );
            println!("{}", message.content);
        }
    }
    if response.truncated == Some(true) {
        println!("\n[Message output truncated by the requested bounds]");
    }
}

fn role_label(role: ConversationMessageRole) -> &'static str {
    match role {
        ConversationMessageRole::User => "user",
        ConversationMessageRole::Assistant => "assistant",
    }
}

fn status_label(status: ConversationMessageStatus) -> &'static str {
    match status {
        ConversationMessageStatus::Complete => "complete",
        ConversationMessageStatus::Interrupted => "interrupted",
        ConversationMessageStatus::Pending => "pending",
    }
}

fn report_failure(error: ConversationCommandError) -> ! {
    let code = error.code();
    if is_json_mode() {
        let message = error.json_message();
        emit_error(&message, code);
    }
    handle_error(error.to_string(), None)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ai::conversation::ConversationMessage;

    fn command_matches(arguments: &[&str]) -> clap::ArgMatches {
        let mut values = vec!["gib"];
        values.extend(arguments.iter().copied());
        crate::cli()
            .try_get_matches_from(values)
            .expect("arguments should parse")
    }

    #[test]
    fn parser_accepts_every_documented_conversation_operation() {
        for arguments in [
            &["ai", "conversation", "new"][..],
            &["ai", "conversation", "new", "A title"][..],
            &["ai", "conversation", "list"][..],
            &["ai", "conversation", "select", "conv-1"][..],
            &["ai", "conversation", "show", "conv-1"][..],
            &[
                "ai",
                "conversation",
                "show",
                "conv-1",
                "--limit",
                "10",
                "--max-bytes",
                "4096",
            ][..],
            &["ai", "conversation", "rename", "conv-1", "Renamed"][..],
            &["ai", "conversation", "delete", "conv-1"][..],
            &["ai", "conversation", "delete", "conv-1", "--yes"][..],
        ] {
            command_matches(arguments);
        }
    }

    #[test]
    fn parser_rejects_missing_and_extra_arguments_and_json_list_needs_no_message() {
        assert!(
            crate::cli()
                .try_get_matches_from(["gib", "ai", "conversation", "select"])
                .is_err()
        );
        assert!(
            crate::cli()
                .try_get_matches_from(["gib", "ai", "conversation", "rename", "conv-1"])
                .is_err()
        );
        assert!(
            crate::cli()
                .try_get_matches_from([
                    "gib",
                    "ai",
                    "conversation",
                    "select",
                    "conv-1",
                    "unexpected",
                ])
                .is_err()
        );
        assert!(
            crate::cli()
                .try_get_matches_from(["gib", "--mode", "json", "ai", "conversation", "list"])
                .is_ok()
        );
    }

    fn message(
        id: &str,
        role: ConversationMessageRole,
        timestamp: &str,
        content: &str,
        status: ConversationMessageStatus,
    ) -> ConversationMessage {
        ConversationMessage::with_status_and_turn_id(
            id.to_string(),
            role,
            timestamp.to_string(),
            content.to_string(),
            status,
            Some("turn-secret".to_string()),
        )
    }

    #[test]
    fn bounded_messages_keep_chronological_visible_data_and_hide_turn_ids() {
        let mut conversation = Conversation::new(
            "conv-show".to_string(),
            "Show test".to_string(),
            "2026-08-28T12:00:00Z".to_string(),
        );
        conversation.messages = vec![
            message(
                "msg-1",
                ConversationMessageRole::User,
                "2026-08-28T12:00:01Z",
                "first",
                ConversationMessageStatus::Complete,
            ),
            message(
                "msg-2",
                ConversationMessageRole::Assistant,
                "2026-08-28T12:00:02Z",
                "second",
                ConversationMessageStatus::Interrupted,
            ),
        ];
        let (messages, truncated) = bounded_messages(&conversation, 2, 8 * 1024);
        assert!(!truncated);
        assert_eq!(messages[0].message_id, "msg-1");
        assert_eq!(messages[1].role, ConversationMessageRole::Assistant);
        let encoded = serde_json::to_string(&messages).expect("messages should serialize");
        assert!(!encoded.contains("turn-secret"));
        assert!(!encoded.contains("turn_id"));
    }

    #[test]
    fn bounded_messages_report_count_or_byte_truncation() {
        let mut conversation = Conversation::new(
            "conv-limit".to_string(),
            "Limit test".to_string(),
            "2026-08-28T12:00:00Z".to_string(),
        );
        conversation.messages = vec![message(
            "msg-1",
            ConversationMessageRole::User,
            "2026-08-28T12:00:01Z",
            "hello",
            ConversationMessageStatus::Complete,
        )];
        let (messages, truncated) = bounded_messages(&conversation, 1, 1);
        assert!(messages.is_empty());
        assert!(truncated);
        assert!(validate_show_bounds(0, 8 * 1024).is_err());
    }

    #[test]
    fn management_error_codes_are_stable_and_safe() {
        assert_eq!(
            management_error_code(&ConversationError::FutureSchemaVersion {
                id: "conv-test".to_string(),
                version: 99,
            }),
            "newer_schema"
        );
        assert_eq!(
            management_error_code(&ConversationError::LockTimeout {
                scope: "conversation".to_string(),
            }),
            "locked"
        );
        assert_eq!(
            management_error_code(&ConversationError::io("persist conversation")),
            "persistence_failure"
        );
    }
}
