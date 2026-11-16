//! Durable run journal.
//!
//! Layout:
//!
//! ```text
//! <root>/.lock
//! <root>/<run_id>/state.json
//! <root>/<run_id>/events.jsonl
//! ```
//!
//! Every transition appends one event. The first event of a transition carries
//! a full checkpoint. If `state.json` is missing or stale, the newest
//! checkpoint rebuilds it. A torn final line is ignored, matching the feedback
//! journal rule. Retrying a transition reuses its `transition_id`, so the event
//! is not appended twice.

use std::{
    fs::{self, OpenOptions},
    io::{self, Write},
    path::{Path, PathBuf},
    sync::atomic::{AtomicU64, Ordering},
    time::{SystemTime, UNIX_EPOCH},
};

use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use thiserror::Error;

use crate::state::{PendingTool, RUN_SCHEMA_VERSION, RunBudget, RunModelCall, RunState, RunStatus};

/// How one pending tool call was resolved.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ApprovalResolution {
    /// The user allowed the call; the tool runs next.
    Granted,
    /// The user denied the call; the command continues without it.
    Denied,
}

impl ApprovalResolution {
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Granted => "granted",
            Self::Denied => "denied",
        }
    }
}

/// Association between one run and its caller.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct RunLink {
    /// Opaque workflow id. The runtime never parses it.
    pub workflow_id: Option<String>,
    /// Caller-defined operation name.
    pub operation: String,
}

/// One durable run event.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct RunEvent {
    pub event_id: String,
    pub transition_id: String,
    pub seq: u64,
    pub at: u64,
    pub status: RunStatus,
    pub kind: String,
    pub payload: Value,
    pub checkpoint: Option<RunState>,
}

/// Run journal rooted at a caller-provided directory.
#[derive(Clone, Debug)]
pub struct RunStore {
    root: PathBuf,
}

/// Exclusive writer lock. Dropping it removes the lock file.
pub struct RunLock {
    path: PathBuf,
}

impl Drop for RunLock {
    fn drop(&mut self) {
        let _ = fs::remove_file(&self.path);
    }
}

impl RunStore {
    #[must_use]
    pub fn new(root: impl Into<PathBuf>) -> Self {
        Self { root: root.into() }
    }

    #[must_use]
    pub fn root(&self) -> &Path {
        &self.root
    }

    /// Create one run in `Queued` and write its first checkpoint.
    pub fn start(&self, link: RunLink, max_steps: u64) -> Result<RunState, RunError> {
        let _lock = self.lock()?;
        let now = now_ms();
        let run_id = next_run_id(now);
        let state = RunState::new(run_id, link.workflow_id, link.operation, max_steps, now);
        let transition_id = format!("{}-v1", state.run_id);
        self.append_event(
            &state,
            &transition_id,
            "run.created",
            json!({
                "workflow_id": state.workflow_id,
                "operation": state.operation,
            }),
            Some(&state),
        )?;
        self.write_state(&state)?;
        Ok(state)
    }

    /// Load one run snapshot, recovering from the newest checkpoint.
    pub fn load(&self, run_id: &str) -> Result<RunState, RunError> {
        validate_id(run_id)?;
        let events = self.read_events(run_id)?;
        let checkpoint = events
            .iter()
            .rev()
            .find_map(|event| event.checkpoint.clone())
            .filter(|state| state.run_id == run_id);
        let path = self.state_path(run_id);
        let mut state = match fs::read(&path) {
            Ok(bytes) => match serde_json::from_slice::<RunState>(&bytes) {
                Ok(state) => state,
                Err(error) => checkpoint
                    .clone()
                    .ok_or_else(|| RunError::Corrupt(error.to_string()))?,
            },
            Err(error) if error.kind() == io::ErrorKind::NotFound => checkpoint
                .clone()
                .ok_or_else(|| RunError::NotFound(run_id.to_owned()))?,
            Err(error) => return Err(RunError::Io(error)),
        };
        if let Some(checkpoint) = checkpoint
            && checkpoint.version > state.version
        {
            state = checkpoint;
        }
        if state.schema_version != RUN_SCHEMA_VERSION {
            return Err(RunError::UnsupportedSchema(state.schema_version));
        }
        Ok(state)
    }

    /// Read the valid prefix of `events.jsonl`.
    ///
    /// A torn final line or an invalid line stops the scan. Everything from
    /// that point on is ignored.
    pub fn read_events(&self, run_id: &str) -> Result<Vec<RunEvent>, RunError> {
        validate_id(run_id)?;
        let path = self.events_path(run_id);
        let contents = match fs::read_to_string(&path) {
            Ok(contents) => contents,
            Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(Vec::new()),
            Err(error) => return Err(RunError::Io(error)),
        };
        let mut events = Vec::new();
        let mut last_seq = 0_u64;
        for line in contents.lines() {
            if line.trim().is_empty() {
                continue;
            }
            let Ok(event) = serde_json::from_str::<RunEvent>(line) else {
                break;
            };
            if event.seq <= last_seq {
                break;
            }
            last_seq = event.seq;
            events.push(event);
        }
        Ok(events)
    }

    /// Move one run to a new status.
    ///
    /// Repeating a transition with the same `transition_id` only rewrites the
    /// snapshot. It never appends a second event.
    pub fn transition(
        &self,
        run_id: &str,
        to: RunStatus,
        detail: Value,
    ) -> Result<RunState, RunError> {
        self.transition_inner(run_id, to, event_kind(to), detail, |_| {})
    }

    /// Record that one tool call is waiting for approval.
    ///
    /// The status change and the `pending_tool` field are written in the same
    /// transition, so a reader never observes one without the other. A run
    /// that just ran an allowed tool is first returned to `CompilingContext`,
    /// because approvals are sequential within one command.
    pub fn set_pending_tool(&self, run_id: &str, tool: PendingTool) -> Result<RunState, RunError> {
        let state = self.load(run_id)?;
        if state.status == RunStatus::ExecutingTool {
            self.transition_inner(
                run_id,
                RunStatus::CompilingContext,
                "run.transitioned",
                json!({ "reason": "tool_finished" }),
                |_| {},
            )?;
        }
        let detail = json!({
            "call_id": tool.call_id,
            "name": tool.name,
            "arguments_hash": tool.arguments_hash,
            "requested_at": tool.requested_at,
        });
        self.transition_inner(
            run_id,
            RunStatus::AwaitingApproval,
            "run.approval_requested",
            detail,
            move |state| state.pending_tool = Some(tool),
        )
    }

    /// Clear the pending tool and record the user's decision.
    ///
    /// A granted call moves to `ExecutingTool`. A denied call returns to
    /// `CompilingContext`: the command may still succeed without that tool.
    pub fn resolve_pending_tool(
        &self,
        run_id: &str,
        resolution: ApprovalResolution,
    ) -> Result<RunState, RunError> {
        let to = match resolution {
            ApprovalResolution::Granted => RunStatus::ExecutingTool,
            ApprovalResolution::Denied => RunStatus::CompilingContext,
        };
        self.transition_inner(
            run_id,
            to,
            "run.approval_resolved",
            json!({ "resolution": resolution.as_str() }),
            |state| state.pending_tool = None,
        )
    }

    /// Cancel one run without pretending it completed.
    pub fn cancel(&self, run_id: &str, reason: &str) -> Result<RunState, RunError> {
        self.transition_inner(
            run_id,
            RunStatus::Cancelled,
            "run.cancelled",
            json!({ "reason": reason }),
            |_| {},
        )
    }

    /// Archive one redacted model call under the run and append its event.
    ///
    /// The payloads are written before the event, so a crash leaves at most an
    /// orphan file, never a reference to a missing one. Status is unchanged:
    /// this is usage evidence, not a transition.
    pub fn archive_model_call(
        &self,
        run_id: &str,
        call: RunModelCall,
        request: &str,
        response: &str,
    ) -> Result<RunState, RunError> {
        let _lock = self.lock()?;
        let state = self.load(run_id)?;
        if state.status.is_terminal() {
            return Err(RunError::Finished(run_id.to_owned()));
        }
        validate_id(&call.turn_id)?;
        self.write_model_call_files(run_id, &call.turn_id, request, response)?;
        self.append_model_call(run_id, state, call)
    }

    fn append_model_call(
        &self,
        run_id: &str,
        state: RunState,
        call: RunModelCall,
    ) -> Result<RunState, RunError> {
        let next_version = state.version + 1;
        let transition_id = format!("{run_id}-v{next_version}");
        if let Some(recovered) =
            self.replayed_side_event(run_id, &state, next_version, &transition_id)?
        {
            self.write_state(&recovered)?;
            return Ok(recovered);
        }

        let final_state = side_event_state(&state, next_version);
        self.append_event(
            &final_state,
            &transition_id,
            "run.model_call",
            model_call_payload(call),
            Some(&final_state),
        )?;
        self.write_state(&final_state)?;
        Ok(final_state)
    }

    fn write_model_call_files(
        &self,
        run_id: &str,
        turn_id: &str,
        request: &str,
        response: &str,
    ) -> Result<(), RunError> {
        let directory = self.run_dir(run_id).join("model-calls").join(turn_id);
        fs::create_dir_all(&directory)?;
        write_atomic(&directory.join("request.txt"), request.as_bytes())?;
        write_atomic(&directory.join("response.txt"), response.as_bytes())?;
        Ok(())
    }

    /// Return the state recorded for a replayed side event, if one exists.
    fn replayed_side_event(
        &self,
        run_id: &str,
        state: &RunState,
        next_version: u64,
        transition_id: &str,
    ) -> Result<Option<RunState>, RunError> {
        let existing = self.read_events(run_id)?;
        let Some(event) = existing
            .iter()
            .rev()
            .find(|event| event.transition_id == transition_id)
        else {
            return Ok(None);
        };
        let recovered = event
            .checkpoint
            .clone()
            .unwrap_or_else(|| side_event_state(state, next_version));
        if recovered.schema_version != RUN_SCHEMA_VERSION {
            return Err(RunError::UnsupportedSchema(recovered.schema_version));
        }
        Ok(Some(recovered))
    }

    fn transition_inner<F>(
        &self,
        run_id: &str,
        to: RunStatus,
        kind: &str,
        detail: Value,
        update: F,
    ) -> Result<RunState, RunError>
    where
        F: FnOnce(&mut RunState),
    {
        let _lock = self.lock()?;
        let state = self.load(run_id)?;
        if !state.status.can_transition_to(to) {
            return Err(RunError::InvalidTransition {
                from: state.status,
                to,
            });
        }

        let next_version = state.version + 1;
        let transition_id = format!("{run_id}-v{next_version}");
        let (mut final_state, retried) =
            self.prepare_transition(&state, to, next_version, &transition_id)?;
        update(&mut final_state);
        if retried {
            self.write_state(&final_state)?;
            return Ok(final_state);
        }
        self.append_event(
            &final_state,
            &transition_id,
            kind,
            json!({
                "from": state.status.as_str(),
                "to": to.as_str(),
                "detail": detail,
            }),
            Some(&final_state),
        )?;
        self.write_state(&final_state)?;
        Ok(final_state)
    }

    /// Build the next snapshot, reusing the recorded one when this transition
    /// id already exists. Returns the state and whether it was a retry.
    fn prepare_transition(
        &self,
        state: &RunState,
        to: RunStatus,
        next_version: u64,
        transition_id: &str,
    ) -> Result<(RunState, bool), RunError> {
        let existing = self.read_events(&state.run_id)?;
        let retried = existing
            .iter()
            .rev()
            .find(|event| event.transition_id == transition_id && event.kind != "run.usage");
        let final_state = match retried {
            Some(event) => event.checkpoint.clone().unwrap_or_else(|| {
                advance_state(
                    state,
                    to,
                    next_version,
                    existing.last().map_or(state.last_seq, |event| event.seq),
                )
            }),
            None => advance_state(state, to, next_version, state.last_seq.saturating_add(1)),
        };
        if final_state.schema_version != RUN_SCHEMA_VERSION {
            return Err(RunError::UnsupportedSchema(final_state.schema_version));
        }
        Ok((final_state, retried.is_some()))
    }

    /// Add usage to one run without changing its status.
    ///
    /// Budget accounting is a separate write from a status transition so a
    /// model call can reserve before it starts and settle afterwards. Each
    /// write takes the next `version`, which keeps `transition_id` unique
    /// across status and usage events. Repeating a write with the same version
    /// rewrites the snapshot instead of appending twice.
    pub fn record_budget(&self, run_id: &str, delta: RunBudget) -> Result<RunState, RunError> {
        let _lock = self.lock()?;
        let state = self.load(run_id)?;
        if state.status.is_terminal() {
            return Err(RunError::Finished(run_id.to_owned()));
        }

        let next_version = state.version + 1;
        let transition_id = format!("{run_id}-v{next_version}");
        let existing = self.read_events(run_id)?;
        if let Some(event) = existing
            .iter()
            .rev()
            .find(|event| event.transition_id == transition_id)
        {
            let recovered = event
                .checkpoint
                .clone()
                .unwrap_or_else(|| apply_budget_delta(&state, &delta, next_version));
            if recovered.schema_version != RUN_SCHEMA_VERSION {
                return Err(RunError::UnsupportedSchema(recovered.schema_version));
            }
            self.write_state(&recovered)?;
            return Ok(recovered);
        }

        let final_state = apply_budget_delta(&state, &delta, next_version);
        self.append_event(
            &final_state,
            &transition_id,
            "run.usage",
            json!({ "delta": delta }),
            Some(&final_state),
        )?;
        self.write_state(&final_state)?;
        Ok(final_state)
    }

    fn lock(&self) -> Result<RunLock, RunError> {
        fs::create_dir_all(&self.root)?;
        let path = self.root.join(".lock");
        match OpenOptions::new().write(true).create_new(true).open(&path) {
            Ok(mut file) => {
                let _ = writeln!(file, "pid={}", std::process::id());
                Ok(RunLock { path })
            }
            Err(error) if error.kind() == io::ErrorKind::AlreadyExists => Err(RunError::Locked),
            Err(error) => Err(RunError::Io(error)),
        }
    }

    fn run_dir(&self, run_id: &str) -> PathBuf {
        self.root.join(run_id)
    }

    fn state_path(&self, run_id: &str) -> PathBuf {
        self.run_dir(run_id).join("state.json")
    }

    fn events_path(&self, run_id: &str) -> PathBuf {
        self.run_dir(run_id).join("events.jsonl")
    }

    fn write_state(&self, state: &RunState) -> Result<(), RunError> {
        let directory = self.run_dir(&state.run_id);
        fs::create_dir_all(&directory)?;
        let bytes =
            serde_json::to_vec(state).map_err(|error| RunError::Corrupt(error.to_string()))?;
        write_atomic(&self.state_path(&state.run_id), &bytes)?;
        Ok(())
    }

    fn append_event(
        &self,
        state: &RunState,
        transition_id: &str,
        kind: &str,
        payload: Value,
        checkpoint: Option<&RunState>,
    ) -> Result<(), RunError> {
        let directory = self.run_dir(&state.run_id);
        fs::create_dir_all(&directory)?;
        let seq = state.last_seq;
        let event = RunEvent {
            event_id: format!("{}-e{seq}", state.run_id),
            transition_id: transition_id.to_owned(),
            seq,
            at: now_ms(),
            status: state.status,
            kind: kind.to_owned(),
            payload,
            checkpoint: checkpoint.cloned(),
        };
        let mut line =
            serde_json::to_vec(&event).map_err(|error| RunError::Corrupt(error.to_string()))?;
        line.push(b'\n');
        let mut file = OpenOptions::new()
            .create(true)
            .append(true)
            .open(self.events_path(&state.run_id))?;
        file.write_all(&line)?;
        file.sync_all()?;
        Ok(())
    }
}

fn event_kind(status: RunStatus) -> &'static str {
    match status {
        RunStatus::Completed => "run.completed",
        RunStatus::Failed => "run.failed",
        RunStatus::Cancelled => "run.cancelled",
        RunStatus::TimedOut => "run.timed_out",
        _ => "run.transitioned",
    }
}

fn advance_state(state: &RunState, to: RunStatus, version: u64, last_seq: u64) -> RunState {
    let now = now_ms();
    let mut next = state.clone();
    next.status = to;
    next.version = version;
    next.last_seq = last_seq;
    next.updated_at = now;
    if to.is_terminal() {
        next.finished_at = Some(now);
        next.pending_tool = None;
    }
    next
}

fn side_event_state(state: &RunState, version: u64) -> RunState {
    let mut next = state.clone();
    next.version = version;
    next.last_seq = state.last_seq.saturating_add(1);
    next.updated_at = now_ms();
    next
}

fn model_call_payload(call: RunModelCall) -> Value {
    let response_ref = format!("model-calls/{}/response.txt", call.turn_id);
    json!({
        "turn_id": call.turn_id,
        "model": call.model,
        "prompt_hash": call.prompt_hash,
        "response_ref": response_ref,
        "input_tokens": call.input_tokens,
        "output_tokens": call.output_tokens,
        "cost": call.cost,
        "cache_hit": call.cache_hit,
        "redactions": call.redactions,
    })
}

fn apply_budget_delta(state: &RunState, delta: &RunBudget, version: u64) -> RunState {
    let mut next = state.clone();
    next.budget.model_calls = next.budget.model_calls.saturating_add(delta.model_calls);
    next.budget.input_tokens = next.budget.input_tokens.saturating_add(delta.input_tokens);
    next.budget.output_tokens = next
        .budget
        .output_tokens
        .saturating_add(delta.output_tokens);
    next.budget.cost_micros = next.budget.cost_micros.saturating_add(delta.cost_micros);
    next.budget.reserved_micros = next
        .budget
        .reserved_micros
        .saturating_add(delta.reserved_micros);
    next.version = version;
    next.last_seq = state.last_seq.saturating_add(1);
    next.updated_at = now_ms();
    next
}

fn validate_id(run_id: &str) -> Result<(), RunError> {
    let valid = !run_id.is_empty()
        && !run_id.contains("..")
        && run_id
            .chars()
            .all(|ch| ch.is_ascii_alphanumeric() || matches!(ch, '-' | '_' | '.'));
    if valid {
        Ok(())
    } else {
        Err(RunError::InvalidId(run_id.to_owned()))
    }
}

fn next_run_id(now: u64) -> String {
    static COUNTER: AtomicU64 = AtomicU64::new(0);
    format!("run-{now}-{}", COUNTER.fetch_add(1, Ordering::Relaxed))
}

fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_millis() as u64)
        .unwrap_or_default()
}

fn write_atomic(path: &Path, bytes: &[u8]) -> io::Result<()> {
    let parent = path
        .parent()
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "path has no parent"))?;
    fs::create_dir_all(parent)?;
    let file_name = path
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or("run");
    let temporary = parent.join(format!(
        ".{file_name}.tmp-{}-{}",
        std::process::id(),
        now_ms()
    ));
    {
        let mut file = fs::File::create(&temporary)?;
        file.write_all(bytes)?;
        file.sync_all()?;
    }
    fs::rename(&temporary, path)?;
    Ok(())
}

/// Durable run errors.
#[derive(Debug, Error)]
pub enum RunError {
    #[error("run journal io error: {0}")]
    Io(#[from] io::Error),
    #[error("run {0} not found")]
    NotFound(String),
    #[error("invalid run id: {0}")]
    InvalidId(String),
    #[error("run journal is corrupt: {0}")]
    Corrupt(String),
    #[error("run schema version {0} is not supported")]
    UnsupportedSchema(u32),
    #[error("another run write is in progress; remove the run journal .lock if it is stale")]
    Locked,
    #[error("cannot transition a run from {from} to {to}")]
    InvalidTransition { from: RunStatus, to: RunStatus },
    #[error("run {0} has already finished")]
    Finished(String),
}

#[cfg(test)]
mod tests {
    use std::time::{SystemTime, UNIX_EPOCH};

    use super::*;

    fn temp_root(label: &str) -> PathBuf {
        let path = std::env::temp_dir().join(format!(
            "slowcode-runtime-{label}-{}-{}",
            std::process::id(),
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        fs::create_dir_all(&path).unwrap();
        path
    }

    fn link() -> RunLink {
        RunLink {
            workflow_id: Some("plan-1".to_owned()),
            operation: "plan".to_owned(),
        }
    }

    #[test]
    fn start_writes_state_and_created_checkpoint() {
        let root = temp_root("start");
        let store = RunStore::new(&root);
        let run = store.start(link(), 8).unwrap();
        assert_eq!(run.status, RunStatus::Queued);
        assert_eq!(run.workflow_id.as_deref(), Some("plan-1"));
        assert_eq!(run.operation, "plan");
        assert_eq!(store.load(&run.run_id).unwrap(), run);

        let events = store.read_events(&run.run_id).unwrap();
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].kind, "run.created");
        assert_eq!(events[0].transition_id, format!("{}-v1", run.run_id));
        assert!(events[0].checkpoint.is_some());
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn transitions_append_one_checkpointed_event() {
        let root = temp_root("transition");
        let store = RunStore::new(&root);
        let run = store.start(link(), 8).unwrap();
        let compiling = store
            .transition(&run.run_id, RunStatus::CompilingContext, json!({}))
            .unwrap();
        assert_eq!(compiling.status, RunStatus::CompilingContext);
        assert_eq!(compiling.version, 2);
        assert_eq!(compiling.last_seq, 2);
        assert!(compiling.finished_at.is_none());

        let calling = store
            .transition(&run.run_id, RunStatus::CallingModel, json!({}))
            .unwrap();
        let completed = store
            .transition(&run.run_id, RunStatus::Completed, json!({}))
            .unwrap();
        assert_eq!(calling.version, 3);
        assert_eq!(completed.status, RunStatus::Completed);
        assert!(completed.finished_at.is_some());

        let events = store.read_events(&run.run_id).unwrap();
        assert_eq!(events.len(), 4);
        assert_eq!(events.last().unwrap().kind, "run.completed");
        assert!(events.last().unwrap().checkpoint.is_some());
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn skipped_and_terminal_transitions_are_rejected() {
        let root = temp_root("invalid");
        let store = RunStore::new(&root);
        let run = store.start(link(), 8).unwrap();
        let error = store
            .transition(&run.run_id, RunStatus::Completed, json!({}))
            .unwrap_err();
        assert!(matches!(
            error,
            RunError::InvalidTransition {
                from: RunStatus::Queued,
                to: RunStatus::Completed
            }
        ));

        store
            .transition(&run.run_id, RunStatus::Cancelled, json!({}))
            .unwrap();
        let error = store.cancel(&run.run_id, "again").unwrap_err();
        assert!(matches!(
            error,
            RunError::InvalidTransition {
                from: RunStatus::Cancelled,
                ..
            }
        ));
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn load_recovers_from_checkpoint_when_state_json_is_missing() {
        let root = temp_root("recover");
        let store = RunStore::new(&root);
        let run = store.start(link(), 8).unwrap();
        store
            .transition(&run.run_id, RunStatus::CompilingContext, json!({}))
            .unwrap();
        fs::remove_file(store.state_path(&run.run_id)).unwrap();

        let recovered = store.load(&run.run_id).unwrap();
        assert_eq!(recovered.status, RunStatus::CompilingContext);
        assert_eq!(recovered.version, 2);
        assert_eq!(recovered.last_seq, 2);
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn half_written_transition_recovers_without_duplicate_events() {
        let root = temp_root("dedup");
        let store = RunStore::new(&root);
        let run = store.start(link(), 8).unwrap();

        // Simulate a crash after the event append but before the snapshot
        // rename: the event exists, `state.json` still has version 1.
        let mut final_state = run.clone();
        final_state.status = RunStatus::CompilingContext;
        final_state.version = 2;
        final_state.last_seq = 2;
        store
            .append_event(
                &final_state,
                &format!("{}-v2", run.run_id),
                "run.transitioned",
                json!({}),
                Some(&final_state),
            )
            .unwrap();

        // Loading already sees the checkpoint. Retrying the same transition is
        // therefore rejected instead of applying it twice.
        let recovered = store.load(&run.run_id).unwrap();
        assert_eq!(recovered.status, RunStatus::CompilingContext);
        assert_eq!(recovered.version, 2);
        let error = store
            .transition(&run.run_id, RunStatus::CompilingContext, json!({}))
            .unwrap_err();
        assert!(matches!(
            error,
            RunError::InvalidTransition {
                from: RunStatus::CompilingContext,
                to: RunStatus::CompilingContext
            }
        ));
        assert_eq!(store.read_events(&run.run_id).unwrap().len(), 2);
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn a_transition_event_without_checkpoint_is_not_appended_twice() {
        let root = temp_root("dedup-no-checkpoint");
        let store = RunStore::new(&root);
        let run = store.start(link(), 8).unwrap();

        let mut final_state = run.clone();
        final_state.status = RunStatus::CompilingContext;
        final_state.version = 2;
        final_state.last_seq = 2;
        store
            .append_event(
                &final_state,
                &format!("{}-v2", run.run_id),
                "run.transitioned",
                json!({}),
                None,
            )
            .unwrap();

        // Without a checkpoint the snapshot still reads version 1, so the
        // transition id matches the already appended event.
        let recovered = store
            .transition(&run.run_id, RunStatus::CompilingContext, json!({}))
            .unwrap();
        assert_eq!(recovered.status, RunStatus::CompilingContext);
        assert_eq!(recovered.version, 2);
        assert_eq!(store.read_events(&run.run_id).unwrap().len(), 2);
        assert_eq!(store.load(&run.run_id).unwrap(), recovered);
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn torn_tail_and_duplicate_sequences_stop_the_scan() {
        let root = temp_root("torn");
        let store = RunStore::new(&root);
        let run = store.start(link(), 8).unwrap();
        let path = store.events_path(&run.run_id);
        let mut file = OpenOptions::new().append(true).open(&path).unwrap();
        file.write_all(b"{\"event_id\":\"cut").unwrap();
        drop(file);
        assert_eq!(store.read_events(&run.run_id).unwrap().len(), 1);

        let mut file = OpenOptions::new().append(true).open(&path).unwrap();
        file.write_all(b"\n").unwrap();
        drop(file);
        assert_eq!(store.read_events(&run.run_id).unwrap().len(), 1);
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn a_live_lock_blocks_other_writers() {
        let root = temp_root("locked");
        let store = RunStore::new(&root);
        let _lock = store.lock().unwrap();
        let error = store.start(link(), 8).unwrap_err();
        assert!(matches!(error, RunError::Locked));
        drop(_lock);
        assert!(store.start(link(), 8).is_ok());
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn invalid_ids_are_rejected_before_touching_disk() {
        let store = RunStore::new(temp_root("ids"));
        assert!(matches!(
            store.load("../escape"),
            Err(RunError::InvalidId(_))
        ));
        assert!(matches!(
            store.read_events("bad/id"),
            Err(RunError::InvalidId(_))
        ));
    }

    #[test]
    fn budget_writes_accumulate_without_changing_status() {
        let root = temp_root("budget");
        let store = RunStore::new(&root);
        let run = store.start(link(), 8).unwrap();
        store
            .transition(&run.run_id, RunStatus::CompilingContext, json!({}))
            .unwrap();

        let reserved = store
            .record_budget(
                &run.run_id,
                RunBudget {
                    model_calls: 1,
                    ..RunBudget::default()
                },
            )
            .unwrap();
        assert_eq!(reserved.status, RunStatus::CompilingContext);
        assert_eq!(reserved.budget.model_calls, 1);

        let settled = store
            .record_budget(
                &run.run_id,
                RunBudget {
                    output_tokens: 30,
                    ..RunBudget::default()
                },
            )
            .unwrap();
        assert_eq!(settled.status, RunStatus::CompilingContext);
        assert_eq!(settled.budget.model_calls, 1);
        assert_eq!(settled.budget.output_tokens, 30);
        assert_eq!(settled.version, run.version + 3);

        let events = store.read_events(&run.run_id).unwrap();
        let usage: Vec<&RunEvent> = events
            .iter()
            .filter(|event| event.kind == "run.usage")
            .collect();
        assert_eq!(usage.len(), 2);
        assert!(usage.iter().all(|event| event.checkpoint.is_some()));
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn budget_snapshot_recovers_from_its_checkpoint() {
        let root = temp_root("budget-recover");
        let store = RunStore::new(&root);
        let run = store.start(link(), 8).unwrap();
        store
            .record_budget(
                &run.run_id,
                RunBudget {
                    model_calls: 2,
                    output_tokens: 7,
                    ..RunBudget::default()
                },
            )
            .unwrap();
        fs::remove_file(store.state_path(&run.run_id)).unwrap();

        let recovered = store.load(&run.run_id).unwrap();
        assert_eq!(recovered.budget.model_calls, 2);
        assert_eq!(recovered.budget.output_tokens, 7);
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn finished_runs_reject_budget_writes() {
        let root = temp_root("budget-finished");
        let store = RunStore::new(&root);
        let run = store.start(link(), 8).unwrap();
        store
            .transition(&run.run_id, RunStatus::CompilingContext, json!({}))
            .unwrap();
        store
            .transition(&run.run_id, RunStatus::Completed, json!({}))
            .unwrap();
        let error = store
            .record_budget(
                &run.run_id,
                RunBudget {
                    model_calls: 1,
                    ..RunBudget::default()
                },
            )
            .unwrap_err();
        assert!(matches!(error, RunError::Finished(id) if id == run.run_id));
        let _ = fs::remove_dir_all(&root);
    }

    fn pending_tool() -> PendingTool {
        PendingTool {
            call_id: "call-1".to_owned(),
            name: "write".to_owned(),
            arguments_hash: "hash-1".to_owned(),
            requested_at: 1_700_000_000_000,
        }
    }

    #[test]
    fn a_pending_tool_is_recorded_and_resolved_in_one_transition() {
        let root = temp_root("pending");
        let store = RunStore::new(&root);
        let run = store.start(link(), 8).unwrap();
        store
            .transition(&run.run_id, RunStatus::CompilingContext, json!({}))
            .unwrap();

        let awaiting = store.set_pending_tool(&run.run_id, pending_tool()).unwrap();
        assert_eq!(awaiting.status, RunStatus::AwaitingApproval);
        assert_eq!(awaiting.pending_tool.as_ref(), Some(&pending_tool()));
        let events = store.read_events(&run.run_id).unwrap();
        assert_eq!(events.last().unwrap().kind, "run.approval_requested");

        let executing = store
            .resolve_pending_tool(&run.run_id, ApprovalResolution::Granted)
            .unwrap();
        assert_eq!(executing.status, RunStatus::ExecutingTool);
        assert!(executing.pending_tool.is_none());
        let events = store.read_events(&run.run_id).unwrap();
        assert_eq!(events.last().unwrap().kind, "run.approval_resolved");
        assert_eq!(
            events.last().unwrap().payload["detail"]["resolution"],
            "granted"
        );
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn denying_a_pending_tool_returns_the_run_to_compiling_context() {
        let root = temp_root("pending-denied");
        let store = RunStore::new(&root);
        let run = store.start(link(), 8).unwrap();
        store
            .transition(&run.run_id, RunStatus::CompilingContext, json!({}))
            .unwrap();
        store.set_pending_tool(&run.run_id, pending_tool()).unwrap();

        let compiling = store
            .resolve_pending_tool(&run.run_id, ApprovalResolution::Denied)
            .unwrap();
        assert_eq!(compiling.status, RunStatus::CompilingContext);
        assert!(compiling.pending_tool.is_none());
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn a_second_approval_can_follow_an_approved_tool() {
        let root = temp_root("pending-second");
        let store = RunStore::new(&root);
        let run = store.start(link(), 8).unwrap();
        store
            .transition(&run.run_id, RunStatus::CompilingContext, json!({}))
            .unwrap();
        store.set_pending_tool(&run.run_id, pending_tool()).unwrap();
        store
            .resolve_pending_tool(&run.run_id, ApprovalResolution::Granted)
            .unwrap();

        let awaiting = store.set_pending_tool(&run.run_id, pending_tool()).unwrap();
        assert_eq!(awaiting.status, RunStatus::AwaitingApproval);
        assert_eq!(awaiting.pending_tool.as_ref(), Some(&pending_tool()));
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn a_pending_tool_survives_state_loss_and_is_never_auto_resolved() {
        let root = temp_root("pending-recovery");
        let store = RunStore::new(&root);
        let run = store.start(link(), 8).unwrap();
        store
            .transition(&run.run_id, RunStatus::CompilingContext, json!({}))
            .unwrap();
        store.set_pending_tool(&run.run_id, pending_tool()).unwrap();
        fs::remove_file(store.state_path(&run.run_id)).unwrap();

        let recovered = RunStore::new(&root).load(&run.run_id).unwrap();
        assert_eq!(recovered.status, RunStatus::AwaitingApproval);
        assert_eq!(recovered.pending_tool.as_ref(), Some(&pending_tool()));
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn cancelling_clears_a_pending_tool() {
        let root = temp_root("pending-cancel");
        let store = RunStore::new(&root);
        let run = store.start(link(), 8).unwrap();
        store
            .transition(&run.run_id, RunStatus::CompilingContext, json!({}))
            .unwrap();
        store.set_pending_tool(&run.run_id, pending_tool()).unwrap();

        let cancelled = store.cancel(&run.run_id, "user").unwrap();
        assert_eq!(cancelled.status, RunStatus::Cancelled);
        assert!(cancelled.pending_tool.is_none());
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn resolving_a_tool_without_a_pending_request_is_rejected() {
        let root = temp_root("pending-invalid");
        let store = RunStore::new(&root);
        let run = store.start(link(), 8).unwrap();
        store
            .transition(&run.run_id, RunStatus::CompilingContext, json!({}))
            .unwrap();

        let error = store
            .resolve_pending_tool(&run.run_id, ApprovalResolution::Granted)
            .unwrap_err();
        assert!(matches!(
            error,
            RunError::InvalidTransition {
                from: RunStatus::CompilingContext,
                to: RunStatus::ExecutingTool
            }
        ));
        let _ = fs::remove_dir_all(&root);
    }

    fn model_call() -> RunModelCall {
        RunModelCall {
            turn_id: "run-turn-1".to_owned(),
            model: "deepseek-flash".to_owned(),
            prompt_hash: "prompt-hash".to_owned(),
            input_tokens: 0,
            output_tokens: 12,
            cache_hit: None,
            cost: None,
            redactions: 1,
        }
    }

    #[test]
    fn a_model_call_archives_under_the_run_without_changing_status() {
        let root = temp_root("model-call");
        let store = RunStore::new(&root);
        let run = store.start(link(), 8).unwrap();
        store
            .transition(&run.run_id, RunStatus::CompilingContext, json!({}))
            .unwrap();

        let archived = store
            .archive_model_call(&run.run_id, model_call(), "request text", "response text")
            .unwrap();
        assert_eq!(archived.status, RunStatus::CompilingContext);

        let directory = root
            .join(&run.run_id)
            .join("model-calls")
            .join("run-turn-1");
        assert_eq!(
            fs::read_to_string(directory.join("request.txt")).unwrap(),
            "request text"
        );
        assert_eq!(
            fs::read_to_string(directory.join("response.txt")).unwrap(),
            "response text"
        );

        let events = store.read_events(&run.run_id).unwrap();
        let event = events.last().unwrap();
        assert_eq!(event.kind, "run.model_call");
        assert_eq!(event.payload["turn_id"], "run-turn-1");
        assert_eq!(
            event.payload["response_ref"],
            "model-calls/run-turn-1/response.txt"
        );
        assert_eq!(event.payload["output_tokens"], 12);
        assert_eq!(event.payload["redactions"], 1);
        assert!(event.checkpoint.is_some());
        assert_eq!(store.load(&run.run_id).unwrap(), archived);
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn a_model_call_archive_rejects_an_escaping_turn_id() {
        let root = temp_root("model-call-id");
        let store = RunStore::new(&root);
        let run = store.start(link(), 8).unwrap();
        store
            .transition(&run.run_id, RunStatus::CompilingContext, json!({}))
            .unwrap();
        let mut call = model_call();
        call.turn_id = "../escape".to_owned();

        let error = store
            .archive_model_call(&run.run_id, call, "request", "response")
            .unwrap_err();
        assert!(matches!(error, RunError::InvalidId(id) if id == "../escape"));
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn a_finished_run_rejects_a_model_call_archive() {
        let root = temp_root("model-call-finished");
        let store = RunStore::new(&root);
        let run = store.start(link(), 8).unwrap();
        store
            .transition(&run.run_id, RunStatus::CompilingContext, json!({}))
            .unwrap();
        store
            .transition(&run.run_id, RunStatus::Completed, json!({}))
            .unwrap();

        let error = store
            .archive_model_call(&run.run_id, model_call(), "request", "response")
            .unwrap_err();
        assert!(matches!(error, RunError::Finished(id) if id == run.run_id));
        let _ = fs::remove_dir_all(&root);
    }
}
