//! Public, renderer-facing projection of committed session events.
//!
//! The agent loop commits every fact to the Session before publishing it here.
//! UI consumers never drive the loop and never see provider-specific values.
//! Text and reasoning deltas are allowed to be dropped when the bounded channel
//! is full. The final [`UiEventKind::AssistantMessage`] carries the complete
//! answer and is authoritative.

use tokio::sync::mpsc::error::TryRecvError;

use super::observer::{
    CommittedUiEvent, CommittedUiKind, CommittedUiReceiver,
    UiAssistantBlockKind as KernelBlockKind, UiAssistantContent,
    UiCompactionError as KernelCompactionError, UiObserverAttachError, UiTokenUsage,
    UiToolFailure as KernelToolFailure, UiTurnEndCancelCause, UiTurnEndReason, UiUserSource,
};

/// Stable identifier and timestamp for one committed UI fact.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct UiEvent {
    pub seq: u64,
    pub time_ms: i64,
    pub kind: UiEventKind,
}

/// Renderer-facing facts. Unknown kernel facts degrade to [`Self::TypeOnly`].
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum UiEventKind {
    TurnStart {
        turn: u64,
    },
    TurnEnd {
        turn: u64,
        reason: UiTurnEnd,
    },
    StepStart {
        turn: u64,
        step: u64,
    },
    StepEnd {
        turn: u64,
        step: u64,
    },
    UserMessage {
        source: UiMessageSource,
        text: String,
        omitted: bool,
    },
    AssistantTextDelta {
        turn: u64,
        step: u64,
        index: u64,
        text: String,
    },
    AssistantReasoningDelta {
        turn: u64,
        step: u64,
        index: u64,
        text: String,
    },
    UsageSample {
        turn: u64,
        step: u64,
        usage: UiUsage,
    },
    AssistantMessage {
        turn: u64,
        step: u64,
        message: UiAssistantMessage,
    },
    ToolRequested {
        turn: u64,
        step: u64,
        call_id: String,
        name: String,
        arguments: String,
        omitted: bool,
    },
    ToolResult {
        turn: u64,
        step: u64,
        call_id: String,
        is_error: bool,
        failure: Option<UiToolFailure>,
        content: String,
        meta: String,
        omitted: bool,
    },
    TodoWrite {
        todos: Vec<UiTodo>,
    },
    RequestContextChanged {
        provider: Option<String>,
        model: Option<String>,
        context_window: Option<u64>,
    },
    CompactionStarted {
        id: String,
        trigger: Option<String>,
        shadowed_nodes: Option<usize>,
    },
    CompactionSummarized {
        id: String,
        shadowed_tokens: u64,
        provider: String,
        model: String,
        usage: Option<UiUsage>,
    },
    CompactionEnded {
        id: String,
        error: Option<UiCompactionError>,
    },
    CompactionPruneMarked {
        shadowed_tokens: u64,
    },
    ApprovalAsked {
        id: String,
        tool_name: String,
        call_id: Option<String>,
        reason: Option<String>,
    },
    ApprovalDecided {
        id: String,
        outcome: String,
    },
    RetryScheduled {
        retry_id: String,
        retry: u64,
        provider: String,
        delay_ms: u64,
        max_retries: Option<u64>,
        failure_code: String,
        failure_message: String,
    },
    RetryStarted {
        retry_id: String,
        retry: u64,
    },
    TypeOnly {
        event_type: &'static str,
    },
}

/// Terminal state of one turn.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum UiTurnEnd {
    Completed,
    Cancelled { cause: String },
    Blocked,
    Error { code: String, message: String },
    MaxTokens,
    Interrupted,
    Other { kind: Option<String> },
}

/// Source of a user-role message.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum UiMessageSource {
    Human,
    Context {
        plugin: String,
        form: Option<String>,
    },
    Other {
        kind: String,
    },
}

/// Token counters attached to one usage sample.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct UiUsage {
    pub input_tokens: u64,
    pub output_tokens: u64,
    pub cache_read_tokens: Option<u64>,
    pub cache_write_tokens: Option<u64>,
    pub reasoning_tokens: Option<u64>,
}

/// One assistant content block in commit order.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct UiAssistantBlock {
    pub index: u16,
    pub kind: UiAssistantBlockKind,
    pub text: String,
}

/// Public assistant block kind.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum UiAssistantBlockKind {
    Text,
    Reasoning,
}

/// Complete assistant message projected for rendering.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct UiAssistantMessage {
    pub blocks: Vec<UiAssistantBlock>,
    pub text: String,
    pub reasoning: String,
    pub provider: String,
    pub model: String,
    pub usage: Option<UiUsage>,
}

/// One todo snapshot entry.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct UiTodo {
    pub content: String,
    pub status: String,
}

/// One tool failure summary.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct UiToolFailure {
    pub name: String,
    pub code: String,
}

/// Failure of one compaction.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct UiCompactionError {
    pub code: Option<String>,
    pub message: String,
}

/// Why a UI observation could not be attached.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum UiAttachError {
    NotFresh,
    AlreadyAttached,
}

impl std::fmt::Display for UiAttachError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::NotFresh => formatter
                .write_str("the live Session observer may only attach before the first event"),
            Self::AlreadyAttached => {
                formatter.write_str("the live Session observer was already attached")
            }
        }
    }
}

impl std::error::Error for UiAttachError {}

impl From<UiObserverAttachError> for UiAttachError {
    fn from(error: UiObserverAttachError) -> Self {
        match error {
            UiObserverAttachError::NotFresh => Self::NotFresh,
            UiObserverAttachError::AlreadyAttached => Self::AlreadyAttached,
        }
    }
}

/// Non-blocking receive error.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum UiTryRecvError {
    Empty,
    Disconnected,
}

/// Bounded receiver for committed UI facts.
pub struct UiEventReceiver {
    receiver: CommittedUiReceiver,
}

impl UiEventReceiver {
    pub(crate) fn new(receiver: CommittedUiReceiver) -> Self {
        Self { receiver }
    }

    /// Wait for the next committed fact. Returns `None` when the producer is
    /// gone.
    pub async fn recv(&mut self) -> Option<UiEvent> {
        self.receiver.recv().await.map(UiEvent::from_committed)
    }

    /// Return the next fact without waiting.
    pub fn try_recv(&mut self) -> Result<UiEvent, UiTryRecvError> {
        self.receiver
            .try_recv()
            .map(UiEvent::from_committed)
            .map_err(|error| match error {
                TryRecvError::Empty => UiTryRecvError::Empty,
                TryRecvError::Disconnected => UiTryRecvError::Disconnected,
            })
    }

    /// Whether the producer permanently detached after a projection or
    /// backpressure failure.
    pub fn is_producer_faulted(&self) -> bool {
        self.receiver.is_producer_faulted()
    }
}

impl UiEvent {
    fn from_committed(event: CommittedUiEvent) -> Self {
        Self {
            seq: event.seq.get(),
            time_ms: event.time.get(),
            kind: UiEventKind::from_kind(event.kind),
        }
    }
}

impl UiEventKind {
    fn from_kind(kind: CommittedUiKind) -> Self {
        match kind {
            CommittedUiKind::TurnStart { turn } => Self::TurnStart { turn: turn.get() },
            CommittedUiKind::TurnEnd { turn, reason } => Self::TurnEnd {
                turn: turn.get(),
                reason: UiTurnEnd::from_reason(reason),
            },
            CommittedUiKind::StepStart { turn, step } => Self::StepStart {
                turn: turn.get(),
                step: step.get(),
            },
            CommittedUiKind::StepEnd { turn, step } => Self::StepEnd {
                turn: turn.get(),
                step: step.get(),
            },
            CommittedUiKind::UserMessage { source, content } => Self::UserMessage {
                source: UiMessageSource::from_source(source),
                text: content.as_str().unwrap_or_default().to_owned(),
                omitted: content.was_omitted(),
            },
            CommittedUiKind::AssistantTextDelta {
                turn,
                step,
                index,
                text,
            } => Self::AssistantTextDelta {
                turn: turn.get(),
                step: step.get(),
                index,
                text,
            },
            CommittedUiKind::AssistantReasoningDelta {
                turn,
                step,
                index,
                text,
            } => Self::AssistantReasoningDelta {
                turn: turn.get(),
                step: step.get(),
                index,
                text,
            },
            CommittedUiKind::UsageSample { turn, step, usage } => Self::UsageSample {
                turn: turn.get(),
                step: step.get(),
                usage: UiUsage::from_usage(usage),
            },
            CommittedUiKind::AssistantMessage {
                turn,
                step,
                content,
                provider,
                model,
                usage,
                ..
            } => Self::AssistantMessage {
                turn: turn.get(),
                step: step.get(),
                message: UiAssistantMessage::from_content(content, provider, model, usage),
            },
            CommittedUiKind::ToolRequested {
                turn,
                step,
                call_id,
                name,
                arguments,
            } => Self::ToolRequested {
                turn: turn.get(),
                step: step.get(),
                call_id: call_id.as_str().to_owned(),
                name: name.as_str().to_owned(),
                arguments: arguments.as_str().unwrap_or_default().to_owned(),
                omitted: arguments.was_omitted(),
            },
            CommittedUiKind::ToolResult {
                turn,
                step,
                call_id,
                is_error,
                failure,
                content,
                meta,
                ..
            } => {
                let omitted = content.was_omitted() || meta.was_omitted();
                Self::ToolResult {
                    turn: turn.get(),
                    step: step.get(),
                    call_id: call_id.as_str().to_owned(),
                    is_error,
                    failure: failure.map(UiToolFailure::from_failure),
                    content: content.as_str().unwrap_or_default().to_owned(),
                    meta: meta.as_str().unwrap_or_default().to_owned(),
                    omitted,
                }
            }
            CommittedUiKind::TodoWrite { todos } => Self::TodoWrite {
                todos: todos
                    .into_iter()
                    .map(|todo| UiTodo {
                        content: todo.content,
                        status: format!("{:?}", todo.status).to_lowercase(),
                    })
                    .collect(),
            },
            CommittedUiKind::RequestContextChanged {
                provider,
                model,
                context_window,
            } => Self::RequestContextChanged {
                provider: provider.map(|value| value.as_str().to_owned()),
                model: model.map(|value| value.as_str().to_owned()),
                context_window,
            },
            CommittedUiKind::CompactionStarted {
                id,
                trigger,
                shadowed_nodes,
                ..
            } => Self::CompactionStarted {
                id: id.as_str().to_owned(),
                trigger: trigger.map(|value| format!("{value:?}")),
                shadowed_nodes,
            },
            CommittedUiKind::CompactionSummarized {
                id,
                shadowed_tokens,
                provider,
                model,
                usage,
            } => Self::CompactionSummarized {
                id: id.as_str().to_owned(),
                shadowed_tokens,
                provider: provider.as_str().to_owned(),
                model: model.as_str().to_owned(),
                usage: usage.map(UiUsage::from_usage),
            },
            CommittedUiKind::CompactionEnded { id, error, .. } => Self::CompactionEnded {
                id: id.as_str().to_owned(),
                error: error.map(UiCompactionError::from_error),
            },
            CommittedUiKind::CompactionPruneMarked {
                shadowed_tokens, ..
            } => Self::CompactionPruneMarked { shadowed_tokens },
            CommittedUiKind::ApprovalAsked {
                id,
                tool_name,
                call_id,
                reason,
            } => Self::ApprovalAsked {
                id: id.as_str().to_owned(),
                tool_name: tool_name.as_str().to_owned(),
                call_id: call_id.map(|value| value.as_str().to_owned()),
                reason,
            },
            CommittedUiKind::ApprovalDecided { id, outcome } => Self::ApprovalDecided {
                id: id.as_str().to_owned(),
                outcome: format!("{outcome:?}"),
            },
            CommittedUiKind::RetryScheduled {
                retry_id,
                retry,
                provider,
                delay_ms,
                max_retries,
                failure_code,
                failure_message,
            } => Self::RetryScheduled {
                retry_id: retry_id.as_str().to_owned(),
                retry: retry.get(),
                provider: provider.as_str().to_owned(),
                delay_ms: delay_ms as u64,
                max_retries: max_retries.map(|value| value.get()),
                failure_code,
                failure_message,
            },
            CommittedUiKind::RetryStarted { retry_id, retry } => Self::RetryStarted {
                retry_id: retry_id.as_str().to_owned(),
                retry: retry.get(),
            },
            CommittedUiKind::TypeOnly { event_type } => Self::TypeOnly { event_type },
        }
    }
}

impl UiTurnEnd {
    fn from_reason(reason: UiTurnEndReason) -> Self {
        match reason {
            UiTurnEndReason::Completed => Self::Completed,
            UiTurnEndReason::Aborted { cause } => Self::Cancelled {
                cause: match cause {
                    UiTurnEndCancelCause::User => "user".to_owned(),
                    UiTurnEndCancelCause::Parent => "parent".to_owned(),
                    UiTurnEndCancelCause::Hook => "hook".to_owned(),
                    UiTurnEndCancelCause::Disposed => "disposed".to_owned(),
                    UiTurnEndCancelCause::Legacy => "legacy".to_owned(),
                },
            },
            UiTurnEndReason::Blocked => Self::Blocked,
            UiTurnEndReason::Error { code, message } => Self::Error { code, message },
            UiTurnEndReason::MaxTokens => Self::MaxTokens,
            UiTurnEndReason::Interrupted => Self::Interrupted,
            UiTurnEndReason::Other { kind } => Self::Other { kind },
        }
    }
}

impl UiMessageSource {
    fn from_source(source: UiUserSource) -> Self {
        match source {
            UiUserSource::Human => Self::Human,
            UiUserSource::Context { plugin, form } => Self::Context {
                plugin,
                form: form.map(|value| format!("{value:?}")),
            },
            UiUserSource::Other { kind } => Self::Other { kind },
        }
    }
}

impl UiUsage {
    fn from_usage(usage: UiTokenUsage) -> Self {
        Self {
            input_tokens: usage.input_tokens,
            output_tokens: usage.output_tokens,
            cache_read_tokens: usage.cache_read_tokens,
            cache_write_tokens: usage.cache_write_tokens,
            reasoning_tokens: usage.reasoning_tokens,
        }
    }
}

impl UiAssistantMessage {
    fn from_content(
        content: UiAssistantContent,
        provider: super::observer::UiIdentity,
        model: super::observer::UiIdentity,
        usage: Option<UiTokenUsage>,
    ) -> Self {
        let mut blocks = Vec::new();
        let mut text = String::new();
        let mut reasoning = String::new();
        match content {
            UiAssistantContent::Indexed(entries) => {
                for entry in entries {
                    let kind = match entry.kind {
                        KernelBlockKind::Text => {
                            text.push_str(&entry.text);
                            UiAssistantBlockKind::Text
                        }
                        KernelBlockKind::Reasoning => {
                            reasoning.push_str(&entry.text);
                            UiAssistantBlockKind::Reasoning
                        }
                    };
                    blocks.push(UiAssistantBlock {
                        index: entry.index,
                        kind,
                        text: entry.text,
                    });
                }
            }
            UiAssistantContent::Degraded { text: whole } => {
                text = whole.clone();
                blocks.push(UiAssistantBlock {
                    index: 0,
                    kind: UiAssistantBlockKind::Text,
                    text: whole,
                });
            }
        }
        Self {
            blocks,
            text,
            reasoning,
            provider: provider.as_str().to_owned(),
            model: model.as_str().to_owned(),
            usage: usage.map(UiUsage::from_usage),
        }
    }
}

impl UiToolFailure {
    fn from_failure(failure: KernelToolFailure) -> Self {
        Self {
            name: failure.name,
            code: failure.code,
        }
    }
}

impl UiCompactionError {
    fn from_error(error: KernelCompactionError) -> Self {
        Self {
            code: error.code,
            message: error.message,
        }
    }
}
