use super::ai::run_turn_for_interactive;
use crate::ai::conversation::{
    Conversation, ConversationMessageRole, ConversationMessageStatus, ConversationService,
};
use crate::ai::profiles::RuntimeConfig;
use crate::ai::runtime::AiUsage;
use crate::ai::{AiCancellation, AiTurnError, AiTurnEvent, AiTurnEventSink, AiTurnResponse};
use crossterm::event::{self, Event, KeyCode, KeyEvent, KeyEventKind, KeyModifiers};
use crossterm::style::{Color, Print, ResetColor, SetForegroundColor};
use crossterm::terminal::{
    self, Clear, ClearType, EnterAlternateScreen, LeaveAlternateScreen, disable_raw_mode,
    enable_raw_mode,
};
use crossterm::{cursor, execute, queue};
use std::io::{self, Stdout, Write};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::thread;
use std::time::Duration;
use tokio::sync::mpsc::{self, UnboundedReceiver, UnboundedSender};
use tokio::task::JoinHandle;
use tokio::time::{self, MissedTickBehavior};

const DEFAULT_TERMINAL_WIDTH: u16 = 80;
const DEFAULT_TERMINAL_HEIGHT: u16 = 24;
const RENDER_TICK: Duration = Duration::from_millis(120);
const TERMINAL_POLL_INTERVAL: Duration = Duration::from_millis(50);
const MAX_TRANSCRIPT_BLOCKS: usize = 512;
const MAX_TRANSCRIPT_BLOCK_BYTES: usize = 64 * 1024;
const MAX_COMPOSER_BYTES: usize = 64 * 1024;
const MAX_HISTORY_ENTRIES: usize = 50;
const TRANSCRIPT_CLIP_MARKER: &str = "… [message clipped in viewport]";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct SlashSuggestion {
    name: &'static str,
    description: &'static str,
    accepts_arguments: bool,
    requires_arguments: bool,
}

const SLASH_SUGGESTIONS: &[SlashSuggestion] = &[
    SlashSuggestion {
        name: "help",
        description: "show available commands",
        accepts_arguments: false,
        requires_arguments: false,
    },
    SlashSuggestion {
        name: "new",
        description: "create and select a conversation",
        accepts_arguments: true,
        requires_arguments: false,
    },
    SlashSuggestion {
        name: "list",
        description: "list conversations",
        accepts_arguments: false,
        requires_arguments: false,
    },
    SlashSuggestion {
        name: "select",
        description: "select a conversation by ID",
        accepts_arguments: true,
        requires_arguments: true,
    },
    SlashSuggestion {
        name: "switch",
        description: "select a conversation by ID",
        accepts_arguments: true,
        requires_arguments: true,
    },
    SlashSuggestion {
        name: "rename",
        description: "rename a conversation",
        accepts_arguments: true,
        requires_arguments: true,
    },
    SlashSuggestion {
        name: "clear",
        description: "clear the visible transcript",
        accepts_arguments: false,
        requires_arguments: false,
    },
    SlashSuggestion {
        name: "status",
        description: "show runtime and generation status",
        accepts_arguments: false,
        requires_arguments: false,
    },
    SlashSuggestion {
        name: "exit",
        description: "leave the interactive frontend",
        accepts_arguments: false,
        requires_arguments: false,
    },
];

/// Input accepted by the future restore SafetyGate. The current chat UI does
/// not approve restore actions; it only owns the lifecycle-safe abstraction.
#[derive(Debug, Clone, PartialEq, Eq)]
#[allow(dead_code)]
pub(crate) enum ConfirmationRiskLevel {
    Low,
    Medium,
    High,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ConfirmationRequest {
    pub(crate) action_id: String,
    pub(crate) summary: String,
    pub(crate) risk_level: ConfirmationRiskLevel,
    pub(crate) affected_paths: Vec<String>,
    pub(crate) affected_count: usize,
    pub(crate) plan_id: Option<String>,
    pub(crate) expires_at: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[allow(dead_code)]
pub(crate) enum ConfirmationResult {
    Approved,
    Rejected,
    Expired,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct Composer {
    text: String,
    cursor: usize,
    preferred_column: Option<usize>,
}

impl Composer {
    fn new() -> Self {
        Self {
            text: String::new(),
            cursor: 0,
            preferred_column: None,
        }
    }

    fn is_empty(&self) -> bool {
        self.text.is_empty()
    }

    #[allow(dead_code)]
    fn is_blank(&self) -> bool {
        self.text.trim().is_empty()
    }

    #[allow(dead_code)]
    fn text(&self) -> &str {
        &self.text
    }

    fn clear(&mut self) {
        self.text.clear();
        self.cursor = 0;
        self.preferred_column = None;
    }

    fn take(&mut self) -> String {
        let text = std::mem::take(&mut self.text);
        self.cursor = 0;
        self.preferred_column = None;
        text
    }

    fn set_text(&mut self, text: String) {
        self.text = text;
        self.cursor = self.text.len();
        self.preferred_column = None;
    }

    fn insert_char(&mut self, character: char) {
        if self.text.len().saturating_add(character.len_utf8()) > MAX_COMPOSER_BYTES {
            return;
        }
        self.text.insert(self.cursor, character);
        self.cursor += character.len_utf8();
        self.preferred_column = None;
    }

    fn insert_text(&mut self, text: &str) {
        for character in text.chars() {
            match character {
                '\r' => {}
                character if character == '\n' || !character.is_control() => {
                    self.insert_char(character)
                }
                _ => {}
            }
        }
    }

    fn backspace(&mut self) {
        if self.cursor == 0 {
            return;
        }
        let previous = previous_boundary(&self.text, self.cursor);
        self.text.drain(previous..self.cursor);
        self.cursor = previous;
        self.preferred_column = None;
    }

    fn delete_forward(&mut self) {
        if self.cursor >= self.text.len() {
            return;
        }
        let next = next_boundary(&self.text, self.cursor);
        self.text.drain(self.cursor..next);
        self.preferred_column = None;
    }

    fn move_left(&mut self) {
        self.cursor = previous_boundary(&self.text, self.cursor);
        self.preferred_column = None;
    }

    fn move_right(&mut self) {
        self.cursor = next_boundary(&self.text, self.cursor);
        self.preferred_column = None;
    }

    fn move_home(&mut self) {
        self.cursor = current_line_start(&self.text, self.cursor);
        self.preferred_column = None;
    }

    fn move_end(&mut self) {
        self.cursor = current_line_end(&self.text, self.cursor);
        self.preferred_column = None;
    }

    fn move_up(&mut self) {
        let line_start = current_line_start(&self.text, self.cursor);
        if line_start == 0 {
            return;
        }
        let current_column = self
            .preferred_column
            .unwrap_or_else(|| self.text[line_start..self.cursor].chars().count());
        let previous_line_end = line_start - 1;
        let previous_line_start = current_line_start(&self.text, previous_line_end);
        self.cursor = line_column_cursor(
            &self.text,
            previous_line_start,
            previous_line_end,
            current_column,
        );
        self.preferred_column = Some(current_column);
    }

    fn move_down(&mut self) {
        let line_end = current_line_end(&self.text, self.cursor);
        if line_end >= self.text.len() {
            return;
        }
        let current_line_start = current_line_start(&self.text, self.cursor);
        let current_column = self
            .preferred_column
            .unwrap_or_else(|| self.text[current_line_start..self.cursor].chars().count());
        let next_line_start = line_end + 1;
        let next_line_end = current_line_end(&self.text, next_line_start);
        self.cursor =
            line_column_cursor(&self.text, next_line_start, next_line_end, current_column);
        self.preferred_column = Some(current_column);
    }

    fn display_text_with_cursor(&self) -> String {
        let mut rendered = String::with_capacity(self.text.len() + 3);
        rendered.push_str(&self.text[..self.cursor]);
        rendered.push('▌');
        rendered.push_str(&self.text[self.cursor..]);
        rendered
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[allow(dead_code)]
enum BlockRole {
    User,
    Assistant,
    System,
    Error,
    Activity,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum BlockStatus {
    Complete,
    Streaming,
    Interrupted,
    Error,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct TranscriptBlock {
    message_id: Option<String>,
    role: BlockRole,
    status: BlockStatus,
    text: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum RuntimeState {
    Ready,
    Generating,
    Cancelling,
    Error,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct ActiveTurn {
    conversation_id: String,
    turn_id: String,
    user_message_id: String,
    assistant_block: usize,
}

/// Explicit state for the interactive frontend. Rendering is derived from
/// this state; model events never write arbitrary cursor sequences directly.
#[derive(Debug)]
pub(crate) struct AiInteractiveApp {
    conversation_id: Option<String>,
    conversation_title: String,
    model_id: String,
    runtime_summary: String,
    runtime_state: RuntimeState,
    transcript: Vec<TranscriptBlock>,
    viewport_scroll: usize,
    follow_newest: bool,
    composer: Composer,
    history: Vec<String>,
    history_cursor: Option<usize>,
    active_turn: Option<ActiveTurn>,
    usage: Option<AiUsage>,
    status_message: Option<String>,
    error_banner: Option<String>,
    spinner_index: usize,
    width: u16,
    height: u16,
    should_exit: bool,
    confirmation: Option<ConfirmationRequest>,
    confirmation_result: Option<ConfirmationResult>,
    pending_user_block: Option<usize>,
    slash_selection: usize,
}

impl AiInteractiveApp {
    fn new(model_id: String, conversation_id: Option<String>, width: u16, height: u16) -> Self {
        Self {
            conversation_id,
            conversation_title: "No active conversation".to_string(),
            model_id,
            runtime_summary: String::new(),
            runtime_state: RuntimeState::Ready,
            transcript: Vec::new(),
            viewport_scroll: 0,
            follow_newest: true,
            composer: Composer::new(),
            history: Vec::new(),
            history_cursor: None,
            active_turn: None,
            usage: None,
            status_message: None,
            error_banner: None,
            spinner_index: 0,
            width: width.max(1),
            height: height.max(1),
            should_exit: false,
            confirmation: None,
            confirmation_result: None,
            pending_user_block: None,
            slash_selection: 0,
        }
    }

    fn set_runtime_config(&mut self, runtime_config: &RuntimeConfig) {
        self.runtime_summary = runtime_config.summary();
        if let Some(notice) = runtime_config.short_notice() {
            self.status_message = Some(notice);
        }
    }

    fn load_conversation(&mut self, conversation: &Conversation) {
        self.conversation_id = Some(conversation.conversation_id.clone());
        self.conversation_title = conversation.title.clone();
        self.transcript.clear();
        self.active_turn = None;
        self.pending_user_block = None;
        self.slash_selection = 0;
        self.viewport_scroll = 0;
        self.follow_newest = true;
        for message in &conversation.messages {
            self.push_block(TranscriptBlock {
                message_id: Some(message.message_id.clone()),
                role: block_role(message.role),
                status: block_status(message.status),
                text: bounded_block_text(&message.content),
            });
        }
    }

    #[allow(dead_code)]
    fn request_confirmation(&mut self, request: ConfirmationRequest) {
        self.confirmation = Some(request);
        self.confirmation_result = None;
        self.status_message = Some("Confirmation required: y approve, n reject".to_string());
    }

    #[allow(dead_code)]
    fn take_confirmation_result(&mut self) -> Option<ConfirmationResult> {
        self.confirmation_result.take()
    }

    fn submit_message(&mut self, message: String) -> bool {
        if message.trim().is_empty() {
            self.status_message = Some("Type a message before pressing Enter.".to_string());
            return false;
        }
        if self.runtime_state == RuntimeState::Generating
            || self.runtime_state == RuntimeState::Cancelling
        {
            self.status_message =
                Some("Generation is active; press Ctrl+C to cancel it.".to_string());
            return false;
        }
        if self.history.last() != Some(&message) {
            self.history.push(message.clone());
            if self.history.len() > MAX_HISTORY_ENTRIES {
                self.history.remove(0);
            }
        }
        self.history_cursor = None;
        self.slash_selection = 0;
        self.composer.clear();
        self.error_banner = None;
        self.status_message = Some("Sending message...".to_string());
        self.runtime_state = RuntimeState::Generating;
        let block_index = self.push_block(TranscriptBlock {
            message_id: None,
            role: BlockRole::User,
            status: BlockStatus::Complete,
            text: bounded_block_text(&message),
        });
        self.pending_user_block = Some(block_index);
        true
    }

    fn apply_turn_event(&mut self, event: &AiTurnEvent) {
        match event {
            AiTurnEvent::Started {
                conversation_id,
                turn_id,
                user_message_id,
                ..
            } => {
                self.conversation_id = Some(conversation_id.clone());
                if let Some(index) = self.pending_user_block.take()
                    && let Some(block) = self.transcript.get_mut(index)
                {
                    block.message_id = Some(user_message_id.clone());
                }
                let assistant_block = self.push_block(TranscriptBlock {
                    message_id: None,
                    role: BlockRole::Assistant,
                    status: BlockStatus::Streaming,
                    text: String::new(),
                });
                self.active_turn = Some(ActiveTurn {
                    conversation_id: conversation_id.clone(),
                    turn_id: turn_id.clone(),
                    user_message_id: user_message_id.clone(),
                    assistant_block,
                });
                self.runtime_state = RuntimeState::Generating;
                self.status_message = Some("Generating response...".to_string());
                self.error_banner = None;
            }
            AiTurnEvent::Progress {
                conversation_id,
                turn_id,
                usage,
            } => {
                if self.matches_active_turn(conversation_id, turn_id) {
                    self.usage = Some(*usage);
                }
            }
            AiTurnEvent::Delta {
                conversation_id,
                turn_id,
                text,
            } => {
                if self.matches_active_turn(conversation_id, turn_id)
                    && let Some(active) = &self.active_turn
                    && let Some(block) = self.transcript.get_mut(active.assistant_block)
                {
                    append_bounded_text(&mut block.text, text);
                    self.status_message = Some("Generating response...".to_string());
                }
            }
            AiTurnEvent::Finished { response } => self.finish_response(response),
            AiTurnEvent::Cancelled {
                conversation_id,
                turn_id,
                partial_text,
                usage,
            } => {
                if self.matches_active_turn(conversation_id, turn_id) {
                    if let Some(active) = self.active_turn.take()
                        && let Some(block) = self.transcript.get_mut(active.assistant_block)
                    {
                        block.status = BlockStatus::Interrupted;
                        block.text = bounded_block_text(if partial_text.is_empty() {
                            "[AI response cancelled]"
                        } else {
                            partial_text
                        });
                    }
                    self.usage = Some(*usage);
                    self.runtime_state = RuntimeState::Ready;
                    self.status_message =
                        Some("Response cancelled; the interrupted turn was saved.".to_string());
                }
            }
            AiTurnEvent::Failed {
                conversation_id,
                turn_id,
                error,
            } => {
                if self.matches_active_turn(conversation_id, turn_id) {
                    self.fail_active_turn(error);
                }
            }
        }
    }

    fn finish_response(&mut self, response: &AiTurnResponse) {
        if let Some(active) = &self.active_turn
            && active.conversation_id == response.conversation_id
            && active.turn_id == response.turn_id
        {
            let assistant_block = active.assistant_block;
            if let Some(block) = self.transcript.get_mut(assistant_block) {
                block.message_id = Some(response.assistant_message_id.clone());
                block.status = BlockStatus::Complete;
                // Replace rather than append: streamed deltas already occupy
                // this block and must never be duplicated by the final event.
                block.text = bounded_block_text(&response.text);
            }
            self.usage = Some(response.usage);
            self.active_turn = None;
            self.runtime_state = RuntimeState::Ready;
            self.status_message = Some("Ready".to_string());
        }
    }

    fn fail_active_turn(&mut self, error: &AiTurnError) {
        let message = error.to_string();
        if let Some(active) = self.active_turn.take()
            && let Some(block) = self.transcript.get_mut(active.assistant_block)
        {
            block.status = BlockStatus::Error;
            if block.text.is_empty() {
                block.text = bounded_block_text("[AI response unavailable]");
            }
        }
        self.runtime_state = RuntimeState::Error;
        self.error_banner = Some(message.clone());
        self.status_message = Some("The turn failed; you can try again.".to_string());
        self.push_block(TranscriptBlock {
            message_id: None,
            role: BlockRole::Error,
            status: BlockStatus::Error,
            text: message,
        });
    }

    fn fail_before_started_turn(&mut self, error: &AiTurnError) {
        let message = error.to_string();
        if let Some(index) = self.pending_user_block.take()
            && index < self.transcript.len()
        {
            // The service may have failed before emitting Started. Remove the
            // optimistic local block so a retry that resumes a persisted
            // pending turn does not render the same user message twice.
            self.transcript.remove(index);
        }
        self.runtime_state = RuntimeState::Error;
        self.error_banner = Some(message.clone());
        self.status_message = Some("The turn failed; you can try again.".to_string());
        self.push_block(TranscriptBlock {
            message_id: None,
            role: BlockRole::Error,
            status: BlockStatus::Error,
            text: message,
        });
    }

    fn matches_active_turn(&self, conversation_id: &str, turn_id: &str) -> bool {
        self.active_turn.as_ref().is_some_and(|active| {
            active.conversation_id == conversation_id && active.turn_id == turn_id
        })
    }

    fn on_resize(&mut self, width: u16, height: u16) {
        self.width = width.max(1);
        self.height = height.max(1);
    }

    fn tick(&mut self) {
        self.spinner_index = self.spinner_index.wrapping_add(1);
    }

    fn scroll_up(&mut self) {
        self.viewport_scroll = self
            .viewport_scroll
            .saturating_add(self.body_height() / 2)
            .max(1);
        self.follow_newest = false;
    }

    fn scroll_down(&mut self) {
        let amount = (self.body_height() / 2).max(1);
        self.viewport_scroll = self.viewport_scroll.saturating_sub(amount).max(0);
        if self.viewport_scroll == 0 {
            self.follow_newest = true;
        }
    }

    fn clear_viewport(&mut self) {
        self.transcript.clear();
        self.viewport_scroll = 0;
        self.follow_newest = true;
        self.status_message =
            Some("Viewport cleared; persisted messages remain unchanged.".to_string());
    }

    fn on_idle_ctrl_c(&mut self) {
        if !self.composer.is_empty() {
            self.composer.clear();
            self.history_cursor = None;
            self.slash_selection = 0;
            self.status_message = Some("Draft cleared.".to_string());
        } else {
            self.should_exit = true;
        }
    }

    fn on_ctrl_d_without_generation(&mut self) {
        if self.confirmation.is_some() {
            self.status_message = Some("Finish the active confirmation first.".to_string());
        } else if self.composer.is_empty() {
            self.should_exit = true;
        } else {
            self.composer.delete_forward();
            self.slash_selection = 0;
        }
    }

    fn slash_suggestions(&self) -> Vec<SlashSuggestion> {
        let Some(prefix) = slash_command_prefix(self.composer.text(), self.composer.cursor) else {
            return Vec::new();
        };
        let prefix = prefix.to_ascii_lowercase();
        SLASH_SUGGESTIONS
            .iter()
            .copied()
            .filter(|suggestion| suggestion.name.starts_with(&prefix))
            .collect()
    }

    fn move_slash_selection(&mut self, direction: i8) -> bool {
        let suggestions = self.slash_suggestions();
        if suggestions.is_empty() {
            return false;
        }
        let len = suggestions.len();
        self.slash_selection = match direction.cmp(&0) {
            std::cmp::Ordering::Less => {
                if self.slash_selection == 0 {
                    len - 1
                } else {
                    self.slash_selection - 1
                }
            }
            std::cmp::Ordering::Greater => (self.slash_selection + 1) % len,
            std::cmp::Ordering::Equal => self.slash_selection.min(len - 1),
        };
        true
    }

    fn selected_slash_suggestion(&self) -> Option<SlashSuggestion> {
        let suggestions = self.slash_suggestions();
        suggestions
            .get(
                self.slash_selection
                    .min(suggestions.len().saturating_sub(1)),
            )
            .copied()
    }

    fn accept_slash_suggestion(&mut self) -> bool {
        let Some(suggestion) = self.selected_slash_suggestion() else {
            return false;
        };
        let mut completed = format!("/{}", suggestion.name);
        if suggestion.accepts_arguments {
            completed.push(' ');
        }
        self.composer.set_text(completed);
        self.slash_selection = 0;
        self.status_message = Some(format!("Completed /{}.", suggestion.name));
        true
    }

    fn should_complete_slash_on_enter(&self, message: &str) -> bool {
        let Some(suggestion) = self.selected_slash_suggestion() else {
            return false;
        };
        let command = message.trim();
        let exact_command = command.eq_ignore_ascii_case(&format!("/{}", suggestion.name));
        suggestion.requires_arguments && exact_command || parse_slash_command(message).is_err()
    }

    fn handle_confirmation_key(&mut self, key: KeyEvent) {
        let result = match key.code {
            KeyCode::Char('y') | KeyCode::Char('Y') => Some(ConfirmationResult::Approved),
            KeyCode::Char('n') | KeyCode::Char('N') | KeyCode::Esc => {
                Some(ConfirmationResult::Rejected)
            }
            _ => None,
        };
        if let Some(result) = result {
            self.confirmation = None;
            self.confirmation_result = Some(result);
            self.status_message = Some(match result {
                ConfirmationResult::Approved => "Confirmation approved.".to_string(),
                ConfirmationResult::Rejected => "Confirmation rejected.".to_string(),
                ConfirmationResult::Expired => "Confirmation expired.".to_string(),
            });
        } else {
            self.status_message = Some("Confirmation required: press y or n.".to_string());
        }
    }

    fn history_up(&mut self) {
        if self.history.is_empty() {
            return;
        }
        let next = self
            .history_cursor
            .unwrap_or(self.history.len())
            .saturating_sub(1);
        self.history_cursor = Some(next);
        self.composer.set_text(self.history[next].clone());
    }

    fn history_down(&mut self) {
        let Some(index) = self.history_cursor else {
            return;
        };
        if index + 1 >= self.history.len() {
            self.history_cursor = None;
            self.composer.clear();
        } else {
            let next = index + 1;
            self.history_cursor = Some(next);
            self.composer.set_text(self.history[next].clone());
        }
    }

    fn push_system(&mut self, text: impl Into<String>) {
        let text = text.into();
        self.push_block(TranscriptBlock {
            message_id: None,
            role: BlockRole::System,
            status: BlockStatus::Complete,
            text: bounded_block_text(&text),
        });
    }

    fn push_block(&mut self, block: TranscriptBlock) -> usize {
        if self.transcript.len() >= MAX_TRANSCRIPT_BLOCKS {
            self.transcript.remove(0);
            if let Some(active) = &mut self.active_turn {
                if active.assistant_block == 0 {
                    self.active_turn = None;
                } else {
                    active.assistant_block -= 1;
                }
            }
            if let Some(index) = &mut self.pending_user_block {
                if *index == 0 {
                    self.pending_user_block = None;
                } else {
                    *index -= 1;
                }
            }
        }
        self.transcript.push(block);
        if self.follow_newest {
            self.viewport_scroll = 0;
        }
        self.transcript.len() - 1
    }

    fn body_height(&self) -> usize {
        let header = self.header_lines(self.width as usize).len();
        let suggestions = self.slash_suggestion_lines(self.width as usize).len();
        let composer = self.composer_lines(self.width as usize).len();
        let footer = self.footer_lines(self.width as usize).len();
        (self.height as usize)
            .saturating_sub(
                header
                    .saturating_add(suggestions)
                    .saturating_add(composer)
                    .saturating_add(footer),
            )
            .max(1)
    }

    fn status_line(&self) -> String {
        if let Some(error) = &self.error_banner {
            return format!("Error: {error}");
        }
        let activity = match self.runtime_state {
            RuntimeState::Ready => "ready".to_string(),
            RuntimeState::Generating => {
                format!(
                    "{} generating",
                    ["·", "•", "●", "•"][self.spinner_index % 4]
                )
            }
            RuntimeState::Cancelling => "cancelling".to_string(),
            RuntimeState::Error => "error".to_string(),
        };
        let usage = self.usage.map(|usage| {
            format!(
                "tokens prompt={} completion={} total={}",
                usage.prompt_tokens, usage.completion_tokens, usage.total_tokens
            )
        });
        let follow = if self.follow_newest {
            "following newest"
        } else {
            "scrolled; PgDn follows newest"
        };
        match (self.status_message.as_deref(), usage) {
            (Some(message), Some(usage)) => format!("{activity} | {message} | {usage} | {follow}"),
            (Some(message), None) => format!("{activity} | {message} | {follow}"),
            (None, Some(usage)) => format!("{activity} | {usage} | {follow}"),
            (None, None) => format!("{activity} | {follow}"),
        }
    }

    fn header_lines(&self, width: usize) -> Vec<RenderLine> {
        let title = sanitize_visible_text(&self.conversation_title);
        let id = self
            .conversation_id
            .as_deref()
            .map(short_id)
            .unwrap_or_else(|| "none".to_string());
        let state = match self.runtime_state {
            RuntimeState::Ready => "ready",
            RuntimeState::Generating => "generating",
            RuntimeState::Cancelling => "cancelling",
            RuntimeState::Error => "error",
        };
        let runtime = if self.runtime_summary.is_empty() {
            format!("runtime: {state}")
        } else {
            format!("runtime: {state} | {}", self.runtime_summary)
        };
        let lines = if width < 48 {
            let mut lines = vec![
                format!("GIB AI | {state} | model {}", short_id(&self.model_id)),
                format!("conversation {id} | {title}"),
            ];
            if !self.runtime_summary.is_empty() {
                lines.push(self.runtime_summary.clone());
            }
            lines
        } else {
            vec![
                format!("GIB AI  |  {title} ({id})"),
                format!("model: {}  |  {runtime}", self.model_id),
            ]
        };
        lines
            .into_iter()
            .flat_map(|line| wrap_text(&line, width.max(1)))
            .map(|text| RenderLine {
                text,
                color: Color::Cyan,
            })
            .collect()
    }

    fn transcript_lines(&self, width: usize) -> Vec<RenderLine> {
        let mut lines = Vec::new();
        for block in &self.transcript {
            let label = block_label(block);
            let prefix = format!("[{label}] ");
            let content_width = width.saturating_sub(prefix.chars().count()).max(1);
            let content = sanitize_visible_text(&block.text);
            let content = if block.role == BlockRole::Assistant {
                trim_leading_blank_lines(&content)
            } else {
                content.as_str()
            };
            let wrapped = if content.is_empty() {
                vec![String::new()]
            } else {
                wrap_text(&content, content_width)
            };
            for (index, text) in wrapped.into_iter().enumerate() {
                let rendered = if index == 0 {
                    format!("{prefix}{text}")
                } else {
                    format!("{}{}", " ".repeat(prefix.chars().count()), text)
                };
                lines.push(RenderLine {
                    text: rendered,
                    color: block_color(block.role),
                });
            }
        }
        lines
    }

    fn composer_lines(&self, width: usize) -> Vec<RenderLine> {
        let prefix = "> ";
        let content = sanitize_visible_text(&self.composer.display_text_with_cursor());
        let wrapped = wrap_text(&content, width.saturating_sub(prefix.len()).max(1));
        wrapped
            .into_iter()
            .enumerate()
            .map(|(index, text)| RenderLine {
                text: if index == 0 {
                    format!("{prefix}{text}")
                } else {
                    format!("  {text}")
                },
                color: Color::White,
            })
            .collect()
    }

    fn slash_suggestion_lines(&self, width: usize) -> Vec<RenderLine> {
        let suggestions = self.slash_suggestions();
        if suggestions.is_empty() {
            return Vec::new();
        }

        let selected = self
            .slash_selection
            .min(suggestions.len().saturating_sub(1));
        let narrow = width < 48;
        let heading = if narrow {
            "Commands · Tab completes"
        } else {
            "Command suggestions · ↑/↓ choose · Tab complete · Enter run"
        };
        let mut lines = vec![RenderLine {
            text: heading.to_string(),
            color: Color::Magenta,
        }];
        for (index, suggestion) in suggestions.iter().enumerate() {
            let marker = if index == selected { "› " } else { "  " };
            let text = if narrow {
                format!("{marker}/{}", suggestion.name)
            } else {
                format!(
                    "{marker}/{:<8} — {}",
                    suggestion.name, suggestion.description
                )
            };
            let color = if index == selected {
                Color::Yellow
            } else {
                Color::DarkGrey
            };
            for (line_index, line) in wrap_text(&text, width.max(1)).into_iter().enumerate() {
                lines.push(RenderLine {
                    text: if line_index == 0 {
                        line
                    } else {
                        format!("  {line}")
                    },
                    color,
                });
            }
        }
        lines
    }

    fn footer_lines(&self, width: usize) -> Vec<RenderLine> {
        let mut lines = Vec::new();
        lines.extend(
            wrap_text(&self.status_line(), width.max(1))
                .into_iter()
                .map(|text| RenderLine {
                    text,
                    color: if self.error_banner.is_some() {
                        Color::Red
                    } else {
                        Color::Yellow
                    },
                }),
        );
        let help = if width < 48 {
            "Enter send | Ctrl+J newline | Ctrl+C cancel/clear | Ctrl+D exit | PgUp/PgDn scroll | /help"
        } else {
            "Enter send · Ctrl+J newline · Ctrl+C cancel/clear · Ctrl+D exit · PgUp/PgDn scroll · /help commands"
        };
        lines.extend(
            wrap_text(help, width.max(1))
                .into_iter()
                .map(|text| RenderLine {
                    text,
                    color: Color::DarkGrey,
                }),
        );
        if let Some(request) = &self.confirmation {
            lines.extend(
                wrap_text(
                    &format!("CONFIRM [{}]: {} (y/n)", request.action_id, request.summary),
                    width.max(1),
                )
                .into_iter()
                .map(|text| RenderLine {
                    text,
                    color: Color::Magenta,
                }),
            );
        }
        lines
    }

    fn render(&self, stdout: &mut Stdout) -> io::Result<()> {
        let width = self.width as usize;
        let header = self.header_lines(width);
        let suggestions = self.slash_suggestion_lines(width);
        let composer = self.composer_lines(width);
        let footer = self.footer_lines(width);
        let body_height = (self.height as usize).saturating_sub(
            header
                .len()
                .saturating_add(suggestions.len())
                .saturating_add(composer.len())
                .saturating_add(footer.len()),
        );
        let transcript = self.transcript_lines(width);
        let end = if self.follow_newest {
            transcript.len()
        } else {
            transcript.len().saturating_sub(self.viewport_scroll)
        };
        let start = end.saturating_sub(body_height);
        let body = &transcript[start..end];

        queue!(
            stdout,
            cursor::MoveTo(0, 0),
            Clear(ClearType::All),
            cursor::Hide
        )?;
        for line in header
            .iter()
            .chain(body)
            .chain(suggestions.iter())
            .chain(composer.iter())
            .chain(footer.iter())
        {
            queue!(
                stdout,
                SetForegroundColor(line.color),
                Print(&line.text),
                ResetColor,
                Print("\r\n")
            )?;
        }
        stdout.flush()
    }
}

#[derive(Debug, Clone)]
struct RenderLine {
    text: String,
    color: Color,
}

#[derive(Debug)]
enum SlashCommand {
    Help,
    New(Option<String>),
    List,
    Select(String),
    Rename { id: String, title: String },
    Clear,
    Status,
    Exit,
}

#[derive(Debug)]
enum TerminalMessage {
    Event(Event),
    Error(String),
}

#[derive(Debug)]
struct TerminalEventSource {
    stop: Arc<AtomicBool>,
    handle: Option<thread::JoinHandle<()>>,
}

impl TerminalEventSource {
    fn start(sender: UnboundedSender<TerminalMessage>) -> Self {
        let stop = Arc::new(AtomicBool::new(false));
        let thread_stop = Arc::clone(&stop);
        let handle = thread::spawn(move || {
            while !thread_stop.load(Ordering::Acquire) {
                match event::poll(TERMINAL_POLL_INTERVAL) {
                    Ok(true) => match event::read() {
                        Ok(event) => {
                            if sender.send(TerminalMessage::Event(event)).is_err() {
                                break;
                            }
                        }
                        Err(error) => {
                            let _ = sender.send(TerminalMessage::Error(format!(
                                "failed to read terminal input: {error}"
                            )));
                            break;
                        }
                    },
                    Ok(false) => {}
                    Err(error) => {
                        let _ = sender.send(TerminalMessage::Error(format!(
                            "failed to poll terminal input: {error}"
                        )));
                        break;
                    }
                }
            }
        });
        Self {
            stop,
            handle: Some(handle),
        }
    }
}

impl Drop for TerminalEventSource {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Release);
        if let Some(handle) = self.handle.take() {
            let _ = handle.join();
        }
    }
}

struct AiTerminalGuard {
    raw_mode: bool,
}

impl AiTerminalGuard {
    fn new() -> Result<Self, String> {
        enable_raw_mode()
            .map_err(|error| format!("failed to enable raw terminal mode: {error}"))?;
        let mut stdout = io::stdout();
        if let Err(error) = execute!(stdout, EnterAlternateScreen, cursor::Hide) {
            let _ = disable_raw_mode();
            return Err(format!("failed to initialize the AI terminal: {error}"));
        }
        Ok(Self { raw_mode: true })
    }
}

impl Drop for AiTerminalGuard {
    fn drop(&mut self) {
        let mut stdout = io::stdout();
        let _ = execute!(stdout, cursor::Show, LeaveAlternateScreen);
        if self.raw_mode {
            let _ = disable_raw_mode();
        }
        let _ = stdout.flush();
    }
}

struct GenerationTask {
    cancellation: AiCancellation,
    handle: JoinHandle<()>,
}

enum AiMessage {
    Turn(AiTurnEvent),
    Completed(Result<AiTurnResponse, AiTurnError>),
}

/// Start the event-driven terminal frontend over the same AiTurnService used
/// by JSON mode. The UI owns presentation and input only; persistence and
/// generation remain in their existing services.
pub(crate) async fn run(
    service: &crate::ai::AiTurnService,
    conversation_id: Option<String>,
    model_id: String,
    runtime_config: RuntimeConfig,
) -> Result<(), String> {
    let conversations = ConversationService::default_store().map_err(|error| error.to_string())?;
    let initial = match conversation_id.as_deref() {
        Some(id) => Some(
            conversations
                .load(id.to_string())
                .await
                .map_err(|error| error.to_string())?,
        ),
        None => conversations
            .active()
            .await
            .map_err(|error| error.to_string())?,
    };

    let guard = AiTerminalGuard::new()?;
    let (width, height) =
        terminal::size().unwrap_or((DEFAULT_TERMINAL_WIDTH, DEFAULT_TERMINAL_HEIGHT));
    let mut app = AiInteractiveApp::new(model_id, conversation_id, width, height);
    app.set_runtime_config(&runtime_config);
    if let Some(conversation) = &initial {
        app.load_conversation(conversation);
    }

    let (terminal_sender, terminal_receiver) = mpsc::unbounded_channel();
    let terminal_source = TerminalEventSource::start(terminal_sender);
    let (ai_sender, ai_receiver) = mpsc::unbounded_channel();
    let mut generation = None;
    let result = run_event_loop(
        service,
        &conversations,
        &mut app,
        terminal_receiver,
        ai_receiver,
        ai_sender,
        &mut generation,
        runtime_config.keep_model_warm,
    )
    .await;

    if let Some(task) = generation.take() {
        task.cancellation.cancel();
        let mut handle = task.handle;
        if time::timeout(Duration::from_secs(2), &mut handle)
            .await
            .is_err()
        {
            handle.abort();
            let _ = handle.await;
        }
    }
    drop(terminal_source);
    drop(guard);
    result
}

async fn run_event_loop(
    service: &crate::ai::AiTurnService,
    conversations: &ConversationService,
    app: &mut AiInteractiveApp,
    mut terminal_receiver: UnboundedReceiver<TerminalMessage>,
    mut ai_receiver: UnboundedReceiver<AiMessage>,
    ai_sender: UnboundedSender<AiMessage>,
    generation: &mut Option<GenerationTask>,
    keep_model_warm: bool,
) -> Result<(), String> {
    let mut stdout = io::stdout();
    let mut ticker = time::interval(RENDER_TICK);
    ticker.set_missed_tick_behavior(MissedTickBehavior::Skip);

    loop {
        app.render(&mut stdout)
            .map_err(|error| format!("failed to render the AI terminal: {error}"))?;
        if app.should_exit {
            break;
        }

        tokio::select! {
            terminal_message = terminal_receiver.recv() => {
                match terminal_message {
                    Some(TerminalMessage::Event(event)) => {
                        handle_terminal_event(
                            event,
                            service,
                            conversations,
                            app,
                            &ai_sender,
                            generation,
                            keep_model_warm,
                        ).await?;
                    }
                    Some(TerminalMessage::Error(error)) => return Err(error),
                    None => return Err("terminal input source closed unexpectedly".to_string()),
                }
            }
            ai_message = ai_receiver.recv() => {
                if let Some(message) = ai_message {
                    handle_ai_message(
                        message,
                        service,
                        conversations,
                        app,
                        generation,
                        keep_model_warm,
                    ).await;
                } else if generation.is_some() {
                    return Err("AI event source closed unexpectedly".to_string());
                }
            }
            _ = ticker.tick() => app.tick(),
        }
    }
    Ok(())
}

async fn handle_terminal_event(
    event: Event,
    service: &crate::ai::AiTurnService,
    conversations: &ConversationService,
    app: &mut AiInteractiveApp,
    ai_sender: &UnboundedSender<AiMessage>,
    generation: &mut Option<GenerationTask>,
    keep_model_warm: bool,
) -> Result<(), String> {
    match event {
        Event::Resize(width, height) => app.on_resize(width, height),
        Event::Paste(text) if app.confirmation.is_none() => {
            app.composer.insert_text(&text);
            app.slash_selection = 0;
        }
        Event::Key(key) if key.kind != KeyEventKind::Release => {
            if app.confirmation.is_some() {
                app.handle_confirmation_key(key);
                let _ = app.take_confirmation_result();
                return Ok(());
            }
            if key.modifiers.contains(KeyModifiers::CONTROL)
                && matches!(key.code, KeyCode::Char('c') | KeyCode::Char('C'))
            {
                if let Some(task) = generation.as_ref() {
                    if !task.cancellation.is_cancelled() {
                        task.cancellation.cancel();
                        app.runtime_state = RuntimeState::Cancelling;
                        app.status_message = Some("Cancelling generation...".to_string());
                    } else {
                        app.status_message =
                            Some("Cancellation is already in progress; please wait.".to_string());
                    }
                } else {
                    app.on_idle_ctrl_c();
                }
                return Ok(());
            }
            if key.modifiers.contains(KeyModifiers::CONTROL)
                && matches!(key.code, KeyCode::Char('d') | KeyCode::Char('D'))
            {
                app.on_ctrl_d_without_generation();
                return Ok(());
            }
            if key.modifiers.contains(KeyModifiers::CONTROL)
                && matches!(key.code, KeyCode::Char('j') | KeyCode::Char('J'))
            {
                app.composer.insert_char('\n');
                app.slash_selection = 0;
                return Ok(());
            }
            let composer_before = app.composer.text().to_string();
            match key.code {
                KeyCode::Enter => {
                    if generation.is_some() {
                        app.status_message = Some(
                            "Generation is active; press Ctrl+C to cancel it first.".to_string(),
                        );
                    } else if app.should_complete_slash_on_enter(app.composer.text()) {
                        app.accept_slash_suggestion();
                    } else {
                        let message = app.composer.take();
                        if message.trim().is_empty() {
                            app.status_message =
                                Some("Type a message before pressing Enter.".to_string());
                        } else if message.trim_start().starts_with('/') {
                            match parse_slash_command(&message) {
                                Ok(Some(command)) => {
                                    dispatch_slash_command(command, conversations, app).await;
                                }
                                Ok(None) => {}
                                Err(error) => app.set_local_error(error),
                            }
                        } else if app.submit_message(message.clone()) {
                            if !keep_model_warm {
                                app.status_message =
                                    Some("Loading the model for this turn...".to_string());
                            }
                            *generation = Some(start_generation(
                                service,
                                app.conversation_id.clone(),
                                message,
                                ai_sender.clone(),
                                !keep_model_warm,
                            ));
                        }
                    }
                }
                KeyCode::Tab => {
                    app.accept_slash_suggestion();
                }
                KeyCode::PageUp => app.scroll_up(),
                KeyCode::PageDown => app.scroll_down(),
                KeyCode::Left => app.composer.move_left(),
                KeyCode::Right => app.composer.move_right(),
                KeyCode::Home => app.composer.move_home(),
                KeyCode::End => app.composer.move_end(),
                KeyCode::Up => {
                    if app.move_slash_selection(-1) {
                    } else if app.composer.is_empty() {
                        app.history_up();
                    } else {
                        app.composer.move_up();
                    }
                }
                KeyCode::Down => {
                    if app.move_slash_selection(1) {
                    } else if app.history_cursor.is_some() {
                        app.history_down();
                    } else {
                        app.composer.move_down();
                    }
                }
                KeyCode::Backspace => app.composer.backspace(),
                KeyCode::Delete => app.composer.delete_forward(),
                KeyCode::Char(character)
                    if !character.is_control()
                        && !key.modifiers.contains(KeyModifiers::CONTROL)
                        && !key.modifiers.contains(KeyModifiers::ALT) =>
                {
                    app.composer.insert_char(character);
                }
                KeyCode::Esc => {
                    app.error_banner = None;
                    app.status_message = Some("Ready".to_string());
                }
                _ => {}
            }
            if app.composer.text() != composer_before {
                app.slash_selection = 0;
            }
        }
        _ => {}
    }
    Ok(())
}

fn start_generation(
    service: &crate::ai::AiTurnService,
    conversation_id: Option<String>,
    message: String,
    sender: UnboundedSender<AiMessage>,
    load_model_before_turn: bool,
) -> GenerationTask {
    let cancellation = AiCancellation::new();
    let task_cancellation = cancellation.clone();
    let service = service.clone();
    let sink: AiTurnEventSink = Arc::new({
        let sender = sender.clone();
        move |event| {
            let _ = sender.send(AiMessage::Turn(event.clone()));
        }
    });
    let handle = tokio::spawn(async move {
        if load_model_before_turn && let Err(error) = service.load_model().await {
            let _ = sender.send(AiMessage::Completed(Err(AiTurnError::Backend(error))));
            return;
        }
        let result = run_turn_for_interactive(
            &service,
            conversation_id,
            message,
            task_cancellation,
            Some(sink),
        )
        .await;
        let _ = sender.send(AiMessage::Completed(result));
    });
    GenerationTask {
        cancellation,
        handle,
    }
}

async fn handle_ai_message(
    message: AiMessage,
    service: &crate::ai::AiTurnService,
    conversations: &ConversationService,
    app: &mut AiInteractiveApp,
    generation: &mut Option<GenerationTask>,
    keep_model_warm: bool,
) {
    match message {
        AiMessage::Turn(event) => app.apply_turn_event(&event),
        AiMessage::Completed(result) => {
            if let Some(task) = generation.take() {
                drop(task.handle);
            }
            match result {
                Ok(response) => {
                    app.finish_response(&response);
                    if let Ok(conversation) =
                        conversations.load(response.conversation_id.clone()).await
                    {
                        app.conversation_title = conversation.title;
                    }
                }
                Err(error) => {
                    if app.active_turn.is_some() {
                        app.fail_active_turn(&error);
                    } else if app.pending_user_block.is_some() {
                        // Validation, conversation resolution, and the
                        // initial persistence step can fail before Started is
                        // emitted. Never leave the composer in a permanent
                        // "sending" state in that case.
                        app.fail_before_started_turn(&error);
                    }
                }
            }
            if !keep_model_warm {
                if let Err(error) = service.unload_model().await {
                    app.set_local_error(format!(
                        "The LowMemory profile could not release the model after the turn: {error}"
                    ));
                } else if app.runtime_state == RuntimeState::Ready {
                    app.status_message = Some(
                        "Ready (LowMemory profile released the model; it will reload on the next turn)."
                            .to_string(),
                    );
                }
            }
        }
    }
}

async fn dispatch_slash_command(
    command: SlashCommand,
    conversations: &ConversationService,
    app: &mut AiInteractiveApp,
) {
    match command {
        SlashCommand::Help => app.push_system(SLASH_HELP),
        SlashCommand::New(title) => match conversations.create_and_select(title).await {
            Ok(conversation) => {
                let id = conversation.conversation_id.clone();
                app.load_conversation(&conversation);
                app.push_system(format!(
                    "Created and selected conversation '{}' ({}).",
                    conversation.title, id
                ));
            }
            Err(error) => app.set_local_error(error.to_string()),
        },
        SlashCommand::List => match conversations.list().await {
            Ok(listing) => {
                let active_id = conversations.active_conversation_id().await.ok().flatten();
                app.push_system(format_conversation_list(&listing, active_id.as_deref()));
            }
            Err(error) => app.set_local_error(error.to_string()),
        },
        SlashCommand::Select(id) => match conversations.select_active(id).await {
            Ok(conversation) => {
                app.load_conversation(&conversation);
                app.push_system(format!(
                    "Selected conversation '{}' ({}).",
                    conversation.title, conversation.conversation_id
                ));
            }
            Err(error) => app.set_local_error(error.to_string()),
        },
        SlashCommand::Rename { id, title } => match conversations.load(id.clone()).await {
            Ok(current) => match conversations.rename(id, current.revision, title).await {
                Ok(conversation) => {
                    if app.conversation_id.as_deref() == Some(conversation.conversation_id.as_str())
                    {
                        app.conversation_title = conversation.title.clone();
                    }
                    app.push_system(format!(
                        "Renamed conversation {} to '{}'.",
                        conversation.conversation_id, conversation.title
                    ));
                }
                Err(error) => app.set_local_error(error.to_string()),
            },
            Err(error) => app.set_local_error(error.to_string()),
        },
        SlashCommand::Clear => app.clear_viewport(),
        SlashCommand::Status => app.push_system(app.status_line()),
        SlashCommand::Exit => app.should_exit = true,
    }
}

impl AiInteractiveApp {
    fn set_local_error(&mut self, message: String) {
        self.error_banner = Some(message.clone());
        self.status_message = Some("Local command failed.".to_string());
        self.push_block(TranscriptBlock {
            message_id: None,
            role: BlockRole::Error,
            status: BlockStatus::Error,
            text: message,
        });
    }
}

const SLASH_HELP: &str = "Local commands: /help, /new [title], /list, /select <id>, /switch <id>, /rename <id> <title>, /clear, /status, /exit. Ctrl+J inserts a newline; Ctrl+C cancels or clears; Ctrl+D exits only with an empty composer.";

fn slash_command_prefix(text: &str, cursor: usize) -> Option<&str> {
    if cursor != text.len() || !text.starts_with('/') {
        return None;
    }
    let command = &text[1..];
    if command.contains(char::is_whitespace) {
        None
    } else {
        Some(command)
    }
}

fn parse_slash_command(input: &str) -> Result<Option<SlashCommand>, String> {
    let trimmed = input.trim();
    if !trimmed.starts_with('/') {
        return Ok(None);
    }
    let command_body = trimmed[1..].trim();
    let (name, rest) = command_body
        .split_once(char::is_whitespace)
        .map_or((command_body, ""), |(name, rest)| (name, rest.trim()));
    match name.to_ascii_lowercase().as_str() {
        "help" | "?" => require_no_arguments(name, rest).map(|()| Some(SlashCommand::Help)),
        "new" => Ok(Some(SlashCommand::New(
            (!rest.is_empty()).then(|| rest.to_string()),
        ))),
        "list" => require_no_arguments(name, rest).map(|()| Some(SlashCommand::List)),
        "select" | "switch" => one_argument(name, rest).map(|id| Some(SlashCommand::Select(id))),
        "rename" => {
            let mut parts = rest.splitn(2, char::is_whitespace);
            let id = parts
                .next()
                .filter(|value| !value.is_empty())
                .ok_or_else(|| "/rename requires <id> and <title>".to_string())?;
            let title = parts
                .next()
                .map(str::trim)
                .filter(|value| !value.is_empty())
                .ok_or_else(|| "/rename requires <id> and <title>".to_string())?;
            Ok(Some(SlashCommand::Rename {
                id: id.to_string(),
                title: title.to_string(),
            }))
        }
        "clear" => require_no_arguments(name, rest).map(|()| Some(SlashCommand::Clear)),
        "status" => require_no_arguments(name, rest).map(|()| Some(SlashCommand::Status)),
        "exit" | "quit" => require_no_arguments(name, rest).map(|()| Some(SlashCommand::Exit)),
        "" => Err("Slash command cannot be empty. Type /help for commands.".to_string()),
        other => Err(format!(
            "Unknown slash command '/{other}'. Type /help for commands."
        )),
    }
}

fn require_no_arguments(command: &str, rest: &str) -> Result<(), String> {
    if rest.is_empty() {
        Ok(())
    } else {
        Err(format!("/{command} does not accept arguments"))
    }
}

fn one_argument(command: &str, rest: &str) -> Result<String, String> {
    let mut parts = rest.split_whitespace();
    let value = parts
        .next()
        .ok_or_else(|| format!("/{command} requires <id>"))?;
    if parts.next().is_some() {
        return Err(format!("/{command} accepts exactly one <id>"));
    }
    Ok(value.to_string())
}

fn format_conversation_list(
    listing: &crate::ai::conversation::ConversationList,
    active_id: Option<&str>,
) -> String {
    if listing.conversations.is_empty() && listing.warnings.is_empty() {
        return "No conversations found.".to_string();
    }
    let mut lines = Vec::new();
    for summary in &listing.conversations {
        let marker = if active_id == Some(summary.conversation_id.as_str()) {
            "*"
        } else {
            " "
        };
        let role = summary
            .last_role
            .map(conversation_role_label)
            .unwrap_or("none");
        lines.push(format!(
            "{marker} {} — {} | {} messages | last: {role}",
            summary.conversation_id, summary.title, summary.message_count
        ));
    }
    for warning in &listing.warnings {
        lines.push(format!(
            "warning [{}] for {}: {}",
            warning.code, warning.conversation_id, warning.message
        ));
    }
    lines.join("\n")
}

fn block_role(role: ConversationMessageRole) -> BlockRole {
    match role {
        ConversationMessageRole::User => BlockRole::User,
        ConversationMessageRole::Assistant => BlockRole::Assistant,
    }
}

fn block_status(status: ConversationMessageStatus) -> BlockStatus {
    match status {
        ConversationMessageStatus::Complete => BlockStatus::Complete,
        ConversationMessageStatus::Interrupted => BlockStatus::Interrupted,
        ConversationMessageStatus::Pending => BlockStatus::Streaming,
    }
}

fn block_label(block: &TranscriptBlock) -> &'static str {
    match (block.role, block.status) {
        (BlockRole::User, _) => "you",
        (BlockRole::Assistant, BlockStatus::Streaming) => "assistant ·",
        (BlockRole::Assistant, BlockStatus::Interrupted) => "assistant interrupted",
        (BlockRole::Assistant, _) => "assistant",
        (BlockRole::System, _) => "system",
        (BlockRole::Error, _) => "error",
        (BlockRole::Activity, _) => "activity",
    }
}

fn block_color(role: BlockRole) -> Color {
    match role {
        BlockRole::User => Color::Green,
        BlockRole::Assistant => Color::White,
        BlockRole::System => Color::Cyan,
        BlockRole::Error => Color::Red,
        BlockRole::Activity => Color::Yellow,
    }
}

fn conversation_role_label(role: ConversationMessageRole) -> &'static str {
    match role {
        ConversationMessageRole::User => "user",
        ConversationMessageRole::Assistant => "assistant",
    }
}

fn bounded_block_text(text: &str) -> String {
    if text.len() <= MAX_TRANSCRIPT_BLOCK_BYTES {
        return text.to_string();
    }
    let content_limit = MAX_TRANSCRIPT_BLOCK_BYTES.saturating_sub(TRANSCRIPT_CLIP_MARKER.len());
    let mut end = content_limit;
    while end > 0 && !text.is_char_boundary(end) {
        end -= 1;
    }
    let mut bounded = text[..end].to_string();
    bounded.push_str(TRANSCRIPT_CLIP_MARKER);
    bounded
}

fn append_bounded_text(target: &mut String, delta: &str) {
    if target.len() >= MAX_TRANSCRIPT_BLOCK_BYTES {
        return;
    }
    let remaining = MAX_TRANSCRIPT_BLOCK_BYTES - target.len();
    let needs_marker = delta.len() > remaining;
    let content_limit = if needs_marker {
        remaining.saturating_sub(TRANSCRIPT_CLIP_MARKER.len())
    } else {
        remaining
    };
    let mut end = delta.len().min(content_limit);
    while end > 0 && !delta.is_char_boundary(end) {
        end -= 1;
    }
    target.push_str(&delta[..end]);
    if needs_marker && remaining.saturating_sub(end) >= TRANSCRIPT_CLIP_MARKER.len() {
        target.push_str(TRANSCRIPT_CLIP_MARKER);
    }
}

fn sanitize_visible_text(text: &str) -> String {
    let mut sanitized = String::with_capacity(text.len());
    for character in text.chars() {
        match character {
            '\n' => sanitized.push('\n'),
            '\t' => sanitized.push_str("    "),
            character if !character.is_control() => sanitized.push(character),
            _ => {}
        }
    }
    sanitized
}

fn trim_leading_blank_lines(text: &str) -> &str {
    let mut offset = 0;
    while offset < text.len() {
        let remaining = &text[offset..];
        let line_end = remaining.find('\n').unwrap_or(remaining.len());
        if !remaining[..line_end].trim().is_empty() {
            break;
        }
        offset += if line_end < remaining.len() {
            line_end + 1
        } else {
            line_end
        };
    }
    &text[offset..]
}

fn wrap_text(text: &str, width: usize) -> Vec<String> {
    let width = width.max(1);
    let mut lines = Vec::new();
    for logical_line in text.split('\n') {
        let characters: Vec<char> = logical_line.chars().collect();
        if characters.is_empty() {
            lines.push(String::new());
            continue;
        }
        let mut start = 0usize;
        while start < characters.len() {
            let end = (start + width).min(characters.len());
            if end == characters.len() {
                lines.push(characters[start..end].iter().collect());
                break;
            }
            let break_at = characters[start..end]
                .iter()
                .rposition(|character| character.is_whitespace())
                .map(|position| start + position)
                .filter(|position| *position > start)
                .unwrap_or(end);
            lines.push(characters[start..break_at].iter().collect());
            start = break_at;
            while start < characters.len() && characters[start].is_whitespace() {
                start += 1;
            }
        }
    }
    lines
}

fn short_id(id: &str) -> String {
    let mut characters = id.chars();
    let prefix: String = characters.by_ref().take(20).collect();
    if characters.next().is_some() {
        format!("{prefix}…")
    } else {
        prefix
    }
}

fn previous_boundary(text: &str, cursor: usize) -> usize {
    text[..cursor]
        .char_indices()
        .next_back()
        .map(|(index, _)| index)
        .unwrap_or(0)
}

fn next_boundary(text: &str, cursor: usize) -> usize {
    text[cursor..]
        .chars()
        .next()
        .map(|character| cursor + character.len_utf8())
        .unwrap_or(text.len())
}

fn current_line_start(text: &str, cursor: usize) -> usize {
    text[..cursor].rfind('\n').map_or(0, |index| index + 1)
}

fn current_line_end(text: &str, cursor: usize) -> usize {
    text[cursor..]
        .find('\n')
        .map_or(text.len(), |index| cursor + index)
}

fn line_column_cursor(text: &str, start: usize, end: usize, column: usize) -> usize {
    let line = &text[start..end];
    line.char_indices()
        .nth(column)
        .map(|(index, _)| start + index)
        .unwrap_or(end)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn response() -> AiTurnResponse {
        AiTurnResponse {
            conversation_id: "conv-test".to_string(),
            turn_id: "turn-test".to_string(),
            user_message_id: "msg-user".to_string(),
            assistant_message_id: "msg-assistant".to_string(),
            model_id: "model-test".to_string(),
            text: "Hello from the assistant".to_string(),
            finish_reason: crate::ai::runtime::AiFinishReason::EndOfGeneration,
            usage: AiUsage {
                prompt_tokens: 4,
                completion_tokens: 5,
                total_tokens: 9,
            },
        }
    }

    fn started() -> AiTurnEvent {
        AiTurnEvent::Started {
            conversation_id: "conv-test".to_string(),
            turn_id: "turn-test".to_string(),
            user_message_id: "msg-user".to_string(),
            model_id: "model-test".to_string(),
        }
    }

    #[test]
    fn composer_supports_multiline_unicode_cursor_editing_and_delete() {
        let mut composer = Composer::new();
        composer.insert_char('a');
        composer.insert_char('é');
        composer.insert_char('\n');
        composer.insert_char('b');
        composer.move_up();
        composer.move_end();
        composer.backspace();
        assert_eq!(composer.text(), "a\nb");
        composer.move_down();
        composer.move_home();
        composer.delete_forward();
        assert_eq!(composer.text(), "a\n");
        composer.clear();
        assert!(composer.is_blank());
    }

    #[test]
    fn composer_accepts_multiline_paste_without_terminal_controls() {
        let mut composer = Composer::new();
        composer.insert_text("first\r\nsecond\u{1b}[31m");
        assert_eq!(composer.text(), "first\nsecond[31m");
    }

    #[test]
    fn slash_commands_are_local_and_support_titles_with_spaces() {
        assert!(matches!(parse_slash_command("hello").unwrap(), None));
        assert!(matches!(
            parse_slash_command("/new Project notes").unwrap(),
            Some(SlashCommand::New(Some(title))) if title == "Project notes"
        ));
        assert!(matches!(
            parse_slash_command("/switch conv-test").unwrap(),
            Some(SlashCommand::Select(id)) if id == "conv-test"
        ));
        assert!(matches!(
            parse_slash_command("/rename conv-test New title").unwrap(),
            Some(SlashCommand::Rename { id, title }) if id == "conv-test" && title == "New title"
        ));
        assert!(parse_slash_command("/list unexpected").is_err());
        assert!(parse_slash_command("/unknown").is_err());
    }

    #[test]
    fn slash_completion_filters_commands_and_inserts_argument_spacing() {
        let mut app = AiInteractiveApp::new("model-test".to_string(), None, 80, 24);
        app.composer.insert_char('/');
        assert_eq!(app.slash_suggestions().len(), SLASH_SUGGESTIONS.len());
        assert!(
            app.slash_suggestion_lines(80)
                .iter()
                .any(|line| line.text.contains("/help"))
        );

        assert!(app.move_slash_selection(1));
        assert!(app.accept_slash_suggestion());
        assert_eq!(app.composer.text(), "/new ");

        app.composer.set_text("/sw".to_string());
        assert_eq!(
            app.slash_suggestions()
                .iter()
                .map(|suggestion| suggestion.name)
                .collect::<Vec<_>>(),
            vec!["switch"]
        );
        assert!(app.accept_slash_suggestion());
        assert_eq!(app.composer.text(), "/switch ");

        app.composer.set_text("/new Project".to_string());
        assert!(app.slash_suggestions().is_empty());
    }

    #[test]
    fn app_state_machine_reconstructs_streaming_without_duplicate_final_text() {
        let mut app = AiInteractiveApp::new("model-test".to_string(), None, 80, 24);
        assert!(app.submit_message("hello".to_string()));
        app.apply_turn_event(&started());
        app.apply_turn_event(&AiTurnEvent::Delta {
            conversation_id: "conv-test".to_string(),
            turn_id: "turn-test".to_string(),
            text: "Hello from".to_string(),
        });
        app.apply_turn_event(&AiTurnEvent::Progress {
            conversation_id: "conv-test".to_string(),
            turn_id: "turn-test".to_string(),
            usage: response().usage,
        });
        app.apply_turn_event(&AiTurnEvent::Delta {
            conversation_id: "conv-test".to_string(),
            turn_id: "turn-test".to_string(),
            text: " the assistant".to_string(),
        });
        app.apply_turn_event(&AiTurnEvent::Finished {
            response: response(),
        });
        assert!(app.active_turn.is_none());
        assert_eq!(app.runtime_state, RuntimeState::Ready);
        assert_eq!(app.usage.unwrap().total_tokens, 9);
        let assistant = app
            .transcript
            .iter()
            .find(|block| block.role == BlockRole::Assistant)
            .expect("assistant block should exist");
        assert_eq!(assistant.text, "Hello from the assistant");
    }

    #[test]
    fn app_state_machine_handles_cancel_error_resize_scroll_and_exit() {
        let mut app = AiInteractiveApp::new("model-test".to_string(), None, 40, 10);
        app.submit_message("hello".to_string());
        app.apply_turn_event(&started());
        app.apply_turn_event(&AiTurnEvent::Cancelled {
            conversation_id: "conv-test".to_string(),
            turn_id: "turn-test".to_string(),
            partial_text: "partial".to_string(),
            usage: AiUsage::default(),
        });
        assert_eq!(app.runtime_state, RuntimeState::Ready);
        assert_eq!(
            app.transcript
                .iter()
                .find(|block| block.role == BlockRole::Assistant)
                .unwrap()
                .status,
            BlockStatus::Interrupted
        );
        app.set_local_error("local failure".to_string());
        assert_eq!(app.runtime_state, RuntimeState::Ready);
        app.on_resize(20, 8);
        assert_eq!(app.width, 20);
        app.scroll_up();
        assert!(!app.follow_newest);
        app.scroll_down();
        assert!(app.follow_newest);
        app.composer.insert_char('x');
        app.on_idle_ctrl_c();
        assert!(app.composer.is_empty());
        app.on_idle_ctrl_c();
        assert!(app.should_exit);
    }

    #[test]
    fn failed_turn_before_started_does_not_leave_a_stuck_generation_state() {
        let mut app = AiInteractiveApp::new("model-test".to_string(), None, 80, 24);
        assert!(app.submit_message("hello".to_string()));
        app.fail_before_started_turn(&AiTurnError::InvalidMessage);
        assert_eq!(app.runtime_state, RuntimeState::Error);
        assert!(app.active_turn.is_none());
        assert!(app.pending_user_block.is_none());
        assert!(app.error_banner.is_some());
        assert!(app.transcript.iter().any(|block| {
            block.role == BlockRole::Error && block.text.contains("cannot be empty")
        }));
    }

    #[test]
    fn empty_submission_and_ctrl_d_follow_the_documented_composer_policy() {
        let mut app = AiInteractiveApp::new("model-test".to_string(), None, 80, 24);
        assert!(!app.submit_message(" \n".to_string()));
        assert_eq!(app.runtime_state, RuntimeState::Ready);
        app.composer.insert_char('a');
        app.composer.move_home();
        app.on_ctrl_d_without_generation();
        assert!(app.composer.is_empty());
        assert!(!app.should_exit);
        app.on_ctrl_d_without_generation();
        assert!(app.should_exit);
    }

    #[test]
    fn wrapping_never_overflows_a_narrow_viewport_and_sanitizes_controls() {
        let mut app = AiInteractiveApp::new("model-test".to_string(), None, 12, 12);
        app.push_system("a-very-long-path-without-spaces\u{1b}[31m");
        for line in app.transcript_lines(12) {
            assert!(line.text.chars().count() <= 12);
            assert!(!line.text.contains('\u{1b}'));
        }
        assert!(app.header_lines(12).len() >= 2);
    }

    #[test]
    fn assistant_leading_blank_lines_do_not_create_an_indented_first_line() {
        let mut app = AiInteractiveApp::new("model-test".to_string(), None, 80, 24);
        app.push_block(TranscriptBlock {
            message_id: Some("assistant-1".to_string()),
            role: BlockRole::Assistant,
            status: BlockStatus::Complete,
            text: "\n\nHello from the assistant".to_string(),
        });

        let lines = app.transcript_lines(80);
        assert_eq!(lines.len(), 1);
        assert_eq!(lines[0].text, "[assistant] Hello from the assistant");
    }

    #[test]
    fn transcript_blocks_remain_bounded_when_streaming_large_deltas() {
        let large = "x".repeat(MAX_TRANSCRIPT_BLOCK_BYTES + 128);
        let bounded = bounded_block_text(&large);
        assert!(bounded.len() <= MAX_TRANSCRIPT_BLOCK_BYTES);
        assert!(bounded.ends_with(TRANSCRIPT_CLIP_MARKER));

        let mut streamed = String::new();
        append_bounded_text(&mut streamed, &large);
        assert!(streamed.len() <= MAX_TRANSCRIPT_BLOCK_BYTES);
        assert!(streamed.ends_with(TRANSCRIPT_CLIP_MARKER));
    }

    #[test]
    fn confirmation_interface_tracks_result_without_approving_actions_implicitly() {
        let mut app = AiInteractiveApp::new("model-test".to_string(), None, 80, 24);
        app.request_confirmation(ConfirmationRequest {
            action_id: "restore-1".to_string(),
            summary: "Restore one file".to_string(),
            risk_level: ConfirmationRiskLevel::High,
            affected_paths: vec!["docs/report.pdf".to_string()],
            affected_count: 1,
            plan_id: Some("plan-1".to_string()),
            expires_at: None,
        });
        assert!(app.confirmation.is_some());
        app.handle_confirmation_key(KeyEvent::new(KeyCode::Char('n'), KeyModifiers::NONE));
        assert_eq!(
            app.take_confirmation_result(),
            Some(ConfirmationResult::Rejected)
        );
    }
}
