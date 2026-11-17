//! Plan command flow shared by the CLI and the TUI.
//!
//! Each Plan command runs its own read-only or workspace agent. Callers that
//! supply a Session reuse it for every command in the run, so Plan turns share
//! the conversation transcript and UI event stream. One-shot callers may omit
//! the Session and keep the historical ephemeral behaviour.

use std::{
    error::Error,
    fs, io,
    path::{Path, PathBuf},
    sync::{
        Arc,
        atomic::{AtomicBool, AtomicU64, Ordering},
    },
    time::{SystemTime, UNIX_EPOCH},
};

use slowcode_cognition::{
    ANSWER_SYSTEM_PROMPT, AnswerVerdictKind, CognitiveTask, DISTILL_SYSTEM_PROMPT,
    FIX_SYSTEM_PROMPT, HINT_SYSTEM_PROMPT, PLAN_SYSTEM_PROMPT, PLAN_TURN_INSTRUCTION,
    PlanValidation, QuestionDraft, SCAFFOLD_SYSTEM_PROMPT, answer_user_prompt,
    answer_verdict_repair_prompt, append_section, apply_decisions, artifact_slug,
    distill_follow_up, distill_repair_prompt, distill_user_prompt,
    failure_classification_repair_prompt, fix_user_prompt, hint_user_prompt, parse_answer_verdict,
    parse_decision_tables, parse_failure_classifications, parse_question_table,
    parse_scaffold_manifest, parse_task_table, question_answer_user_prompt, render_artifact,
    render_task_bullets, repair_prompt, scaffold_tasks, scaffold_user_prompt, set_artifact_status,
    validate_answer_verdict, validate_decisions, validate_distill_output,
    validate_failure_classifications, validate_plan_output,
};
use slowcode_kernel::skill_roots::SkillRoots;
use slowcode_kernel::{
    agent::{
        AgentLoop, ApprovalFuture, ApprovalProvider, ApprovalRequest, FileChangePolicy,
        MAX_AGENT_STEPS_PER_TURN, NoApprovalProvider, ShellPolicy, ToolExecutor, ToolExecutorError,
        TurnProposal,
    },
    model::{ContentBlock, LlmCallConfig, Message, MessageSource},
    provider::{ModelProvider, deepseek::DEEPSEEK_PROVIDER},
    session::{ApprovalOutcome, EventKind, NewEvent, Session, SurfaceIntent, TurnEndReason},
};
use slowcode_roles::agent::{AgentSpec, RegistryMode};
use slowcode_runtime::{
    ApprovalResolution, PendingTool, RunBudget, RunLink, RunModelCall, RunStatus, RunStore,
};
use tokio_util::sync::CancellationToken;

use crate::workflow::{
    AnswerSnapshot, AnswerSource, AnswerUpdate, CapabilityProfile, ClassifiedFailure,
    DEFAULT_MAX_OUTPUT_TOKENS, DistillAnswerUpdate, DistillDecision, Domain, ExecState,
    MAX_CONSECUTIVE_SAME_FAILURE, MAX_OUTPUT_TOKENS_PER_TURN, ManifestFile, ModelCall, Phase,
    ProjectTestConfig, Question, QuestionKind, QuestionStatus, ScaffoldContract, SecretPolicy,
    SelectorChoice, Strategy, Task as WorkflowTask, TestContract, TestRun, TestRunRecord,
    TestRunUpdate, TestStatus, UnknownGap, WorkflowBudget, WorkflowError, WorkflowState,
    WorkflowStatus, WorkflowStore, build_contract, build_review_contract, build_skill_manifest,
    changed_files, derive_selector, detect_project, hash_arguments, input::redact_secrets,
    read_answer_file, sha256_hex, snapshot_workspace, test_exec, validate_selector,
    verify_core_implemented, verify_hashes, verify_test_contract_hashes, verify_test_flip,
};

/// Errors from a Plan command. The message is user-facing.
type BoxError = Box<dyn Error + Send + Sync>;
type PlanOutcomeFuture<'a> =
    std::pin::Pin<Box<dyn std::future::Future<Output = Result<PlanOutcome, BoxError>> + Send + 'a>>;
type PlanBodyFuture<'a, T> =
    std::pin::Pin<Box<dyn std::future::Future<Output = Result<T, BoxError>> + Send + 'a>>;
type PlanAgentFuture<'a> = std::pin::Pin<
    Box<
        dyn std::future::Future<Output = Result<(AgentLoop, Arc<dyn ToolExecutor>), BoxError>>
            + Send
            + 'a,
    >,
>;

/// Handle to the run that owns the current command. Model turns read it from
/// the task-local below so reserve and settle do not have to thread through
/// every Plan command signature.
#[derive(Clone)]
struct RunRecorder {
    store: RunStore,
    run_id: String,
}

tokio::task_local! {
    static ACTIVE_RUN: RunRecorder;
}

/// Wraps the real approval UI so every request is durable in the workflow
/// journal before the user sees it.
struct PersistedApprovalProvider {
    inner: Arc<dyn ApprovalProvider>,
    store: WorkflowStore,
    workflow_id: String,
    run: Option<RunRecorder>,
}

impl ApprovalProvider for PersistedApprovalProvider {
    fn request(
        &self,
        request: ApprovalRequest,
        cancellation: CancellationToken,
    ) -> ApprovalFuture<'_> {
        Box::pin(async move {
            let preview = request.preview().to_owned();
            let arguments_hash = hash_arguments(&preview);
            let requested_at = SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .map(|duration| duration.as_millis() as u64)
                .unwrap_or_default();
            let pending_tool = self
                .run
                .as_ref()
                .and_then(|run| {
                    run.store
                        .set_pending_tool(
                            &run.run_id,
                            PendingTool {
                                call_id: request.call_id().as_str().to_owned(),
                                name: request.tool_name().to_owned(),
                                arguments_hash: arguments_hash.clone(),
                                requested_at,
                            },
                        )
                        .ok()
                })
                .is_some();
            let pending = self
                .store
                .request_approval(
                    &self.workflow_id,
                    crate::workflow::ApprovalRequest {
                        tool_name: request.tool_name().to_owned(),
                        arguments: preview.clone(),
                        reason: request.reason().map(str::to_owned),
                        preview,
                        risk: "unknown".to_owned(),
                        side_effect: "tool side effect".to_owned(),
                    },
                )
                .ok()
                .map(|(_, record)| record.approval_id);
            let outcome = self.inner.request(request, cancellation).await?;
            if pending_tool {
                self.resolve_run_approval(outcome);
            }
            if let Some(approval_id) = pending {
                let decision = match outcome {
                    ApprovalOutcome::AllowedOnce => crate::workflow::ApprovalDecision::Approve,
                    ApprovalOutcome::Rejected | ApprovalOutcome::Unavailable => {
                        crate::workflow::ApprovalDecision::Reject
                    }
                    ApprovalOutcome::Cancelled => crate::workflow::ApprovalDecision::Cancel,
                };
                let _ = self.store.resolve_approval(
                    &self.workflow_id,
                    &approval_id,
                    decision,
                    Some("terminal"),
                    &arguments_hash,
                );
            }
            Ok(outcome)
        })
    }
}

impl PersistedApprovalProvider {
    /// Mirror the decision into the run snapshot. A cancelled approval ends the
    /// run; a denied one only drops that tool call.
    fn resolve_run_approval(&self, outcome: ApprovalOutcome) {
        let Some(run) = &self.run else {
            return;
        };
        let _ = match outcome {
            ApprovalOutcome::AllowedOnce => run
                .store
                .resolve_pending_tool(&run.run_id, ApprovalResolution::Granted),
            ApprovalOutcome::Rejected | ApprovalOutcome::Unavailable => run
                .store
                .resolve_pending_tool(&run.run_id, ApprovalResolution::Denied),
            ApprovalOutcome::Cancelled => run.store.cancel(&run.run_id, "approval_cancelled"),
        };
    }
}

/// Static inputs shared by every Plan command.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct PlanConfig {
    pub workspace: PathBuf,
    pub model: String,
    pub plan: Option<PathBuf>,
    pub max_tokens: Option<u64>,
    pub read_only: bool,
}

impl PlanConfig {
    pub fn new(workspace: PathBuf) -> Self {
        Self {
            workspace,
            model: "deepseek-flash".to_owned(),
            plan: None,
            max_tokens: None,
            read_only: false,
        }
    }
}

/// One Plan command, with its user-provided argument.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum PlanCommand {
    Plan(String),
    Answer(String),
    Scaffold,
    Continue,
    Test,
    Fix,
    Execute,
    Distill(String),
    Approve,
    Status(Option<String>),
    Next,
    Workflow(Option<String>),
    Reclassify(String),
    Hint(String),
}

impl PlanCommand {
    /// Short label used in progress messages.
    pub fn label(&self) -> &'static str {
        match self {
            Self::Plan(_) => "plan",
            Self::Answer(_) => "answer",
            Self::Scaffold => "scaffold",
            Self::Continue => "continue",
            Self::Test => "test",
            Self::Fix => "fix",
            Self::Execute => "execute",
            Self::Distill(_) => "distill",
            Self::Approve => "approve",
            Self::Status(_) => "status",
            Self::Next => "next",
            Self::Workflow(_) => "workflow",
            Self::Reclassify(_) => "reclassify",
            Self::Hint(_) => "hint",
        }
    }

    /// Whether this command can spend model tokens.
    #[must_use]
    pub fn uses_model(&self) -> bool {
        matches!(
            self,
            Self::Plan(_)
                | Self::Answer(_)
                | Self::Scaffold
                | Self::Fix
                | Self::Distill(_)
                | Self::Hint(_)
        )
    }
}

/// Split the optional `--new` flag from a `/plan` task.
///
/// `--new` means "pause the active workflow and start over". It is a leading
/// flag only, so a task that merely contains `--new` later is left alone.
#[must_use]
pub fn split_new_flag(task: &str) -> (bool, &str) {
    let trimmed = task.trim_start();
    let Some(rest) = trimmed.strip_prefix("--new") else {
        return (false, task);
    };
    if rest.chars().next().is_none_or(char::is_whitespace) {
        (true, rest.trim_start())
    } else {
        (false, task)
    }
}

/// Result of one Plan command.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct PlanOutcome {
    pub path: PathBuf,
    pub output: String,
    pub workflow_id: Option<String>,
}

/// Session ownership for one Plan command.
///
/// When the caller supplies a Session, every model command in the run reuses
/// it, so Plan turns share the user's conversation transcript. Without a
/// Session, commands keep the historical ephemeral behaviour used by
/// one-shot CLI runs.
struct PlanSession {
    session: Option<Session>,
    reuse: bool,
}

impl PlanSession {
    fn new(session: Option<Session>) -> Self {
        Self {
            reuse: session.is_some(),
            session,
        }
    }

    /// Persist the human text of a Plan command before any gate or parameter
    /// validation runs.
    ///
    /// The text is what the user typed, not what the command did. Recording it
    /// here keeps the session honest when the command is rejected, and the UI
    /// event stream renders it even though no model turn starts. Model turns
    /// must not append the same text again.
    async fn record_human(&mut self, prefix: &'static str, text: &str) -> Result<(), BoxError> {
        let text = text.trim();
        if text.is_empty() {
            return Ok(());
        }
        if self.session.is_none() {
            self.session = Some(Session::new(format!("{prefix}{}", now_unix_millis()?))?);
        }
        let message = Message::user(
            next_human_message_id()?,
            vec![ContentBlock::text(text)?],
            MessageSource::user()?,
        )?;
        self.session
            .as_mut()
            .expect("session was created above")
            .append_settled(NewEvent::surface(
                EventKind::user_message(message),
                SurfaceIntent::append(),
            ))
            .await?;
        Ok(())
    }

    fn take_or_create(&mut self, prefix: &'static str) -> Result<Session, BoxError> {
        match self.session.take() {
            Some(session) => Ok(session),
            None if self.reuse => Err(invalid("plan session is already in use")),
            None => Ok(Session::new(format!("{prefix}{}", now_unix_millis()?))?),
        }
    }

    fn put(&mut self, session: Session) {
        self.session = Some(session);
    }

    fn into_session(self) -> Option<Session> {
        if self.reuse { self.session } else { None }
    }
}

fn next_human_message_id() -> Result<String, BoxError> {
    static COUNTER: AtomicU64 = AtomicU64::new(0);
    Ok(format!(
        "plan-human-{}-{}",
        now_unix_millis()?,
        COUNTER.fetch_add(1, Ordering::Relaxed)
    ))
}

struct PlanAgentPolicy {
    system: &'static str,
    profile: CapabilityProfile,
    registry: RegistryMode,
    file_change: FileChangePolicy,
    session_prefix: &'static str,
    approval: Option<Arc<dyn ApprovalProvider>>,
    workflow_id: Option<String>,
    audit_sink: Option<Arc<std::sync::Mutex<Vec<serde_json::Value>>>>,
    max_steps_per_turn: Option<usize>,
}

/// Run one Plan command.
///
/// `approval` is `Some` when the caller runs with approvals enabled. Only
/// `/scaffold` and `/execute` consult it; the other commands are read-only and
/// deny mutations unconditionally.
pub async fn run_command(
    command: PlanCommand,
    config: &PlanConfig,
    provider: Arc<dyn ModelProvider>,
    approval: Option<Arc<dyn ApprovalProvider>>,
    cancellation: CancellationToken,
) -> Result<PlanOutcome, BoxError> {
    let mut session = None;
    run_command_with_session(
        command,
        config,
        provider,
        approval,
        cancellation,
        &mut session,
    )
    .await
}

/// Run one Plan command, optionally reusing a caller-owned Session.
///
/// `session` is replaced with the same Session after the command, even when the
/// command fails, so the caller can restore its conversation transcript.
pub async fn run_command_with_session(
    command: PlanCommand,
    config: &PlanConfig,
    provider: Arc<dyn ModelProvider>,
    approval: Option<Arc<dyn ApprovalProvider>>,
    cancellation: CancellationToken,
    session: &mut Option<Session>,
) -> Result<PlanOutcome, BoxError> {
    let mut sessions = PlanSession::new(session.take());
    let result = run_command_inner(
        command,
        config,
        provider,
        approval,
        cancellation,
        &mut sessions,
    )
    .await;
    *session = sessions.into_session();
    result
}

/// Human text and session prefix for commands that carry typed input.
///
/// Used when a pre-dispatch gate blocks the command before its own handler can
/// record the text.
fn command_human_text(command: &PlanCommand) -> Option<(&'static str, &str)> {
    match command {
        PlanCommand::Plan(task) => {
            let (_, task) = split_new_flag(task);
            Some(("slowcode-plan-", task))
        }
        PlanCommand::Answer(answer) => Some(("slowcode-answer-", answer)),
        PlanCommand::Distill(explanation) => Some(("slowcode-distill-", explanation)),
        PlanCommand::Hint(question) => Some(("slowcode-hint-", question)),
        _ => None,
    }
}

async fn run_command_inner(
    command: PlanCommand,
    config: &PlanConfig,
    provider: Arc<dyn ModelProvider>,
    approval: Option<Arc<dyn ApprovalProvider>>,
    cancellation: CancellationToken,
    sessions: &mut PlanSession,
) -> Result<PlanOutcome, BoxError> {
    let store = WorkflowStore::new(&config.workspace);
    let active = store.load_active().ok().flatten();
    let may_write = matches!(
        command,
        PlanCommand::Scaffold | PlanCommand::Fix | PlanCommand::Execute
    );
    let uses_model = command.uses_model();
    let operation = command.label();
    let creates_workflow = matches!(&command, PlanCommand::Plan(_));
    let record_run = !matches!(
        command,
        PlanCommand::Status(_) | PlanCommand::Next | PlanCommand::Workflow(_)
    );
    // A model command must not start once the workflow's total token budget
    // is already spent. `/plan` has no active workflow yet and is checked
    // after it creates one.
    if uses_model && let Some(state) = &active {
        let budget = load_budget(&config.workspace)?;
        if let Some(reason) = budget_exhausted(state, &budget) {
            if let Some((prefix, text)) = command_human_text(&command) {
                sessions.record_human(prefix, text).await?;
            }
            return block_for_budget(&store, state, &reason, "");
        }
    }
    // Run-level journal sits beside the workflow journal. A run links back by
    // workflow id, but the runtime never reads the workflow state. Starting a
    // run fails closed; finishing it is best effort so a successful command is
    // not lost when only the audit write fails.
    //
    // `/plan` creates its own workflow, so a run cannot be linked up front. It
    // is backfilled after success. Starting one under the currently active
    // workflow would record a rejected `/plan` against the wrong workflow.
    let run = if record_run && !creates_workflow {
        match &active {
            Some(state) => {
                let runs = RunStore::new(
                    config
                        .workspace
                        .join(".scratch")
                        .join("workflows")
                        .join(&state.workflow_id)
                        .join("run-states"),
                );
                let started = runs.start(
                    RunLink {
                        workflow_id: Some(state.workflow_id.clone()),
                        operation: operation.to_owned(),
                    },
                    8,
                )?;
                let run_id = started.run_id;
                runs.transition(&run_id, RunStatus::CompilingContext, serde_json::json!({}))?;
                Some((runs, run_id))
            }
            None => None,
        }
    } else {
        None
    };
    let recorder = run.as_ref().map(|(store, run_id)| RunRecorder {
        store: store.clone(),
        run_id: run_id.clone(),
    });
    // The provider needs the run handle so a pending tool is durable before
    // the prompt appears. That is why the run starts before the wrap.
    let approval = match (approval, &active, recorder.as_ref()) {
        (Some(inner), Some(state), Some(run)) => Some(Arc::new(PersistedApprovalProvider {
            inner,
            store: store.clone(),
            workflow_id: state.workflow_id.clone(),
            run: Some(run.clone()),
        }) as Arc<dyn ApprovalProvider>),
        (approval, _, _) => approval,
    };
    // The Plan commands produce large futures. Boxing here keeps the 2 MB
    // test-thread stack safe, matching `run_fix` and `run_answer`.
    let dispatch = Box::pin(async {
        match command {
            PlanCommand::Plan(task) => {
                run_plan_with_session(config, provider, &task, cancellation.clone(), sessions).await
            }
            PlanCommand::Answer(answer) => {
                run_answer_with_session(config, provider, &answer, cancellation.clone(), sessions)
                    .await
            }
            PlanCommand::Scaffold => {
                run_scaffold_with_session(
                    config,
                    provider,
                    approval,
                    cancellation.clone(),
                    sessions,
                )
                .await
            }
            PlanCommand::Continue => run_continue(config).await,
            PlanCommand::Test => run_test(config).await,
            PlanCommand::Fix => {
                run_fix_with_session(config, provider, approval, cancellation.clone(), sessions)
                    .await
            }
            PlanCommand::Execute => {
                run_execute_with_session(config, provider, approval, cancellation.clone(), sessions)
                    .await
            }
            PlanCommand::Distill(explanation) => {
                run_distill_with_session(
                    config,
                    provider,
                    &explanation,
                    cancellation.clone(),
                    sessions,
                )
                .await
            }
            PlanCommand::Approve => run_approve(config).await,
            PlanCommand::Status(workflow_id) => run_status(config, workflow_id).await,
            PlanCommand::Next => run_next(config).await,
            PlanCommand::Workflow(argument) => run_workflow(config, argument).await,
            PlanCommand::Reclassify(text) => run_reclassify(config, &text).await,
            PlanCommand::Hint(question) => {
                run_hint_with_session(config, provider, &question, cancellation.clone(), sessions)
                    .await
            }
        }
    });
    let mut result = match recorder {
        Some(recorder) => ACTIVE_RUN.scope(recorder, dispatch).await,
        None => dispatch.await,
    };
    // A turn can spend the final tokens of the budget. Block the workflow
    // after the command so the next model call cannot start.
    if uses_model
        && result.is_ok()
        && let Ok(Some(state)) = store.load_active()
        && state.status == WorkflowStatus::Active
    {
        let budget = load_budget(&config.workspace)?;
        if let Some(reason) = budget_exhausted(&state, &budget) {
            let detail = result
                .as_ref()
                .map(|outcome| outcome.output.clone())
                .unwrap_or_default();
            result = Ok(block_for_budget(&store, &state, &reason, &detail)?);
        }
    }
    let cancelled = cancellation.is_cancelled()
        || result
            .as_ref()
            .err()
            .is_some_and(|error| error.to_string().contains("cancelled"));
    if let Some((runs, run_id)) = &run {
        let _ = if cancelled {
            runs.cancel(run_id, "cancelled")
        } else if result.is_ok() {
            // An allowed tool leaves the run in `ExecutingTool`; return to
            // `CompilingContext` before closing it as completed.
            if runs
                .load(run_id)
                .is_ok_and(|state| state.status == RunStatus::ExecutingTool)
            {
                let _ = runs.transition(
                    run_id,
                    RunStatus::CompilingContext,
                    serde_json::json!({ "reason": "tool_finished" }),
                );
            }
            runs.transition(run_id, RunStatus::Completed, serde_json::json!({}))
        } else {
            let detail = match result.as_ref().err() {
                Some(error) => serde_json::json!({
                    "reason": "command_failed",
                    "error": error.to_string(),
                }),
                None => serde_json::json!({ "reason": "command_failed" }),
            };
            runs.transition(run_id, RunStatus::Failed, detail)
        };
    }
    if let Some(state) = active
        && cancelled
    {
        let turn_id = format!("turn-{}", state.last_seq.saturating_add(1));
        let _ = store.cancel_run(&state.workflow_id, &turn_id, "cancelled", may_write);
    }
    // `/plan` creates its workflow inside the command, so no run could be
    // started up front. Backfill one once the workflow id exists. This is an
    // audit record of an already successful command, so a journal failure is
    // ignored rather than turning success into failure. The transition payload
    // carries `backfilled: true` because timestamps are written after the fact.
    if run.is_none()
        && record_run
        && creates_workflow
        && let Ok(outcome) = &result
        && let Some(workflow_id) = outcome.workflow_id.as_deref()
    {
        record_backfilled_plan_run(config, workflow_id);
    }
    result
}

fn record_backfilled_plan_run(config: &PlanConfig, workflow_id: &str) {
    let runs = RunStore::new(
        config
            .workspace
            .join(".scratch")
            .join("workflows")
            .join(workflow_id)
            .join("run-states"),
    );
    let Ok(started) = runs.start(
        RunLink {
            workflow_id: Some(workflow_id.to_owned()),
            operation: "plan".to_owned(),
        },
        8,
    ) else {
        return;
    };
    let run_id = started.run_id;
    let detail = serde_json::json!({
        "workflow_id": workflow_id,
        "backfilled": true,
    });
    if runs
        .transition(&run_id, RunStatus::CompilingContext, detail.clone())
        .is_ok()
    {
        let _ = runs.transition(&run_id, RunStatus::Completed, detail);
    }
}

/// Run one `/plan` turn in a read-only agent and persist the result.
async fn run_plan_with_session(
    config: &PlanConfig,
    provider: Arc<dyn ModelProvider>,
    task: &str,
    cancellation: CancellationToken,
    sessions: &mut PlanSession,
) -> Result<PlanOutcome, BoxError> {
    let (start_new, task) = split_new_flag(task);
    let task = task.trim().to_owned();
    if task.is_empty() {
        return Err(invalid("usage: /plan [--new] <task>"));
    }
    sessions.record_human("slowcode-plan-", &task).await?;
    let store = WorkflowStore::new(&config.workspace);
    if start_new {
        store.pause_active()?;
    }
    if let Err(error) = store.ensure_no_active() {
        return Err(plan_conflict_error(&store, error));
    }

    let audit_sink = Arc::new(std::sync::Mutex::new(Vec::new()));
    let plan_config = config.clone();
    let plan_task = task.clone();
    with_plan_agent(
        sessions,
        config,
        provider,
        PlanAgentPolicy {
            system: PLAN_SYSTEM_PROMPT,
            profile: CapabilityProfile::Navigator,
            registry: RegistryMode::ReadOnly,
            file_change: FileChangePolicy::Deny,
            session_prefix: "slowcode-plan-",
            approval: None,
            workflow_id: None,
            audit_sink: Some(Arc::clone(&audit_sink)),
            max_steps_per_turn: Some(MAX_AGENT_STEPS_PER_TURN),
        },
        move |agent| {
            Box::pin(async move {
                let config = &plan_config;
                let task = plan_task.as_str();
                let (mut output, mut usage) =
                    run_task_turn_usage(agent, task, 1, config, &cancellation).await?;
                let mut validation = validate_plan_output(&output);
                if !validation.is_valid() {
                    let repair = repair_prompt(&validation.reasons);
                    let (repaired, repair_usage) =
                        run_text_turn_usage(agent, &repair, 2, config, &cancellation).await?;
                    usage.model_calls += repair_usage.model_calls;
                    usage.output_tokens += repair_usage.output_tokens;
                    output = repaired;
                    validation = validate_plan_output(&output);
                }

                let path = write_plan_artifact(config, task, &output, &validation)?;
                if !validation.is_valid() {
                    return Err(invalid(format!(
                        "/plan output did not satisfy the contract: {}\n\n{output}",
                        validation.reasons.join("; "),
                    )));
                }
                let tasks = workflow_tasks(&parse_task_table(&output), &[]);
                let questions = workflow_questions(&tasks, &parse_question_table(&output));
                let state = store.create_with_questions(task, None, tasks, questions)?;
                let state = store.record_model_usage(
                    &state.workflow_id,
                    usage.model_calls,
                    usage.output_tokens,
                )?;
                flush_shell_audit(&store, &state.workflow_id, &audit_sink)?;
                record_manifest(
                    &config.workspace,
                    &state.workflow_id,
                    Phase::Classify,
                    CapabilityProfile::Navigator,
                )?;
                let output = format!(
                    "{output}\n\n[workflow: {} phase: {}]",
                    state.workflow_id, state.phase
                );
                Ok(PlanOutcome {
                    path,
                    output,
                    workflow_id: Some(state.workflow_id),
                })
            })
        },
    )
    .await
}

/// Turn the generic "active workflow" error into a message that names the
/// workflow, its task and the number of open questions, and points at --new.
fn plan_conflict_error(store: &WorkflowStore, error: WorkflowError) -> BoxError {
    if let WorkflowError::ActiveExists(id) = &error
        && let Ok(state) = store.load(id)
    {
        return invalid(format!(
            "workflow {id} is still active ({} open question(s), task: {}); \
             run /workflow end, /workflow use <id>, or /plan --new <task> to pause it and start over",
            state.open_questions().len(),
            state.task
        ));
    }
    error.into()
}

/// `/answer` records decisions and reclassifies the plan.
fn run_answer_with_session<'a>(
    config: &'a PlanConfig,
    provider: Arc<dyn ModelProvider>,
    answer: &'a str,
    cancellation: CancellationToken,
    sessions: &'a mut PlanSession,
) -> PlanOutcomeFuture<'a> {
    Box::pin(async move {
        let human_recorded = Arc::new(AtomicBool::new(false));
        let outcome = run_answer_body(
            config,
            provider,
            answer,
            cancellation,
            sessions,
            Arc::clone(&human_recorded),
        )
        .await;
        if outcome.is_err() && !human_recorded.load(Ordering::Relaxed) {
            sessions.record_human("slowcode-answer-", answer).await?;
        }
        outcome
    })
}

fn run_answer_body<'a>(
    config: &'a PlanConfig,
    provider: Arc<dyn ModelProvider>,
    answer: &'a str,
    cancellation: CancellationToken,
    sessions: &'a mut PlanSession,
    human_recorded: Arc<AtomicBool>,
) -> PlanOutcomeFuture<'a> {
    Box::pin(async move {
        let argument = parse_answer_argument(answer)?;
        let store = WorkflowStore::new(&config.workspace);
        let active = store.require_active()?;
        if !matches!(
            active.phase,
            Phase::Interview | Phase::Review | Phase::Reclassify | Phase::Distill
        ) {
            return Err(invalid(format!(
                "/answer is not allowed in phase {}",
                active.phase
            )));
        }
        let path = resolve_plan_path(config)?;
        let artifact = fs::read_to_string(&path)?;
        let requested_question = argument.question_id.clone();
        let (answer_text, source, snapshot) = match argument.file {
            Some(file) => {
                if !argument.text.is_empty() {
                    return Err(invalid(
                        "mixed text and @file is not supported; use text or one file",
                    ));
                }
                let input = read_answer_file(
                    &config.workspace,
                    &active.workflow_id,
                    &file,
                    SecretPolicy::Reject,
                )?;
                (input.text, AnswerSource::File, Some(input.snapshot))
            }
            None => (argument.text, AnswerSource::Text, None),
        };
        if answer_text.trim().is_empty() {
            return Err(invalid("usage: /answer <answer> or /answer @<file>"));
        }
        sessions
            .record_human("slowcode-answer-", &answer_text)
            .await?;
        human_recorded.store(true, Ordering::Relaxed);
        if active.phase == Phase::Distill {
            if !active
                .distill
                .as_ref()
                .is_some_and(|distill| distill.reported)
            {
                return Err(invalid(
                    "/answer in distill phase needs a report; run /distill first",
                ));
            }
            return answer_distill_question(
                &store,
                &active,
                requested_question.as_deref(),
                &answer_text,
                source,
                snapshot,
                &path,
                &artifact,
            );
        }
        let tasks = effective_tasks(&artifact)?;
        let question = match argument.question_id {
            Some(question_id) => Some(
                active
                    .questions
                    .iter()
                    .find(|question| {
                        question.question_id == question_id
                            && question.status == QuestionStatus::Open
                    })
                    .cloned()
                    .ok_or_else(|| invalid(format!("no open question `{question_id}`")))?,
            ),
            None => active
                .open_questions()
                .first()
                .map(|question| (*question).clone()),
        };

        if let Some(question) = &question
            && question.kind == QuestionKind::TestSelector
        {
            let project = detect_project(&config.workspace).ok_or_else(|| {
                invalid("test selector needs a Cargo package or pytest configuration")
            })?;
            let selector = resolve_selector_answer(&question.options, &answer_text);
            validate_selector(&project, &selector)
                .map_err(|error| invalid(format!("invalid test selector: {error}")))?;
            let state = store.answer_test_selector(
                &active.workflow_id,
                Some(&question.question_id),
                &selector,
                source,
            )?;
            let body = format!(
                "### 测试选择器\n\n{selector}\n\n### 下一步\n\n{}",
                answer_next_step(&state)
            );
            let heading = format!("决策记录 {}", now_unix()?);
            let updated = append_section(&artifact, &heading, &body);
            let updated = set_artifact_status(&updated, "answered");
            fs::write(&path, updated)?;
            return Ok(PlanOutcome {
                path,
                output: body,
                workflow_id: Some(state.workflow_id),
            });
        }

        let profile = active
            .phase
            .capability_profile()
            .unwrap_or(CapabilityProfile::Navigator);
        record_manifest(
            &config.workspace,
            &active.workflow_id,
            active.phase,
            profile,
        )?;
        let plan_config = config.clone();
        with_plan_agent(
            sessions,
            config,
            provider,
            PlanAgentPolicy {
                system: ANSWER_SYSTEM_PROMPT,
                profile,
                registry: RegistryMode::ReadOnly,
                file_change: FileChangePolicy::Deny,
                session_prefix: "slowcode-answer-",
                approval: None,
                workflow_id: Some(active.workflow_id.clone()),
                audit_sink: None,
                max_steps_per_turn: None,
            },
        move |agent| Box::pin(async move {
            let config = &plan_config;
            if let Some(question) = question {
                if question.kind == QuestionKind::Unknown {
                    return answer_unknown_question(
                        &store,
                        &active,
                        question,
                        &answer_text,
                        source,
                        snapshot,
                        &path,
                        &artifact,
                    );
                }
                return answer_model_question(
                    agent,
                    config,
                    &store,
                    &active,
                    question,
                    &answer_text,
                    source,
                    snapshot,
                    &path,
                    &artifact,
                    &tasks,
                    &cancellation,
                )
                .await;
            }

            let prompt = answer_user_prompt(&artifact, &answer_text);
            let mut output =
                run_human_turn(agent, &prompt, &answer_text, 1, config, &cancellation).await?;
            let mut decisions = parse_decision_tables(&output);
            let mut validation = validate_decisions(&decisions);
            if !validation.is_valid() {
                let repair = decision_repair(&validation.reasons);
                output = run_text_turn(agent, &repair, 2, config, &cancellation).await?;
                decisions = parse_decision_tables(&output);
                validation = validate_decisions(&decisions);
            }

            let effective = apply_decisions(&tasks, &decisions);
            let body = match &effective {
                Ok(effective) => format!(
                    "### 用户回答\n\n{answer_text}\n\n### 决策表\n\n{output}\n\n### 有效状态\n\n{}",
                    render_task_bullets(effective)
                ),
                Err(error) => {
                    format!(
                        "### 用户回答\n\n{answer_text}\n\n### 决策表\n\n{output}\n\n### 错误\n\n{error}"
                    )
                }
            };
            let heading = format!("决策记录 {}", now_unix()?);
            let updated = append_section(&artifact, &heading, &body);
            let updated = set_artifact_status(
                &updated,
                if validation.is_valid() && effective.is_ok() {
                    "answered"
                } else {
                    "answer-failed"
                },
            );
            fs::write(&path, updated)?;

            if !validation.is_valid() || effective.is_err() {
                let reasons = if !validation.is_valid() {
                    validation.reasons.join("; ")
                } else {
                    effective.as_ref().err().cloned().unwrap_or_default()
                };
                return Err(invalid(format!(
                    "/answer did not produce a valid reclassification: {reasons}\n\n{body}"
                )));
            }
            let tasks = effective
                .as_ref()
                .map(|tasks| workflow_tasks(tasks, &active.tasks))
                .unwrap_or_default();
            let mut update = AnswerUpdate::new(answer_text, source);
            update.tasks = Some(tasks);
            update.snapshot = snapshot;
            let state = store.apply_answer(&active.workflow_id, update)?;
            Ok(PlanOutcome {
                path,
                output: body,
                workflow_id: Some(state.workflow_id),
            })
        }),
    )
    .await
    })
}

#[allow(clippy::too_many_arguments)]
fn answer_distill_question(
    store: &WorkflowStore,
    active: &WorkflowState,
    question_id: Option<&str>,
    answer_text: &str,
    source: AnswerSource,
    snapshot: Option<AnswerSnapshot>,
    path: &Path,
    artifact: &str,
) -> Result<PlanOutcome, BoxError> {
    let decision = if answer_text.contains("接受剩余缺口") {
        DistillDecision::AcceptGaps
    } else if answer_text.contains("重新分类") || answer_text.contains("未知域") {
        DistillDecision::Reclassify
    } else {
        DistillDecision::Answered
    };
    let follow_up = (decision == DistillDecision::Reclassify)
        .then(|| knowledge_follow_up_for(None, "Distill 追问".to_owned()));
    let mut update = DistillAnswerUpdate::new(decision, answer_text.to_owned(), source);
    update.question_id = question_id.map(str::to_owned);
    update.snapshot = snapshot;
    update.follow_up = follow_up;
    let state = store.answer_distill_follow_up(&active.workflow_id, update)?;
    let result = match decision {
        DistillDecision::Answered => "追问已回答，workflow 完成。",
        DistillDecision::Reclassify => {
            "追问暴露新的未知域，已回到 Reclassify；先回答新的知识问题。"
        }
        DistillDecision::AcceptGaps => "已显式接受剩余缺口，workflow 完成。",
    };
    let body = format!(
        "### 用户回答\n\n{answer_text}\n\n### 结果\n\n{result}\n\n### 下一步\n\n{}",
        answer_next_step(&state)
    );
    let heading = format!("Distill 追问 {}", now_unix()?);
    let updated = append_section(artifact, &heading, &body);
    let updated = set_artifact_status(
        &updated,
        if state.phase == Phase::Done {
            "done"
        } else {
            "distilled"
        },
    );
    fs::write(path, updated)?;
    Ok(PlanOutcome {
        path: path.to_owned(),
        output: body,
        workflow_id: Some(state.workflow_id),
    })
}

#[allow(clippy::too_many_arguments)]
fn answer_unknown_question(
    store: &WorkflowStore,
    active: &WorkflowState,
    question: Question,
    answer_text: &str,
    source: AnswerSource,
    snapshot: Option<AnswerSnapshot>,
    path: &Path,
    artifact: &str,
) -> Result<PlanOutcome, BoxError> {
    let wants_knowledge = answer_text.contains("补充知识");
    let wants_core = answer_text.contains("自己实现");
    let wants_continue =
        answer_text.contains("继续按已知域修复") || answer_text.contains("继续修复");
    let choices = [wants_knowledge, wants_core, wants_continue]
        .into_iter()
        .filter(|wanted| *wanted)
        .count();
    if choices != 1 {
        return Err(invalid(
            "请回答选项之一：补充知识并转已知域 / 我自己实现核心 / 继续按已知域修复",
        ));
    }
    let mut tasks = active.tasks.clone();
    let mut follow_up = None;
    let mut next_phase = None;
    let label = if wants_continue {
        next_phase = Some(Phase::TestFix);
        "继续按已知域修复：已回到 TestFix，先 /fix 或 /test"
    } else if let Some(task_id) = question.task_id.clone() {
        let task = tasks
            .iter_mut()
            .find(|task| task.task_id == task_id)
            .ok_or_else(|| invalid(format!("question references missing task `{task_id}`")))?;
        if wants_knowledge {
            task.strategy = Strategy::AiImplements;
            task.exec_state = ExecState::PendingDecision;
            follow_up = Some(knowledge_follow_up(
                task.task_id.clone(),
                task.title.clone(),
            ));
            "补充知识：下一步请给出缺失知识或规则来源"
        } else {
            task.strategy = Strategy::UserImplementsCore;
            task.exec_state = ExecState::PendingUser;
            next_phase = Some(Phase::Review);
            "自己实现核心：分类已记录，批准后进入脚手架"
        }
    } else if wants_knowledge {
        follow_up = Some(knowledge_follow_up_for(None, "未知域失败".to_owned()));
        "补充知识：下一步请给出缺失知识或规则来源"
    } else {
        next_phase = Some(Phase::Review);
        "自己实现核心：分类已记录，批准后继续"
    };
    let mut update = AnswerUpdate::new(answer_text.to_owned(), source);
    update.question_id = Some(question.question_id);
    update.snapshot = snapshot;
    update.tasks = Some(tasks);
    update.follow_up = follow_up;
    update.next_phase = next_phase;
    let state = store.apply_answer(&active.workflow_id, update)?;
    let body = format!(
        "### 用户选择\n\n{answer_text}\n\n### 结果\n\n{label}\n\n### 下一步\n\n{}",
        answer_next_step(&state)
    );
    let heading = format!("决策记录 {}", now_unix()?);
    let updated = append_section(artifact, &heading, &body);
    let updated = set_artifact_status(&updated, "answered");
    fs::write(path, updated)?;
    Ok(PlanOutcome {
        path: path.to_owned(),
        output: body,
        workflow_id: Some(state.workflow_id),
    })
}

#[allow(clippy::too_many_arguments)]
async fn answer_model_question(
    agent: &mut AgentLoop,
    config: &PlanConfig,
    store: &WorkflowStore,
    active: &WorkflowState,
    question: Question,
    answer_text: &str,
    source: AnswerSource,
    snapshot: Option<AnswerSnapshot>,
    path: &Path,
    artifact: &str,
    tasks: &[CognitiveTask],
    cancellation: &CancellationToken,
) -> Result<PlanOutcome, BoxError> {
    let prompt = question_answer_user_prompt(
        artifact,
        &question.question_id,
        &question.prompt,
        &question.options,
        &question.criteria,
        answer_text,
    );
    let mut output = run_human_turn(agent, &prompt, answer_text, 1, config, cancellation).await?;
    let mut validation = validate_answer_verdict(&output, &question.question_id);
    let mut verdict = parse_answer_verdict(&output);
    if !validation.is_valid() || verdict.is_none() {
        let repair = answer_verdict_repair_prompt(&validation.reasons);
        output = run_text_turn(agent, &repair, 2, config, cancellation).await?;
        validation = validate_answer_verdict(&output, &question.question_id);
        verdict = parse_answer_verdict(&output);
    }
    if !validation.is_valid() {
        return Err(invalid(format!(
            "/answer did not satisfy the verdict contract: {}",
            validation.reasons.join("; ")
        )));
    }
    let verdict = verdict.ok_or_else(|| invalid("missing `## 回答判定` table"))?;
    if verdict.verdict == AnswerVerdictKind::Insufficient {
        let gap = if verdict.gap.is_empty() {
            "需要补充更具体的回答".to_owned()
        } else {
            verdict.gap
        };
        let mut update = AnswerUpdate::new(answer_text.to_owned(), source);
        let current_question = question.question_id.clone();
        update.question_id = Some(current_question.clone());
        update.snapshot = snapshot;
        update.insufficient = Some(gap.clone());
        let state = store.apply_answer(&active.workflow_id, update)?;
        let attempts = state
            .questions
            .iter()
            .find(|question| question.question_id == current_question)
            .map_or(0, |question| question.attempts);
        let mut body = format!("回答不足：{gap}\n已尝试 {attempts} 次。");
        if attempts >= 2 {
            body.push_str(
                "\n可以改写回答、用 /answer #<id> @file 提交文件，或 /workflow end 后重新开始。",
            );
        }
        return Ok(PlanOutcome {
            path: path.to_owned(),
            output: body,
            workflow_id: Some(state.workflow_id),
        });
    }

    let mut decisions = parse_decision_tables(&output);
    let mut decision_validation = validate_decisions(&decisions);
    if !decision_validation.is_valid() {
        let repair = decision_repair(&decision_validation.reasons);
        let repaired = run_text_turn(agent, &repair, 3, config, cancellation).await?;
        decisions = parse_decision_tables(&repaired);
        decision_validation = validate_decisions(&decisions);
        if !decision_validation.is_valid() {
            return Err(invalid(format!(
                "/answer did not produce a valid decision table: {}",
                decision_validation.reasons.join("; ")
            )));
        }
        output = repaired;
    }
    let effective = apply_decisions(tasks, &decisions).map_err(invalid)?;
    let workflow_tasks = workflow_tasks(&effective, &active.tasks);
    let mut update = AnswerUpdate::new(answer_text.to_owned(), source);
    update.question_id = Some(question.question_id.clone());
    update.snapshot = snapshot;
    update.tasks = Some(workflow_tasks);
    let state = store.apply_answer(&active.workflow_id, update)?;
    let body = format!(
        "### 用户回答\n\n{answer_text}\n\n### 回答判定\n\nsufficient\n\n### 决策表\n\n{output}\n\n### 有效状态\n\n{}\n\n### 下一步\n\n{}",
        render_task_bullets(&effective),
        answer_next_step(&state)
    );
    let heading = format!("决策记录 {}", now_unix()?);
    let updated = append_section(artifact, &heading, &body);
    let updated = set_artifact_status(&updated, "answered");
    fs::write(path, updated)?;
    Ok(PlanOutcome {
        path: path.to_owned(),
        output: body,
        workflow_id: Some(state.workflow_id),
    })
}

/// `/scaffold` writes scaffolds for tasks still marked 待用户实现.
async fn run_scaffold_with_session(
    config: &PlanConfig,
    provider: Arc<dyn ModelProvider>,
    approval: Option<Arc<dyn ApprovalProvider>>,
    cancellation: CancellationToken,
    sessions: &mut PlanSession,
) -> Result<PlanOutcome, BoxError> {
    if config.read_only {
        return Err(invalid("/scaffold cannot run with --read-only"));
    }
    let store = WorkflowStore::new(&config.workspace);
    let active = store.require_active()?;
    active.can_enter_scaffold()?;
    if !active.allows_workspace_write() {
        return Err(invalid(format!(
            "/scaffold cannot write in phase {}",
            active.phase
        )));
    }
    let path = resolve_plan_path(config)?;
    let artifact = fs::read_to_string(&path)?;
    let tasks = effective_tasks(&artifact)?;
    let targets = scaffold_tasks(&tasks);
    if targets.is_empty() {
        return Err(invalid(
            "no 待用户实现 tasks to scaffold; run /answer or check the plan",
        ));
    }
    let project = detect_project(&config.workspace).ok_or_else(|| {
        invalid(
            "/scaffold needs a project test command; add a Cargo package or pytest configuration",
        )
    })?;
    let selector = match derive_selector(
        &config.workspace,
        &active.tasks,
        &project,
        active.test_selector.as_deref(),
    ) {
        SelectorChoice::Resolved(selector) => selector,
        SelectorChoice::NeedsUser { reason, .. } => {
            return Err(invalid(format!(
                "/scaffold needs a resolved test selector: {reason}; run /approve first"
            )));
        }
    };
    let baseline = test_exec::run(&config.workspace, &project, &selector)
        .await
        .map_err(|error| invalid(format!("scaffold baseline could not run: {error}")))?;
    let baseline_snapshot = snapshot_workspace(&config.workspace)?;

    let (file_change, _) = write_policies(&approval);
    record_manifest(
        &config.workspace,
        &active.workflow_id,
        active.phase,
        CapabilityProfile::Driver,
    )?;
    let plan_config = config.clone();
    with_plan_agent(
        sessions,
        config,
        provider,
        PlanAgentPolicy {
            system: SCAFFOLD_SYSTEM_PROMPT,
            profile: CapabilityProfile::Driver,
            registry: RegistryMode::Workspace,
            file_change,
            session_prefix: "slowcode-scaffold-",
            approval: approval.clone(),
            workflow_id: Some(active.workflow_id.clone()),
            audit_sink: None,
            max_steps_per_turn: None,
        },
        move |agent| Box::pin(async move {
            let config = &plan_config;
            let output = run_text_turn(
                agent,
                &scaffold_user_prompt(&artifact, &targets),
                1,
                config,
                &cancellation,
            )
            .await?;
            let entries = parse_scaffold_manifest(&output);
            if entries.is_empty() {
                return Err(invalid(format!(
                    "scaffold output did not contain a valid `## Scaffold manifest` table\n\n{output}"
                )));
            }

            let mut files = Vec::new();
            let mut manifest = Vec::new();
            for entry in &entries {
                let paths = manifest_files(&entry.file);
                if paths.is_empty() {
                    return Err(invalid(format!(
                        "scaffold manifest has no file for task: {}",
                        entry.task
                    )));
                }
                for path in paths {
                    let file = ensure_workspace_file(&config.workspace, path)?;
                    files.push(format!("- {} -> {}", entry.task, file.display()));
                    manifest.push(ManifestFile::new(path, entry.blank_core.clone()));
                }
            }

            let after_snapshot = snapshot_workspace(&config.workspace)?;
            let changed = changed_files(&baseline_snapshot, &after_snapshot);
            let after = test_exec::run(&config.workspace, &project, &selector)
                .await
                .map_err(|error| invalid(format!("scaffold test run failed: {error}")))?;
            let contract = match build_contract(
                &config.workspace,
                &manifest,
                &changed,
                &project,
                &selector,
                &baseline,
                &after,
            ) {
                Ok(contract) => contract,
                Err(error) => {
                    let _ = store.record_scaffold_invalid(&active.workflow_id, &error.to_string());
                    return Err(invalid(format!("scaffold contract is invalid: {error}")));
                }
            };

            let body = format!(
                "### Manifest\n\n{output}\n\n### Files\n\n{}\n\n### Contract\n\n{}",
                files.join("\n"),
                render_contract(&contract)
            );
            let heading = format!("Scaffold {}", now_unix()?);
            let updated = append_section(&artifact, &heading, &body);
            let updated = set_artifact_status(&updated, "scaffolded");
            fs::write(&path, updated)?;
            let state =
                store.finish_scaffold(&active.workflow_id, contract, Some(selector))?;
            Ok(PlanOutcome {
                path,
                output: body,
                workflow_id: Some(state.workflow_id),
            })
        }),
    )
    .await
}

/// `/continue` checks the user's implementation against the scaffold contract.
pub async fn run_continue(config: &PlanConfig) -> Result<PlanOutcome, BoxError> {
    let store = WorkflowStore::new(&config.workspace);
    let active = store.require_active()?;
    active.can_continue()?;
    let contract = active
        .scaffold
        .clone()
        .ok_or_else(|| invalid("no scaffold contract; run /scaffold first"))?;
    let path = resolve_plan_path(config)?;
    let artifact = fs::read_to_string(&path)?;

    if let Err(error) = verify_hashes(&config.workspace, &contract) {
        let _ = store.block(
            &active.workflow_id,
            "contract",
            Some(error.to_string()),
            "restore the framework and test files, or rerun /scaffold",
        );
        return Err(invalid(format!(
            "contract check failed before /continue: {error}"
        )));
    }
    if let Err(error) = verify_core_implemented(&config.workspace, &contract) {
        return Err(invalid(format!(
            "/continue rejected; implement the core regions first: {error}"
        )));
    }

    let test_contract = active
        .test_contract
        .as_ref()
        .ok_or_else(|| invalid("no test contract; rerun /scaffold"))?;
    let run = run_fixed_contract(&config.workspace, test_contract).await?;
    if let Err(error) = verify_test_flip(&contract, &run) {
        return Err(invalid(format!(
            "/continue rejected; the contract did not flip green: {error}\n\n{}",
            render_test_run(&run)
        )));
    }

    let state = store.complete_user_implementation(&active.workflow_id, true, None)?;
    let body = format!(
        "### 核心实现\n\n核心标记已清除，框架与测试文件未改动。\n\n### Test\n\n{}",
        render_test_run(&run)
    );
    let heading = format!("Continue {}", now_unix()?);
    let updated = append_section(&artifact, &heading, &body);
    let updated = set_artifact_status(&updated, "continued");
    fs::write(&path, updated)?;
    Ok(PlanOutcome {
        path,
        output: format!("{body}\n\nnext: {}", next_action(&state)),
        workflow_id: Some(state.workflow_id),
    })
}

async fn run_fixed_contract(
    workspace: &Path,
    contract: &TestContract,
) -> Result<TestRun, BoxError> {
    let project =
        ProjectTestConfig::from_command(&contract.project_test_command).ok_or_else(|| {
            invalid(format!(
                "test contract command `{}` is not supported",
                contract.project_test_command
            ))
        })?;
    test_exec::run(workspace, &project, &contract.test_selector)
        .await
        .map_err(|error| invalid(format!("test contract could not run: {error}")))
}

fn render_contract(contract: &ScaffoldContract) -> String {
    let core = contract
        .core_regions
        .iter()
        .map(|region| format!("- {} :: {}", region.file, region.symbol))
        .collect::<Vec<_>>()
        .join("\n");
    format!(
        "framework files: {}\ntest files: {}\ncore regions:\n{core}\ncommand: {} {}\nexpected failing: {}\nexpected passing: {}",
        contract.framework_files.len(),
        contract.test_files.len(),
        contract.project_test_command,
        contract.test_selector,
        contract.expected_failing_tests.join(", "),
        contract.expected_passing_tests.len(),
    )
}

fn render_test_run(run: &TestRun) -> String {
    let status = if run.succeeded() { "passed" } else { "failed" };
    let failures = if run.failures.is_empty() {
        "none".to_owned()
    } else {
        run.failures.join(", ")
    };
    format!(
        "status: {status}\ncommand: {}\nexit: {:?}\nduration_ms: {}\nfailures: {failures}",
        run.command, run.exit_code, run.duration_ms
    )
}

/// Extra full runs allowed when a red run might be flaky.
pub const MAX_FLAKY_RERUNS: u32 = 2;

fn load_budget(workspace: &Path) -> Result<WorkflowBudget, BoxError> {
    WorkflowBudget::load(workspace).map_err(|error| invalid(error.to_string()))
}

fn budget_exhausted(state: &WorkflowState, budget: &WorkflowBudget) -> Option<String> {
    (state.budget.enforced_tokens >= budget.max_output_tokens.value).then(|| {
        format!(
            "token budget reached: {} of {} output tokens",
            state.budget.enforced_tokens, budget.max_output_tokens.value
        )
    })
}

fn blocked_outcome(
    store: &WorkflowStore,
    state: &WorkflowState,
    reason: &str,
    detail: &str,
) -> PlanOutcome {
    let mut output = format!("blocked: {reason}");
    if !detail.trim().is_empty() {
        output.push_str("\n\n");
        output.push_str(detail.trim());
    }
    PlanOutcome {
        path: store.root().join(&state.workflow_id).join("state.json"),
        output,
        workflow_id: Some(state.workflow_id.clone()),
    }
}

fn block_for_budget(
    store: &WorkflowStore,
    state: &WorkflowState,
    reason: &str,
    detail: &str,
) -> Result<PlanOutcome, BoxError> {
    let state = store.block(
        &state.workflow_id,
        "budget",
        Some(reason.to_owned()),
        "raise [budget].max_output_tokens in .slowcode/workflow.toml, then /workflow use <id>",
    )?;
    Ok(blocked_outcome(store, &state, reason, detail))
}

async fn run_contract_with_retries(
    workspace: &Path,
    contract: &TestContract,
) -> Result<(TestRun, u32, u64), BoxError> {
    let mut run = run_fixed_contract(workspace, contract).await?;
    let mut total_ms = run.duration_ms;
    if run.succeeded() {
        return Ok((run, 0, total_ms));
    }
    for rerun in 1..=MAX_FLAKY_RERUNS {
        run = run_fixed_contract(workspace, contract).await?;
        total_ms += run.duration_ms;
        if run.succeeded() {
            return Ok((run, rerun, total_ms));
        }
    }
    Ok((run, MAX_FLAKY_RERUNS, total_ms))
}

fn test_run_update(run: &TestRun, flaky_reruns: u32, total_ms: u64) -> TestRunUpdate {
    let mut update = TestRunUpdate::new(run.record(flaky_reruns));
    update.active_seconds = total_ms / 1_000;
    update
}

/// `/test` runs the fixed contract, verifies locked files, and records the run.
pub async fn run_test(config: &PlanConfig) -> Result<PlanOutcome, BoxError> {
    let store = WorkflowStore::new(&config.workspace);
    let active = store.require_active()?;
    active.can_enter_test()?;
    let contract = active
        .test_contract
        .clone()
        .ok_or_else(|| invalid("no test contract; run /approve or /scaffold first"))?;
    if let Err(error) = verify_test_contract_hashes(&config.workspace, &contract) {
        store.block(
            &active.workflow_id,
            "contract",
            Some(error.to_string()),
            "restore the framework and test files, or rerun /scaffold",
        )?;
        return Err(invalid(format!(
            "contract check failed before /test: {error}"
        )));
    }
    let (run, flaky_reruns, total_ms) =
        run_contract_with_retries(&config.workspace, &contract).await?;
    let update = test_run_update(&run, flaky_reruns, total_ms);
    let state = store.record_test_run(&active.workflow_id, update, MAX_CONSECUTIVE_SAME_FAILURE)?;
    let path = resolve_plan_path(config)?;
    let artifact = fs::read_to_string(&path)?;
    let heading = format!("Test {}", now_unix()?);
    let body = format!(
        "### Result\n\n{}\n\n### Output tail\n\n```text\n{}\n```",
        render_test_run(&run),
        run.record(flaky_reruns).output_tail
    );
    let updated = append_section(&artifact, &heading, &body);
    let updated = set_artifact_status(
        &updated,
        if run.succeeded() {
            "tested"
        } else {
            "test-failed"
        },
    );
    fs::write(&path, updated)?;
    let mut output = body;
    if let Some(blocked) = &state.blocked {
        output.push_str(&format!(
            "\n\nblocked: {} ({})",
            blocked.reason,
            blocked.resume_hint.as_deref().unwrap_or("")
        ));
    } else {
        output.push_str(&format!("\n\nnext: {}", next_action(&state)));
    }
    Ok(PlanOutcome {
        path,
        output,
        workflow_id: Some(state.workflow_id),
    })
}

/// `/fix` runs one driver turn over the last red test run and applies the
/// classification table's transition.
async fn run_fix_with_session(
    config: &PlanConfig,
    provider: Arc<dyn ModelProvider>,
    approval: Option<Arc<dyn ApprovalProvider>>,
    cancellation: CancellationToken,
    sessions: &mut PlanSession,
) -> Result<PlanOutcome, BoxError> {
    if config.read_only {
        return Err(invalid("/fix cannot run with --read-only"));
    }
    let store = WorkflowStore::new(&config.workspace);
    let active = store.require_active()?;
    active.can_fix()?;
    if !active.allows_workspace_write() {
        return Err(invalid(format!(
            "/fix cannot write in phase {}",
            active.phase
        )));
    }
    let contract = active
        .test_contract
        .clone()
        .ok_or_else(|| invalid("no test contract; run /test first"))?;
    if let Err(error) = verify_test_contract_hashes(&config.workspace, &contract) {
        store.block(
            &active.workflow_id,
            "contract",
            Some(error.to_string()),
            "restore the framework and test files, or rerun /scaffold",
        )?;
        return Err(invalid(format!(
            "contract check failed before /fix: {error}"
        )));
    }
    let last_run = active
        .test
        .last_run
        .clone()
        .unwrap_or_else(|| TestRunRecord {
            command: format!(
                "{} {}",
                contract.project_test_command, contract.test_selector
            ),
            status: TestStatus::Failed,
            exit_code: None,
            duration_ms: 0,
            failures: Vec::new(),
            passed: Vec::new(),
            flaky_reruns: 0,
            output_tail: active.test.last_failure.clone().unwrap_or_default(),
        });
    let expected_failures = if last_run.failures.is_empty() {
        vec!["<build>".to_owned()]
    } else {
        last_run.failures.clone()
    };
    let path = resolve_plan_path(config)?;
    let artifact = fs::read_to_string(&path)?;
    let evidence = render_fix_evidence(&last_run, &expected_failures);

    let (file_change, _) = write_policies(&approval);
    record_manifest(
        &config.workspace,
        &active.workflow_id,
        active.phase,
        CapabilityProfile::Driver,
    )?;
    let plan_config = config.clone();
    with_plan_agent(
        sessions,
        config,
        provider,
        PlanAgentPolicy {
            system: FIX_SYSTEM_PROMPT,
            profile: CapabilityProfile::Driver,
            registry: RegistryMode::Workspace,
            file_change,
            session_prefix: "slowcode-fix-",
            approval: approval.clone(),
            workflow_id: Some(active.workflow_id.clone()),
            audit_sink: None,
            max_steps_per_turn: None,
        },
        move |agent| {
            Box::pin(async move {
                let config = &plan_config;
                let started = std::time::Instant::now();
                let mut output = run_text_turn(
                    agent,
                    &fix_user_prompt(&artifact, &evidence),
                    1,
                    config,
                    &cancellation,
                )
                .await?;
                let mut validation = validate_failure_classifications(&output, &expected_failures);
                if !validation.is_valid() {
                    let repair = failure_classification_repair_prompt(&validation.reasons);
                    let repaired = run_text_turn(agent, &repair, 2, config, &cancellation).await?;
                    output = repaired;
                    validation = validate_failure_classifications(&output, &expected_failures);
                }
                let active_seconds = started.elapsed().as_secs();
                let state = store.record_fix_active_seconds(&active.workflow_id, active_seconds)?;
                if let Err(error) = verify_test_contract_hashes(&config.workspace, &contract) {
                    store.block(
                        &state.workflow_id,
                        "contract",
                        Some(error.to_string()),
                        "restore the framework and test files, then /workflow use <id>",
                    )?;
                    return Err(invalid(format!(
                        "/fix modified locked framework or test files: {error}"
                    )));
                }
                if !validation.is_valid() {
                    return Err(invalid(format!(
                        "/fix did not produce a valid `## 失败分类` table: {}\n\n{output}",
                        validation.reasons.join("; ")
                    )));
                }
                let rows = parse_failure_classifications(&output);
                let classifications: Vec<ClassifiedFailure> = rows
                    .iter()
                    .map(|row| ClassifiedFailure {
                        failure: row.failure.clone(),
                        kind: row.kind.clone(),
                        evidence: row.evidence.clone(),
                    })
                    .collect();
                let state = store.record_fix_completed(&state.workflow_id, &classifications)?;
                let body = format!("### 失败分类\n\n{output}");
                let heading = format!("Fix {}", now_unix()?);
                let updated = append_section(&artifact, &heading, &body);
                let updated = set_artifact_status(&updated, "fixed");
                fs::write(&path, updated)?;

                let unknown: Vec<UnknownGap> = rows
                    .iter()
                    .filter(|row| row.kind == "unknown_gap")
                    .map(|row| UnknownGap {
                        failure: row.failure.clone(),
                        evidence: row.evidence.clone(),
                    })
                    .collect();
                let contract_error = rows.iter().find(|row| row.kind == "test_contract_error");
                let environment = rows.iter().find(|row| row.kind == "environment");

                if !unknown.is_empty() {
                    let state = store.ask_unknown_gaps(&state.workflow_id, &unknown)?;
                    let question = state
                        .open_questions()
                        .first()
                        .map(|question| question.question_id.clone())
                        .unwrap_or_default();
                    return Ok(PlanOutcome {
                        path,
                        output: format!(
                            "{body}\n\nunknown gap: {}\nnext question: {question}",
                            question
                        ),
                        workflow_id: Some(state.workflow_id),
                    });
                }
                if let Some(row) = contract_error {
                    let state = store.return_to_review(
                        &state.workflow_id,
                        &format!("{}: {}", row.failure, row.evidence),
                    )?;
                    return Ok(PlanOutcome {
                        path,
                        output: format!(
                            "{body}\n\ncontract error: {}; back to Review, run /approve again",
                            row.evidence
                        ),
                        workflow_id: Some(state.workflow_id),
                    });
                }
                if let Some(row) = environment {
                    let state = store.block(
                        &state.workflow_id,
                        "environment",
                        Some(format!("{}: {}", row.failure, row.evidence)),
                        "fix the environment, then /workflow use <id>",
                    )?;
                    return Ok(PlanOutcome {
                        path,
                        output: format!("{body}\n\nblocked: environment ({})", row.evidence),
                        workflow_id: Some(state.workflow_id),
                    });
                }
                let flaky = rows.iter().filter(|row| row.kind == "flaky").count();
                Ok(PlanOutcome {
                    path,
                    output: format!(
                        "{body}\n\nclassified {} failure(s), flaky {flaky}\nnext: /test",
                        rows.len()
                    ),
                    workflow_id: Some(state.workflow_id),
                })
            })
        },
    )
    .await
}

fn render_fix_evidence(run: &TestRunRecord, expected_failures: &[String]) -> String {
    format!(
        "command: {}\nstatus: {}\nexit: {:?}\nduration_ms: {}\nfailures:\n{}\n\noutput tail:\n{}",
        run.command,
        match run.status {
            TestStatus::Passed => "passed",
            TestStatus::Failed => "failed",
        },
        run.exit_code,
        run.duration_ms,
        expected_failures
            .iter()
            .map(|failure| format!("- {failure}"))
            .collect::<Vec<_>>()
            .join("\n"),
        run.output_tail
    )
}

/// `/execute` is a compatibility alias: ensure a red run, fix it, then test.
async fn run_execute_with_session(
    config: &PlanConfig,
    provider: Arc<dyn ModelProvider>,
    approval: Option<Arc<dyn ApprovalProvider>>,
    cancellation: CancellationToken,
    sessions: &mut PlanSession,
) -> Result<PlanOutcome, BoxError> {
    if config.read_only {
        return Err(invalid("/execute cannot run with --read-only"));
    }
    let store = WorkflowStore::new(&config.workspace);
    let active = store.require_active()?;
    active.can_enter_test()?;
    let first = if active.test.last_status == Some(TestStatus::Failed) {
        None
    } else {
        Some(run_test(config).await?)
    };
    if let Some(outcome) = &first {
        let state = store.load(
            outcome
                .workflow_id
                .as_deref()
                .ok_or_else(|| invalid("test run has no workflow"))?,
        )?;
        if state.phase != Phase::TestFix || state.status != crate::workflow::WorkflowStatus::Active
        {
            return Ok(outcome.clone());
        }
    }
    let fix = Box::pin(run_fix_with_session(
        config,
        provider,
        approval,
        cancellation,
        sessions,
    ))
    .await?;
    let state = store.load(
        fix.workflow_id
            .as_deref()
            .ok_or_else(|| invalid("fix run has no workflow"))?,
    )?;
    let mut output = String::new();
    if let Some(first) = first {
        output.push_str(&first.output);
        output.push_str("\n\n");
    }
    output.push_str(&fix.output);
    if state.phase == Phase::TestFix && state.status == crate::workflow::WorkflowStatus::Active {
        let test = run_test(config).await?;
        output.push_str("\n\n");
        output.push_str(&test.output);
        return Ok(PlanOutcome {
            path: test.path,
            output,
            workflow_id: test.workflow_id,
        });
    }
    Ok(PlanOutcome {
        path: fix.path,
        output,
        workflow_id: fix.workflow_id,
    })
}

/// `/distill` records a Teach-back and returns an understanding-gap report.
async fn run_distill_with_session(
    config: &PlanConfig,
    provider: Arc<dyn ModelProvider>,
    explanation: &str,
    cancellation: CancellationToken,
    sessions: &mut PlanSession,
) -> Result<PlanOutcome, BoxError> {
    let explanation = explanation.trim().to_owned();
    if explanation.is_empty() {
        return Err(invalid("usage: /distill <your explanation>"));
    }
    sessions
        .record_human("slowcode-distill-", &explanation)
        .await?;
    let store = WorkflowStore::new(&config.workspace);
    let active = store.require_active()?;
    active.can_enter_distill()?;
    let path = resolve_plan_path(config)?;
    let artifact = fs::read_to_string(&path)?;

    record_manifest(
        &config.workspace,
        &active.workflow_id,
        active.phase,
        CapabilityProfile::Auditor,
    )?;
    let plan_config = config.clone();
    let plan_explanation = explanation.clone();
    with_plan_agent(
        sessions,
        config,
        provider,
        PlanAgentPolicy {
            system: DISTILL_SYSTEM_PROMPT,
            profile: CapabilityProfile::Auditor,
            registry: RegistryMode::ReadOnly,
            file_change: FileChangePolicy::Deny,
            session_prefix: "slowcode-distill-",
            approval: None,
            workflow_id: Some(active.workflow_id.clone()),
            audit_sink: None,
            max_steps_per_turn: None,
        },
        move |agent| {
            Box::pin(async move {
                let config = &plan_config;
                let explanation = plan_explanation.as_str();
                let prompt = distill_user_prompt(&artifact, explanation);
                let mut output =
                    run_human_turn(agent, &prompt, explanation, 1, config, &cancellation).await?;
                let mut validation = validate_distill_output(&output);
                if !validation.is_valid() {
                    output = run_text_turn(
                        agent,
                        &distill_repair_prompt(&validation.reasons),
                        2,
                        config,
                        &cancellation,
                    )
                    .await?;
                    validation = validate_distill_output(&output);
                }
                if !validation.is_valid() {
                    return Err(invalid(format!(
                        "distill report does not satisfy the contract: {}\n\n{output}",
                        validation.reasons.join("; "),
                    )));
                }

                let follow_up = distill_follow_up(&output).unwrap_or_default();
                let body = format!("### 用户解释\n\n{explanation}\n\n{}", output.trim());
                let heading = format!("Distill {}", now_unix()?);
                let updated = append_section(&artifact, &heading, &body);
                let updated = set_artifact_status(&updated, "distilled");
                fs::write(&path, updated)?;
                let state = store.record_distill_report(
                    &active.workflow_id,
                    Some(distill_follow_up_question(follow_up)),
                )?;
                let body = format!("{body}\n\n### 下一步\n\n{}", answer_next_step(&state));
                Ok(PlanOutcome {
                    path,
                    output: body,
                    workflow_id: Some(state.workflow_id),
                })
            })
        },
    )
    .await
}

fn distill_follow_up_question(question: String) -> Question {
    Question {
        question_id: String::new(),
        task_id: None,
        kind: QuestionKind::Knowledge,
        prompt: format!("Distill 追问：{question}\n请回答追问，或选择下一步。"),
        options: vec![
            "已回答，完成".to_owned(),
            "补充知识并按新未知域重新分类".to_owned(),
            "接受剩余缺口并完成".to_owned(),
        ],
        recommendation: None,
        reason: "auditor 的追问必须回答或显式接受剩余缺口，之后才能完成 workflow。".to_owned(),
        status: QuestionStatus::Open,
        criteria: "直接回答追问；若追问暴露新未知域，选择重新分类；或显式接受剩余缺口。".to_owned(),
        created_at: 0,
        answer: None,
        answer_source: None,
        answer_snapshot: None,
        attempts: 0,
        last_gap: None,
    }
}

/// `/hint` runs one read-only coach turn without changing phase or role.
async fn run_hint_with_session(
    config: &PlanConfig,
    provider: Arc<dyn ModelProvider>,
    question: &str,
    cancellation: CancellationToken,
    sessions: &mut PlanSession,
) -> Result<PlanOutcome, BoxError> {
    let question = question.trim().to_owned();
    if question.is_empty() {
        return Err(invalid("usage: /hint <question>"));
    }
    sessions.record_human("slowcode-hint-", &question).await?;
    let store = WorkflowStore::new(&config.workspace);
    let active = store.require_active()?;
    if active.status != crate::workflow::WorkflowStatus::Active
        || active.phase.capability_profile().is_none()
    {
        return Err(invalid(format!(
            "/hint is not available in phase {}",
            active.phase
        )));
    }
    let path = resolve_plan_path(config)?;
    let artifact = fs::read_to_string(&path)?;
    record_manifest(
        &config.workspace,
        &active.workflow_id,
        active.phase,
        CapabilityProfile::Coach,
    )?;
    let plan_config = config.clone();
    let plan_question = question.clone();
    with_plan_agent(
        sessions,
        config,
        provider,
        PlanAgentPolicy {
            system: HINT_SYSTEM_PROMPT,
            profile: CapabilityProfile::Coach,
            registry: RegistryMode::ReadOnly,
            file_change: FileChangePolicy::Deny,
            session_prefix: "slowcode-hint-",
            approval: None,
            workflow_id: Some(active.workflow_id.clone()),
            audit_sink: None,
            max_steps_per_turn: None,
        },
        move |agent| {
            Box::pin(async move {
                let config = &plan_config;
                let question = plan_question.as_str();
                let prompt = hint_user_prompt(&artifact, question);
                let output =
                    run_human_turn(agent, &prompt, question, 1, config, &cancellation).await?;
                let state =
                    store.record_capability_override(&active.workflow_id, question, "coach")?;
                Ok(PlanOutcome {
                    path,
                    output,
                    workflow_id: Some(state.workflow_id),
                })
            })
        },
    )
    .await
}

/// `/approve` accepts the current Review table and advances the phase.
pub async fn run_approve(config: &PlanConfig) -> Result<PlanOutcome, BoxError> {
    let store = WorkflowStore::new(&config.workspace);
    let active = store.require_active()?;
    if active.phase != Phase::Review {
        return Err(invalid(format!(
            "/approve is only available in review (current phase {})",
            active.phase
        )));
    }
    let user_owns_core = active.tasks.iter().any(|task| {
        task.exec_state == ExecState::PendingUser || task.strategy == Strategy::UserImplementsCore
    });
    let project = detect_project(&config.workspace);
    if project.is_none() && !user_owns_core {
        return Err(invalid(
            "/approve needs a project test command; add a Cargo package or pytest configuration",
        ));
    }
    let selector = if let Some(project) = &project {
        match derive_selector(
            &config.workspace,
            &active.tasks,
            project,
            active.test_selector.as_deref(),
        ) {
            SelectorChoice::Resolved(selector) => Some(selector),
            SelectorChoice::NeedsUser { options, reason } => {
                let state = store.ask_test_selector(&active.workflow_id, &options, &reason)?;
                let question = state
                    .open_questions()
                    .first()
                    .map(|question| question.question_id.clone())
                    .unwrap_or_default();
                return Ok(PlanOutcome {
                    path: store.root().join(&state.workflow_id).join("state.json"),
                    output: format!(
                        "selector required: {reason}\noptions: {}\nnext question: {question}\nanswer with /answer {question} <selector>",
                        if options.is_empty() {
                            "none".to_owned()
                        } else {
                            options.join(", ")
                        }
                    ),
                    workflow_id: Some(state.workflow_id),
                });
            }
        }
    } else {
        None
    };
    let contract = if user_owns_core {
        None
    } else {
        let project = project
            .as_ref()
            .ok_or_else(|| invalid("/approve needs a project test command"))?;
        let selector = selector.as_deref().unwrap_or_default();
        let baseline = test_exec::run(&config.workspace, project, selector)
            .await
            .map_err(|error| invalid(format!("review baseline could not run: {error}")))?;
        if !baseline.succeeded() {
            let failures = if baseline.failures.is_empty() {
                baseline
                    .output
                    .lines()
                    .last()
                    .unwrap_or("tests failed")
                    .to_owned()
            } else {
                baseline.failures.join(", ")
            };
            return Err(invalid(format!(
                "review baseline is red; fix these pre-existing failures or exclude them before /approve:\n{failures}"
            )));
        }
        Some(build_review_contract(project, selector, &baseline)?.test_contract())
    };
    let state = store.approve(&active.workflow_id, contract, selector)?;
    Ok(workflow_outcome(&store, &state))
}

/// `/status [--workflow <id>]` shows one workflow or the workspace list.
pub async fn run_status(
    config: &PlanConfig,
    workflow_id: Option<String>,
) -> Result<PlanOutcome, BoxError> {
    let store = WorkflowStore::new(&config.workspace);
    let requested = workflow_id
        .as_deref()
        .map(str::trim)
        .map(|value| value.strip_prefix("--workflow ").unwrap_or(value).trim())
        .filter(|value| !value.is_empty());
    let state = match requested {
        Some(id) => store.load(id)?,
        None => match store.load_active()? {
            Some(state) => state,
            None => {
                let listing = store.list()?;
                return Ok(PlanOutcome {
                    path: store.root().to_path_buf(),
                    output: render_workflow_list(&listing),
                    workflow_id: None,
                });
            }
        },
    };
    let budget = load_budget(&config.workspace)?;
    let mut outcome = workflow_outcome(&store, &state);
    outcome.output.push_str(&format!(
        "\n\n{}\n  model_calls: {}\n  tokens: {}\n  enforced_tokens: {}\n  active_seconds: {}",
        budget.render(),
        state.budget.model_calls,
        state.budget.tokens,
        state.budget.enforced_tokens,
        state.budget.active_seconds,
    ));
    Ok(outcome)
}

/// `/next` shows the next required action.
pub async fn run_next(config: &PlanConfig) -> Result<PlanOutcome, BoxError> {
    let store = WorkflowStore::new(&config.workspace);
    let state = store.require_active()?;
    let output = if let Some(question) = state.open_questions().first() {
        format!(
            "next question: {}\n{}",
            question.question_id, question.prompt
        )
    } else {
        format!("next: {}", next_action(&state))
    };
    Ok(PlanOutcome {
        path: store.root().join(&state.workflow_id).join("state.json"),
        output,
        workflow_id: Some(state.workflow_id),
    })
}

/// `/reclassify <text>` freezes writes and asks for the missing knowledge.
pub async fn run_reclassify(config: &PlanConfig, text: &str) -> Result<PlanOutcome, BoxError> {
    let text = text.trim();
    if text.is_empty() {
        return Err(invalid(
            "usage: /reclassify <new unknown-domain description>",
        ));
    }
    let store = WorkflowStore::new(&config.workspace);
    let active = store.require_active()?;
    let state = store.reclassify(&active.workflow_id, text)?;
    Ok(workflow_outcome(&store, &state))
}

/// `/workflow [list|use <id>|end]` manages workspace workflows.
pub async fn run_workflow(
    config: &PlanConfig,
    argument: Option<String>,
) -> Result<PlanOutcome, BoxError> {
    let store = WorkflowStore::new(&config.workspace);
    let argument = argument.unwrap_or_default();
    let argument = argument.trim();
    if argument.is_empty() || argument == "list" {
        let listing = store.list()?;
        return Ok(PlanOutcome {
            path: store.root().to_path_buf(),
            output: render_workflow_list(&listing),
            workflow_id: listing.active,
        });
    }
    if let Some(id) = argument.strip_prefix("use ") {
        let id = id.trim();
        if id.is_empty() {
            return Err(invalid("usage: /workflow use <id>"));
        }
        let state = store.use_workflow(id)?;
        return Ok(workflow_outcome(&store, &state));
    }
    if argument == "end" {
        let active = store.require_active()?;
        let state = store.end(&active.workflow_id)?;
        return Ok(workflow_outcome(&store, &state));
    }
    Err(invalid("usage: /workflow [list|use <id>|end]"))
}

fn workflow_outcome(store: &WorkflowStore, state: &WorkflowState) -> PlanOutcome {
    PlanOutcome {
        path: store.root().join(&state.workflow_id).join("state.json"),
        output: render_workflow_status(state),
        workflow_id: Some(state.workflow_id.clone()),
    }
}

fn render_workflow_status(state: &WorkflowState) -> String {
    let open = state.open_questions().len();
    let pending = state.pending_decisions().len();
    let approval = if state.review_is_current() {
        "approved".to_owned()
    } else {
        "approval required (run /approve)".to_owned()
    };
    format!(
        "workflow: {}\ntask: {}\nphase: {}\nstatus: {}\nversion: {}\nquestions: {open} open / {} total\ntasks: {} ({pending} pending decision)\napproval: {approval}\nnext: {}",
        state.workflow_id,
        state.task,
        state.phase,
        state.status.as_str(),
        state.version,
        state.questions.len(),
        state.tasks.len(),
        next_action(state),
    ) + &format!(
        "\nselector: {}\ntest: {} rounds, {} attempts, last {}{}",
        state.test_selector.as_deref().unwrap_or("(unresolved)"),
        state.test.rounds,
        state.test.attempts,
        match state.test.last_status {
            Some(crate::workflow::TestStatus::Passed) => "passed",
            Some(crate::workflow::TestStatus::Failed) => "failed",
            None => "not run",
        },
        if state.test.consecutive_same_failure > 0 {
            format!(
                ", consecutive same failure {}",
                state.test.consecutive_same_failure
            )
        } else {
            String::new()
        }
    ) + &format!(
        "\ndistill: {}",
        match state.distill.as_ref() {
            None => "not reported".to_owned(),
            Some(distill) if distill.gaps_accepted => "gaps accepted".to_owned(),
            Some(distill) if distill.reported => "reported, follow-up open".to_owned(),
            Some(_) => "not reported".to_owned(),
        }
    ) + &format!(
        "\nrole: {} / {}",
        state.last_role.as_deref().unwrap_or("none"),
        state.last_profile.as_deref().unwrap_or("none"),
    ) + &format!(
        "\nmanifest: {}",
        match state.context_manifest.as_ref() {
            None => "not recorded".to_owned(),
            Some(manifest) => {
                let skills = manifest.skill_names();
                format!(
                    "{} / {}; skills: {}",
                    manifest.phase,
                    manifest.profile.as_deref().unwrap_or("none"),
                    if skills.is_empty() {
                        "none".to_owned()
                    } else {
                        skills.join(", ")
                    }
                )
            }
        }
    ) + &format!(
        "\napprovals: {} pending\nworkspace: {}",
        state.pending_approvals.len(),
        if state.workspace_dirty {
            "dirty"
        } else {
            "clean"
        }
    )
}

fn render_workflow_list(listing: &crate::workflow::WorkflowListing) -> String {
    let active = listing.active.as_deref().unwrap_or("none");
    let mut output = format!("active: {active}\n");
    if listing.workflows.is_empty() {
        output.push_str("no workflows; run /plan <task> to start one");
        return output;
    }
    for summary in &listing.workflows {
        output.push_str(&format!(
            "{}  {}  {}  {}\n",
            summary.workflow_id,
            summary.status.as_str(),
            summary.phase,
            summary.task
        ));
    }
    output.trim_end().to_owned()
}

fn next_action(state: &WorkflowState) -> String {
    match state.phase {
        Phase::Classify => "finish classification".to_owned(),
        Phase::Interview => "answer the open question with /answer".to_owned(),
        Phase::Reclassify => "answer the new question with /answer".to_owned(),
        Phase::Review => "/approve".to_owned(),
        Phase::Scaffold => "/scaffold".to_owned(),
        Phase::AwaitUser => "implement the core regions, then /continue".to_owned(),
        Phase::TestFix => {
            if state.test.last_status == Some(TestStatus::Failed) {
                "/fix, then /test".to_owned()
            } else {
                "/test".to_owned()
            }
        }
        Phase::Distill => {
            let reported = state
                .distill
                .as_ref()
                .is_some_and(|distill| distill.reported);
            if !reported {
                "/distill <your explanation>".to_owned()
            } else if let Some(question) = state.open_questions().first() {
                format!(
                    "answer the distill follow-up with /answer #{} <answer>, or /answer #{} 接受剩余缺口并完成",
                    question.question_id, question.question_id
                )
            } else {
                "accept the remaining gaps with /answer 接受剩余缺口并完成".to_owned()
            }
        }
        Phase::Done => "workflow complete".to_owned(),
        Phase::Blocked => state
            .blocked
            .as_ref()
            .map(|blocked| format!("blocked: {}", blocked.reason))
            .unwrap_or_else(|| "blocked".to_owned()),
    }
}

fn answer_next_step(state: &WorkflowState) -> String {
    if let Some(question) = state.open_questions().first() {
        format!(
            "next question: {} {}",
            question.question_id, question.prompt
        )
    } else {
        format!("next: {}", next_action(state))
    }
}

/// Map cognitive tasks from the model output onto workflow tasks, preserving
/// existing ids when the title is unchanged.
fn workflow_tasks(tasks: &[CognitiveTask], existing: &[WorkflowTask]) -> Vec<WorkflowTask> {
    tasks
        .iter()
        .enumerate()
        .map(|(index, task)| {
            let task_id = existing
                .iter()
                .find(|current| current.title == task.task)
                .map(|current| current.task_id.clone())
                .unwrap_or_else(|| format!("task-{}", index + 1));
            let domain = match task.attribute {
                slowcode_cognition::CognitiveAttribute::Known => Domain::Known,
                slowcode_cognition::CognitiveAttribute::SemiKnown => Domain::SemiKnown,
                slowcode_cognition::CognitiveAttribute::Unknown => Domain::Unknown,
            };
            let exec_state = match task.execution_state {
                Some(slowcode_cognition::ExecutionState::Executable) => ExecState::Executable,
                Some(slowcode_cognition::ExecutionState::NeedsDecision) => {
                    ExecState::PendingDecision
                }
                Some(slowcode_cognition::ExecutionState::NeedsUserImplementation) => {
                    ExecState::PendingUser
                }
                None if domain == Domain::Unknown => ExecState::PendingUser,
                None if domain == Domain::SemiKnown => ExecState::PendingDecision,
                None => ExecState::Executable,
            };
            let strategy = if exec_state == ExecState::PendingUser {
                Strategy::UserImplementsCore
            } else if domain == Domain::Known {
                Strategy::AiImplements
            } else {
                Strategy::Undecided
            };
            WorkflowTask::new(
                task_id,
                task.task.clone(),
                domain,
                task.basis.clone(),
                exec_state,
                task.agent_action.clone(),
                task.user_role.clone(),
                task.minimum_path.clone(),
                task.acceptance.clone(),
                strategy,
            )
        })
        .collect()
}

fn workflow_questions(tasks: &[WorkflowTask], drafts: &[QuestionDraft]) -> Vec<Question> {
    let mut questions = Vec::new();
    for task in tasks {
        if task.domain == Domain::Unknown {
            questions.push(system_unknown_question(task));
        } else if task.exec_state == ExecState::PendingDecision
            && let Some(draft) = drafts.iter().find(|draft| draft.task == task.title)
        {
            questions.push(Question {
                question_id: String::new(),
                task_id: Some(task.task_id.clone()),
                kind: QuestionKind::SemiKnown,
                prompt: draft.prompt.clone(),
                options: draft.options.clone(),
                recommendation: draft.recommendation.clone(),
                reason: draft.reason.clone(),
                status: QuestionStatus::Open,
                criteria: draft.criteria.clone(),
                created_at: 0,
                answer: None,
                answer_source: None,
                answer_snapshot: None,
                attempts: 0,
                last_gap: None,
            });
        }
    }
    questions
}

fn system_unknown_question(task: &WorkflowTask) -> Question {
    Question {
        question_id: String::new(),
        task_id: Some(task.task_id.clone()),
        kind: QuestionKind::Unknown,
        prompt: format!(
            "{} 的缺失知识由你补充，还是你自己实现核心逻辑？",
            task.title
        ),
        options: vec!["补充知识".to_owned(), "自己实现核心".to_owned()],
        recommendation: Some("自己实现核心".to_owned()),
        reason: "仓库缺少这块知识，核心逻辑需要由你决定。".to_owned(),
        status: QuestionStatus::Open,
        criteria: "必须选择一种策略；选择补充知识时还要给出规则来源。".to_owned(),
        created_at: 0,
        answer: None,
        answer_source: None,
        answer_snapshot: None,
        attempts: 0,
        last_gap: None,
    }
}

fn knowledge_follow_up(task_id: String, task_title: String) -> Question {
    knowledge_follow_up_for(Some(task_id), task_title)
}

fn knowledge_follow_up_for(task_id: Option<String>, task_title: String) -> Question {
    Question {
        question_id: String::new(),
        task_id,
        kind: QuestionKind::Knowledge,
        prompt: format!("请给出 `{task_title}` 的缺失知识或规则来源，可以输入文本或用 @file。"),
        options: Vec::new(),
        recommendation: None,
        reason: "选择补充知识后，任务需要可引用的知识或约束才能重新分类。".to_owned(),
        status: QuestionStatus::Open,
        criteria: "给出缺失知识、规则或约束，并说明它约束哪些实现。".to_owned(),
        created_at: 0,
        answer: None,
        answer_source: None,
        answer_snapshot: None,
        attempts: 0,
        last_gap: None,
    }
}

#[derive(Clone, Debug, Default, Eq, PartialEq)]
struct AnswerArgument {
    question_id: Option<String>,
    file: Option<String>,
    text: String,
}

fn parse_answer_argument(raw: &str) -> Result<AnswerArgument, BoxError> {
    let mut rest = raw.trim();
    let mut argument = AnswerArgument::default();
    if let Some(after) = rest.strip_prefix('#') {
        let (question_id, tail) = split_first_token(after);
        if question_id.is_empty() {
            return Err(invalid("usage: /answer #<id> <answer>"));
        }
        argument.question_id = Some(question_id.to_owned());
        rest = tail.trim_start();
    }
    if let Some(after) = rest.strip_prefix('@') {
        let (file, tail) = split_file_token(after)?;
        if file.is_empty() {
            return Err(invalid("usage: /answer @<workspace-relative-file>"));
        }
        argument.file = Some(file.to_owned());
        rest = tail.trim_start();
    }
    argument.text = rest.to_owned();
    if argument.question_id.is_none() && argument.file.is_none() && argument.text.is_empty() {
        return Err(invalid("usage: /answer <answer>"));
    }
    Ok(argument)
}

fn split_first_token(value: &str) -> (&str, &str) {
    match value.find(char::is_whitespace) {
        Some(index) => (&value[..index], &value[index..]),
        None => (value, ""),
    }
}

fn split_file_token(value: &str) -> Result<(&str, &str), BoxError> {
    if let Some(rest) = value.strip_prefix('"') {
        let Some(end) = rest.find('"') else {
            return Err(invalid("unterminated quoted @file path"));
        };
        return Ok((&rest[..end], &rest[end + 1..]));
    }
    Ok(split_first_token(value))
}

fn resolve_selector_answer(options: &[String], text: &str) -> String {
    let first = text
        .lines()
        .map(str::trim)
        .find(|line| !line.is_empty())
        .unwrap_or_default();
    if let Ok(index) = first.parse::<usize>()
        && index >= 1
        && index <= options.len()
    {
        return options[index - 1].clone();
    }
    first.to_owned()
}

fn invalid(message: impl Into<String>) -> BoxError {
    io::Error::new(io::ErrorKind::InvalidInput, message.into()).into()
}

fn write_policies(approval: &Option<Arc<dyn ApprovalProvider>>) -> (FileChangePolicy, ShellPolicy) {
    if approval.is_some() {
        (FileChangePolicy::Ask, ShellPolicy::Ask)
    } else {
        (FileChangePolicy::Allow, ShellPolicy::Allow)
    }
}

/// Build the agent shared by the five Plan commands. The returned handle owns
/// the same toolset so the caller can always settle it after the turn.
fn plan_agent<'a>(
    config: &'a PlanConfig,
    provider: Arc<dyn ModelProvider>,
    policy: PlanAgentPolicy,
    sessions: &'a mut PlanSession,
) -> PlanAgentFuture<'a> {
    Box::pin(async move {
        let session_prefix = policy.session_prefix;
        let call = LlmCallConfig::new(DEEPSEEK_PROVIDER, &config.model)?;
        // Keep a single request from spending the whole workflow budget. The
        // caller still settles actual usage after the turn.
        let budget =
            WorkflowBudget::load(&config.workspace).map_err(|error| invalid(error.to_string()))?;
        let active = WorkflowStore::new(&config.workspace)
            .load_active()
            .ok()
            .flatten();
        let remaining = active.as_ref().map_or(DEFAULT_MAX_OUTPUT_TOKENS, |state| {
            budget
                .max_output_tokens
                .value
                .saturating_sub(state.budget.enforced_tokens)
        });
        let request_cap = MAX_OUTPUT_TOKENS_PER_TURN.min(remaining).max(1);
        let max_tokens = config
            .max_tokens
            .map_or(request_cap, |value| value.min(request_cap));
        let call = crate::apply_max_tokens(call, Some(max_tokens))?;
        let approval: Arc<dyn ApprovalProvider> = policy
            .approval
            .clone()
            .unwrap_or_else(|| Arc::new(NoApprovalProvider));
        let system = format!(
            "{}\n\n{}",
            crate::system_prompt(&config.workspace),
            policy.system
        );
        let mut spec = AgentSpec::new(config.workspace.clone(), call, system, policy.registry);
        spec.skill_roots = skill_roots_for(policy.profile)?;
        spec.read_only = policy.registry == RegistryMode::ReadOnly;
        spec.file_change = policy.file_change;
        // Plan drivers have no shell. Tests and process work go through the
        // system-owned TestExecutor.
        spec.shell = ShellPolicy::Deny;
        spec.approval = approval;
        spec.install_workspace_instructions = true;
        spec.max_steps_per_turn = policy.max_steps_per_turn;
        if policy.profile.is_read_only() {
            let store = WorkflowStore::new(&config.workspace);
            let shell = match policy.audit_sink.clone() {
                Some(sink) => crate::workflow::AuditedReadonlyShell::open_with_sink(
                    &config.workspace,
                    store,
                    policy.workflow_id.clone(),
                    sink,
                )?,
                None => crate::workflow::AuditedReadonlyShell::open(
                    &config.workspace,
                    store,
                    policy.workflow_id.clone(),
                )?,
            };
            spec.extra_tools
                .push(slowcode_roles::agent::ExtraToolSet::new(
                    Arc::new(shell),
                    vec![crate::workflow::readonly_shell_schema()?],
                ));
        }
        let prepared =
            slowcode_roles::agent::prepare(spec).map_err(|error| invalid(error.to_string()))?;
        let session = sessions.take_or_create(session_prefix)?;
        match slowcode_roles::agent::finish(prepared, session, provider).await {
            Ok(agent) => Ok(agent),
            Err(failure) => {
                if let Some(session) = failure.session {
                    sessions.put(*session);
                }
                Err(invalid(failure.error.to_string()))
            }
        }
    })
}

/// Run one Plan command body against an agent built around the shared Session.
///
/// The agent is always released afterwards so the Session returns to the
/// caller even when the command body fails.
async fn with_plan_agent<T, F>(
    sessions: &mut PlanSession,
    config: &PlanConfig,
    provider: Arc<dyn ModelProvider>,
    policy: PlanAgentPolicy,
    run: F,
) -> Result<T, BoxError>
where
    F: for<'a> FnOnce(&'a mut AgentLoop) -> PlanBodyFuture<'a, T>,
{
    let (mut agent, _tools) = plan_agent(config, provider, policy, sessions).await?;
    let result = run(&mut agent).await;
    let (session, shutdown) = match agent.shutdown_into_session().await {
        Ok(session) => (session, Ok(())),
        Err(error) => {
            let (source, session) = error.into_parts();
            (session, Err(source))
        }
    };
    sessions.put(session);
    merge_plan_result(result, shutdown)
}

fn skill_roots_for(profile: CapabilityProfile) -> Result<SkillRoots, BoxError> {
    let root = PathBuf::from(format!(".slowcode/roles/{}/skills", profile.role_name()));
    SkillRoots::new([root]).map_err(|error| invalid(error.to_string()))
}

fn record_manifest(
    workspace: &Path,
    workflow_id: &str,
    phase: Phase,
    profile: CapabilityProfile,
) -> Result<(), BoxError> {
    let manifest = build_skill_manifest(workspace, phase, Some(profile));
    WorkflowStore::new(workspace).record_context_manifest(workflow_id, manifest)?;
    Ok(())
}

fn flush_shell_audit(
    store: &WorkflowStore,
    workflow_id: &str,
    sink: &Arc<std::sync::Mutex<Vec<serde_json::Value>>>,
) -> Result<(), BoxError> {
    let Ok(mut pending) = sink.lock() else {
        return Ok(());
    };
    for payload in pending.drain(..) {
        store.record_shell_readonly(workflow_id, payload)?;
    }
    Ok(())
}

/// Run the command body, then always settle the toolset before returning.
/// A body error wins over a shutdown error so the user sees the real cause.
#[cfg(test)]
async fn finish_plan<T>(
    run: impl std::future::Future<Output = Result<T, BoxError>>,
    tools: Arc<dyn ToolExecutor>,
) -> Result<T, BoxError> {
    let result = run.await;
    let shutdown = tools.shutdown().await;
    merge_plan_result(result, shutdown)
}

fn merge_plan_result<T>(
    result: Result<T, BoxError>,
    shutdown: Result<(), ToolExecutorError>,
) -> Result<T, BoxError> {
    match (result, shutdown) {
        (Ok(value), Ok(())) => Ok(value),
        (Ok(_), Err(error)) => Err(error.into()),
        (Err(error), _) => Err(error),
    }
}

fn resolve_plan_path(config: &PlanConfig) -> Result<PathBuf, BoxError> {
    if let Some(path) = &config.plan {
        return Ok(path.to_owned());
    }
    latest_plan_path(&config.workspace)
}

fn latest_plan_path(workspace: &Path) -> Result<PathBuf, BoxError> {
    let directory = workspace.join(".scratch").join("plans");
    let entries = fs::read_dir(&directory).map_err(|_| {
        io::Error::new(
            io::ErrorKind::NotFound,
            format!(
                "no plan artifacts in {}; run /plan first",
                directory.display()
            ),
        )
    })?;
    let mut best: Option<(u128, PathBuf)> = None;
    for entry in entries {
        let path = entry?.path();
        if path.extension().and_then(|value| value.to_str()) != Some("md") {
            continue;
        }
        let name = path
            .file_name()
            .and_then(|value| value.to_str())
            .unwrap_or_default();
        if name.contains("-scaffold") || name.contains("-execute") || name.contains("-distill") {
            continue;
        }
        let stamp = name
            .split('-')
            .next()
            .and_then(|value| value.parse::<u128>().ok())
            .unwrap_or(0);
        if best.as_ref().is_none_or(|(current, _)| stamp > *current) {
            best = Some((stamp, path));
        }
    }
    best.map(|(_, path)| path).ok_or_else(|| {
        io::Error::new(
            io::ErrorKind::NotFound,
            "no plan artifact found; run /plan first",
        )
        .into()
    })
}

fn effective_tasks(artifact: &str) -> Result<Vec<CognitiveTask>, BoxError> {
    let tasks = parse_task_table(artifact);
    if tasks.is_empty() {
        return Err(invalid("plan artifact has no cognitive task table"));
    }
    let decisions = parse_decision_tables(artifact);
    if !decisions.is_empty() {
        let validation = validate_decisions(&decisions);
        if !validation.is_valid() {
            return Err(invalid(format!(
                "decision table is invalid: {}",
                validation.reasons.join("; ")
            )));
        }
    }
    apply_decisions(&tasks, &decisions).map_err(invalid)
}

fn decision_repair(reasons: &[String]) -> String {
    let mut prompt = String::from("Your decision table is invalid. Fix these problems:\n");
    for reason in reasons {
        prompt.push_str("- ");
        prompt.push_str(reason);
        prompt.push('\n');
    }
    prompt.push_str(
        "\nReturn only a corrected table with columns 任务 | 用户决策 | 新认知属性 | 新执行状态. Use the exact task text from the plan.",
    );
    prompt
}

fn clean_manifest_path(value: &str) -> &str {
    value.trim().trim_matches('`').trim()
}

fn manifest_files(value: &str) -> Vec<&str> {
    value
        .split([';', '；', ',', '，', '、', '\n'])
        .map(clean_manifest_path)
        .filter(|path| !path.is_empty())
        .collect()
}

fn ensure_workspace_file(workspace: &Path, file: &str) -> Result<PathBuf, BoxError> {
    let cleaned = clean_manifest_path(file);
    let candidate = Path::new(cleaned);
    let full = if candidate.is_absolute() {
        candidate.to_owned()
    } else {
        workspace.join(candidate)
    };
    let canonical = full.canonicalize().map_err(|_| {
        io::Error::new(
            io::ErrorKind::InvalidData,
            format!("scaffold manifest lists missing file: {file}"),
        )
    })?;
    let root = workspace
        .canonicalize()
        .unwrap_or_else(|_| workspace.to_owned());
    if !canonical.starts_with(&root) {
        return Err(invalid(format!(
            "scaffold file escapes the workspace: {file}"
        )));
    }
    Ok(canonical)
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
struct TurnUsage {
    model_calls: u64,
    output_tokens: u64,
}

/// Reserve one model call before the request starts. A reserve failure fails
/// closed: the run must not spend a call it could not record. Outside a run
/// scope (direct `run_*` calls in tests) this is a no-op.
fn reserve_model_call() -> Result<(), BoxError> {
    match ACTIVE_RUN.try_with(|run| {
        run.store.record_budget(
            &run.run_id,
            RunBudget {
                model_calls: 1,
                ..RunBudget::default()
            },
        )
    }) {
        Ok(Ok(_)) => Ok(()),
        Ok(Err(error)) => Err(format!("cannot reserve run budget: {error}").into()),
        Err(_) => Ok(()),
    }
}

/// Settle the difference after the turn. The reservation already counted the
/// first attempt, so only retries and reported output tokens are added.
/// Settlement is best effort: a journal failure must not lose a finished turn.
fn settle_model_usage(usage: TurnUsage) {
    let delta = RunBudget {
        model_calls: usage.model_calls.saturating_sub(1),
        output_tokens: usage.output_tokens,
        ..RunBudget::default()
    };
    if delta == RunBudget::default() {
        return;
    }
    let _ = ACTIVE_RUN.try_with(|run| run.store.record_budget(&run.run_id, delta.clone()));
}

async fn run_text_turn(
    agent: &mut AgentLoop,
    text: &str,
    turn: u64,
    config: &PlanConfig,
    cancellation: &CancellationToken,
) -> Result<String, BoxError> {
    Ok(run_text_turn_usage(agent, text, turn, config, cancellation)
        .await?
        .0)
}

async fn run_text_turn_usage(
    agent: &mut AgentLoop,
    text: &str,
    turn: u64,
    config: &PlanConfig,
    cancellation: &CancellationToken,
) -> Result<(String, TurnUsage), BoxError> {
    let message = Message::user(
        format!("plan-user-{turn}"),
        vec![ContentBlock::text(text)?],
        MessageSource::plugin("slowcode-plan")?,
    )?;
    run_messages_usage(agent, vec![message], text, turn, config, cancellation).await
}

/// One turn whose trailing text was typed by the human.
///
/// The caller records the human message before this turn, so only the
/// workflow instruction is appended here. `split_trailing_human` still guards
/// the prompt shape: the request text must end with the human text.
async fn run_human_turn(
    agent: &mut AgentLoop,
    prompt: &str,
    human: &str,
    turn: u64,
    config: &PlanConfig,
    cancellation: &CancellationToken,
) -> Result<String, BoxError> {
    Ok(
        run_human_turn_usage(agent, prompt, human, turn, config, cancellation)
            .await?
            .0,
    )
}

async fn run_human_turn_usage(
    agent: &mut AgentLoop,
    prompt: &str,
    human: &str,
    turn: u64,
    config: &PlanConfig,
    cancellation: &CancellationToken,
) -> Result<(String, TurnUsage), BoxError> {
    let (instruction, _human) = split_trailing_human(prompt, human)
        .ok_or_else(|| io::Error::other("prompt does not end with the human text"))?;
    let messages = vec![Message::user(
        format!("plan-context-{turn}"),
        vec![ContentBlock::text(instruction)?],
        MessageSource::plugin("slowcode-plan")?,
    )?];
    run_messages_usage(agent, messages, prompt, turn, config, cancellation).await
}

/// One `/plan` turn: the task was recorded by the caller, so only the
/// workflow instruction is appended here.
async fn run_task_turn_usage(
    agent: &mut AgentLoop,
    task: &str,
    turn: u64,
    config: &PlanConfig,
    cancellation: &CancellationToken,
) -> Result<(String, TurnUsage), BoxError> {
    let messages = vec![Message::user(
        format!("plan-context-{turn}"),
        vec![ContentBlock::text(PLAN_TURN_INSTRUCTION)?],
        MessageSource::plugin("slowcode-plan")?,
    )?];
    let request = format!("{task}\n\n{PLAN_TURN_INSTRUCTION}");
    run_messages_usage(agent, messages, &request, turn, config, cancellation).await
}

/// Split a prompt that ends with the human text into the workflow prefix and
/// the human suffix. The suffix is returned from `prompt` so it keeps the
/// exact text the prompt builder used.
fn split_trailing_human<'a>(prompt: &'a str, human: &str) -> Option<(&'a str, &'a str)> {
    if human.is_empty() {
        return None;
    }
    let index = prompt.rfind(human)?;
    Some((&prompt[..index], &prompt[index..]))
}

async fn run_messages_usage(
    agent: &mut AgentLoop,
    messages: Vec<Message>,
    request: &str,
    turn: u64,
    config: &PlanConfig,
    cancellation: &CancellationToken,
) -> Result<(String, TurnUsage), BoxError> {
    reserve_model_call()?;
    let outcome = agent
        .run_turn(TurnProposal::Enter(messages), cancellation.clone())
        .await?;
    let usage = TurnUsage {
        model_calls: u64::try_from(outcome.attempts()).unwrap_or(u64::MAX),
        output_tokens: outcome.reported_output_tokens(),
    };
    settle_model_usage(usage);
    let output = crate::outcome_text(&outcome);
    record_model_turn(config, turn, request, &output, usage);
    match outcome.reason() {
        TurnEndReason::Completed | TurnEndReason::MaxTokens => {}
        TurnEndReason::Aborted { .. } => {
            return Err(io::Error::new(io::ErrorKind::Interrupted, "cancelled").into());
        }
        TurnEndReason::Error { error } => {
            return Err(io::Error::other(format!(
                "model call failed: {} ({})",
                error.message(),
                error.code()
            ))
            .into());
        }
        reason => {
            return Err(io::Error::other(format!("turn ended with {reason:?}")).into());
        }
    }
    Ok((output, usage))
}

fn record_model_turn(
    config: &PlanConfig,
    turn: u64,
    request: &str,
    response: &str,
    usage: TurnUsage,
) {
    let store = WorkflowStore::new(&config.workspace);
    let Ok(Some(state)) = store.load_active() else {
        return;
    };
    let _ = store.record_model_usage(&state.workflow_id, usage.model_calls, usage.output_tokens);
    let call = ModelCall {
        turn_id: format!("{}-turn-{turn}", state.workflow_id),
        model: config.model.clone(),
        input_tokens: 0,
        output_tokens: usage.output_tokens,
        cache_hit: None,
        cost: None,
    };
    let (request, request_redactions) = redact_secrets(request, &config.workspace);
    let (response, response_redactions) = redact_secrets(response, &config.workspace);
    let redactions = request_redactions + response_redactions;
    let prompt_hash = sha256_hex(request.as_bytes());
    if let Ok((runs, run_id)) = ACTIVE_RUN.try_with(|run| (run.store.clone(), run.run_id.clone())) {
        let archived = runs.archive_model_call(
            &run_id,
            RunModelCall {
                turn_id: call.turn_id.clone(),
                model: call.model.clone(),
                prompt_hash: prompt_hash.clone(),
                input_tokens: call.input_tokens,
                output_tokens: call.output_tokens,
                cache_hit: call.cache_hit,
                cost: call.cost,
                redactions,
            },
            &request,
            &response,
        );
        if archived.is_ok() {
            let response_ref = format!(
                "run-states/{run_id}/model-calls/{}/response.txt",
                call.turn_id
            );
            let _ = store.record_model_call_reference(
                &state.workflow_id,
                call,
                &prompt_hash,
                &response_ref,
                redactions,
            );
            return;
        }
    }
    // Without a run, or when the run archive fails, keep the workflow copy so
    // the audit trail is never silently lost.
    let _ = store.record_model_call(&state.workflow_id, call, &request, &response);
}

fn write_plan_artifact(
    config: &PlanConfig,
    task: &str,
    output: &str,
    validation: &PlanValidation,
) -> Result<PathBuf, BoxError> {
    let created = now_unix()?;
    let file_stamp = now_unix_millis()?;
    let directory = config.workspace.join(".scratch").join("plans");
    fs::create_dir_all(&directory)?;
    let path = directory.join(format!("{file_stamp}-{}.md", artifact_slug(task)));
    let workspace = config.workspace.display().to_string();
    let artifact = render_artifact(task, &config.model, &workspace, created, output, validation);
    fs::write(&path, artifact)?;
    Ok(path)
}

fn now_unix() -> Result<u64, BoxError> {
    Ok(SystemTime::now().duration_since(UNIX_EPOCH)?.as_secs())
}

fn now_unix_millis() -> Result<u128, BoxError> {
    Ok(SystemTime::now().duration_since(UNIX_EPOCH)?.as_millis())
}

#[cfg(test)]
async fn run_plan(
    config: &PlanConfig,
    provider: Arc<dyn ModelProvider>,
    task: &str,
    cancellation: CancellationToken,
) -> Result<PlanOutcome, BoxError> {
    let mut sessions = PlanSession::new(None);
    run_plan_with_session(config, provider, task, cancellation, &mut sessions).await
}

#[cfg(test)]
fn run_answer<'a>(
    config: &'a PlanConfig,
    provider: Arc<dyn ModelProvider>,
    answer: &'a str,
    cancellation: CancellationToken,
) -> PlanOutcomeFuture<'a> {
    Box::pin(async move {
        let mut sessions = PlanSession::new(None);
        run_answer_with_session(config, provider, answer, cancellation, &mut sessions).await
    })
}

#[cfg(test)]
async fn run_scaffold(
    config: &PlanConfig,
    provider: Arc<dyn ModelProvider>,
    approval: Option<Arc<dyn ApprovalProvider>>,
    cancellation: CancellationToken,
) -> Result<PlanOutcome, BoxError> {
    let mut sessions = PlanSession::new(None);
    run_scaffold_with_session(config, provider, approval, cancellation, &mut sessions).await
}

#[cfg(test)]
async fn run_fix(
    config: &PlanConfig,
    provider: Arc<dyn ModelProvider>,
    approval: Option<Arc<dyn ApprovalProvider>>,
    cancellation: CancellationToken,
) -> Result<PlanOutcome, BoxError> {
    let mut sessions = PlanSession::new(None);
    run_fix_with_session(config, provider, approval, cancellation, &mut sessions).await
}

#[cfg(test)]
async fn run_execute(
    config: &PlanConfig,
    provider: Arc<dyn ModelProvider>,
    approval: Option<Arc<dyn ApprovalProvider>>,
    cancellation: CancellationToken,
) -> Result<PlanOutcome, BoxError> {
    let mut sessions = PlanSession::new(None);
    run_execute_with_session(config, provider, approval, cancellation, &mut sessions).await
}

#[cfg(test)]
async fn run_distill(
    config: &PlanConfig,
    provider: Arc<dyn ModelProvider>,
    explanation: &str,
    cancellation: CancellationToken,
) -> Result<PlanOutcome, BoxError> {
    let mut sessions = PlanSession::new(None);
    run_distill_with_session(config, provider, explanation, cancellation, &mut sessions).await
}

#[cfg(test)]
async fn run_hint(
    config: &PlanConfig,
    provider: Arc<dyn ModelProvider>,
    question: &str,
    cancellation: CancellationToken,
) -> Result<PlanOutcome, BoxError> {
    let mut sessions = PlanSession::new(None);
    run_hint_with_session(config, provider, question, cancellation, &mut sessions).await
}

#[cfg(test)]
mod tests {
    use std::{
        collections::VecDeque,
        path::Path,
        sync::{
            Arc, Mutex,
            atomic::{AtomicU64, Ordering},
        },
        time::{SystemTime, UNIX_EPOCH},
    };

    use futures_util::stream;
    use serde_json::json;
    use slowcode_kernel::{
        agent::{ToolExecutionFuture, ToolExecutionRequest, ToolShutdownFuture},
        model::{
            ContentBlockType, FinishReason, LlmCallConfigAdapterDefaults, NonNegativeSafeInteger,
            StreamChunk, TokenUsage,
        },
        provider::{
            PreparedProviderCall, PreparedRequestPreflight, ProviderPreflightError,
            ProviderPrepareError, ProviderRequest, ProviderRequestDraft, ProviderStream,
            ProviderStreamError,
        },
        session::ApprovalOutcome,
        ui::{UiEventKind, UiMessageSource},
    };
    use slowcode_runtime::RunState;

    use super::*;

    struct RecordingTools {
        order: Arc<Mutex<Vec<&'static str>>>,
        fail_shutdown: bool,
    }

    impl ToolExecutor for RecordingTools {
        fn execute(
            &self,
            _request: ToolExecutionRequest,
            _cancellation: CancellationToken,
        ) -> ToolExecutionFuture<'_> {
            Box::pin(async { Err(ToolExecutorError::new("not used")) })
        }

        fn shutdown(&self) -> ToolShutdownFuture<'_> {
            let order = Arc::clone(&self.order);
            let fail_shutdown = self.fail_shutdown;
            Box::pin(async move {
                order.lock().expect("order lock").push("shutdown");
                if fail_shutdown {
                    return Err(ToolExecutorError::new("shutdown failed"));
                }
                Ok(())
            })
        }
    }

    #[tokio::test]
    async fn finish_plan_settles_tools_after_the_body() {
        let order = Arc::new(Mutex::new(Vec::new()));
        let tools = Arc::new(RecordingTools {
            order: Arc::clone(&order),
            fail_shutdown: false,
        }) as Arc<dyn ToolExecutor>;
        let body_order = Arc::clone(&order);

        let result: Result<(), BoxError> = finish_plan(
            async move {
                body_order.lock().expect("order lock").push("body");
                Ok(())
            },
            tools,
        )
        .await;

        assert!(result.is_ok());
        assert_eq!(*order.lock().expect("order lock"), ["body", "shutdown"]);
    }

    #[tokio::test]
    async fn finish_plan_keeps_the_body_error_when_shutdown_also_fails() {
        let tools = Arc::new(RecordingTools {
            order: Arc::new(Mutex::new(Vec::new())),
            fail_shutdown: true,
        }) as Arc<dyn ToolExecutor>;

        let result: Result<(), BoxError> = finish_plan(
            async { Err(io::Error::new(io::ErrorKind::InvalidData, "body failed").into()) },
            tools,
        )
        .await;

        let error = result.expect_err("the body error wins");
        assert!(error.to_string().contains("body failed"));
    }

    #[test]
    fn merge_plan_result_reports_shutdown_failures_after_success() {
        let result =
            merge_plan_result::<()>(Ok(()), Err(ToolExecutorError::new("shutdown failed")));

        let error = result.expect_err("shutdown failure surfaces");
        assert!(error.to_string().contains("tool executor failed"));
    }

    struct PanicProvider;

    impl ModelProvider for PanicProvider {
        fn prepare_call(
            &self,
            _config: LlmCallConfig,
        ) -> Result<PreparedProviderCall, ProviderPrepareError> {
            panic!("provider must not be called")
        }

        fn preflight_request(
            &self,
            _draft: ProviderRequestDraft<'_>,
        ) -> Result<PreparedRequestPreflight, ProviderPreflightError> {
            panic!("provider must not be called")
        }

        fn stream(&self, _request: ProviderRequest, _cancel: CancellationToken) -> ProviderStream {
            panic!("provider must not be called")
        }
    }

    struct FakeProvider {
        attempts: Mutex<VecDeque<Vec<Result<StreamChunk, ProviderStreamError>>>>,
    }

    impl FakeProvider {
        fn new(responses: &[&str]) -> Self {
            Self {
                attempts: Mutex::new(responses.iter().map(|text| text_response(text)).collect()),
            }
        }

        fn with_attempts(attempts: Vec<Vec<Result<StreamChunk, ProviderStreamError>>>) -> Self {
            Self {
                attempts: Mutex::new(attempts.into()),
            }
        }
    }

    impl ModelProvider for FakeProvider {
        fn prepare_call(
            &self,
            config: LlmCallConfig,
        ) -> Result<PreparedProviderCall, ProviderPrepareError> {
            let mut raw = config.raw().as_value().clone();
            raw.as_object_mut()
                .expect("call config is an object")
                .insert("maxTokens".to_owned(), json!(4_096));
            let config = serde_json::from_value(raw).expect("max tokens produce a valid config");
            Ok(PreparedProviderCall::new(
                config,
                LlmCallConfigAdapterDefaults::default(),
                Some(NonNegativeSafeInteger::new(16_384).expect("context window fits")),
            ))
        }

        fn preflight_request(
            &self,
            draft: ProviderRequestDraft<'_>,
        ) -> Result<PreparedRequestPreflight, ProviderPreflightError> {
            let prepared = self.prepare_call(draft.config().clone())?;
            draft.finish(prepared, 1)
        }

        fn stream(&self, _request: ProviderRequest, _cancel: CancellationToken) -> ProviderStream {
            let chunks = self
                .attempts
                .lock()
                .unwrap()
                .pop_front()
                .expect("fake provider has a scripted response");
            Box::pin(stream::iter(chunks))
        }
    }

    fn text_response(text: &str) -> Vec<Result<StreamChunk, ProviderStreamError>> {
        vec![
            Ok(StreamChunk::block_start(0, ContentBlockType::Text).unwrap()),
            Ok(StreamChunk::text_delta(0, text).unwrap()),
            Ok(StreamChunk::block_end(0, ContentBlock::text(text).unwrap()).unwrap()),
            Ok(StreamChunk::finish(FinishReason::stop().unwrap(), None).unwrap()),
        ]
    }

    fn text_response_with_usage(
        text: &str,
        input_tokens: u64,
        output_tokens: u64,
    ) -> Vec<Result<StreamChunk, ProviderStreamError>> {
        let mut chunks = text_response(text);
        chunks.insert(
            0,
            Ok(StreamChunk::usage(TokenUsage::new(input_tokens, output_tokens).unwrap()).unwrap()),
        );
        chunks
    }

    fn write_call_response(
        file_path: &str,
        content: &str,
    ) -> Vec<Result<StreamChunk, ProviderStreamError>> {
        let arguments = json!({ "file_path": file_path, "content": content }).to_string();
        vec![
            Ok(StreamChunk::block_start(0, ContentBlockType::ToolCall).unwrap()),
            Ok(
                StreamChunk::tool_call_delta(0, "call-1", Some("write".to_owned()), &arguments)
                    .unwrap(),
            ),
            Ok(StreamChunk::block_end(
                0,
                ContentBlock::tool_call("call-1", "write", &arguments).unwrap(),
            )
            .unwrap()),
            Ok(StreamChunk::finish(FinishReason::tool_calls().unwrap(), None).unwrap()),
        ]
    }

    fn tool_call_response(
        name: &str,
        arguments: serde_json::Value,
    ) -> Vec<Result<StreamChunk, ProviderStreamError>> {
        let arguments = arguments.to_string();
        vec![
            Ok(StreamChunk::block_start(0, ContentBlockType::ToolCall).unwrap()),
            Ok(
                StreamChunk::tool_call_delta(0, "call-1", Some(name.to_owned()), &arguments)
                    .unwrap(),
            ),
            Ok(StreamChunk::block_end(
                0,
                ContentBlock::tool_call("call-1", name, &arguments).unwrap(),
            )
            .unwrap()),
            Ok(StreamChunk::finish(FinishReason::tool_calls().unwrap(), None).unwrap()),
        ]
    }

    #[derive(Default)]
    struct RecordingApproval {
        requests: AtomicU64,
        outcome: Option<ApprovalOutcome>,
    }

    impl ApprovalProvider for RecordingApproval {
        fn request(
            &self,
            _request: slowcode_kernel::agent::ApprovalRequest,
            _cancellation: CancellationToken,
        ) -> slowcode_kernel::agent::ApprovalFuture<'_> {
            self.requests.fetch_add(1, Ordering::SeqCst);
            let outcome = self.outcome.unwrap_or(ApprovalOutcome::AllowedOnce);
            Box::pin(async move { Ok(outcome) })
        }
    }

    /// A decision provider that snapshots the run just before it answers, so a
    /// test can prove the run was persisted as waiting before the prompt.
    struct InspectingApproval {
        seen: Arc<Mutex<Option<RunState>>>,
        runs: RunStore,
        run_id: String,
        outcome: ApprovalOutcome,
    }

    impl ApprovalProvider for InspectingApproval {
        fn request(
            &self,
            _request: slowcode_kernel::agent::ApprovalRequest,
            _cancellation: CancellationToken,
        ) -> slowcode_kernel::agent::ApprovalFuture<'_> {
            let seen = Arc::clone(&self.seen);
            let runs = self.runs.clone();
            let run_id = self.run_id.clone();
            let outcome = self.outcome;
            Box::pin(async move {
                *seen.lock().expect("seen lock") = Some(runs.load(&run_id).expect("run loads"));
                Ok(outcome)
            })
        }
    }

    fn temp_workspace() -> PathBuf {
        static COUNTER: AtomicU64 = AtomicU64::new(0);
        let nonce = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("clock")
            .as_nanos();
        let unique = COUNTER.fetch_add(1, Ordering::Relaxed);
        let path = std::env::temp_dir().join(format!(
            "slowcode-app-plan-{}-{nonce}-{unique}",
            std::process::id()
        ));
        fs::create_dir_all(&path).expect("temp workspace");
        path
    }

    fn run_store(workspace: &Path, workflow_id: &str) -> RunStore {
        RunStore::new(
            workspace
                .join(".scratch/workflows")
                .join(workflow_id)
                .join("run-states"),
        )
    }

    fn only_run_id(workspace: &Path, workflow_id: &str) -> String {
        let root = workspace
            .join(".scratch/workflows")
            .join(workflow_id)
            .join("run-states");
        let mut ids: Vec<String> = fs::read_dir(&root)
            .expect("run-states directory")
            .map(|entry| {
                entry
                    .expect("run entry")
                    .file_name()
                    .to_string_lossy()
                    .into_owned()
            })
            .collect();
        ids.sort();
        assert_eq!(ids.len(), 1, "{ids:?}");
        ids.remove(0)
    }

    /// A minimal cargo project whose baseline test suite is green.
    fn seed_cargo_baseline(workspace: &Path) {
        fs::create_dir_all(workspace.join("src")).expect("src directory");
        fs::create_dir_all(workspace.join("tests")).expect("tests directory");
        fs::write(
            workspace.join("Cargo.toml"),
            "[package]\nname = \"fixture\"\nversion = \"0.0.0\"\nedition = \"2021\"\n",
        )
        .expect("cargo manifest");
        fs::write(
            workspace.join("src/lib.rs"),
            "pub fn parse(value: &str) -> String {\n    value.to_owned()\n}\n",
        )
        .expect("baseline implementation");
        fs::write(
            workspace.join("tests/parse_test.rs"),
            "#[test]\nfn parses() {\n    assert_eq!(fixture::parse(\"x\"), \"x\");\n}\n",
        )
        .expect("baseline test");
    }

    fn scaffold_manifest() -> &'static str {
        "\
## Scaffold manifest
| 任务 | 文件 | 留空位置 | 验收命令 |
|---|---|---|---|
| 设计解析算法 | src/lib.rs | parse 核心 | cargo test |
| 设计解析算法 | tests/parse_test.rs | | cargo test |
"
    }

    fn write_plan(workspace: &Path, plan: &str) {
        let directory = workspace.join(".scratch/plans");
        fs::create_dir_all(&directory).expect("plan directory");
        fs::write(directory.join("1000-plan.md"), plan).expect("plan artifact");
    }

    fn seed_scaffold_workflow(workspace: &Path) {
        let store = WorkflowStore::new(workspace);
        let tasks = workflow_tasks(&parse_task_table(plan_text()), &[]);
        let state = store
            .create_with_tasks("test", None, tasks)
            .expect("workflow");
        let state = store
            .approve(&state.workflow_id, None, None)
            .expect("approval");
        assert_eq!(state.phase, Phase::Scaffold);
    }

    fn plan_text() -> &'static str {
        "\
---
kind: plan
created_at_unix: 1000
model: fake
workspace: test
status: paused
phases: Explore, Navigate
validation: ok
---

# Plan: test

## 认知任务分配表
| 任务 | 认知属性 | 判定依据 | 执行状态 | Agent 行为 | 用户角色 | 最小深度链路 | 验收 |
|---|---|---|---|---|---|---|---|
| 读取输入 | 已知域 | src/main.rs 的 read 工具 | 可执行 | 自主执行 | 审查者 | read | 打印内容 |
| 设计解析算法 | 未知域 | 仓库没有解析规则 | 待用户实现 | 搭建脚手架 | 创造者 | parse 核心留空 | 通过测试 |
"
    }

    fn question_plan_text() -> &'static str {
        "\
## 证据包
- read src/cache.rs

## 认知任务分配表
| 任务 | 认知属性 | 判定依据 | 执行状态 | Agent 行为 | 用户角色 | 最小深度链路 | 验收 |
|---|---|---|---|---|---|---|---|
| 选择缓存策略 | 半已知域 | src/cache.rs | 待决策 | 给出选项 | 决策者 | 决策后实现 | 用户确认 |
| 设计解析算法 | 未知域 | 仓库没有解析规则 | 待用户实现 | 搭建脚手架 | 创造者 | parse 核心留空 | 通过测试 |

## 半已知域问题
| 任务 | 问题 | 选项 | 推荐 | 推荐理由 | 判定标准 |
|---|---|---|---|---|---|
| 选择缓存策略 | 缓存策略选 A 还是 B？ | A / B | A | 与现有接口兼容 | 必须二选一并说明兼容性 |
"
    }

    #[tokio::test]
    async fn a_cancelled_plan_does_not_write_an_artifact() {
        let workspace = temp_workspace();
        let config = PlanConfig::new(workspace.clone());
        let cancellation = CancellationToken::new();
        cancellation.cancel();

        let error = run_plan(
            &config,
            Arc::new(PanicProvider),
            "count lines",
            cancellation,
        )
        .await
        .expect_err("a cancelled plan fails");

        assert!(error.to_string().contains("cancelled"), "{error}");
        assert!(!workspace.join(".scratch/plans").exists());
    }

    #[tokio::test]
    async fn read_only_rejects_writing_plan_commands() {
        let workspace = temp_workspace();
        write_plan(&workspace, plan_text());
        let mut config = PlanConfig::new(workspace);
        config.read_only = true;

        let scaffold = run_scaffold(
            &config,
            Arc::new(PanicProvider),
            None,
            CancellationToken::new(),
        )
        .await
        .expect_err("scaffold is rejected");
        assert!(scaffold.to_string().contains("--read-only"), "{scaffold}");

        let execute = run_execute(
            &config,
            Arc::new(PanicProvider),
            None,
            CancellationToken::new(),
        )
        .await
        .expect_err("execute is rejected");
        assert!(execute.to_string().contains("--read-only"), "{execute}");
    }

    #[test]
    fn approval_turns_writes_into_asks() {
        let approval: Option<Arc<dyn ApprovalProvider>> = Some(Arc::new(NoApprovalProvider));
        assert_eq!(
            write_policies(&approval),
            (FileChangePolicy::Ask, ShellPolicy::Ask)
        );

        assert_eq!(
            write_policies(&None),
            (FileChangePolicy::Allow, ShellPolicy::Allow)
        );
    }

    #[tokio::test]
    async fn scaffold_asks_before_writing_when_approval_is_installed() {
        let workspace = temp_workspace();
        seed_cargo_baseline(&workspace);
        write_plan(&workspace, plan_text());
        seed_scaffold_workflow(&workspace);
        let config = PlanConfig::new(workspace.clone());
        let provider = FakeProvider::with_attempts(vec![
            write_call_response(
                "src/lib.rs",
                "pub fn parse(value: &str) -> String {\n    // TODO(user)\n    String::new()\n}\n",
            ),
            text_response(scaffold_manifest()),
        ]);
        let approval = Arc::new(RecordingApproval::default());

        let outcome = run_scaffold(
            &config,
            Arc::new(provider),
            Some(Arc::clone(&approval) as Arc<dyn ApprovalProvider>),
            CancellationToken::new(),
        )
        .await
        .expect("scaffold succeeds");

        assert_eq!(approval.requests.load(Ordering::SeqCst), 1);
        let file = fs::read_to_string(workspace.join("src/lib.rs")).expect("file");
        assert!(file.contains("TODO(user)"), "{file}");
        let artifact = fs::read_to_string(outcome.path).expect("artifact");
        assert!(artifact.contains("status: scaffolded"), "{artifact}");
    }

    #[tokio::test]
    async fn a_rejected_approval_leaves_the_file_untouched() {
        let workspace = temp_workspace();
        seed_cargo_baseline(&workspace);
        let original = fs::read_to_string(workspace.join("src/lib.rs")).expect("baseline");
        write_plan(&workspace, plan_text());
        seed_scaffold_workflow(&workspace);
        let config = PlanConfig::new(workspace.clone());
        let provider = FakeProvider::with_attempts(vec![
            write_call_response(
                "src/lib.rs",
                "pub fn parse(value: &str) -> String {\n    // TODO(user)\n    String::new()\n}\n",
            ),
            text_response(scaffold_manifest()),
        ]);
        let approval = Arc::new(RecordingApproval {
            requests: AtomicU64::new(0),
            outcome: Some(ApprovalOutcome::Rejected),
        });

        let error = run_scaffold(
            &config,
            Arc::new(provider),
            Some(Arc::clone(&approval) as Arc<dyn ApprovalProvider>),
            CancellationToken::new(),
        )
        .await
        .expect_err("a rejected write cannot produce a contract");

        assert_eq!(approval.requests.load(Ordering::SeqCst), 1);
        assert!(error.to_string().contains("TODO(user)"), "{error}");
        let file = fs::read_to_string(workspace.join("src/lib.rs")).expect("file");
        assert_eq!(file, original);
    }

    #[tokio::test]
    async fn plan_writes_an_artifact_through_the_shared_layer() {
        let workspace = temp_workspace();
        let config = PlanConfig::new(workspace.clone());
        let provider = FakeProvider::new(&["\
## 证据包
- read src/main.rs

## 认知任务分配表
| 任务 | 认知属性 | 判定依据 | 执行状态 | Agent 行为 | 用户角色 | 最小深度链路 | 验收 |
|---|---|---|---|---|---|---|---|
| 读取输入 | 已知域 | src/main.rs | 可执行 | 自主执行 | 审查者 | read | 打印内容 |
"]);

        let outcome = run_plan(
            &config,
            Arc::new(provider),
            "count lines",
            CancellationToken::new(),
        )
        .await
        .expect("plan succeeds");

        assert!(outcome.path.exists(), "{}", outcome.path.display());
        assert!(outcome.output.contains("认知任务分配表"));
    }

    #[tokio::test]
    async fn scaffold_is_rejected_before_approval() {
        let workspace = temp_workspace();
        write_plan(&workspace, plan_text());
        let store = WorkflowStore::new(&workspace);
        let tasks = workflow_tasks(&parse_task_table(plan_text()), &[]);
        store
            .create_with_tasks("test", None, tasks)
            .expect("workflow");

        let error = run_scaffold(
            &PlanConfig::new(workspace.clone()),
            Arc::new(PanicProvider),
            None,
            CancellationToken::new(),
        )
        .await
        .expect_err("scaffold needs approval");

        assert!(
            error.to_string().contains("does not allow /scaffold"),
            "{error}"
        );
        let _ = fs::remove_dir_all(&workspace);
    }

    #[tokio::test]
    async fn approval_rejects_a_red_baseline() {
        let workspace = temp_workspace();
        seed_cargo_baseline(&workspace);
        fs::write(
            workspace.join("tests/parse_test.rs"),
            "#[test]\nfn parses() {\n    assert_eq!(fixture::parse(\"x\"), \"y\");\n}\n",
        )
        .expect("red baseline");
        write_plan(&workspace, plan_text());
        let store = WorkflowStore::new(&workspace);
        let tasks = workflow_tasks(&parse_task_table(plan_text()), &[])
            .into_iter()
            .filter(|task| task.domain == Domain::Known)
            .collect();
        let state = store
            .create_with_tasks("test", None, tasks)
            .expect("workflow");

        let error = run_approve(&PlanConfig::new(workspace.clone()))
            .await
            .expect_err("a red baseline blocks approval");
        assert!(error.to_string().contains("baseline is red"), "{error}");
        let state = store.load(&state.workflow_id).expect("state is readable");
        assert_eq!(state.phase, Phase::Review);
        assert!(state.review.approved_table_hash.is_none());
        let _ = fs::remove_dir_all(&workspace);
    }

    #[tokio::test]
    async fn scaffold_without_a_core_marker_cannot_enter_await_user() {
        let workspace = temp_workspace();
        seed_cargo_baseline(&workspace);
        write_plan(&workspace, plan_text());
        seed_scaffold_workflow(&workspace);
        let provider = FakeProvider::with_attempts(vec![
            write_call_response(
                "src/lib.rs",
                "pub fn parse(value: &str) -> String {\n    value.to_owned()\n}\n",
            ),
            text_response(scaffold_manifest()),
        ]);

        let error = run_scaffold(
            &PlanConfig::new(workspace.clone()),
            Arc::new(provider),
            None,
            CancellationToken::new(),
        )
        .await
        .expect_err("a contract without a core region is rejected");

        assert!(error.to_string().contains("TODO(user)"), "{error}");
        let state = WorkflowStore::new(&workspace)
            .require_active()
            .expect("workflow stays active");
        assert_eq!(state.phase, Phase::Scaffold);
        assert!(state.scaffold.is_none());
        let _ = fs::remove_dir_all(&workspace);
    }

    #[tokio::test]
    async fn continue_blocks_and_freezes_when_a_test_file_changed() {
        let workspace = temp_workspace();
        seed_cargo_baseline(&workspace);
        write_plan(&workspace, plan_text());
        seed_scaffold_workflow(&workspace);
        let provider = FakeProvider::with_attempts(vec![
            write_call_response(
                "src/lib.rs",
                "pub fn parse(value: &str) -> String {\n    // TODO(user)\n    String::new()\n}\n",
            ),
            text_response(scaffold_manifest()),
        ]);
        run_scaffold(
            &PlanConfig::new(workspace.clone()),
            Arc::new(provider),
            None,
            CancellationToken::new(),
        )
        .await
        .expect("scaffold succeeds");

        fs::write(
            workspace.join("src/lib.rs"),
            "pub fn parse(value: &str) -> String {\n    value.to_owned()\n}\n",
        )
        .expect("user implementation");
        let mut test_file =
            fs::read_to_string(workspace.join("tests/parse_test.rs")).expect("test file");
        test_file.push_str("\n// tampered\n");
        fs::write(workspace.join("tests/parse_test.rs"), test_file).expect("tampered test file");

        let error = run_continue(&PlanConfig::new(workspace.clone()))
            .await
            .expect_err("a changed test file blocks continue");
        assert!(error.to_string().contains("changed after"), "{error}");
        let listing = WorkflowStore::new(&workspace)
            .list()
            .expect("index is readable");
        let summary = listing
            .workflows
            .iter()
            .find(|workflow| workflow.status == crate::workflow::WorkflowStatus::Blocked)
            .expect("blocked workflow remains listed");
        let state = WorkflowStore::new(&workspace)
            .load(&summary.workflow_id)
            .expect("blocked state is readable");
        assert_eq!(state.status, crate::workflow::WorkflowStatus::Blocked);
        assert_eq!(
            state
                .blocked
                .as_ref()
                .map(|blocked| blocked.reason.as_str()),
            Some("contract")
        );
        let _ = fs::remove_dir_all(&workspace);
    }

    #[tokio::test]
    async fn plan_is_rejected_while_a_workflow_is_active() {
        let workspace = temp_workspace();
        let store = WorkflowStore::new(&workspace);
        store
            .create_with_tasks(
                "existing",
                None,
                vec![
                    workflow_tasks(&parse_task_table(plan_text()), &[])
                        .into_iter()
                        .next()
                        .expect("one task"),
                ],
            )
            .expect("active workflow");

        let error = run_plan(
            &PlanConfig::new(workspace.clone()),
            Arc::new(PanicProvider),
            "another task",
            CancellationToken::new(),
        )
        .await
        .expect_err("an active workflow blocks /plan");

        let text = error.to_string();
        assert!(text.contains("still active"), "{text}");
        assert!(text.contains("task: existing"), "{text}");
        assert!(text.contains("/plan --new"), "{text}");
        let _ = fs::remove_dir_all(&workspace);
    }

    #[test]
    fn split_new_flag_accepts_only_a_leading_flag() {
        assert_eq!(split_new_flag("--new count lines"), (true, "count lines"));
        assert_eq!(
            split_new_flag("  --new   count lines"),
            (true, "count lines")
        );
        assert_eq!(
            split_new_flag("count --new lines"),
            (false, "count --new lines")
        );
        assert_eq!(split_new_flag("--newer task"), (false, "--newer task"));
        assert_eq!(split_new_flag("--new"), (true, ""));
    }

    fn human_texts(session: &Session) -> Vec<String> {
        session
            .replay_ui_events()
            .into_iter()
            .filter_map(|event| match event.kind {
                UiEventKind::UserMessage {
                    source: UiMessageSource::Human,
                    text,
                    ..
                } => Some(text),
                _ => None,
            })
            .collect()
    }

    fn valid_plan_output() -> &'static str {
        "\
## 证据包
- read src/main.rs

## 认知任务分配表
| 任务 | 认知属性 | 判定依据 | 执行状态 | Agent 行为 | 用户角色 | 最小深度链路 | 验收 |
|---|---|---|---|---|---|---|---|
| 读取输入 | 已知域 | src/main.rs | 可执行 | 自主执行 | 审查者 | read | 打印内容 |
"
    }

    #[tokio::test]
    async fn failed_plan_records_the_task_before_the_gate() {
        let workspace = temp_workspace();
        let store = WorkflowStore::new(&workspace);
        store
            .create_with_tasks("existing", None, Vec::new())
            .expect("active workflow");
        let mut session = Some(Session::new("session-plan-blocked").expect("session opens"));

        let error = run_command_with_session(
            PlanCommand::Plan("another task".to_owned()),
            &PlanConfig::new(workspace.clone()),
            Arc::new(PanicProvider),
            None,
            CancellationToken::new(),
            &mut session,
        )
        .await
        .expect_err("an active workflow blocks /plan");

        assert!(error.to_string().contains("still active"), "{error}");
        let session = session.expect("the session is returned");
        assert_eq!(human_texts(&session), vec!["another task"]);
        let _ = fs::remove_dir_all(&workspace);
    }

    #[tokio::test]
    async fn plan_new_pauses_the_active_workflow_and_starts_another() {
        let workspace = temp_workspace();
        let store = WorkflowStore::new(&workspace);
        let old = store
            .create_with_tasks("existing", None, Vec::new())
            .expect("active workflow");
        let provider: Arc<dyn ModelProvider> =
            Arc::new(FakeProvider::with_attempts(vec![text_response(
                valid_plan_output(),
            )]));
        let mut session = Some(Session::new("session-plan-new").expect("session opens"));

        let outcome = run_command_with_session(
            PlanCommand::Plan("--new count lines".to_owned()),
            &PlanConfig::new(workspace.clone()),
            provider,
            None,
            CancellationToken::new(),
            &mut session,
        )
        .await
        .expect("plan --new succeeds");

        let new_id = outcome.workflow_id.expect("new workflow id");
        assert_ne!(new_id, old.workflow_id);
        assert_eq!(
            store.load(&old.workflow_id).expect("old workflow").status,
            WorkflowStatus::Paused
        );
        assert_eq!(
            store.active_id().expect("index readable").as_deref(),
            Some(new_id.as_str())
        );
        let session = session.expect("the session is returned");
        assert_eq!(human_texts(&session), vec!["count lines"]);
        let _ = fs::remove_dir_all(&workspace);
    }

    #[tokio::test]
    async fn rejected_answer_records_the_raw_argument() {
        let workspace = temp_workspace();
        let mut session = Some(Session::new("session-answer-blocked").expect("session opens"));

        let error = run_command_with_session(
            PlanCommand::Answer("#1 @answers.md".to_owned()),
            &PlanConfig::new(workspace.clone()),
            Arc::new(PanicProvider),
            None,
            CancellationToken::new(),
            &mut session,
        )
        .await
        .expect_err("no active workflow blocks /answer");

        assert!(error.to_string().contains("no active workflow"), "{error}");
        let session = session.expect("the session is returned");
        assert_eq!(human_texts(&session), vec!["#1 @answers.md"]);
        let _ = fs::remove_dir_all(&workspace);
    }

    #[tokio::test]
    async fn rejected_hint_records_the_question() {
        let workspace = temp_workspace();
        let mut session = Some(Session::new("session-hint-blocked").expect("session opens"));

        let error = run_command_with_session(
            PlanCommand::Hint("why is this needed?".to_owned()),
            &PlanConfig::new(workspace.clone()),
            Arc::new(PanicProvider),
            None,
            CancellationToken::new(),
            &mut session,
        )
        .await
        .expect_err("no active workflow blocks /hint");

        assert!(error.to_string().contains("no active workflow"), "{error}");
        let session = session.expect("the session is returned");
        assert_eq!(human_texts(&session), vec!["why is this needed?"]);
        let _ = fs::remove_dir_all(&workspace);
    }

    #[tokio::test]
    async fn rejected_distill_records_the_explanation() {
        let workspace = temp_workspace();
        let mut session = Some(Session::new("session-distill-blocked").expect("session opens"));

        let error = run_command_with_session(
            PlanCommand::Distill("I implemented it".to_owned()),
            &PlanConfig::new(workspace.clone()),
            Arc::new(PanicProvider),
            None,
            CancellationToken::new(),
            &mut session,
        )
        .await
        .expect_err("no active workflow blocks /distill");

        assert!(error.to_string().contains("no active workflow"), "{error}");
        let session = session.expect("the session is returned");
        assert_eq!(human_texts(&session), vec!["I implemented it"]);
        let _ = fs::remove_dir_all(&workspace);
    }

    #[tokio::test]
    async fn red_tests_reject_distill() {
        let workspace = temp_workspace();
        write_plan(&workspace, plan_text());
        let store = WorkflowStore::new(&workspace);
        let tasks = workflow_tasks(&parse_task_table(plan_text()), &[])
            .into_iter()
            .filter(|task| task.domain == Domain::Known)
            .collect();
        let state = store
            .create_with_tasks("test", None, tasks)
            .expect("workflow");
        let state = store
            .approve(
                &state.workflow_id,
                Some(crate::workflow::TestContract::default()),
                None,
            )
            .expect("approval");
        store
            .record_test(&state.workflow_id, false, Some("assert failed".to_owned()))
            .expect("red test is recorded");

        let error = run_distill(
            &PlanConfig::new(workspace.clone()),
            Arc::new(PanicProvider),
            "I implemented it",
            CancellationToken::new(),
        )
        .await
        .expect_err("red tests block distill");

        assert!(error.to_string().contains("not green"), "{error}");
        let _ = fs::remove_dir_all(&workspace);
    }

    #[tokio::test]
    async fn await_user_rejects_execute_writes() {
        let workspace = temp_workspace();
        write_plan(&workspace, plan_text());
        let store = WorkflowStore::new(&workspace);
        let tasks = workflow_tasks(&parse_task_table(plan_text()), &[]);
        let state = store
            .create_with_tasks("test", None, tasks)
            .expect("workflow");
        let state = store
            .approve(&state.workflow_id, None, None)
            .expect("approval");
        let state = store
            .finish_scaffold(&state.workflow_id, ScaffoldContract::default(), None)
            .expect("scaffold finishes");
        assert_eq!(state.phase, Phase::AwaitUser);

        let error = run_execute(
            &PlanConfig::new(workspace.clone()),
            Arc::new(PanicProvider),
            None,
            CancellationToken::new(),
        )
        .await
        .expect_err("AwaitUser rejects writes");

        assert!(error.to_string().contains("await_user"), "{error}");
        let _ = fs::remove_dir_all(&workspace);
    }

    #[tokio::test]
    async fn known_task_workflow_runs_plan_to_distill() {
        let workspace = temp_workspace();
        seed_cargo_baseline(&workspace);
        let config = PlanConfig::new(workspace.clone());
        let provider: Arc<dyn ModelProvider> = Arc::new(FakeProvider::with_attempts(vec![
            text_response(
                "\
## 证据包
- read src/main.rs

## 认知任务分配表
| 任务 | 认知属性 | 判定依据 | 执行状态 | Agent 行为 | 用户角色 | 最小深度链路 | 验收 |
|---|---|---|---|---|---|---|---|
| 读取输入 | 已知域 | src/main.rs | 可执行 | 自主执行 | 审查者 | read | 打印内容 |
",
            ),
            text_response(
                "\
## Execution report
| 任务 | 结果 | 证据 |
|---|---|---|
| 读取输入 | ok | read 工具输出 |
",
            ),
            text_response(
                "\
## 理解漏洞报告
### 清晰
读取与计数分离。
### 模糊
无。
### 未覆盖边界
空文件。
### 追问
空文件怎么计数？
",
            ),
            text_response(
                "\
## 证据包
- read src/main.rs

## 认知任务分配表
| 任务 | 认知属性 | 判定依据 | 执行状态 | Agent 行为 | 用户角色 | 最小深度链路 | 验收 |
|---|---|---|---|---|---|---|---|
| 读取输入 | 已知域 | src/main.rs | 可执行 | 自主执行 | 审查者 | read | 打印内容 |
",
            ),
        ]));

        let plan = run_plan(
            &config,
            Arc::clone(&provider),
            "count lines",
            CancellationToken::new(),
        )
        .await
        .expect("plan succeeds");
        let workflow_id = plan.workflow_id.expect("plan returns a workflow id");

        let status = run_status(&config, None).await.expect("status succeeds");
        assert!(status.output.contains("phase: review"), "{}", status.output);
        assert!(
            status.output.contains("next: /approve"),
            "{}",
            status.output
        );
        let next = run_next(&config).await.expect("next succeeds");
        assert!(next.output.contains("/approve"), "{}", next.output);

        let approved = run_approve(&config).await.expect("approval succeeds");
        assert!(
            approved.output.contains("phase: test_fix"),
            "{}",
            approved.output
        );

        run_execute(
            &config,
            Arc::clone(&provider),
            None,
            CancellationToken::new(),
        )
        .await
        .expect("execute succeeds");
        let store = WorkflowStore::new(&workspace);
        let state = store.load(&workflow_id).expect("state is readable");
        assert_eq!(state.phase, Phase::Distill);
        assert_eq!(state.test.attempts, 1);
        assert_eq!(
            state.test.last_status,
            Some(crate::workflow::TestStatus::Passed)
        );

        let distill = run_distill(
            &config,
            Arc::clone(&provider),
            "I implemented the line counter",
            CancellationToken::new(),
        )
        .await
        .expect("distill succeeds");
        assert!(
            distill.output.contains("## 理解漏洞报告"),
            "{}",
            distill.output
        );

        let ended = run_workflow(&config, Some("end".to_owned()))
            .await
            .expect("workflow ends");
        assert!(ended.output.contains("status: ended"), "{}", ended.output);
        assert_eq!(store.active_id().expect("index is readable"), None);

        let second = run_plan(
            &config,
            Arc::clone(&provider),
            "count lines again",
            CancellationToken::new(),
        )
        .await
        .expect("a new workflow may start after /workflow end");
        assert_ne!(second.workflow_id.as_deref(), Some(workflow_id.as_str()));
        let _ = fs::remove_dir_all(&workspace);
    }

    #[tokio::test]
    async fn distill_follow_up_must_be_answered_or_accepted() {
        let workspace = temp_workspace();
        write_plan(&workspace, plan_text());
        let config = PlanConfig::new(workspace.clone());
        let store = WorkflowStore::new(&workspace);
        let tasks = workflow_tasks(&parse_task_table(plan_text()), &[])
            .into_iter()
            .filter(|task| task.domain == Domain::Known)
            .collect();
        let state = store
            .create_with_tasks("test", None, tasks)
            .expect("workflow");
        let state = store
            .approve(
                &state.workflow_id,
                Some(crate::workflow::TestContract::default()),
                None,
            )
            .expect("approval");
        store
            .record_test(&state.workflow_id, true, None)
            .expect("green test");

        let provider: Arc<dyn ModelProvider> = Arc::new(FakeProvider::new(&["\
## 理解漏洞报告
### 清晰
读取与计数分离。
### 模糊
无。
### 未覆盖边界
空文件。
### 追问
尾随换行如何计数？
"]));
        run_distill(
            &config,
            provider,
            "I implemented the line counter",
            CancellationToken::new(),
        )
        .await
        .expect("distill succeeds");
        let reported = store.load(&state.workflow_id).expect("state loads");
        assert_eq!(reported.phase, Phase::Distill);
        assert!(reported.distill.as_ref().expect("distill").reported);
        assert_eq!(reported.open_questions().len(), 1);

        let error = run_distill(
            &config,
            Arc::new(PanicProvider),
            "again",
            CancellationToken::new(),
        )
        .await
        .expect_err("an open follow-up blocks a second distill");
        assert!(
            error.to_string().contains("follow-up"),
            "unexpected error: {error}"
        );

        let accepted = run_answer(
            &config,
            Arc::new(PanicProvider),
            "接受剩余缺口并完成",
            CancellationToken::new(),
        )
        .await
        .expect("accepting the gaps completes the workflow");
        assert!(
            accepted.output.contains("workflow 完成"),
            "{}",
            accepted.output
        );
        let done = store.load(&state.workflow_id).expect("state loads");
        assert_eq!(done.phase, Phase::Done);
        assert_eq!(done.status, crate::workflow::WorkflowStatus::Completed);
        assert!(done.distill.as_ref().expect("distill").gaps_accepted);
        assert_eq!(store.active_id().expect("index"), None);
        let artifact = fs::read_to_string(&accepted.path).expect("artifact");
        assert!(artifact.contains("status: done"), "{artifact}");
        assert!(artifact.contains("接受剩余缺口并完成"), "{artifact}");
        let _ = fs::remove_dir_all(&workspace);
    }

    #[tokio::test]
    async fn plan_records_navigator_skills_in_the_context_manifest() {
        let workspace = temp_workspace();
        for (root, dir, name) in [
            (".slowcode/skills", "shared", "shared-skill"),
            (
                ".slowcode/roles/navigator/skills",
                "navigate",
                "navigate-skill",
            ),
            (".slowcode/roles/driver/skills", "drive", "drive-skill"),
        ] {
            let path = workspace.join(root).join(dir);
            fs::create_dir_all(&path).expect("skill dir");
            fs::write(
                path.join("SKILL.md"),
                format!("---\nname: {name}\ndescription: test\n---\nBody.\n"),
            )
            .expect("skill");
        }
        let config = PlanConfig::new(workspace.clone());
        let provider: Arc<dyn ModelProvider> = Arc::new(FakeProvider::new(&["\
## 证据包
- read src/main.rs

## 认知任务分配表
| 任务 | 认知属性 | 判定依据 | 执行状态 | Agent 行为 | 用户角色 | 最小深度链路 | 验收 |
|---|---|---|---|---|---|---|---|
| 读取输入 | 已知域 | src/main.rs | 可执行 | 自主执行 | 审查者 | read | 打印内容 |
"]));
        let plan = run_plan(&config, provider, "count lines", CancellationToken::new())
            .await
            .expect("plan succeeds");
        let state = WorkflowStore::new(&workspace)
            .load(plan.workflow_id.as_deref().expect("workflow id"))
            .expect("state loads");
        let manifest = state.context_manifest.expect("manifest is recorded");
        assert_eq!(
            manifest.skill_names(),
            vec!["navigate-skill", "shared-skill"]
        );
        assert!(
            manifest
                .skill_roots
                .contains(&".slowcode/roles/navigator/skills".to_owned())
        );
        let events = fs::read_to_string(
            WorkflowStore::new(&workspace)
                .root()
                .join(state.workflow_id)
                .join("events.jsonl"),
        )
        .expect("events");
        assert!(events.contains("\"kind\":\"context.manifest\""), "{events}");
        let _ = fs::remove_dir_all(&workspace);
    }

    #[tokio::test]
    async fn plan_audits_a_model_driven_readonly_shell_call() {
        let workspace = temp_workspace();
        let config = PlanConfig::new(workspace.clone());
        let provider: Arc<dyn ModelProvider> = Arc::new(FakeProvider::with_attempts(vec![
            tool_call_response("readonly_shell", serde_json::json!({ "argv": ["ls"] })),
            text_response(
                "\
## 证据包
- read src/main.rs

## 认知任务分配表
| 任务 | 认知属性 | 判定依据 | 执行状态 | Agent 行为 | 用户角色 | 最小深度链路 | 验收 |
|---|---|---|---|---|---|---|---|
| 读取输入 | 已知域 | src/main.rs | 可执行 | 自主执行 | 审查者 | read | 打印内容 |
",
            ),
        ]));
        let plan = run_plan(
            &config,
            provider,
            "inspect the workspace",
            CancellationToken::new(),
        )
        .await
        .expect("plan succeeds");
        let store = WorkflowStore::new(&workspace);
        let state = store
            .load(plan.workflow_id.as_deref().expect("workflow id"))
            .expect("state loads");
        let events = fs::read_to_string(store.root().join(&state.workflow_id).join("events.jsonl"))
            .expect("events");
        assert!(
            events.contains("\"kind\":\"shell.readonly\""),
            "{events}\n\n{}",
            plan.output
        );
        assert!(events.contains("\"argv\":[\"ls\"]"), "{events}");
        let _ = fs::remove_dir_all(&workspace);
    }

    #[tokio::test]
    async fn hint_runs_a_read_only_turn_and_records_the_override() {
        let workspace = temp_workspace();
        write_plan(&workspace, plan_text());
        let config = PlanConfig::new(workspace.clone());
        let store = WorkflowStore::new(&workspace);
        let tasks = workflow_tasks(&parse_task_table(plan_text()), &[]);
        let state = store
            .create_with_tasks("test", None, tasks)
            .expect("workflow");
        let provider: Arc<dyn ModelProvider> = Arc::new(FakeProvider::new(&[
            "Read src/main.rs first, then write the smallest failing test.",
        ]));
        let outcome = run_hint(
            &config,
            provider,
            "where should I start?",
            CancellationToken::new(),
        )
        .await
        .expect("hint succeeds");
        assert!(
            outcome.output.contains("Read src/main.rs"),
            "{}",
            outcome.output
        );
        let reloaded = store.load(&state.workflow_id).expect("state loads");
        assert_eq!(reloaded.phase, Phase::Review);
        assert_eq!(reloaded.status, crate::workflow::WorkflowStatus::Active);
        let events = fs::read_to_string(store.root().join(&state.workflow_id).join("events.jsonl"))
            .expect("events");
        assert!(
            events.contains("\"kind\":\"capability.override\""),
            "{events}"
        );
        assert!(events.contains("\"profile\":\"coach\""), "{events}");
        let manifest = reloaded.context_manifest.expect("manifest");
        assert_eq!(manifest.profile.as_deref(), Some("coach"));
        let status = run_status(&config, None).await.expect("status succeeds");
        assert!(
            status.output.contains("role: navigator / navigator"),
            "{}",
            status.output
        );
        assert!(
            status.output.contains("manifest: review / coach"),
            "{}",
            status.output
        );
        let _ = fs::remove_dir_all(&workspace);
    }

    const SEMI_ANSWER: &str = "\
## 回答判定
| 问题 | 结论 | 缺口 |
|---|---|---|
| q-1 | sufficient | |

## 决策记录
| 任务 | 用户决策 | 新认知属性 | 新执行状态 |
|---|---|---|---|
| 选择缓存策略 | 选 A | 已知域 | 可执行 |
";

    #[tokio::test]
    async fn questions_are_interviewed_then_approved() {
        let workspace = temp_workspace();
        let config = PlanConfig::new(workspace.clone());
        let provider: Arc<dyn ModelProvider> = Arc::new(FakeProvider::with_attempts(vec![
            text_response(question_plan_text()),
            text_response(SEMI_ANSWER),
        ]));

        let plan = run_plan(
            &config,
            Arc::clone(&provider),
            "cache and parser",
            CancellationToken::new(),
        )
        .await
        .expect("plan succeeds");
        let workflow_id = plan.workflow_id.expect("workflow id");
        let store = WorkflowStore::new(&workspace);
        let state = store.load(&workflow_id).expect("state");
        assert_eq!(state.phase, Phase::Interview);
        assert_eq!(state.questions.len(), 2);
        assert_eq!(state.questions[0].kind, QuestionKind::SemiKnown);
        assert_eq!(state.questions[1].kind, QuestionKind::Unknown);

        run_answer(
            &config,
            Arc::clone(&provider),
            "#q-1 A",
            CancellationToken::new(),
        )
        .await
        .expect("semi-known answer succeeds");
        let state = store.load(&workflow_id).expect("state");
        assert_eq!(state.phase, Phase::Interview);
        assert_eq!(state.questions[0].status, QuestionStatus::Answered);
        assert_eq!(state.questions[1].status, QuestionStatus::Open);

        run_answer(
            &config,
            provider,
            "#q-2 自己实现核心",
            CancellationToken::new(),
        )
        .await
        .expect("unknown choice succeeds without a model call");
        let state = store.load(&workflow_id).expect("state");
        assert_eq!(state.phase, Phase::Review);
        assert_eq!(state.questions[1].status, QuestionStatus::Answered);
        assert!(
            state
                .tasks
                .iter()
                .any(|task| task.strategy == Strategy::UserImplementsCore)
        );

        run_approve(&config).await.expect("approval succeeds");
        let state = store.load(&workflow_id).expect("state");
        assert_eq!(state.phase, Phase::Scaffold);
        let _ = fs::remove_dir_all(&workspace);
    }

    #[tokio::test]
    async fn file_answers_record_a_redacted_snapshot() {
        let workspace = temp_workspace();
        fs::write(workspace.join("notes.md"), "选 A\n").expect("notes");
        let config = PlanConfig::new(workspace.clone());
        let provider: Arc<dyn ModelProvider> = Arc::new(FakeProvider::with_attempts(vec![
            text_response(question_plan_text()),
            text_response(SEMI_ANSWER),
        ]));
        let plan = run_plan(
            &config,
            Arc::clone(&provider),
            "cache and parser",
            CancellationToken::new(),
        )
        .await
        .expect("plan succeeds");
        let workflow_id = plan.workflow_id.expect("workflow id");

        run_answer(
            &config,
            provider,
            "#q-1 @notes.md",
            CancellationToken::new(),
        )
        .await
        .expect("file answer succeeds");
        let state = WorkflowStore::new(&workspace)
            .load(&workflow_id)
            .expect("state");
        assert_eq!(state.questions[0].answer_source, Some(AnswerSource::File));
        let snapshot = state.questions[0]
            .answer_snapshot
            .as_ref()
            .expect("snapshot");
        assert_eq!(snapshot.path, "notes.md");
        assert_eq!(snapshot.redactions, 0);
        assert!(
            workspace
                .join(".scratch/workflows")
                .join(&workflow_id)
                .join("inputs")
                .join(format!("{}-notes.md", snapshot.sha256))
                .exists()
        );
        let _ = fs::remove_dir_all(&workspace);
    }

    #[tokio::test]
    async fn insufficient_answers_keep_the_task_table() {
        let workspace = temp_workspace();
        let config = PlanConfig::new(workspace.clone());
        let provider: Arc<dyn ModelProvider> = Arc::new(FakeProvider::with_attempts(vec![
            text_response(question_plan_text()),
            text_response(
                "\
## 回答判定
| 问题 | 结论 | 缺口 |
|---|---|---|
| q-1 | insufficient | 没有说明兼容性 |
",
            ),
        ]));
        let plan = run_plan(
            &config,
            Arc::clone(&provider),
            "cache and parser",
            CancellationToken::new(),
        )
        .await
        .expect("plan succeeds");
        let workflow_id = plan.workflow_id.expect("workflow id");
        let store = WorkflowStore::new(&workspace);
        let before = store.load(&workflow_id).expect("state");

        let outcome = run_answer(&config, provider, "#q-1 maybe", CancellationToken::new())
            .await
            .expect("answer is recorded");
        assert!(
            outcome.output.contains("没有说明兼容性"),
            "{}",
            outcome.output
        );
        let after = store.load(&workflow_id).expect("state");
        assert_eq!(after.tasks, before.tasks);
        assert_eq!(after.questions[0].attempts, 1);
        assert_eq!(after.questions[0].status, QuestionStatus::Open);
        let _ = fs::remove_dir_all(&workspace);
    }

    #[tokio::test]
    async fn secret_files_are_rejected_before_the_model_call() {
        let workspace = temp_workspace();
        fs::write(workspace.join("secret.md"), "sk-abcdefghijklmnopqrstuvwxyz").expect("secret");
        let config = PlanConfig::new(workspace.clone());
        let provider: Arc<dyn ModelProvider> =
            Arc::new(FakeProvider::with_attempts(vec![text_response(
                question_plan_text(),
            )]));
        run_plan(
            &config,
            Arc::clone(&provider),
            "cache and parser",
            CancellationToken::new(),
        )
        .await
        .expect("plan succeeds");

        let error = run_answer(
            &config,
            Arc::new(PanicProvider),
            "#q-1 @secret.md",
            CancellationToken::new(),
        )
        .await
        .expect_err("secret files are rejected");
        assert!(error.to_string().contains("likely secret"), "{error}");
        let _ = fs::remove_dir_all(&workspace);
    }

    #[tokio::test]
    async fn reclassify_adds_a_knowledge_question() {
        let workspace = temp_workspace();
        let config = PlanConfig::new(workspace.clone());
        let provider: Arc<dyn ModelProvider> =
            Arc::new(FakeProvider::with_attempts(vec![text_response(
                question_plan_text(),
            )]));
        let plan = run_plan(
            &config,
            provider,
            "cache and parser",
            CancellationToken::new(),
        )
        .await
        .expect("plan succeeds");
        let workflow_id = plan.workflow_id.expect("workflow id");
        let state = run_reclassify(&config, "缓存一致性语义未定")
            .await
            .expect("reclassify succeeds");
        assert!(
            state.output.contains("phase: interview"),
            "{}",
            state.output
        );
        let state = WorkflowStore::new(&workspace)
            .load(&workflow_id)
            .expect("state");
        assert_eq!(state.questions.len(), 3);
        assert_eq!(state.questions[2].kind, QuestionKind::Knowledge);
        let _ = fs::remove_dir_all(&workspace);
    }

    #[tokio::test]
    async fn choosing_knowledge_adds_a_follow_up_question() {
        let workspace = temp_workspace();
        let config = PlanConfig::new(workspace.clone());
        let provider: Arc<dyn ModelProvider> =
            Arc::new(FakeProvider::with_attempts(vec![text_response(
                question_plan_text(),
            )]));
        let plan = run_plan(
            &config,
            provider,
            "cache and parser",
            CancellationToken::new(),
        )
        .await
        .expect("plan succeeds");
        let workflow_id = plan.workflow_id.expect("workflow id");

        run_answer(
            &config,
            Arc::new(PanicProvider),
            "#q-2 补充知识",
            CancellationToken::new(),
        )
        .await
        .expect("knowledge choice does not call the model");
        let state = WorkflowStore::new(&workspace)
            .load(&workflow_id)
            .expect("state");
        assert_eq!(state.phase, Phase::Interview);
        assert_eq!(state.questions[1].status, QuestionStatus::Answered);
        assert_eq!(state.questions[2].kind, QuestionKind::Knowledge);
        assert_eq!(state.questions[2].status, QuestionStatus::Open);
        let task = state
            .tasks
            .iter()
            .find(|task| task.task_id == "task-2")
            .expect("unknown task");
        assert_eq!(task.strategy, Strategy::AiImplements);
        assert_eq!(task.exec_state, ExecState::PendingDecision);
        let _ = fs::remove_dir_all(&workspace);
    }

    fn seed_red_cargo_project(workspace: &Path) {
        fs::create_dir_all(workspace.join("src")).expect("src directory");
        fs::create_dir_all(workspace.join("tests")).expect("tests directory");
        fs::write(
            workspace.join("Cargo.toml"),
            "[package]\nname = \"fixture\"\nversion = \"0.0.0\"\nedition = \"2021\"\n",
        )
        .expect("cargo manifest");
        fs::write(
            workspace.join("src/lib.rs"),
            "pub fn value() -> u32 {\n    1\n}\n",
        )
        .expect("red implementation");
        fs::write(
            workspace.join("tests/value_test.rs"),
            "#[test]\nfn value_is_two() {\n    assert_eq!(fixture::value(), 2);\n}\n",
        )
        .expect("red test");
    }

    fn known_plan_text() -> &'static str {
        "\
## 证据包
- read src/lib.rs

## 认知任务分配表
| 任务 | 认知属性 | 判定依据 | 执行状态 | Agent 行为 | 用户角色 | 最小深度链路 | 验收 |
|---|---|---|---|---|---|---|---|
| 修复取值 | 已知域 | src/lib.rs 的实现 | 可执行 | 自主执行 | 审查者 | src/lib.rs | tests/value_test.rs |
"
    }

    fn test_fix_contract(workspace: &Path) -> TestContract {
        let bytes = fs::read(workspace.join("tests/value_test.rs")).expect("test file");
        let mut framework_hashes = std::collections::BTreeMap::new();
        framework_hashes.insert(
            "tests/value_test.rs".to_owned(),
            crate::workflow::sha256_hex(&bytes),
        );
        TestContract {
            project_test_command: "cargo test".to_owned(),
            test_selector: "-p fixture".to_owned(),
            expected_failing_tests: Vec::new(),
            expected_passing_tests: Vec::new(),
            test_files: vec!["tests/value_test.rs".to_owned()],
            framework_hashes,
        }
    }

    fn seed_test_fix_workflow(workspace: &Path) -> String {
        write_plan(workspace, known_plan_text());
        let store = WorkflowStore::new(workspace);
        let tasks = workflow_tasks(&parse_task_table(known_plan_text()), &[]);
        let state = store
            .create_with_tasks("test", None, tasks)
            .expect("workflow");
        let state = store
            .approve(
                &state.workflow_id,
                Some(test_fix_contract(workspace)),
                Some("-p fixture".to_owned()),
            )
            .expect("approval");
        assert_eq!(state.phase, Phase::TestFix);
        state.workflow_id
    }

    const KNOWN_CLASSIFICATION: &str = "\
## 失败分类
| 失败 | 分类 | 证据 |
|---|---|---|
| value_is_two | known_regression | 实现返回 1，测试要求 2 |
";

    #[tokio::test]
    async fn test_fix_repairs_a_known_regression_then_passes() {
        let workspace = temp_workspace();
        seed_red_cargo_project(&workspace);
        let workflow_id = seed_test_fix_workflow(&workspace);
        let config = PlanConfig::new(workspace.clone());

        let red = run_test(&config).await.expect("red test runs");
        assert!(
            red.output.contains("failures: value_is_two"),
            "{}",
            red.output
        );
        let provider: Arc<dyn ModelProvider> = Arc::new(FakeProvider::with_attempts(vec![
            write_call_response("src/lib.rs", "pub fn value() -> u32 {\n    2\n}\n"),
            text_response(KNOWN_CLASSIFICATION),
        ]));
        run_fix(&config, provider, None, CancellationToken::new())
            .await
            .expect("fix turn succeeds");
        let state = WorkflowStore::new(&workspace)
            .load(&workflow_id)
            .expect("state");
        assert_eq!(state.phase, Phase::TestFix);
        assert_eq!(state.test.last_status, Some(TestStatus::Failed));

        let green = run_test(&config).await.expect("green test runs");
        let state = WorkflowStore::new(&workspace)
            .load(&workflow_id)
            .expect("state");
        assert_eq!(state.phase, Phase::Distill);
        assert_eq!(state.test.last_status, Some(TestStatus::Passed));
        assert!(green.output.contains("status: passed"), "{}", green.output);
        let _ = fs::remove_dir_all(&workspace);
    }

    #[tokio::test]
    async fn unknown_gap_asks_a_question_and_continue_returns_to_test_fix() {
        let workspace = temp_workspace();
        seed_red_cargo_project(&workspace);
        let workflow_id = seed_test_fix_workflow(&workspace);
        let config = PlanConfig::new(workspace.clone());
        run_test(&config).await.expect("red test runs");
        let classification = "\
## 失败分类
| 失败 | 分类 | 证据 |
|---|---|---|
| value_is_two | unknown_gap | 仓库里没有定义取值规则，无法从现有代码推断期望值 |
";
        let provider: Arc<dyn ModelProvider> =
            Arc::new(FakeProvider::with_attempts(vec![text_response(
                classification,
            )]));
        let fixed = run_fix(&config, provider, None, CancellationToken::new())
            .await
            .expect("fix reports an unknown gap");
        assert!(fixed.output.contains("unknown gap"), "{}", fixed.output);
        let store = WorkflowStore::new(&workspace);
        let state = store.load(&workflow_id).expect("state");
        assert_eq!(state.phase, Phase::Reclassify);
        let question = state
            .questions
            .iter()
            .find(|question| question.status == QuestionStatus::Open)
            .expect("question");
        assert_eq!(question.kind, QuestionKind::Unknown);

        run_answer(
            &config,
            Arc::new(PanicProvider),
            "继续按已知域修复",
            CancellationToken::new(),
        )
        .await
        .expect("continue choice does not call the model");
        let state = store.load(&workflow_id).expect("state");
        assert_eq!(state.phase, Phase::TestFix);
        let _ = fs::remove_dir_all(&workspace);
    }

    #[tokio::test]
    async fn changing_a_locked_test_file_blocks_the_workflow() {
        let workspace = temp_workspace();
        seed_red_cargo_project(&workspace);
        let workflow_id = seed_test_fix_workflow(&workspace);
        let config = PlanConfig::new(workspace.clone());
        run_test(&config).await.expect("red test runs");
        let provider: Arc<dyn ModelProvider> = Arc::new(FakeProvider::with_attempts(vec![
            write_call_response(
                "tests/value_test.rs",
                "#[test]\nfn value_is_two() {\n    assert_eq!(fixture::value(), 1);\n}\n",
            ),
            text_response(KNOWN_CLASSIFICATION),
        ]));
        let error = run_fix(&config, provider, None, CancellationToken::new())
            .await
            .expect_err("locked test change is rejected");
        assert!(error.to_string().contains("locked"), "{error}");
        let state = WorkflowStore::new(&workspace)
            .load(&workflow_id)
            .expect("state");
        assert_eq!(state.status, crate::workflow::WorkflowStatus::Blocked);
        assert_eq!(
            state
                .blocked
                .as_ref()
                .map(|blocked| blocked.reason.as_str()),
            Some("contract")
        );
        let _ = fs::remove_dir_all(&workspace);
    }

    #[tokio::test]
    async fn legacy_round_limit_is_ignored() {
        let workspace = temp_workspace();
        seed_red_cargo_project(&workspace);
        let workflow_id = seed_test_fix_workflow(&workspace);
        fs::create_dir_all(workspace.join(".slowcode")).expect("config directory");
        fs::write(
            workspace.join(".slowcode/workflow.toml"),
            "[test_fix]\nmax_rounds = 1\n",
        )
        .expect("budget config");
        let config = PlanConfig::new(workspace.clone());
        let outcome = run_test(&config).await.expect("red test runs");
        assert!(
            !outcome.output.contains("blocked: budget"),
            "{}",
            outcome.output
        );
        let state = WorkflowStore::new(&workspace)
            .load(&workflow_id)
            .expect("state");
        assert_eq!(state.status, crate::workflow::WorkflowStatus::Active);
        assert_eq!(state.test.rounds, 1);
        let _ = fs::remove_dir_all(&workspace);
    }

    #[tokio::test]
    async fn plan_backfills_usage_into_the_new_workflow_budget() {
        let workspace = temp_workspace();
        let config = PlanConfig::new(workspace.clone());
        let provider: Arc<dyn ModelProvider> =
            Arc::new(FakeProvider::with_attempts(vec![text_response_with_usage(
                known_plan_text(),
                7,
                11,
            )]));

        let outcome = run_command(
            PlanCommand::Plan("analyze the notes".to_owned()),
            &config,
            provider,
            None,
            CancellationToken::new(),
        )
        .await
        .expect("plan succeeds");
        let workflow_id = outcome.workflow_id.expect("workflow id");
        let state = WorkflowStore::new(&workspace)
            .load(&workflow_id)
            .expect("state");
        assert_eq!(state.budget.model_calls, 1);
        assert_eq!(state.budget.tokens, 11);
        assert_eq!(state.budget.enforced_tokens, 11);
        assert_eq!(state.status, WorkflowStatus::Active);
        let _ = fs::remove_dir_all(&workspace);
    }

    #[tokio::test]
    async fn exhausted_token_budget_blocks_the_next_model_command() {
        let workspace = temp_workspace();
        seed_red_cargo_project(&workspace);
        let workflow_id = seed_test_fix_workflow(&workspace);
        fs::create_dir_all(workspace.join(".slowcode")).expect("config directory");
        fs::write(
            workspace.join(".slowcode/workflow.toml"),
            "[budget]\nmax_output_tokens = 10\n",
        )
        .expect("budget config");
        let store = WorkflowStore::new(&workspace);
        store
            .record_model_usage(&workflow_id, 1, 10)
            .expect("usage recorded");
        let config = PlanConfig::new(workspace.clone());

        let outcome = run_command(
            PlanCommand::Hint("what next".to_owned()),
            &config,
            Arc::new(PanicProvider),
            None,
            CancellationToken::new(),
        )
        .await
        .expect("budget block is not an error");

        assert!(
            outcome.output.contains("blocked: token budget"),
            "{}",
            outcome.output
        );
        let state = store.load(&workflow_id).expect("state");
        assert_eq!(state.status, WorkflowStatus::Blocked);
        assert_eq!(
            state
                .blocked
                .as_ref()
                .map(|blocked| blocked.reason.as_str()),
            Some("budget")
        );
        let _ = fs::remove_dir_all(&workspace);
    }

    #[tokio::test]
    async fn a_turn_that_exhausts_the_budget_blocks_after_the_command() {
        let workspace = temp_workspace();
        seed_red_cargo_project(&workspace);
        let workflow_id = seed_test_fix_workflow(&workspace);
        fs::create_dir_all(workspace.join(".slowcode")).expect("config directory");
        fs::write(
            workspace.join(".slowcode/workflow.toml"),
            "[budget]\nmax_output_tokens = 10\n",
        )
        .expect("budget config");
        let config = PlanConfig::new(workspace.clone());
        let provider: Arc<dyn ModelProvider> =
            Arc::new(FakeProvider::with_attempts(vec![text_response_with_usage(
                "coach answer",
                1,
                11,
            )]));

        let outcome = run_command(
            PlanCommand::Hint("what next".to_owned()),
            &config,
            provider,
            None,
            CancellationToken::new(),
        )
        .await
        .expect("hint completes");

        assert!(
            outcome.output.contains("blocked: token budget"),
            "{}",
            outcome.output
        );
        let state = WorkflowStore::new(&workspace)
            .load(&workflow_id)
            .expect("state");
        assert_eq!(state.budget.enforced_tokens, 11);
        assert_eq!(state.status, WorkflowStatus::Blocked);
        let _ = fs::remove_dir_all(&workspace);
    }

    #[test]
    fn every_plan_command_has_a_label() {
        let commands = [
            PlanCommand::Plan("task".to_owned()),
            PlanCommand::Answer("answer".to_owned()),
            PlanCommand::Scaffold,
            PlanCommand::Continue,
            PlanCommand::Test,
            PlanCommand::Fix,
            PlanCommand::Execute,
            PlanCommand::Distill("report".to_owned()),
            PlanCommand::Approve,
            PlanCommand::Status(None),
            PlanCommand::Next,
            PlanCommand::Workflow(None),
            PlanCommand::Reclassify("text".to_owned()),
            PlanCommand::Hint("question".to_owned()),
        ];
        let labels: Vec<&str> = commands.iter().map(PlanCommand::label).collect();
        assert_eq!(
            labels,
            [
                "plan",
                "answer",
                "scaffold",
                "continue",
                "test",
                "fix",
                "execute",
                "distill",
                "approve",
                "status",
                "next",
                "workflow",
                "reclassify",
                "hint",
            ]
        );
    }

    #[tokio::test]
    async fn persisted_approval_provider_records_and_resolves() {
        let workspace = temp_workspace();
        let store = WorkflowStore::new(&workspace);
        let state = store
            .create_with_tasks("approval", None, Vec::new())
            .expect("workflow");
        let provider = PersistedApprovalProvider {
            inner: Arc::new(RecordingApproval {
                requests: AtomicU64::new(0),
                outcome: Some(ApprovalOutcome::AllowedOnce),
            }),
            store: store.clone(),
            workflow_id: state.workflow_id.clone(),
            run: None,
        };
        let request = slowcode_kernel::agent::ApprovalRequest::for_tool(
            slowcode_kernel::session::ApprovalRequestId::new("approval-1"),
            "write".to_owned(),
            slowcode_kernel::model::CallId::new("call-1"),
            Some("scaffold".to_owned()),
            "preview of the write",
        )
        .expect("approval request");
        let outcome = provider
            .request(request, CancellationToken::new())
            .await
            .expect("provider resolves");
        assert!(matches!(outcome, ApprovalOutcome::AllowedOnce));
        let events = store.read_events(&state.workflow_id).expect("events parse");
        assert!(
            events
                .iter()
                .any(|event| event.kind == "approval.requested"),
            "{events:?}"
        );
        assert!(
            events.iter().any(|event| event.kind == "approval.granted"),
            "{events:?}"
        );
        let reloaded = store.load(&state.workflow_id).expect("state");
        assert!(reloaded.pending_approvals.is_empty());
        let _ = fs::remove_dir_all(&workspace);
    }

    fn approval_request() -> slowcode_kernel::agent::ApprovalRequest {
        slowcode_kernel::agent::ApprovalRequest::for_tool(
            slowcode_kernel::session::ApprovalRequestId::new("approval-1"),
            "write".to_owned(),
            slowcode_kernel::model::CallId::new("call-1"),
            Some("scaffold".to_owned()),
            "preview of the write",
        )
        .expect("approval request")
    }

    #[tokio::test]
    async fn a_persisted_approval_marks_the_run_as_waiting_before_the_prompt() {
        let workspace = temp_workspace();
        let store = WorkflowStore::new(&workspace);
        let state = store
            .create_with_tasks("approval-run", None, Vec::new())
            .expect("workflow");
        let runs = run_store(&workspace, &state.workflow_id);
        let run = runs
            .start(
                RunLink {
                    workflow_id: Some(state.workflow_id.clone()),
                    operation: "scaffold".to_owned(),
                },
                8,
            )
            .expect("run");
        runs.transition(&run.run_id, RunStatus::CompilingContext, json!({}))
            .expect("compiling");
        let seen = Arc::new(Mutex::new(None));
        let provider = PersistedApprovalProvider {
            inner: Arc::new(InspectingApproval {
                seen: Arc::clone(&seen),
                runs: runs.clone(),
                run_id: run.run_id.clone(),
                outcome: ApprovalOutcome::AllowedOnce,
            }),
            store: store.clone(),
            workflow_id: state.workflow_id.clone(),
            run: Some(RunRecorder {
                store: runs.clone(),
                run_id: run.run_id.clone(),
            }),
        };

        let outcome = provider
            .request(approval_request(), CancellationToken::new())
            .await
            .expect("provider resolves");
        assert!(matches!(outcome, ApprovalOutcome::AllowedOnce));

        let waiting = seen
            .lock()
            .expect("seen lock")
            .clone()
            .expect("inner saw the run");
        assert_eq!(waiting.status, RunStatus::AwaitingApproval);
        let pending = waiting.pending_tool.expect("pending tool");
        assert_eq!(pending.call_id, "call-1");
        assert_eq!(pending.name, "write");
        assert_eq!(
            pending.arguments_hash,
            hash_arguments("preview of the write")
        );

        let resolved = run_store(&workspace, &state.workflow_id)
            .load(&run.run_id)
            .expect("run reloads");
        assert_eq!(resolved.status, RunStatus::ExecutingTool);
        assert!(resolved.pending_tool.is_none());
        let kinds: Vec<String> = runs
            .read_events(&run.run_id)
            .expect("run events parse")
            .into_iter()
            .map(|event| event.kind)
            .collect();
        assert!(
            kinds.contains(&"run.approval_requested".to_owned()),
            "{kinds:?}"
        );
        assert!(
            kinds.contains(&"run.approval_resolved".to_owned()),
            "{kinds:?}"
        );
        let _ = fs::remove_dir_all(&workspace);
    }

    #[tokio::test]
    async fn a_rejected_approval_returns_the_run_to_compiling_context() {
        let workspace = temp_workspace();
        let store = WorkflowStore::new(&workspace);
        let state = store
            .create_with_tasks("approval-reject", None, Vec::new())
            .expect("workflow");
        let runs = run_store(&workspace, &state.workflow_id);
        let run = runs
            .start(
                RunLink {
                    workflow_id: Some(state.workflow_id.clone()),
                    operation: "scaffold".to_owned(),
                },
                8,
            )
            .expect("run");
        runs.transition(&run.run_id, RunStatus::CompilingContext, json!({}))
            .expect("compiling");
        let provider = PersistedApprovalProvider {
            inner: Arc::new(RecordingApproval {
                requests: AtomicU64::new(0),
                outcome: Some(ApprovalOutcome::Rejected),
            }),
            store: store.clone(),
            workflow_id: state.workflow_id.clone(),
            run: Some(RunRecorder {
                store: runs.clone(),
                run_id: run.run_id.clone(),
            }),
        };

        provider
            .request(approval_request(), CancellationToken::new())
            .await
            .expect("provider resolves");

        let resolved = runs.load(&run.run_id).expect("run reloads");
        assert_eq!(resolved.status, RunStatus::CompilingContext);
        assert!(resolved.pending_tool.is_none());
        let events = runs.read_events(&run.run_id).expect("run events parse");
        let resolved_event = events
            .iter()
            .rev()
            .find(|event| event.kind == "run.approval_resolved")
            .expect("resolved event");
        assert_eq!(resolved_event.payload["detail"]["resolution"], "denied");
        let _ = fs::remove_dir_all(&workspace);
    }

    #[tokio::test]
    async fn a_cancelled_approval_cancels_the_run() {
        let workspace = temp_workspace();
        let store = WorkflowStore::new(&workspace);
        let state = store
            .create_with_tasks("approval-cancel", None, Vec::new())
            .expect("workflow");
        let runs = run_store(&workspace, &state.workflow_id);
        let run = runs
            .start(
                RunLink {
                    workflow_id: Some(state.workflow_id.clone()),
                    operation: "scaffold".to_owned(),
                },
                8,
            )
            .expect("run");
        runs.transition(&run.run_id, RunStatus::CompilingContext, json!({}))
            .expect("compiling");
        let provider = PersistedApprovalProvider {
            inner: Arc::new(RecordingApproval {
                requests: AtomicU64::new(0),
                outcome: Some(ApprovalOutcome::Cancelled),
            }),
            store: store.clone(),
            workflow_id: state.workflow_id.clone(),
            run: Some(RunRecorder {
                store: runs.clone(),
                run_id: run.run_id.clone(),
            }),
        };

        provider
            .request(approval_request(), CancellationToken::new())
            .await
            .expect("provider resolves");

        let cancelled = runs.load(&run.run_id).expect("run reloads");
        assert_eq!(cancelled.status, RunStatus::Cancelled);
        assert!(cancelled.pending_tool.is_none());
        let _ = fs::remove_dir_all(&workspace);
    }

    #[tokio::test]
    async fn scaffold_through_run_command_records_approval_and_completes() {
        let workspace = temp_workspace();
        seed_cargo_baseline(&workspace);
        write_plan(&workspace, plan_text());
        seed_scaffold_workflow(&workspace);
        let state = WorkflowStore::new(&workspace)
            .load_active()
            .expect("active workflow")
            .expect("workflow");
        let config = PlanConfig::new(workspace.clone());
        let provider = FakeProvider::with_attempts(vec![
            write_call_response(
                "src/lib.rs",
                "pub fn parse(value: &str) -> String {\n    // TODO(user)\n    String::new()\n}\n",
            ),
            text_response(scaffold_manifest()),
        ]);
        let approval = Arc::new(RecordingApproval::default());

        run_command(
            PlanCommand::Scaffold,
            &config,
            Arc::new(provider),
            Some(Arc::clone(&approval) as Arc<dyn ApprovalProvider>),
            CancellationToken::new(),
        )
        .await
        .expect("scaffold succeeds");

        assert_eq!(approval.requests.load(Ordering::SeqCst), 1);
        let runs = run_store(&workspace, &state.workflow_id);
        let run = runs
            .load(&only_run_id(&workspace, &state.workflow_id))
            .expect("run loads");
        assert_eq!(run.status, RunStatus::Completed);
        assert!(run.pending_tool.is_none());
        let kinds: Vec<String> = runs
            .read_events(&run.run_id)
            .expect("run events parse")
            .into_iter()
            .map(|event| event.kind)
            .collect();
        assert!(
            kinds.contains(&"run.approval_requested".to_owned()),
            "{kinds:?}"
        );
        assert!(
            kinds.contains(&"run.approval_resolved".to_owned()),
            "{kinds:?}"
        );
        let _ = fs::remove_dir_all(&workspace);
    }

    #[tokio::test]
    async fn a_cancelled_command_records_run_cancelled_without_a_phase_change() {
        let workspace = temp_workspace();
        write_plan(&workspace, plan_text());
        let store = WorkflowStore::new(&workspace);
        let state = store
            .create_with_tasks(
                "cancel",
                None,
                workflow_tasks(&parse_task_table(plan_text()), &[]),
            )
            .expect("workflow");
        let config = PlanConfig::new(workspace.clone());
        let cancellation = CancellationToken::new();
        cancellation.cancel();
        let error = run_command(
            PlanCommand::Hint("what should I do?".to_owned()),
            &config,
            Arc::new(PanicProvider),
            None,
            cancellation,
        )
        .await
        .expect_err("cancelled hint fails");
        assert!(error.to_string().contains("cancelled"), "{error}");

        let events = store.read_events(&state.workflow_id).expect("events parse");
        assert!(
            events.iter().any(|event| event.kind == "run.cancelled"),
            "{events:?}"
        );
        let loaded = store.load(&state.workflow_id).expect("state loads");
        assert_eq!(loaded.phase, state.phase);

        let runs = run_store(&workspace, &state.workflow_id);
        let run = runs
            .load(&only_run_id(&workspace, &state.workflow_id))
            .expect("run loads");
        assert_eq!(run.status, RunStatus::Cancelled);
        assert_eq!(run.operation, "hint");
        assert_eq!(run.workflow_id.as_deref(), Some(state.workflow_id.as_str()));
        let _ = fs::remove_dir_all(&workspace);
    }

    #[tokio::test]
    async fn a_plan_turn_archives_model_calls_under_the_run() {
        let workspace = temp_workspace();
        write_plan(&workspace, plan_text());
        let store = WorkflowStore::new(&workspace);
        let state = store
            .create_with_tasks(
                "audit",
                None,
                workflow_tasks(&parse_task_table(plan_text()), &[]),
            )
            .expect("workflow");
        let config = PlanConfig::new(workspace.clone());
        run_command(
            PlanCommand::Hint("what next?".to_owned()),
            &config,
            Arc::new(FakeProvider::new(&["read the plan"])),
            None,
            CancellationToken::new(),
        )
        .await
        .expect("hint succeeds");

        let events = store.read_events(&state.workflow_id).expect("events parse");
        let call = events
            .iter()
            .find(|event| event.kind == "model.called")
            .expect("model.called is recorded");
        assert_eq!(call.payload["phase"], "review");
        assert_eq!(call.payload["model"], "deepseek-flash");
        assert!(call.payload["prompt_hash"].as_str().is_some());

        let runs = run_store(&workspace, &state.workflow_id);
        let run_id = only_run_id(&workspace, &state.workflow_id);
        let turn = format!("{}-turn-1", state.workflow_id);
        assert_eq!(
            call.payload["response_ref"],
            format!("run-states/{run_id}/model-calls/{turn}/response.txt")
        );
        let directory = workspace
            .join(".scratch/workflows")
            .join(&state.workflow_id)
            .join("run-states")
            .join(&run_id)
            .join("model-calls")
            .join(&turn);
        let request = fs::read_to_string(directory.join("request.txt")).expect("archived request");
        assert!(request.contains("what next?"), "{request}");
        let response =
            fs::read_to_string(directory.join("response.txt")).expect("archived response");
        assert!(response.contains("read the plan"), "{response}");
        assert!(
            !workspace
                .join(".scratch/workflows")
                .join(&state.workflow_id)
                .join("runs")
                .join(&turn)
                .exists(),
            "the workflow must not keep a second copy"
        );

        let run = runs.load(&run_id).expect("run loads");
        assert_eq!(run.status, RunStatus::Completed);
        assert_eq!(run.operation, "hint");
        let kinds: Vec<String> = runs
            .read_events(&run_id)
            .expect("run events parse")
            .into_iter()
            .map(|event| event.kind)
            .collect();
        assert_eq!(
            kinds,
            [
                "run.created",
                "run.transitioned",
                "run.usage",
                "run.model_call",
                "run.completed"
            ]
        );
        assert_eq!(run.budget.model_calls, 1);
        let _ = fs::remove_dir_all(&workspace);
    }

    #[tokio::test]
    async fn a_model_turn_without_a_run_falls_back_to_the_workflow_archive() {
        let workspace = temp_workspace();
        let store = WorkflowStore::new(&workspace);
        let state = store
            .create_with_tasks(
                "fallback",
                None,
                workflow_tasks(&parse_task_table(plan_text()), &[]),
            )
            .expect("workflow");
        let config = PlanConfig::new(workspace.clone());

        record_model_turn(
            &config,
            1,
            "fallback request",
            "fallback response",
            TurnUsage {
                model_calls: 1,
                output_tokens: 4,
            },
        );

        let events = store.read_events(&state.workflow_id).expect("events parse");
        let call = events
            .iter()
            .find(|event| event.kind == "model.called")
            .expect("model.called is recorded");
        let turn = format!("{}-turn-1", state.workflow_id);
        assert_eq!(
            call.payload["response_ref"],
            format!("runs/{turn}/response.txt")
        );
        let directory = workspace
            .join(".scratch/workflows")
            .join(&state.workflow_id)
            .join("runs")
            .join(&turn);
        assert!(directory.join("request.txt").exists());
        assert!(directory.join("response.txt").exists());
        let _ = fs::remove_dir_all(&workspace);
    }

    #[tokio::test]
    async fn a_gated_command_records_a_failed_run() {
        let workspace = temp_workspace();
        write_plan(&workspace, plan_text());
        let store = WorkflowStore::new(&workspace);
        let state = store
            .create_with_tasks(
                "failure",
                None,
                workflow_tasks(&parse_task_table(plan_text()), &[]),
            )
            .expect("workflow");
        let config = PlanConfig::new(workspace.clone());
        let error = run_command(
            PlanCommand::Continue,
            &config,
            Arc::new(PanicProvider),
            None,
            CancellationToken::new(),
        )
        .await
        .expect_err("continue is gated before AwaitUser");
        assert!(!error.to_string().is_empty());

        let runs = run_store(&workspace, &state.workflow_id);
        let run_id = only_run_id(&workspace, &state.workflow_id);
        let run = runs.load(&run_id).expect("run loads");
        assert_eq!(run.status, RunStatus::Failed);
        assert_eq!(run.operation, "continue");
        let kinds: Vec<String> = runs
            .read_events(&run_id)
            .expect("run events parse")
            .into_iter()
            .map(|event| event.kind)
            .collect();
        assert_eq!(kinds, ["run.created", "run.transitioned", "run.failed"]);
        let _ = fs::remove_dir_all(&workspace);
    }

    #[tokio::test]
    async fn a_plan_command_backfills_a_completed_run() {
        let workspace = temp_workspace();
        let config = PlanConfig::new(workspace.clone());
        let outcome = run_command(
            PlanCommand::Plan("count lines".to_owned()),
            &config,
            Arc::new(FakeProvider::new(&["\
## 证据包
- read src

## 认知任务分配表
| 任务 | 认知属性 | 判定依据 | 执行状态 | Agent 行为 | 用户角色 | 最小深度链路 | 验收 |
|---|---|---|---|---|---|---|---|
| 读取输入 | 已知域 | src 目录存在 | 可执行 | 自主执行 | 审查者 | read | 打印内容 |
"])),
            None,
            CancellationToken::new(),
        )
        .await
        .expect("/plan succeeds");
        let workflow_id = outcome.workflow_id.expect("workflow id");

        let runs = run_store(&workspace, &workflow_id);
        let run_id = only_run_id(&workspace, &workflow_id);
        let run = runs.load(&run_id).expect("run loads");
        assert_eq!(run.status, RunStatus::Completed);
        assert_eq!(run.operation, "plan");
        assert_eq!(run.workflow_id.as_deref(), Some(workflow_id.as_str()));

        let events = runs.read_events(&run_id).expect("run events parse");
        let kinds: Vec<&str> = events.iter().map(|event| event.kind.as_str()).collect();
        assert_eq!(kinds, ["run.created", "run.transitioned", "run.completed"]);
        assert_eq!(events[0].payload["workflow_id"], workflow_id);
        assert_eq!(events[1].payload["detail"]["backfilled"], true);
        assert_eq!(events[1].payload["detail"]["workflow_id"], workflow_id);
        let _ = fs::remove_dir_all(&workspace);
    }

    #[tokio::test]
    async fn model_turns_reserve_and_settle_run_budget() {
        let workspace = temp_workspace();
        write_plan(&workspace, plan_text());
        let store = WorkflowStore::new(&workspace);
        let state = store
            .create_with_tasks(
                "budget",
                None,
                workflow_tasks(&parse_task_table(plan_text()), &[]),
            )
            .expect("workflow");
        let config = PlanConfig::new(workspace.clone());
        let provider = FakeProvider::with_attempts(vec![text_response_with_usage("done", 120, 30)]);

        run_command(
            PlanCommand::Hint("what next?".to_owned()),
            &config,
            Arc::new(provider),
            None,
            CancellationToken::new(),
        )
        .await
        .expect("hint succeeds");

        let runs = run_store(&workspace, &state.workflow_id);
        let run = runs
            .load(&only_run_id(&workspace, &state.workflow_id))
            .expect("run loads");
        assert_eq!(run.status, RunStatus::Completed);
        assert_eq!(run.budget.model_calls, 1);
        assert_eq!(run.budget.output_tokens, 30);
        // The kernel still reports only output tokens, so input stays zero.
        assert_eq!(run.budget.input_tokens, 0);

        let kinds: Vec<String> = runs
            .read_events(&run.run_id)
            .expect("run events parse")
            .into_iter()
            .filter(|event| event.kind == "run.usage")
            .map(|event| {
                format!(
                    "{}:{}",
                    event.payload["delta"]["model_calls"], event.payload["delta"]["output_tokens"]
                )
            })
            .collect();
        assert_eq!(kinds, ["1:0", "0:30"]);
        let _ = fs::remove_dir_all(&workspace);
    }

    #[test]
    fn workflow_list_renders_empty_and_populated_states() {
        let empty = render_workflow_list(&crate::workflow::WorkflowListing {
            active: None,
            workflows: Vec::new(),
        });
        assert_eq!(
            empty,
            "active: none\nno workflows; run /plan <task> to start one"
        );

        let listing = crate::workflow::WorkflowListing {
            active: Some("plan-1".to_owned()),
            workflows: vec![crate::workflow::WorkflowSummary {
                workflow_id: "plan-1".to_owned(),
                task: "count lines".to_owned(),
                status: crate::workflow::WorkflowStatus::Active,
                phase: crate::workflow::Phase::Interview,
                created_at: 1,
                updated_at: 2,
            }],
        };
        let rendered = render_workflow_list(&listing);
        assert!(rendered.starts_with("active: plan-1"), "{rendered}");
        assert!(
            rendered.contains("plan-1  active  interview  count lines"),
            "{rendered}"
        );
    }

    #[tokio::test]
    async fn run_command_returns_a_reused_session_after_a_failure() {
        let workspace = temp_workspace();
        let config = PlanConfig::new(workspace.clone());
        let mut session = Some(Session::new("session-reused").expect("session opens"));
        let id = session.as_ref().expect("session").id().as_str().to_owned();

        let result = run_command_with_session(
            PlanCommand::Status(None),
            &config,
            Arc::new(PanicProvider),
            None,
            CancellationToken::new(),
            &mut session,
        )
        .await;

        let _ = result;
        let session = session.expect("the session is returned");
        assert_eq!(session.id().as_str(), id);
        let _ = fs::remove_dir_all(&workspace);
    }

    #[tokio::test]
    async fn run_command_reuses_the_supplied_session_for_model_turns() {
        let workspace = temp_workspace();
        let config = PlanConfig::new(workspace.clone());
        let provider: Arc<dyn ModelProvider> =
            Arc::new(FakeProvider::with_attempts(vec![text_response(
                "\
## 证据包
- read src/main.rs

## 认知任务分配表
| 任务 | 认知属性 | 判定依据 | 执行状态 | Agent 行为 | 用户角色 | 最小深度链路 | 验收 |
|---|---|---|---|---|---|---|---|
| 读取输入 | 已知域 | src/main.rs | 可执行 | 自主执行 | 审查者 | read | 打印内容 |
",
            )]));
        let mut session = Some(Session::new("session-plan-reuse").expect("session opens"));

        let outcome = run_command_with_session(
            PlanCommand::Plan("count lines".to_owned()),
            &config,
            provider,
            None,
            CancellationToken::new(),
            &mut session,
        )
        .await
        .expect("plan succeeds");

        let session = session.expect("the session is returned");
        assert_eq!(session.id().as_str(), "session-plan-reuse");
        assert!(
            session.messages().iter().any(|message| matches!(
                message.role(),
                slowcode_kernel::model::MessageRole::Assistant
            )),
            "the plan response is committed to the shared session"
        );
        let replayed = session.replay_ui_events();
        assert!(
            replayed
                .iter()
                .any(|event| matches!(event.kind, UiEventKind::AssistantMessage { .. })),
            "the assistant answer is replayable"
        );
        let human: Vec<&str> = replayed
            .iter()
            .filter_map(|event| match &event.kind {
                UiEventKind::UserMessage {
                    source: UiMessageSource::Human,
                    text,
                    ..
                } => Some(text.as_str()),
                _ => None,
            })
            .collect();
        assert_eq!(
            human,
            vec!["count lines"],
            "the human task is replayable verbatim"
        );
        assert!(
            session.messages().iter().any(|message| matches!(
                message.source().kind(),
                slowcode_kernel::model::MessageSourceKind::Plugin { plugin, .. }
                    if plugin == "slowcode-plan"
            )),
            "the workflow instruction stays a plugin message"
        );
        assert!(outcome.output.contains("## 证据包"));
        let _ = fs::remove_dir_all(&workspace);
    }

    #[test]
    fn split_trailing_human_keeps_the_last_occurrence() {
        let prompt = "## Plan artifact\n\nPLAN\n\n## User answer\n\nPLAN";
        let (instruction, human) = split_trailing_human(prompt, "PLAN").expect("prompt splits");
        assert_eq!(
            instruction,
            "## Plan artifact\n\nPLAN\n\n## User answer\n\n"
        );
        assert_eq!(human, "PLAN");
    }

    #[test]
    fn split_trailing_human_rejects_a_missing_suffix() {
        assert!(split_trailing_human("## Plan artifact\n\nPLAN", "missing").is_none());
        assert!(split_trailing_human("## Plan artifact\n\nPLAN", "").is_none());
    }

    #[tokio::test]
    async fn sequential_plan_commands_keep_one_session() {
        let workspace = temp_workspace();
        let config = PlanConfig::new(workspace.clone());
        let provider: Arc<dyn ModelProvider> = Arc::new(FakeProvider::with_attempts(vec![
            text_response(
                "\
## 证据包
- read src/main.rs

## 认知任务分配表
| 任务 | 认知属性 | 判定依据 | 执行状态 | Agent 行为 | 用户角色 | 最小深度链路 | 验收 |
|---|---|---|---|---|---|---|---|
| 读取输入 | 已知域 | src/main.rs | 可执行 | 自主执行 | 审查者 | read | 打印内容 |
",
            ),
            text_response("Do the next step."),
        ]));
        let mut session = Some(Session::new("session-sequential").expect("session opens"));

        run_command_with_session(
            PlanCommand::Plan("count lines".to_owned()),
            &config,
            Arc::clone(&provider),
            None,
            CancellationToken::new(),
            &mut session,
        )
        .await
        .expect("plan succeeds");
        let after_plan = session
            .as_ref()
            .expect("session returned after plan")
            .messages()
            .len();

        let hint = run_command_with_session(
            PlanCommand::Hint("what next".to_owned()),
            &config,
            provider,
            None,
            CancellationToken::new(),
            &mut session,
        )
        .await
        .expect("hint succeeds");

        let session = session.expect("the session is returned");
        assert_eq!(session.id().as_str(), "session-sequential");
        assert!(
            session.messages().len() > after_plan,
            "the hint turn appends to the same session"
        );
        assert!(hint.output.contains("Do the next step"), "{}", hint.output);
        let _ = fs::remove_dir_all(&workspace);
    }

    #[test]
    fn plan_session_reuse_requires_returning_the_session() {
        let mut sessions = PlanSession::new(Some(Session::new("session-guard").unwrap()));
        let taken = sessions.take_or_create("slowcode-test-").unwrap();
        assert!(
            sessions.take_or_create("slowcode-test-").is_err(),
            "a second take without a put is rejected"
        );
        sessions.put(taken);
        assert!(sessions.into_session().is_some());
    }

    #[tokio::test]
    async fn run_command_returns_the_session_after_a_model_stream_failure() {
        let workspace = temp_workspace();
        let config = PlanConfig::new(workspace.clone());
        let provider: Arc<dyn ModelProvider> = Arc::new(FakeProvider::with_attempts(vec![vec![
            Ok(StreamChunk::block_start(0, ContentBlockType::Text).unwrap()),
            Ok(StreamChunk::text_delta(0, "partial").unwrap()),
        ]]));
        let mut session = Some(Session::new("session-stream-failure").expect("session opens"));

        let result = run_command_with_session(
            PlanCommand::Plan("count lines".to_owned()),
            &config,
            provider,
            None,
            CancellationToken::new(),
            &mut session,
        )
        .await;

        assert!(result.is_err(), "an incomplete stream fails the turn");
        let session = session.expect("the session is returned after the failure");
        assert_eq!(session.id().as_str(), "session-stream-failure");
        let _ = fs::remove_dir_all(&workspace);
    }
}
