# Task 07 — Build the modern interactive gib ai terminal frontend

## Roadmap position

This task finishes the first usable slice of gib ai. The direct-chat service from Task 05 remains the source of truth; this task provides a robust terminal presentation and input layer without creating a second AI execution path.

## Objective

Build an interactive frontend with a conversation viewport, incremental streaming, multiline input, header, status bar, activity indicators, terminal resize handling, Ctrl+C and Ctrl+D semantics, slash commands, and a confirmation abstraction ready for future safe actions. The frontend must remain a thin event consumer over the same AiTurnService used by JSON mode.

## Current repository analysis

The repository already uses crossterm in src/commands/explore.rs. Its TerminalGuard enters raw mode and the alternate screen, handles resize, Ctrl+C, navigation, search, history overlays, and cleanup. Reuse the lessons and cleanup guarantees from that implementation, but do not couple the AI state to ExplorerNavigator or catalog selection. Cargo.toml does not currently include ratatui; ratatui with its crossterm integration is the recommended rendering foundation if it fits the supported targets.

src/output.rs has global output mode and progress types intended for command output. The interactive AI frontend should consume domain events directly and render them locally; it must not rely on generic progress strings for the conversation transcript. In JSON mode this frontend must never initialize raw terminal mode or emit ANSI.

## UI architecture

Create an AiInteractiveApp with explicit state:

- conversation summary and active model/runtime status;
- a bounded list of rendered transcript blocks with roles and message IDs;
- viewport scroll position and whether the user is following the newest output;
- multiline composer text, cursor position, and optional editing history;
- generation state, cancellation handle, activity phase, and error banner;
- a command palette or slash-command parser;
- a ConfirmationRequest/ConfirmationResult interface that can later be supplied by the restore SafetyGate.

Render from state on each event rather than writing arbitrary strings at cursor positions. Wrap text based on the current terminal width, preserve code blocks and long paths without overflowing, and distinguish user, assistant, system, error, and activity content using accessible text plus color where available. Keep a minimum-width fallback that remains usable in a narrow terminal.

The header should show a short conversation title/ID, model ID, and connection/runtime state. The status bar should show generation activity, cancellation hint, context/budget information when available, and slash-command help. Never show hidden chain-of-thought. If a future tool trace is displayed, show only safe phase names and concise progress, not raw arguments, secrets, or arbitrary tool output.

## Input and event behavior

Support:

- Enter to submit a non-empty message;
- a documented multiline key, such as Shift+Enter or Ctrl+J, without making newline insertion platform-dependent;
- cursor movement, delete/backspace, and scrolling;
- PageUp/PageDown or equivalent viewport navigation;
- Ctrl+C to cancel an active generation and clear or preserve the composer according to a documented policy; a second Ctrl+C may exit;
- Ctrl+D to exit only when the composer is empty and no protected confirmation is active; otherwise delete a character or remain in the UI;
- terminal resize events that recompute layout and redraw without losing text or stream state;
- EOF/non-TTY behavior that exits cleanly rather than leaving raw mode enabled.

Stream token events must update the current assistant block without flicker or duplicated text. Backpressure from the model worker must not freeze key handling. Use an event loop that multiplexes terminal events and backend events, and ensure all blocking model work remains outside the UI thread.

Implement slash commands as a parser and dispatcher, not as prompts sent to the model. At minimum support commands for help, new conversation, list conversations, switch/select, rename, clear viewport, model/runtime status, and exit. Unknown commands should be a local error. Slash commands that mutate state must call ConversationService and update the UI from the resulting event.

## Confirmation abstraction

Define a future-proof request containing action ID, human-readable summary, risk level, affected paths/counts, and an expiry or plan ID. The current frontend may only expose a stub or use it for conversation deletion if required. It must not approve restore actions, invent a safety decision, or block JSON mode. Task 19 will connect the same abstraction to RestorePlan and SafetyGate.

Guarantee cleanup with a guard that restores the previous terminal state on normal return, errors, Ctrl+C, and panic. Reuse or improve the existing TerminalGuard patterns. Provide a non-interactive fallback message when stdin/stdout are not TTYs rather than entering raw mode.

## Tests and acceptance criteria

Add:

- state-machine tests for submit, stream delta, finish, cancel, error, resize, scroll, and exit;
- composer tests for multiline insertion, cursor movement, empty submission, Ctrl+C, and Ctrl+D;
- slash-command parsing and service dispatch tests;
- rendering tests or snapshots for empty, long, wrapped, streaming, error, narrow-width, and scrolled viewports;
- a pseudo-terminal or manual integration test proving raw mode and alternate-screen cleanup on success, error, and interrupt;
- tests proving JSON mode never initializes the interactive frontend and that both modes consume the same event sequence;
- a manual matrix for Linux, macOS, and Windows terminals, including dumb terminals and redirected output.

The task is complete when an interactive user can hold a real conversation with streaming and multiline input, resize or cancel safely, manage conversations without leaving the screen, and exit without terminal corruption. The frontend must not duplicate persistence, model loading, or orchestration logic.

## References

- [ratatui documentation](https://docs.rs/ratatui/latest/ratatui/) — immediate-mode terminal rendering and crossterm integration.
- [crossterm documentation](https://docs.rs/crossterm/latest/crossterm/) — terminal events, raw mode, alternate screen, and resize handling.
- [GIB explorer frontend](../src/commands/explore.rs) — existing TerminalGuard and terminal lifecycle behavior.

