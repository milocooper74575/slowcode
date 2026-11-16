//! Run state and its explicit transition table.

use serde::{Deserialize, Serialize};

/// Schema version written into `state.json`.
pub const RUN_SCHEMA_VERSION: u32 = 1;

/// Lifecycle of one run.
///
/// The table mirrors `docs/writing/生产级内核.md` section 4. Terminal states are
/// irreversible. `TimedOut` may be entered from any active state because a
/// deadline can fire between any two steps.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RunStatus {
    Queued,
    CompilingContext,
    CallingModel,
    AwaitingApproval,
    ExecutingTool,
    Completed,
    Failed,
    Cancelled,
    TimedOut,
}

impl RunStatus {
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Queued => "queued",
            Self::CompilingContext => "compiling_context",
            Self::CallingModel => "calling_model",
            Self::AwaitingApproval => "awaiting_approval",
            Self::ExecutingTool => "executing_tool",
            Self::Completed => "completed",
            Self::Failed => "failed",
            Self::Cancelled => "cancelled",
            Self::TimedOut => "timed_out",
        }
    }

    #[must_use]
    pub fn is_terminal(self) -> bool {
        matches!(
            self,
            Self::Completed | Self::Failed | Self::Cancelled | Self::TimedOut
        )
    }

    /// Whether one transition is allowed.
    #[must_use]
    pub fn can_transition_to(self, next: Self) -> bool {
        if self.is_terminal() {
            return false;
        }
        if next == Self::TimedOut {
            return true;
        }
        match self {
            Self::Queued => matches!(
                next,
                Self::CompilingContext | Self::Cancelled | Self::Failed
            ),
            // A deterministic command can finish while compiling context, for
            // example `/test` or `/approve`, which run no model call. It can
            // also pause for a tool approval without entering a model call,
            // because the approval is requested mid-command.
            Self::CompilingContext => matches!(
                next,
                Self::CallingModel
                    | Self::AwaitingApproval
                    | Self::Completed
                    | Self::Cancelled
                    | Self::Failed
            ),
            Self::CallingModel => matches!(
                next,
                Self::Completed
                    | Self::AwaitingApproval
                    | Self::ExecutingTool
                    | Self::Cancelled
                    | Self::Failed
            ),
            Self::AwaitingApproval => matches!(
                next,
                // A denied approval returns control to the command without
                // executing the tool.
                Self::CompilingContext | Self::ExecutingTool | Self::Cancelled | Self::Failed
            ),
            Self::ExecutingTool => {
                matches!(
                    next,
                    Self::CompilingContext | Self::Cancelled | Self::Failed
                )
            }
            Self::Completed | Self::Failed | Self::Cancelled | Self::TimedOut => false,
        }
    }
}

impl std::fmt::Display for RunStatus {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(self.as_str())
    }
}

/// A tool call waiting for approval.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct PendingTool {
    pub call_id: String,
    pub name: String,
    pub arguments_hash: String,
    pub requested_at: u64,
}

/// One model call summarized for the run journal.
///
/// The caller redacts the payloads before archiving them and computes the
/// prompt hash, so the runtime stores bytes without knowing the policy.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct RunModelCall {
    pub turn_id: String,
    pub model: String,
    pub prompt_hash: String,
    pub input_tokens: u64,
    pub output_tokens: u64,
    pub cache_hit: Option<bool>,
    pub cost: Option<u64>,
    pub redactions: u64,
}

/// Failure detail kept in the run snapshot.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct RunErrorInfo {
    pub kind: String,
    pub message: String,
}

/// Usage accounting for one run. Budget enforcement lands in a later slice;
/// the counters are persisted now so recovery does not lose them.
#[derive(Clone, Debug, Default, Eq, PartialEq, Serialize, Deserialize)]
pub struct RunBudget {
    pub model_calls: u64,
    pub input_tokens: u64,
    pub output_tokens: u64,
    pub cost_micros: u64,
    pub reserved_micros: u64,
}

/// One run snapshot. This is the source of truth for run status.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct RunState {
    pub schema_version: u32,
    pub run_id: String,
    /// Opaque link to the owning workflow, if any. The runtime never parses it.
    pub workflow_id: Option<String>,
    /// Caller-defined operation name, for example `plan` or `fix`.
    pub operation: String,
    pub status: RunStatus,
    pub version: u64,
    pub last_seq: u64,
    pub step: u64,
    pub max_steps: u64,
    pub created_at: u64,
    pub updated_at: u64,
    pub finished_at: Option<u64>,
    pub pending_tool: Option<PendingTool>,
    pub error: Option<RunErrorInfo>,
    pub budget: RunBudget,
}

impl RunState {
    #[must_use]
    pub fn new(
        run_id: String,
        workflow_id: Option<String>,
        operation: String,
        max_steps: u64,
        now: u64,
    ) -> Self {
        Self {
            schema_version: RUN_SCHEMA_VERSION,
            run_id,
            workflow_id,
            operation,
            status: RunStatus::Queued,
            version: 1,
            last_seq: 1,
            step: 0,
            max_steps,
            created_at: now,
            updated_at: now,
            finished_at: None,
            pending_tool: None,
            error: None,
            budget: RunBudget::default(),
        }
    }
}
