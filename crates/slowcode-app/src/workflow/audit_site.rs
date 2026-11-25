//! Read-only local static audit site for one Plan workflow.
//!
//! The site is a pure projection of durable inputs:
//! `state.json`, `events.jsonl`, the session journal named by
//! `state.session_id`, the approval records, and matching feedback bundles.
//! It never becomes a state source. Generation is best-effort: a missing
//! source degrades one page, it does not block the workflow.
//!
//! Layout under the workspace:
//!
//! ```text
//! .slowcode/audit/<workflow_id>/
//!   index.html
//!   workflow.html
//!   session.html
//!   feedback.html
//!   assets/style.css
//!   assets/data.json
//! ```

use std::{
    fs, io,
    path::{Path, PathBuf},
    sync::atomic::{AtomicU64, Ordering},
};

use serde_json::{Value, json};
use slowcode_kernel::session::{SessionStore, validate_session_id};

use super::{
    Phase, TestStatus, WorkflowError, WorkflowEvent, WorkflowState, WorkflowStore,
    input::redact_secrets,
};

/// Workspace-relative directory that holds every workflow audit site.
pub const AUDIT_DIR: &str = ".slowcode/audit";

const MAX_SESSION_ROWS: usize = 4000;
const MAX_ROW_TEXT: usize = 6000;
const MAX_FEEDBACK_ISSUE: usize = 120_000;

#[derive(Debug, thiserror::Error)]
pub enum AuditError {
    #[error("workflow error: {0}")]
    Workflow(#[from] WorkflowError),
    #[error("cannot write audit site: {0}")]
    Io(#[from] io::Error),
}

/// Which durable inputs were available while rendering.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct Sources {
    pub state: bool,
    pub events: bool,
    pub session: bool,
    pub feedback: bool,
    /// True when `events.jsonl` contained a torn or invalid suffix and only a
    /// valid prefix was used.
    pub recovered_events: bool,
}

impl Sources {
    fn badge(self) -> String {
        let mark = |ok: bool| if ok { "✓" } else { "✗" };
        format!(
            "sources: state {} / events {} / session {} / feedback {}",
            mark(self.state),
            mark(self.events),
            mark(self.session),
            mark(self.feedback)
        )
    }
}

/// One generated workflow site.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct AuditReport {
    pub dir: PathBuf,
    pub files: Vec<PathBuf>,
    pub sources: Sources,
}

/// Result of an explicit rebuild.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct RebuildOutcome {
    pub reports: Vec<AuditReport>,
    pub failures: Vec<(String, String)>,
}

/// `<workspace>/.slowcode/audit`
#[must_use]
pub fn audit_root(workspace: &Path) -> PathBuf {
    workspace.join(".slowcode").join("audit")
}

/// `<workspace>/.slowcode/audit/<workflow_id>`
#[must_use]
pub fn workflow_dir(workspace: &Path, workflow_id: &str) -> PathBuf {
    audit_root(workspace).join(workflow_id)
}

/// Stable entry point shown in the TUI and in `/status`.
#[must_use]
pub fn entry_path(workspace: &Path, workflow_id: &str) -> PathBuf {
    workflow_dir(workspace, workflow_id).join("index.html")
}

/// Generate the site for one in-memory workflow snapshot.
pub fn generate(workspace: &Path, state: &WorkflowState) -> Result<AuditReport, AuditError> {
    let store = WorkflowStore::new(workspace);
    let events_path = store.root().join(&state.workflow_id).join("events.jsonl");
    let events_file_present = events_path.is_file();
    let events = store.read_events(&state.workflow_id).unwrap_or_default();
    let recovered_events = events_file_present && has_torn_suffix(&events_path, events.len());

    let session = load_session(state.session_id.as_deref());
    let feedback = load_feedback(workspace, state.session_id.as_deref());
    let approvals = load_approvals(workspace, &state.workflow_id);
    let sources = Sources {
        state: true,
        events: events_file_present,
        session: session.is_loaded(),
        feedback: !feedback.is_empty(),
        recovered_events,
    };
    let input = RenderInput {
        state,
        events: &events,
        session: &session,
        feedback: &feedback,
        approvals: &approvals,
        sources,
    };
    let dir = workflow_dir(workspace, &state.workflow_id);
    let files = render_site(&input);
    let written = write_site(workspace, &dir, files)?;
    Ok(AuditReport {
        dir,
        files: written,
        sources,
    })
}

/// Best-effort variant used on the write path. A failure is ignored so the
/// workflow journal stays authoritative.
pub(crate) fn refresh_best_effort(workspace: &Path, state: &WorkflowState) {
    let _ = generate(workspace, state);
}

/// Rebuild one site or every site under `.scratch/workflows`.
pub fn rebuild(workspace: &Path, workflow_id: Option<&str>) -> Result<RebuildOutcome, AuditError> {
    let store = WorkflowStore::new(workspace);
    let mut outcome = RebuildOutcome::default();
    let ids: Vec<String> = match workflow_id {
        Some(id) => vec![id.to_owned()],
        None => {
            let mut ids = Vec::new();
            if let Ok(entries) = fs::read_dir(store.root()) {
                for entry in entries.flatten() {
                    let path = entry.path();
                    if !path.is_dir() {
                        continue;
                    }
                    let Some(name) = path.file_name().and_then(|name| name.to_str()) else {
                        continue;
                    };
                    if name.starts_with('.') || name == "run-states" {
                        continue;
                    }
                    ids.push(name.to_owned());
                }
            }
            ids.sort();
            ids
        }
    };
    for id in ids {
        match store.load(&id) {
            Ok(state) => match generate(workspace, &state) {
                Ok(report) => outcome.reports.push(report),
                Err(error) => outcome.failures.push((id, error.to_string())),
            },
            Err(error) => outcome.failures.push((id, error.to_string())),
        }
    }
    Ok(outcome)
}

// ---------------------------------------------------------------------------
// Sources
// ---------------------------------------------------------------------------

enum SessionView {
    /// The workflow has no linked session id.
    Unlinked,
    /// The id is linked but the journal could not be read.
    Missing {
        id: String,
        reason: String,
    },
    Loaded(Transcript),
}

impl SessionView {
    fn is_loaded(&self) -> bool {
        matches!(self, Self::Loaded(_))
    }
}

struct Transcript {
    id: String,
    rows: Vec<SessionRow>,
    usage: SessionUsage,
    total_events: usize,
    recovered: bool,
}

#[derive(Clone, Copy, Debug, Default)]
struct SessionUsage {
    input_tokens: u64,
    output_tokens: u64,
    cache_read_tokens: u64,
    reasoning_tokens: u64,
}

struct SessionRow {
    kind: &'static str,
    at: Option<u64>,
    label: String,
    body: String,
}

struct FeedbackView {
    name: String,
    issue: Option<String>,
    metadata: Option<String>,
    files: Vec<String>,
}

fn load_session(session_id: Option<&str>) -> SessionView {
    let Some(id) = session_id else {
        return SessionView::Unlinked;
    };
    let Ok(session_id) = validate_session_id(id) else {
        return SessionView::Missing {
            id: id.to_owned(),
            reason: "not a durable session id".to_owned(),
        };
    };
    let store = match SessionStore::open_default() {
        Ok(store) => store,
        Err(error) => {
            return SessionView::Missing {
                id: id.to_owned(),
                reason: error.to_string(),
            };
        }
    };
    let bytes = match store.read_journal(&session_id) {
        Ok(bytes) => bytes,
        Err(error) => {
            return SessionView::Missing {
                id: id.to_owned(),
                reason: error.to_string(),
            };
        }
    };
    let text = String::from_utf8_lossy(&bytes);
    SessionView::Loaded(parse_transcript(id, &text))
}

fn parse_transcript(id: &str, text: &str) -> Transcript {
    let mut rows = Vec::new();
    let mut usage = SessionUsage::default();
    let mut total_events = 0_usize;
    let mut recovered = false;
    for line in text.lines() {
        if line.trim().is_empty() {
            continue;
        }
        let Ok(event) = serde_json::from_str::<Value>(line) else {
            recovered = true;
            break;
        };
        total_events += 1;
        let kind = event.get("type").and_then(Value::as_str).unwrap_or("");
        let at = event.get("time").and_then(Value::as_u64);
        let data = event.get("data").unwrap_or(&Value::Null);
        match kind {
            "user/message" => {
                let body = message_text(data);
                if !body.is_empty() {
                    rows.push(SessionRow {
                        kind: "user",
                        at,
                        label: "user".to_owned(),
                        body,
                    });
                }
            }
            "assistant/message" => {
                let body = message_text(data.get("message").unwrap_or(data));
                if !body.is_empty() {
                    rows.push(SessionRow {
                        kind: "assistant",
                        at,
                        label: "assistant".to_owned(),
                        body,
                    });
                }
                if let Some(usage_value) = data.get("usage") {
                    usage.input_tokens = usage.input_tokens.saturating_add(
                        usage_value
                            .get("inputTokens")
                            .and_then(Value::as_u64)
                            .unwrap_or(0),
                    );
                    usage.output_tokens = usage.output_tokens.saturating_add(
                        usage_value
                            .get("outputTokens")
                            .and_then(Value::as_u64)
                            .unwrap_or(0),
                    );
                    usage.cache_read_tokens = usage.cache_read_tokens.saturating_add(
                        usage_value
                            .get("cacheReadTokens")
                            .and_then(Value::as_u64)
                            .unwrap_or(0),
                    );
                    usage.reasoning_tokens = usage.reasoning_tokens.saturating_add(
                        usage_value
                            .get("reasoningTokens")
                            .and_then(Value::as_u64)
                            .unwrap_or(0),
                    );
                }
            }
            "tool/call" => {
                let name = data.get("name").and_then(Value::as_str).unwrap_or("tool");
                let arguments = data.get("arguments").cloned().unwrap_or(Value::Null);
                let body = match arguments {
                    Value::String(text) => text,
                    Value::Null => String::new(),
                    other => serde_json::to_string_pretty(&other).unwrap_or_default(),
                };
                rows.push(SessionRow {
                    kind: "tool-call",
                    at,
                    label: format!("tool call · {name}"),
                    body,
                });
            }
            "tool/result" => {
                let body = message_text(data.get("message").unwrap_or(data));
                let label = if data
                    .get("error")
                    .and_then(Value::as_str)
                    .is_some_and(|value| !value.is_empty())
                {
                    "tool result · error"
                } else {
                    "tool result"
                };
                rows.push(SessionRow {
                    kind: "tool-result",
                    at,
                    label: label.to_owned(),
                    body,
                });
            }
            "turn/end" => {
                let reason = data.get("reason").and_then(Value::as_str).unwrap_or("");
                rows.push(SessionRow {
                    kind: "turn-end",
                    at,
                    label: "turn end".to_owned(),
                    body: reason.to_owned(),
                });
            }
            "error" | "session/error" | "llm/error" => {
                rows.push(SessionRow {
                    kind: "error",
                    at,
                    label: "error".to_owned(),
                    body: serde_json::to_string_pretty(data).unwrap_or_default(),
                });
            }
            _ => {}
        }
        if rows.len() >= MAX_SESSION_ROWS {
            recovered = true;
            break;
        }
    }
    Transcript {
        id: id.to_owned(),
        rows,
        usage,
        total_events,
        recovered,
    }
}

fn message_text(message: &Value) -> String {
    let content = message.get("content").unwrap_or(message);
    match content {
        Value::String(text) => text.clone(),
        Value::Array(blocks) => blocks
            .iter()
            .filter_map(|block| {
                block
                    .get("text")
                    .and_then(Value::as_str)
                    .or_else(|| block.get("content").and_then(Value::as_str))
            })
            .collect::<Vec<_>>()
            .join("\n"),
        Value::Object(_) => content
            .get("text")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_owned(),
        _ => String::new(),
    }
}

fn load_feedback(workspace: &Path, session_id: Option<&str>) -> Vec<FeedbackView> {
    let Some(session_id) = session_id else {
        return Vec::new();
    };
    let dir = workspace.join("feedback").join(session_id);
    if !dir.is_dir() {
        return Vec::new();
    }
    let issue = read_clipped(&dir.join("issue.md"), MAX_FEEDBACK_ISSUE);
    let metadata = read_clipped(&dir.join("metadata.json"), 40_000);
    let mut files = Vec::new();
    if let Ok(entries) = fs::read_dir(&dir) {
        for entry in entries.flatten() {
            if let Some(name) = entry.file_name().to_str() {
                files.push(name.to_owned());
            }
        }
    }
    files.sort();
    vec![FeedbackView {
        name: session_id.to_owned(),
        issue,
        metadata,
        files,
    }]
}

fn read_clipped(path: &Path, limit: usize) -> Option<String> {
    let text = fs::read_to_string(path).ok()?;
    if text.chars().count() <= limit {
        Some(text)
    } else {
        Some(text.chars().take(limit).collect::<String>() + "\n…")
    }
}

fn load_approvals(workspace: &Path, workflow_id: &str) -> Vec<Value> {
    let dir = workspace
        .join(".scratch")
        .join("workflows")
        .join(workflow_id)
        .join("approvals");
    let mut approvals = Vec::new();
    let Ok(entries) = fs::read_dir(dir) else {
        return approvals;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.extension().and_then(|ext| ext.to_str()) != Some("json") {
            continue;
        }
        let Ok(text) = fs::read_to_string(&path) else {
            continue;
        };
        if let Ok(value) = serde_json::from_str::<Value>(&text) {
            approvals.push(value);
        }
    }
    approvals.sort_by_key(|value| {
        value
            .get("requested_at")
            .and_then(Value::as_u64)
            .unwrap_or(0)
    });
    approvals
}

fn has_torn_suffix(path: &Path, valid_events: usize) -> bool {
    let Ok(text) = fs::read_to_string(path) else {
        return false;
    };
    text.lines().filter(|line| !line.trim().is_empty()).count() > valid_events
}

// ---------------------------------------------------------------------------
// Rendering
// ---------------------------------------------------------------------------

struct RenderInput<'a> {
    state: &'a WorkflowState,
    events: &'a [WorkflowEvent],
    session: &'a SessionView,
    feedback: &'a [FeedbackView],
    approvals: &'a [Value],
    sources: Sources,
}

fn render_site(input: &RenderInput<'_>) -> Vec<(String, String)> {
    vec![
        ("index.html".to_owned(), render_index(input)),
        ("workflow.html".to_owned(), render_workflow(input)),
        ("session.html".to_owned(), render_session(input)),
        ("feedback.html".to_owned(), render_feedback(input)),
        ("assets/style.css".to_owned(), CSS.to_owned()),
        ("assets/data.json".to_owned(), render_data(input)),
    ]
}

fn render_index(input: &RenderInput<'_>) -> String {
    let state = input.state;
    let mut body = String::new();
    body.push_str("<h1>总览</h1>");
    body.push_str(&card(
        "workflow",
        &format!(
            "<dl>\
             <dt>id</dt><dd><code>{}</code></dd>\
             <dt>task</dt><dd>{}</dd>\
             <dt>phase</dt><dd>{} ({})</dd>\
             <dt>status</dt><dd>{}</dd>\
             <dt>version</dt><dd>{}</dd>\
             <dt>created</dt><dd>{}</dd>\
             <dt>updated</dt><dd>{}</dd>\
             <dt>workspace</dt><dd><code>{}</code></dd>\
             <dt>session</dt><dd>{}</dd>\
             </dl>",
            escape(&state.workflow_id),
            escape(&state.task),
            escape(state.phase.as_str()),
            escape(state.phase.user_label()),
            escape(state.status.as_str()),
            state.version,
            fmt_time(state.created_at),
            fmt_time(state.updated_at),
            escape(&state.workspace),
            escape(state.session_id.as_deref().unwrap_or("(unlinked)")),
        ),
    ));
    body.push_str(&card("阶段门禁", &render_gates(state)));
    body.push_str(&card(
        "预算",
        &format!(
            "<dl>\
             <dt>model calls</dt><dd>{}</dd>\
             <dt>tokens</dt><dd>{} (enforced {})</dd>\
             <dt>active seconds</dt><dd>{}</dd>\
             <dt>blocked</dt><dd>{}</dd>\
             </dl>",
            state.budget.model_calls,
            state.budget.tokens,
            state.budget.enforced_tokens,
            state.budget.active_seconds,
            match state.blocked.as_ref() {
                Some(blocked) => escape(&blocked.reason),
                None => "no".to_owned(),
            },
        ),
    ));
    body.push_str(&card("未决问题", &render_open_questions(state)));
    body.push_str(&card(
        "下一步",
        &format!(
            "<p><code>{}</code></p>",
            escape(&crate::plan::next_action(state))
        ),
    ));
    body.push_str(&card(
        "审批",
        &render_approvals(input.approvals, state.pending_approvals.len()),
    ));
    page("总览", "index", input, &body)
}

fn render_gates(state: &WorkflowState) -> String {
    let approval = if state.review_is_current() {
        "approved"
    } else {
        "not approved"
    };
    let test = match state.test.last_status {
        Some(TestStatus::Passed) => "passed",
        Some(TestStatus::Failed) => "failed",
        None => "not run",
    };
    let distill = match state.distill.as_ref() {
        None => "not reported",
        Some(distill) if distill.gaps_accepted => "gaps accepted",
        Some(distill) if distill.reported => "reported, follow-up open",
        Some(_) => "not reported",
    };
    format!(
        "<ul class=\"gates\">\
         <li><span>approval</span><strong>{}</strong></li>\
         <li><span>user implementation</span><strong>{}</strong></li>\
         <li><span>tests</span><strong>{}</strong> ({} rounds, {} attempts)</li>\
         <li><span>distill</span><strong>{}</strong></li>\
         </ul>",
        escape(approval),
        if state.phase == Phase::AwaitUser {
            "pending"
        } else {
            "not pending"
        },
        escape(test),
        state.test.rounds,
        state.test.attempts,
        escape(distill),
    )
}

fn render_open_questions(state: &WorkflowState) -> String {
    let questions = state.open_questions();
    if questions.is_empty() {
        return "<p class=\"empty\">no open questions</p>".to_owned();
    }
    let mut out = String::from("<ul class=\"questions\">");
    for question in questions {
        out.push_str(&format!(
            "<li><code>{}</code><pre>{}</pre></li>",
            escape(&question.question_id),
            escape(&question.render()),
        ));
    }
    out.push_str("</ul>");
    out
}

fn render_approvals(approvals: &[Value], pending: usize) -> String {
    let mut out = format!("<p>pending approvals in state: {pending}</p>");
    if approvals.is_empty() {
        out.push_str("<p class=\"empty\">no approval records</p>");
        return out;
    }
    out.push_str("<table><thead><tr><th>id</th><th>tool</th><th>status</th><th>risk</th><th>approver</th><th>requested</th></tr></thead><tbody>");
    for approval in approvals {
        let field = |name: &str| {
            approval
                .get(name)
                .and_then(Value::as_str)
                .unwrap_or("")
                .to_owned()
        };
        out.push_str(&format!(
            "<tr><td><code>{}</code></td><td>{}</td><td>{}</td><td>{}</td><td>{}</td><td>{}</td></tr>",
            escape(&field("approval_id")),
            escape(&field("tool_name")),
            escape(&field("status")),
            escape(&field("risk")),
            escape(&field("approver")),
            fmt_time(approval.get("requested_at").and_then(Value::as_u64).unwrap_or(0)),
        ));
    }
    out.push_str("</tbody></table>");
    out
}

fn render_workflow(input: &RenderInput<'_>) -> String {
    let mut body = String::from("<h1>workflow 节点</h1>");
    if input.sources.recovered_events {
        body.push_str("<p class=\"warn\">recovered prefix: only the valid event prefix was rendered; later events may be missing.</p>");
    }
    body.push_str(&card(
        "契约与证据",
        &format!(
            "<dl>\
             <dt>table hash</dt><dd><code>{}</code></dd>\
             <dt>test selector</dt><dd>{}</dd>\
             <dt>test contract</dt><dd>{}</dd>\
             <dt>scaffold</dt><dd>{}</dd>\
             </dl>",
            escape(&input.state.table_hash()),
            escape(
                input
                    .state
                    .test_skip
                    .as_ref()
                    .map(|skip| skip.reason.as_str())
                    .or(input.state.test_selector.as_deref())
                    .unwrap_or("(unresolved)"),
            ),
            match input.state.test_contract.as_ref() {
                Some(contract) => format!(
                    "<code>{}</code> · {} test files",
                    escape(&contract.project_test_command),
                    contract.test_files.len()
                ),
                None => "not recorded".to_owned(),
            },
            match input.state.scaffold.as_ref() {
                Some(contract) => format!(
                    "{} framework files · {} core regions",
                    contract.framework_files.len(),
                    contract.core_regions.len()
                ),
                None => "not recorded".to_owned(),
            },
        ),
    ));
    body.push_str(&card("节点时间线", &render_timeline(input.events)));
    body.push_str(&card(
        "审批记录",
        &render_approvals(input.approvals, input.state.pending_approvals.len()),
    ));
    page("workflow 节点", "workflow", input, &body)
}

fn render_timeline(events: &[WorkflowEvent]) -> String {
    if events.is_empty() {
        return "<p class=\"empty\">no events recorded</p>".to_owned();
    }
    let mut out = String::from("<div class=\"timeline\">");
    for event in events {
        out.push_str(&format!(
            "<article class=\"event\">\
             <header><span class=\"seq\">#{}</span> <strong>{}</strong> \
             <span class=\"meta\">{} · phase {} · {}</span></header>",
            event.seq,
            escape(&event.kind),
            fmt_time(event.at),
            escape(event.phase.as_str()),
            escape(&event.transition_id),
        ));
        if let Some(payload) = render_event_payload(event) {
            out.push_str(&format!("<pre>{}</pre>", escape(&payload)));
        }
        out.push_str("</article>");
    }
    out.push_str("</div>");
    out
}

fn render_event_payload(event: &WorkflowEvent) -> Option<String> {
    let text = serde_json::to_string_pretty(&event.payload).ok()?;
    if text == "{}" || text == "null" {
        return None;
    }
    Some(clip(&text, 4000))
}

fn render_session(input: &RenderInput<'_>) -> String {
    let mut body = String::from("<h1>会话</h1>");
    match input.session {
        SessionView::Unlinked => {
            body.push_str(
                "<p class=\"empty\">source missing: this workflow has no linked session id.</p>",
            );
        }
        SessionView::Missing { id, reason } => {
            body.push_str(&format!(
                "<p class=\"warn\">source missing: session journal <code>{}</code> ({})</p>",
                escape(id),
                escape(reason),
            ));
        }
        SessionView::Loaded(transcript) => {
            if transcript.recovered {
                body.push_str("<p class=\"warn\">recovered prefix: transcript was truncated while reading the journal.</p>");
            }
            body.push_str(&card(
                "session",
                &format!(
                    "<dl>\
                     <dt>id</dt><dd><code>{}</code></dd>\
                     <dt>journal events</dt><dd>{}</dd>\
                     <dt>rendered rows</dt><dd>{}</dd>\
                     <dt>tokens</dt><dd>in {} / out {} / cache read {} / reasoning {}</dd>\
                     </dl>",
                    escape(&transcript.id),
                    transcript.total_events,
                    transcript.rows.len(),
                    transcript.usage.input_tokens,
                    transcript.usage.output_tokens,
                    transcript.usage.cache_read_tokens,
                    transcript.usage.reasoning_tokens,
                ),
            ));
            if transcript.rows.is_empty() {
                body.push_str("<p class=\"empty\">journal contains no renderable turns</p>");
            } else {
                body.push_str("<div class=\"transcript\">");
                for row in &transcript.rows {
                    body.push_str(&format!(
                        "<article class=\"row row-{}\"><header><span class=\"kind\">{}</span>\
                         <span class=\"meta\">{}</span></header><pre>{}</pre></article>",
                        row.kind,
                        escape(&row.label),
                        row.at.map(fmt_time).unwrap_or_default(),
                        escape(&clip(&row.body, MAX_ROW_TEXT)),
                    ));
                }
                body.push_str("</div>");
            }
        }
    }
    page("会话", "session", input, &body)
}

fn render_feedback(input: &RenderInput<'_>) -> String {
    let mut body = String::from("<h1>feedback</h1>");
    if input.feedback.is_empty() {
        body.push_str(
            "<p class=\"empty\">no feedback bundle is linked to this workflow's session.</p>",
        );
    } else {
        for bundle in input.feedback {
            let files = bundle
                .files
                .iter()
                .map(|name| format!("<li><code>{}</code></li>", escape(name)))
                .collect::<String>();
            body.push_str(&card(
                &format!("bundle {}", bundle.name),
                &format!(
                    "<p>source directory: <code>feedback/{}</code></p><ul>{files}</ul>",
                    escape(&bundle.name),
                ),
            ));
            if let Some(metadata) = &bundle.metadata {
                body.push_str(&card(
                    "metadata",
                    &format!("<pre>{}</pre>", escape(metadata)),
                ));
            }
            if let Some(issue) = &bundle.issue {
                body.push_str(&card("issue.md", &format!("<pre>{}</pre>", escape(issue))));
            }
        }
    }
    body.push_str("<p class=\"note\">feedback bundles can contain conversation content. This site is local only; review before sharing.</p>");
    page("feedback", "feedback", input, &body)
}

fn render_data(input: &RenderInput<'_>) -> String {
    let state = input.state;
    let data = json!({
        "workflow_id": &state.workflow_id,
        "task": &state.task,
        "phase": state.phase.as_str(),
        "status": state.status.as_str(),
        "version": state.version,
        "created_at": state.created_at,
        "updated_at": state.updated_at,
        "session_id": state.session_id.as_deref(),
        "sources": {
            "state": input.sources.state,
            "events": input.sources.events,
            "session": input.sources.session,
            "feedback": input.sources.feedback,
            "recovered_events": input.sources.recovered_events,
        },
        "counts": {
            "events": input.events.len(),
            "tasks": state.tasks.len(),
            "open_questions": state.open_questions().len(),
            "approvals": input.approvals.len(),
            "feedback": input.feedback.len(),
        },
        "budget": {
            "model_calls": state.budget.model_calls,
            "tokens": state.budget.tokens,
            "enforced_tokens": state.budget.enforced_tokens,
            "active_seconds": state.budget.active_seconds,
        },
    });
    serde_json::to_string_pretty(&data).unwrap_or_else(|_| "{}".to_owned())
}

fn card(title: &str, inner: &str) -> String {
    format!(
        "<section class=\"card\"><h2>{}</h2>{inner}</section>",
        escape(title)
    )
}

fn page(title: &str, current: &str, input: &RenderInput<'_>, body: &str) -> String {
    let nav = [
        ("index", "总览"),
        ("workflow", "workflow"),
        ("session", "会话"),
        ("feedback", "feedback"),
    ]
    .iter()
    .map(|(id, label)| {
        let class = if *id == current {
            "nav-link current"
        } else {
            "nav-link"
        };
        format!("<a class=\"{class}\" href=\"./{id}.html\">{label}</a>")
    })
    .collect::<String>();
    format!(
        "<!doctype html><html lang=\"zh-CN\"><head><meta charset=\"utf-8\">\
         <meta name=\"viewport\" content=\"width=device-width, initial-scale=1\">\
         <title>{} · {}</title><link rel=\"stylesheet\" href=\"./assets/style.css\"></head>\
         <body><div class=\"layout\"><aside class=\"sidebar\">\
         <a class=\"brand\" href=\"./index.html\"><strong>slowcode audit</strong>\
         <span>{}</span></a>{nav}\
         <div class=\"source-status\">{}</div></aside>\
         <main class=\"content\"><div class=\"content-inner\">\
         <p class=\"crumb\">{}</p>{body}\
         <footer>generated by <code>slowcode audit</code> · workflow <code>{}</code> · \
         read-only projection; the workflow journal is authoritative</footer>\
         </div></main></div></body></html>",
        escape(title),
        escape(&input.state.workflow_id),
        escape(&short_task(&input.state.task)),
        escape(&input.sources.badge()),
        escape(title),
        escape(&input.state.workflow_id),
    )
}

fn short_task(task: &str) -> String {
    let text = clip(task, 80);
    if text.is_empty() {
        "(untitled)".to_owned()
    } else {
        text
    }
}

// ---------------------------------------------------------------------------
// Writing
// ---------------------------------------------------------------------------

fn write_site(
    workspace: &Path,
    dir: &Path,
    files: Vec<(String, String)>,
) -> Result<Vec<PathBuf>, AuditError> {
    fs::create_dir_all(dir.join("assets"))?;
    let mut written = Vec::with_capacity(files.len());
    let mut expected = Vec::with_capacity(files.len());
    for (name, content) in files {
        let path = dir.join(&name);
        let (redacted, _) = redact_secrets(&content, workspace);
        write_atomic(&path, redacted.as_bytes())?;
        expected.push(path.clone());
        written.push(path);
    }
    prune(dir, &expected);
    Ok(written)
}

fn prune(dir: &Path, expected: &[PathBuf]) {
    fn collect(directory: &Path, found: &mut Vec<PathBuf>) {
        let Ok(entries) = fs::read_dir(directory) else {
            return;
        };
        for entry in entries.flatten() {
            let path = entry.path();
            if path.is_dir() {
                collect(&path, found);
            } else {
                found.push(path);
            }
        }
    }
    let mut found = Vec::new();
    collect(dir, &mut found);
    for path in found {
        if expected.contains(&path) {
            continue;
        }
        let Some(name) = path.file_name().and_then(|name| name.to_str()) else {
            continue;
        };
        if name.starts_with('.') {
            continue;
        }
        let _ = fs::remove_file(path);
    }
}

fn write_atomic(path: &Path, bytes: &[u8]) -> Result<(), io::Error> {
    static COUNTER: AtomicU64 = AtomicU64::new(0);
    let serial = COUNTER.fetch_add(1, Ordering::Relaxed);
    let file_name = path
        .file_name()
        .map(|name| name.to_string_lossy().into_owned())
        .unwrap_or_else(|| "audit".to_owned());
    let temporary =
        path.with_file_name(format!(".{file_name}.tmp-{}-{serial}", std::process::id()));
    fs::write(&temporary, bytes)?;
    if let Err(error) = fs::rename(&temporary, path) {
        let _ = fs::remove_file(&temporary);
        return Err(error);
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// Formatting helpers
// ---------------------------------------------------------------------------

fn escape(value: &str) -> String {
    let mut escaped = String::with_capacity(value.len());
    for character in value.chars() {
        match character {
            '&' => escaped.push_str("&amp;"),
            '<' => escaped.push_str("&lt;"),
            '>' => escaped.push_str("&gt;"),
            '"' => escaped.push_str("&quot;"),
            '\'' => escaped.push_str("&#39;"),
            other => escaped.push(other),
        }
    }
    escaped
}

fn clip(text: &str, limit: usize) -> String {
    if text.chars().count() <= limit {
        return text.to_owned();
    }
    let mut clipped = text.chars().take(limit).collect::<String>();
    clipped.push_str("\n…");
    clipped
}

fn fmt_time(ms: u64) -> String {
    if ms == 0 {
        return "-".to_owned();
    }
    let seconds = ms / 1000;
    let days = (seconds / 86_400) as i64;
    let remainder = seconds % 86_400;
    let (year, month, day) = civil_from_days(days);
    format!(
        "{year:04}-{month:02}-{day:02} {:02}:{:02}:{:02}Z",
        remainder / 3600,
        (remainder % 3600) / 60,
        remainder % 60
    )
}

/// Howard Hinnant's `civil_from_days`, shifted to the Unix epoch.
fn civil_from_days(days: i64) -> (i64, u32, u32) {
    let z = days + 719_468;
    let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
    let doe = (z - era * 146_097) as u64;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let year = yoe as i64 + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let day = (doy - (153 * mp + 2) / 5 + 1) as u32;
    let month = if mp < 10 { mp + 3 } else { mp - 9 } as u32;
    (if month <= 2 { year + 1 } else { year }, month, day)
}

const CSS: &str = "\
:root { color-scheme: light dark; --bg:#fbfbfa; --panel:#fff; --ink:#1f2328; \
--muted:#656d76; --line:#d8dee4; --accent:#0b6bcb; --warn:#9a6700; --danger:#cf222e; }
@media (prefers-color-scheme: dark) { :root { --bg:#0d1117; --panel:#161b22; \
--ink:#e6edf3; --muted:#8b949e; --line:#30363d; --accent:#58a6ff; --warn:#d29922; \
--danger:#f85149; } }
* { box-sizing: border-box; }
body { margin:0; background:var(--bg); color:var(--ink); \
font:14px/1.6 -apple-system,BlinkMacSystemFont,'Segoe UI',Helvetica,Arial,sans-serif; }
.layout { display:flex; min-height:100vh; }
.sidebar { width:230px; flex:0 0 230px; padding:18px 14px; border-right:1px solid var(--line); \
background:var(--panel); position:sticky; top:0; height:100vh; overflow:auto; }
.brand { display:block; margin-bottom:14px; text-decoration:none; color:inherit; }
.brand strong { display:block; font-size:15px; }
.brand span { color:var(--muted); font-size:12px; }
.nav-link { display:block; padding:6px 9px; border-radius:6px; color:var(--ink); \
text-decoration:none; }
.nav-link:hover { background:rgba(127,127,127,.12); }
.nav-link.current { background:rgba(11,107,203,.14); color:var(--accent); font-weight:600; }
.source-status { margin-top:16px; padding-top:12px; border-top:1px solid var(--line); \
color:var(--muted); font-size:12px; }
.content { flex:1; min-width:0; }
.content-inner { max-width:980px; margin:0 auto; padding:24px 28px 60px; }
.crumb { color:var(--muted); font-size:12px; text-transform:uppercase; letter-spacing:.06em; }
h1 { font-size:22px; margin:6px 0 18px; }
h2 { font-size:15px; margin:0 0 12px; }
.card { background:var(--panel); border:1px solid var(--line); border-radius:10px; \
padding:16px 18px; margin-bottom:16px; }
dl { display:grid; grid-template-columns:max-content 1fr; gap:6px 18px; margin:0; }
dt { color:var(--muted); }
dd { margin:0; overflow-wrap:anywhere; }
code { font-family:ui-monospace,SFMono-Regular,Menlo,monospace; font-size:.92em; }
pre { margin:8px 0 0; padding:10px 12px; background:rgba(127,127,127,.09); \
border-radius:8px; overflow:auto; white-space:pre-wrap; overflow-wrap:anywhere; \
font-family:ui-monospace,SFMono-Regular,Menlo,monospace; font-size:12.5px; }
.gates { list-style:none; padding:0; margin:0; display:grid; grid-template-columns:1fr 1fr; gap:8px; }
.gates li { display:flex; justify-content:space-between; gap:10px; padding:8px 10px; \
border:1px solid var(--line); border-radius:8px; }
.gates span { color:var(--muted); }
.empty { color:var(--muted); }
.warn { color:var(--warn); }
.note { color:var(--muted); font-size:12px; }
.questions { padding-left:18px; }
.timeline { border-left:2px solid var(--line); margin-left:6px; padding-left:16px; }
.event { margin-bottom:14px; }
.event header { display:flex; flex-wrap:wrap; gap:8px; align-items:baseline; }
.seq { color:var(--muted); }
.meta { color:var(--muted); font-size:12px; }
.transcript .row { border-left:3px solid var(--line); padding-left:12px; margin-bottom:14px; }
.row-user { border-left-color:var(--accent) !important; }
.row-assistant { border-left-color:#2da44e !important; }
.row-tool-call { border-left-color:var(--warn) !important; }
.row-tool-result { border-left-color:#8250df !important; }
.row-error { border-left-color:var(--danger) !important; }
.row header { display:flex; gap:10px; align-items:baseline; }
.kind { font-weight:600; }
table { width:100%; border-collapse:collapse; }
th, td { text-align:left; padding:6px 8px; border-bottom:1px solid var(--line); \
vertical-align:top; overflow-wrap:anywhere; }
th { color:var(--muted); font-weight:600; font-size:12px; }
footer { margin-top:28px; padding-top:14px; border-top:1px solid var(--line); \
color:var(--muted); font-size:12px; overflow-wrap:anywhere; }
@media (max-width:720px) { .layout { display:block; } \
.sidebar { width:auto; height:auto; position:static; border-right:0; border-bottom:1px solid var(--line); } \
.content-inner { padding:18px 16px 40px; } .gates { grid-template-columns:1fr; } }
";

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicU64, Ordering};

    fn temp_workspace(name: &str) -> PathBuf {
        static COUNTER: AtomicU64 = AtomicU64::new(0);
        let serial = COUNTER.fetch_add(1, Ordering::Relaxed);
        let root = std::env::temp_dir().join(format!(
            "slowcode-audit-{name}-{}-{serial}",
            std::process::id()
        ));
        let _ = fs::remove_dir_all(&root);
        fs::create_dir_all(&root).expect("workspace");
        root
    }

    fn created_state(workspace: &Path) -> WorkflowState {
        WorkflowStore::new(workspace)
            .create_with_tasks("audit the workflow", None, Vec::new())
            .expect("create workflow")
    }

    #[test]
    fn every_page_is_written_and_linked() {
        let workspace = temp_workspace("pages");
        let state = created_state(&workspace);
        let report = generate(&workspace, &state).expect("generate");

        assert!(report.sources.state);
        assert!(report.sources.events);
        assert!(!report.sources.session);
        for name in [
            "index.html",
            "workflow.html",
            "session.html",
            "feedback.html",
            "assets/style.css",
            "assets/data.json",
        ] {
            assert!(report.dir.join(name).is_file(), "missing {name}");
        }
        let index = fs::read_to_string(report.dir.join("index.html")).expect("index");
        assert!(index.contains("audit the workflow"), "{index}");
        assert!(index.contains("review"), "{index}");
        assert!(index.contains("sources: state ✓ / events ✓ / session ✗ / feedback ✗"));

        let session = fs::read_to_string(report.dir.join("session.html")).expect("session");
        assert!(session.contains("no linked session id"), "{session}");

        let feedback = fs::read_to_string(report.dir.join("feedback.html")).expect("feedback");
        assert!(feedback.contains("no feedback bundle"), "{feedback}");
    }

    #[test]
    fn state_changes_refresh_the_generated_site() {
        let workspace = temp_workspace("refresh");
        let store = WorkflowStore::new(&workspace);
        let state = created_state(&workspace);
        let updated = store
            .record_model_usage(&state.workflow_id, 3, 42)
            .expect("usage");
        let index =
            fs::read_to_string(workflow_dir(&workspace, &state.workflow_id).join("index.html"))
                .expect("index");
        assert!(updated.version > state.version);
        assert!(index.contains("42"), "{index}");
        assert!(index.contains(&format!(">{}<", updated.version)), "{index}");
    }

    #[test]
    fn pages_are_offline_and_internal_links_resolve() {
        let workspace = temp_workspace("offline");
        let state = created_state(&workspace);
        let report = generate(&workspace, &state).expect("generate");

        for path in report
            .files
            .iter()
            .filter(|path| path.extension().and_then(|ext| ext.to_str()) == Some("html"))
        {
            let html = fs::read_to_string(path).expect("html");
            assert!(
                !html.contains("http://"),
                "{} links to the network",
                path.display()
            );
            assert!(
                !html.contains("https://"),
                "{} links to the network",
                path.display()
            );
            for href in html.split("href=\"").skip(1) {
                let target = href.split('"').next().unwrap_or_default();
                if target.is_empty() || target.starts_with('#') {
                    continue;
                }
                let resolved = path.parent().unwrap_or(&report.dir).join(target);
                assert!(
                    resolved.is_file(),
                    "{} links to missing {}",
                    path.display(),
                    resolved.display()
                );
            }
        }
    }

    #[test]
    fn torn_event_suffix_is_reported_as_a_recovered_prefix() {
        let workspace = temp_workspace("torn");
        let state = created_state(&workspace);
        let path = WorkflowStore::new(&workspace)
            .root()
            .join(&state.workflow_id)
            .join("events.jsonl");
        let mut events = fs::read_to_string(&path).expect("events");
        events.push_str("{\"torn\":");
        fs::write(&path, events).expect("append torn line");

        let report = generate(&workspace, &state).expect("generate");
        assert!(report.sources.recovered_events);
        let workflow = fs::read_to_string(report.dir.join("workflow.html")).expect("workflow");
        assert!(workflow.contains("recovered prefix"), "{workflow}");
    }

    #[test]
    fn rebuild_covers_every_workflow_directory() {
        let workspace = temp_workspace("rebuild");
        let store = WorkflowStore::new(&workspace);
        let first = created_state(&workspace);
        store.end(&first.workflow_id).expect("end first");
        let second = store
            .create_with_tasks("second workflow", None, Vec::new())
            .expect("second");

        let outcome = rebuild(&workspace, None).expect("rebuild");
        assert_eq!(outcome.reports.len(), 2, "{outcome:?}");
        assert!(outcome.failures.is_empty(), "{outcome:?}");
        let ids = outcome
            .reports
            .iter()
            .map(|report| {
                report
                    .dir
                    .file_name()
                    .unwrap()
                    .to_string_lossy()
                    .into_owned()
            })
            .collect::<Vec<_>>();
        assert!(ids.contains(&first.workflow_id));
        assert!(ids.contains(&second.workflow_id));
    }

    #[test]
    fn rebuild_reports_an_unknown_workflow() {
        let workspace = temp_workspace("unknown");
        let outcome = rebuild(&workspace, Some("plan-0")).expect("rebuild");
        assert!(outcome.reports.is_empty());
        assert_eq!(outcome.failures.len(), 1);
    }

    #[test]
    fn stale_pages_are_pruned() {
        let workspace = temp_workspace("prune");
        let state = created_state(&workspace);
        let dir = workflow_dir(&workspace, &state.workflow_id);
        fs::create_dir_all(&dir).expect("dir");
        fs::write(dir.join("stale.html"), "old").expect("stale");
        generate(&workspace, &state).expect("generate");
        assert!(!dir.join("stale.html").exists());
    }

    #[test]
    fn feedback_bundle_for_the_session_is_rendered() {
        let workspace = temp_workspace("feedback");
        let mut state = created_state(&workspace);
        // Invalid on purpose: feedback loads without opening the session store.
        state.session_id = Some("session-not-a-uuid".to_owned());
        let bundle = workspace.join("feedback").join("session-not-a-uuid");
        fs::create_dir_all(&bundle).expect("bundle");
        fs::write(
            bundle.join("issue.md"),
            "issue body with sk-abcdefghijklmnopqrstuvwx",
        )
        .expect("issue");

        let report = generate(&workspace, &state).expect("generate");
        assert!(report.sources.feedback);
        let page = fs::read_to_string(report.dir.join("feedback.html")).expect("feedback");
        assert!(page.contains("issue body"), "{page}");
        assert!(!page.contains("sk-abcdefghijklmnopqrstuvwx"), "{page}");
        assert!(page.contains("[REDACTED:"), "{page}");
    }

    #[test]
    fn data_json_projects_counts_and_sources() {
        let workspace = temp_workspace("data");
        let state = created_state(&workspace);
        let report = generate(&workspace, &state).expect("generate");
        let text = fs::read_to_string(report.dir.join("assets/data.json")).expect("data");
        let value: Value = serde_json::from_str(&text).expect("json");
        assert_eq!(value["workflow_id"], state.workflow_id);
        assert_eq!(value["sources"]["events"], true);
        assert_eq!(value["counts"]["events"], 2);
    }

    #[test]
    fn transcript_parses_user_assistant_tool_and_usage() {
        let journal = [
            r#"{"type":"session","id":"session-x"}"#,
            r#"{"type":"user/message","time":1000,"data":{"content":[{"text":"hello"}]}}"#,
            r#"{"type":"tool/call","time":2000,"data":{"name":"read","arguments":{"path":"a"}}}"#,
            r#"{"type":"assistant/message","time":3000,"data":{"message":{"content":[{"text":"hi"}]},"usage":{"inputTokens":5,"outputTokens":2,"cacheReadTokens":7,"reasoningTokens":1}}}"#,
            r#"{"type":"tool/result","time":4000,"data":{"message":{"content":[{"type":"toolResult","content":"ok"}]}}}"#,
            r#"{"type":"turn/end","time":5000,"data":{"reason":"done"}}"#,
        ]
        .join("\n");
        let transcript = parse_transcript("session-x", &journal);
        assert_eq!(transcript.total_events, 6);
        assert_eq!(transcript.rows.len(), 5);
        assert_eq!(transcript.rows[0].kind, "user");
        assert!(transcript.rows[0].body.contains("hello"));
        assert!(transcript.rows[1].label.contains("read"));
        assert!(transcript.rows[2].body.contains("hi"));
        assert!(transcript.rows[3].body.contains("ok"));
        assert_eq!(transcript.usage.input_tokens, 5);
        assert_eq!(transcript.usage.output_tokens, 2);
        assert_eq!(transcript.usage.cache_read_tokens, 7);
        assert_eq!(transcript.usage.reasoning_tokens, 1);
    }

    #[test]
    fn an_invalid_session_id_degrades_without_panicking() {
        let view = load_session(Some("not-durable"));
        assert!(!view.is_loaded());
        match view {
            SessionView::Missing { reason, .. } => assert!(reason.contains("durable")),
            _ => panic!("expected a missing session"),
        }
    }

    #[test]
    fn timestamps_use_utc_civil_dates() {
        assert_eq!(fmt_time(0), "-");
        assert_eq!(fmt_time(1_700_000_000_000), "2023-11-14 22:13:20Z");
    }
}
