//! Plan workflow state, journal and gates.
//!
//! Step one keeps this module inside `slowcode-app`. It is intentionally shaped
//! so it can move to a dedicated `slowcode-workflow` crate later without
//! changing the public data model.

pub mod approval;
pub mod budget;
pub mod contract;
pub mod input;
pub mod manifest;
pub mod readonly_shell;
pub mod selector;
mod state;
mod store;
pub mod test_exec;

/// Directory names skipped when walking a workspace for project files.
pub(crate) const SCAN_EXCLUDED_DIRS: &[&str] = &[
    ".git",
    ".scratch",
    ".slowcode",
    "target",
    "node_modules",
    ".venv",
    "__pycache__",
    ".pytest_cache",
    "dist",
    "build",
];

pub(crate) fn is_scan_excluded(path: &std::path::Path) -> bool {
    path.file_name()
        .and_then(|name| name.to_str())
        .is_some_and(|name| SCAN_EXCLUDED_DIRS.contains(&name))
}

pub use approval::{
    APPROVAL_POLICY_VERSION, ApprovalDecision, ApprovalRecord, ApprovalRequest, ApprovalStatus,
    ModelCall, hash_arguments,
};
pub use budget::{
    BudgetError, BudgetSource, BudgetValue, DEFAULT_MAX_ACTIVE_SECONDS,
    DEFAULT_MAX_CONSECUTIVE_SAME_FAILURE, DEFAULT_MAX_MODEL_CALLS_PER_ROUND,
    DEFAULT_MAX_OUTPUT_TOKENS_PER_ROUND, DEFAULT_MAX_ROUNDS, WorkflowBudget,
};
pub use contract::{
    CORE_MARKER, ContractError, FileStamp, ManifestFile, build_contract, build_review_contract,
    changed_files, looks_like_test_file, sha256_hex, snapshot_workspace, verify_core_implemented,
    verify_hashes, verify_test_contract_hashes, verify_test_flip,
};
pub use input::{InputError, InputSnapshot, MAX_INPUT_BYTES, SecretPolicy, read_answer_file};
pub use manifest::{ContextManifest, SkillManifestEntry, build_skill_manifest};
pub use readonly_shell::{AuditedReadonlyShell, ReadonlyShellHandle, readonly_shell_schema};
pub use selector::{SelectorChoice, derive_selector};
pub use state::{
    AnswerSnapshot, AnswerSource, BlockedState, BudgetState, CapabilityProfile, CoreRegion,
    DistillState, Domain, ExecState, GateError, PendingApproval, Phase, Question, QuestionKind,
    QuestionStatus, ReviewState, SCHEMA_VERSION, ScaffoldContract, Strategy, Task, TestContract,
    TestRunRecord, TestState, TestStatus, WorkflowState, WorkflowStatus, changed_task_ids,
};
pub use store::{
    AnswerUpdate, ClassifiedFailure, DistillAnswerUpdate, DistillDecision, TestRunUpdate,
    UnknownGap, WorkflowError, WorkflowEvent, WorkflowListing, WorkflowLock, WorkflowStore,
    WorkflowSummary,
};
pub use test_exec::{
    DEFAULT_TEST_TIMEOUT_SECONDS, ProjectTestConfig, Runner, TestExecError, TestRun,
    detect_project, validate_selector,
};
