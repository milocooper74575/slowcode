//! Persisted approval records for Plan workflow side effects.
//!
//! `state.json` keeps only the pending id and argument hash. The full record
//! lives in `.scratch/workflows/<workflow_id>/approvals/<id>.json`.

use serde::{Deserialize, Serialize};

use super::contract::sha256_hex;

/// Approval record schema version.
pub const APPROVAL_POLICY_VERSION: u32 = 1;

/// Lifecycle of one approval request.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ApprovalStatus {
    Pending,
    Approved,
    Rejected,
    Cancelled,
    Stale,
}

/// Everything the user needs to decide one side effect.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct ApprovalRecord {
    pub approval_id: String,
    pub workflow_id: String,
    pub tool_name: String,
    pub arguments_hash: String,
    pub workspace: String,
    pub reason: Option<String>,
    pub preview: String,
    pub risk: String,
    pub side_effect: String,
    pub requested_at: u64,
    pub approver: Option<String>,
    pub approved_at: Option<u64>,
    pub policy_version: u32,
    pub status: ApprovalStatus,
}

/// Input for one approval request before an id is assigned.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ApprovalRequest {
    pub tool_name: String,
    pub arguments: String,
    pub reason: Option<String>,
    pub preview: String,
    pub risk: String,
    pub side_effect: String,
}

impl ApprovalRequest {
    /// Hash the raw arguments. Executing a stale request is refused by
    /// comparing a freshly computed hash with the stored one.
    #[must_use]
    pub fn arguments_hash(&self) -> String {
        hash_arguments(&self.arguments)
    }
}

/// Hash raw tool arguments for stale-request detection.
#[must_use]
pub fn hash_arguments(arguments: &str) -> String {
    sha256_hex(arguments.as_bytes())
}

/// What the user decided.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ApprovalDecision {
    Approve,
    Reject,
    Cancel,
}

impl ApprovalDecision {
    #[must_use]
    pub fn event_kind(self) -> &'static str {
        match self {
            Self::Approve => "approval.granted",
            Self::Reject => "approval.rejected",
            Self::Cancel => "approval.cancelled",
        }
    }

    #[must_use]
    pub fn status(self) -> ApprovalStatus {
        match self {
            Self::Approve => ApprovalStatus::Approved,
            Self::Reject => ApprovalStatus::Rejected,
            Self::Cancel => ApprovalStatus::Cancelled,
        }
    }
}

/// One summarized model call.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ModelCall {
    pub turn_id: String,
    pub model: String,
    pub input_tokens: u64,
    pub output_tokens: u64,
    pub cache_hit: Option<bool>,
    pub cost: Option<u64>,
}
