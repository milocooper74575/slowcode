//! TUI state, rendering and key handling.

use std::sync::Arc;

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers, MouseButton, MouseEvent, MouseEventKind};
use ratatui::{
    Frame,
    buffer::Buffer,
    layout::{Alignment, Constraint, Direction, Layout, Position, Rect},
    style::{Color, Modifier, Style},
    text::{Line, Span},
    widgets::{Paragraph, Wrap},
};
use slowcode_app::workflow::{ApprovalDecision, ApprovalRecord, ApprovalStatus, WorkflowStore};
use slowcode_app::{
    Command, CommandCompletion, CredentialSourceKind, CredentialStatus, Runtime, SessionCommand,
    command_specs, complete, complete_answer, credential_status, escape_terminal, parse_command,
    remove_api_key, save_api_key,
};
use slowcode_feedback::{BundlePreview, BundleRequest, clip, preview, write_bundle};
use slowcode_kernel::ui::{
    UiEvent, UiEventKind, UiMessageSource, UiTodo, UiToolFailure, UiTurnEnd, UiUsage,
};
use slowcode_mcp::{
    McpAddRequest, McpConfig, McpResolver, McpScope, McpServerConfig, McpSession, McpTransport,
    McpTrustStore, parse_add_request,
};
use slowcode_skills::{InstallOptions, InstallPreview, SkillInstaller, SkillScope};
use tokio::sync::{Mutex, mpsc};
use tokio_util::sync::CancellationToken;

use crate::approval::ApprovalMessage;

/// Result of handling one key.
pub struct KeyAction {
    pub quit: bool,
    pub new_events: Option<slowcode_kernel::ui::UiEventReceiver>,
}

impl KeyAction {
    fn none() -> Self {
        Self {
            quit: false,
            new_events: None,
        }
    }

    fn quit() -> Self {
        Self {
            quit: true,
            new_events: None,
        }
    }
}

/// Messages sent from the background turn task.
#[derive(Debug)]
pub enum AppEvent {
    TurnFinished(Result<(), String>),
    PlanFinished(Result<String, String>),
    McpResolved(Result<McpServerConfig, String>),
    McpTested {
        server: McpServerConfig,
        result: Result<Vec<String>, String>,
    },
    McpRefreshed(Result<String, String>),
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum RunState {
    Idle,
    Streaming,
    Tool,
    Approval,
    Error,
}

impl RunState {
    fn label(&self) -> &'static str {
        match self {
            Self::Idle => "idle",
            Self::Streaming => "streaming",
            Self::Tool => "tool",
            Self::Approval => "approval",
            Self::Error => "error",
        }
    }

    fn color(&self) -> Color {
        match self {
            Self::Idle => Color::Green,
            Self::Streaming => Color::Yellow,
            Self::Tool => Color::Cyan,
            Self::Approval => Color::Magenta,
            Self::Error => Color::Red,
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum MessageBlock {
    User(String),
    Assistant(String),
    Reasoning(String),
    Prompt(String),
    Tool {
        call_id: String,
        name: String,
        arguments: String,
        status: String,
        result: Option<String>,
        is_error: bool,
    },
    System(String),
    Error(String),
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum LiveKind {
    Assistant,
    Reasoning,
}

impl LiveKind {
    fn block(self, text: String) -> MessageBlock {
        match self {
            Self::Assistant => MessageBlock::Assistant(text),
            Self::Reasoning => MessageBlock::Reasoning(text),
        }
    }

    fn buffer_mut(self, block: Option<&mut MessageBlock>) -> Option<&mut String> {
        match (self, block) {
            (Self::Assistant, Some(MessageBlock::Assistant(buffer))) => Some(buffer),
            (Self::Reasoning, Some(MessageBlock::Reasoning(buffer))) => Some(buffer),
            _ => None,
        }
    }
}

/// Append a streamed delta to the live block, creating the block on first use.
fn append_live(
    messages: &mut Vec<MessageBlock>,
    slot: &mut Option<usize>,
    kind: LiveKind,
    text: &str,
) {
    if text.is_empty() {
        return;
    }
    let index = match *slot {
        Some(index) => index,
        None => {
            messages.push(kind.block(String::new()));
            let index = messages.len() - 1;
            *slot = Some(index);
            index
        }
    };
    if let Some(buffer) = kind.buffer_mut(messages.get_mut(index)) {
        buffer.push_str(text);
    }
}

/// Replace the live block with the authoritative final text, or append it when
/// no delta stream opened a block.
fn replace_live(
    messages: &mut Vec<MessageBlock>,
    slot: &mut Option<usize>,
    kind: LiveKind,
    text: &str,
) {
    if let Some(index) = slot.take() {
        if let Some(buffer) = kind.buffer_mut(messages.get_mut(index)) {
            *buffer = text.to_owned();
        }
    } else if !text.is_empty() {
        messages.push(kind.block(text.to_owned()));
    }
}

fn tool_result_value(failure: Option<&UiToolFailure>, content: String) -> String {
    match failure {
        Some(failure) => format!("{}: {}", failure.code, content),
        None => content,
    }
}

/// Fill in the newest matching running tool block and return whether one was
/// found.
fn update_tool_result(
    messages: &mut [MessageBlock],
    call_id: &str,
    is_error: bool,
    result_text: String,
) -> bool {
    for block in messages.iter_mut().rev() {
        if let MessageBlock::Tool {
            call_id: existing,
            status,
            result,
            is_error: existing_error,
            ..
        } = block
            && existing == call_id
        {
            *status = if is_error { "error" } else { "done" }.to_owned();
            *existing_error = is_error;
            *result = Some(result_text);
            return true;
        }
    }
    false
}

#[derive(Clone, Debug, Default)]
pub struct UsageTotals {
    pub input_tokens: u64,
    pub output_tokens: u64,
    pub cache_read_tokens: u64,
    pub cache_write_tokens: u64,
}

struct PendingApproval {
    message: ApprovalMessage,
    details: bool,
}

struct PendingSkillInstall {
    source: std::path::PathBuf,
    scope: SkillScope,
    force: bool,
    name: Option<String>,
}

struct PendingFeedback {
    session_id: String,
    journal: Vec<u8>,
    workspace: std::path::PathBuf,
    model: String,
    title: Option<String>,
    note: Option<String>,
    plan: Option<std::path::PathBuf>,
    prompt: String,
}

/// Masked credential entry opened by `/login`.
struct PendingLogin {
    replacing: bool,
}

#[derive(Debug)]
struct SkillInstallRequest {
    path: String,
    scope: SkillScope,
    force: bool,
    name: Option<String>,
    dry_run: bool,
}

#[derive(Debug)]
struct SkillRemoveRequest {
    name: String,
    scope: SkillScope,
}

#[derive(Debug, Eq, PartialEq)]
struct FeedbackRequest {
    title: Option<String>,
    note: Option<String>,
    plan: Option<std::path::PathBuf>,
}

impl FeedbackRequest {
    fn is_empty(&self) -> bool {
        self.title.is_none() && self.note.is_none() && self.plan.is_none()
    }
}

struct Selection {
    anchor: (u16, u16),
    cursor: (u16, u16),
}

enum PendingMcpInstall {
    Test { server: McpServerConfig },
    Save { server: McpServerConfig },
    Activate { server: McpServerConfig },
}

pub struct App {
    runtime: Arc<Mutex<Runtime>>,
    session_id: String,
    model: String,
    role: String,
    state: RunState,
    messages: Vec<MessageBlock>,
    assistant_live: Option<usize>,
    reasoning_live: Option<usize>,
    composer: String,
    composer_cursor: usize,
    history: Vec<String>,
    history_cursor: Option<usize>,
    scroll: u16,
    follow_tail: bool,
    last_max_scroll: u16,
    pending_approval: Option<PendingApproval>,
    recovered_approval: Option<ApprovalRecord>,
    pending_skill: Option<PendingSkillInstall>,
    pending_feedback: Option<PendingFeedback>,
    pending_login: Option<PendingLogin>,
    pending_mcp: Option<PendingMcpInstall>,
    mcp_busy: bool,
    running: bool,
    cancel: Option<CancellationToken>,
    usage: UsageTotals,
    last_prompt_tokens: u64,
    context_window: Option<u64>,
    message_area: Rect,
    message_screen: Vec<String>,
    selection: Option<Selection>,
    had_turn: bool,
    handoff_running: bool,
    quit_after_turn: bool,
    app_tx: mpsc::UnboundedSender<AppEvent>,
}

impl App {
    pub fn new(
        runtime: Arc<Mutex<Runtime>>,
        session_id: String,
        model: String,
        role: String,
        history: Vec<UiEvent>,
        app_tx: mpsc::UnboundedSender<AppEvent>,
    ) -> Self {
        let mut app = Self {
            runtime,
            session_id,
            model,
            role,
            state: RunState::Idle,
            messages: Vec::new(),
            assistant_live: None,
            reasoning_live: None,
            composer: String::new(),
            composer_cursor: 0,
            history: Vec::new(),
            history_cursor: None,
            scroll: 0,
            follow_tail: true,
            last_max_scroll: 0,
            pending_approval: None,
            recovered_approval: None,
            pending_skill: None,
            pending_feedback: None,
            pending_login: None,
            pending_mcp: None,
            mcp_busy: false,
            running: false,
            cancel: None,
            usage: UsageTotals::default(),
            last_prompt_tokens: 0,
            context_window: None,
            message_area: Rect::default(),
            message_screen: Vec::new(),
            selection: None,
            had_turn: false,
            handoff_running: false,
            quit_after_turn: false,
            app_tx,
        };
        for event in history {
            app.handle_ui_event(event);
        }
        if app.messages.is_empty() {
            app.push(MessageBlock::System(
                "slowcode TUI. Type a task, or /help for commands.".to_owned(),
            ));
        } else {
            app.push(MessageBlock::System(format!(
                "resumed session: {}",
                app.session_id
            )));
        }
        let status = credential_status(&app.workspace());
        if !status.configured() {
            app.push(MessageBlock::System(format!(
                "no DeepSeek API key is configured. Type /login to enter one; it is saved to {} and never printed.",
                status.env_path.display()
            )));
        }
        app.context_window = app
            .runtime
            .try_lock()
            .ok()
            .and_then(|runtime| runtime.context_window());
        app.refresh_identity();
        app.recover_pending_approval();
        app
    }

    fn recover_pending_approval(&mut self) {
        let store = WorkflowStore::new(&self.workspace());
        let Ok(Some(state)) = store.load_active() else {
            return;
        };
        let Ok(records) = store.pending_approvals(&state.workflow_id) else {
            return;
        };
        let Some(record) = records.into_iter().next() else {
            return;
        };
        self.push(MessageBlock::Prompt(format!(
            "approval pending: {}\n{}",
            record.tool_name,
            escape_terminal(&record.preview)
        )));
        self.push(MessageBlock::System(
            "y allow, n reject, d details".to_owned(),
        ));
        self.recovered_approval = Some(record);
        self.state = RunState::Approval;
    }

    fn refresh_identity(&mut self) {
        if let Ok(runtime) = self.runtime.try_lock() {
            self.session_id = runtime.session_id().to_owned();
            self.model = runtime.model().to_owned();
            if let Some(role) = runtime.role() {
                self.role = role.as_str().to_owned();
            }
        }
    }

    pub fn handle_approval(&mut self, message: ApprovalMessage) {
        self.state = RunState::Approval;
        let text = approval_text(&message, false);
        self.pending_approval = Some(PendingApproval {
            message,
            details: false,
        });
        self.messages.push(MessageBlock::Prompt(text));
        self.scroll_to_bottom();
    }

    pub fn handle_app_event(&mut self, event: AppEvent) {
        match event {
            AppEvent::TurnFinished(result) => {
                self.running = false;
                self.cancel = None;
                self.pending_approval = None;
                self.state = RunState::Idle;
                if self.handoff_running {
                    self.handoff_running = false;
                    self.quit_after_turn = true;
                }
                if let Err(error) = result {
                    self.state = RunState::Error;
                    self.push(MessageBlock::Error(format!("turn failed: {error}")));
                }
            }
            AppEvent::PlanFinished(result) => {
                self.running = false;
                self.cancel = None;
                self.pending_approval = None;
                self.state = RunState::Idle;
                match result {
                    Ok(output) => {
                        if !output.trim().is_empty() {
                            self.push(MessageBlock::System(output));
                        }
                    }
                    Err(error) => {
                        self.state = RunState::Error;
                        self.push(MessageBlock::Error(error));
                    }
                }
            }
            AppEvent::McpResolved(result) => {
                self.mcp_busy = false;
                match result {
                    Ok(server) if self.mcp_name_taken(&server.name) => {
                        self.push(MessageBlock::Error(format!(
                            "server {} already exists; remove it first",
                            server.name
                        )))
                    }
                    Ok(server) => {
                        self.messages
                            .push(MessageBlock::Prompt(mcp_preview_text(&server)));
                        self.scroll_to_bottom();
                        self.pending_mcp = Some(PendingMcpInstall::Test { server });
                    }
                    Err(error) => self.push(MessageBlock::Error(format!("mcp resolve: {error}"))),
                }
            }
            AppEvent::McpTested { server, result } => {
                self.mcp_busy = false;
                match result {
                    Ok(tools) => {
                        self.messages
                            .push(MessageBlock::Prompt(mcp_tested_text(&server.name, &tools)));
                        self.scroll_to_bottom();
                        self.pending_mcp = Some(PendingMcpInstall::Save { server });
                    }
                    Err(error) => {
                        self.push(MessageBlock::Error(format!("MCP {}: {error}", server.name)));
                    }
                }
            }
            AppEvent::McpRefreshed(result) => {
                self.mcp_busy = false;
                match result {
                    Ok(summary) => self.push(MessageBlock::System(summary)),
                    Err(error) => self.push(MessageBlock::Error(format!("mcp reload: {error}"))),
                }
            }
        }
    }

    pub fn handle_ui_event(&mut self, event: UiEvent) {
        match event.kind {
            kind @ (UiEventKind::TurnStart { .. }
            | UiEventKind::AssistantTextDelta { .. }
            | UiEventKind::AssistantReasoningDelta { .. }
            | UiEventKind::AssistantMessage { .. }
            | UiEventKind::UserMessage { .. }
            | UiEventKind::ToolRequested { .. }
            | UiEventKind::ToolResult { .. }) => self.handle_stream_event(kind),
            UiEventKind::UsageSample { usage, .. } => {
                self.record_usage(&usage);
                self.last_prompt_tokens = usage
                    .input_tokens
                    .saturating_add(usage.cache_read_tokens.unwrap_or(0));
            }
            UiEventKind::RequestContextChanged {
                context_window,
                provider,
                model,
            } => self.apply_request_context(context_window, model, provider),
            kind @ (UiEventKind::TurnEnd { .. }
            | UiEventKind::RetryScheduled { .. }
            | UiEventKind::ApprovalAsked { .. }
            | UiEventKind::ApprovalDecided { .. }) => self.handle_lifecycle_event(kind),
            kind @ (UiEventKind::CompactionStarted { .. }
            | UiEventKind::CompactionEnded { .. }
            | UiEventKind::CompactionSummarized { .. }) => self.handle_compaction_event(kind),
            UiEventKind::TodoWrite { todos } => self.push_todo_summary(&todos),
            UiEventKind::StepStart { .. }
            | UiEventKind::StepEnd { .. }
            | UiEventKind::CompactionPruneMarked { .. }
            | UiEventKind::RetryStarted { .. }
            | UiEventKind::TypeOnly { .. } => {}
        }
    }

    fn handle_stream_event(&mut self, kind: UiEventKind) {
        match kind {
            UiEventKind::TurnStart { .. } => {
                self.running = true;
                self.state = RunState::Streaming;
                self.assistant_live = None;
                self.reasoning_live = None;
            }
            UiEventKind::AssistantTextDelta { text, .. } => {
                self.append_stream_text(LiveKind::Assistant, &text, true);
            }
            UiEventKind::AssistantReasoningDelta { text, .. } => {
                self.append_stream_text(LiveKind::Reasoning, &text, false);
            }
            UiEventKind::AssistantMessage { message, .. } => {
                replace_live(
                    &mut self.messages,
                    &mut self.assistant_live,
                    LiveKind::Assistant,
                    &message.text,
                );
                replace_live(
                    &mut self.messages,
                    &mut self.reasoning_live,
                    LiveKind::Reasoning,
                    &message.reasoning,
                );
                self.state = RunState::Streaming;
                self.stick_to_bottom();
            }
            UiEventKind::UserMessage { source, text, .. } => {
                if matches!(source, UiMessageSource::Human) {
                    self.messages.push(MessageBlock::User(text));
                    self.stick_to_bottom();
                }
            }
            UiEventKind::ToolRequested {
                call_id,
                name,
                arguments,
                ..
            } => {
                self.state = RunState::Tool;
                self.messages.push(MessageBlock::Tool {
                    call_id,
                    name,
                    arguments,
                    status: "running".to_owned(),
                    result: None,
                    is_error: false,
                });
                self.stick_to_bottom();
            }
            UiEventKind::ToolResult {
                call_id,
                is_error,
                content,
                failure,
                ..
            } => {
                let value = tool_result_value(failure.as_ref(), content);
                update_tool_result(&mut self.messages, &call_id, is_error, value);
                self.state = RunState::Streaming;
                self.stick_to_bottom();
            }
            _ => {}
        }
    }

    fn append_stream_text(&mut self, kind: LiveKind, text: &str, mark_streaming: bool) {
        if text.is_empty() {
            return;
        }
        let slot = match kind {
            LiveKind::Assistant => &mut self.assistant_live,
            LiveKind::Reasoning => &mut self.reasoning_live,
        };
        if mark_streaming {
            self.state = RunState::Streaming;
        }
        append_live(&mut self.messages, slot, kind, text);
        if mark_streaming {
            self.stick_to_bottom();
        }
    }

    fn apply_request_context(
        &mut self,
        context_window: Option<u64>,
        model: Option<String>,
        provider: Option<String>,
    ) {
        if let Some(value) = context_window {
            self.context_window = Some(value);
        }
        if let Some(model) = model {
            self.model = model;
        }
        let _ = provider;
    }

    fn handle_lifecycle_event(&mut self, kind: UiEventKind) {
        match kind {
            UiEventKind::TurnEnd { reason, .. } => self.finish_turn(reason),
            UiEventKind::RetryScheduled {
                retry,
                delay_ms,
                failure_code,
                ..
            } => {
                self.push_background(MessageBlock::System(format!(
                    "retry {retry} in {delay_ms} ms after {failure_code}"
                )));
            }
            UiEventKind::ApprovalAsked { .. } => {
                self.state = RunState::Approval;
            }
            UiEventKind::ApprovalDecided { outcome, .. } => {
                self.state = RunState::Streaming;
                self.push_background(MessageBlock::System(format!("approval: {outcome}")));
            }
            _ => {}
        }
    }

    fn finish_turn(&mut self, reason: UiTurnEnd) {
        self.running = false;
        self.cancel = None;
        self.state = RunState::Idle;
        if let UiTurnEnd::Error { code, message } = reason {
            self.state = RunState::Error;
            self.push(MessageBlock::Error(format!("{code}: {message}")));
        }
        self.pending_approval = None;
        self.assistant_live = None;
        self.reasoning_live = None;
    }

    fn handle_compaction_event(&mut self, kind: UiEventKind) {
        match kind {
            UiEventKind::CompactionStarted { .. } => {
                self.push_background(MessageBlock::System("compacting session".to_owned()));
            }
            UiEventKind::CompactionEnded { error, .. } => {
                if let Some(error) = error {
                    self.push(MessageBlock::Error(format!(
                        "compaction: {}",
                        error.message
                    )));
                } else {
                    self.push_background(MessageBlock::System("compaction finished".to_owned()));
                }
            }
            UiEventKind::CompactionSummarized {
                shadowed_tokens, ..
            } => {
                self.push_background(MessageBlock::System(format!(
                    "compaction shadowed {shadowed_tokens} tokens"
                )));
            }
            _ => {}
        }
    }

    fn push_todo_summary(&mut self, todos: &[UiTodo]) {
        let text = todos
            .iter()
            .map(|todo| format!("[{}] {}", todo.status, todo.content))
            .collect::<Vec<_>>()
            .join("\n");
        if !text.is_empty() {
            self.push_background(MessageBlock::System(text));
        }
    }

    pub async fn handle_key(&mut self, key: KeyEvent) -> KeyAction {
        if let Some(action) = self.handle_control_key(&key) {
            return action;
        }
        match key.code {
            KeyCode::Enter => self.submit().await,
            KeyCode::Tab => {
                if let Some(completion) = self.completion() {
                    self.composer = completion.full;
                    self.composer_cursor = self.composer.len();
                }
                KeyAction::none()
            }
            code @ (KeyCode::Backspace
            | KeyCode::Delete
            | KeyCode::Left
            | KeyCode::Right
            | KeyCode::Up
            | KeyCode::Down
            | KeyCode::PageUp
            | KeyCode::PageDown) => {
                self.handle_edit_key(code);
                KeyAction::none()
            }
            code @ (KeyCode::Home | KeyCode::End) => {
                self.handle_home_end(code);
                KeyAction::none()
            }
            KeyCode::Char(character)
                if !key.modifiers.contains(KeyModifiers::CONTROL)
                    && !key.modifiers.contains(KeyModifiers::ALT) =>
            {
                self.insert_char(character);
                KeyAction::none()
            }
            _ => KeyAction::none(),
        }
    }

    fn handle_control_key(&mut self, key: &KeyEvent) -> Option<KeyAction> {
        if key.kind != crossterm::event::KeyEventKind::Press {
            return Some(KeyAction::none());
        }
        if key.modifiers.contains(KeyModifiers::CONTROL) && key.code == KeyCode::Char('c') {
            return Some(KeyAction::quit());
        }
        if is_newline_key(key) {
            self.insert_str("\n");
            return Some(KeyAction::none());
        }
        if key.code == KeyCode::Esc {
            if self.pending_login.is_some() {
                self.cancel_login();
                return Some(KeyAction::none());
            }
            self.cancel_current_turn();
            return Some(KeyAction::none());
        }
        None
    }

    fn cancel_current_turn(&mut self) {
        if self.running
            && let Some(cancel) = &self.cancel
        {
            cancel.cancel();
            self.push(MessageBlock::System("当前会话已中断".to_owned()));
        }
    }

    fn handle_edit_key(&mut self, code: KeyCode) {
        match code {
            KeyCode::Backspace => self.backspace(),
            KeyCode::Delete => self.delete_forward(),
            KeyCode::Left => self.move_left(),
            KeyCode::Right => self.move_right(),
            KeyCode::Up => self.move_vertical_or_history(-1),
            KeyCode::Down => self.move_vertical_or_history(1),
            KeyCode::PageUp => self.scroll_up(5),
            KeyCode::PageDown => self.scroll_down(5),
            _ => {}
        }
    }

    fn move_vertical_or_history(&mut self, direction: i8) {
        if self.composer.contains('\n') {
            self.move_vertical(direction);
        } else if direction < 0 {
            self.history_up();
        } else {
            self.history_down();
        }
    }

    fn handle_home_end(&mut self, code: KeyCode) {
        if self.composer.is_empty() {
            if code == KeyCode::Home {
                self.scroll_up(u16::MAX);
            } else {
                self.scroll_to_bottom();
            }
        } else if code == KeyCode::Home {
            self.move_line_start();
        } else {
            self.move_line_end();
        }
    }

    fn completion(&self) -> Option<CommandCompletion> {
        if self.composer.starts_with("/session switch ")
            && let Ok(runtime) = self.runtime.try_lock()
            && let Ok(sessions) = runtime.list_sessions()
        {
            let ids = sessions
                .iter()
                .map(|session| session.id().as_str())
                .collect::<Vec<_>>();
            if let Some(completion) = session_completion(&self.composer, &ids, self.composer_cursor)
            {
                return Some(completion);
            }
        }
        if self.composer.starts_with("/answer") {
            let questions = WorkflowStore::new(&self.workspace())
                .load_active()
                .ok()
                .flatten()
                .map(|state| {
                    state
                        .open_questions()
                        .into_iter()
                        .cloned()
                        .collect::<Vec<_>>()
                })
                .unwrap_or_default();
            if let Some(completion) =
                complete_answer(&self.composer, self.composer_cursor, &questions)
            {
                return Some(completion);
            }
        }
        complete(&self.composer, self.composer_cursor)
    }

    fn insert_str(&mut self, text: &str) {
        self.composer.insert_str(self.composer_cursor, text);
        self.composer_cursor += text.len();
    }

    fn insert_char(&mut self, character: char) {
        self.composer.insert(self.composer_cursor, character);
        self.composer_cursor += character.len_utf8();
    }

    fn backspace(&mut self) {
        let Some(previous) = previous_boundary(&self.composer, self.composer_cursor) else {
            return;
        };
        self.composer
            .replace_range(previous..self.composer_cursor, "");
        self.composer_cursor = previous;
    }

    fn delete_forward(&mut self) {
        let Some(next) = next_boundary(&self.composer, self.composer_cursor) else {
            return;
        };
        self.composer.replace_range(self.composer_cursor..next, "");
    }

    fn move_left(&mut self) {
        if let Some(previous) = previous_boundary(&self.composer, self.composer_cursor) {
            self.composer_cursor = previous;
        }
    }

    fn move_right(&mut self) {
        if let Some(next) = next_boundary(&self.composer, self.composer_cursor) {
            self.composer_cursor = next;
        }
    }

    fn move_line_start(&mut self) {
        self.composer_cursor = self.composer[..self.composer_cursor]
            .rfind('\n')
            .map_or(0, |index| index + 1);
    }

    fn move_line_end(&mut self) {
        self.composer_cursor = self.composer[self.composer_cursor..]
            .find('\n')
            .map_or(self.composer.len(), |index| self.composer_cursor + index);
    }

    fn move_vertical(&mut self, direction: i8) {
        let start = self.composer[..self.composer_cursor]
            .rfind('\n')
            .map_or(0, |index| index + 1);
        let column = self.composer[start..self.composer_cursor].chars().count();
        if direction < 0 {
            if start == 0 {
                return;
            }
            let end = start - 1;
            let previous_start = self.composer[..end]
                .rfind('\n')
                .map_or(0, |index| index + 1);
            self.composer_cursor =
                char_offset(&self.composer[previous_start..end], column) + previous_start;
        } else {
            let Some(offset) = self.composer[self.composer_cursor..].find('\n') else {
                return;
            };
            let next_start = self.composer_cursor + offset + 1;
            let next_end = self.composer[next_start..]
                .find('\n')
                .map_or(self.composer.len(), |index| next_start + index);
            self.composer_cursor =
                char_offset(&self.composer[next_start..next_end], column) + next_start;
        }
    }

    async fn submit(&mut self) -> KeyAction {
        let input = self.composer.trim().to_owned();
        if input.is_empty() {
            return KeyAction::none();
        }
        if self.pending_login.is_some() {
            self.composer.clear();
            self.composer_cursor = 0;
            self.finish_login(&input);
            return KeyAction::none();
        }
        if self.pending_approval.is_some()
            || self.pending_skill.is_some()
            || self.pending_feedback.is_some()
            || self.pending_mcp.is_some()
        {
            self.composer.clear();
            self.composer_cursor = 0;
            self.respond_to_prompt(&input).await;
            return KeyAction::none();
        }
        let command = parse_command(&input);
        let needs_credential = command.as_ref().is_none_or(command_needs_credential);
        if needs_credential && !self.credential_ready() {
            return KeyAction::none();
        }
        self.composer.clear();
        self.composer_cursor = 0;
        self.history.push(input.clone());
        self.history_cursor = None;
        self.scroll_to_bottom();

        if let Some(command) = command {
            return self.handle_command(command).await;
        }
        if self.running {
            self.push(MessageBlock::System(
                "a turn is already running; press Ctrl-C to cancel".to_owned(),
            ));
            return KeyAction::none();
        }
        self.ensure_await_user_coach().await;
        self.start_turn(input);
        KeyAction::none()
    }

    /// AwaitUser is read-only. Ordinary prompts act as a coach, which is the
    /// navigator capability with a phase instruction. Switch at the turn
    /// boundary so a driver role cannot write during the user's implementation.
    async fn ensure_await_user_coach(&mut self) {
        let store = WorkflowStore::new(&self.workspace());
        let Ok(Some(state)) = store.load_active() else {
            return;
        };
        if state.phase != slowcode_app::workflow::Phase::AwaitUser || self.role == "navigator" {
            return;
        }
        let switched = {
            let mut runtime = self.runtime.lock().await;
            runtime
                .switch_role(slowcode_roles::RoleName::Navigator)
                .await
        };
        match switched {
            Ok(_) => {
                self.role = "navigator".to_owned();
                self.push_system(
                    "coach mode: navigator (read-only); implement the core regions, then /continue"
                        .to_owned(),
                );
            }
            Err(error) => {
                self.push(MessageBlock::Error(format!("coach mode: {error}")));
            }
        }
    }

    async fn respond_to_prompt(&mut self, input: &str) {
        if let Some(record) = self.recovered_approval.take() {
            self.handle_recovered_approval(input, record);
            self.scroll_to_bottom();
            return;
        }

        if let Some(pending) = self.pending_approval.take() {
            match parse_approval_reply(input) {
                ApprovalReply::Allow => {
                    let _ = pending
                        .message
                        .respond
                        .send(slowcode_kernel::session::ApprovalOutcome::AllowedOnce);
                    self.state = RunState::Streaming;
                }
                ApprovalReply::Reject => {
                    let _ = pending
                        .message
                        .respond
                        .send(slowcode_kernel::session::ApprovalOutcome::Rejected);
                    self.state = RunState::Streaming;
                }
                ApprovalReply::Details => {
                    let details = !pending.details;
                    self.messages.push(MessageBlock::Prompt(approval_text(
                        &pending.message,
                        details,
                    )));
                    self.scroll_to_bottom();
                    self.pending_approval = Some(PendingApproval {
                        message: pending.message,
                        details,
                    });
                }
                ApprovalReply::Invalid => {
                    self.messages.push(MessageBlock::System(
                        "reply y to allow, n to reject, d for details".to_owned(),
                    ));
                    self.scroll_to_bottom();
                    self.pending_approval = Some(pending);
                }
            }
            return;
        }

        if let Some(pending) = self.pending_skill.take() {
            match parse_yes_no(input) {
                YesNoReply::Yes => {
                    self.finish_skill_install(
                        pending.source,
                        pending.scope,
                        pending.force,
                        pending.name,
                    );
                }
                YesNoReply::No => {
                    self.push(MessageBlock::System("skill install cancelled".to_owned()));
                }
                YesNoReply::Invalid => {
                    self.messages.push(MessageBlock::System(
                        "reply y to install, n to cancel".to_owned(),
                    ));
                    self.scroll_to_bottom();
                    self.pending_skill = Some(pending);
                }
            }
            return;
        }

        if let Some(pending) = self.pending_feedback.take() {
            match parse_yes_no(input) {
                YesNoReply::Yes => self.finish_feedback(pending),
                YesNoReply::No => {
                    self.push(MessageBlock::System("feedback cancelled".to_owned()));
                }
                YesNoReply::Invalid => {
                    self.messages
                        .push(MessageBlock::Prompt(pending.prompt.clone()));
                    self.scroll_to_bottom();
                    self.pending_feedback = Some(pending);
                }
            }
            return;
        }

        if let Some(pending) = self.pending_mcp.take() {
            match parse_yes_no(input) {
                YesNoReply::Yes => match pending {
                    PendingMcpInstall::Test { server } => self.start_mcp_test(server),
                    PendingMcpInstall::Save { server } => self.save_mcp_server(server).await,
                    PendingMcpInstall::Activate { server } => {
                        self.finish_mcp_activation(server).await;
                    }
                },
                YesNoReply::No => {
                    self.push(MessageBlock::System("mcp install cancelled".to_owned()));
                }
                YesNoReply::Invalid => {
                    self.messages.push(MessageBlock::System(
                        "reply y to continue, n to cancel".to_owned(),
                    ));
                    self.scroll_to_bottom();
                    self.pending_mcp = Some(pending);
                }
            }
        }
    }

    fn handle_recovered_approval(&mut self, input: &str, record: ApprovalRecord) {
        match parse_approval_reply(input) {
            ApprovalReply::Allow | ApprovalReply::Reject => {
                let decision = if matches!(parse_approval_reply(input), ApprovalReply::Allow) {
                    ApprovalDecision::Approve
                } else {
                    ApprovalDecision::Reject
                };
                self.resolve_recovered_approval(record, decision);
            }
            ApprovalReply::Details => {
                self.push(MessageBlock::Prompt(recovered_approval_details(&record)));
                self.recovered_approval = Some(record);
            }
            ApprovalReply::Invalid => {
                self.push_system(
                    "approval pending: y allow, n reject, d details; empty line keeps it pending"
                        .to_owned(),
                );
                self.recovered_approval = Some(record);
            }
        }
    }

    fn resolve_recovered_approval(&mut self, record: ApprovalRecord, decision: ApprovalDecision) {
        let store = WorkflowStore::new(&self.workspace());
        match store.resolve_approval(
            &record.workflow_id,
            &record.approval_id,
            decision,
            Some("tui"),
            &record.arguments_hash,
        ) {
            Ok((_, resolved)) if resolved.status == ApprovalStatus::Stale => {
                self.push_system(
                    "approval is stale; run the command again to request a fresh one".to_owned(),
                );
            }
            Ok((_, resolved)) => {
                self.push_system(format!("approval: {:?}", resolved.status));
            }
            Err(error) => {
                self.push(MessageBlock::Error(format!("approval: {error}")));
            }
        }
        self.state = RunState::Idle;
    }

    fn start_turn(&mut self, text: String) {
        self.had_turn = true;
        self.start_turn_inner(text);
    }

    fn start_turn_inner(&mut self, text: String) {
        self.running = true;
        self.state = RunState::Streaming;
        let cancel = CancellationToken::new();
        self.cancel = Some(cancel.clone());
        let runtime = Arc::clone(&self.runtime);
        let tx = self.app_tx.clone();
        tokio::spawn(async move {
            let mut runtime = runtime.lock().await;
            let result = runtime
                .run_turn(text, cancel)
                .await
                .map(|_| ())
                .map_err(|error| error.to_string());
            let _ = tx.send(AppEvent::TurnFinished(result));
        });
    }

    fn request_quit(&mut self) -> KeyAction {
        if self.running {
            return KeyAction::quit();
        }
        if !self.had_turn {
            return KeyAction::quit();
        }
        if self.handoff_running {
            return KeyAction::none();
        }
        let workspace = self.workspace();
        let skill_path = workspace.join(".jcode/skills/handoff/SKILL.md");
        let Ok(skill) = std::fs::read_to_string(&skill_path) else {
            self.push(MessageBlock::System(
                "handoff skill not found; exiting".to_owned(),
            ));
            return KeyAction::quit();
        };
        let prompt = handoff_prompt(&workspace, &skill);
        self.handoff_running = true;
        self.start_turn_inner(prompt);
        KeyAction::none()
    }

    fn start_plan(&mut self, command: slowcode_app::plan::PlanCommand) {
        let label = command.label();
        let is_plan = matches!(command, slowcode_app::plan::PlanCommand::Plan(_));
        self.running = true;
        self.state = RunState::Streaming;
        let cancel = CancellationToken::new();
        self.cancel = Some(cancel.clone());
        let runtime = Arc::clone(&self.runtime);
        let tx = self.app_tx.clone();
        self.push_system(format!("running /{label}"));
        tokio::spawn(async move {
            let mut runtime = runtime.lock().await;
            let result = runtime
                .run_plan_command(command, cancel)
                .await
                .map(|outcome| {
                    let mut text = outcome.output;
                    if is_plan {
                        text.push_str(&format!("\n\n[plan saved to {}]", outcome.path.display()));
                    }
                    text
                })
                .map_err(|error| error.to_string());
            let _ = tx.send(AppEvent::PlanFinished(result));
        });
    }

    async fn handle_command(&mut self, command: Command) -> KeyAction {
        match command {
            Command::Model(Some(model)) => {
                self.switch_model(model).await;
                KeyAction::none()
            }
            Command::Session(command) => self.handle_session_command(command).await,
            Command::Role(role) => self.handle_role_command(role).await,
            Command::Skills(argument) => self.handle_skills(argument.as_deref()).await,
            Command::Mcp(argument) => self.handle_mcp(argument.as_deref()).await,
            Command::Feedback(argument) => self.handle_feedback(&argument).await,
            Command::Quit => self.request_quit(),
            Command::Plan(_)
            | Command::Answer(_)
            | Command::Scaffold
            | Command::Continue
            | Command::Test
            | Command::Fix
            | Command::Execute
            | Command::Distill(_)
            | Command::Approve
            | Command::Status(_)
            | Command::Next
            | Command::Workflow(_)
            | Command::Reclassify(_)
            | Command::Hint(_) => self.handle_plan_command(command).await,
            other => self.handle_info_command(other),
        }
    }

    async fn handle_session_command(&mut self, command: SessionCommand) -> KeyAction {
        match command {
            SessionCommand::List => {
                self.session_report().await;
                KeyAction::none()
            }
            SessionCommand::New => self.new_session_command().await,
            SessionCommand::Switch(id) => self.switch_session(&id).await,
            SessionCommand::Rename(title) => self.rename_session_command(title).await,
        }
    }

    async fn new_session_command(&mut self) -> KeyAction {
        if self.running {
            self.push(MessageBlock::System(
                "cancel the current turn first".to_owned(),
            ));
            return KeyAction::none();
        }
        let outcome = {
            let mut runtime = self.runtime.lock().await;
            match runtime.new_session().await {
                Ok(events) => Ok((events, runtime.session_id().to_owned())),
                Err(error) => Err(error),
            }
        };
        match outcome {
            Ok((events, session_id)) => {
                self.session_id = session_id;
                self.messages.clear();
                self.push(MessageBlock::System("new session".to_owned()));
                KeyAction {
                    quit: false,
                    new_events: Some(events),
                }
            }
            Err(error) => {
                self.push(MessageBlock::Error(format!("new session: {error}")));
                KeyAction::none()
            }
        }
    }

    async fn rename_session_command(&mut self, title: String) -> KeyAction {
        let outcome = {
            let mut runtime = self.runtime.lock().await;
            runtime.rename_session(&title).await
        };
        match outcome {
            Ok(Some(title)) => {
                self.push(MessageBlock::System(format!("session renamed: {title}")));
            }
            Ok(None) => self.push(MessageBlock::Error(
                "session title is empty or too long".to_owned(),
            )),
            Err(error) => self.push(MessageBlock::Error(format!("session rename: {error}"))),
        }
        KeyAction::none()
    }

    async fn handle_role_command(&mut self, role: Option<slowcode_roles::RoleName>) -> KeyAction {
        let Some(role) = role else {
            self.push(MessageBlock::System(format!("role: {}", self.role)));
            return KeyAction::none();
        };
        if let Some(phase) = self.active_phase()
            && !role_switch_allowed(Some(phase), role)
        {
            let desired = phase_role(phase);
            self.push(MessageBlock::Error(format!(
                "phase {} uses {}; switch refused",
                phase,
                desired.as_str()
            )));
            return KeyAction::none();
        }
        if self.running {
            self.push(MessageBlock::System(
                "switch roles at a turn boundary".to_owned(),
            ));
            return KeyAction::none();
        }
        let outcome = {
            let mut runtime = self.runtime.lock().await;
            runtime.switch_role(role).await
        };
        match outcome {
            Ok(report) => {
                self.role = role.as_str().to_owned();
                self.push(MessageBlock::System(if report.changed {
                    format!("role: {} ({})", role.as_str(), role_capability(role))
                } else {
                    format!(
                        "role unchanged: {} ({})",
                        role.as_str(),
                        role_capability(role)
                    )
                }));
                for warning in report.warnings {
                    self.push(MessageBlock::System(format!(
                        "role switch warning: {warning}"
                    )));
                }
            }
            Err(error) => self.push(MessageBlock::Error(format!("role switch: {error}"))),
        }
        KeyAction::none()
    }

    fn active_phase(&self) -> Option<slowcode_app::workflow::Phase> {
        WorkflowStore::new(&self.workspace())
            .load_active()
            .ok()
            .flatten()
            .map(|state| state.phase)
    }

    async fn ensure_plan_role(&mut self, command: &Command) -> bool {
        let desired = match self.active_phase() {
            Some(phase) => phase_role(phase),
            None if matches!(command, Command::Plan(_)) => slowcode_roles::RoleName::Navigator,
            None => return true,
        };
        if self.role == desired.as_str() {
            return true;
        }
        let outcome = {
            let mut runtime = self.runtime.lock().await;
            runtime.switch_role(desired).await
        };
        match outcome {
            Ok(_) => {
                self.role = desired.as_str().to_owned();
                self.push(MessageBlock::System(format!(
                    "role: {} ({})",
                    desired.as_str(),
                    phase_role_reason(self.active_phase())
                )));
                true
            }
            Err(error) => {
                self.push(MessageBlock::Error(format!("role switch: {error}")));
                false
            }
        }
    }

    async fn handle_plan_command(&mut self, command: Command) -> KeyAction {
        if self.running {
            self.push(MessageBlock::System(
                "cancel the current command first".to_owned(),
            ));
            return KeyAction::none();
        }
        if !self.ensure_plan_role(&command).await {
            return KeyAction::none();
        }
        let plan_command = match command {
            Command::Plan(task) => slowcode_app::plan::PlanCommand::Plan(task),
            Command::Answer(answer) => slowcode_app::plan::PlanCommand::Answer(answer),
            Command::Scaffold => slowcode_app::plan::PlanCommand::Scaffold,
            Command::Continue => slowcode_app::plan::PlanCommand::Continue,
            Command::Test => slowcode_app::plan::PlanCommand::Test,
            Command::Fix => slowcode_app::plan::PlanCommand::Fix,
            Command::Execute => slowcode_app::plan::PlanCommand::Execute,
            Command::Distill(explanation) => slowcode_app::plan::PlanCommand::Distill(explanation),
            Command::Approve => slowcode_app::plan::PlanCommand::Approve,
            Command::Status(workflow) => slowcode_app::plan::PlanCommand::Status(workflow),
            Command::Next => slowcode_app::plan::PlanCommand::Next,
            Command::Workflow(argument) => slowcode_app::plan::PlanCommand::Workflow(argument),
            Command::Reclassify(text) => slowcode_app::plan::PlanCommand::Reclassify(text),
            Command::Hint(question) => slowcode_app::plan::PlanCommand::Hint(question),
            _ => return KeyAction::none(),
        };
        self.start_plan(plan_command);
        KeyAction::none()
    }

    fn handle_info_command(&mut self, command: Command) -> KeyAction {
        match command {
            Command::Help(argument) => {
                self.push(MessageBlock::System(help_text(argument.as_deref())));
            }
            Command::Model(None) => {
                self.push(MessageBlock::System(model_report(&self.model)));
            }
            Command::Context(argument) => {
                self.context_report(argument.as_deref());
            }
            Command::Cache => {
                self.push(MessageBlock::System(format!(
                    "cache: {} read / {} written; prompt {} / completion {}",
                    self.usage.cache_read_tokens,
                    self.usage.cache_write_tokens,
                    self.usage.input_tokens,
                    self.usage.output_tokens
                )));
            }
            Command::Login => self.handle_login(),
            Command::Logout => self.handle_logout(),
            Command::Unknown(name) => {
                self.push(MessageBlock::Error(format!("unknown command: {name}")));
            }
            _ => {}
        }
        KeyAction::none()
    }

    fn handle_login(&mut self) {
        let status = credential_status(&self.workspace());
        self.push(MessageBlock::System(credential_report(&status)));
        self.start_login(status.configured());
    }

    fn start_login(&mut self, replacing: bool) {
        self.composer.clear();
        self.composer_cursor = 0;
        let status = credential_status(&self.workspace());
        let path = status.env_path.display();
        let prompt = if replacing {
            format!(
                "DEEPSEEK_API_KEY is configured. Paste a new key to replace it, or press Esc to keep the current key. The key is saved to {path} and never printed."
            )
        } else {
            format!(
                "no DeepSeek API key is configured. Paste your DeepSeek API key and press Enter. The key is saved to {path} with mode 0600 and never printed. Esc cancels."
            )
        };
        self.pending_login = Some(PendingLogin { replacing });
        self.push(MessageBlock::Prompt(prompt));
    }

    fn finish_login(&mut self, value: &str) {
        let replacing = self
            .pending_login
            .as_ref()
            .is_some_and(|pending| pending.replacing);
        match save_api_key(&self.workspace(), value) {
            Ok(path) => {
                self.pending_login = None;
                let verb = if replacing { "replaced" } else { "saved" };
                self.push(MessageBlock::System(format!(
                    "credential {verb}: {}; the next request uses it, no restart needed",
                    path.display()
                )));
            }
            Err(error) => {
                self.push(MessageBlock::Error(format!(
                    "credential not saved: {error}; try again or press Esc"
                )));
            }
        }
    }

    fn cancel_login(&mut self) {
        if self.pending_login.take().is_some() {
            self.composer.clear();
            self.composer_cursor = 0;
            self.push(MessageBlock::System(
                "credential entry cancelled".to_owned(),
            ));
        }
    }

    fn handle_logout(&mut self) {
        let status = credential_status(&self.workspace());
        match remove_api_key(&self.workspace()) {
            Ok(true) => {
                let mut message = format!(
                    "removed DEEPSEEK_API_KEY from {}",
                    status.env_path.display()
                );
                if std::env::var_os(slowcode_app::API_KEY_ENV).is_some() {
                    message.push_str(
                        "; this process still has DEEPSEEK_API_KEY in its environment, so restart slowcode to clear it (unset it in your shell too if you exported it there)",
                    );
                }
                self.push(MessageBlock::System(message));
            }
            Ok(false) if status.configured() => {
                self.push(MessageBlock::System(
                    "the workspace .env has no key; the current credential comes from the environment. Unset DEEPSEEK_API_KEY to log out.".to_owned(),
                ));
            }
            Ok(false) => {
                self.push(MessageBlock::System(
                    "no credential is configured".to_owned(),
                ));
            }
            Err(error) => {
                self.push(MessageBlock::Error(format!("logout: {error}")));
            }
        }
    }

    fn credential_ready(&mut self) -> bool {
        let status = credential_status(&self.workspace());
        if status.configured() {
            return true;
        }
        self.push(MessageBlock::Error(format!(
            "cannot start a turn: no DeepSeek API key is configured. Type /login to enter one; it is saved to {} and never printed.",
            status.env_path.display()
        )));
        false
    }

    async fn switch_model(&mut self, model: String) {
        let outcome = {
            let mut runtime = self.runtime.lock().await;
            runtime.set_model(&model).await
        };
        match outcome {
            Ok(()) => {
                self.model = model;
                self.push(MessageBlock::System(format!("model: {}", self.model)));
            }
            Err(error) => self.push(MessageBlock::Error(format!("model switch: {error}"))),
        }
    }

    async fn switch_session(&mut self, id: &str) -> KeyAction {
        if self.running {
            self.push(MessageBlock::System(
                "cancel the current turn first".to_owned(),
            ));
            return KeyAction::none();
        }
        let outcome = {
            let mut runtime = self.runtime.lock().await;
            match runtime.switch_session(id).await {
                Ok((events, history)) => Ok((
                    events,
                    history,
                    runtime.session_id().to_owned(),
                    runtime.model().to_owned(),
                    runtime.role().map(|role| role.as_str().to_owned()),
                )),
                Err(error) => Err(error),
            }
        };
        match outcome {
            Ok((events, history, session_id, model, role)) => {
                self.session_id = session_id;
                self.model = model;
                if let Some(role) = role {
                    self.role = role;
                }
                self.messages.clear();
                for event in history {
                    self.handle_ui_event(event);
                }
                self.push(MessageBlock::System(format!(
                    "session: {}",
                    self.session_id
                )));
                KeyAction {
                    quit: false,
                    new_events: Some(events),
                }
            }
            Err(error) => {
                self.push(MessageBlock::Error(format!("session switch: {error}")));
                KeyAction::none()
            }
        }
    }

    async fn session_report(&mut self) {
        let result = {
            let runtime = self.runtime.lock().await;
            runtime.list_sessions()
        };
        match result {
            Ok(sessions) => {
                let mut lines = vec!["sessions:".to_owned()];
                for session in sessions {
                    let marker = if session.id().as_str() == self.session_id {
                        "*"
                    } else {
                        " "
                    };
                    let title = session.title().unwrap_or("untitled");
                    lines.push(format!("{marker} {}  {title}", session.id()));
                }
                lines.push(
                    "usage: /session switch <id> | /session new | /session rename <title>"
                        .to_owned(),
                );
                self.push(MessageBlock::System(lines.join("\n")));
            }
            Err(error) => self.push(MessageBlock::Error(format!("list sessions: {error}"))),
        }
    }

    fn context_report(&mut self, argument: Option<&str>) {
        let reloaded = argument.is_some();
        let breakdown = self
            .runtime
            .try_lock()
            .map(|runtime| runtime.context_breakdown())
            .unwrap_or_default();
        let window = breakdown.window;
        let mut lines = Vec::new();
        match window {
            Some(window) => lines.push(format!(
                "context window: {} tokens  used at last request: {} ({})",
                format_count(window),
                format_count(self.last_prompt_tokens),
                format_percent(self.last_prompt_tokens, window)
            )),
            None => lines.push(format!(
                "context window: unknown  used at last request: {} tokens",
                format_count(self.last_prompt_tokens)
            )),
        }
        lines.push(String::new());
        for slice in &breakdown.slices {
            let percent = window
                .map(|window| format_percent(slice.tokens, window))
                .unwrap_or_else(|| "-".to_owned());
            let note = slice
                .note
                .as_deref()
                .map(|note| format!("  {note}"))
                .unwrap_or_default();
            lines.push(format!(
                "{:<20} {:>8}  {:>7}{note}",
                slice.name,
                format_count(slice.tokens),
                percent
            ));
        }
        if reloaded {
            lines.push(String::new());
            lines.push("context reloaded from disk".to_owned());
        }
        self.push(MessageBlock::System(lines.join("\n")));
    }

    async fn handle_skills(&mut self, argument: Option<&str>) -> KeyAction {
        let workspace = self.workspace();
        let installer = SkillInstaller::new(&workspace);
        let argument = argument.unwrap_or("").trim();
        if let Some(request) = parse_skill_install(argument) {
            self.handle_skill_install(&installer, request);
            return KeyAction::none();
        }
        if let Some(request) = parse_skill_remove(argument) {
            let request = match request {
                Ok(request) => request,
                Err(error) => {
                    self.push(MessageBlock::Error(error.to_owned()));
                    return KeyAction::none();
                }
            };
            match installer.remove(&request.name, &request.scope) {
                Ok(()) => self.push(MessageBlock::System(format!(
                    "skill removed: {} [{}]",
                    request.name,
                    skill_scope_label(&request.scope)
                ))),
                Err(error) => self.push(MessageBlock::Error(format!("skill remove: {error}"))),
            }
            return KeyAction::none();
        }
        match installer.list() {
            Ok(skills) if skills.is_empty() => self.push(MessageBlock::System(
                "no skills installed; use /skills install <skill-dir>".to_owned(),
            )),
            Ok(skills) => {
                let lines = skills
                    .into_iter()
                    .map(|skill| format!("{} [{}] {}", skill.name, skill.scope, skill.description))
                    .collect::<Vec<_>>()
                    .join("\n");
                self.push(MessageBlock::System(lines));
            }
            Err(error) => self.push(MessageBlock::Error(format!("skill list: {error}"))),
        }
        KeyAction::none()
    }

    fn handle_skill_install(
        &mut self,
        installer: &SkillInstaller,
        request: Result<SkillInstallRequest, &'static str>,
    ) {
        let request = match request {
            Ok(request) => request,
            Err(error) => {
                self.push(MessageBlock::Error(error.to_owned()));
                return;
            }
        };
        let options = InstallOptions {
            force: request.force,
            name: request.name.clone(),
            dry_run: request.dry_run,
            ..InstallOptions::default()
        };
        match installer.preview(&request.path, &request.scope, &options) {
            Ok(preview) => {
                self.messages.push(MessageBlock::Prompt(skill_preview_text(
                    &preview,
                    &request.scope,
                    request.dry_run,
                )));
                self.scroll_to_bottom();
                if !request.dry_run {
                    self.pending_skill = Some(PendingSkillInstall {
                        source: std::path::PathBuf::from(request.path),
                        scope: request.scope,
                        force: request.force,
                        name: request.name,
                    });
                }
            }
            Err(error) => self.push(MessageBlock::Error(format!("skill preview: {error}"))),
        }
    }

    fn finish_skill_install(
        &mut self,
        source: std::path::PathBuf,
        scope: SkillScope,
        force: bool,
        name: Option<String>,
    ) {
        let installer = SkillInstaller::new(self.workspace());
        let options = InstallOptions {
            force,
            name,
            ..InstallOptions::default()
        };
        match installer.install(&source, &scope, &options) {
            Ok(outcome) => self.push(MessageBlock::System(format!(
                "skill installed: {} [{}] ({} files, {} bytes)",
                outcome.preview.name,
                skill_scope_label(&scope),
                outcome.preview.files,
                outcome.preview.bytes
            ))),
            Err(error) => self.push(MessageBlock::Error(format!("skill install: {error}"))),
        }
    }

    async fn handle_feedback(&mut self, argument: &str) -> KeyAction {
        if self.running {
            self.push(MessageBlock::System(
                "cancel the current turn first".to_owned(),
            ));
            return KeyAction::none();
        }
        let request = match parse_feedback_request(argument) {
            Ok(request) => request,
            Err(error) => {
                self.push(MessageBlock::Error(error.to_owned()));
                return KeyAction::none();
            }
        };
        if request.is_empty() {
            self.push(MessageBlock::System(feedback_hint()));
            return KeyAction::none();
        }
        let snapshot = {
            let mut runtime = self.runtime.lock().await;
            runtime.feedback_snapshot().await
        };
        let snapshot = match snapshot {
            Ok(snapshot) => snapshot,
            Err(error) => {
                self.push(MessageBlock::Error(format!("feedback: {error}")));
                return KeyAction::none();
            }
        };
        let out_dir = snapshot
            .workspace
            .join("feedback")
            .join(&snapshot.session_id);
        let os = format!("{} {}", std::env::consts::OS, std::env::consts::ARCH);
        let workspace = snapshot.workspace.to_string_lossy();
        let candidates = plan_artifacts(&snapshot.workspace);
        let plan = request.plan.clone().or_else(|| match candidates.len() {
            1 => candidates.first().cloned(),
            _ => None,
        });
        let bundle = BundleRequest {
            session_id: &snapshot.session_id,
            journal: &snapshot.journal,
            workspace: Some(workspace.as_ref()),
            version: env!("CARGO_PKG_VERSION"),
            model: Some(&snapshot.model),
            provider: None,
            os: &os,
            out_dir: out_dir.clone(),
            title: request.title.clone(),
            note: request.note.clone(),
            plan: plan.clone(),
        };
        let preview = match preview(&bundle) {
            Ok(preview) => preview,
            Err(error) => {
                self.push(MessageBlock::Error(format!("feedback: {error}")));
                return KeyAction::none();
            }
        };
        let prompt = feedback_preview_text(&preview, candidates.len());
        self.messages.push(MessageBlock::Prompt(prompt.clone()));
        self.scroll_to_bottom();
        self.pending_feedback = Some(PendingFeedback {
            session_id: snapshot.session_id,
            journal: snapshot.journal,
            workspace: snapshot.workspace,
            model: snapshot.model,
            title: request.title,
            note: request.note,
            plan,
            prompt,
        });
        KeyAction::none()
    }

    fn finish_feedback(&mut self, pending: PendingFeedback) {
        let os = format!("{} {}", std::env::consts::OS, std::env::consts::ARCH);
        let workspace = pending.workspace.to_string_lossy();
        let outcome = write_bundle(BundleRequest {
            session_id: &pending.session_id,
            journal: &pending.journal,
            workspace: Some(workspace.as_ref()),
            version: env!("CARGO_PKG_VERSION"),
            model: Some(&pending.model),
            provider: None,
            os: &os,
            out_dir: pending.workspace.join("feedback").join(&pending.session_id),
            title: pending.title,
            note: pending.note,
            plan: pending.plan,
        });
        match outcome {
            Ok(outcome) if outcome.complete => self.push(MessageBlock::System(format!(
                "feedback bundle: {} ({} events, seq {}..{})",
                outcome.out_dir.display(),
                outcome.events,
                outcome
                    .first_seq
                    .map(|seq| seq.to_string())
                    .unwrap_or_else(|| "none".to_owned()),
                outcome
                    .last_seq
                    .map(|seq| seq.to_string())
                    .unwrap_or_else(|| "none".to_owned()),
            ))),
            Ok(outcome) => {
                let reasons = outcome
                    .hard_failures
                    .iter()
                    .map(|reason| format!("  - {reason}"))
                    .collect::<Vec<_>>()
                    .join("\n");
                self.push(MessageBlock::Error(format!(
                    "feedback bundle INCOMPLETE: {}\n{reasons}\nreview issue.md before posting",
                    outcome.out_dir.display()
                )));
            }
            Err(error) => self.push(MessageBlock::Error(format!("feedback: {error}"))),
        }
    }

    async fn handle_mcp(&mut self, argument: Option<&str>) -> KeyAction {
        let workspace = self.workspace();
        let config = match McpConfig::load(&workspace) {
            Ok(config) => config,
            Err(error) => {
                self.push(MessageBlock::Error(format!("mcp config: {error}")));
                return KeyAction::none();
            }
        };
        let argument = argument.unwrap_or("").trim();
        let mut parts = argument.split_whitespace();
        match parts.next() {
            None | Some("list") => self.list_mcp_servers(&config).await,
            Some("add") => {
                let rest = argument.strip_prefix("add").unwrap_or_default().trim();
                self.start_mcp_add(rest);
            }
            Some("test") | Some("tools") => self.test_mcp_command(&config, parts.next()).await,
            Some("enable") => self.enable_mcp(&config, parts.next()).await,
            Some("disable") => self.disable_mcp(parts.next()).await,
            Some("remove") => self.remove_mcp(&config, parts.next()).await,
            Some("reload") => self.refresh_mcp_tools(),
            Some(other) => self.push(MessageBlock::Error(format!(
                "unknown /mcp subcommand: {other}"
            ))),
        }
        KeyAction::none()
    }

    async fn list_mcp_servers(&mut self, config: &McpConfig) {
        let servers = config.list();
        if servers.is_empty() {
            self.push(MessageBlock::System(
                "no MCP servers configured; use /mcp add <name> <command> [args...]".to_owned(),
            ));
            return;
        }
        let active = {
            let runtime = self.runtime.lock().await;
            config
                .servers
                .keys()
                .filter(|name| runtime.is_mcp_active(name))
                .cloned()
                .collect::<Vec<_>>()
        };
        let lines = servers
            .into_iter()
            .map(|server| mcp_server_line(server, &active))
            .collect::<Vec<_>>()
            .join("\n");
        self.push(MessageBlock::System(lines));
    }

    async fn test_mcp_command(&mut self, config: &McpConfig, name: Option<&str>) {
        match resolve_mcp_server(config, name, "test") {
            Ok(server) => self.test_mcp(&server).await,
            Err(error) => self.push(MessageBlock::Error(error)),
        }
    }

    async fn enable_mcp(&mut self, config: &McpConfig, name: Option<&str>) {
        let server = match resolve_mcp_server(config, name, "enable") {
            Ok(server) => server,
            Err(error) => {
                self.push(MessageBlock::Error(error));
                return;
            }
        };
        let already_active = {
            let runtime = self.runtime.lock().await;
            runtime.is_mcp_active(&server.name)
        };
        if already_active {
            self.push(MessageBlock::System(format!(
                "MCP server already active: {}",
                server.name
            )));
            return;
        }
        let trusted = McpTrustStore::open_default()
            .map(|store| store.is_trusted(&server))
            .unwrap_or(false);
        if trusted {
            self.activate_mcp(&server).await;
        } else {
            self.messages
                .push(MessageBlock::Prompt(mcp_activation_text(&server)));
            self.scroll_to_bottom();
            self.pending_mcp = Some(PendingMcpInstall::Activate { server });
        }
    }

    async fn disable_mcp(&mut self, name: Option<&str>) {
        let Some(name) = name else {
            self.push(MessageBlock::Error("usage: /mcp disable <name>".to_owned()));
            return;
        };
        {
            let mut runtime = self.runtime.lock().await;
            runtime.deactivate_mcp_server(name);
        }
        self.push(MessageBlock::System(format!("MCP server disabled: {name}")));
        self.refresh_mcp_tools();
    }

    async fn remove_mcp(&mut self, config: &McpConfig, name: Option<&str>) {
        let Some(name) = name else {
            self.push(MessageBlock::Error("usage: /mcp remove <name>".to_owned()));
            return;
        };
        let removed = config.servers.get(name).cloned();
        match config.remove(McpScope::Project, name) {
            Ok(()) => {
                if let Some(server) = removed
                    && let Ok(store) = McpTrustStore::open_default()
                {
                    let _ = store.forget(&server);
                }
                self.runtime.lock().await.deactivate_mcp_server(name);
                self.push(MessageBlock::System(format!("MCP server removed: {name}")));
                self.refresh_mcp_tools();
            }
            Err(error) => self.push(MessageBlock::Error(format!("mcp remove: {error}"))),
        }
    }

    fn start_mcp_add(&mut self, argument: &str) {
        if self.mcp_busy {
            self.push(MessageBlock::System(
                "an MCP install is already running".to_owned(),
            ));
            return;
        }
        let request = match parse_add_request(argument) {
            Ok(request) => request,
            Err(error) => {
                self.push(MessageBlock::Error(error));
                return;
            }
        };
        match request {
            McpAddRequest::Github { repo } => {
                if self.workspace().join(&repo).exists() {
                    self.push(MessageBlock::Error(format!(
                        "{repo} is a local path; use /mcp add <name> <command>"
                    )));
                    return;
                }
                self.mcp_busy = true;
                self.push(MessageBlock::System(format!("resolving github.com/{repo}")));
                let tx = self.app_tx.clone();
                tokio::spawn(async move {
                    let result = McpResolver::new()
                        .resolve_repo(&repo)
                        .await
                        .map_err(|error| error.to_string());
                    let _ = tx.send(AppEvent::McpResolved(result));
                });
            }
            McpAddRequest::Stdio {
                name,
                command,
                args,
            } => self.preview_mcp_server(McpServerConfig {
                name,
                transport: McpTransport::Stdio {
                    command,
                    args,
                    env: Default::default(),
                },
                trusted: false,
                enabled: true,
                scope: Some(McpScope::Project),
                source: None,
            }),
            McpAddRequest::Http { name, url } => self.preview_mcp_server(McpServerConfig {
                name,
                transport: McpTransport::Http {
                    url,
                    headers: Default::default(),
                },
                trusted: false,
                enabled: true,
                scope: Some(McpScope::Project),
                source: None,
            }),
        }
    }

    fn preview_mcp_server(&mut self, server: McpServerConfig) {
        if self.mcp_name_taken(&server.name) {
            self.push(MessageBlock::Error(format!(
                "server {} already exists; remove it first",
                server.name
            )));
            return;
        }
        self.messages
            .push(MessageBlock::Prompt(mcp_preview_text(&server)));
        self.scroll_to_bottom();
        self.pending_mcp = Some(PendingMcpInstall::Test { server });
    }

    fn start_mcp_test(&mut self, server: McpServerConfig) {
        self.mcp_busy = true;
        self.push(MessageBlock::System(format!("testing {}", server.name)));
        let tx = self.app_tx.clone();
        let tested = server.clone();
        tokio::spawn(async move {
            let result = match McpSession::connect(&tested).await {
                Ok(session) => {
                    let tools = session
                        .tools()
                        .iter()
                        .map(|tool| tool.qualified_name())
                        .collect::<Vec<_>>();
                    session.shutdown().await;
                    Ok(tools)
                }
                Err(error) => Err(error.to_string()),
            };
            let _ = tx.send(AppEvent::McpTested {
                server: tested,
                result,
            });
        });
    }

    async fn save_mcp_server(&mut self, server: McpServerConfig) {
        if self.mcp_name_taken(&server.name) {
            self.push(MessageBlock::Error(format!(
                "server {} already exists; remove it first",
                server.name
            )));
            return;
        }
        let config = match McpConfig::load(&self.workspace()) {
            Ok(config) => config,
            Err(error) => {
                self.push(MessageBlock::Error(format!("mcp config: {error}")));
                return;
            }
        };
        match config.upsert(McpScope::Project, server.clone()) {
            Ok(()) => {
                self.push(MessageBlock::System(format!(
                    "MCP server saved: {}",
                    server.name
                )));
                if let Ok(store) = McpTrustStore::open_default()
                    && let Err(error) = store.remember(&server)
                {
                    self.push(MessageBlock::Error(format!("mcp trust: {error}")));
                }
                self.activate_mcp(&server).await;
            }
            Err(error) => self.push(MessageBlock::Error(format!("mcp add: {error}"))),
        }
    }

    async fn finish_mcp_activation(&mut self, server: McpServerConfig) {
        if let Ok(store) = McpTrustStore::open_default()
            && let Err(error) = store.remember(&server)
        {
            self.push(MessageBlock::Error(format!("mcp trust: {error}")));
        }
        self.activate_mcp(&server).await;
    }

    async fn activate_mcp(&mut self, server: &McpServerConfig) {
        {
            let mut runtime = self.runtime.lock().await;
            runtime.activate_mcp_server(&server.name);
        }
        self.push(MessageBlock::System(format!(
            "MCP server enabled: {}",
            server.name
        )));
        self.refresh_mcp_tools();
    }

    fn refresh_mcp_tools(&mut self) {
        self.mcp_busy = true;
        let runtime = Arc::clone(&self.runtime);
        let tx = self.app_tx.clone();
        tokio::spawn(async move {
            let result = {
                let mut runtime = runtime.lock().await;
                runtime
                    .install_mcp_tools()
                    .await
                    .map(|report| report.summary())
                    .map_err(|error| error.to_string())
            };
            let _ = tx.send(AppEvent::McpRefreshed(result));
        });
    }

    fn mcp_name_taken(&self, name: &str) -> bool {
        McpConfig::load(&self.workspace())
            .map(|config| {
                config
                    .servers
                    .get(name)
                    .is_some_and(|server| server.scope == Some(McpScope::Project))
            })
            .unwrap_or(false)
    }

    async fn test_mcp(&mut self, server: &McpServerConfig) {
        match McpSession::connect(server).await {
            Ok(session) => {
                let tools = session
                    .tools()
                    .iter()
                    .map(|tool| tool.qualified_name())
                    .collect::<Vec<_>>();
                let message = if tools.is_empty() {
                    format!("MCP {} connected; no tools", server.name)
                } else {
                    format!(
                        "MCP {} connected; {} tool(s): {}",
                        server.name,
                        tools.len(),
                        tools.join(", ")
                    )
                };
                session.shutdown().await;
                self.push(MessageBlock::System(message));
            }
            Err(error) => self.push(MessageBlock::Error(format!("MCP {}: {error}", server.name))),
        }
    }

    fn workspace(&self) -> std::path::PathBuf {
        self.runtime
            .try_lock()
            .map(|runtime| runtime.workspace().to_path_buf())
            .unwrap_or_else(|_| std::env::current_dir().unwrap_or_default())
    }

    fn record_usage(&mut self, usage: &UiUsage) {
        self.usage.input_tokens = self.usage.input_tokens.saturating_add(usage.input_tokens);
        self.usage.output_tokens = self.usage.output_tokens.saturating_add(usage.output_tokens);
        self.usage.cache_read_tokens = self
            .usage
            .cache_read_tokens
            .saturating_add(usage.cache_read_tokens.unwrap_or(0));
        self.usage.cache_write_tokens = self
            .usage
            .cache_write_tokens
            .saturating_add(usage.cache_write_tokens.unwrap_or(0));
    }

    fn history_up(&mut self) {
        if self.history.is_empty() {
            return;
        }
        let index = match self.history_cursor {
            Some(index) => index.saturating_sub(1),
            None => self.history.len() - 1,
        };
        self.history_cursor = Some(index);
        self.composer = self.history[index].clone();
        self.composer_cursor = self.composer.len();
    }

    fn history_down(&mut self) {
        let Some(index) = self.history_cursor else {
            return;
        };
        if index + 1 >= self.history.len() {
            self.history_cursor = None;
            self.composer.clear();
        } else {
            self.history_cursor = Some(index + 1);
            self.composer = self.history[index + 1].clone();
        }
        self.composer_cursor = self.composer.len();
    }

    fn push(&mut self, block: MessageBlock) {
        self.messages.push(block);
        self.scroll_to_bottom();
    }

    fn push_background(&mut self, block: MessageBlock) {
        self.messages.push(block);
        self.stick_to_bottom();
    }

    pub fn push_error(&mut self, message: String) {
        self.state = RunState::Error;
        self.push(MessageBlock::Error(message));
    }

    pub fn push_system(&mut self, message: String) {
        self.push(MessageBlock::System(message));
    }

    pub fn cancel_running(&self) {
        if let Some(cancel) = &self.cancel {
            cancel.cancel();
        }
    }

    fn scroll_to_bottom(&mut self) {
        self.follow_tail = true;
        self.scroll = u16::MAX;
    }

    fn scroll_up(&mut self, amount: u16) {
        (self.scroll, self.follow_tail) =
            scroll_up_from(self.scroll, self.follow_tail, self.last_max_scroll, amount);
    }

    fn scroll_down(&mut self, amount: u16) {
        (self.scroll, self.follow_tail) =
            scroll_down_from(self.scroll, self.follow_tail, self.last_max_scroll, amount);
    }

    fn stick_to_bottom(&mut self) {
        if self.follow_tail {
            self.scroll = u16::MAX;
        }
    }

    pub fn handle_mouse_scroll(&mut self, up: bool) {
        if up {
            self.scroll_up(3);
        } else {
            self.scroll_down(3);
        }
    }

    pub fn handle_mouse_event(&mut self, mouse: MouseEvent) {
        match mouse.kind {
            MouseEventKind::ScrollUp => self.handle_mouse_scroll(true),
            MouseEventKind::ScrollDown => self.handle_mouse_scroll(false),
            MouseEventKind::Down(MouseButton::Left) => {
                self.handle_mouse_down(mouse.column, mouse.row);
            }
            MouseEventKind::Drag(MouseButton::Left) => {
                self.handle_mouse_drag(mouse.column, mouse.row);
            }
            MouseEventKind::Up(MouseButton::Left) => {
                self.handle_mouse_up(mouse.column, mouse.row);
            }
            _ => {}
        }
    }

    pub fn handle_mouse_down(&mut self, column: u16, row: u16) {
        let Some(point) = self.selection_point(column, row) else {
            return;
        };
        self.selection = Some(Selection {
            anchor: point,
            cursor: point,
        });
    }

    pub fn handle_mouse_drag(&mut self, column: u16, row: u16) {
        let Some(point) = self.selection_point_clamped(column, row) else {
            return;
        };
        if let Some(selection) = &mut self.selection {
            selection.cursor = point;
        }
    }

    pub fn handle_mouse_up(&mut self, column: u16, row: u16) {
        let Some(point) = self.selection_point_clamped(column, row) else {
            return;
        };
        let Some(mut selection) = self.selection.take() else {
            return;
        };
        selection.cursor = point;
        if selection.anchor == selection.cursor {
            return;
        }
        let text = selected_text(&self.message_screen, &selection);
        if text.is_empty() {
            return;
        }
        match arboard::Clipboard::new().and_then(|mut clipboard| clipboard.set_text(text.clone())) {
            Ok(()) => self.push(MessageBlock::System(format!(
                "已复制 {} 个字符到剪切板！",
                text.chars().count()
            ))),
            Err(error) => self.push(MessageBlock::Error(format!("clipboard: {error}"))),
        }
    }

    fn selection_point(&self, column: u16, row: u16) -> Option<(u16, u16)> {
        let area = self.message_area;
        if area.width == 0 || area.height == 0 {
            return None;
        }
        if column < area.x
            || column >= area.x.saturating_add(area.width)
            || row < area.y
            || row >= area.y.saturating_add(area.height)
        {
            return None;
        }
        Some((row - area.y, column - area.x))
    }

    fn selection_point_clamped(&self, column: u16, row: u16) -> Option<(u16, u16)> {
        let area = self.message_area;
        if area.width == 0 || area.height == 0 {
            return None;
        }
        let x = column
            .saturating_sub(area.x)
            .min(area.width.saturating_sub(1));
        let y = row
            .saturating_sub(area.y)
            .min(area.height.saturating_sub(1));
        Some((y, x))
    }

    pub fn should_quit(&self) -> bool {
        self.quit_after_turn
    }

    pub fn paste(&mut self, text: &str) {
        let text = text.replace("\r\n", "\n").replace('\r', "\n");
        self.insert_str(&text);
    }

    pub fn draw(&mut self, frame: &mut Frame) {
        let area = frame.area();
        let composer_height = composer_height(&self.composer);
        let chunks = Layout::default()
            .direction(Direction::Vertical)
            .constraints([
                Constraint::Length(1),
                Constraint::Min(3),
                Constraint::Length(composer_height),
                Constraint::Length(1),
            ])
            .split(area);

        self.draw_status(frame, chunks[0]);
        self.draw_messages(frame, chunks[1]);
        self.draw_composer(frame, chunks[2]);
        self.draw_footer(frame, chunks[3]);
    }

    fn draw_status(&self, frame: &mut Frame, area: Rect) {
        let mut spans = vec![
            Span::styled(" slowcode ", Style::default().add_modifier(Modifier::BOLD)),
            Span::raw(" "),
            Span::styled(self.state.label(), Style::default().fg(self.state.color())),
            Span::raw("  "),
            Span::styled(&self.model, Style::default().fg(Color::Cyan)),
            Span::raw("  "),
            Span::styled(&self.role, Style::default().fg(Color::Magenta)),
            Span::raw("  "),
            Span::styled(
                short_session(&self.session_id),
                Style::default().fg(Color::DarkGray),
            ),
            Span::raw("  "),
            Span::raw(format!(
                "tok {}/{}  cache {}/{}",
                self.usage.input_tokens,
                self.usage.output_tokens,
                self.usage.cache_read_tokens,
                self.usage.cache_write_tokens
            )),
        ];
        if let Some(window) = self.context_window {
            spans.push(Span::raw(format!("  ctx {window}")));
        }
        if self.recovered_approval.is_some() {
            spans.push(Span::styled(
                "  approval pending",
                Style::default().fg(Color::Magenta),
            ));
        }
        let status = Line::from(spans);
        frame.render_widget(Paragraph::new(status), area);
    }

    fn draw_messages(&mut self, frame: &mut Frame, area: Rect) {
        let lines = message_lines(&self.messages);
        let width = area.width.max(1);
        let widget = Paragraph::new(lines).wrap(Wrap { trim: false });
        let content_height = widget.line_count(width).min(u16::MAX as usize) as u16;
        let max_scroll = content_height.saturating_sub(area.height);
        self.last_max_scroll = max_scroll;
        if self.follow_tail {
            self.scroll = max_scroll;
        } else {
            self.scroll = self.scroll.min(max_scroll);
        }
        frame.render_widget(widget.scroll((self.scroll, 0)), area);
        self.message_area = area;
        self.message_screen = snapshot_area(frame.buffer_mut(), area);
        if let Some(selection) = &self.selection {
            highlight_selection(frame.buffer_mut(), area, selection);
        }
    }

    fn draw_composer(&self, frame: &mut Frame, area: Rect) {
        let (display, cursor, completion) = if self.pending_login.is_some() {
            let masked = masked_input(&self.composer);
            let cursor = masked.len();
            (masked, cursor, None)
        } else {
            (
                self.composer.clone(),
                self.composer_cursor,
                self.completion(),
            )
        };
        let view = composer_view(&display, cursor, completion.as_ref(), area);
        let widget = Paragraph::new(view.lines).scroll((0, view.horizontal_scroll));
        frame.render_widget(widget, area);
        if let Some(completion) = &completion {
            let text = match completion.description.as_deref() {
                Some(description) => format!("tab 补全 · {description}"),
                None => "tab 键补全".to_owned(),
            };
            let width = (Line::from(text.as_str()).width() as u16).saturating_add(1);
            if area.width >= width {
                let hint = Rect::new(area.x + area.width - width, view.cursor_y, width, 1);
                frame.render_widget(
                    Paragraph::new(Line::from(Span::styled(
                        text,
                        Style::default().fg(Color::DarkGray),
                    )))
                    .alignment(Alignment::Right),
                    hint,
                );
            }
        }
        frame.set_cursor_position(Position::new(view.cursor_x, view.cursor_y));
    }

    fn draw_footer(&self, frame: &mut Frame, area: Rect) {
        let hint = if self.handoff_running {
            "writing HANDOFF.md  Ctrl-C quit"
        } else if self.recovered_approval.is_some() {
            "approval pending: y allow, n reject, d details, empty line later"
        } else if self.pending_login.is_some() {
            "credential entry: Enter save, Esc cancel  (input is masked)"
        } else if self.pending_approval.is_some() {
            "approval: type y or n, then Enter. d shows details"
        } else if self.pending_skill.is_some() {
            "skill install: type y or n, then Enter"
        } else if self.pending_feedback.is_some() {
            "feedback: type y to write or n to cancel, then Enter"
        } else if self.running {
            "Esc interrupt  PgUp/PgDn scroll  Ctrl-C quit"
        } else {
            "Shift+Enter newline  PgUp/PgDn scroll  Ctrl-C quit"
        };
        let progress = self.context_progress(hint.chars().count(), area.width);
        let width = area.width as usize;
        let left = hint.chars().count();
        let right = progress.chars().count();
        let padding = width.saturating_sub(left + right).max(1);
        let line = Line::from(vec![
            Span::styled(hint, Style::default().fg(Color::DarkGray)),
            Span::raw(" ".repeat(padding)),
            Span::styled(progress, Style::default().fg(Color::DarkGray)),
        ]);
        frame.render_widget(Paragraph::new(line).alignment(Alignment::Left), area);
    }

    fn context_progress(&self, left: usize, width: u16) -> String {
        let bar_width = (width as usize).saturating_sub(left + 10).clamp(4, 24);
        let window = self.context_window.or_else(|| {
            self.runtime
                .try_lock()
                .ok()
                .and_then(|runtime| runtime.context_window())
        });
        let Some(window) = window.filter(|window| *window > 0) else {
            return format!("ctx [{}] --", "░".repeat(bar_width));
        };
        let ratio = (self.last_prompt_tokens as f64 / window as f64).clamp(0.0, 1.0);
        let filled = (ratio * bar_width as f64).round() as usize;
        format!(
            "ctx [{}{}] {:.1}%",
            "█".repeat(filled),
            "░".repeat(bar_width.saturating_sub(filled)),
            ratio * 100.0
        )
    }
}

fn message_lines(messages: &[MessageBlock]) -> Vec<Line<'static>> {
    let mut lines = Vec::new();
    for block in messages {
        if !lines.is_empty() {
            lines.push(Line::default());
        }
        lines.extend(block_lines(block));
    }
    lines
}

fn block_lines(block: &MessageBlock) -> Vec<Line<'static>> {
    let mut lines = Vec::new();
    match block {
        MessageBlock::User(text) => push_user_text(&mut lines, text, Color::Cyan),
        MessageBlock::Assistant(text) => {
            push_text_aligned(&mut lines, text, Color::Reset, Alignment::Left);
        }
        MessageBlock::Reasoning(text) => {
            lines.push(Line::from(Span::styled(
                "thinking",
                Style::default().fg(Color::DarkGray),
            )));
            push_text(&mut lines, text, Color::DarkGray);
        }
        MessageBlock::Prompt(text) => push_text(&mut lines, text, Color::Yellow),
        MessageBlock::Tool {
            name,
            arguments,
            status,
            result,
            is_error,
            ..
        } => push_tool_lines(&mut lines, name, arguments, status, result, *is_error),
        MessageBlock::System(text) => push_text(&mut lines, text, Color::DarkGray),
        MessageBlock::Error(text) => {
            lines.push(Line::from(Span::styled(
                "error",
                Style::default().fg(Color::Red).add_modifier(Modifier::BOLD),
            )));
            push_text(&mut lines, text, Color::Red);
        }
    }
    lines
}

fn push_tool_lines(
    lines: &mut Vec<Line<'static>>,
    name: &str,
    arguments: &str,
    status: &str,
    result: &Option<String>,
    is_error: bool,
) {
    lines.push(Line::from(vec![
        Span::styled("tool ", Style::default().fg(Color::Yellow)),
        Span::styled(
            name.to_owned(),
            Style::default().add_modifier(Modifier::BOLD),
        ),
        Span::raw(" "),
        Span::styled(
            status.to_owned(),
            Style::default().fg(if is_error { Color::Red } else { Color::Green }),
        ),
    ]));
    if !arguments.is_empty() {
        push_text(lines, arguments, Color::DarkGray);
    }
    if let Some(result) = result {
        push_text(
            lines,
            result,
            if is_error { Color::Red } else { Color::Gray },
        );
    }
}

fn push_text(lines: &mut Vec<Line<'static>>, text: &str, color: Color) {
    push_text_aligned(lines, text, color, Alignment::Left);
}

fn push_user_text(lines: &mut Vec<Line<'static>>, text: &str, color: Color) {
    for (index, line) in text.lines().enumerate() {
        let prefix = if index == 0 { "> " } else { "  " };
        lines.push(
            Line::from(Span::styled(
                format!("{prefix}{line}"),
                Style::default().fg(color),
            ))
            .alignment(Alignment::Left),
        );
    }
}

fn push_text_aligned(
    lines: &mut Vec<Line<'static>>,
    text: &str,
    color: Color,
    alignment: Alignment,
) {
    for line in text.lines() {
        lines.push(
            Line::from(Span::styled(line.to_owned(), Style::default().fg(color)))
                .alignment(alignment),
        );
    }
}

fn help_text(argument: Option<&str>) -> String {
    if let Some(argument) = argument {
        let name = if argument.starts_with('/') {
            argument.to_owned()
        } else {
            format!("/{argument}")
        };
        return command_specs()
            .iter()
            .find(|spec| spec.name == name)
            .map_or_else(
                || format!("unknown command: {argument}\ntry /help"),
                |spec| command_help_text(spec.name, spec.args, spec.description),
            );
    }

    let mut lines = vec!["commands".to_owned()];
    for spec in command_specs() {
        lines.push(format!(
            "{:<10} {:<26} {}",
            spec.name, spec.args, spec.description
        ));
    }
    lines.push(String::new());
    lines.push("keys".to_owned());
    for line in [
        "Enter send, Shift+Enter or Ctrl-J newline",
        "Left/Right move the input cursor, Home/End line start and end",
        "Tab accept the inline command completion",
        "Esc interrupt the running turn",
        "Ctrl-C quit immediately",
        "PgUp/PgDn or mouse wheel scroll",
        "Home/End move the cursor when input is non-empty",
        "Up/Down browse input history, or move between lines",
        "drag in the message area to copy the selection",
        "/quit runs the handoff skill, writes HANDOFF.md, then exits",
    ] {
        lines.push(line.to_owned());
    }
    lines.join("\n")
}

fn command_help_text(name: &str, args: &str, description: &str) -> String {
    let mut text = format!("{name}\n{description}\nusage: {name} {args}");
    match name {
        "/model" => text.push_str("\nexample: /model deepseek-v4-pro"),
        "/session" => text.push_str(
            "\nexample: /session switch session-<uuid>\n         /session rename demo",
        ),
        "/skills" => text.push_str(
            "\nexample: /skills install ./path/to/skill\n         /skills install ./path/to/skill --role navigator\n         /skills remove skill-name --global",
        ),
        "/feedback" => text.push_str(
            "\nfree text becomes the note; its first line becomes the title\n  --plan PATH   attach a plan artifact\n  --title TEXT  override the derived title\n  --note TEXT   same as typing free text\nexample: /feedback the plan never paused\n         /feedback --plan .scratch/plans/<artifact>.md",
        ),
        "/mcp" => text.push_str(
            "\nexample: /mcp add ChromeDevTools/chrome-devtools-mcp\n         /mcp enable chrome-devtools\n         /mcp add fs npx -y @modelcontextprotocol/server-filesystem",
        ),
        "/login" => text.push_str(
            "\nalways opens masked entry; the current source is shown first\nsaves to the workspace .env with mode 0600\nexample: /login",
        ),
        "/logout" => text.push_str("\nremoves DEEPSEEK_API_KEY from the workspace .env"),
        "/plan" => text.push_str("\nexample: /plan add a line-count summary to notes.txt"),
        _ => {}
    }
    text
}

fn model_report(current: &str) -> String {
    let mut lines = vec!["models:".to_owned()];
    for model in slowcode_app::known_models() {
        let id = model.id();
        let marker = if id == current { "*" } else { " " };
        lines.push(format!("{marker} {id}"));
    }
    lines.push("usage: /model <name>".to_owned());
    lines.join("\n")
}

fn role_capability(role: slowcode_roles::RoleName) -> &'static str {
    if role.spec().is_read_only() {
        "read only"
    } else {
        "read and write"
    }
}

fn phase_role(phase: slowcode_app::workflow::Phase) -> slowcode_roles::RoleName {
    use slowcode_app::workflow::Phase;
    match phase {
        Phase::Scaffold | Phase::TestFix => slowcode_roles::RoleName::Driver,
        Phase::Classify
        | Phase::Interview
        | Phase::Reclassify
        | Phase::Review
        | Phase::AwaitUser
        | Phase::Distill
        | Phase::Done
        | Phase::Blocked => slowcode_roles::RoleName::Navigator,
    }
}

fn role_switch_allowed(
    phase: Option<slowcode_app::workflow::Phase>,
    role: slowcode_roles::RoleName,
) -> bool {
    phase.is_none_or(|phase| role == phase_role(phase))
}

fn phase_role_reason(phase: Option<slowcode_app::workflow::Phase>) -> &'static str {
    phase.map_or("plan", |phase| phase.as_str())
}

fn skill_scope_label(scope: &SkillScope) -> String {
    match scope {
        SkillScope::Project => "project".to_owned(),
        SkillScope::Global => "global".to_owned(),
        SkillScope::Role(role) => format!("role {}", role.as_str()),
    }
}

fn skill_preview_text(preview: &InstallPreview, scope: &SkillScope, dry_run: bool) -> String {
    let mut lines = vec![format!(
        "skill install: {} [{}]",
        preview.name,
        skill_scope_label(scope)
    )];
    if let Some(from) = &preview.renamed_from {
        lines.push(format!("rename: {from} -> {}", preview.name));
    }
    lines.push(format!("description: {}", preview.description));
    lines.push(format!("source: {}", preview.source.display()));
    lines.push(format!("target: {}", preview.target.display()));
    lines.push(format!(
        "files: {}  bytes: {}{}",
        preview.files,
        preview.bytes,
        if preview.overwrites {
            "  (overwrites existing)"
        } else {
            ""
        }
    ));
    lines.push(if dry_run {
        "dry run: nothing was written".to_owned()
    } else {
        "reply y to install, n to cancel".to_owned()
    });
    lines.join("\n")
}

const SKILL_INSTALL_USAGE: &str = "usage: /skills install <skill-dir> [--global | --role navigator|driver] [--name NAME] [--force] [--dry-run]";
const SKILL_REMOVE_USAGE: &str =
    "usage: /skills remove <name> [--global | --role navigator|driver]";

fn parse_skill_install(argument: &str) -> Option<Result<SkillInstallRequest, &'static str>> {
    let source = argument.strip_prefix("install ")?.trim();
    Some(
        parse_skill_flags(source, SKILL_INSTALL_USAGE, true).map(|flags| SkillInstallRequest {
            path: flags.rest,
            scope: flags.scope,
            force: flags.force,
            name: flags.name,
            dry_run: flags.dry_run,
        }),
    )
}

fn parse_skill_remove(argument: &str) -> Option<Result<SkillRemoveRequest, &'static str>> {
    let name = argument.strip_prefix("remove ")?.trim();
    Some(
        parse_skill_flags(name, SKILL_REMOVE_USAGE, false).map(|flags| SkillRemoveRequest {
            name: flags.rest,
            scope: flags.scope,
        }),
    )
}

struct SkillFlags {
    scope: SkillScope,
    rest: String,
    force: bool,
    name: Option<String>,
    dry_run: bool,
}

/// Parse trailing `--force`, `--dry-run`, `--name`, `--global` and `--role` flags in any order.
///
/// `rest` is everything left, which may still contain spaces.
fn parse_skill_flags(
    input: &str,
    usage: &'static str,
    allow_force: bool,
) -> Result<SkillFlags, &'static str> {
    let mut raw = RawSkillFlags::default();
    let mut rest = input;
    while let Some((flag, prefix)) = take_skill_flag(rest, usage, allow_force)? {
        raw.apply(flag);
        rest = prefix;
    }

    let scope = resolve_skill_scope(raw.global, raw.role)?;
    let rest = validate_skill_rest(rest, usage)?;
    Ok(SkillFlags {
        scope,
        rest: rest.to_owned(),
        force: raw.force,
        name: raw.name,
        dry_run: raw.dry_run,
    })
}

fn resolve_skill_scope(
    global: bool,
    role: Option<slowcode_roles::RoleName>,
) -> Result<SkillScope, &'static str> {
    if global && role.is_some() {
        return Err("use either --global or --role, not both");
    }
    Ok(match (global, role) {
        (true, _) => SkillScope::Global,
        (_, Some(role)) => SkillScope::Role(role),
        _ => SkillScope::Project,
    })
}

fn validate_skill_rest<'a>(rest: &'a str, usage: &'static str) -> Result<&'a str, &'static str> {
    let rest = rest.trim();
    if rest.is_empty() || rest.starts_with("--") || has_skill_flag_token(rest) {
        return Err(usage);
    }
    Ok(rest)
}

fn has_skill_flag_token(rest: &str) -> bool {
    rest.split_whitespace().any(|token| {
        matches!(
            token,
            "--force" | "--dry-run" | "--global" | "--name" | "--role"
        ) || token.starts_with("--name=")
            || token.starts_with("--role=")
    })
}

enum SkillFlag {
    Force,
    DryRun,
    Name(String),
    Global,
    Role(slowcode_roles::RoleName),
}

#[derive(Default)]
struct RawSkillFlags {
    force: bool,
    dry_run: bool,
    name: Option<String>,
    global: bool,
    role: Option<slowcode_roles::RoleName>,
}

impl RawSkillFlags {
    fn apply(&mut self, flag: SkillFlag) {
        match flag {
            SkillFlag::Force => self.force = true,
            SkillFlag::DryRun => self.dry_run = true,
            SkillFlag::Name(value) => self.name = Some(value),
            SkillFlag::Global => self.global = true,
            SkillFlag::Role(value) => self.role = Some(value),
        }
    }
}

/// Strip one trailing flag. Returns the flag and the remaining prefix.
fn take_skill_flag<'a>(
    rest: &'a str,
    usage: &'static str,
    allow_force: bool,
) -> Result<Option<(SkillFlag, &'a str)>, &'static str> {
    let trimmed = rest.trim_end();
    if let Some(prefix) = trimmed.strip_suffix(" --force") {
        return if allow_force {
            Ok(Some((SkillFlag::Force, prefix)))
        } else {
            Err(usage)
        };
    }
    if let Some(prefix) = trimmed.strip_suffix(" --dry-run") {
        return if allow_force {
            Ok(Some((SkillFlag::DryRun, prefix)))
        } else {
            Err(usage)
        };
    }
    if let Some(prefix) = trimmed.strip_suffix(" --global") {
        return Ok(Some((SkillFlag::Global, prefix)));
    }
    if let Some((prefix, value)) = named_flag_value(trimmed, "name", "missing name after --name")? {
        return if allow_force {
            Ok(Some((SkillFlag::Name(value), prefix)))
        } else {
            Err(usage)
        };
    }
    take_role_flag(trimmed)
}

fn take_role_flag(trimmed: &str) -> Result<Option<(SkillFlag, &str)>, &'static str> {
    let Some((prefix, value)) = named_flag_value(
        trimmed,
        "role",
        "missing role after --role; use navigator or driver",
    )?
    else {
        return Ok(None);
    };
    let Some(role) = slowcode_roles::RoleName::parse(&value) else {
        return Err("unknown role; use navigator or driver");
    };
    Ok(Some((SkillFlag::Role(role), prefix)))
}

/// Parse `--flag value` or `--flag=value` at the end of the input.
fn named_flag_value<'a>(
    trimmed: &'a str,
    flag: &str,
    missing: &'static str,
) -> Result<Option<(&'a str, String)>, &'static str> {
    if trimmed.ends_with(&format!(" --{flag}")) {
        return Err(missing);
    }
    let equals = format!(" --{flag}=");
    if let Some((prefix, value)) = trimmed.rsplit_once(&equals) {
        let value = value.trim();
        if value.split_whitespace().count() != 1 {
            return Ok(None);
        }
        return Ok(Some((prefix, value.to_owned())));
    }
    let separator = format!(" --{flag} ");
    let Some((prefix, value)) = trimmed.rsplit_once(&separator) else {
        return Ok(None);
    };
    let value = value.trim();
    if value.split_whitespace().count() != 1 {
        return Ok(None);
    }
    Ok(Some((prefix, value.to_owned())))
}

const FEEDBACK_USAGE: &str = "usage: /feedback [--title TEXT] [--note TEXT] [--plan PATH] [text]";

fn feedback_hint() -> String {
    "feedback: type the problem in plain words, or add --plan <path>\nexample: /feedback the plan never paused\n         /feedback --plan .scratch/plans/<artifact>.md".to_owned()
}

fn feedback_preview_text(preview: &BundlePreview, candidates: usize) -> String {
    let mut lines = vec![
        format!("feedback bundle: {}", preview.title),
        format!(
            "note: {}",
            preview
                .note
                .as_deref()
                .map(|note| clip(note, 200))
                .unwrap_or_else(|| "none".to_owned())
        ),
    ];
    match &preview.plan {
        Some(plan) => lines.push(format!("plan: {}", plan.display())),
        None if candidates > 1 => lines.push(format!(
            "plan: none selected ({candidates} artifacts found; add --plan <path> to choose)"
        )),
        None => lines.push("plan: none".to_owned()),
    }
    lines.push(format!("out: {}", preview.out_dir.display()));
    if preview.complete {
        lines.push("completeness: complete".to_owned());
    } else {
        lines.push(format!(
            "completeness: INCOMPLETE ({})",
            preview.hard_failures.join("; ")
        ));
    }
    for warning in &preview.warnings {
        lines.push(format!("warning: {warning}"));
    }
    lines.push("reply y to write, n to cancel".to_owned());
    lines.join("\n")
}

fn plan_artifacts(workspace: &std::path::Path) -> Vec<std::path::PathBuf> {
    let directory = workspace.join(".scratch/plans");
    let Ok(entries) = std::fs::read_dir(directory) else {
        return Vec::new();
    };
    let mut plans = entries
        .flatten()
        .map(|entry| entry.path())
        .filter(|path| path.extension().is_some_and(|extension| extension == "md"))
        .collect::<Vec<_>>();
    plans.sort();
    plans
}

fn parse_feedback_request(argument: &str) -> Result<FeedbackRequest, &'static str> {
    let words = split_feedback_words(argument)?;
    let mut title = None;
    let mut note = None;
    let mut plan = None;
    let mut free = Vec::new();
    let mut iter = words.into_iter();
    while let Some(word) = iter.next() {
        let mut take_value = || iter.next().ok_or(FEEDBACK_USAGE);
        match word.as_str() {
            "--title" => title = Some(take_value()?),
            "--note" => note = Some(take_value()?),
            "--plan" => plan = Some(std::path::PathBuf::from(take_value()?)),
            other if other.starts_with("--title=") => {
                title = Some(other.trim_start_matches("--title=").to_owned())
            }
            other if other.starts_with("--note=") => {
                note = Some(other.trim_start_matches("--note=").to_owned())
            }
            other if other.starts_with("--plan=") => {
                plan = Some(std::path::PathBuf::from(
                    other.trim_start_matches("--plan="),
                ))
            }
            other if other.starts_with("--") => return Err(FEEDBACK_USAGE),
            other => free.push(other.to_owned()),
        }
    }
    if !free.is_empty() {
        if note.is_some() {
            return Err("use either free text or --note, not both");
        }
        note = Some(free.join(" "));
    }
    Ok(FeedbackRequest { title, note, plan })
}

fn split_feedback_words(input: &str) -> Result<Vec<String>, &'static str> {
    let mut words = Vec::new();
    let mut current = String::new();
    let mut quote: Option<char> = None;
    let mut started = false;
    for character in input.chars() {
        match quote {
            Some(active) if character == active => quote = None,
            Some(_) => current.push(character),
            None if character == '"' || character == '\'' => {
                quote = Some(character);
                started = true;
            }
            None if character.is_whitespace() => {
                if started {
                    words.push(std::mem::take(&mut current));
                    started = false;
                }
            }
            None => {
                current.push(character);
                started = true;
            }
        }
    }
    if quote.is_some() {
        return Err(FEEDBACK_USAGE);
    }
    if started {
        words.push(current);
    }
    Ok(words)
}

fn mcp_transport_label(transport: &McpTransport) -> String {
    match transport {
        McpTransport::Stdio { command, args, .. } => {
            format!("stdio: {command} {}", args.join(" "))
        }
        McpTransport::Http { url, .. } => format!("http: {url}"),
    }
}

fn mcp_server_line(server: &McpServerConfig, active: &[String]) -> String {
    let state = if !server.enabled {
        "disabled"
    } else if active.contains(&server.name) {
        "active"
    } else {
        "dormant"
    };
    let mut line = format!(
        "{} [{}] {} {} {}",
        server.name,
        slowcode_mcp::scope_label(server.scope, "unknown"),
        state,
        mcp_transport_label(&server.transport),
        if server.trusted { "trusted" } else { "ask" },
    );
    if let Some(source) = &server.source {
        line.push_str(&format!("\n  source {}", source.label()));
    }
    line
}

fn resolve_mcp_server(
    config: &McpConfig,
    name: Option<&str>,
    subcommand: &str,
) -> Result<McpServerConfig, String> {
    let Some(name) = name else {
        return Err(format!("usage: /mcp {subcommand} <name>"));
    };
    let server = config
        .servers
        .get(name)
        .cloned()
        .ok_or_else(|| format!("unknown MCP server: {name}"))?;
    if !server.enabled {
        return Err(format!(
            "MCP server {name} is disabled in the configuration"
        ));
    }
    Ok(server)
}

fn mcp_preview_text(server: &McpServerConfig) -> String {
    let mut lines = vec![format!("MCP {}", server.name)];
    match &server.source {
        Some(source) => lines.push(format!("source  {}", source.label())),
        None => lines.push("source  manual".to_owned()),
    }
    match &server.transport {
        McpTransport::Stdio { command, args, .. } => {
            let mut command = command.clone();
            for argument in args {
                command.push(' ');
                command.push_str(argument);
            }
            lines.push(format!("command {command}"));
        }
        McpTransport::Http { url, .. } => lines.push(format!("url     {url}")),
    }
    lines.push("scope   project".to_owned());
    lines.push("Run connection test? [y/n]".to_owned());
    lines.join("\n")
}

fn mcp_tested_text(name: &str, tools: &[String]) -> String {
    let mut lines = vec![format!("MCP {name} connected: {} tool(s)", tools.len())];
    if !tools.is_empty() {
        let shown = tools.iter().take(8).cloned().collect::<Vec<_>>();
        let more = tools.len().saturating_sub(shown.len());
        let suffix = if more > 0 {
            format!(", and {more} more")
        } else {
            String::new()
        };
        lines.push(format!("tools: {}{suffix}", shown.join(", ")));
    }
    lines.push("Adding this server resets the prompt prefix.".to_owned());
    lines.push("Save? [y/n]".to_owned());
    lines.join("\n")
}

fn mcp_activation_text(server: &McpServerConfig) -> String {
    let mut lines = vec![format!(
        "MCP {} [{}]",
        server.name,
        match server.scope {
            Some(McpScope::User) => "user",
            Some(McpScope::Project) => "project",
            None => "unknown",
        }
    )];
    if let Some(source) = &server.source {
        lines.push(format!("source  {}", source.label()));
    }
    match &server.transport {
        McpTransport::Stdio { command, args, .. } => {
            let mut command = command.clone();
            for argument in args {
                command.push(' ');
                command.push_str(argument);
            }
            lines.push(format!("command {command}"));
        }
        McpTransport::Http { url, .. } => lines.push(format!("url     {url}")),
    }
    lines.push("Enable this server for this workspace? [y/n]".to_owned());
    lines.push("Trust is remembered for this exact configuration.".to_owned());
    lines.join("\n")
}

fn approval_text(message: &ApprovalMessage, details: bool) -> String {
    let mut lines = vec![format!("approval: {}", message.tool_name)];
    if details {
        lines.push(format!("id: {}", message.id));
    }
    if let Some(reason) = &message.reason {
        lines.push(format!("reason: {reason}"));
    }
    let preview = escape_terminal(&message.preview);
    let preview_lines = preview.lines().collect::<Vec<_>>();
    let limit = if details { preview_lines.len() } else { 12 };
    if !preview.is_empty() {
        lines.push(String::new());
        for line in preview_lines.iter().take(limit) {
            lines.push((*line).to_owned());
        }
        if !details && preview_lines.len() > limit {
            lines.push("... (type d for full details)".to_owned());
        }
    }
    lines.push(String::new());
    lines.push("reply y to allow, n to reject, d for details".to_owned());
    lines.join("\n")
}

fn recovered_approval_details(record: &ApprovalRecord) -> String {
    format!(
        "approval: {}\nid: {}\nrisk: {}\nside effect: {}\n--- preview ---\n{}",
        record.tool_name,
        record.approval_id,
        record.risk,
        record.side_effect,
        escape_terminal(&record.preview)
    )
}

struct ComposerView {
    lines: Vec<Line<'static>>,
    horizontal_scroll: u16,
    cursor_x: u16,
    cursor_y: u16,
}

fn composer_height(composer: &str) -> u16 {
    (composer.split('\n').count() as u16).clamp(1, 6)
}

/// Replace every character so a pasted key never reaches the screen.
fn masked_input(composer: &str) -> String {
    "*".repeat(composer.chars().count())
}

fn credential_report(status: &CredentialStatus) -> String {
    match status.source {
        CredentialSourceKind::WorkspaceEnv => format!(
            "credential: DEEPSEEK_API_KEY from {}",
            status.env_path.display()
        ),
        CredentialSourceKind::Environment => {
            "credential: DEEPSEEK_API_KEY from the environment; a key in the workspace .env takes precedence".to_owned()
        }
        CredentialSourceKind::Missing => format!(
            "credential: not configured; /login writes DEEPSEEK_API_KEY to {}",
            status.env_path.display()
        ),
    }
}

/// Slash commands that cannot run without a provider credential.
fn command_needs_credential(command: &Command) -> bool {
    matches!(
        command,
        Command::Plan(_)
            | Command::Answer(_)
            | Command::Continue
            | Command::Test
            | Command::Fix
            | Command::Execute
            | Command::Distill(_)
    )
}

fn composer_view(
    composer: &str,
    cursor: usize,
    completion: Option<&CommandCompletion>,
    area: Rect,
) -> ComposerView {
    let raw_lines = composer.split('\n').collect::<Vec<_>>();
    let total = raw_lines.len();
    let height = composer_height(composer) as usize;
    let cursor = cursor.min(composer.len());
    let cursor_line = composer[..cursor].matches('\n').count();
    let line_start = composer[..cursor].rfind('\n').map_or(0, |index| index + 1);
    let cursor_column = 2 + Line::from(&composer[line_start..cursor]).width();
    let offset = cursor_line
        .saturating_sub(height.saturating_sub(1))
        .min(total.saturating_sub(height));
    let mut lines = Vec::new();
    for (index, line) in raw_lines.iter().enumerate().skip(offset).take(height) {
        let prefix = if index == 0 { "> " } else { "  " };
        let mut spans = vec![Span::raw(format!("{prefix}{line}"))];
        if index == total - 1
            && let Some(completion) = completion
        {
            spans.push(Span::styled(
                completion.suffix.clone(),
                Style::default().fg(Color::DarkGray),
            ));
        }
        lines.push(Line::from(spans));
    }

    let max_width = area.width.max(1) as usize;
    let horizontal_scroll = cursor_column
        .saturating_sub(max_width.saturating_sub(1))
        .min(u16::MAX as usize) as u16;
    let cursor_x = area.x.saturating_add(
        cursor_column
            .saturating_sub(horizontal_scroll as usize)
            .min(max_width.saturating_sub(1)) as u16,
    );
    let cursor_row = cursor_line.saturating_sub(offset).min(height - 1);
    let cursor_y = area
        .y
        .saturating_add(cursor_row as u16)
        .min(area.y.saturating_add(area.height.saturating_sub(1)));

    ComposerView {
        lines,
        horizontal_scroll,
        cursor_x,
        cursor_y,
    }
}

fn short_session(id: &str) -> String {
    if id.len() > 12 {
        format!("{}...", &id[..12])
    } else {
        id.to_owned()
    }
}

fn format_count(value: u64) -> String {
    let digits = value.to_string();
    let mut out = String::new();
    for (index, character) in digits.chars().enumerate() {
        if index > 0 && (digits.len() - index).is_multiple_of(3) {
            out.push(',');
        }
        out.push(character);
    }
    out
}

fn strip_frontmatter(skill: &str) -> &str {
    let Some(rest) = skill.strip_prefix("---\n") else {
        return skill;
    };
    match rest.find("\n---\n") {
        Some(index) => &rest[index + 5..],
        None => skill,
    }
}

fn session_completion(composer: &str, ids: &[&str], cursor: usize) -> Option<CommandCompletion> {
    if cursor != composer.len() {
        return None;
    }
    let prefix = composer.strip_prefix("/session switch ")?;
    let id = ids
        .iter()
        .find(|id| id.starts_with(prefix) && **id != prefix)?;
    let suffix = id[prefix.len()..].to_owned();
    Some(CommandCompletion {
        full: format!("{composer}{suffix}"),
        suffix,
        description: None,
    })
}

fn handoff_prompt(workspace: &std::path::Path, skill: &str) -> String {
    let body = strip_frontmatter(skill);
    format!(
        "Use the handoff skill below to update the project handoff document.\n\
         Write the result to {}/HANDOFF.md, replacing the existing file. The skill normally writes to a temporary directory; this project keeps HANDOFF.md in the workspace instead.\n\
         Do not edit any other file. If there is nothing new to record, reply exactly NO_CHANGES and leave the file unchanged.\n\n\
         ---\n{body}",
        workspace.display()
    )
}

fn format_percent(value: u64, window: u64) -> String {
    if window == 0 {
        return "-".to_owned();
    }
    format!("{:.2}%", value as f64 / window as f64 * 100.0)
}

fn previous_boundary(text: &str, index: usize) -> Option<usize> {
    if index == 0 {
        return None;
    }
    let mut previous = index - 1;
    while previous > 0 && !text.is_char_boundary(previous) {
        previous -= 1;
    }
    Some(previous)
}

fn next_boundary(text: &str, index: usize) -> Option<usize> {
    if index >= text.len() {
        return None;
    }
    let mut next = index + 1;
    while next < text.len() && !text.is_char_boundary(next) {
        next += 1;
    }
    Some(next)
}

fn char_offset(text: &str, column: usize) -> usize {
    text.char_indices()
        .nth(column)
        .map_or(text.len(), |(index, _)| index)
}

fn snapshot_area(buffer: &Buffer, area: Rect) -> Vec<String> {
    (0..area.height)
        .map(|row| {
            (0..area.width)
                .map(|column| {
                    buffer
                        .cell((area.x + column, area.y + row))
                        .map_or(" ", |cell| cell.symbol())
                })
                .collect()
        })
        .collect()
}

fn ordered_selection(selection: &Selection) -> ((u16, u16), (u16, u16)) {
    if selection.anchor <= selection.cursor {
        (selection.anchor, selection.cursor)
    } else {
        (selection.cursor, selection.anchor)
    }
}

fn selected_text(screen: &[String], selection: &Selection) -> String {
    let ((start_row, start_column), (end_row, end_column)) = ordered_selection(selection);
    let mut out = String::new();
    for row in start_row..=end_row {
        let line = screen.get(row as usize).map(String::as_str).unwrap_or("");
        let characters = line.chars().collect::<Vec<_>>();
        let from = if row == start_row {
            start_column as usize
        } else {
            0
        }
        .min(characters.len());
        let to = if row == end_row {
            (end_column as usize).saturating_add(1)
        } else {
            characters.len()
        }
        .min(characters.len());
        let segment = characters[from..to].iter().collect::<String>();
        out.push_str(segment.trim_end());
        if row != end_row {
            out.push('\n');
        }
    }
    out.trim_end_matches('\n').to_owned()
}

fn highlight_selection(buffer: &mut Buffer, area: Rect, selection: &Selection) {
    let ((start_row, start_column), (end_row, end_column)) = ordered_selection(selection);
    for row in start_row..=end_row {
        let from = if row == start_row {
            start_column as usize
        } else {
            0
        };
        let to = if row == end_row {
            (end_column as usize).saturating_add(1)
        } else {
            area.width as usize
        }
        .min(area.width as usize);
        for column in from..to {
            if let Some(cell) = buffer.cell_mut((area.x + column as u16, area.y + row)) {
                cell.set_bg(Color::DarkGray);
                cell.set_fg(Color::Black);
            }
        }
    }
}

fn scroll_up_from(current: u16, follow_tail: bool, max: u16, amount: u16) -> (u16, bool) {
    let base = if follow_tail { max } else { current.min(max) };
    let next = base.saturating_sub(amount);
    (next, next >= max)
}

fn scroll_down_from(current: u16, follow_tail: bool, max: u16, amount: u16) -> (u16, bool) {
    let base = if follow_tail { max } else { current.min(max) };
    let next = base.saturating_add(amount).min(max);
    (next, next >= max)
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum ApprovalReply {
    Allow,
    Reject,
    Details,
    Invalid,
}

fn parse_approval_reply(input: &str) -> ApprovalReply {
    match input.trim().to_ascii_lowercase().as_str() {
        "y" | "a" | "s" => ApprovalReply::Allow,
        "n" => ApprovalReply::Reject,
        "d" => ApprovalReply::Details,
        _ => ApprovalReply::Invalid,
    }
}

fn is_newline_key(key: &KeyEvent) -> bool {
    (key.code == KeyCode::Enter && key.modifiers.contains(KeyModifiers::SHIFT))
        || (key.code == KeyCode::Char('j') && key.modifiers.contains(KeyModifiers::CONTROL))
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum YesNoReply {
    Yes,
    No,
    Invalid,
}

fn parse_yes_no(input: &str) -> YesNoReply {
    match input.trim().to_ascii_lowercase().as_str() {
        "y" => YesNoReply::Yes,
        "n" => YesNoReply::No,
        _ => YesNoReply::Invalid,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use ratatui::{Terminal, backend::TestBackend, widgets::Paragraph};
    use slowcode_app::{RuntimeConfig, SessionChoice};
    use slowcode_kernel::ui::{
        UiAssistantMessage, UiCompactionError, UiEvent, UiMessageSource, UiTodo, UiToolFailure,
        UiTurnEnd, UiUsage,
    };
    use std::{
        path::PathBuf,
        time::{SystemTime, UNIX_EPOCH},
    };
    use tokio::sync::oneshot;

    static SESSION_ROOT_LOCK: Mutex<()> = Mutex::const_new(());

    fn unique_temp_dir(tag: &str) -> PathBuf {
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("clock")
            .as_nanos();
        let path =
            std::env::temp_dir().join(format!("slowcode-tui-{tag}-{}-{nanos}", std::process::id()));
        std::fs::create_dir_all(&path).expect("temp dir");
        path
    }

    async fn test_app() -> App {
        let guard = SESSION_ROOT_LOCK.lock().await;
        let root = unique_temp_dir("app");
        let workspace = root.join("workspace");
        std::fs::create_dir_all(&workspace).expect("workspace");
        // SAFETY: every test that starts a runtime holds SESSION_ROOT_LOCK, so
        // no other test observes or changes the session root concurrently.
        unsafe { std::env::set_var("DSH_SESSION_ROOT", root.join("sessions")) };
        let started = Runtime::start(RuntimeConfig::new(workspace), SessionChoice::New)
            .await
            .expect("runtime");
        // SAFETY: see the note above; the session root is read while starting.
        unsafe { std::env::remove_var("DSH_SESSION_ROOT") };
        drop(guard);

        let model = started.runtime.model().to_owned();
        let role = started
            .runtime
            .role()
            .map(|role| role.as_str().to_owned())
            .unwrap_or_else(|| "driver".to_owned());
        let (app_tx, _app_rx) = mpsc::unbounded_channel();
        App::new(
            Arc::new(Mutex::new(started.runtime)),
            started.session_id,
            model,
            role,
            started.history,
            app_tx,
        )
    }

    fn ui_event(kind: UiEventKind) -> UiEvent {
        UiEvent {
            seq: 0,
            time_ms: 0,
            kind,
        }
    }

    fn usage(input: u64, output: u64, read: Option<u64>, write: Option<u64>) -> UiUsage {
        UiUsage {
            input_tokens: input,
            output_tokens: output,
            cache_read_tokens: read,
            cache_write_tokens: write,
            reasoning_tokens: None,
        }
    }

    fn line_text(line: &Line<'_>) -> String {
        line.spans
            .iter()
            .map(|span| span.content.as_ref())
            .collect()
    }

    #[test]
    fn live_deltas_share_one_buffer_per_kind() {
        let mut messages = Vec::new();
        let mut assistant = None;
        let mut reasoning = None;

        append_live(&mut messages, &mut assistant, LiveKind::Assistant, "Hel");
        append_live(&mut messages, &mut assistant, LiveKind::Assistant, "lo");
        append_live(&mut messages, &mut reasoning, LiveKind::Reasoning, "think");
        append_live(&mut messages, &mut reasoning, LiveKind::Reasoning, "ing");

        assert_eq!(
            messages,
            vec![
                MessageBlock::Assistant("Hello".to_owned()),
                MessageBlock::Reasoning("thinking".to_owned()),
            ]
        );
        assert_eq!(assistant, Some(0));
        assert_eq!(reasoning, Some(1));
    }

    #[test]
    fn empty_live_deltas_create_nothing() {
        let mut messages = Vec::new();
        let mut assistant = None;

        append_live(&mut messages, &mut assistant, LiveKind::Assistant, "");

        assert!(messages.is_empty());
        assert_eq!(assistant, None);
    }

    #[test]
    fn final_message_replaces_live_text_in_place_and_clears_the_slot() {
        let mut messages = Vec::new();
        let mut assistant = None;
        append_live(&mut messages, &mut assistant, LiveKind::Assistant, "par");
        messages.push(MessageBlock::System("between".to_owned()));

        replace_live(
            &mut messages,
            &mut assistant,
            LiveKind::Assistant,
            "final text",
        );

        assert_eq!(
            messages,
            vec![
                MessageBlock::Assistant("final text".to_owned()),
                MessageBlock::System("between".to_owned()),
            ]
        );
        assert_eq!(assistant, None);
    }

    #[test]
    fn final_message_without_a_live_buffer_appends_only_non_empty_text() {
        let mut messages = Vec::new();
        let mut assistant = None;

        replace_live(&mut messages, &mut assistant, LiveKind::Assistant, "");

        assert!(messages.is_empty());
        assert_eq!(assistant, None);

        replace_live(
            &mut messages,
            &mut assistant,
            LiveKind::Assistant,
            "full text",
        );

        assert_eq!(
            messages,
            vec![MessageBlock::Assistant("full text".to_owned())]
        );
        assert_eq!(assistant, None);
    }

    #[test]
    fn final_message_clears_a_live_buffer_with_empty_text() {
        let mut messages = Vec::new();
        let mut reasoning = None;
        append_live(&mut messages, &mut reasoning, LiveKind::Reasoning, "draft");

        replace_live(&mut messages, &mut reasoning, LiveKind::Reasoning, "");

        assert_eq!(messages, vec![MessageBlock::Reasoning(String::new())]);
        assert_eq!(reasoning, None);
    }

    #[test]
    fn user_messages_are_left_aligned_with_a_prompt_prefix() {
        let lines = message_lines(&[
            MessageBlock::User("hello".to_owned()),
            MessageBlock::Assistant("world".to_owned()),
        ]);

        assert_eq!(line_text(&lines[0]), "> hello");
        assert_eq!(lines[0].alignment, Some(Alignment::Left));
        let assistant = lines
            .iter()
            .find(|line| line_text(line) == "world")
            .expect("assistant line");
        assert_eq!(assistant.alignment, Some(Alignment::Left));
        assert!(!lines.iter().any(|line| {
            let text = line_text(line);
            text == "you" || text == "slowcode"
        }));
    }

    #[test]
    fn multiline_user_messages_keep_the_prefix_alignment() {
        let lines = message_lines(&[MessageBlock::User("first\nsecond".to_owned())]);
        assert_eq!(line_text(&lines[0]), "> first");
        assert_eq!(line_text(&lines[1]), "  second");
        assert_eq!(lines[0].alignment, Some(Alignment::Left));
        assert_eq!(lines[1].alignment, Some(Alignment::Left));
    }

    #[test]
    fn context_counts_and_percentages_render_stably() {
        assert_eq!(format_count(0), "0");
        assert_eq!(format_count(999), "999");
        assert_eq!(format_count(1_000), "1,000");
        assert_eq!(format_count(1_234_567), "1,234,567");
        assert_eq!(format_percent(0, 1_000_000), "0.00%");
        assert_eq!(format_percent(500_000, 1_000_000), "50.00%");
        assert_eq!(format_percent(1, 0), "-");
    }

    #[test]
    fn composer_cursor_tracks_the_last_line() {
        let area = Rect {
            x: 3,
            y: 10,
            width: 20,
            height: 1,
        };
        let view = composer_view("hello", 5, None, area);
        assert_eq!(view.cursor_x, 10);
        assert_eq!(view.cursor_y, 10);
        assert_eq!(view.horizontal_scroll, 0);

        let area = Rect {
            x: 3,
            y: 10,
            width: 20,
            height: 2,
        };
        let view = composer_view("first\nsecond", 12, None, area);
        assert_eq!(view.cursor_x, 11);
        assert_eq!(view.cursor_y, 11);
    }

    #[test]
    fn composer_scrolls_long_lines_to_keep_the_cursor_visible() {
        let area = Rect {
            x: 0,
            y: 0,
            width: 10,
            height: 1,
        };
        let view = composer_view("012345678901234567890123456789", 30, None, area);
        assert_eq!(view.cursor_x, 9);
        assert!(view.horizontal_scroll > 0);
    }

    #[test]
    fn composer_cursor_can_move_inside_the_input() {
        let area = Rect {
            x: 0,
            y: 0,
            width: 20,
            height: 1,
        };
        let view = composer_view("hello", 2, None, area);
        assert_eq!(view.cursor_x, 4);

        assert_eq!(previous_boundary("hello", 2), Some(1));
        assert_eq!(next_boundary("hello", 2), Some(3));
        assert_eq!(previous_boundary("a中b", 4), Some(1));
        assert_eq!(next_boundary("a中b", 1), Some(4));
        assert_eq!(char_offset("a中b", 2), 4);
    }

    #[test]
    fn composer_renders_the_inline_completion_suffix() {
        let area = Rect {
            x: 0,
            y: 0,
            width: 40,
            height: 1,
        };
        let completion = CommandCompletion {
            suffix: "le".to_owned(),
            full: "/role".to_owned(),
            description: None,
        };
        let view = composer_view("/ro", 3, Some(&completion), area);
        assert_eq!(line_text(&view.lines[0]), "> /role");
        assert_eq!(view.cursor_x, 5);
    }

    #[test]
    fn selection_text_is_extracted_between_two_points() {
        let screen = vec!["abc def  ".to_owned(), "ghi jkl".to_owned()];
        let selection = Selection {
            anchor: (0, 4),
            cursor: (1, 2),
        };
        assert_eq!(selected_text(&screen, &selection), "def\nghi");

        let selection = Selection {
            anchor: (1, 7),
            cursor: (0, 0),
        };
        assert_eq!(selected_text(&screen, &selection), "abc def\nghi jkl");
    }

    #[test]
    fn handoff_prompt_overrides_the_temp_directory_rule() {
        let skill = "---\nname: handoff\ndescription: test\n---\nWrite a handoff document.\n";
        assert_eq!(strip_frontmatter(skill), "Write a handoff document.\n");
        let prompt = handoff_prompt(std::path::Path::new("/work"), skill);
        assert!(prompt.contains("Write the result to /work/HANDOFF.md"));
        assert!(prompt.contains("NO_CHANGES"));
        assert!(!prompt.contains("name: handoff"));
        assert!(prompt.contains("Write a handoff document."));
    }

    #[test]
    fn session_completion_matches_ids_after_the_switch_verb() {
        let ids = ["session-aaa", "session-bbb"];
        let completion = session_completion("/session switch session-a", &ids, 25)
            .expect("session id completion");
        assert_eq!(completion.suffix, "aa");
        assert_eq!(completion.full, "/session switch session-aaa");
        assert_eq!(
            session_completion("/session switch ", &ids, 16)
                .expect("first session")
                .suffix,
            "session-aaa"
        );
        assert!(session_completion("/session switch session-aaa", &ids, 28).is_none());
        assert!(session_completion("/role driver", &ids, 12).is_none());
    }

    #[test]
    fn help_text_is_inline_and_mentions_usage() {
        let all = help_text(None);
        assert!(all.contains("commands"));
        assert!(all.contains("/model"));
        assert!(all.contains("PgUp/PgDn"));

        let model = help_text(Some("model"));
        assert!(model.contains("usage: /model [name]"));
        assert!(model.contains("/model deepseek-v4-pro"));
    }

    #[test]
    fn model_report_marks_the_current_model() {
        let report = model_report("deepseek-v4-pro");
        assert!(report.contains("* deepseek-v4-pro"));
        assert!(report.contains("usage: /model <name>"));
    }

    #[test]
    fn mcp_preview_shows_source_and_command() {
        let server = McpServerConfig {
            name: "chrome-devtools".to_owned(),
            transport: McpTransport::Stdio {
                command: "npx".to_owned(),
                args: vec!["-y".to_owned(), "chrome-devtools-mcp@1.11.0".to_owned()],
                env: Default::default(),
            },
            trusted: false,
            enabled: true,
            scope: Some(McpScope::Project),
            source: Some(slowcode_mcp::McpSource {
                kind: "github".to_owned(),
                repo: "ChromeDevTools/chrome-devtools-mcp".to_owned(),
                package: "chrome-devtools-mcp".to_owned(),
                version: "1.11.0".to_owned(),
                integrity: None,
                bin: "chrome-devtools-mcp".to_owned(),
            }),
        };
        let preview = mcp_preview_text(&server);
        assert!(preview.contains("MCP chrome-devtools"));
        assert!(preview.contains("ChromeDevTools/chrome-devtools-mcp@1.11.0"));
        assert!(preview.contains("npx -y chrome-devtools-mcp@1.11.0"));
        assert!(preview.contains("Run connection test? [y/n]"));
    }

    #[test]
    fn approval_text_truncates_until_details_are_requested() {
        let (respond, _reply) = oneshot::channel();
        let message = ApprovalMessage {
            id: "approval-1".to_owned(),
            tool_name: "write".to_owned(),
            reason: Some("create notes.txt".to_owned()),
            preview: (1..=20)
                .map(|line| format!("line {line}"))
                .collect::<Vec<_>>()
                .join("\n"),
            respond,
        };

        let compact = approval_text(&message, false);
        assert!(compact.contains("approval: write"));
        assert!(compact.contains("line 12"));
        assert!(!compact.contains("line 13"));
        assert!(compact.contains("type d for full details"));

        let full = approval_text(&message, true);
        assert!(full.contains("id: approval-1"));
        assert!(full.contains("line 20"));
        assert!(!full.contains("type d for full details"));
    }

    #[test]
    fn scrolling_keeps_tail_following_when_content_does_not_overflow() {
        assert_eq!(scroll_up_from(u16::MAX, true, 0, 5), (0, true));
        assert_eq!(scroll_up_from(u16::MAX, true, 12, 5), (7, false));
        assert_eq!(scroll_up_from(7, false, 12, 5), (2, false));
        assert_eq!(scroll_down_from(2, false, 12, 5), (7, false));
        assert_eq!(scroll_down_from(7, false, 12, 5), (12, true));
    }

    #[test]
    fn yes_no_reply_is_trimmed_and_case_insensitive() {
        assert_eq!(parse_yes_no(" Y "), YesNoReply::Yes);
        assert_eq!(parse_yes_no("n"), YesNoReply::No);
        assert_eq!(parse_yes_no("yes"), YesNoReply::Invalid);
        assert_eq!(parse_yes_no(""), YesNoReply::Invalid);
    }

    #[test]
    fn prompt_replies_are_short_and_case_insensitive() {
        assert_eq!(parse_approval_reply("Y"), ApprovalReply::Allow);
        assert_eq!(parse_approval_reply(" a "), ApprovalReply::Allow);
        assert_eq!(parse_approval_reply("s"), ApprovalReply::Allow);
        assert_eq!(parse_approval_reply("N"), ApprovalReply::Reject);
        assert_eq!(parse_approval_reply("d"), ApprovalReply::Details);
        assert_eq!(parse_approval_reply("yes"), ApprovalReply::Invalid);
    }

    #[test]
    fn rendered_user_message_starts_at_the_left_edge_without_borders() {
        let backend = TestBackend::new(20, 4);
        let mut terminal = Terminal::new(backend).expect("terminal");
        terminal
            .draw(|frame| {
                let area = frame.area();
                frame.render_widget(
                    Paragraph::new(message_lines(&[MessageBlock::User("hi".to_owned())]))
                        .wrap(Wrap { trim: false }),
                    area,
                );
            })
            .expect("draw");

        let buffer = terminal.backend().buffer();
        assert_eq!(buffer.cell((0, 0)).expect("cell").symbol(), ">");
        assert_eq!(buffer.cell((2, 0)).expect("cell").symbol(), "h");
        assert_eq!(buffer.cell((3, 0)).expect("cell").symbol(), "i");
        assert!(buffer.content().iter().all(|cell| {
            !matches!(cell.symbol(), "│" | "┌" | "┐" | "└" | "┘" | "─")
        }));
    }

    #[test]
    fn rendered_composer_sets_a_visible_cursor() {
        let backend = TestBackend::new(20, 6);
        let mut terminal = Terminal::new(backend).expect("terminal");
        terminal
            .draw(|frame| {
                let area = Rect::new(0, 5, 20, 1);
                let view = composer_view("hello", 5, None, area);
                frame.render_widget(
                    Paragraph::new(view.lines).scroll((0, view.horizontal_scroll)),
                    area,
                );
                frame.set_cursor_position(Position::new(view.cursor_x, view.cursor_y));
            })
            .expect("draw");

        assert_eq!(
            terminal.get_cursor_position().expect("cursor"),
            Position::new(7, 5)
        );
        assert_eq!(
            terminal
                .backend()
                .buffer()
                .cell((0, 5))
                .expect("cell")
                .symbol(),
            ">"
        );
    }

    #[test]
    fn rendered_help_contains_commands_and_keys() {
        let backend = TestBackend::new(100, 30);
        let mut terminal = Terminal::new(backend).expect("terminal");
        terminal
            .draw(|frame| {
                frame.render_widget(
                    Paragraph::new(help_text(None)).wrap(Wrap { trim: false }),
                    frame.area(),
                );
            })
            .expect("draw");

        let text = terminal
            .backend()
            .buffer()
            .content()
            .iter()
            .map(|cell| cell.symbol())
            .collect::<String>();
        assert!(text.contains("commands"));
        assert!(text.contains("/model"));
        assert!(text.contains("keys"));
    }

    #[tokio::test]
    async fn tool_events_track_state_and_update_the_matching_tool_block() {
        let mut app = test_app().await;

        app.handle_ui_event(ui_event(UiEventKind::TurnStart { turn: 1 }));
        assert!(app.running);
        assert_eq!(app.state, RunState::Streaming);

        app.handle_ui_event(ui_event(UiEventKind::ToolRequested {
            turn: 1,
            step: 1,
            call_id: "call-1".to_owned(),
            name: "write".to_owned(),
            arguments: "{\"path\":\"notes.txt\"}".to_owned(),
            omitted: false,
        }));
        assert_eq!(app.state, RunState::Tool);

        app.handle_ui_event(ui_event(UiEventKind::ToolResult {
            turn: 1,
            step: 1,
            call_id: "call-1".to_owned(),
            is_error: true,
            failure: Some(UiToolFailure {
                name: "write".to_owned(),
                code: "DENIED".to_owned(),
            }),
            content: "not allowed".to_owned(),
            meta: String::new(),
            omitted: false,
        }));
        assert_eq!(app.state, RunState::Streaming);
        assert!(matches!(
            app.messages.last(),
            Some(MessageBlock::Tool {
                call_id,
                status,
                result,
                is_error: true,
                ..
            }) if call_id == "call-1"
                && status == "error"
                && result.as_deref() == Some("DENIED: not allowed")
        ));

        let before = app.messages.clone();
        app.handle_ui_event(ui_event(UiEventKind::ToolResult {
            turn: 1,
            step: 1,
            call_id: "call-missing".to_owned(),
            is_error: false,
            failure: None,
            content: "ignored".to_owned(),
            meta: String::new(),
            omitted: false,
        }));
        assert_eq!(app.messages, before);
    }

    #[tokio::test]
    async fn only_human_user_messages_are_rendered() {
        let mut app = test_app().await;
        let before = app.messages.len();

        app.handle_ui_event(ui_event(UiEventKind::UserMessage {
            source: UiMessageSource::Context {
                plugin: "context".to_owned(),
                form: None,
            },
            text: "hidden".to_owned(),
            omitted: false,
        }));
        assert_eq!(app.messages.len(), before);

        app.handle_ui_event(ui_event(UiEventKind::UserMessage {
            source: UiMessageSource::Human,
            text: "hello".to_owned(),
            omitted: false,
        }));
        assert_eq!(
            app.messages.last(),
            Some(&MessageBlock::User("hello".to_owned()))
        );
    }

    #[tokio::test]
    async fn usage_samples_accumulate_and_track_the_last_prompt() {
        let mut app = test_app().await;

        app.handle_ui_event(ui_event(UiEventKind::UsageSample {
            turn: 1,
            step: 1,
            usage: usage(10, 4, Some(3), Some(2)),
        }));
        app.handle_ui_event(ui_event(UiEventKind::UsageSample {
            turn: 1,
            step: 2,
            usage: usage(5, 1, None, None),
        }));

        assert_eq!(app.usage.input_tokens, 15);
        assert_eq!(app.usage.output_tokens, 5);
        assert_eq!(app.usage.cache_read_tokens, 3);
        assert_eq!(app.usage.cache_write_tokens, 2);
        assert_eq!(app.last_prompt_tokens, 5);
    }

    #[tokio::test]
    async fn request_context_changes_update_model_and_window() {
        let mut app = test_app().await;

        app.handle_ui_event(ui_event(UiEventKind::RequestContextChanged {
            provider: Some("deepseek".to_owned()),
            model: Some("deepseek-v4-pro".to_owned()),
            context_window: Some(128_000),
        }));

        assert_eq!(app.model, "deepseek-v4-pro");
        assert_eq!(app.context_window, Some(128_000));
    }

    #[tokio::test]
    async fn turn_end_clears_running_approval_and_live_buffers() {
        let mut app = test_app().await;
        let (respond, _reply) = oneshot::channel();
        app.pending_approval = Some(PendingApproval {
            message: ApprovalMessage {
                id: "approval-1".to_owned(),
                tool_name: "write".to_owned(),
                reason: None,
                preview: String::new(),
                respond,
            },
            details: false,
        });
        app.assistant_live = Some(0);
        app.reasoning_live = Some(0);
        app.running = true;

        app.handle_ui_event(ui_event(UiEventKind::TurnEnd {
            turn: 1,
            reason: UiTurnEnd::Error {
                code: "provider".to_owned(),
                message: "boom".to_owned(),
            },
        }));

        assert!(!app.running);
        assert_eq!(app.state, RunState::Error);
        assert!(app.pending_approval.is_none());
        assert!(app.assistant_live.is_none());
        assert!(app.reasoning_live.is_none());
        assert!(matches!(
            app.messages.last(),
            Some(MessageBlock::Error(text)) if text == "provider: boom"
        ));
    }

    #[tokio::test]
    async fn compaction_retry_approval_and_todo_events_append_expected_notes() {
        let mut app = test_app().await;
        let before = app.messages.len();

        app.handle_ui_event(ui_event(UiEventKind::RetryScheduled {
            retry_id: "r1".to_owned(),
            retry: 2,
            provider: "deepseek".to_owned(),
            delay_ms: 500,
            max_retries: Some(3),
            failure_code: "rate_limit".to_owned(),
            failure_message: "slow down".to_owned(),
        }));
        assert_eq!(
            app.messages.last(),
            Some(&MessageBlock::System(
                "retry 2 in 500 ms after rate_limit".to_owned()
            ))
        );

        app.handle_ui_event(ui_event(UiEventKind::ApprovalAsked {
            id: "a1".to_owned(),
            tool_name: "write".to_owned(),
            call_id: None,
            reason: None,
        }));
        assert_eq!(app.state, RunState::Approval);

        app.handle_ui_event(ui_event(UiEventKind::ApprovalDecided {
            id: "a1".to_owned(),
            outcome: "allowed".to_owned(),
        }));
        assert_eq!(app.state, RunState::Streaming);
        assert_eq!(
            app.messages.last(),
            Some(&MessageBlock::System("approval: allowed".to_owned()))
        );

        app.handle_ui_event(ui_event(UiEventKind::CompactionStarted {
            id: "c1".to_owned(),
            trigger: None,
            shadowed_nodes: None,
        }));
        app.handle_ui_event(ui_event(UiEventKind::CompactionSummarized {
            id: "c1".to_owned(),
            shadowed_tokens: 1024,
            provider: "deepseek".to_owned(),
            model: "deepseek-flash".to_owned(),
            usage: None,
        }));
        app.handle_ui_event(ui_event(UiEventKind::CompactionEnded {
            id: "c1".to_owned(),
            error: Some(UiCompactionError {
                code: Some("E".to_owned()),
                message: "failed".to_owned(),
            }),
        }));
        assert_eq!(
            app.messages.last(),
            Some(&MessageBlock::Error("compaction: failed".to_owned()))
        );

        app.handle_ui_event(ui_event(UiEventKind::TodoWrite { todos: vec![] }));
        let after_empty = app.messages.len();
        app.handle_ui_event(ui_event(UiEventKind::TodoWrite {
            todos: vec![
                UiTodo {
                    content: "ship".to_owned(),
                    status: "pending".to_owned(),
                },
                UiTodo {
                    content: "test".to_owned(),
                    status: "completed".to_owned(),
                },
            ],
        }));
        assert_eq!(
            app.messages.last(),
            Some(&MessageBlock::System(
                "[pending] ship\n[completed] test".to_owned()
            ))
        );
        assert_eq!(after_empty, before + 5);
    }

    #[tokio::test]
    async fn assistant_message_replaces_live_deltas_and_ignores_empty_deltas() {
        let mut app = test_app().await;
        let before = app.messages.len();

        app.handle_ui_event(ui_event(UiEventKind::AssistantTextDelta {
            turn: 1,
            step: 1,
            index: 0,
            text: String::new(),
        }));
        assert_eq!(app.messages.len(), before);

        app.handle_ui_event(ui_event(UiEventKind::AssistantTextDelta {
            turn: 1,
            step: 1,
            index: 0,
            text: "par".to_owned(),
        }));
        app.handle_ui_event(ui_event(UiEventKind::AssistantReasoningDelta {
            turn: 1,
            step: 1,
            index: 0,
            text: "think".to_owned(),
        }));
        app.handle_ui_event(ui_event(UiEventKind::AssistantMessage {
            turn: 1,
            step: 1,
            message: UiAssistantMessage {
                blocks: Vec::new(),
                text: "final".to_owned(),
                reasoning: "reasoning".to_owned(),
                provider: "deepseek".to_owned(),
                model: "deepseek-flash".to_owned(),
                usage: None,
            },
        }));

        assert!(
            app.messages
                .contains(&MessageBlock::Assistant("final".to_owned()))
        );
        assert!(
            app.messages
                .contains(&MessageBlock::Reasoning("reasoning".to_owned()))
        );
        assert!(
            !app.messages
                .contains(&MessageBlock::Assistant("par".to_owned()))
        );
    }

    #[tokio::test]
    async fn mcp_list_reports_no_servers_and_unknown_subcommands_error() {
        let mut app = test_app().await;

        app.handle_mcp(None).await;
        assert!(matches!(
            app.messages.last(),
            Some(MessageBlock::System(text)) if text.contains("no MCP servers configured")
        ));

        app.handle_mcp(Some("bogus")).await;
        assert!(matches!(
            app.messages.last(),
            Some(MessageBlock::Error(text)) if text.contains("unknown /mcp subcommand: bogus")
        ));
    }

    #[tokio::test]
    async fn mcp_add_stdio_previews_the_server_and_waits_for_a_reply() {
        let mut app = test_app().await;

        app.handle_mcp(Some("add demo npx -y demo-server")).await;

        assert!(matches!(
            app.messages.last(),
            Some(MessageBlock::Prompt(text))
                if text.contains("MCP demo")
                    && text.contains("command npx -y demo-server")
                    && text.contains("Run connection test? [y/n]")
        ));
        assert!(matches!(
            app.pending_mcp.as_ref(),
            Some(PendingMcpInstall::Test { server }) if server.name == "demo"
        ));
    }

    #[tokio::test]
    async fn mcp_add_without_arguments_reports_usage() {
        let mut app = test_app().await;

        app.handle_mcp(Some("add")).await;

        assert!(matches!(
            app.messages.last(),
            Some(MessageBlock::Error(text)) if text.contains("usage: /mcp add")
        ));
        assert!(app.pending_mcp.is_none());
    }

    #[tokio::test]
    async fn mcp_subcommands_require_and_validate_names() {
        let mut app = test_app().await;

        for (subcommand, usage) in [
            ("test", "usage: /mcp test <name>"),
            ("enable", "usage: /mcp enable <name>"),
            ("disable", "usage: /mcp disable <name>"),
            ("remove", "usage: /mcp remove <name>"),
        ] {
            app.handle_mcp(Some(subcommand)).await;
            assert!(matches!(
                app.messages.last(),
                Some(MessageBlock::Error(text)) if text == usage
            ));
        }

        app.handle_mcp(Some("test missing")).await;
        assert!(matches!(
            app.messages.last(),
            Some(MessageBlock::Error(text)) if text == "unknown MCP server: missing"
        ));
    }

    #[tokio::test]
    async fn skills_list_install_and_remove_report_their_outcomes() {
        let mut app = test_app().await;

        app.handle_skills(None).await;
        assert!(matches!(
            app.messages.last(),
            Some(MessageBlock::System(text)) if text.contains("no skills installed")
        ));

        app.handle_skills(Some("install")).await;
        assert!(matches!(
            app.messages.last(),
            Some(MessageBlock::System(text)) if text.contains("no skills installed")
        ));

        app.handle_skills(Some("install /does/not/exist")).await;
        assert!(matches!(
            app.messages.last(),
            Some(MessageBlock::Error(text)) if text.starts_with("skill preview:")
        ));

        app.handle_skills(Some("remove missing")).await;
        assert!(matches!(
            app.messages.last(),
            Some(MessageBlock::System(text)) if text == "skill removed: missing [project]"
        ));

        app.handle_skills(Some("remove ../bad")).await;
        assert!(matches!(
            app.messages.last(),
            Some(MessageBlock::Error(text)) if text.starts_with("skill remove:")
        ));
    }

    #[tokio::test]
    async fn skills_remove_uses_the_requested_scope() {
        let mut app = test_app().await;
        let workspace = app.workspace();
        let source = workspace.join("source/demo");
        std::fs::create_dir_all(&source).expect("source skill");
        std::fs::write(
            source.join("SKILL.md"),
            "---\nname: demo\ndescription: test skill\n---\nbody\n",
        )
        .expect("skill file");
        let installer = SkillInstaller::new(&workspace);
        installer
            .install(&source, &SkillScope::Project, &InstallOptions::default())
            .unwrap();
        installer
            .install(
                &source,
                &SkillScope::Role(slowcode_roles::RoleName::Navigator),
                &InstallOptions::default(),
            )
            .unwrap();

        app.handle_skills(Some("remove demo --role navigator"))
            .await;

        assert!(matches!(
            app.messages.last(),
            Some(MessageBlock::System(text)) if text == "skill removed: demo [role navigator]"
        ));
        assert!(
            !workspace
                .join(".slowcode/roles/navigator/skills/demo")
                .exists()
        );
        assert!(workspace.join(".slowcode/skills/demo").is_dir());

        app.handle_skills(Some("remove demo --global --role driver"))
            .await;
        assert!(matches!(
            app.messages.last(),
            Some(MessageBlock::Error(text)) if text == "use either --global or --role, not both"
        ));
    }

    #[tokio::test]
    async fn skills_install_dry_run_previews_without_pending_or_writes() {
        let mut app = test_app().await;
        let workspace = app.workspace();
        let source = workspace.join("source/demo");
        std::fs::create_dir_all(&source).expect("source skill");
        std::fs::write(
            source.join("SKILL.md"),
            "---\nname: demo\ndescription: test skill\n---\nbody\n",
        )
        .expect("skill file");

        app.handle_skills(Some(&format!("install {} --dry-run", source.display())))
            .await;

        assert!(matches!(
            app.messages.last(),
            Some(MessageBlock::Prompt(text))
                if text.contains("skill install: demo [project]")
                    && text.contains("dry run: nothing was written")
        ));
        assert!(app.pending_skill.is_none());
        assert!(!workspace.join(".slowcode/skills/demo").exists());
        assert!(!workspace.join(".slowcode/skills-lock.json").exists());
    }

    #[tokio::test]
    async fn skills_install_name_renames_the_installed_frontmatter() {
        let mut app = test_app().await;
        let workspace = app.workspace();
        let source = workspace.join("source/demo");
        std::fs::create_dir_all(&source).expect("source skill");
        std::fs::write(
            source.join("SKILL.md"),
            "---\nname: demo\ndescription: test skill\n---\nbody\n",
        )
        .expect("skill file");

        app.handle_skills(Some(&format!(
            "install {} --name renamed",
            source.display()
        )))
        .await;

        assert!(matches!(
            app.messages.last(),
            Some(MessageBlock::Prompt(text))
                if text.contains("skill install: renamed [project]")
                    && text.contains("rename: demo -> renamed")
        ));
        assert!(app.pending_skill.is_some());

        app.composer = "y".to_owned();
        app.submit().await;

        assert!(matches!(
            app.messages.last(),
            Some(MessageBlock::System(text)) if text.starts_with("skill installed: renamed")
        ));
        let installed =
            std::fs::read_to_string(workspace.join(".slowcode/skills/renamed/SKILL.md"))
                .expect("installed skill");
        assert!(installed.contains("name: renamed"));
        assert!(!installed.contains("name: demo"));
        assert!(!workspace.join(".slowcode/skills/demo").exists());

        app.handle_skills(Some("remove renamed")).await;
        assert!(matches!(
            app.messages.last(),
            Some(MessageBlock::System(text)) if text == "skill removed: renamed [project]"
        ));
        assert!(!workspace.join(".slowcode/skills/renamed").exists());
        let lock = std::fs::read_to_string(workspace.join(".slowcode/skills-lock.json")).unwrap();
        assert!(!lock.contains("project:renamed"));
    }

    #[tokio::test]
    async fn skills_install_name_flag_reports_missing_and_invalid_values() {
        let mut app = test_app().await;

        app.handle_skills(Some(&format!(
            "install {} --name",
            app.workspace().join("source/demo").display()
        )))
        .await;
        assert!(matches!(
            app.messages.last(),
            Some(MessageBlock::Error(text)) if text == "missing name after --name"
        ));

        app.handle_skills(Some("remove demo --dry-run")).await;
        assert!(matches!(
            app.messages.last(),
            Some(MessageBlock::Error(text))
                if text.starts_with("usage: /skills remove")
        ));
    }

    #[test]
    fn skill_flags_parse_trailing_options_in_any_order() {
        let flags = parse_skill_flags(
            "./demo --force --name=renamed --global --dry-run",
            "usage",
            true,
        )
        .expect("flags parse");
        assert_eq!(flags.rest, "./demo");
        assert_eq!(flags.name.as_deref(), Some("renamed"));
        assert_eq!(flags.scope, SkillScope::Global);
        assert!(flags.force);
        assert!(flags.dry_run);

        let flags = parse_skill_flags("./demo --dry-run --role=driver", "usage", true)
            .expect("flags parse");
        assert_eq!(
            flags.scope,
            SkillScope::Role(slowcode_roles::RoleName::Driver)
        );
        assert!(flags.dry_run);
        assert!(!flags.force);

        assert!(parse_skill_flags("./demo --name renamed", "usage", false).is_err());
        assert!(parse_skill_flags("./demo --dry-run", "usage", false).is_err());
    }

    #[tokio::test]
    async fn skills_install_rejects_invalid_names_and_dry_run_keeps_the_scope() {
        let mut app = test_app().await;
        let workspace = app.workspace();
        let source = workspace.join("source/demo");
        std::fs::create_dir_all(&source).expect("source skill");
        std::fs::write(
            source.join("SKILL.md"),
            "---\nname: demo\ndescription: test skill\n---\nbody\n",
        )
        .expect("skill file");

        app.handle_skills(Some(&format!(
            "install {} --name=Bad-Name",
            source.display()
        )))
        .await;
        assert!(matches!(
            app.messages.last(),
            Some(MessageBlock::Error(text))
                if text.starts_with("skill preview:")
                    && text.contains("invalid skill name")
        ));
        assert!(app.pending_skill.is_none());

        app.handle_skills(Some(&format!(
            "install {} --name renamed --role navigator --dry-run",
            source.display()
        )))
        .await;
        assert!(matches!(
            app.messages.last(),
            Some(MessageBlock::Prompt(text))
                if text.contains("skill install: renamed [role navigator]")
                    && text.contains("dry run: nothing was written")
        ));
        assert!(app.pending_skill.is_none());
        assert!(
            !workspace
                .join(".slowcode/roles/navigator/skills/renamed")
                .exists()
        );
        assert!(!workspace.join(".slowcode/skills-lock.json").exists());
    }

    #[tokio::test]
    async fn skills_install_dry_run_reports_an_existing_target_without_writing() {
        let mut app = test_app().await;
        let workspace = app.workspace();
        let source = workspace.join("source/demo");
        std::fs::create_dir_all(&source).expect("source skill");
        std::fs::write(
            source.join("SKILL.md"),
            "---\nname: demo\ndescription: test skill\n---\nbody\n",
        )
        .expect("skill file");
        SkillInstaller::new(&workspace)
            .with_home(&workspace)
            .install(&source, &SkillScope::Project, &InstallOptions::default())
            .expect("initial install");
        let installed = workspace.join(".slowcode/skills/demo/SKILL.md");
        let before = std::fs::read_to_string(&installed).expect("installed skill");

        app.handle_skills(Some(&format!(
            "install {} --dry-run --force",
            source.display()
        )))
        .await;

        assert!(matches!(
            app.messages.last(),
            Some(MessageBlock::Prompt(text))
                if text.contains("(overwrites existing)")
                    && text.contains("dry run: nothing was written")
        ));
        assert!(app.pending_skill.is_none());
        assert_eq!(
            std::fs::read_to_string(&installed).expect("installed skill"),
            before
        );
    }

    #[tokio::test]
    async fn informational_commands_report_state_without_a_running_turn() {
        let mut app = test_app().await;

        app.handle_command(Command::Help(Some("model".to_owned())))
            .await;
        assert!(matches!(
            app.messages.last(),
            Some(MessageBlock::System(text)) if text.contains("usage: /model")
        ));

        app.handle_command(Command::Model(None)).await;
        assert!(matches!(
            app.messages.last(),
            Some(MessageBlock::System(text)) if text.contains("models:")
        ));

        app.handle_command(Command::Cache).await;
        assert!(matches!(
            app.messages.last(),
            Some(MessageBlock::System(text)) if text.starts_with("cache:")
        ));

        app.handle_command(Command::Role(None)).await;
        assert!(matches!(
            app.messages.last(),
            Some(MessageBlock::System(text)) if text.starts_with("role: ")
        ));

        app.handle_command(Command::Unknown("nope".to_owned()))
            .await;
        assert!(matches!(
            app.messages.last(),
            Some(MessageBlock::Error(text)) if text == "unknown command: nope"
        ));
    }

    #[tokio::test]
    async fn feedback_command_writes_a_complete_bundle() {
        let mut app = test_app().await;
        let session_id = app.session_id.clone();
        let out_dir = app.workspace().join("feedback").join(&session_id);

        app.handle_command(Command::Feedback("the plan never paused".to_owned()))
            .await;

        let prompt = match app.messages.last() {
            Some(MessageBlock::Prompt(text)) => text.clone(),
            other => panic!("expected a feedback prompt, got {other:?}"),
        };
        assert!(
            prompt.contains("feedback bundle: the plan never paused"),
            "{prompt}"
        );
        assert!(prompt.contains("reply y to write"), "{prompt}");
        assert!(!out_dir.exists(), "nothing is written before confirmation");

        app.respond_to_prompt("y").await;

        let text = match app.messages.last() {
            Some(MessageBlock::System(text)) => text.clone(),
            other => panic!("expected a system message, got {other:?}"),
        };
        assert!(text.starts_with("feedback bundle:"), "{text}");

        assert!(out_dir.join("issue.md").is_file());
        assert!(out_dir.join("session.jsonl").is_file());
        assert!(out_dir.join("metadata.json").is_file());
        assert!(out_dir.join("manifest.json").is_file());
        let issue = std::fs::read_to_string(out_dir.join("issue.md")).expect("issue");
        assert!(issue.contains("the plan never paused"), "{issue}");
        assert!(issue.contains("complete: `true`"), "{issue}");
        let manifest = std::fs::read_to_string(out_dir.join("manifest.json")).expect("manifest");
        assert!(manifest.contains("\"complete\": true"), "{manifest}");
    }

    #[tokio::test]
    async fn bare_feedback_shows_a_hint_and_cancel_writes_nothing() {
        let mut app = test_app().await;
        let session_id = app.session_id.clone();

        app.handle_command(Command::Feedback(String::new())).await;
        let hint = match app.messages.last() {
            Some(MessageBlock::System(text)) => text.clone(),
            other => panic!("expected a system hint, got {other:?}"),
        };
        assert!(hint.contains("type the problem in plain words"), "{hint}");

        app.handle_command(Command::Feedback("a failure".to_owned()))
            .await;
        app.respond_to_prompt("n").await;
        assert!(matches!(
            app.messages.last(),
            Some(MessageBlock::System(text)) if text == "feedback cancelled"
        ));
        assert!(!app.workspace().join("feedback").join(&session_id).exists());
    }

    #[tokio::test]
    async fn feedback_attaches_the_only_plan_artifact() {
        let mut app = test_app().await;
        let plan_dir = app.workspace().join(".scratch/plans");
        std::fs::create_dir_all(&plan_dir).expect("plan directory");
        let plan = plan_dir.join("plan-1.md");
        std::fs::write(&plan, "# plan\n").expect("plan file");

        app.handle_command(Command::Feedback("plan did not pause".to_owned()))
            .await;
        let prompt = match app.messages.last() {
            Some(MessageBlock::Prompt(text)) => text.clone(),
            other => panic!("expected a prompt, got {other:?}"),
        };
        assert!(prompt.contains(&plan.display().to_string()), "{prompt}");

        app.respond_to_prompt("y").await;
        let out_dir = app
            .workspace()
            .join("feedback")
            .join(app.session_id.clone());
        assert!(out_dir.join("plan.md").is_file());
    }

    #[tokio::test]
    async fn session_commands_refuse_to_interrupt_a_running_turn() {
        let mut app = test_app().await;
        app.running = true;

        let action = app
            .handle_command(Command::Session(SessionCommand::New))
            .await;
        assert!(!action.quit);
        assert!(action.new_events.is_none());
        assert!(matches!(
            app.messages.last(),
            Some(MessageBlock::System(text)) if text == "cancel the current turn first"
        ));

        let action = app
            .handle_command(Command::Role(Some(slowcode_roles::RoleName::Navigator)))
            .await;
        assert!(!action.quit);
        assert!(matches!(
            app.messages.last(),
            Some(MessageBlock::System(text)) if text == "switch roles at a turn boundary"
        ));
    }

    #[test]
    fn skill_install_parser_separates_scope_and_force() {
        assert!(parse_skill_install("list").is_none());

        let request = parse_skill_install("install ./one")
            .expect("install")
            .expect("request");
        assert_eq!(request.path, "./one");
        assert_eq!(request.scope, SkillScope::Project);
        assert!(!request.force);

        let request = parse_skill_install("install ./two --global")
            .expect("install")
            .expect("request");
        assert_eq!(request.path, "./two");
        assert_eq!(request.scope, SkillScope::Global);
        assert!(!request.force);

        let request = parse_skill_install("install ./three --role navigator")
            .expect("install")
            .expect("request");
        assert_eq!(request.path, "./three");
        assert_eq!(
            request.scope,
            SkillScope::Role(slowcode_roles::RoleName::Navigator)
        );
        assert!(!request.force);

        let request = parse_skill_install("install ./four --role=driver --force")
            .expect("install")
            .expect("request");
        assert_eq!(request.path, "./four");
        assert_eq!(
            request.scope,
            SkillScope::Role(slowcode_roles::RoleName::Driver)
        );
        assert!(request.force);

        // Flags may come in either order, and the path is everything left.
        let request = parse_skill_install("install ./five --force --role navigator")
            .expect("install")
            .expect("request");
        assert_eq!(request.path, "./five");
        assert_eq!(
            request.scope,
            SkillScope::Role(slowcode_roles::RoleName::Navigator)
        );
        assert!(request.force);

        let request = parse_skill_install("install ./six --global --force")
            .expect("install")
            .expect("request");
        assert_eq!(request.path, "./six");
        assert_eq!(request.scope, SkillScope::Global);
        assert!(request.force);

        // A directory name with spaces still works when no flags follow it.
        let request = parse_skill_install("install ./my skill")
            .expect("install")
            .expect("request");
        assert_eq!(request.path, "./my skill");
        assert_eq!(request.scope, SkillScope::Project);

        assert_eq!(
            parse_skill_install("install ./x --global --role driver")
                .expect("install")
                .unwrap_err(),
            "use either --global or --role, not both"
        );
        assert_eq!(
            parse_skill_install("install ./x --role")
                .expect("install")
                .unwrap_err(),
            "missing role after --role; use navigator or driver"
        );
        assert_eq!(
            parse_skill_install("install ./x --role reviewer")
                .expect("install")
                .unwrap_err(),
            "unknown role; use navigator or driver"
        );
        assert_eq!(
            parse_skill_install("install ")
                .expect("install")
                .unwrap_err(),
            SKILL_INSTALL_USAGE
        );
    }

    #[test]
    fn skill_remove_parser_separates_name_and_scope() {
        assert!(parse_skill_remove("list").is_none());
        assert!(parse_skill_remove("install ./x").is_none());

        let request = parse_skill_remove("remove demo")
            .expect("remove")
            .expect("request");
        assert_eq!(request.name, "demo");
        assert_eq!(request.scope, SkillScope::Project);

        let request = parse_skill_remove("remove demo --global")
            .expect("remove")
            .expect("request");
        assert_eq!(request.name, "demo");
        assert_eq!(request.scope, SkillScope::Global);

        let request = parse_skill_remove("remove demo --role navigator")
            .expect("remove")
            .expect("request");
        assert_eq!(request.name, "demo");
        assert_eq!(
            request.scope,
            SkillScope::Role(slowcode_roles::RoleName::Navigator)
        );

        let request = parse_skill_remove("remove demo --role=driver")
            .expect("remove")
            .expect("request");
        assert_eq!(request.name, "demo");
        assert_eq!(
            request.scope,
            SkillScope::Role(slowcode_roles::RoleName::Driver)
        );

        assert_eq!(
            parse_skill_remove("remove demo --global --role driver")
                .expect("remove")
                .unwrap_err(),
            "use either --global or --role, not both"
        );
        assert_eq!(
            parse_skill_remove("remove demo --role")
                .expect("remove")
                .unwrap_err(),
            "missing role after --role; use navigator or driver"
        );
        assert_eq!(
            parse_skill_remove("remove demo --role reviewer")
                .expect("remove")
                .unwrap_err(),
            "unknown role; use navigator or driver"
        );
        assert_eq!(
            parse_skill_remove("remove ").expect("remove").unwrap_err(),
            SKILL_REMOVE_USAGE
        );
        assert_eq!(
            parse_skill_remove("remove --global")
                .expect("remove")
                .unwrap_err(),
            SKILL_REMOVE_USAGE
        );
    }

    #[test]
    fn feedback_parser_reads_flags_free_text_and_quotes() {
        let request = parse_feedback_request("").expect("empty request");
        assert!(request.title.is_none());
        assert!(request.note.is_none());
        assert!(request.plan.is_none());

        let request = parse_feedback_request("the plan never paused").expect("free text");
        assert_eq!(request.note.as_deref(), Some("the plan never paused"));

        let request = parse_feedback_request(
            "--title \"plan pause\" --note \"expected a pause\" --plan .scratch/plans/p.md",
        )
        .expect("flags");
        assert_eq!(request.title.as_deref(), Some("plan pause"));
        assert_eq!(request.note.as_deref(), Some("expected a pause"));
        assert_eq!(
            request.plan.as_deref(),
            Some(std::path::Path::new(".scratch/plans/p.md"))
        );

        let request = parse_feedback_request("--title=quick --note=fast").expect("inline flags");
        assert_eq!(request.title.as_deref(), Some("quick"));
        assert_eq!(request.note.as_deref(), Some("fast"));

        assert_eq!(parse_feedback_request("--title"), Err(FEEDBACK_USAGE));
        assert_eq!(parse_feedback_request("--bogus"), Err(FEEDBACK_USAGE));
        assert_eq!(
            parse_feedback_request("free text --note flag note"),
            Err("use either free text or --note, not both")
        );
        assert_eq!(parse_feedback_request("\"unbalanced"), Err(FEEDBACK_USAGE));
    }

    #[test]
    fn skill_scope_label_names_the_destination() {
        assert_eq!(skill_scope_label(&SkillScope::Project), "project");
        assert_eq!(skill_scope_label(&SkillScope::Global), "global");
        assert_eq!(
            skill_scope_label(&SkillScope::Role(slowcode_roles::RoleName::Navigator)),
            "role navigator"
        );
    }

    #[test]
    fn mcp_server_line_reports_state_scope_and_source() {
        let mut server = McpServerConfig {
            name: "demo".to_owned(),
            transport: McpTransport::Stdio {
                command: "npx".to_owned(),
                args: vec!["-y".to_owned(), "demo".to_owned()],
                env: Default::default(),
            },
            trusted: true,
            enabled: true,
            scope: Some(McpScope::Project),
            source: None,
        };

        assert_eq!(
            mcp_server_line(&server, &["demo".to_owned()]),
            "demo [project] active stdio: npx -y demo trusted"
        );
        assert_eq!(
            mcp_server_line(&server, &[]),
            "demo [project] dormant stdio: npx -y demo trusted"
        );

        server.enabled = false;
        assert_eq!(
            mcp_server_line(&server, &[]),
            "demo [project] disabled stdio: npx -y demo trusted"
        );

        server.enabled = true;
        server.trusted = false;
        server.scope = Some(McpScope::User);
        server.transport = McpTransport::Http {
            url: "https://example.test/mcp".to_owned(),
            headers: Default::default(),
        };
        server.source = Some(slowcode_mcp::McpSource {
            kind: "github".to_owned(),
            repo: "demo/repo".to_owned(),
            package: "demo".to_owned(),
            version: "1.0.0".to_owned(),
            integrity: None,
            bin: "demo".to_owned(),
        });
        assert_eq!(
            mcp_server_line(&server, &[]),
            "demo [user] dormant http: https://example.test/mcp ask\n  source demo/repo@1.0.0, npm demo"
        );
    }

    #[test]
    fn tool_result_updates_the_newest_matching_block() {
        let mut messages = vec![MessageBlock::Tool {
            call_id: "call-1".to_owned(),
            name: "write".to_owned(),
            arguments: "{}".to_owned(),
            status: "running".to_owned(),
            result: None,
            is_error: false,
        }];

        assert!(!update_tool_result(
            &mut messages,
            "other",
            false,
            "ignored".to_owned()
        ));
        assert!(update_tool_result(
            &mut messages,
            "call-1",
            false,
            "done".to_owned()
        ));
        assert!(matches!(
            &messages[0],
            MessageBlock::Tool {
                status,
                result,
                is_error: false,
                ..
            } if status == "done" && result.as_deref() == Some("done")
        ));

        assert_eq!(
            tool_result_value(
                Some(&UiToolFailure {
                    name: "write".to_owned(),
                    code: "DENIED".to_owned(),
                }),
                "no".to_owned()
            ),
            "DENIED: no"
        );
        assert_eq!(tool_result_value(None, "plain".to_owned()), "plain");
    }

    #[test]
    fn block_lines_render_reasoning_tool_and_error_blocks() {
        let lines = block_lines(&MessageBlock::Reasoning("hmm".to_owned()));
        assert_eq!(line_text(&lines[0]), "thinking");
        assert_eq!(line_text(&lines[1]), "hmm");

        let lines = block_lines(&MessageBlock::Tool {
            call_id: "call-1".to_owned(),
            name: "write".to_owned(),
            arguments: "{\"path\":\"a\"}".to_owned(),
            status: "done".to_owned(),
            result: Some("ok".to_owned()),
            is_error: false,
        });
        assert_eq!(line_text(&lines[0]), "tool write done");
        assert_eq!(line_text(&lines[1]), "{\"path\":\"a\"}");
        assert_eq!(line_text(&lines[2]), "ok");

        let lines = block_lines(&MessageBlock::Error("bad".to_owned()));
        assert_eq!(line_text(&lines[0]), "error");
        assert_eq!(line_text(&lines[1]), "bad");
    }

    #[test]
    fn help_text_lists_examples_for_named_commands_and_rejects_unknown_ones() {
        assert!(help_text(Some("mcp")).contains("/mcp add ChromeDevTools/chrome-devtools-mcp"));
        assert!(help_text(Some("/skills")).contains("/skills install ./path/to/skill"));
        assert!(
            help_text(Some("/skills")).contains("/skills install ./path/to/skill --role navigator")
        );
        assert!(help_text(Some("plan")).contains("/plan add a line-count summary"));
        assert!(help_text(Some("session")).contains("/session switch session-<uuid>"));
        assert_eq!(
            help_text(Some("bogus")),
            "unknown command: bogus\ntry /help"
        );
    }

    fn key(code: KeyCode, modifiers: KeyModifiers) -> KeyEvent {
        KeyEvent::new(code, modifiers)
    }

    #[tokio::test]
    async fn key_editing_moves_and_deletes_within_the_composer() {
        let mut app = test_app().await;

        for character in ['a', 'b', 'c'] {
            app.handle_key(key(KeyCode::Char(character), KeyModifiers::NONE))
                .await;
        }
        assert_eq!(app.composer, "abc");
        assert_eq!(app.composer_cursor, 3);

        app.handle_key(key(KeyCode::Left, KeyModifiers::NONE)).await;
        assert_eq!(app.composer_cursor, 2);
        app.handle_key(key(KeyCode::Backspace, KeyModifiers::NONE))
            .await;
        assert_eq!(app.composer, "ac");
        assert_eq!(app.composer_cursor, 1);
        app.handle_key(key(KeyCode::Delete, KeyModifiers::NONE))
            .await;
        assert_eq!(app.composer, "a");

        app.handle_key(key(KeyCode::Home, KeyModifiers::NONE)).await;
        assert_eq!(app.composer_cursor, 0);
        app.handle_key(key(KeyCode::End, KeyModifiers::NONE)).await;
        assert_eq!(app.composer_cursor, 1);
    }

    #[tokio::test]
    async fn newline_shortcuts_insert_newlines_and_ctrl_c_quits() {
        let mut app = test_app().await;

        app.handle_key(key(KeyCode::Enter, KeyModifiers::SHIFT))
            .await;
        app.handle_key(key(KeyCode::Char('j'), KeyModifiers::CONTROL))
            .await;
        assert_eq!(app.composer, "\n\n");

        let action = app
            .handle_key(key(KeyCode::Char('c'), KeyModifiers::CONTROL))
            .await;
        assert!(action.quit);

        let release = KeyEvent::new_with_kind(
            KeyCode::Char('x'),
            KeyModifiers::NONE,
            crossterm::event::KeyEventKind::Release,
        );
        let action = app.handle_key(release).await;
        assert!(!action.quit);
        assert_eq!(app.composer, "\n\n");
    }

    #[tokio::test]
    async fn escape_cancels_a_running_turn() {
        let mut app = test_app().await;
        let cancel = CancellationToken::new();
        app.running = true;
        app.cancel = Some(cancel.clone());

        app.handle_key(key(KeyCode::Esc, KeyModifiers::NONE)).await;

        assert!(cancel.is_cancelled());
        assert!(matches!(
            app.messages.last(),
            Some(MessageBlock::System(text)) if text == "当前会话已中断"
        ));
    }

    #[tokio::test]
    async fn a_finished_plan_command_clears_the_busy_state() {
        let mut app = test_app().await;
        app.running = true;
        app.cancel = Some(CancellationToken::new());

        app.handle_app_event(AppEvent::PlanFinished(Err("cancelled".to_owned())));

        assert!(!app.running);
        assert!(app.cancel.is_none());
        assert!(matches!(app.state, RunState::Error));
        assert!(matches!(
            app.messages.last(),
            Some(MessageBlock::Error(text)) if text == "cancelled"
        ));
    }

    #[tokio::test]
    async fn plan_commands_refuse_to_start_while_running() {
        let mut app = test_app().await;
        app.running = true;

        app.handle_plan_command(Command::Scaffold).await;

        assert!(matches!(
            app.messages.last(),
            Some(MessageBlock::System(text)) if text == "cancel the current command first"
        ));
        assert!(app.cancel.is_none());
    }

    #[tokio::test]
    async fn tab_accepts_the_inline_completion() {
        let mut app = test_app().await;
        app.composer = "/ro".to_owned();
        app.composer_cursor = 3;

        app.handle_key(key(KeyCode::Tab, KeyModifiers::NONE)).await;

        assert_eq!(app.composer, "/role");
        assert_eq!(app.composer_cursor, 5);
    }

    #[tokio::test]
    async fn up_down_browse_history_when_the_composer_is_single_line() {
        let mut app = test_app().await;
        app.history = vec!["first".to_owned(), "second".to_owned()];

        app.handle_key(key(KeyCode::Up, KeyModifiers::NONE)).await;
        assert_eq!(app.composer, "second");
        app.handle_key(key(KeyCode::Up, KeyModifiers::NONE)).await;
        assert_eq!(app.composer, "first");
        app.handle_key(key(KeyCode::Down, KeyModifiers::NONE)).await;
        assert_eq!(app.composer, "second");
        app.handle_key(key(KeyCode::Down, KeyModifiers::NONE)).await;
        assert_eq!(app.composer, "");
    }

    #[tokio::test]
    async fn up_down_move_between_lines_when_the_composer_is_multiline() {
        let mut app = test_app().await;
        app.composer = "ab\ncd".to_owned();
        app.composer_cursor = app.composer.len();
        app.history = vec!["unused".to_owned()];

        app.handle_key(key(KeyCode::Up, KeyModifiers::NONE)).await;
        assert_eq!(app.composer_cursor, 2);
        app.handle_key(key(KeyCode::Down, KeyModifiers::NONE)).await;
        assert_eq!(app.composer_cursor, 5);
    }

    #[tokio::test]
    async fn home_end_scroll_when_the_composer_is_empty() {
        let mut app = test_app().await;
        app.last_max_scroll = 10;
        app.follow_tail = true;
        app.scroll = u16::MAX;

        app.handle_key(key(KeyCode::Home, KeyModifiers::NONE)).await;
        assert_eq!(app.scroll, 0);
        assert!(!app.follow_tail);

        app.handle_key(key(KeyCode::End, KeyModifiers::NONE)).await;
        assert_eq!(app.scroll, u16::MAX);
        assert!(app.follow_tail);
    }

    fn mouse(kind: MouseEventKind, column: u16, row: u16) -> MouseEvent {
        MouseEvent {
            kind,
            column,
            row,
            modifiers: KeyModifiers::NONE,
        }
    }

    #[tokio::test]
    async fn mouse_events_dispatch_scroll_and_selection() {
        let mut app = test_app().await;
        app.last_max_scroll = 20;
        app.follow_tail = true;
        app.scroll = u16::MAX;

        app.handle_mouse_event(mouse(MouseEventKind::ScrollUp, 0, 0));
        assert_eq!(app.scroll, 17);
        assert!(!app.follow_tail);

        app.message_area = Rect::new(2, 1, 20, 10);
        app.handle_mouse_event(mouse(MouseEventKind::Down(MouseButton::Left), 3, 5));
        assert!(matches!(
            app.selection,
            Some(Selection {
                anchor: (4, 1),
                cursor: (4, 1),
            })
        ));
        app.handle_mouse_event(mouse(MouseEventKind::Drag(MouseButton::Left), 6, 8));
        assert!(matches!(
            app.selection,
            Some(Selection {
                anchor: (4, 1),
                cursor: (7, 4),
            })
        ));

        app.selection = None;
        app.handle_mouse_event(mouse(MouseEventKind::Up(MouseButton::Left), 6, 8));
        assert!(app.selection.is_none());
    }

    #[test]
    fn role_switches_follow_phase_capability_profiles() {
        use slowcode_app::workflow::Phase;
        use slowcode_roles::RoleName;

        assert!(role_switch_allowed(
            Some(Phase::Interview),
            RoleName::Navigator
        ));
        assert!(!role_switch_allowed(
            Some(Phase::Interview),
            RoleName::Driver
        ));
        assert!(role_switch_allowed(Some(Phase::TestFix), RoleName::Driver));
        assert!(
            !role_switch_allowed(Some(Phase::TestFix), RoleName::Navigator),
            "TestFix keeps driver in v1"
        );
        assert!(role_switch_allowed(
            Some(Phase::AwaitUser),
            RoleName::Navigator
        ));
        assert!(role_switch_allowed(
            Some(Phase::Distill),
            RoleName::Navigator
        ));
        assert!(role_switch_allowed(None, RoleName::Driver));
    }

    #[tokio::test]
    async fn role_command_refuses_navigator_during_test_fix() {
        use slowcode_app::workflow::{
            Domain, ExecState, Strategy, Task, TestContract, WorkflowStore,
        };
        use slowcode_roles::RoleName;

        let mut app = test_app().await;
        let workspace = app.workspace();
        let store = WorkflowStore::new(&workspace);
        let task = Task::new(
            "task-1".to_owned(),
            "read input".to_owned(),
            Domain::Known,
            "evidence".to_owned(),
            ExecState::Executable,
            "implement".to_owned(),
            "reviewer".to_owned(),
            "read".to_owned(),
            "prints".to_owned(),
            Strategy::AiImplements,
        );
        let state = store
            .create_with_tasks("role test", None, vec![task])
            .expect("workflow");
        store
            .approve(&state.workflow_id, Some(TestContract::default()), None)
            .expect("test fix");

        app.handle_role_command(Some(RoleName::Navigator)).await;
        assert!(
            app.messages.iter().any(|message| matches!(
                message,
                MessageBlock::Error(text)
                    if text.contains("phase test_fix uses driver; switch refused")
            )),
            "{:?}",
            app.messages
        );
        assert_eq!(app.role, "driver");
    }

    #[test]
    fn run_state_labels_and_colors_are_stable() {
        let states = [
            (RunState::Idle, "idle", Color::Green),
            (RunState::Streaming, "streaming", Color::Yellow),
            (RunState::Tool, "tool", Color::Cyan),
            (RunState::Approval, "approval", Color::Magenta),
            (RunState::Error, "error", Color::Red),
        ];
        for (state, label, color) in states {
            assert_eq!(state.label(), label);
            assert_eq!(state.color(), color);
        }
    }

    #[test]
    fn mcp_tested_text_counts_hidden_tools() {
        let tools: Vec<String> = (0..10).map(|index| format!("tool-{index}")).collect();
        let text = mcp_tested_text("demo", &tools);
        assert!(text.contains("connected: 10 tool(s)"), "{text}");
        assert!(text.contains("tool-7"), "{text}");
        assert!(!text.contains("tool-8,"), "{text}");
        assert!(text.contains("and 2 more"), "{text}");
        assert!(text.contains("Save? [y/n]"), "{text}");

        let empty = mcp_tested_text("demo", &[]);
        assert!(empty.contains("0 tool(s)"), "{empty}");
        assert!(!empty.contains("tools:"), "{empty}");
    }

    #[test]
    fn mcp_activation_text_names_scope_and_transport() {
        let server = McpServerConfig {
            name: "demo".to_owned(),
            transport: McpTransport::Stdio {
                command: "npx".to_owned(),
                args: vec!["-y".to_owned(), "demo-mcp".to_owned()],
                env: Default::default(),
            },
            trusted: false,
            enabled: true,
            scope: Some(McpScope::User),
            source: None,
        };
        let text = mcp_activation_text(&server);
        assert!(text.contains("MCP demo [user]"), "{text}");
        assert!(text.contains("command npx -y demo-mcp"), "{text}");
    }

    #[tokio::test]
    async fn recovered_approval_can_be_granted_from_the_composer() {
        let mut app = test_app().await;
        let store = WorkflowStore::new(&app.workspace());
        let state = store
            .create_with_tasks("recovered approval", None, Vec::new())
            .expect("workflow");
        let (_, record) = store
            .request_approval(
                &state.workflow_id,
                slowcode_app::workflow::ApprovalRequest {
                    tool_name: "write".to_owned(),
                    arguments: "{}".to_owned(),
                    reason: Some("test".to_owned()),
                    preview: "write src/lib.rs".to_owned(),
                    risk: "medium".to_owned(),
                    side_effect: "writes a file".to_owned(),
                },
            )
            .expect("approval requested");
        app.recovered_approval = Some(record.clone());
        app.respond_to_prompt("d").await;
        assert!(app.recovered_approval.is_some());
        app.respond_to_prompt("x").await;
        assert!(app.recovered_approval.is_some());
        app.respond_to_prompt("y").await;
        assert!(app.recovered_approval.is_none());
        assert_eq!(app.state, RunState::Idle);
        let pending = store
            .pending_approvals(&state.workflow_id)
            .expect("pending approvals");
        assert!(pending.is_empty());
        let resolved = store
            .read_approval(&state.workflow_id, &record.approval_id)
            .expect("record");
        assert_eq!(resolved.status, ApprovalStatus::Approved);
        assert!(
            app.messages.iter().any(|message| matches!(
                message,
                MessageBlock::System(text) if text.contains("Approved")
            )),
            "{:?}",
            app.messages
        );
    }

    #[test]
    fn masked_input_hides_every_character() {
        assert_eq!(masked_input("sk-abc"), "******");
        assert_eq!(masked_input(""), "");
    }

    #[tokio::test]
    async fn login_writes_the_key_without_echoing_it() {
        let mut app = test_app().await;
        let workspace = app.workspace();

        app.handle_command(Command::Login).await;
        assert!(app.pending_login.is_some());

        let secret = "sk-test-secret-123";
        app.composer = secret.to_owned();
        app.composer_cursor = secret.len();
        app.submit().await;

        assert!(app.pending_login.is_none());
        assert!(app.composer.is_empty());
        let contents = std::fs::read_to_string(workspace.join(".env")).expect(".env");
        assert!(contents.contains("DEEPSEEK_API_KEY=sk-test-secret-123"));
        assert!(app.history.iter().all(|entry| !entry.contains(secret)));
        assert!(
            !app.messages.iter().any(|message| match message {
                MessageBlock::System(text)
                | MessageBlock::Prompt(text)
                | MessageBlock::Error(text)
                | MessageBlock::Assistant(text)
                | MessageBlock::User(text) => text.contains(secret),
                _ => false,
            }),
            "{:?}",
            app.messages
        );

        app.handle_command(Command::Logout).await;
        let contents = std::fs::read_to_string(workspace.join(".env")).expect(".env");
        assert!(!contents.contains("DEEPSEEK_API_KEY"));
    }

    #[tokio::test]
    async fn an_invalid_key_keeps_the_entry_open_for_a_retry() {
        let mut app = test_app().await;
        let workspace = app.workspace();

        app.handle_command(Command::Login).await;
        app.composer = "two words".to_owned();
        app.composer_cursor = app.composer.len();
        app.submit().await;

        assert!(app.pending_login.is_some());
        assert!(matches!(
            app.messages.last(),
            Some(MessageBlock::Error(text)) if text.contains("not saved")
        ));
        assert!(!workspace.join(".env").exists());

        app.composer = "sk-retry-works".to_owned();
        app.composer_cursor = app.composer.len();
        app.submit().await;

        assert!(app.pending_login.is_none());
        let contents = std::fs::read_to_string(workspace.join(".env")).expect(".env");
        assert!(contents.contains("DEEPSEEK_API_KEY=sk-retry-works"));
        assert!(app.history.iter().all(|entry| !entry.contains("two words")));
    }

    #[tokio::test]
    async fn login_reports_the_source_and_opens_masked_entry() {
        let mut app = test_app().await;
        let workspace = app.workspace();
        std::fs::write(workspace.join(".env"), "DEEPSEEK_API_KEY=sk-existing\n").expect(".env");

        app.handle_command(Command::Login).await;

        assert!(app.pending_login.is_some());
        assert!(
            app.messages.iter().any(|message| matches!(
                message,
                MessageBlock::System(text)
                    if text.contains("from") && text.contains(".env")
            )),
            "{:?}",
            app.messages
        );
        assert!(
            app.messages.iter().any(|message| matches!(
                message,
                MessageBlock::Prompt(text) if text.contains("Paste a new key to replace it")
            )),
            "{:?}",
            app.messages
        );
    }

    #[tokio::test]
    async fn esc_cancels_credential_entry() {
        let mut app = test_app().await;
        app.handle_command(Command::Login).await;
        app.composer = "sk-half-typed".to_owned();
        app.composer_cursor = app.composer.len();

        app.handle_key(key(KeyCode::Esc, KeyModifiers::NONE)).await;

        assert!(app.pending_login.is_none());
        assert!(app.composer.is_empty());
        assert!(
            !app.workspace().join(".env").exists()
                || !std::fs::read_to_string(app.workspace().join(".env"))
                    .expect(".env")
                    .contains("sk-half-typed")
        );
    }

    #[tokio::test]
    async fn login_replacement_never_becomes_a_prompt() {
        let mut app = test_app().await;
        let workspace = app.workspace();
        std::fs::write(workspace.join(".env"), "DEEPSEEK_API_KEY=sk-old\n").expect(".env");
        let secret = "sk-replacement";

        app.handle_command(Command::Login).await;
        app.composer = secret.to_owned();
        app.composer_cursor = secret.len();
        app.submit().await;

        assert!(app.pending_login.is_none());
        assert!(!app.running);
        assert!(app.history.is_empty());
        assert!(
            !app.messages.iter().any(
                |message| matches!(message, MessageBlock::User(text) if text.contains(secret))
            ),
            "{:?}",
            app.messages
        );
        let contents = std::fs::read_to_string(workspace.join(".env")).expect(".env");
        assert!(contents.contains("DEEPSEEK_API_KEY=sk-replacement"));
        assert!(!contents.contains("sk-old"));
    }
}
