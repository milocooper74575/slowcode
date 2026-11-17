//! Durable Plan workflow journal.
//!
//! Layout under the workspace:
//!
//! ```text
//! .scratch/workflows/.lock
//! .scratch/workflows/index.json
//! .scratch/workflows/<workflow_id>/state.json
//! .scratch/workflows/<workflow_id>/events.jsonl
//! ```
//!
//! `state.json` is an atomic snapshot. `events.jsonl` is append-only and is the
//! audit trail. Both live outside the session JSONL.

use std::{
    fs::{self, File, OpenOptions},
    io::{self, Write},
    path::{Path, PathBuf},
    sync::atomic::{AtomicU64, Ordering},
    time::{SystemTime, UNIX_EPOCH},
};

use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use thiserror::Error;

use super::state::{
    AnswerSnapshot, AnswerSource, PendingApproval, Phase, Question, QuestionKind, QuestionStatus,
    Task, TestRunRecord, TestStatus, WorkflowState, WorkflowStatus, changed_task_ids,
};
use super::{
    approval::{
        APPROVAL_POLICY_VERSION, ApprovalDecision, ApprovalRecord, ApprovalRequest, ApprovalStatus,
        ModelCall,
    },
    input::redact_secrets,
};

const INDEX_SCHEMA_VERSION: u32 = 1;

/// One row in the workspace workflow index.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct WorkflowSummary {
    pub workflow_id: String,
    pub task: String,
    pub status: WorkflowStatus,
    pub phase: Phase,
    pub created_at: u64,
    pub updated_at: u64,
}

/// Result of listing workflows.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct WorkflowListing {
    pub active: Option<String>,
    pub workflows: Vec<WorkflowSummary>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
struct WorkflowIndex {
    schema_version: u32,
    #[serde(default)]
    active: Option<String>,
    #[serde(default)]
    workflows: Vec<WorkflowSummary>,
}

impl Default for WorkflowIndex {
    fn default() -> Self {
        Self {
            schema_version: INDEX_SCHEMA_VERSION,
            active: None,
            workflows: Vec::new(),
        }
    }
}

/// One journaled event.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct WorkflowEvent {
    pub event_id: String,
    pub transition_id: String,
    pub seq: u64,
    pub at: u64,
    pub phase: Phase,
    pub kind: String,
    #[serde(default)]
    pub payload: Value,
    /// Post-transition snapshot. The newest one can rebuild `state.json` after
    /// a crash between the event append and the atomic snapshot rename.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub checkpoint: Option<WorkflowState>,
}

/// One answer attempt, sufficient or insufficient.
#[derive(Clone, Debug)]
pub struct AnswerUpdate {
    pub question_id: Option<String>,
    pub answer: String,
    pub source: AnswerSource,
    pub snapshot: Option<AnswerSnapshot>,
    pub tasks: Option<Vec<Task>>,
    pub insufficient: Option<String>,
    pub follow_up: Option<Question>,
    pub next_phase: Option<Phase>,
}

impl AnswerUpdate {
    #[must_use]
    pub fn new(answer: String, source: AnswerSource) -> Self {
        Self {
            question_id: None,
            answer,
            source,
            snapshot: None,
            tasks: None,
            insufficient: None,
            follow_up: None,
            next_phase: None,
        }
    }
}

/// One `/test` result plus budget deltas.
#[derive(Clone, Debug)]
pub struct TestRunUpdate {
    pub record: TestRunRecord,
    pub model_calls: u64,
    pub tokens: u64,
    pub active_seconds: u64,
}

impl TestRunUpdate {
    #[must_use]
    pub fn new(record: TestRunRecord) -> Self {
        Self {
            record,
            model_calls: 0,
            tokens: 0,
            active_seconds: 0,
        }
    }
}

/// How the user closed the Distill follow-up.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum DistillDecision {
    /// The answer resolves the follow-up; the workflow is complete.
    Answered,
    /// The follow-up exposed a new unknown domain; return to Reclassify.
    Reclassify,
    /// The user explicitly accepts the remaining gaps.
    AcceptGaps,
}

/// Input for closing the Distill follow-up.
#[derive(Clone, Debug)]
pub struct DistillAnswerUpdate {
    pub question_id: Option<String>,
    pub answer: String,
    pub source: AnswerSource,
    pub snapshot: Option<AnswerSnapshot>,
    pub decision: DistillDecision,
    pub follow_up: Option<Question>,
}

impl DistillAnswerUpdate {
    #[must_use]
    pub fn new(decision: DistillDecision, answer: String, source: AnswerSource) -> Self {
        Self {
            question_id: None,
            answer,
            source,
            snapshot: None,
            decision,
            follow_up: None,
        }
    }
}

/// One validated row of the `/fix` failure-classification table.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct ClassifiedFailure {
    pub failure: String,
    pub kind: String,
    pub evidence: String,
}

/// One failure the driver judged to be a new unknown domain.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct UnknownGap {
    pub failure: String,
    pub evidence: String,
}

/// Durable journal rooted at `<workspace>/.scratch/workflows`.
#[derive(Clone, Debug)]
pub struct WorkflowStore {
    root: PathBuf,
}

/// Exclusive write lock. Dropping it removes the lock file.
pub struct WorkflowLock {
    path: PathBuf,
}

impl Drop for WorkflowLock {
    fn drop(&mut self) {
        let _ = fs::remove_file(&self.path);
    }
}

impl WorkflowStore {
    #[must_use]
    pub fn new(workspace: &Path) -> Self {
        Self {
            root: workspace.join(".scratch").join("workflows"),
        }
    }

    #[must_use]
    pub fn root(&self) -> &Path {
        &self.root
    }

    /// Return the active workflow id, if any.
    pub fn active_id(&self) -> Result<Option<String>, WorkflowError> {
        Ok(self.read_index()?.active)
    }

    /// Refuse to start `/plan` while another workflow is active.
    pub fn ensure_no_active(&self) -> Result<(), WorkflowError> {
        if let Some(id) = self.read_index()?.active {
            return Err(WorkflowError::ActiveExists(id));
        }
        Ok(())
    }

    /// Create an active workflow and classify its task table in one commit.
    pub fn create_with_tasks(
        &self,
        task: &str,
        session_id: Option<&str>,
        tasks: Vec<Task>,
    ) -> Result<WorkflowState, WorkflowError> {
        self.create_with_questions(task, session_id, tasks, Vec::new())
    }

    /// Create an active workflow with its first-round questions.
    pub fn create_with_questions(
        &self,
        task: &str,
        session_id: Option<&str>,
        tasks: Vec<Task>,
        questions: Vec<super::state::Question>,
    ) -> Result<WorkflowState, WorkflowError> {
        let _lock = self.lock()?;
        let mut index = self.read_index()?;
        if let Some(id) = &index.active {
            return Err(WorkflowError::ActiveExists(id.clone()));
        }

        let now = now_ms();
        let mut state = WorkflowState::new(
            next_workflow_id(now),
            &self.workspace_root(),
            task.to_owned(),
            session_id.map(str::to_owned),
            now,
        );
        state.tasks = tasks;
        for mut question in questions {
            question.question_id = state.next_question_id();
            if question.created_at == 0 {
                question.created_at = now;
            }
            state.questions.push(question);
        }
        state.version = 1;
        state.phase = if state.open_questions().is_empty() {
            Phase::Review
        } else {
            Phase::Interview
        };
        let transition_id = format!("{}-v1", state.workflow_id);
        let question_ids: Vec<String> = state
            .questions
            .iter()
            .map(|question| question.question_id.clone())
            .collect();
        state.last_seq = u64::try_from(2 + question_ids.len()).unwrap_or(u64::MAX);
        let checkpoint = Some(&state);
        self.append_event(
            &state,
            1,
            &transition_id,
            "workflow.created",
            json!({
                "task": state.task.clone(),
                "role": state.last_role.clone(),
                "profile": state.last_profile.clone(),
            }),
            None,
        )?;
        self.append_event(
            &state,
            2,
            &transition_id,
            "task.classified",
            json!({
                "tasks": state.tasks.len(),
                "table_hash": state.table_hash(),
            }),
            if question_ids.is_empty() {
                checkpoint
            } else {
                None
            },
        )?;
        for (index, question_id) in question_ids.iter().enumerate() {
            let seq = u64::try_from(index + 3).unwrap_or(u64::MAX);
            self.append_event(
                &state,
                seq,
                &transition_id,
                "question.asked",
                json!({ "question_id": question_id }),
                if index + 1 == question_ids.len() {
                    checkpoint
                } else {
                    None
                },
            )?;
        }
        self.write_state(&state)?;

        index.active = Some(state.workflow_id.clone());
        upsert_summary(&mut index.workflows, &state);
        self.write_index(&index)?;
        Ok(state)
    }

    /// Load the active workflow snapshot.
    pub fn load_active(&self) -> Result<Option<WorkflowState>, WorkflowError> {
        let Some(id) = self.read_index()?.active else {
            return Ok(None);
        };
        self.load(&id).map(Some)
    }

    /// Require an active workflow.
    pub fn require_active(&self) -> Result<WorkflowState, WorkflowError> {
        self.load_active()?.ok_or(WorkflowError::NoActive)
    }

    /// Load one workflow snapshot.
    pub fn load(&self, workflow_id: &str) -> Result<WorkflowState, WorkflowError> {
        validate_id(workflow_id)?;
        let events = self.read_events(workflow_id)?;
        let checkpoint = events
            .iter()
            .rev()
            .find_map(|event| event.checkpoint.clone())
            .filter(|state| state.workflow_id == workflow_id);
        let path = self.state_path(workflow_id);
        let mut state = match fs::read(&path) {
            Ok(bytes) => match serde_json::from_slice::<WorkflowState>(&bytes) {
                Ok(state) => state,
                Err(error) => checkpoint
                    .clone()
                    .ok_or_else(|| WorkflowError::Corrupt(error.to_string()))?,
            },
            Err(error) if error.kind() == io::ErrorKind::NotFound => checkpoint
                .clone()
                .ok_or_else(|| WorkflowError::NotFound(workflow_id.to_owned()))?,
            Err(error) => return Err(WorkflowError::Io(error)),
        };
        if let Some(checkpoint) = checkpoint
            && checkpoint.version > state.version
        {
            state = checkpoint;
        }
        if state.schema_version != super::state::SCHEMA_VERSION {
            return Err(WorkflowError::UnsupportedSchema(state.schema_version));
        }
        Ok(state)
    }

    /// Read the valid prefix of `events.jsonl`.
    ///
    /// A torn final line or an invalid line stops the scan and everything from
    /// that point on is ignored, matching the `/feedback` journal rule.
    pub fn read_events(&self, workflow_id: &str) -> Result<Vec<WorkflowEvent>, WorkflowError> {
        validate_id(workflow_id)?;
        let path = self.events_path(workflow_id);
        let contents = match fs::read_to_string(&path) {
            Ok(contents) => contents,
            Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(Vec::new()),
            Err(error) => return Err(WorkflowError::Io(error)),
        };
        let mut events = Vec::new();
        let mut last_seq = 0_u64;
        for line in contents.lines() {
            if line.trim().is_empty() {
                continue;
            }
            let Ok(event) = serde_json::from_str::<WorkflowEvent>(line) else {
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

    /// List the active id and every known workflow.
    pub fn list(&self) -> Result<WorkflowListing, WorkflowError> {
        let index = self.read_index()?;
        Ok(WorkflowListing {
            active: index.active,
            workflows: index.workflows,
        })
    }

    /// Answer the current question and optionally replace the task table.
    pub fn answer(
        &self,
        workflow_id: &str,
        answer: &str,
        tasks: Option<Vec<Task>>,
        source: AnswerSource,
    ) -> Result<WorkflowState, WorkflowError> {
        let mut update = AnswerUpdate::new(answer.to_owned(), source);
        update.tasks = tasks;
        self.apply_answer(workflow_id, update)
    }

    /// Apply an answer attempt, including insufficient answers and follow-ups.
    pub fn apply_answer(
        &self,
        workflow_id: &str,
        update: AnswerUpdate,
    ) -> Result<WorkflowState, WorkflowError> {
        let _lock = self.lock()?;
        let mut state = self.load(workflow_id)?;
        if state.status != WorkflowStatus::Active {
            return Err(super::state::GateError::NotActive(state.status).into());
        }
        if !matches!(
            state.phase,
            Phase::Interview | Phase::Review | Phase::Reclassify
        ) {
            return Err(super::state::GateError::WrongPhase {
                phase: state.phase,
                action: "/answer",
            }
            .into());
        }

        let question_id = update.question_id.or_else(|| {
            if update.insufficient.is_none() {
                state
                    .open_questions()
                    .first()
                    .map(|question| question.question_id.clone())
            } else {
                None
            }
        });

        if let Some(gap) = update.insufficient {
            let question_id = question_id.ok_or_else(|| {
                WorkflowError::Corrupt("insufficient answer has no question id".to_owned())
            })?;
            let attempts = state
                .record_insufficient(&question_id, gap.clone())
                .ok_or_else(|| WorkflowError::NotFound(format!("question {question_id}")))?;
            self.commit(
                &mut state,
                vec![(
                    "question.insufficient",
                    json!({ "question_id": question_id, "gap": gap, "attempts": attempts }),
                )],
            )?;
            return Ok(state);
        }

        let mut events: Vec<(&str, Value)> = Vec::new();
        if let Some(tasks) = update.tasks {
            let changed = changed_task_ids(&state.tasks, &tasks);
            state.tasks = tasks;
            if !changed.is_empty() {
                state.mark_stale_rows(changed.clone());
                events.push((
                    "task.reclassified",
                    json!({ "changed": changed, "table_hash": state.table_hash() }),
                ));
            }
        }
        if let Some(question_id) = question_id {
            if state
                .answer_question(&question_id, update.answer, update.source)
                .is_none()
            {
                return Err(WorkflowError::NotFound(format!(
                    "open question {question_id}"
                )));
            }
            if let Some(question) = state
                .questions
                .iter_mut()
                .find(|question| question.question_id == question_id)
            {
                question.answer_snapshot = update.snapshot.clone();
            }
            events.push((
                "question.answered",
                json!({
                    "question_id": question_id,
                    "source": update.source,
                    "snapshot": update.snapshot,
                }),
            ));
        }
        if let Some(mut follow_up) = update.follow_up {
            if follow_up.created_at == 0 {
                follow_up.created_at = now_ms();
            }
            let question_id = state.append_question(follow_up);
            events.push((
                "question.asked",
                json!({ "question_id": question_id, "follow_up": true }),
            ));
        }
        if let Some(phase) = update.next_phase {
            state.phase = phase;
            events.push(("phase.advanced", json!({ "phase": phase })));
        }
        if events.is_empty() {
            return Ok(state);
        }
        self.commit(&mut state, events)?;
        Ok(state)
    }

    /// Approve the current classification and advance the phase.
    ///
    /// A workflow that enters TestFix needs a test contract. Scaffold-bound
    /// workflows build theirs later, so they pass `None` here.
    pub fn approve(
        &self,
        workflow_id: &str,
        test_contract: Option<super::state::TestContract>,
        test_selector: Option<String>,
    ) -> Result<WorkflowState, WorkflowError> {
        let _lock = self.lock()?;
        let mut state = self.load(workflow_id)?;
        state.approve()?;
        state.test_selector = test_selector;
        if state.phase == Phase::TestFix {
            let contract = test_contract.ok_or(super::state::GateError::MissingTestContract)?;
            state.test_contract = Some(contract);
        }
        let payload = json!({
            "approved_version": state.version,
            "table_hash": state.table_hash(),
            "phase": state.phase,
            "test_selector": state.test_selector,
        });
        self.commit(&mut state, vec![("classification.reviewed", payload)])?;
        Ok(state)
    }

    /// Add a user-requested reclassification question.
    pub fn reclassify(
        &self,
        workflow_id: &str,
        text: &str,
    ) -> Result<WorkflowState, WorkflowError> {
        let _lock = self.lock()?;
        let mut state = self.load(workflow_id)?;
        if state.status.is_terminal() {
            return Err(WorkflowError::Terminal(workflow_id.to_owned()));
        }
        if state.phase == Phase::Distill {
            return Err(super::state::GateError::WrongPhase {
                phase: state.phase,
                action: "/reclassify",
            }
            .into());
        }
        let from_phase = state.phase;
        state.phase = Phase::Reclassify;
        let mut question = Question {
            question_id: String::new(),
            task_id: None,
            kind: super::state::QuestionKind::Knowledge,
            prompt: format!(
                "你提出了新的未知域：{text}\n请补充这块知识、规则或约束，系统会据此重新分类。"
            ),
            options: Vec::new(),
            recommendation: None,
            reason: String::new(),
            status: super::state::QuestionStatus::Open,
            criteria: "给出缺失知识、规则或约束，并说明它约束哪些任务。".to_owned(),
            created_at: now_ms(),
            answer: None,
            answer_source: None,
            answer_snapshot: None,
            attempts: 0,
            last_gap: None,
        };
        question.question_id = state.next_question_id();
        let question_id = state.append_question(question);
        self.commit(
            &mut state,
            vec![
                (
                    "user.reclassify_requested",
                    json!({ "text": text, "from_phase": from_phase }),
                ),
                ("question.asked", json!({ "question_id": question_id })),
            ],
        )?;
        Ok(state)
    }

    /// Record a validated scaffold and wait for the user's core implementation.
    pub fn finish_scaffold(
        &self,
        workflow_id: &str,
        contract: super::state::ScaffoldContract,
        test_selector: Option<String>,
    ) -> Result<WorkflowState, WorkflowError> {
        let _lock = self.lock()?;
        let mut state = self.load(workflow_id)?;
        state.can_enter_scaffold()?;
        state.test_contract = Some(contract.test_contract());
        state.scaffold = Some(contract);
        if test_selector.is_some() {
            state.test_selector = test_selector;
        }
        let framework_files = state
            .scaffold
            .as_ref()
            .map_or(0, |contract| contract.framework_files.len());
        state.phase = Phase::AwaitUser;
        state.workspace_dirty = false;
        state.workspace_fingerprint = Some(self.workspace_fingerprint(&state));
        self.commit(
            &mut state,
            vec![(
                "scaffold.built",
                json!({
                    "contract": true,
                    "framework_files": framework_files,
                }),
            )],
        )?;
        Ok(state)
    }

    /// Record a refused scaffold. The phase stays in Scaffold so it can retry.
    pub fn record_scaffold_invalid(
        &self,
        workflow_id: &str,
        reason: &str,
    ) -> Result<WorkflowState, WorkflowError> {
        let _lock = self.lock()?;
        let mut state = self.load(workflow_id)?;
        if state.phase != Phase::Scaffold {
            return Err(super::state::GateError::WrongPhase {
                phase: state.phase,
                action: "/scaffold",
            }
            .into());
        }
        self.commit(
            &mut state,
            vec![("scaffold.invalid", json!({ "reason": reason }))],
        )?;
        Ok(state)
    }

    /// The user finished the core regions and the contract flipped green.
    pub fn complete_user_implementation(
        &self,
        workflow_id: &str,
        passed: bool,
        failure: Option<String>,
    ) -> Result<WorkflowState, WorkflowError> {
        let _lock = self.lock()?;
        let mut state = self.load(workflow_id)?;
        state.can_continue()?;
        state.test.attempts += 1;
        state.test.last_status = Some(if passed {
            super::state::TestStatus::Passed
        } else {
            super::state::TestStatus::Failed
        });
        state.test.last_failure = failure.clone();
        state.phase = Phase::TestFix;
        self.commit(
            &mut state,
            vec![
                ("user_implementation.completed", json!({ "passed": passed })),
                ("test.run", json!({ "passed": passed, "failure": failure })),
            ],
        )?;
        Ok(state)
    }

    /// Freeze the workflow in Blocked so the evidence survives.
    pub fn block(
        &self,
        workflow_id: &str,
        reason: &str,
        last_failure: Option<String>,
        resume_hint: &str,
    ) -> Result<WorkflowState, WorkflowError> {
        let _lock = self.lock()?;
        let mut state = self.load(workflow_id)?;
        if state.status.is_terminal() {
            return Err(WorkflowError::Terminal(workflow_id.to_owned()));
        }
        state.status = super::state::WorkflowStatus::Blocked;
        state.blocked = Some(super::state::BlockedState {
            reason: reason.to_owned(),
            last_failure: last_failure.clone(),
            pending_action: Some("/workflow use <id> after fixing the cause".to_owned()),
            resume_hint: Some(resume_hint.to_owned()),
        });
        self.commit(
            &mut state,
            vec![(
                "workflow.blocked",
                json!({ "reason": reason, "last_failure": last_failure, "resume_hint": resume_hint }),
            )],
        )?;
        Ok(state)
    }

    /// Record a synthetic test result. Kept for callers that only have a
    /// pass/fail bit; `/test` uses `record_test_run` with full evidence.
    pub fn record_test(
        &self,
        workflow_id: &str,
        passed: bool,
        failure: Option<String>,
    ) -> Result<WorkflowState, WorkflowError> {
        let record = TestRunRecord {
            command: "record_test".to_owned(),
            status: if passed {
                TestStatus::Passed
            } else {
                TestStatus::Failed
            },
            exit_code: None,
            duration_ms: 0,
            failures: failure.iter().cloned().collect(),
            passed: Vec::new(),
            flaky_reruns: 0,
            output_tail: failure.unwrap_or_default(),
        };
        self.record_test_run(workflow_id, TestRunUpdate::new(record), u32::MAX)
    }

    /// Record one fixed-contract test run.
    ///
    /// A pass moves to Distill. A red run counts a round, tracks repeated
    /// identical failure sets, and blocks when a limit is reached.
    pub fn record_test_run(
        &self,
        workflow_id: &str,
        update: TestRunUpdate,
        max_consecutive_same_failure: u32,
    ) -> Result<WorkflowState, WorkflowError> {
        let _lock = self.lock()?;
        let mut state = self.load(workflow_id)?;
        state.can_enter_test()?;
        state.workspace_dirty = false;
        state.workspace_fingerprint = Some(self.workspace_fingerprint(&state));
        state.test.attempts += 1;
        state.budget.model_calls += update.model_calls;
        state.budget.tokens += update.tokens;
        state.budget.enforced_tokens += update.tokens;
        state.budget.active_seconds += update.active_seconds;
        let record = update.record;
        state.test.last_status = Some(record.status);
        state.test.last_failure = failure_text(&record);
        let mut events: Vec<(&str, Value)> = Vec::new();

        if record.status == TestStatus::Passed {
            state.test.consecutive_same_failure = 0;
            state.test.last_failure_signature = None;
            state.phase = Phase::Distill;
            events.push((
                "test.run",
                json!({
                    "status": "passed",
                    "command": record.command,
                    "duration_ms": record.duration_ms,
                    "flaky_reruns": record.flaky_reruns,
                    "budget": state.budget,
                }),
            ));
            if record.flaky_reruns > 0 {
                events.push(("test.flaky", json!({ "flaky_reruns": record.flaky_reruns })));
            }
            events.push(("test.passed", json!({})));
            state.test.last_run = Some(record);
            self.commit(&mut state, events)?;
            return Ok(state);
        }

        state.test.rounds += 1;
        let signature = record.failure_signature();
        if state.test.last_failure_signature.as_deref() == Some(signature.as_str()) {
            state.test.consecutive_same_failure += 1;
        } else {
            state.test.consecutive_same_failure = 1;
            state.test.last_failure_signature = Some(signature);
        }
        let rounds = state.test.rounds;
        let consecutive = state.test.consecutive_same_failure;
        events.push((
            "test.run",
            json!({
                "status": "failed",
                "command": record.command,
                "exit_code": record.exit_code,
                "duration_ms": record.duration_ms,
                "failures": record.failures,
                "flaky_reruns": record.flaky_reruns,
                "rounds": rounds,
                "consecutive_same_failure": consecutive,
                "budget": state.budget,
            }),
        ));
        if record.flaky_reruns > 0 {
            events.push(("test.flaky", json!({ "flaky_reruns": record.flaky_reruns })));
        }
        events.push((
            "test.failed",
            json!({
                "failure": state.test.last_failure,
                "rounds": rounds,
                "consecutive_same_failure": consecutive,
            }),
        ));
        state.test.last_run = Some(record);

        if consecutive >= max_consecutive_same_failure {
            let failure = state.test.last_failure.clone();
            apply_block(
                &mut state,
                "retry_limit",
                failure.clone(),
                "the same failure set repeated; change the approach or retry with /workflow use <id>",
            );
            events.push((
                "workflow.blocked",
                json!({
                    "reason": "retry_limit",
                    "last_failure": failure,
                    "consecutive_same_failure": consecutive,
                }),
            ));
        }
        self.commit(&mut state, events)?;
        Ok(state)
    }

    /// Add one model turn's usage to the workflow budget.
    pub fn record_model_usage(
        &self,
        workflow_id: &str,
        model_calls: u64,
        tokens: u64,
    ) -> Result<WorkflowState, WorkflowError> {
        let _lock = self.lock()?;
        let mut state = self.load(workflow_id)?;
        if state.status != WorkflowStatus::Active {
            return Err(super::state::GateError::NotActive(state.status).into());
        }
        state.budget.model_calls += model_calls;
        state.budget.tokens += tokens;
        state.budget.enforced_tokens += tokens;
        let budget = state.budget.clone();
        self.commit(
            &mut state,
            vec![(
                "model.used",
                json!({
                    "model_calls": model_calls,
                    "tokens": tokens,
                    "budget": budget,
                }),
            )],
        )?;
        Ok(state)
    }

    /// Add one fix turn's active time to the workflow counters.
    pub fn record_fix_active_seconds(
        &self,
        workflow_id: &str,
        active_seconds: u64,
    ) -> Result<WorkflowState, WorkflowError> {
        let _lock = self.lock()?;
        let mut state = self.load(workflow_id)?;
        if state.status != WorkflowStatus::Active {
            return Err(super::state::GateError::NotActive(state.status).into());
        }
        state.budget.active_seconds += active_seconds;
        let budget = state.budget.clone();
        self.commit(
            &mut state,
            vec![(
                "fix.used",
                json!({
                    "active_seconds": active_seconds,
                    "budget": budget,
                }),
            )],
        )?;
        Ok(state)
    }

    /// Record the classification table from one `/fix` turn.
    pub fn record_fix_completed(
        &self,
        workflow_id: &str,
        classifications: &[ClassifiedFailure],
    ) -> Result<WorkflowState, WorkflowError> {
        let _lock = self.lock()?;
        let mut state = self.load(workflow_id)?;
        if state.status != WorkflowStatus::Active {
            return Err(super::state::GateError::NotActive(state.status).into());
        }
        state.workspace_dirty = false;
        state.workspace_fingerprint = Some(self.workspace_fingerprint(&state));
        self.commit(
            &mut state,
            vec![(
                "fix.completed",
                json!({ "classifications": classifications }),
            )],
        )?;
        Ok(state)
    }

    /// Freeze a bad contract and send the workflow back to Review.
    pub fn return_to_review(
        &self,
        workflow_id: &str,
        reason: &str,
    ) -> Result<WorkflowState, WorkflowError> {
        let _lock = self.lock()?;
        let mut state = self.load(workflow_id)?;
        if state.status != WorkflowStatus::Active {
            return Err(super::state::GateError::NotActive(state.status).into());
        }
        state.phase = Phase::Review;
        state.review.approved_table_hash = None;
        state.review.approved_version = None;
        self.commit(
            &mut state,
            vec![("contract.invalid", json!({ "reason": reason }))],
        )?;
        Ok(state)
    }

    /// Ask one question per unknown-gap failure and move to Reclassify.
    pub fn ask_unknown_gaps(
        &self,
        workflow_id: &str,
        gaps: &[UnknownGap],
    ) -> Result<WorkflowState, WorkflowError> {
        let _lock = self.lock()?;
        let mut state = self.load(workflow_id)?;
        if state.status != WorkflowStatus::Active {
            return Err(super::state::GateError::NotActive(state.status).into());
        }
        let mut events: Vec<(&str, Value)> = vec![(
            "fix.unknown_gap",
            json!({ "failures": gaps.iter().map(|gap| &gap.failure).collect::<Vec<_>>() }),
        )];
        for gap in gaps {
            let question = Question {
                question_id: String::new(),
                task_id: None,
                kind: QuestionKind::Unknown,
                prompt: format!(
                    "测试失败暴露了新的未知域：{}\n证据：{}\n请选择下一步。",
                    gap.failure, gap.evidence
                ),
                options: vec![
                    "补充知识并转已知域".to_owned(),
                    "我自己实现核心".to_owned(),
                    "继续按已知域修复".to_owned(),
                ],
                recommendation: None,
                reason: gap.evidence.clone(),
                status: QuestionStatus::Open,
                criteria: "选择固定选项之一；补充知识时给出缺失概念或约束。".to_owned(),
                created_at: now_ms(),
                answer: None,
                answer_source: None,
                answer_snapshot: None,
                attempts: 0,
                last_gap: None,
            };
            let question_id = state.append_question(question);
            events.push(("question.asked", json!({ "question_id": question_id })));
        }
        state.phase = Phase::Reclassify;
        self.commit(&mut state, events)?;
        Ok(state)
    }

    /// Ask the user which test selector `/approve` should use.
    pub fn ask_test_selector(
        &self,
        workflow_id: &str,
        options: &[String],
        reason: &str,
    ) -> Result<WorkflowState, WorkflowError> {
        let _lock = self.lock()?;
        let mut state = self.load(workflow_id)?;
        if state.status != WorkflowStatus::Active || state.phase != Phase::Review {
            return Err(super::state::GateError::WrongPhase {
                phase: state.phase,
                action: "/approve",
            }
            .into());
        }
        let question = Question {
            question_id: String::new(),
            task_id: None,
            kind: QuestionKind::TestSelector,
            prompt: format!(
                "无法唯一推导测试选择器：{reason}\n请选择 selector。直接回复选项，或用 /answer #<id> <selector>。"
            ),
            options: options.to_vec(),
            recommendation: options.first().cloned(),
            reason: reason.to_owned(),
            status: QuestionStatus::Open,
            criteria: "必须是项目测试命令接受的 package、目录或测试路径。".to_owned(),
            created_at: now_ms(),
            answer: None,
            answer_source: None,
            answer_snapshot: None,
            attempts: 0,
            last_gap: None,
        };
        let question_id = state.append_question(question);
        self.commit(
            &mut state,
            vec![
                (
                    "selector.required",
                    json!({ "reason": reason, "options": options }),
                ),
                ("question.asked", json!({ "question_id": question_id })),
            ],
        )?;
        Ok(state)
    }

    /// Answer a `TestSelector` question and return to Review.
    pub fn answer_test_selector(
        &self,
        workflow_id: &str,
        question_id: Option<&str>,
        selector: &str,
        source: AnswerSource,
    ) -> Result<WorkflowState, WorkflowError> {
        let _lock = self.lock()?;
        let mut state = self.load(workflow_id)?;
        if state.status != WorkflowStatus::Active {
            return Err(super::state::GateError::NotActive(state.status).into());
        }
        let question_id = question_id
            .map(str::to_owned)
            .or_else(|| {
                state
                    .questions
                    .iter()
                    .find(|question| {
                        question.kind == QuestionKind::TestSelector
                            && question.status == QuestionStatus::Open
                    })
                    .map(|question| question.question_id.clone())
            })
            .ok_or_else(|| WorkflowError::NotFound("open test selector question".to_owned()))?;
        let question = state
            .questions
            .iter()
            .find(|question| question.question_id == question_id)
            .cloned()
            .ok_or_else(|| WorkflowError::NotFound(format!("question {question_id}")))?;
        if question.kind != QuestionKind::TestSelector {
            return Err(WorkflowError::Corrupt(format!(
                "question {question_id} is not a test selector question"
            )));
        }
        if state
            .answer_question(&question_id, selector.to_owned(), source)
            .is_none()
        {
            return Err(WorkflowError::NotFound(format!(
                "open question {question_id}"
            )));
        }
        state.test_selector = Some(selector.to_owned());
        if state.open_questions().is_empty() {
            state.phase = Phase::Review;
        }
        self.commit(
            &mut state,
            vec![
                (
                    "question.answered",
                    json!({ "question_id": question_id, "source": source }),
                ),
                ("selector.resolved", json!({ "selector": selector })),
            ],
        )?;
        Ok(state)
    }

    /// Record a Teach-back report and append its follow-up question.
    pub fn record_distill_report(
        &self,
        workflow_id: &str,
        follow_up: Option<Question>,
    ) -> Result<WorkflowState, WorkflowError> {
        let _lock = self.lock()?;
        let mut state = self.load(workflow_id)?;
        state.record_distill_report()?;
        let mut events: Vec<(&str, Value)> = vec![("distill.reported", json!({}))];
        if let Some(mut question) = follow_up {
            if question.created_at == 0 {
                question.created_at = now_ms();
            }
            let question_id = state.push_question(question);
            events.push((
                "question.asked",
                json!({ "question_id": question_id, "kind": "distill" }),
            ));
        }
        self.commit(&mut state, events)?;
        Ok(state)
    }

    /// Record the Skill manifest used by one model command.
    ///
    /// Repeated identical manifests do not append another event.
    pub fn record_context_manifest(
        &self,
        workflow_id: &str,
        manifest: super::manifest::ContextManifest,
    ) -> Result<WorkflowState, WorkflowError> {
        let _lock = self.lock()?;
        let mut state = self.load(workflow_id)?;
        if state.status != WorkflowStatus::Active {
            return Err(super::state::GateError::NotActive(state.status).into());
        }
        if state.context_manifest.as_ref() == Some(&manifest) {
            return Ok(state);
        }
        state.context_manifest = Some(manifest.clone());
        self.commit(
            &mut state,
            vec![(
                "context.manifest",
                json!({
                    "phase": manifest.phase,
                    "profile": manifest.profile,
                    "skill_roots": manifest.skill_roots,
                    "skills": manifest.skills.iter().map(|skill| skill.name.as_str()).collect::<Vec<_>>(),
                }),
            )],
        )?;
        Ok(state)
    }

    /// Append one `shell.readonly` audit event. Inactive workflows are skipped.
    pub fn record_shell_readonly(
        &self,
        workflow_id: &str,
        payload: Value,
    ) -> Result<(), WorkflowError> {
        let _lock = self.lock()?;
        let mut state = self.load(workflow_id)?;
        if state.status != WorkflowStatus::Active {
            return Ok(());
        }
        self.commit(&mut state, vec![("shell.readonly", payload)])?;
        Ok(())
    }

    /// Record a one-run hint override without changing phase or role.
    pub fn record_capability_override(
        &self,
        workflow_id: &str,
        question: &str,
        profile: &str,
    ) -> Result<WorkflowState, WorkflowError> {
        let _lock = self.lock()?;
        let mut state = self.load(workflow_id)?;
        if state.status != WorkflowStatus::Active {
            return Err(super::state::GateError::NotActive(state.status).into());
        }
        let hash = super::contract::sha256_hex(question.as_bytes());
        let phase = state.phase;
        self.commit(
            &mut state,
            vec![(
                "capability.override",
                json!({
                    "phase": phase,
                    "profile": profile,
                    "reason": "user_request",
                    "question_sha256": hash,
                }),
            )],
        )?;
        Ok(state)
    }

    /// Answer or explicitly accept the Distill follow-up.
    ///
    /// `Answered` and `Reclassify` require an open question. `AcceptGaps` may
    /// close a report that has no follow-up and always emits an audit event.
    pub fn answer_distill_follow_up(
        &self,
        workflow_id: &str,
        update: DistillAnswerUpdate,
    ) -> Result<WorkflowState, WorkflowError> {
        let DistillAnswerUpdate {
            question_id,
            answer,
            source,
            snapshot,
            decision,
            follow_up,
        } = update;
        let _lock = self.lock()?;
        let mut state = self.load(workflow_id)?;
        if state.status != WorkflowStatus::Active {
            return Err(super::state::GateError::NotActive(state.status).into());
        }
        if state.phase != Phase::Distill {
            return Err(super::state::GateError::WrongPhase {
                phase: state.phase,
                action: "/answer",
            }
            .into());
        }
        if !state
            .distill
            .as_ref()
            .is_some_and(|distill| distill.reported)
        {
            return Err(WorkflowError::Corrupt(
                "distill follow-up answered before a report".to_owned(),
            ));
        }
        if decision == DistillDecision::Reclassify && follow_up.is_none() {
            return Err(WorkflowError::Corrupt(
                "reclassification needs a follow-up question".to_owned(),
            ));
        }

        let question_id = match question_id {
            Some(question_id) => {
                let open = state.questions.iter().any(|question| {
                    question.question_id == question_id && question.status == QuestionStatus::Open
                });
                if !open {
                    return Err(WorkflowError::NotFound(format!(
                        "open question {question_id}"
                    )));
                }
                Some(question_id)
            }
            None => state
                .open_questions()
                .first()
                .map(|question| question.question_id.clone()),
        };
        let mut events: Vec<(&str, Value)> = Vec::new();
        if let Some(question_id) = question_id {
            if state
                .answer_question(&question_id, answer.clone(), source)
                .is_some()
            {
                if let Some(question) = state
                    .questions
                    .iter_mut()
                    .find(|question| question.question_id == question_id)
                {
                    question.answer_snapshot = snapshot.clone();
                }
                events.push((
                    "question.answered",
                    json!({
                        "question_id": question_id,
                        "source": source,
                        "snapshot": snapshot,
                    }),
                ));
            } else if decision != DistillDecision::AcceptGaps {
                return Err(WorkflowError::NotFound(format!(
                    "open question {question_id}"
                )));
            }
        } else if decision != DistillDecision::AcceptGaps {
            return Err(WorkflowError::NotFound(
                "open distill follow-up question".to_owned(),
            ));
        }

        match decision {
            DistillDecision::AcceptGaps => {
                state
                    .distill
                    .get_or_insert_with(Default::default)
                    .gaps_accepted = true;
                state.phase = Phase::Done;
                state.status = WorkflowStatus::Completed;
                events.push(("distill.gaps_accepted", json!({ "answer": answer })));
                events.push(("workflow.completed", json!({ "via": "gaps_accepted" })));
            }
            DistillDecision::Reclassify => {
                state.phase = Phase::Reclassify;
                events.push((
                    "distill.followup_answered",
                    json!({ "phase": "reclassify", "next_phase": "interview" }),
                ));
                if let Some(mut question) = follow_up {
                    if question.created_at == 0 {
                        question.created_at = now_ms();
                    }
                    // The knowledge question needs an answer, so the workflow
                    // moves through Reclassify into Interview in this commit.
                    let follow_up_id = state.append_question(question);
                    events.push((
                        "question.asked",
                        json!({ "question_id": follow_up_id, "kind": "knowledge" }),
                    ));
                }
            }
            DistillDecision::Answered => {
                state.phase = Phase::Done;
                state.status = WorkflowStatus::Completed;
                events.push(("distill.followup_answered", json!({ "phase": "done" })));
                events.push(("workflow.completed", json!({ "via": "followup_answered" })));
            }
        }

        self.commit(&mut state, events)?;
        Ok(state)
    }

    /// Mark a workflow ended. Phase is preserved; the lifecycle changes.
    pub fn end(&self, workflow_id: &str) -> Result<WorkflowState, WorkflowError> {
        let _lock = self.lock()?;
        let mut state = self.load(workflow_id)?;
        state.status = WorkflowStatus::Ended;
        self.commit(&mut state, vec![("workflow.ended", json!({}))])?;
        let mut index = self.read_index()?;
        if index.active.as_deref() == Some(workflow_id) {
            index.active = None;
        }
        upsert_summary(&mut index.workflows, &state);
        self.write_index(&index)?;
        Ok(state)
    }

    /// Pause the active workflow and clear the active slot.
    ///
    /// Unlike [`Self::end`], the workflow stays resumable with
    /// `/workflow use <id>`. `/plan --new` uses this to step aside instead of
    /// destroying the old workflow.
    pub fn pause_active(&self) -> Result<Option<WorkflowState>, WorkflowError> {
        let _lock = self.lock()?;
        let mut index = self.read_index()?;
        let Some(id) = index.active.clone() else {
            return Ok(None);
        };
        let mut state = self.load(&id)?;
        if state.status != WorkflowStatus::Active {
            index.active = None;
            self.write_index(&index)?;
            return Ok(Some(state));
        }
        state.status = WorkflowStatus::Paused;
        self.commit(
            &mut state,
            vec![("workflow.paused", json!({ "reason": "plan_new" }))],
        )?;
        Ok(Some(state))
    }

    /// Pause the old active workflow and activate the chosen one.
    pub fn use_workflow(&self, workflow_id: &str) -> Result<WorkflowState, WorkflowError> {
        let _lock = self.lock()?;
        let mut index = self.read_index()?;
        let mut target = self.load(workflow_id)?;
        if target.status.is_terminal() {
            return Err(WorkflowError::Terminal(workflow_id.to_owned()));
        }
        if index.active.as_deref() == Some(workflow_id) {
            return Ok(target);
        }
        if let Some(old_id) = index.active.clone()
            && old_id != workflow_id
        {
            let mut old = self.load(&old_id)?;
            old.status = WorkflowStatus::Paused;
            self.commit(&mut old, vec![("workflow.paused", json!({}))])?;
            upsert_summary(&mut index.workflows, &old);
        }
        target.status = WorkflowStatus::Active;
        self.commit(&mut target, vec![("workflow.resumed", json!({}))])?;
        index.active = Some(workflow_id.to_owned());
        upsert_summary(&mut index.workflows, &target);
        self.write_index(&index)?;
        Ok(target)
    }

    /// Record one cancelled run. The phase never changes.
    pub fn cancel_run(
        &self,
        workflow_id: &str,
        turn_id: &str,
        reason: &str,
        workspace_dirty: bool,
    ) -> Result<WorkflowState, WorkflowError> {
        let _lock = self.lock()?;
        let mut state = self.load(workflow_id)?;
        if state.status.is_terminal() {
            return Err(WorkflowError::Terminal(workflow_id.to_owned()));
        }
        let phase = state.phase;
        state.workspace_dirty = state.workspace_dirty || workspace_dirty;
        state.workspace_fingerprint = Some(self.workspace_fingerprint(&state));
        let workspace_dirty = state.workspace_dirty;
        self.commit(
            &mut state,
            vec![(
                "run.cancelled",
                json!({
                    "phase": phase,
                    "turn_id": turn_id,
                    "reason": reason,
                    "workspace_dirty": workspace_dirty,
                }),
            )],
        )?;
        Ok(state)
    }

    /// Record one summarized model call and archive its redacted payloads.
    pub fn record_model_call(
        &self,
        workflow_id: &str,
        call: ModelCall,
        request: &str,
        response: &str,
    ) -> Result<WorkflowState, WorkflowError> {
        let _lock = self.lock()?;
        let mut state = self.load(workflow_id)?;
        if state.status != WorkflowStatus::Active {
            return Err(super::state::GateError::NotActive(state.status).into());
        }
        if !safe_component(&call.turn_id) {
            return Err(WorkflowError::Corrupt(format!(
                "invalid turn id: {}",
                call.turn_id
            )));
        }
        let workspace = self.workspace_root();
        let (request, request_redactions) = redact_secrets(request, &workspace);
        let (response, response_redactions) = redact_secrets(response, &workspace);
        let directory = self.runs_dir(workflow_id).join(&call.turn_id);
        fs::create_dir_all(&directory)?;
        fs::write(directory.join("request.txt"), &request)?;
        fs::write(directory.join("response.txt"), &response)?;
        let prompt_hash = super::contract::sha256_hex(request.as_bytes());
        let response_ref = format!("runs/{}/response.txt", call.turn_id);
        let redactions = request_redactions + response_redactions;
        self.commit_model_call(&mut state, call, prompt_hash, response_ref, redactions)?;
        Ok(state)
    }

    /// Record one model call whose payloads already live in the run archive.
    ///
    /// The workflow journal keeps the event for phase correlation, but the
    /// bytes are owned by the run. `response_ref` is relative to the workflow
    /// directory and must stay inside it.
    pub fn record_model_call_reference(
        &self,
        workflow_id: &str,
        call: ModelCall,
        prompt_hash: &str,
        response_ref: &str,
        redactions: u64,
    ) -> Result<WorkflowState, WorkflowError> {
        let _lock = self.lock()?;
        let mut state = self.load(workflow_id)?;
        if state.status != WorkflowStatus::Active {
            return Err(super::state::GateError::NotActive(state.status).into());
        }
        if response_ref.contains("..") || response_ref.starts_with('/') {
            return Err(WorkflowError::Corrupt(format!(
                "unsafe model call reference: {response_ref}"
            )));
        }
        self.commit_model_call(
            &mut state,
            call,
            prompt_hash.to_owned(),
            response_ref.to_owned(),
            redactions,
        )?;
        Ok(state)
    }

    fn commit_model_call(
        &self,
        state: &mut WorkflowState,
        call: ModelCall,
        prompt_hash: String,
        response_ref: String,
        redactions: u64,
    ) -> Result<(), WorkflowError> {
        if !safe_component(&call.turn_id) {
            return Err(WorkflowError::Corrupt(format!(
                "invalid turn id: {}",
                call.turn_id
            )));
        }
        let phase = state.phase;
        let turn_id = call.turn_id;
        let model = call.model;
        let input_tokens = call.input_tokens;
        let output_tokens = call.output_tokens;
        let cost = call.cost;
        let cache_hit = call.cache_hit;
        self.commit(
            state,
            vec![(
                "model.called",
                json!({
                    "phase": phase,
                    "turn_id": turn_id,
                    "model": model,
                    "prompt_hash": prompt_hash,
                    "response_ref": response_ref,
                    "input_tokens": input_tokens,
                    "output_tokens": output_tokens,
                    "cost": cost,
                    "cache_hit": cache_hit,
                    "redactions": redactions,
                }),
            )],
        )?;
        Ok(())
    }

    /// Persist one pending approval request.
    pub fn request_approval(
        &self,
        workflow_id: &str,
        request: ApprovalRequest,
    ) -> Result<(WorkflowState, ApprovalRecord), WorkflowError> {
        let _lock = self.lock()?;
        let mut state = self.load(workflow_id)?;
        if state.status != WorkflowStatus::Active {
            return Err(super::state::GateError::NotActive(state.status).into());
        }
        let workspace = self.workspace_root();
        let arguments_hash = request.arguments_hash();
        let (preview, preview_redactions) = redact_secrets(&request.preview, &workspace);
        let (reason, reason_redactions) = match request.reason {
            Some(reason) => {
                let (reason, count) = redact_secrets(&reason, &workspace);
                (Some(reason), count)
            }
            None => (None, 0),
        };
        let approval_id = next_approval_id(now_ms());
        let record = ApprovalRecord {
            approval_id: approval_id.clone(),
            workflow_id: workflow_id.to_owned(),
            tool_name: request.tool_name,
            arguments_hash,
            workspace: workspace.display().to_string(),
            reason,
            preview,
            risk: request.risk,
            side_effect: request.side_effect,
            requested_at: now_ms(),
            approver: None,
            approved_at: None,
            policy_version: APPROVAL_POLICY_VERSION,
            status: ApprovalStatus::Pending,
        };
        self.write_approval(&record)?;
        state.pending_approvals.push(PendingApproval {
            approval_id: approval_id.clone(),
            arguments_hash: record.arguments_hash.clone(),
        });
        let redactions = preview_redactions + reason_redactions;
        self.commit(
            &mut state,
            vec![(
                "approval.requested",
                json!({
                    "approval_id": approval_id,
                    "tool_name": record.tool_name,
                    "arguments_hash": record.arguments_hash,
                    "risk": record.risk,
                    "redactions": redactions,
                }),
            )],
        )?;
        Ok((state, record))
    }

    /// Read pending approval records, newest request first.
    pub fn pending_approvals(
        &self,
        workflow_id: &str,
    ) -> Result<Vec<ApprovalRecord>, WorkflowError> {
        let state = self.load(workflow_id)?;
        let mut records = Vec::new();
        for pending in &state.pending_approvals {
            let record = self.read_approval(workflow_id, &pending.approval_id)?;
            if record.status == ApprovalStatus::Pending {
                records.push(record);
            }
        }
        records.sort_by_key(|record| std::cmp::Reverse(record.requested_at));
        Ok(records)
    }

    /// Read one persisted approval record.
    pub fn read_approval(
        &self,
        workflow_id: &str,
        approval_id: &str,
    ) -> Result<ApprovalRecord, WorkflowError> {
        if !safe_component(approval_id) {
            return Err(WorkflowError::InvalidId(approval_id.to_owned()));
        }
        let path = self
            .approvals_dir(workflow_id)
            .join(format!("{approval_id}.json"));
        let bytes = fs::read(&path).map_err(|error| {
            if error.kind() == io::ErrorKind::NotFound {
                WorkflowError::NotFound(approval_id.to_owned())
            } else {
                WorkflowError::Io(error)
            }
        })?;
        serde_json::from_slice(&bytes).map_err(|error| WorkflowError::Corrupt(error.to_string()))
    }

    /// Resolve one pending approval.
    ///
    /// A changed argument hash marks the request stale instead of executing it.
    /// Resolving a stale or already resolved record is idempotent.
    pub fn resolve_approval(
        &self,
        workflow_id: &str,
        approval_id: &str,
        decision: ApprovalDecision,
        approver: Option<&str>,
        current_arguments_hash: &str,
    ) -> Result<(WorkflowState, ApprovalRecord), WorkflowError> {
        let _lock = self.lock()?;
        let mut state = self.load(workflow_id)?;
        let Some(position) = state
            .pending_approvals
            .iter()
            .position(|pending| pending.approval_id == approval_id)
        else {
            let record = self.read_approval(workflow_id, approval_id)?;
            if record.status != ApprovalStatus::Pending {
                return Ok((state, record));
            }
            return Err(WorkflowError::NotFound(format!(
                "pending approval {approval_id}"
            )));
        };
        let mut record = self.read_approval(workflow_id, approval_id)?;
        if record.arguments_hash != current_arguments_hash {
            return self.mark_approval_stale(
                approval_id,
                state,
                position,
                record,
                approver,
                current_arguments_hash,
            );
        }

        record.status = decision.status();
        record.approver = approver.map(str::to_owned);
        record.approved_at = matches!(decision, ApprovalDecision::Approve).then(now_ms);
        self.write_approval(&record)?;
        state.pending_approvals.remove(position);
        let kind = decision.event_kind();
        self.commit(
            &mut state,
            vec![(
                kind,
                json!({
                    "approval_id": approval_id,
                    "tool_name": record.tool_name,
                    "arguments_hash": record.arguments_hash,
                }),
            )],
        )?;
        Ok((state, record))
    }

    fn mark_approval_stale(
        &self,
        approval_id: &str,
        mut state: WorkflowState,
        position: usize,
        mut record: ApprovalRecord,
        approver: Option<&str>,
        current_arguments_hash: &str,
    ) -> Result<(WorkflowState, ApprovalRecord), WorkflowError> {
        record.status = ApprovalStatus::Stale;
        record.approver = approver.map(str::to_owned);
        record.approved_at = None;
        self.write_approval(&record)?;
        state.pending_approvals.remove(position);
        self.commit(
            &mut state,
            vec![(
                "approval.stale",
                json!({
                    "approval_id": approval_id,
                    "tool_name": record.tool_name,
                    "stored_hash": record.arguments_hash,
                    "current_hash": current_arguments_hash,
                }),
            )],
        )?;
        Ok((state, record))
    }

    fn write_approval(&self, record: &ApprovalRecord) -> Result<(), WorkflowError> {
        let directory = self.approvals_dir(&record.workflow_id);
        fs::create_dir_all(&directory)?;
        let bytes = serde_json::to_vec_pretty(record)
            .map_err(|error| WorkflowError::Corrupt(error.to_string()))?;
        write_atomic(
            &directory.join(format!("{}.json", record.approval_id)),
            &bytes,
        )?;
        Ok(())
    }

    fn workspace_fingerprint(&self, state: &WorkflowState) -> String {
        let mut parts = Vec::new();
        if let Ok(output) = std::process::Command::new("git")
            .arg("-C")
            .arg(self.workspace_root())
            .args(["rev-parse", "HEAD"])
            .output()
            && output.status.success()
        {
            parts.push(String::from_utf8_lossy(&output.stdout).trim().to_owned());
        }
        if let Some(contract) = &state.scaffold {
            for (path, hash) in &contract.framework_hashes {
                parts.push(format!("framework:{path}:{hash}"));
            }
        }
        if let Some(contract) = &state.test_contract {
            for file in &contract.test_files {
                parts.push(format!("test:{file}"));
            }
            for (path, hash) in &contract.framework_hashes {
                parts.push(format!("test-framework:{path}:{hash}"));
            }
        }
        super::contract::sha256_hex(parts.join("\n").as_bytes())
    }

    fn commit(
        &self,
        state: &mut WorkflowState,
        events: Vec<(&str, Value)>,
    ) -> Result<(), WorkflowError> {
        let next_version = state.version + 1;
        let transition_id = format!("{}-v{next_version}", state.workflow_id);
        let mut events = events;
        let effective = if state.status == WorkflowStatus::Active {
            state.phase.capability_profile()
        } else {
            None
        };
        let effective_role = effective.map(|profile| profile.role_name().to_owned());
        let effective_profile = effective.map(|profile| profile.as_str().to_owned());
        if effective_role != state.last_role || effective_profile != state.last_profile {
            let trigger_event = events
                .first()
                .map_or("unknown", |(kind, _)| *kind)
                .to_owned();
            events.insert(
                0,
                (
                    "role.switched",
                    json!({
                        "from_role": state.last_role.clone(),
                        "to_role": effective_role.clone(),
                        "from_profile": state.last_profile.clone(),
                        "to_profile": effective_profile.clone(),
                        "reason": "phase_transition",
                        "phase": state.phase,
                        "trigger_event": trigger_event,
                    }),
                ),
            );
            state.last_role = effective_role;
            state.last_profile = effective_profile;
        }

        // A retry after a crash between the event append and the snapshot
        // rename reuses the same transition id. Do not append its events twice.
        let existing = self.read_events(&state.workflow_id)?;
        if existing
            .iter()
            .any(|event| event.transition_id == transition_id)
        {
            if let Some(checkpoint) = existing
                .iter()
                .rev()
                .find(|event| event.transition_id == transition_id)
                .and_then(|event| event.checkpoint.clone())
            {
                *state = checkpoint;
            } else {
                state.last_seq = existing.last().map_or(state.last_seq, |event| event.seq);
                state.version = next_version;
                state.updated_at = now_ms();
            }
            self.write_state(state)?;
            self.update_index(state)?;
            return Ok(());
        }

        let mut final_state = state.clone();
        final_state.version = next_version;
        final_state.updated_at = now_ms();
        final_state.last_seq = state
            .last_seq
            .saturating_add(u64::try_from(events.len()).unwrap_or(u64::MAX));
        for (index, (kind, payload)) in events.into_iter().enumerate() {
            let seq = state
                .last_seq
                .saturating_add(u64::try_from(index).unwrap_or(u64::MAX))
                .saturating_add(1);
            let checkpoint = (index == 0).then_some(&final_state);
            self.append_event(&final_state, seq, &transition_id, kind, payload, checkpoint)?;
        }
        *state = final_state;
        self.write_state(state)?;
        self.update_index(state)?;
        Ok(())
    }

    fn update_index(&self, state: &WorkflowState) -> Result<(), WorkflowError> {
        let mut index = self.read_index()?;
        if state.status == WorkflowStatus::Active {
            index.active = Some(state.workflow_id.clone());
        } else if index.active.as_deref() == Some(state.workflow_id.as_str()) {
            index.active = None;
        }
        upsert_summary(&mut index.workflows, state);
        self.write_index(&index)?;
        Ok(())
    }

    fn lock(&self) -> Result<WorkflowLock, WorkflowError> {
        fs::create_dir_all(&self.root)?;
        let path = self.root.join(".lock");
        match OpenOptions::new().write(true).create_new(true).open(&path) {
            Ok(mut file) => {
                let _ = writeln!(file, "pid={}", std::process::id());
                Ok(WorkflowLock { path })
            }
            Err(error) if error.kind() == io::ErrorKind::AlreadyExists => {
                Err(WorkflowError::Locked)
            }
            Err(error) => Err(WorkflowError::Io(error)),
        }
    }

    fn workspace_root(&self) -> PathBuf {
        self.root
            .parent()
            .and_then(Path::parent)
            .map(Path::to_path_buf)
            .unwrap_or_else(|| self.root.clone())
    }

    fn workflow_dir(&self, workflow_id: &str) -> PathBuf {
        self.root.join(workflow_id)
    }

    fn state_path(&self, workflow_id: &str) -> PathBuf {
        self.workflow_dir(workflow_id).join("state.json")
    }

    fn events_path(&self, workflow_id: &str) -> PathBuf {
        self.workflow_dir(workflow_id).join("events.jsonl")
    }

    fn approvals_dir(&self, workflow_id: &str) -> PathBuf {
        self.workflow_dir(workflow_id).join("approvals")
    }

    fn runs_dir(&self, workflow_id: &str) -> PathBuf {
        self.workflow_dir(workflow_id).join("runs")
    }

    fn write_state(&self, state: &WorkflowState) -> Result<(), WorkflowError> {
        let directory = self.workflow_dir(&state.workflow_id);
        fs::create_dir_all(&directory)?;
        let bytes =
            serde_json::to_vec(state).map_err(|error| WorkflowError::Corrupt(error.to_string()))?;
        let bytes = self.redact_bytes(bytes)?;
        write_atomic(&self.state_path(&state.workflow_id), &bytes)?;
        Ok(())
    }

    fn redact_bytes(&self, bytes: Vec<u8>) -> Result<Vec<u8>, WorkflowError> {
        let text =
            String::from_utf8(bytes).map_err(|error| WorkflowError::Corrupt(error.to_string()))?;
        let (redacted, _) = redact_secrets(&text, &self.workspace_root());
        Ok(redacted.into_bytes())
    }

    fn append_event(
        &self,
        state: &WorkflowState,
        seq: u64,
        transition_id: &str,
        kind: &str,
        payload: Value,
        checkpoint: Option<&WorkflowState>,
    ) -> Result<(), WorkflowError> {
        let directory = self.workflow_dir(&state.workflow_id);
        fs::create_dir_all(&directory)?;
        let event = WorkflowEvent {
            event_id: format!("{}-e{seq}", state.workflow_id),
            transition_id: transition_id.to_owned(),
            seq,
            at: now_ms(),
            phase: state.phase,
            kind: kind.to_owned(),
            payload,
            checkpoint: checkpoint.cloned(),
        };
        let mut line = serde_json::to_vec(&event)
            .map_err(|error| WorkflowError::Corrupt(error.to_string()))?;
        line = self.redact_bytes(line)?;
        line.push(b'\n');
        let mut file = OpenOptions::new()
            .create(true)
            .append(true)
            .open(self.events_path(&state.workflow_id))?;
        file.write_all(&line)?;
        file.sync_all()?;
        Ok(())
    }

    fn read_index(&self) -> Result<WorkflowIndex, WorkflowError> {
        let path = self.root.join("index.json");
        match fs::read(&path) {
            Ok(bytes) => serde_json::from_slice(&bytes)
                .map_err(|error| WorkflowError::Corrupt(error.to_string())),
            Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(WorkflowIndex::default()),
            Err(error) => Err(WorkflowError::Io(error)),
        }
    }

    fn write_index(&self, index: &WorkflowIndex) -> Result<(), WorkflowError> {
        fs::create_dir_all(&self.root)?;
        let bytes = serde_json::to_vec_pretty(index)
            .map_err(|error| WorkflowError::Corrupt(error.to_string()))?;
        write_atomic(&self.root.join("index.json"), &bytes)?;
        Ok(())
    }
}

fn apply_block(
    state: &mut WorkflowState,
    reason: &str,
    last_failure: Option<String>,
    resume_hint: &str,
) {
    state.status = WorkflowStatus::Blocked;
    state.blocked = Some(super::state::BlockedState {
        reason: reason.to_owned(),
        last_failure,
        pending_action: Some("/workflow use <id> after fixing the cause".to_owned()),
        resume_hint: Some(resume_hint.to_owned()),
    });
}

fn failure_text(record: &TestRunRecord) -> Option<String> {
    if !record.failures.is_empty() {
        return Some(record.failures.join(", "));
    }
    if record.status == TestStatus::Failed {
        return Some(
            record
                .exit_code
                .map(|code| format!("tests failed with exit code {code}"))
                .unwrap_or_else(|| "<build>".to_owned()),
        );
    }
    None
}

fn upsert_summary(summaries: &mut Vec<WorkflowSummary>, state: &WorkflowState) {
    let summary = WorkflowSummary {
        workflow_id: state.workflow_id.clone(),
        task: state.task.clone(),
        status: state.status,
        phase: state.phase,
        created_at: state.created_at,
        updated_at: state.updated_at,
    };
    if let Some(existing) = summaries
        .iter_mut()
        .find(|existing| existing.workflow_id == state.workflow_id)
    {
        *existing = summary;
    } else {
        summaries.push(summary);
    }
}

fn validate_id(workflow_id: &str) -> Result<(), WorkflowError> {
    let valid = !workflow_id.is_empty()
        && workflow_id
            .chars()
            .all(|ch| ch.is_ascii_alphanumeric() || matches!(ch, '-' | '_' | '.'));
    if valid && !workflow_id.contains("..") {
        Ok(())
    } else {
        Err(WorkflowError::InvalidId(workflow_id.to_owned()))
    }
}

fn safe_component(value: &str) -> bool {
    !value.is_empty()
        && !value.contains("..")
        && value
            .chars()
            .all(|ch| ch.is_ascii_alphanumeric() || matches!(ch, '-' | '_' | '.'))
}

fn next_approval_id(now: u64) -> String {
    next_id("approval", now)
}

fn next_workflow_id(now: u64) -> String {
    next_id("plan", now)
}

fn next_id(prefix: &str, now: u64) -> String {
    static COUNTER: AtomicU64 = AtomicU64::new(0);
    format!("{prefix}-{now}-{}", COUNTER.fetch_add(1, Ordering::Relaxed))
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
        .unwrap_or("workflow");
    let temporary = parent.join(format!(
        ".{file_name}.tmp-{}-{}",
        std::process::id(),
        now_ms()
    ));
    {
        let mut file = File::create(&temporary)?;
        file.write_all(bytes)?;
        file.sync_all()?;
    }
    fs::rename(&temporary, path)?;
    Ok(())
}

/// Durable workflow errors.
#[derive(Debug, Error)]
pub enum WorkflowError {
    #[error("workflow journal io error: {0}")]
    Io(#[from] io::Error),
    #[error("workflow {0} not found")]
    NotFound(String),
    #[error("invalid workflow id: {0}")]
    InvalidId(String),
    #[error("workflow journal is corrupt: {0}")]
    Corrupt(String),
    #[error("workflow schema version {0} is not supported")]
    UnsupportedSchema(u32),
    #[error(
        "workflow {0} already exists as the active workflow; run /workflow end or /workflow use <id> first"
    )]
    ActiveExists(String),
    #[error("no active workflow; run /plan first")]
    NoActive,
    #[error(
        "another workflow write is in progress; remove .scratch/workflows/.lock if it is stale"
    )]
    Locked,
    #[error("workflow {0} has ended and cannot be resumed")]
    Terminal(String),
    #[error("{0}")]
    Gate(#[from] super::state::GateError),
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::workflow::state::{
        Domain, ExecState, Question, QuestionKind, QuestionStatus, Strategy,
    };

    fn temp_workspace() -> PathBuf {
        static COUNTER: AtomicU64 = AtomicU64::new(0);
        let nonce = now_ms();
        let unique = COUNTER.fetch_add(1, Ordering::Relaxed);
        let path = std::env::temp_dir().join(format!(
            "slowcode-workflow-{}-{nonce}-{unique}",
            std::process::id()
        ));
        fs::create_dir_all(&path).expect("temp workspace is created");
        path
    }

    fn task(id: &str, domain: Domain, exec_state: ExecState, strategy: Strategy) -> Task {
        Task::new(
            id.to_owned(),
            format!("title {id}"),
            domain,
            "evidence".to_owned(),
            exec_state,
            "action".to_owned(),
            "role".to_owned(),
            "path".to_owned(),
            "acceptance".to_owned(),
            strategy,
        )
    }

    fn known_task() -> Task {
        task(
            "task-1",
            Domain::Known,
            ExecState::Executable,
            Strategy::AiImplements,
        )
    }

    #[test]
    fn creates_state_events_and_index() {
        let workspace = temp_workspace();
        let store = WorkflowStore::new(&workspace);
        let state = store
            .create_with_tasks("count lines", None, vec![known_task()])
            .expect("workflow is created");
        assert_eq!(state.phase, Phase::Review);
        assert_eq!(state.status, WorkflowStatus::Active);
        assert!(store.state_path(&state.workflow_id).exists());
        assert!(store.events_path(&state.workflow_id).exists());
        assert_eq!(
            store.active_id().expect("index is readable").as_deref(),
            Some(state.workflow_id.as_str())
        );
        let events = fs::read_to_string(store.events_path(&state.workflow_id)).expect("events");
        assert_eq!(events.lines().count(), 2);
        let _ = fs::remove_dir_all(&workspace);
    }

    #[test]
    fn a_second_active_workflow_is_rejected() {
        let workspace = temp_workspace();
        let store = WorkflowStore::new(&workspace);
        store
            .create_with_tasks("first", None, vec![known_task()])
            .expect("first workflow");
        assert!(matches!(
            store.create_with_tasks("second", None, vec![known_task()]),
            Err(WorkflowError::ActiveExists(_))
        ));
        let _ = fs::remove_dir_all(&workspace);
    }

    #[test]
    fn context_manifest_is_recorded_once_per_change() {
        let workspace = temp_workspace();
        let store = WorkflowStore::new(&workspace);
        let state = store
            .create_with_tasks("manifest", None, vec![known_task()])
            .expect("workflow");
        let skill_dir = workspace.join(".slowcode/skills/navigate");
        fs::create_dir_all(&skill_dir).expect("skill dir");
        fs::write(
            skill_dir.join("SKILL.md"),
            "---\nname: navigate\ndescription: test\n---\nBody.\n",
        )
        .expect("skill");
        let manifest = crate::workflow::manifest::build_skill_manifest(
            &workspace,
            Phase::Review,
            Some(crate::workflow::CapabilityProfile::Navigator),
        );
        assert_eq!(manifest.skill_names(), vec!["navigate"]);

        let updated = store
            .record_context_manifest(&state.workflow_id, manifest.clone())
            .expect("manifest is recorded");
        assert_eq!(updated.context_manifest.as_ref(), Some(&manifest));
        let before = fs::read_to_string(store.events_path(&state.workflow_id))
            .expect("events")
            .lines()
            .count();
        store
            .record_context_manifest(&state.workflow_id, manifest)
            .expect("duplicate manifest is accepted");
        let after = fs::read_to_string(store.events_path(&state.workflow_id))
            .expect("events")
            .lines()
            .count();
        assert_eq!(before, after, "identical manifests do not append events");
        let events = fs::read_to_string(store.events_path(&state.workflow_id)).expect("events");
        assert!(events.contains("\"kind\":\"context.manifest\""), "{events}");
        assert!(events.contains("\"skills\":[\"navigate\"]"), "{events}");
        let _ = fs::remove_dir_all(&workspace);
    }

    #[test]
    fn shell_readonly_is_recorded_on_the_workflow() {
        let workspace = temp_workspace();
        let store = WorkflowStore::new(&workspace);
        let state = store
            .create_with_tasks("shell", None, Vec::new())
            .expect("workflow");
        store
            .record_shell_readonly(
                &state.workflow_id,
                json!({ "kind": "shell.readonly", "argv": ["ls"], "exit_code": 0 }),
            )
            .expect("event is recorded");
        let events = fs::read_to_string(store.events_path(&state.workflow_id)).expect("events");
        assert!(events.contains("\"kind\":\"shell.readonly\""), "{events}");
        assert!(events.contains("\"argv\":[\"ls\"]"), "{events}");
        let _ = fs::remove_dir_all(&workspace);
    }

    #[test]
    fn ending_the_active_workflow_clears_the_index() {
        let workspace = temp_workspace();
        let store = WorkflowStore::new(&workspace);
        let state = store
            .create_with_tasks("first", None, vec![known_task()])
            .expect("first workflow");
        store.end(&state.workflow_id).expect("workflow ends");
        assert_eq!(store.active_id().expect("index"), None);
        store
            .create_with_tasks("second", None, vec![known_task()])
            .expect("a new workflow may start");
        let _ = fs::remove_dir_all(&workspace);
    }

    #[test]
    fn pausing_the_active_workflow_clears_the_index_and_keeps_it_resumable() {
        let workspace = temp_workspace();
        let store = WorkflowStore::new(&workspace);
        let state = store
            .create_with_tasks("first", None, vec![known_task()])
            .expect("first workflow");

        let paused = store
            .pause_active()
            .expect("pause succeeds")
            .expect("a workflow was active");
        assert_eq!(paused.workflow_id, state.workflow_id);
        assert_eq!(paused.status, WorkflowStatus::Paused);
        assert_eq!(store.active_id().expect("index"), None);
        assert_eq!(
            store.load(&state.workflow_id).expect("state").status,
            WorkflowStatus::Paused
        );

        store
            .create_with_tasks("second", None, vec![known_task()])
            .expect("a new workflow may start");
        store
            .use_workflow(&state.workflow_id)
            .expect("the paused workflow is resumable");
        let _ = fs::remove_dir_all(&workspace);
    }

    #[test]
    fn pausing_without_an_active_workflow_is_a_noop() {
        let workspace = temp_workspace();
        let store = WorkflowStore::new(&workspace);
        assert!(store.pause_active().expect("pause succeeds").is_none());
        assert_eq!(store.active_id().expect("index"), None);
        let _ = fs::remove_dir_all(&workspace);
    }

    #[test]
    fn use_pauses_the_old_active_workflow() {
        let workspace = temp_workspace();
        let store = WorkflowStore::new(&workspace);
        let mut first = store
            .create_with_tasks("first", None, vec![known_task()])
            .expect("first workflow");
        store.end(&first.workflow_id).expect("first ends");
        first.status = WorkflowStatus::Paused;
        first.version = store.load(&first.workflow_id).expect("ended state").version + 1;
        store.write_state(&first).expect("first is paused on disk");
        let second = store
            .create_with_tasks("second", None, vec![known_task()])
            .expect("second workflow");
        store
            .use_workflow(&first.workflow_id)
            .expect("first is resumed");
        let paused = store.load(&second.workflow_id).expect("second loads");
        assert_eq!(paused.status, WorkflowStatus::Paused);
        assert_eq!(
            store.active_id().expect("index").as_deref(),
            Some(first.workflow_id.as_str())
        );
        let _ = fs::remove_dir_all(&workspace);
    }

    #[test]
    fn approving_advances_the_phase() {
        let workspace = temp_workspace();
        let store = WorkflowStore::new(&workspace);
        let state = store
            .create_with_tasks("first", None, vec![known_task()])
            .expect("workflow");
        let approved = store
            .approve(
                &state.workflow_id,
                Some(crate::workflow::state::TestContract::default()),
                None,
            )
            .expect("approval");
        assert_eq!(approved.phase, Phase::TestFix);
        assert!(approved.review_is_current());
        let _ = fs::remove_dir_all(&workspace);
    }

    #[test]
    fn red_tests_keep_the_workflow_in_test_fix() {
        let workspace = temp_workspace();
        let store = WorkflowStore::new(&workspace);
        let state = store
            .create_with_tasks("first", None, vec![known_task()])
            .expect("workflow");
        store
            .approve(
                &state.workflow_id,
                Some(crate::workflow::state::TestContract::default()),
                None,
            )
            .expect("approval");
        let failed = store
            .record_test(&state.workflow_id, false, Some("assert failed".to_owned()))
            .expect("test is recorded");
        assert_eq!(failed.phase, Phase::TestFix);
        assert_eq!(
            failed.test.last_status,
            Some(crate::workflow::state::TestStatus::Failed)
        );
        let _ = fs::remove_dir_all(&workspace);
    }

    #[test]
    fn a_stale_lock_file_rejects_writes() {
        let workspace = temp_workspace();
        let store = WorkflowStore::new(&workspace);
        fs::create_dir_all(store.root()).expect("root");
        fs::write(store.root().join(".lock"), "stale").expect("stale lock is written");
        assert!(matches!(
            store.create_with_tasks("first", None, vec![known_task()]),
            Err(WorkflowError::Locked)
        ));
        let _ = fs::remove_dir_all(&workspace);
    }

    #[test]
    fn answering_the_last_question_moves_to_review() {
        let workspace = temp_workspace();
        let store = WorkflowStore::new(&workspace);
        let state = store
            .create_with_tasks("first", None, vec![known_task()])
            .expect("workflow");
        let mut state = state;
        state.phase = Phase::Interview;
        state.questions.push(Question {
            question_id: "q-1".to_owned(),
            task_id: None,
            kind: QuestionKind::SemiKnown,
            prompt: "pick".to_owned(),
            options: Vec::new(),
            recommendation: None,
            reason: String::new(),
            status: QuestionStatus::Open,
            criteria: String::new(),
            created_at: 1,
            answer: None,
            answer_source: None,
            answer_snapshot: None,
            attempts: 0,
            last_gap: None,
        });
        store.write_state(&state).expect("state is seeded");
        let answered = store
            .answer(&state.workflow_id, "A", None, AnswerSource::Text)
            .expect("answer is recorded");
        assert_eq!(answered.phase, Phase::Review);
        assert!(answered.open_questions().is_empty());
        let _ = fs::remove_dir_all(&workspace);
    }

    fn distill_ready(workspace: &Path) -> (WorkflowStore, WorkflowState) {
        let store = WorkflowStore::new(workspace);
        let state = store
            .create_with_tasks("distill", None, vec![known_task()])
            .expect("workflow");
        store
            .approve(
                &state.workflow_id,
                Some(crate::workflow::state::TestContract::default()),
                None,
            )
            .expect("approval");
        let state = store
            .record_test(&state.workflow_id, true, None)
            .expect("green test");
        assert_eq!(state.phase, Phase::Distill);
        (store, state)
    }

    fn distill_question() -> Question {
        Question {
            question_id: String::new(),
            task_id: None,
            kind: QuestionKind::Knowledge,
            prompt: "尾随换行如何计数？".to_owned(),
            options: Vec::new(),
            recommendation: None,
            reason: "auditor follow-up".to_owned(),
            status: QuestionStatus::Open,
            criteria: "answer the follow-up".to_owned(),
            created_at: 0,
            answer: None,
            answer_source: None,
            answer_snapshot: None,
            attempts: 0,
            last_gap: None,
        }
    }

    #[test]
    fn accepting_distill_gaps_completes_the_workflow() {
        let workspace = temp_workspace();
        let (store, state) = distill_ready(&workspace);
        let id = state.workflow_id.clone();
        let state = store
            .record_distill_report(&id, Some(distill_question()))
            .expect("report is recorded");
        assert_eq!(state.open_questions().len(), 1);

        let update = DistillAnswerUpdate::new(
            DistillDecision::AcceptGaps,
            "接受剩余缺口并完成".to_owned(),
            AnswerSource::Text,
        );
        let done = store
            .answer_distill_follow_up(&id, update)
            .expect("gaps are accepted");
        assert_eq!(done.phase, Phase::Done);
        assert_eq!(done.status, WorkflowStatus::Completed);
        assert!(done.distill.as_ref().expect("distill").gaps_accepted);
        assert!(done.open_questions().is_empty());
        assert_eq!(store.active_id().expect("index"), None);
        let events = fs::read_to_string(store.events_path(&id)).expect("events");
        assert!(events.contains("distill.gaps_accepted"), "{events}");
        assert!(events.contains("workflow.completed"), "{events}");
        let _ = fs::remove_dir_all(&workspace);
    }

    #[test]
    fn answering_the_distill_follow_up_completes_the_workflow() {
        let workspace = temp_workspace();
        let (store, state) = distill_ready(&workspace);
        let id = state.workflow_id.clone();
        store
            .record_distill_report(&id, Some(distill_question()))
            .expect("report is recorded");

        let update = DistillAnswerUpdate::new(
            DistillDecision::Answered,
            "没有尾随换行时不算最后一行".to_owned(),
            AnswerSource::Text,
        );
        let done = store
            .answer_distill_follow_up(&id, update)
            .expect("answer is recorded");
        assert_eq!(done.phase, Phase::Done);
        assert_eq!(done.status, WorkflowStatus::Completed);
        assert!(done.open_questions().is_empty());
        let events = fs::read_to_string(store.events_path(&id)).expect("events");
        assert!(events.contains("distill.followup_answered"), "{events}");
        let _ = fs::remove_dir_all(&workspace);
    }

    #[test]
    fn a_distill_follow_up_can_return_to_reclassify() {
        let workspace = temp_workspace();
        let (store, state) = distill_ready(&workspace);
        let id = state.workflow_id.clone();
        store
            .record_distill_report(&id, Some(distill_question()))
            .expect("report is recorded");

        let mut update = DistillAnswerUpdate::new(
            DistillDecision::Reclassify,
            "这暴露了新的未知域".to_owned(),
            AnswerSource::Text,
        );
        update.follow_up = Some(distill_question());
        let reclassified = store
            .answer_distill_follow_up(&id, update)
            .expect("reclassification is recorded");
        assert_eq!(
            reclassified.phase,
            Phase::Interview,
            "the new knowledge question must be answered in Interview"
        );
        assert_eq!(reclassified.open_questions().len(), 1);
        assert_eq!(
            reclassified.questions[1].question_id, "q-2",
            "the follow-up answer appends one knowledge question"
        );
        let events = fs::read_to_string(store.events_path(&id)).expect("events");
        assert!(events.contains("distill.followup_answered"), "{events}");
        assert!(events.contains("\"phase\":\"reclassify\""), "{events}");
        assert!(events.contains("\"next_phase\":\"interview\""), "{events}");
        let _ = fs::remove_dir_all(&workspace);
    }

    #[test]
    fn answering_a_reclassified_distill_question_returns_to_review() {
        let workspace = temp_workspace();
        let (store, state) = distill_ready(&workspace);
        let id = state.workflow_id.clone();
        store
            .record_distill_report(&id, Some(distill_question()))
            .expect("report is recorded");
        let mut update = DistillAnswerUpdate::new(
            DistillDecision::Reclassify,
            "这暴露了新的未知域".to_owned(),
            AnswerSource::Text,
        );
        update.follow_up = Some(distill_question());
        store
            .answer_distill_follow_up(&id, update)
            .expect("reclassification is recorded");

        let answered = store
            .answer(&id, "the missing rule", None, AnswerSource::Text)
            .expect("the knowledge question is answered");
        assert_eq!(answered.phase, Phase::Review);
        assert!(answered.open_questions().is_empty());
        let _ = fs::remove_dir_all(&workspace);
    }

    #[test]
    fn a_second_distill_report_is_rejected() {
        let workspace = temp_workspace();
        let (store, state) = distill_ready(&workspace);
        let id = state.workflow_id.clone();
        store
            .record_distill_report(&id, Some(distill_question()))
            .expect("first report");
        let error = store
            .record_distill_report(&id, Some(distill_question()))
            .expect_err("a pending follow-up blocks a second report");
        assert!(matches!(error, WorkflowError::Gate(_)), "{error}");
        let reloaded = store.load(&id).expect("state loads");
        assert_eq!(reloaded.open_questions().len(), 1);
        let _ = fs::remove_dir_all(&workspace);
    }

    #[test]
    fn a_distill_answer_rejects_an_unknown_question_id() {
        let workspace = temp_workspace();
        let (store, state) = distill_ready(&workspace);
        let id = state.workflow_id.clone();
        store
            .record_distill_report(&id, Some(distill_question()))
            .expect("report is recorded");
        let mut update = DistillAnswerUpdate::new(
            DistillDecision::AcceptGaps,
            "接受剩余缺口并完成".to_owned(),
            AnswerSource::Text,
        );
        update.question_id = Some("q-99".to_owned());
        let error = store
            .answer_distill_follow_up(&id, update)
            .expect_err("unknown question ids are rejected");
        assert!(matches!(error, WorkflowError::NotFound(_)), "{error}");
        let reloaded = store.load(&id).expect("state loads");
        assert_eq!(reloaded.phase, Phase::Distill);
        assert_eq!(reloaded.open_questions().len(), 1);
        let _ = fs::remove_dir_all(&workspace);
    }

    #[test]
    fn reclassify_is_blocked_while_a_distill_follow_up_is_open() {
        let workspace = temp_workspace();
        let (store, state) = distill_ready(&workspace);
        let id = state.workflow_id.clone();
        store
            .record_distill_report(&id, Some(distill_question()))
            .expect("report is recorded");
        let error = store
            .reclassify(&id, "a new unknown domain")
            .expect_err("distill answers must go through the follow-up");
        assert!(matches!(error, WorkflowError::Gate(_)), "{error}");
        let reloaded = store.load(&id).expect("state loads");
        assert_eq!(reloaded.phase, Phase::Distill);
        assert_eq!(reloaded.open_questions().len(), 1);
        let _ = fs::remove_dir_all(&workspace);
    }

    #[test]
    fn events_stay_monotonic_across_transitions() {
        let workspace = temp_workspace();
        let store = WorkflowStore::new(&workspace);
        let state = store
            .create_with_tasks("first", None, vec![known_task()])
            .expect("workflow");
        store
            .approve(
                &state.workflow_id,
                Some(crate::workflow::state::TestContract::default()),
                None,
            )
            .expect("approval");
        store.end(&state.workflow_id).expect("end");
        let lines = fs::read_to_string(store.events_path(&state.workflow_id)).expect("events");
        let events: Vec<WorkflowEvent> = lines
            .lines()
            .map(|line| serde_json::from_str(line).expect("event parses"))
            .collect();
        let sequences: Vec<u64> = events.iter().map(|event| event.seq).collect();
        assert_eq!(sequences, vec![1, 2, 3, 4, 5, 6]);
        let role_events: Vec<&WorkflowEvent> = events
            .iter()
            .filter(|event| event.kind == "role.switched")
            .collect();
        assert_eq!(role_events.len(), 2, "{events:?}");
        assert_eq!(role_events[0].payload["to_role"], "driver");
        assert_eq!(role_events[0].payload["to_profile"], "driver");
        assert_eq!(role_events[0].payload["reason"], "phase_transition");
        assert_eq!(role_events[1].payload["to_role"], serde_json::Value::Null);
        let transition_ids: Vec<&str> = events
            .iter()
            .map(|event| event.transition_id.as_str())
            .collect();
        let id = &state.workflow_id;
        let expected = [
            format!("{id}-v1"),
            format!("{id}-v1"),
            format!("{id}-v2"),
            format!("{id}-v2"),
            format!("{id}-v3"),
            format!("{id}-v3"),
        ];
        assert!(
            transition_ids
                .iter()
                .copied()
                .eq(expected.iter().map(String::as_str))
        );
        let _ = fs::remove_dir_all(&workspace);
    }

    #[test]
    fn role_events_follow_phase_capability_profiles() {
        let workspace = temp_workspace();
        let store = WorkflowStore::new(&workspace);
        let state = store
            .create_with_tasks(
                "profiles",
                None,
                vec![task(
                    "task-1",
                    Domain::Unknown,
                    ExecState::PendingUser,
                    Strategy::UserImplementsCore,
                )],
            )
            .expect("workflow");
        let id = state.workflow_id.clone();
        store
            .approve(&id, None, None)
            .expect("approval enters scaffold");
        store
            .finish_scaffold(
                &id,
                crate::workflow::state::ScaffoldContract::default(),
                None,
            )
            .expect("scaffold enters await user");
        store
            .complete_user_implementation(&id, true, None)
            .expect("continue enters test fix");
        store.record_test(&id, true, None).expect("green test");

        let events = fs::read_to_string(store.events_path(&id)).expect("events");
        let profiles: Vec<serde_json::Value> = events
            .lines()
            .map(|line| serde_json::from_str::<WorkflowEvent>(line).expect("event"))
            .filter(|event| event.kind == "role.switched")
            .map(|event| event.payload["to_profile"].clone())
            .collect();
        assert_eq!(
            profiles,
            vec![
                serde_json::json!("driver"),
                serde_json::json!("coach"),
                serde_json::json!("driver"),
                serde_json::json!("auditor"),
            ]
        );
        let _ = fs::remove_dir_all(&workspace);
    }

    #[test]
    fn corrupt_state_without_a_checkpoint_is_rejected() {
        let workspace = temp_workspace();
        let store = WorkflowStore::new(&workspace);
        let state = store
            .create_with_tasks("first", None, vec![known_task()])
            .expect("workflow");
        fs::remove_file(store.events_path(&state.workflow_id)).expect("events removed");
        fs::write(store.state_path(&state.workflow_id), "{not json").expect("state is corrupted");
        assert!(matches!(
            store.load(&state.workflow_id),
            Err(WorkflowError::Corrupt(_))
        ));
        let _ = fs::remove_dir_all(&workspace);
    }

    #[test]
    fn path_like_ids_are_rejected() {
        let workspace = temp_workspace();
        let store = WorkflowStore::new(&workspace);
        assert!(matches!(
            store.load("../escape"),
            Err(WorkflowError::InvalidId(_))
        ));
        assert!(matches!(
            store.load("nested/id"),
            Err(WorkflowError::InvalidId(_))
        ));
        let _ = fs::remove_dir_all(&workspace);
    }

    #[test]
    fn terminal_workflows_cannot_be_reactivated() {
        let workspace = temp_workspace();
        let store = WorkflowStore::new(&workspace);
        let state = store
            .create_with_tasks("first", None, vec![known_task()])
            .expect("workflow");
        store.end(&state.workflow_id).expect("end");
        assert!(matches!(
            store.use_workflow(&state.workflow_id),
            Err(WorkflowError::Terminal(_))
        ));
        let _ = fs::remove_dir_all(&workspace);
    }

    #[test]
    fn the_lock_is_released_after_a_write() {
        let workspace = temp_workspace();
        let store = WorkflowStore::new(&workspace);
        store
            .create_with_tasks("first", None, vec![known_task()])
            .expect("workflow");
        assert!(!store.root().join(".lock").exists());
        let _ = fs::remove_dir_all(&workspace);
    }

    #[test]
    fn concurrent_creates_allow_only_one_winner() {
        let workspace = temp_workspace();
        let store = std::sync::Arc::new(WorkflowStore::new(&workspace));
        let barrier = std::sync::Arc::new(std::sync::Barrier::new(2));
        let results = std::thread::scope(|scope| {
            let mut handles = Vec::new();
            for index in 0..2 {
                let store = std::sync::Arc::clone(&store);
                let barrier = std::sync::Arc::clone(&barrier);
                handles.push(scope.spawn(move || {
                    barrier.wait();
                    let task = format!("task {index}");
                    store.create_with_tasks(&task, None, vec![known_task()])
                }));
            }
            handles
                .into_iter()
                .map(|handle| handle.join().expect("thread finishes"))
                .collect::<Vec<_>>()
        });
        let successes = results.iter().filter(|result| result.is_ok()).count();
        assert_eq!(successes, 1, "{results:?}");
        let _ = fs::remove_dir_all(&workspace);
    }

    fn semi_question(task_id: &str) -> Question {
        Question {
            question_id: String::new(),
            task_id: Some(task_id.to_owned()),
            kind: QuestionKind::SemiKnown,
            prompt: "选 A 还是 B？".to_owned(),
            options: vec!["A".to_owned(), "B".to_owned()],
            recommendation: Some("A".to_owned()),
            reason: "兼容".to_owned(),
            status: QuestionStatus::Open,
            criteria: "必须选一个".to_owned(),
            created_at: 0,
            answer: None,
            answer_source: None,
            answer_snapshot: None,
            attempts: 0,
            last_gap: None,
        }
    }

    #[test]
    fn questions_start_the_workflow_in_interview() {
        let workspace = temp_workspace();
        let store = WorkflowStore::new(&workspace);
        let state = store
            .create_with_questions(
                "first",
                None,
                vec![known_task()],
                vec![semi_question("task-1")],
            )
            .expect("workflow");
        assert_eq!(state.phase, Phase::Interview);
        assert_eq!(state.questions.len(), 1);
        assert_eq!(state.questions[0].question_id, "q-1");
        let events = fs::read_to_string(store.events_path(&state.workflow_id)).expect("events");
        assert!(events.contains("\"question.asked\""), "{events}");
        let _ = fs::remove_dir_all(&workspace);
    }

    #[test]
    fn a_follow_up_keeps_the_workflow_in_interview() {
        let workspace = temp_workspace();
        let store = WorkflowStore::new(&workspace);
        let state = store
            .create_with_questions(
                "first",
                None,
                vec![known_task()],
                vec![semi_question("task-1")],
            )
            .expect("workflow");
        let mut update = AnswerUpdate::new("A".to_owned(), AnswerSource::Text);
        update.question_id = Some("q-1".to_owned());
        update.follow_up = Some(semi_question("task-1"));
        let answered = store
            .apply_answer(&state.workflow_id, update)
            .expect("answer");
        assert_eq!(answered.phase, Phase::Interview);
        assert_eq!(answered.questions[0].status, QuestionStatus::Answered);
        assert_eq!(answered.questions[1].question_id, "q-2");
        assert_eq!(answered.questions[1].status, QuestionStatus::Open);
        let _ = fs::remove_dir_all(&workspace);
    }

    #[test]
    fn an_insufficient_answer_keeps_the_task_table() {
        let workspace = temp_workspace();
        let store = WorkflowStore::new(&workspace);
        let state = store
            .create_with_questions(
                "first",
                None,
                vec![known_task()],
                vec![semi_question("task-1")],
            )
            .expect("workflow");
        let mut update = AnswerUpdate::new("maybe".to_owned(), AnswerSource::Text);
        update.question_id = Some("q-1".to_owned());
        update.insufficient = Some("没有说明兼容性".to_owned());
        let answered = store
            .apply_answer(&state.workflow_id, update)
            .expect("answer");
        assert_eq!(answered.tasks, state.tasks);
        assert_eq!(answered.questions[0].attempts, 1);
        assert_eq!(
            answered.questions[0].last_gap.as_deref(),
            Some("没有说明兼容性")
        );
        assert_eq!(answered.questions[0].status, QuestionStatus::Open);
        let _ = fs::remove_dir_all(&workspace);
    }

    #[test]
    fn reclassify_adds_a_system_question() {
        let workspace = temp_workspace();
        let store = WorkflowStore::new(&workspace);
        let state = store
            .create_with_tasks("first", None, vec![known_task()])
            .expect("workflow");
        let state = store
            .reclassify(&state.workflow_id, "缓存一致性语义未定")
            .expect("reclassify");
        assert_eq!(state.phase, Phase::Interview);
        assert_eq!(state.questions.len(), 1);
        assert_eq!(state.questions[0].kind, QuestionKind::Knowledge);
        let events = fs::read_to_string(store.events_path(&state.workflow_id)).expect("events");
        assert!(events.contains("user.reclassify_requested"), "{events}");
        let _ = fs::remove_dir_all(&workspace);
    }

    #[test]
    fn selector_question_moves_through_interview_and_back_to_review() {
        let workspace = temp_workspace();
        let store = WorkflowStore::new(&workspace);
        let state = store
            .create_with_tasks("first", None, vec![known_task()])
            .expect("workflow");
        assert_eq!(state.phase, Phase::Review);
        let state = store
            .ask_test_selector(
                &state.workflow_id,
                &["-p alpha".to_owned(), "-p beta".to_owned()],
                "tasks span two packages",
            )
            .expect("selector question");
        assert_eq!(state.phase, Phase::Interview);
        assert_eq!(state.questions[0].kind, QuestionKind::TestSelector);

        let state = store
            .answer_test_selector(&state.workflow_id, None, "-p beta", AnswerSource::Text)
            .expect("selector answer");
        assert_eq!(state.phase, Phase::Review);
        assert_eq!(state.test_selector.as_deref(), Some("-p beta"));
        assert_eq!(state.questions[0].status, QuestionStatus::Answered);
        let _ = fs::remove_dir_all(&workspace);
    }

    #[test]
    fn returning_to_review_clears_the_approval() {
        let workspace = temp_workspace();
        let store = WorkflowStore::new(&workspace);
        let state = store
            .create_with_tasks("review", None, vec![known_task()])
            .expect("workflow");
        let approved = store
            .approve(
                &state.workflow_id,
                Some(crate::workflow::state::TestContract::default()),
                None,
            )
            .expect("approval");
        assert!(approved.review_is_current());

        let returned = store
            .return_to_review(&state.workflow_id, "stale contract")
            .expect("contract returns to review");
        assert_eq!(returned.phase, Phase::Review);
        assert!(!returned.review_is_current());
        let events = fs::read_to_string(store.events_path(&state.workflow_id)).expect("events");
        assert!(events.contains("contract.invalid"), "{events}");
        let _ = fs::remove_dir_all(&workspace);
    }

    #[test]
    fn a_retried_transition_does_not_duplicate_events() {
        let workspace = temp_workspace();
        let store = WorkflowStore::new(&workspace);
        let state = store
            .create_with_tasks("retry", None, vec![known_task()])
            .expect("workflow");
        let transition = format!("{}-v2", state.workflow_id);
        let mut final_state = state.clone();
        final_state.version = 2;
        final_state.last_seq = 3;
        store
            .append_event(
                &final_state,
                3,
                &transition,
                "shell.readonly",
                json!({ "probe": "first attempt" }),
                None,
            )
            .expect("first attempt event is written");

        // state.json is still version 1, so the retry recomputes transition v2.
        store
            .record_shell_readonly(&state.workflow_id, json!({ "probe": "retry" }))
            .expect("retry completes");
        let events = store.read_events(&state.workflow_id).expect("events parse");
        assert_eq!(events.len(), 3);
        assert_eq!(
            events
                .iter()
                .filter(|event| event.transition_id == transition)
                .count(),
            1,
            "the retry must not append a second copy"
        );
        let persisted = store.load(&state.workflow_id).expect("state loads");
        assert_eq!(persisted.version, 2);
        assert_eq!(persisted.last_seq, 3);
        let _ = fs::remove_dir_all(&workspace);
    }

    #[test]
    fn a_torn_tail_is_ignored_and_the_checkpoint_rebuilds_state() {
        let workspace = temp_workspace();
        let store = WorkflowStore::new(&workspace);
        let state = store
            .create_with_tasks("recovery", None, vec![known_task()])
            .expect("workflow");
        fs::remove_file(store.state_path(&state.workflow_id)).expect("snapshot removed");
        let recovered = store.load(&state.workflow_id).expect("checkpoint recovers");
        assert_eq!(recovered.version, 1);
        assert_eq!(recovered.phase, state.phase);
        assert_eq!(recovered.tasks.len(), 1);

        let mut file = OpenOptions::new()
            .append(true)
            .open(store.events_path(&state.workflow_id))
            .expect("events open");
        file.write_all(b"{\"event_id\":\"torn")
            .expect("torn tail written");
        let events = store.read_events(&state.workflow_id).expect("events parse");
        assert_eq!(events.len(), 2);
        let _ = fs::remove_dir_all(&workspace);
    }

    #[test]
    fn cancelling_a_run_keeps_the_phase_and_marks_the_workspace_dirty() {
        let workspace = temp_workspace();
        let store = WorkflowStore::new(&workspace);
        let state = store
            .create_with_tasks("cancel", None, vec![known_task()])
            .expect("workflow");
        let phase = state.phase;
        let cancelled = store
            .cancel_run(&state.workflow_id, "turn-1", "user pressed esc", true)
            .expect("cancel is recorded");
        assert_eq!(cancelled.phase, phase);
        assert!(cancelled.workspace_dirty);
        assert!(cancelled.workspace_fingerprint.is_some());
        let events = fs::read_to_string(store.events_path(&state.workflow_id)).expect("events");
        assert!(events.contains("run.cancelled"), "{events}");
        let _ = fs::remove_dir_all(&workspace);
    }

    #[test]
    fn approvals_persist_and_stale_requests_are_not_executed() {
        let workspace = temp_workspace();
        let store = WorkflowStore::new(&workspace);
        let state = store
            .create_with_tasks("approval", None, vec![known_task()])
            .expect("workflow");
        let request = ApprovalRequest {
            tool_name: "write".to_owned(),
            arguments: "{\"path\":\"src/lib.rs\"}".to_owned(),
            reason: Some("scaffold writes files".to_owned()),
            preview: "token sk-abcdefghijklmnopqrstuvwx".to_owned(),
            risk: "medium".to_owned(),
            side_effect: "writes src/lib.rs".to_owned(),
        };
        let (pending_state, record) = store
            .request_approval(&state.workflow_id, request)
            .expect("approval is requested");
        assert_eq!(pending_state.pending_approvals.len(), 1);
        assert_eq!(record.status, ApprovalStatus::Pending);
        assert!(!record.preview.contains("sk-abcdefghijklmnopqrstuvwx"));
        assert!(record.preview.contains("[REDACTED:api_key]"));

        let reloaded = store.load(&state.workflow_id).expect("state reloads");
        assert_eq!(reloaded.pending_approvals.len(), 1);
        let pending = store
            .pending_approvals(&state.workflow_id)
            .expect("pending approvals read");
        assert_eq!(pending.len(), 1);
        assert_eq!(pending[0].approval_id, record.approval_id);

        let wrong_hash = super::super::approval::hash_arguments("different arguments");
        let (after_stale, stale) = store
            .resolve_approval(
                &state.workflow_id,
                &record.approval_id,
                ApprovalDecision::Approve,
                Some("tester"),
                &wrong_hash,
            )
            .expect("stale approval is resolved without executing");
        assert_eq!(stale.status, ApprovalStatus::Stale);
        assert!(after_stale.pending_approvals.is_empty());
        let events = fs::read_to_string(store.events_path(&state.workflow_id)).expect("events");
        assert!(events.contains("approval.stale"), "{events}");

        let second = ApprovalRequest {
            tool_name: "write".to_owned(),
            arguments: "{\"path\":\"src/lib.rs\"}".to_owned(),
            reason: None,
            preview: "safe preview".to_owned(),
            risk: "low".to_owned(),
            side_effect: "writes src/lib.rs".to_owned(),
        };
        let (_, second) = store
            .request_approval(&state.workflow_id, second)
            .expect("second approval is requested");
        let (after_approve, approved) = store
            .resolve_approval(
                &state.workflow_id,
                &second.approval_id,
                ApprovalDecision::Approve,
                Some("tester"),
                &second.arguments_hash,
            )
            .expect("approval is granted");
        assert_eq!(approved.status, ApprovalStatus::Approved);
        assert!(approved.approved_at.is_some());
        assert!(after_approve.pending_approvals.is_empty());
        let events = fs::read_to_string(store.events_path(&state.workflow_id)).expect("events");
        assert!(events.contains("approval.granted"), "{events}");
    }

    #[test]
    fn model_calls_are_summarized_and_archives_are_redacted() {
        let workspace = temp_workspace();
        let store = WorkflowStore::new(&workspace);
        let state = store
            .create_with_tasks("model audit", None, vec![known_task()])
            .expect("workflow");
        let secret = "sk-abcdefghijklmnopqrstuvwx";
        let call = ModelCall {
            turn_id: "turn-1".to_owned(),
            model: "deepseek-flash".to_owned(),
            input_tokens: 12,
            output_tokens: 8,
            cache_hit: Some(true),
            cost: None,
        };
        store
            .record_model_call(
                &state.workflow_id,
                call,
                &format!("prompt {secret}"),
                &format!("response {secret}"),
            )
            .expect("model call is recorded");

        let directory = workspace
            .join(".scratch/workflows")
            .join(&state.workflow_id)
            .join("runs/turn-1");
        let request = fs::read_to_string(directory.join("request.txt")).expect("request");
        let response = fs::read_to_string(directory.join("response.txt")).expect("response");
        assert!(!request.contains(secret), "{request}");
        assert!(!response.contains(secret), "{response}");
        assert!(request.contains("[REDACTED:api_key]"), "{request}");
        let events = fs::read_to_string(store.events_path(&state.workflow_id)).expect("events");
        assert!(!events.contains(secret), "{events}");
        assert!(events.contains("model.called"), "{events}");
        assert!(events.contains("\"redactions\":2"), "{events}");

        store
            .reclassify(&state.workflow_id, secret)
            .expect("reclassify is recorded");
        let events = fs::read_to_string(store.events_path(&state.workflow_id)).expect("events");
        assert!(!events.contains(secret), "{events}");
        let snapshot = fs::read_to_string(store.state_path(&state.workflow_id)).expect("state");
        assert!(!snapshot.contains(secret), "{snapshot}");
    }

    #[test]
    fn a_model_call_reference_points_at_the_run_archive() {
        let workspace = temp_workspace();
        let store = WorkflowStore::new(&workspace);
        let state = store
            .create_with_tasks("model ref", None, vec![known_task()])
            .expect("workflow");
        let call = ModelCall {
            turn_id: "turn-1".to_owned(),
            model: "deepseek-flash".to_owned(),
            input_tokens: 1,
            output_tokens: 2,
            cache_hit: None,
            cost: None,
        };
        let reference = "run-states/run-1/model-calls/turn-1/response.txt";
        store
            .record_model_call_reference(&state.workflow_id, call, "prompt-hash", reference, 3)
            .expect("reference is recorded");

        let events = fs::read_to_string(store.events_path(&state.workflow_id)).expect("events");
        assert!(events.contains("model.called"), "{events}");
        assert!(events.contains(reference), "{events}");
        assert!(events.contains("\"redactions\":3"), "{events}");
        assert!(
            !workspace
                .join(".scratch/workflows")
                .join(&state.workflow_id)
                .join("runs/turn-1")
                .exists(),
            "a reference must not copy the payloads"
        );
    }

    #[test]
    fn a_model_call_reference_cannot_escape_the_workflow() {
        let workspace = temp_workspace();
        let store = WorkflowStore::new(&workspace);
        let state = store
            .create_with_tasks("model ref escape", None, vec![known_task()])
            .expect("workflow");
        let call = ModelCall {
            turn_id: "turn-1".to_owned(),
            model: "deepseek-flash".to_owned(),
            input_tokens: 1,
            output_tokens: 2,
            cache_hit: None,
            cost: None,
        };

        let error = store
            .record_model_call_reference(
                &state.workflow_id,
                call,
                "prompt-hash",
                "../outside/response.txt",
                0,
            )
            .expect_err("escaping reference is rejected");
        assert!(error.to_string().contains("unsafe"), "{error}");
    }

    #[test]
    fn a_checkpoint_ahead_of_the_snapshot_is_used() {
        let workspace = temp_workspace();
        let store = WorkflowStore::new(&workspace);
        let state = store
            .create_with_tasks("checkpoint", None, vec![known_task()])
            .expect("workflow");
        let transition = format!("{}-v2", state.workflow_id);
        let mut final_state = state.clone();
        final_state.version = 2;
        final_state.last_seq = 3;
        final_state.phase = Phase::Interview;
        store
            .append_event(
                &final_state,
                3,
                &transition,
                "phase.advanced",
                json!({ "phase": final_state.phase }),
                Some(&final_state),
            )
            .expect("transition event is written");

        // state.json is still version 1; the checkpoint must win.
        let loaded = store.load(&state.workflow_id).expect("checkpoint loads");
        assert_eq!(loaded.version, 2);
        assert_eq!(loaded.last_seq, 3);
        assert_eq!(loaded.phase, Phase::Interview);
    }

    #[test]
    fn resolving_an_approval_twice_is_idempotent() {
        let workspace = temp_workspace();
        let store = WorkflowStore::new(&workspace);
        let state = store
            .create_with_tasks("approval idempotency", None, Vec::new())
            .expect("workflow");
        let (_, record) = store
            .request_approval(
                &state.workflow_id,
                ApprovalRequest {
                    tool_name: "write".to_owned(),
                    arguments: "{}".to_owned(),
                    reason: None,
                    preview: "preview".to_owned(),
                    risk: "low".to_owned(),
                    side_effect: "write".to_owned(),
                },
            )
            .expect("approval requested");
        let (_, first) = store
            .resolve_approval(
                &state.workflow_id,
                &record.approval_id,
                ApprovalDecision::Approve,
                Some("tester"),
                &record.arguments_hash,
            )
            .expect("first resolve");
        let events_after_first = store.read_events(&state.workflow_id).expect("events").len();
        let (_, second) = store
            .resolve_approval(
                &state.workflow_id,
                &record.approval_id,
                ApprovalDecision::Approve,
                Some("tester"),
                &record.arguments_hash,
            )
            .expect("second resolve is a no-op");
        assert_eq!(first.status, second.status);
        assert_eq!(
            store.read_events(&state.workflow_id).expect("events").len(),
            events_after_first
        );
    }

    #[test]
    fn a_corrupt_snapshot_falls_back_to_the_checkpoint() {
        let workspace = temp_workspace();
        let store = WorkflowStore::new(&workspace);
        let state = store
            .create_with_tasks("corrupt", None, vec![known_task()])
            .expect("workflow");
        fs::write(store.state_path(&state.workflow_id), b"{not json").expect("corrupt snapshot");
        let loaded = store.load(&state.workflow_id).expect("checkpoint wins");
        assert_eq!(loaded.workflow_id, state.workflow_id);
        assert_eq!(loaded.version, state.version);
        assert_eq!(loaded.tasks.len(), 1);
    }

    #[test]
    fn a_successful_test_run_clears_workspace_dirty() {
        let workspace = temp_workspace();
        let store = WorkflowStore::new(&workspace);
        let state = store
            .create_with_tasks("dirty", None, vec![known_task()])
            .expect("workflow");
        store
            .cancel_run(&state.workflow_id, "turn-1", "cancelled", true)
            .expect("cancel marks dirty");
        let state = store
            .approve(
                &state.workflow_id,
                Some(crate::workflow::state::TestContract::default()),
                None,
            )
            .expect("approval");
        assert!(state.workspace_dirty);
        let state = store
            .record_test(&state.workflow_id, true, None)
            .expect("green test");
        assert!(!state.workspace_dirty);
        assert!(state.workspace_fingerprint.is_some());
    }

    #[test]
    fn model_archives_redact_workspace_env_values() {
        let workspace = temp_workspace();
        let secret = "verysecretvalue123";
        fs::write(workspace.join(".env"), format!("MY_SECRET={secret}\n")).expect(".env");
        let store = WorkflowStore::new(&workspace);
        let state = store
            .create_with_tasks("env redaction", None, vec![known_task()])
            .expect("workflow");
        store
            .record_model_call(
                &state.workflow_id,
                ModelCall {
                    turn_id: "turn-env".to_owned(),
                    model: "deepseek-flash".to_owned(),
                    input_tokens: 1,
                    output_tokens: 1,
                    cache_hit: None,
                    cost: None,
                },
                &format!("value {secret}"),
                &format!("echo {secret}"),
            )
            .expect("model call");
        let directory = workspace
            .join(".scratch/workflows")
            .join(&state.workflow_id)
            .join("runs/turn-env");
        let request = fs::read_to_string(directory.join("request.txt")).expect("request");
        let response = fs::read_to_string(directory.join("response.txt")).expect("response");
        assert!(!request.contains(secret), "{request}");
        assert!(!response.contains(secret), "{response}");
        assert!(request.contains("[REDACTED:env]"), "{request}");
    }
}
