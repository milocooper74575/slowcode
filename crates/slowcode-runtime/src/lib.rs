//! Generic run-level state for slowcode.
//!
//! A run is one executable unit above the agent loop: a Plan command turn, or
//! later a single workflow step. The runtime knows about transitions,
//! checkpoints and run events. It never knows what a Plan phase is.
//!
//! The workflow layer owns phase gates and user questions. It links to a run by
//! an opaque `workflow_id`. Keeping the link opaque is the seam that lets
//! `slowcode-app::workflow` and this crate evolve separately.

mod state;
mod store;

pub use state::{
    PendingTool, RUN_SCHEMA_VERSION, RunBudget, RunErrorInfo, RunModelCall, RunState, RunStatus,
};
pub use store::{ApprovalResolution, RunError, RunEvent, RunLink, RunStore};
