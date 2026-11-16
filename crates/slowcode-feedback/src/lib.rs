//! Build an issue-ready feedback bundle from one durable session journal.
//!
//! The bundle is a local snapshot: `issue.md`, `session.jsonl`,
//! `metadata.json`, and `manifest.json`, plus an optional `plan.md`. Before
//! anything is written, the journal is checked against the agreed hard gate:
//! the journal must end with a newline and every event sequence number must be
//! present exactly once in order from zero. Hard-gate failures still produce a
//! bundle because the scene is valuable, but they mark it incomplete and are
//! visible in all three machine-readable surfaces.

use std::{
    collections::BTreeMap,
    fs,
    io::{self, Write as _},
    path::{Path, PathBuf},
    time::{SystemTime, UNIX_EPOCH},
};

use serde_json::Value;
use sha2::{Digest, Sha256};
use thiserror::Error;

/// Everything needed to write one bundle.
#[derive(Clone, Debug)]
pub struct BundleRequest<'a> {
    pub session_id: &'a str,
    pub journal: &'a [u8],
    pub workspace: Option<&'a str>,
    pub version: &'a str,
    pub model: Option<&'a str>,
    pub provider: Option<&'a str>,
    pub os: &'a str,
    pub out_dir: PathBuf,
    pub title: Option<String>,
    pub note: Option<String>,
    pub plan: Option<PathBuf>,
}

/// What the bundle contains after a successful write.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct BundleOutcome {
    pub out_dir: PathBuf,
    pub complete: bool,
    pub hard_failures: Vec<String>,
    pub warnings: Vec<String>,
    pub events: usize,
    pub first_seq: Option<u64>,
    pub last_seq: Option<u64>,
    pub sha256: String,
}

/// Facts shown before the user confirms a bundle.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct BundlePreview {
    pub title: String,
    pub note: Option<String>,
    pub plan: Option<PathBuf>,
    pub out_dir: PathBuf,
    pub complete: bool,
    pub hard_failures: Vec<String>,
    pub warnings: Vec<String>,
    pub events: usize,
    pub first_seq: Option<u64>,
    pub last_seq: Option<u64>,
}

/// One error event worth carrying into the issue draft.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ErrorFact {
    /// `turn` for a `turn/end` error, `tool` for a failed `tool/result`.
    pub source: &'static str,
    pub code: String,
    pub message: String,
    pub turn: Option<u64>,
    pub step: Option<u64>,
}

/// Bundle failures. Incomplete journals are not errors; they are reported in
/// [`BundleOutcome`].
#[derive(Debug, Error)]
pub enum FeedbackError {
    #[error("session journal is empty")]
    Empty,
    #[error("session journal does not start with a session header")]
    MissingHeader,
    #[error("session journal belongs to {found}, not {expected}")]
    SessionMismatch { found: String, expected: String },
    #[error("plan artifact not found: {0}")]
    PlanMissing(PathBuf),
    #[error("io: {0}")]
    Io(#[from] io::Error),
}

/// Parsed and validated journal facts.
#[derive(Debug)]
struct Analysis {
    header_id: String,
    created_at_ms: Option<u64>,
    workspace: Option<String>,
    title: Option<String>,
    model: Option<String>,
    provider: Option<String>,
    user_messages: Vec<String>,
    assistant_messages: Vec<String>,
    tool_names: BTreeMap<String, usize>,
    errors: Vec<ErrorFact>,
    tool_calls: usize,
    turn_reasons: BTreeMap<String, usize>,
    usage: BTreeMap<String, u64>,
    event_count: usize,
    first_seq: Option<u64>,
    last_seq: Option<u64>,
    complete: bool,
    hard_failures: Vec<String>,
    warnings: Vec<String>,
}

/// Write a feedback bundle and return its completeness report.
pub fn write_bundle(request: BundleRequest<'_>) -> Result<BundleOutcome, FeedbackError> {
    let prepared = prepare(&request)?;
    let Prepared {
        analysis,
        workspace,
        title,
        note,
        plan,
        sha256,
    } = prepared;
    let workspace = workspace.as_deref();
    let workspace_name = workspace
        .and_then(|workspace| workspace.trim_end_matches('/').rsplit('/').next())
        .filter(|name| !name.is_empty())
        .unwrap_or("unknown");

    fs::create_dir_all(&request.out_dir)?;
    write_private(&request.out_dir.join("session.jsonl"), request.journal)?;

    let plan_display = match &plan {
        Some(plan) => {
            let bytes = fs::read(plan)?;
            write_private(&request.out_dir.join("plan.md"), &bytes)?;
            Some("plan.md")
        }
        None => None,
    };

    let issue = render_issue(
        &analysis,
        RenderContext {
            title: &title,
            note: note.as_deref(),
            workspace_name,
            version: request.version,
            model: request.model.or(analysis.model.as_deref()),
            provider: request.provider.or(analysis.provider.as_deref()),
            os: request.os,
            plan: plan_display,
            sha256: &sha256,
        },
    );
    write_private(&request.out_dir.join("issue.md"), issue.as_bytes())?;

    let metadata = render_metadata(
        &analysis,
        request.session_id,
        &sha256,
        request.journal.len(),
        MetaContext {
            workspace,
            version: request.version,
            model: request.model.or(analysis.model.as_deref()),
            provider: request.provider.or(analysis.provider.as_deref()),
            os: request.os,
        },
    );
    write_private(&request.out_dir.join("metadata.json"), metadata.as_bytes())?;

    let manifest = render_manifest(
        &analysis,
        request.session_id,
        &sha256,
        request.journal.len(),
    );
    write_private(&request.out_dir.join("manifest.json"), manifest.as_bytes())?;

    Ok(BundleOutcome {
        out_dir: request.out_dir,
        complete: analysis.complete,
        hard_failures: analysis.hard_failures,
        warnings: analysis.warnings,
        events: analysis.event_count,
        first_seq: analysis.first_seq,
        last_seq: analysis.last_seq,
        sha256,
    })
}

/// Validate a request and return what the bundle would contain without writing.
pub fn preview(request: &BundleRequest<'_>) -> Result<BundlePreview, FeedbackError> {
    let prepared = prepare(request)?;
    Ok(BundlePreview {
        title: prepared.title,
        note: prepared.note,
        plan: prepared.plan,
        out_dir: request.out_dir.clone(),
        complete: prepared.analysis.complete,
        hard_failures: prepared.analysis.hard_failures,
        warnings: prepared.analysis.warnings,
        events: prepared.analysis.event_count,
        first_seq: prepared.analysis.first_seq,
        last_seq: prepared.analysis.last_seq,
    })
}

struct Prepared {
    analysis: Analysis,
    workspace: Option<String>,
    title: String,
    note: Option<String>,
    plan: Option<PathBuf>,
    sha256: String,
}

fn prepare(request: &BundleRequest<'_>) -> Result<Prepared, FeedbackError> {
    let mut analysis = analyze(request.journal, request.session_id)?;
    let workspace = request
        .workspace
        .or(analysis.workspace.as_deref())
        .map(str::to_owned);
    for message in analysis
        .user_messages
        .iter_mut()
        .chain(analysis.assistant_messages.iter_mut())
    {
        *message = redact(message, workspace.as_deref());
    }
    for error in &mut analysis.errors {
        error.message = redact(&error.message, workspace.as_deref());
    }
    let title = request
        .title
        .clone()
        .filter(|title| !title.trim().is_empty())
        .or_else(|| derive_title(request.note.as_deref()))
        .or_else(|| analysis.title.clone())
        .unwrap_or_else(|| "slowcode session feedback".to_owned());
    let title = redact(&title, workspace.as_deref());
    let note = request
        .note
        .clone()
        .map(|note| redact(&note, workspace.as_deref()));
    let plan = match &request.plan {
        Some(plan) if !plan.is_file() => return Err(FeedbackError::PlanMissing(plan.clone())),
        plan => plan.clone(),
    };
    let sha256 = sha256_hex(request.journal);
    Ok(Prepared {
        analysis,
        workspace,
        title,
        note,
        plan,
        sha256,
    })
}

/// Use the first non-empty line of the free-form note as a title.
#[must_use]
pub fn derive_title(note: Option<&str>) -> Option<String> {
    const MAX_CHARS: usize = 60;
    let line = note?.lines().map(str::trim).find(|line| !line.is_empty())?;
    if line.chars().count() <= MAX_CHARS {
        return Some(line.to_owned());
    }
    let mut title: String = line.chars().take(MAX_CHARS).collect();
    title.push_str("...");
    Some(title)
}

fn analyze(journal: &[u8], expected_id: &str) -> Result<Analysis, FeedbackError> {
    let (values, hard_failures) = parse_journal_lines(journal)?;
    let header = session_header(&values, expected_id)?;
    let mut analysis = Analysis::start(header, values.len(), hard_failures);
    let mut scan = ScanState::default();
    for value in values.iter().skip(1) {
        scan_event(&mut analysis, &mut scan, value);
    }
    finish_checks(&mut analysis, &scan);
    analysis.complete = analysis.hard_failures.is_empty();
    Ok(analysis)
}

fn parse_journal_lines(journal: &[u8]) -> Result<(Vec<Value>, Vec<String>), FeedbackError> {
    if journal.is_empty() {
        return Err(FeedbackError::Empty);
    }
    let mut hard_failures = Vec::new();
    let tail_complete = journal.last() == Some(&b'\n');
    if !tail_complete {
        hard_failures.push("journal does not end with a newline (torn tail)".to_owned());
    }
    let body = match tail_complete {
        true => &journal[..journal.len() - 1],
        false => journal,
    };
    let mut values = Vec::new();
    for (index, line) in body.split(|byte| *byte == b'\n').enumerate() {
        collect_line(line, index + 1, &mut values, &mut hard_failures);
    }
    if values.is_empty() {
        return Err(FeedbackError::MissingHeader);
    }
    Ok((values, hard_failures))
}

fn collect_line(
    line: &[u8],
    line_number: usize,
    values: &mut Vec<Value>,
    hard_failures: &mut Vec<String>,
) {
    if line.iter().all(u8::is_ascii_whitespace) {
        hard_failures.push(format!("line {line_number} is empty"));
        return;
    }
    match serde_json::from_slice::<Value>(line) {
        Ok(value) => values.push(value),
        Err(_) => hard_failures.push(format!("line {line_number} is not valid JSON")),
    }
}

fn session_header<'a>(values: &'a [Value], expected_id: &str) -> Result<&'a Value, FeedbackError> {
    let header = &values[0];
    if header.get("type").and_then(Value::as_str) != Some("session") {
        return Err(FeedbackError::MissingHeader);
    }
    let Some(header_id) = header.get("id").and_then(Value::as_str) else {
        return Err(FeedbackError::MissingHeader);
    };
    if header_id != expected_id {
        return Err(FeedbackError::SessionMismatch {
            found: header_id.to_owned(),
            expected: expected_id.to_owned(),
        });
    }
    Ok(header)
}

impl Analysis {
    fn start(header: &Value, event_count: usize, hard_failures: Vec<String>) -> Self {
        Self {
            header_id: header
                .get("id")
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_owned(),
            created_at_ms: header.get("createdAt").and_then(Value::as_u64),
            workspace: header.get("cwd").and_then(Value::as_str).map(str::to_owned),
            title: None,
            model: None,
            provider: None,
            user_messages: Vec::new(),
            assistant_messages: Vec::new(),
            tool_names: BTreeMap::new(),
            errors: Vec::new(),
            tool_calls: 0,
            turn_reasons: BTreeMap::new(),
            usage: BTreeMap::new(),
            event_count,
            first_seq: None,
            last_seq: None,
            complete: false,
            hard_failures,
            warnings: Vec::new(),
        }
    }
}

#[derive(Default)]
struct ScanState {
    expected_seq: u64,
    saw_request_header: bool,
    saw_turn_end: bool,
}

fn scan_event(analysis: &mut Analysis, scan: &mut ScanState, value: &Value) {
    let kind = value
        .get("type")
        .and_then(Value::as_str)
        .unwrap_or_default();
    scan_seq(analysis, scan, value, kind);
    let data = value.get("data").unwrap_or(&Value::Null);
    match kind {
        "request/header" => apply_request_header(analysis, scan, data),
        "session/title" => apply_title(analysis, data),
        "user/message" => apply_user_message(analysis, data),
        "assistant/message" => apply_assistant_message(analysis, data),
        "tool/result" => apply_tool_result(analysis, data),
        "turn/end" => apply_turn_end(analysis, scan, data),
        _ => {}
    }
}

fn scan_seq(analysis: &mut Analysis, scan: &mut ScanState, value: &Value, kind: &str) {
    let Some(seq) = value.get("seq").and_then(Value::as_u64) else {
        analysis
            .hard_failures
            .push(format!("event {kind} has no numeric seq"));
        return;
    };
    if seq != scan.expected_seq {
        analysis.hard_failures.push(format!(
            "seq gap: expected {}, found {seq}",
            scan.expected_seq
        ));
        scan.expected_seq = seq.saturating_add(1);
    } else {
        scan.expected_seq += 1;
    }
    analysis.first_seq.get_or_insert(seq);
    analysis.last_seq = Some(seq);
}

fn apply_request_header(analysis: &mut Analysis, scan: &mut ScanState, data: &Value) {
    scan.saw_request_header = true;
    let config = data
        .get("header")
        .and_then(|header| header.get("config"))
        .unwrap_or(&Value::Null);
    if let Some(model) = config.get("model").and_then(Value::as_str) {
        analysis.model = Some(model.to_owned());
    }
    if let Some(provider) = config.get("provider").and_then(Value::as_str) {
        analysis.provider = Some(provider.to_owned());
    }
}

fn apply_title(analysis: &mut Analysis, data: &Value) {
    if let Some(title) = data.get("title").and_then(Value::as_str) {
        analysis.title = Some(title.to_owned());
    }
}

fn apply_user_message(analysis: &mut Analysis, data: &Value) {
    let message = data.get("message").unwrap_or(data);
    let text = blocks_text(message.get("content"), &mut analysis.tool_names);
    if !text.trim().is_empty() {
        analysis.user_messages.push(text);
    }
}

fn apply_assistant_message(analysis: &mut Analysis, data: &Value) {
    let message = data.get("message").unwrap_or(&Value::Null);
    let text = blocks_text(message.get("content"), &mut analysis.tool_names);
    if !text.trim().is_empty() {
        analysis.assistant_messages.push(text);
    }
    if let Some(content) = message.get("content").and_then(Value::as_array) {
        analysis.tool_calls += content
            .iter()
            .filter(|block| {
                matches!(
                    block.get("type").and_then(Value::as_str),
                    Some("tool_call" | "tool-call")
                )
            })
            .count();
    }
    record_usage(analysis, data);
}

fn apply_tool_result(analysis: &mut Analysis, data: &Value) {
    let Some(error) = data.get("error").and_then(Value::as_object) else {
        return;
    };
    let code = error
        .get("code")
        .and_then(Value::as_str)
        .unwrap_or("TOOL_ERROR")
        .to_owned();
    let message = tool_error_text(data).unwrap_or_else(|| {
        error
            .get("name")
            .and_then(Value::as_str)
            .unwrap_or("tool failed")
            .to_owned()
    });
    analysis.errors.push(ErrorFact {
        source: "tool",
        code,
        message,
        turn: data.get("turn").and_then(Value::as_u64),
        step: data.get("step").and_then(Value::as_u64),
    });
}

/// Pull the human-readable text out of a tool-result message envelope.
fn tool_error_text(data: &Value) -> Option<String> {
    let blocks = data
        .get("message")
        .and_then(|message| message.get("content"))
        .and_then(Value::as_array)?;
    let mut parts = Vec::new();
    for block in blocks {
        let nested = block
            .get("content")
            .and_then(Value::as_array)
            .map_or(std::slice::from_ref(block), Vec::as_slice);
        for inner in nested {
            if let Some(text) = inner.get("text").and_then(Value::as_str) {
                parts.push(text.to_owned());
            }
        }
    }
    let text = parts.join("\n");
    (!text.trim().is_empty()).then_some(text)
}

fn record_usage(analysis: &mut Analysis, data: &Value) {
    let Some(usage) = data.get("usage").and_then(Value::as_object) else {
        return;
    };
    for (key, value) in usage {
        if let Some(value) = value.as_u64() {
            *analysis.usage.entry(key.clone()).or_default() += value;
        }
    }
}

fn apply_turn_end(analysis: &mut Analysis, scan: &mut ScanState, data: &Value) {
    scan.saw_turn_end = true;
    let reason = data.get("reason").unwrap_or(&Value::Null);
    let kind = reason
        .get("kind")
        .and_then(Value::as_str)
        .unwrap_or("unknown");
    *analysis.turn_reasons.entry(kind.to_owned()).or_default() += 1;
    if kind == "error"
        && let Some(error) = reason.get("error")
    {
        analysis.errors.push(ErrorFact {
            source: "turn",
            code: error
                .get("code")
                .and_then(Value::as_str)
                .unwrap_or("TURN_ERROR")
                .to_owned(),
            message: error
                .get("message")
                .and_then(Value::as_str)
                .unwrap_or("turn failed without a message")
                .to_owned(),
            turn: data.get("turn").and_then(Value::as_u64),
            step: None,
        });
    }
}

fn finish_checks(analysis: &mut Analysis, scan: &ScanState) {
    if !scan.saw_request_header {
        analysis
            .warnings
            .push("no request/header event; model and provider are unknown".to_owned());
    }
    if analysis.model.is_none() {
        analysis.warnings.push("model is unknown".to_owned());
    }
    if analysis.provider.is_none() {
        analysis.warnings.push("provider is unknown".to_owned());
    }
    let turns: usize = analysis.turn_reasons.values().sum();
    if turns != analysis.assistant_messages.len() {
        analysis.warnings.push(format!(
            "turn/end count {turns} differs from assistant message count {}",
            analysis.assistant_messages.len()
        ));
    }
    if turns > 0 && !scan.saw_turn_end {
        analysis
            .warnings
            .push("journal ends inside a turn".to_owned());
    }
}

fn blocks_text(content: Option<&Value>, tool_names: &mut BTreeMap<String, usize>) -> String {
    let mut parts = Vec::new();
    let Some(blocks) = content.and_then(Value::as_array) else {
        return String::new();
    };
    for block in blocks {
        match block.get("type").and_then(Value::as_str) {
            Some("text") => {
                if let Some(text) = block.get("text").and_then(Value::as_str) {
                    parts.push(text.to_owned());
                }
            }
            Some("tool_call" | "tool-call") => {
                let name = block
                    .get("name")
                    .and_then(Value::as_str)
                    .unwrap_or("unknown");
                *tool_names.entry(name.to_owned()).or_default() += 1;
            }
            _ => {}
        }
    }
    parts.join("\n")
}

struct RenderContext<'a> {
    title: &'a str,
    note: Option<&'a str>,
    workspace_name: &'a str,
    version: &'a str,
    model: Option<&'a str>,
    provider: Option<&'a str>,
    os: &'a str,
    plan: Option<&'a str>,
    sha256: &'a str,
}

fn render_issue(analysis: &Analysis, context: RenderContext<'_>) -> String {
    let mut issue = String::new();
    issue.push_str(&format!("# [feedback] {}\n\n", context.title));
    if !analysis.complete {
        issue.push_str("> **INCOMPLETE BUNDLE**\n>\n");
        for failure in &analysis.hard_failures {
            issue.push_str(&format!("> - {failure}\n"));
        }
        issue.push('\n');
    }
    issue.push_str("## Completeness\n\n");
    issue.push_str(&format!(
        "- complete: `{}`\n",
        if analysis.complete { "true" } else { "false" }
    ));
    issue.push_str(&format!(
        "- events: {} (seq {}..{})\n",
        analysis.event_count,
        analysis
            .first_seq
            .map(|seq| seq.to_string())
            .unwrap_or_else(|| "none".to_owned()),
        analysis
            .last_seq
            .map(|seq| seq.to_string())
            .unwrap_or_else(|| "none".to_owned())
    ));
    issue.push_str(&format!("- sha256: `{}`\n", context.sha256));
    if !analysis.hard_failures.is_empty() {
        issue.push_str("- hard failures:\n");
        for failure in &analysis.hard_failures {
            issue.push_str(&format!("  - {failure}\n"));
        }
    }
    if !analysis.warnings.is_empty() {
        issue.push_str("- warnings:\n");
        for warning in &analysis.warnings {
            issue.push_str(&format!("  - {warning}\n"));
        }
    }
    issue.push_str(&format!(
        "\n## Trace\n\n- traceId: `{}`\n- slowcode: `{}`\n- model: `{}`\n- provider: `{}`\n- OS: `{}`\n- workspace: `<workspace>`\n- workspace-name: `{}`\n- started: `{}`\n- journal: `session.jsonl`\n- plan artifact: `{}`\n",
        analysis.header_id,
        context.version,
        context.model.unwrap_or("unknown"),
        context.provider.unwrap_or("unknown"),
        context.os,
        context.workspace_name,
        analysis
            .created_at_ms
            .map(format_unix_millis)
            .unwrap_or_else(|| "unknown".to_owned()),
        context.plan.unwrap_or("none"),
    ));
    issue.push_str("\n## 关键错误\n\n");
    issue.push_str(&render_errors(&analysis.errors));
    issue.push_str(&format!(
        "\n## 期望\n\n请补充你期望的行为。\n\n## 实际\n\n{}\n",
        context
            .note
            .unwrap_or("请补充实际行为，例如报错、耗时、输出差异。")
    ));
    issue.push_str("\n## 复现步骤\n\n1. \n2. \n3. \n");

    let turns: usize = analysis.turn_reasons.values().sum();
    let reason_text = if analysis.turn_reasons.is_empty() {
        "none".to_owned()
    } else {
        analysis
            .turn_reasons
            .iter()
            .map(|(reason, count)| format!("{reason} x{count}"))
            .collect::<Vec<_>>()
            .join(", ")
    };
    let tools = if analysis.tool_names.is_empty() {
        "none".to_owned()
    } else {
        analysis
            .tool_names
            .iter()
            .map(|(name, count)| format!("{name} x{count}"))
            .collect::<Vec<_>>()
            .join(", ")
    };
    let errors = analysis.turn_reasons.get("error").copied().unwrap_or(0)
        + analysis.turn_reasons.get("aborted").copied().unwrap_or(0);
    let usage = |key: &str| analysis.usage.get(key).copied().unwrap_or(0);
    issue.push_str(&format!(
        "\n## 会话摘要\n\n- events: {}\n- turns: {} ({})\n- user messages: {}\n- assistant messages: {}\n- tool calls: {} ({})\n- errors: {}\n- tokens: input {}, output {}, cache read {}, reasoning {}\n",
        analysis.event_count,
        turns,
        reason_text,
        analysis.user_messages.len(),
        analysis.assistant_messages.len(),
        analysis.tool_calls,
        tools,
        errors,
        usage("inputTokens"),
        usage("outputTokens"),
        usage("cacheReadTokens"),
        usage("reasoningTokens"),
    ));

    issue.push_str("\n## 最近用户消息\n\n");
    issue.push_str(&recent(&analysis.user_messages, 5, 1000));
    issue.push_str("\n\n## 最近 Agent 回复\n\n");
    issue.push_str(&recent(&analysis.assistant_messages, 3, 1500));
    issue.push_str("\n\n## 附件\n\n- `session.jsonl`：完整会话日志，可能包含工作区文件内容与推理文本，附加前请检查。\n");
    if context.plan.is_some() {
        issue.push_str("- `plan.md`：相关 Plan artifact。\n");
    }
    issue.push_str("\n## 反馈类型\n\n- [ ] 功能不正确\n- [ ] 输出质量\n- [ ] 工具或权限\n- [ ] 性能或成本\n- [ ] 文档或可用性\n");
    issue
}

/// Render the most recent errors, newest first. This is the part a reader
/// needs most, so it stays near the top of the issue.
fn render_errors(errors: &[ErrorFact]) -> String {
    const LIMIT: usize = 8;
    if errors.is_empty() {
        return "- none\n".to_owned();
    }
    let mut text = String::new();
    for error in errors.iter().rev().take(LIMIT) {
        let mut location = String::new();
        if let Some(turn) = error.turn {
            location.push_str(&format!("turn {turn}"));
        }
        if let Some(step) = error.step {
            if !location.is_empty() {
                location.push(' ');
            }
            location.push_str(&format!("step {step}"));
        }
        let location = if location.is_empty() {
            String::new()
        } else {
            format!(" ({location})")
        };
        let message = clip(&error.message.replace('\n', " "), 800);
        text.push_str(&format!(
            "- [{}] `{}`{}: {message}\n",
            error.source, error.code, location
        ));
    }
    if errors.len() > LIMIT {
        text.push_str(&format!(
            "- ... {} older error(s) omitted\n",
            errors.len() - LIMIT
        ));
    }
    text
}

fn recent(messages: &[String], limit: usize, max_chars: usize) -> String {
    if messages.is_empty() {
        return "- none".to_owned();
    }
    let start = messages.len().saturating_sub(limit);
    messages[start..]
        .iter()
        .map(|message| format!("- {}", clip(message, max_chars)))
        .collect::<Vec<_>>()
        .join("\n\n")
}

/// Trim text and append a truncation marker past `limit` characters.
#[must_use]
pub fn clip(text: &str, limit: usize) -> String {
    let text = text.trim();
    if text.chars().count() <= limit {
        return text.to_owned();
    }
    let clipped: String = text.chars().take(limit).collect();
    format!("{clipped}\n...(truncated)")
}

struct MetaContext<'a> {
    workspace: Option<&'a str>,
    version: &'a str,
    model: Option<&'a str>,
    provider: Option<&'a str>,
    os: &'a str,
}

fn render_metadata(
    analysis: &Analysis,
    session_id: &str,
    sha256: &str,
    bytes: usize,
    context: MetaContext<'_>,
) -> String {
    let metadata = serde_json::json!({
        "traceId": session_id,
        "version": context.version,
        "model": context.model,
        "provider": context.provider,
        "os": context.os,
        "workspace": context.workspace,
        "createdAt": analysis.created_at_ms,
        "events": analysis.event_count,
        "turnReasons": analysis.turn_reasons,
        "toolCalls": analysis.tool_names,
        "errors": analysis.errors.iter().map(|error| serde_json::json!({
            "source": error.source,
            "code": error.code,
            "message": error.message,
            "turn": error.turn,
            "step": error.step,
        })).collect::<Vec<_>>(),
        "usage": analysis.usage,
        "complete": analysis.complete,
        "hardFailures": analysis.hard_failures,
        "warnings": analysis.warnings,
        "journal": {
            "bytes": bytes,
            "events": analysis.event_count,
            "firstSeq": analysis.first_seq,
            "lastSeq": analysis.last_seq,
            "sha256": sha256,
        },
    });
    let mut text = serde_json::to_string_pretty(&metadata).unwrap_or_else(|_| "{}".to_owned());
    text.push('\n');
    text
}

fn render_manifest(analysis: &Analysis, session_id: &str, sha256: &str, bytes: usize) -> String {
    let generated_at = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_secs())
        .unwrap_or_default();
    let manifest = serde_json::json!({
        "traceId": session_id,
        "bytes": bytes,
        "events": analysis.event_count,
        "firstSeq": analysis.first_seq,
        "lastSeq": analysis.last_seq,
        "sha256": sha256,
        "generatedAt": generated_at,
        "complete": analysis.complete,
        "hardFailures": analysis.hard_failures,
        "warnings": analysis.warnings,
    });
    let mut text = serde_json::to_string_pretty(&manifest).unwrap_or_else(|_| "{}".to_owned());
    text.push('\n');
    text
}

fn sha256_hex(bytes: &[u8]) -> String {
    let digest = Sha256::digest(bytes);
    let mut hex = String::with_capacity(digest.len() * 2);
    for byte in digest {
        hex.push_str(&format!("{byte:02x}"));
    }
    hex
}

fn write_private(path: &Path, bytes: &[u8]) -> Result<(), FeedbackError> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt as _;
        let mut file = fs::OpenOptions::new()
            .write(true)
            .create(true)
            .truncate(true)
            .mode(0o600)
            .open(path)?;
        file.write_all(bytes)?;
        file.sync_all()?;
        Ok(())
    }
    #[cfg(not(unix))]
    {
        fs::write(path, bytes)?;
        Ok(())
    }
}

fn format_unix_millis(millis: u64) -> String {
    let seconds = i64::try_from(millis / 1000).unwrap_or(0);
    let days = seconds.div_euclid(86_400);
    let remaining = seconds.rem_euclid(86_400);
    let (year, month, day) = civil_from_days(days);
    let hour = remaining / 3600;
    let minute = (remaining % 3600) / 60;
    let second = remaining % 60;
    format!("{year:04}-{month:02}-{day:02}T{hour:02}:{minute:02}:{second:02}Z")
}

/// Howard Hinnant's civil-from-days algorithm.
fn civil_from_days(days: i64) -> (i64, u32, u32) {
    let z = days + 719_468;
    let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let year = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let day = (doy - (153 * mp + 2) / 5 + 1) as u32;
    let month = if mp < 10 { mp + 3 } else { mp - 9 } as u32;
    (if month <= 2 { year + 1 } else { year }, month, day)
}

/// Replace the workspace path and common credential shapes in visible text.
/// The raw `session.jsonl` is never rewritten.
fn redact(text: &str, workspace: Option<&str>) -> String {
    let text = match workspace {
        Some(workspace) if !workspace.is_empty() => text.replace(workspace, "<workspace>"),
        _ => text.to_owned(),
    };
    redact_secrets(&text)
}

fn redact_secrets(text: &str) -> String {
    let bytes = text.as_bytes();
    let mut output = String::with_capacity(text.len());
    let mut index = 0;
    while index < bytes.len() {
        if let Some(length) = secret_at(bytes, index) {
            output.push_str("<redacted>");
            index += length;
            continue;
        }
        let character = text[index..].chars().next().unwrap_or_default();
        output.push(character);
        index += character.len_utf8();
    }
    output
}

fn secret_at(bytes: &[u8], index: usize) -> Option<usize> {
    if let Some(length) = prefixed_run(bytes, index, b"sk-", 16) {
        return Some(length);
    }
    for prefix in [&b"ghp_"[..], b"gho_", b"ghu_", b"ghs_", b"ghr_"] {
        if let Some(length) = prefixed_run(bytes, index, prefix, 20) {
            return Some(length);
        }
    }
    if let Some(length) = case_insensitive_run(bytes, index, b"bearer", 16) {
        return Some(length);
    }
    for keyword in [
        b"api_key".as_slice(),
        b"api-key",
        b"apikey",
        b"token",
        b"secret",
    ] {
        if let Some(length) = assignment_run(bytes, index, keyword) {
            return Some(length);
        }
    }
    None
}

fn prefixed_run(bytes: &[u8], index: usize, prefix: &[u8], minimum: usize) -> Option<usize> {
    if !bytes[index..].starts_with(prefix) {
        return None;
    }
    let run = token_run(&bytes[index + prefix.len()..]);
    (run >= minimum).then_some(prefix.len() + run)
}

fn case_insensitive_run(
    bytes: &[u8],
    index: usize,
    keyword: &[u8],
    minimum: usize,
) -> Option<usize> {
    if index + keyword.len() > bytes.len() {
        return None;
    }
    if !bytes[index..index + keyword.len()].eq_ignore_ascii_case(keyword) {
        return None;
    }
    let mut cursor = index + keyword.len();
    if cursor >= bytes.len() || !bytes[cursor].is_ascii_whitespace() {
        return None;
    }
    while cursor < bytes.len() && bytes[cursor].is_ascii_whitespace() {
        cursor += 1;
    }
    let run = token_run(&bytes[cursor..]);
    (run >= minimum).then_some(cursor + run - index)
}

fn assignment_run(bytes: &[u8], index: usize, keyword: &[u8]) -> Option<usize> {
    if index + keyword.len() > bytes.len() {
        return None;
    }
    if !bytes[index..index + keyword.len()].eq_ignore_ascii_case(keyword) {
        return None;
    }
    let mut cursor = index + keyword.len();
    while cursor < bytes.len() && bytes[cursor] == b' ' {
        cursor += 1;
    }
    if cursor >= bytes.len() || (bytes[cursor] != b':' && bytes[cursor] != b'=') {
        return None;
    }
    cursor += 1;
    while cursor < bytes.len() && bytes[cursor].is_ascii_whitespace() {
        cursor += 1;
    }
    if cursor < bytes.len() && (bytes[cursor] == b'"' || bytes[cursor] == b'\'') {
        cursor += 1;
    }
    let run = token_run(&bytes[cursor..]);
    (run >= 8).then_some(cursor + run - index)
}

fn token_run(bytes: &[u8]) -> usize {
    bytes
        .iter()
        .take_while(|byte| {
            byte.is_ascii_alphanumeric() || matches!(byte, b'_' | b'-' | b'.' | b'/' | b'+')
        })
        .count()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicU64, Ordering};

    const SESSION_ID: &str = "session-33333333-3333-4333-8333-333333333333";

    fn temp_dir(label: &str) -> PathBuf {
        static COUNTER: AtomicU64 = AtomicU64::new(0);
        let unique = COUNTER.fetch_add(1, Ordering::Relaxed);
        let path = std::env::temp_dir().join(format!(
            "slowcode-feedback-{label}-{}-{unique}",
            std::process::id()
        ));
        fs::create_dir_all(&path).unwrap();
        path
    }

    fn journal() -> String {
        format!(
            concat!(
                "{{\"type\":\"session\",\"id\":\"{id}\",\"createdAt\":1792466172940,",
                "\"cwd\":\"/tmp/example-project\"}}\n",
                "{{\"type\":\"request/header\",\"seq\":0,\"time\":1,\"data\":{{\"header\":",
                "{{\"config\":{{\"model\":\"deepseek-flash\",\"provider\":\"deepseek-official\"}}}}}}}}\n",
                "{{\"type\":\"user/message\",\"seq\":1,\"time\":2,\"data\":{{\"content\":[",
                "{{\"type\":\"text\",\"text\":\"explain /tmp/example-project sk-abcdefghijklmnopqrstuv\"}}]}}}}\n",
                "{{\"type\":\"assistant/message\",\"seq\":2,\"time\":3,\"data\":{{\"message\":",
                "{{\"content\":[{{\"type\":\"text\",\"text\":\"the answer\"}}]}},\"usage\":{{",
                "\"inputTokens\":10,\"outputTokens\":2,\"cacheReadTokens\":5,",
                "\"reasoningTokens\":0}},\"turn\":1}}}}\n",
                "{{\"type\":\"turn/end\",\"seq\":3,\"time\":4,\"data\":{{\"reason\":",
                "{{\"kind\":\"completed\"}},\"turn\":1}}}}\n"
            ),
            id = SESSION_ID
        )
    }

    fn request<'a>(out_dir: &Path, journal: &'a [u8]) -> BundleRequest<'a> {
        BundleRequest {
            session_id: SESSION_ID,
            journal,
            workspace: None,
            version: "0.1.0",
            model: None,
            provider: None,
            os: "Darwin 21.6.0 x86_64",
            out_dir: out_dir.to_path_buf(),
            title: Some("a real failure".to_owned()),
            note: Some("expected success".to_owned()),
            plan: None,
        }
    }

    fn error_journal() -> String {
        format!(
            concat!(
                "{{\"type\":\"session\",\"id\":\"{id}\",\"createdAt\":1792466172940,",
                "\"cwd\":\"/tmp/example-project\"}}\n",
                "{{\"type\":\"request/header\",\"seq\":0,\"time\":1,\"data\":{{\"header\":",
                "{{\"config\":{{\"model\":\"deepseek-flash\",\"provider\":\"deepseek-official\"}}}}}}}}\n",
                "{{\"type\":\"user/message\",\"seq\":1,\"time\":2,\"data\":{{\"content\":[",
                "{{\"type\":\"text\",\"text\":\"run the shell\"}}]}}}}\n",
                "{{\"type\":\"assistant/message\",\"seq\":2,\"time\":3,\"data\":{{\"message\":",
                "{{\"content\":[{{\"type\":\"text\",\"text\":\"checking\"}},",
                "{{\"type\":\"tool-call\",\"name\":\"readonly_shell\",\"id\":\"call-1\",",
                "\"arguments\":\"{{}}\"}}]}}}}}}\n",
                "{{\"type\":\"tool/call\",\"seq\":3,\"time\":4,\"data\":{{\"name\":\"readonly_shell\",",
                "\"callId\":\"call-1\",\"turn\":1,\"step\":1}}}}\n",
                "{{\"type\":\"tool/result\",\"seq\":4,\"time\":5,\"data\":{{\"error\":",
                "{{\"code\":\"ABORTED_BEFORE_DISPATCH\",\"name\":\"AbortError\"}},\"message\":",
                "{{\"content\":[{{\"content\":[{{\"text\":\"tool was not started because the turn was stopping\",",
                "\"type\":\"text\"}}],\"isError\":true,\"toolCallId\":\"call-1\",\"type\":\"tool-result\"}}]}},",
                "\"step\":1,\"turn\":1}}}}\n",
                "{{\"type\":\"turn/end\",\"seq\":5,\"time\":6,\"data\":{{\"reason\":",
                "{{\"error\":{{\"code\":\"AGENT_TOOL_EXECUTOR\",\"message\":",
                "\"the tool executor failed before producing a result\"}},\"kind\":\"error\"}},",
                "\"turn\":1}}}}\n"
            ),
            id = SESSION_ID
        )
    }

    fn many_error_journal() -> String {
        let mut journal = format!(
            concat!(
                "{{\"type\":\"session\",\"id\":\"{id}\",\"createdAt\":1792466172940,",
                "\"cwd\":\"/tmp/example-project\"}}\n",
                "{{\"type\":\"request/header\",\"seq\":0,\"time\":1,\"data\":{{\"header\":",
                "{{\"config\":{{\"model\":\"deepseek-flash\",\"provider\":\"deepseek-official\"}}}}}}}}\n",
                "{{\"type\":\"user/message\",\"seq\":1,\"time\":2,\"data\":{{\"content\":[",
                "{{\"type\":\"text\",\"text\":\"run the shell\"}}]}}}}\n"
            ),
            id = SESSION_ID
        );
        let mut seq = 2;
        for step in 0..10 {
            journal.push_str(&format!(
                concat!(
                    "{{\"type\":\"tool/result\",\"seq\":{seq},\"time\":{seq},\"data\":",
                    "{{\"error\":{{\"code\":\"TOOL_{step}\"}},\"message\":{{\"content\":[",
                    "{{\"content\":[{{\"text\":\"failed with sk-abcdefghijklmnopqrstuv",
                    " at /tmp/example-project\",\"type\":\"text\"}}]}}]}},",
                    "\"turn\":1,\"step\":{step}}}}}\n"
                ),
                step = step,
                seq = seq,
            ));
            seq += 1;
        }
        journal.push_str(&format!(
            concat!(
                "{{\"type\":\"turn/end\",\"seq\":{seq},\"time\":{seq},\"data\":{{\"reason\":",
                "{{\"error\":{{\"code\":\"TURN_ERROR\",\"message\":",
                "\"failed in /tmp/example-project\"}},\"kind\":\"error\"}},",
                "\"turn\":1}}}}\n"
            ),
            seq = seq,
        ));
        journal
    }

    fn titled_journal() -> String {
        format!(
            concat!(
                "{{\"type\":\"session\",\"id\":\"{id}\",\"createdAt\":1792466172940,",
                "\"cwd\":\"/tmp/example-project\"}}\n",
                "{{\"type\":\"request/header\",\"seq\":0,\"time\":1,\"data\":{{\"header\":",
                "{{\"config\":{{\"model\":\"deepseek-flash\",\"provider\":\"deepseek-official\"}}}}}}}}\n",
                "{{\"type\":\"session/title\",\"seq\":1,\"time\":2,\"data\":{{\"title\":\"测试\"}}}}\n",
                "{{\"type\":\"user/message\",\"seq\":2,\"time\":3,\"data\":{{\"content\":[",
                "{{\"type\":\"text\",\"text\":\"测试\"}}]}}}}\n",
                "{{\"type\":\"assistant/message\",\"seq\":3,\"time\":4,\"data\":{{\"message\":",
                "{{\"content\":[{{\"type\":\"text\",\"text\":\"ready\"}}]}}}}}}\n",
                "{{\"type\":\"turn/end\",\"seq\":4,\"time\":5,\"data\":{{\"reason\":",
                "{{\"kind\":\"completed\"}},\"turn\":1}}}}\n"
            ),
            id = SESSION_ID
        )
    }

    #[test]
    fn writes_a_complete_bundle_with_redaction_and_manifest() {
        let base = temp_dir("complete");
        let out = base.join("out");
        let bytes = journal().into_bytes();
        let outcome = write_bundle(request(&out, &bytes)).unwrap();

        assert!(outcome.complete, "{:?}", outcome.hard_failures);
        assert_eq!(outcome.first_seq, Some(0));
        assert_eq!(outcome.last_seq, Some(3));
        assert_eq!(outcome.events, 5);
        assert_eq!(outcome.sha256, sha256_hex(&bytes));

        let issue = fs::read_to_string(out.join("issue.md")).unwrap();
        assert!(issue.contains("# [feedback] a real failure"), "{issue}");
        assert!(issue.contains("complete: `true`"), "{issue}");
        assert!(
            issue.contains("workspace-name: `example-project`"),
            "{issue}"
        );
        assert!(!issue.contains("/tmp/example-project"), "{issue}");
        assert!(!issue.contains("sk-abcdefghijklmnopqrstuv"), "{issue}");
        let actual = issue.split("## 实际").nth(1).expect("actual section");
        assert!(actual.contains("expected success"), "{issue}");
        let expected = issue
            .split("## 期望")
            .nth(1)
            .expect("expected section")
            .split("## 实际")
            .next()
            .expect("expected body");
        assert!(!expected.contains("expected success"), "{issue}");
        assert!(issue.contains("## 关键错误\n\n- none"), "{issue}");
        assert!(issue.contains("turns: 1 (completed x1)"), "{issue}");
        assert!(
            issue.contains("input 10, output 2, cache read 5"),
            "{issue}"
        );

        let metadata: Value =
            serde_json::from_str(&fs::read_to_string(out.join("metadata.json")).unwrap()).unwrap();
        assert_eq!(metadata["traceId"], SESSION_ID);
        assert_eq!(metadata["complete"], true);
        assert_eq!(metadata["journal"]["sha256"], outcome.sha256.as_str());

        let manifest: Value =
            serde_json::from_str(&fs::read_to_string(out.join("manifest.json")).unwrap()).unwrap();
        assert_eq!(manifest["complete"], true);
        assert_eq!(manifest["firstSeq"], 0);
        assert_eq!(manifest["lastSeq"], 3);
        assert_eq!(manifest["sha256"], outcome.sha256.as_str());

        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt as _;
            let mode = fs::metadata(out.join("session.jsonl"))
                .unwrap()
                .permissions()
                .mode()
                & 0o777;
            assert_eq!(mode, 0o600);
        }
        let _ = fs::remove_dir_all(base);
    }

    #[test]
    fn carries_turn_and_tool_errors_into_the_issue() {
        let base = temp_dir("errors");
        let out = base.join("out");
        let journal = error_journal();
        let mut request = request(&out, journal.as_bytes());
        request.note = Some("the shell call was interrupted".to_owned());
        let outcome = write_bundle(request).unwrap();

        assert!(outcome.complete, "{:?}", outcome.hard_failures);
        let issue = fs::read_to_string(out.join("issue.md")).unwrap();
        let section = issue
            .split("## 关键错误")
            .nth(1)
            .expect("error section")
            .split("## 期望")
            .next()
            .expect("error body");
        assert!(
            section.contains("[turn] `AGENT_TOOL_EXECUTOR` (turn 1)"),
            "{issue}"
        );
        assert!(
            section.contains("[tool] `ABORTED_BEFORE_DISPATCH` (turn 1 step 1)"),
            "{issue}"
        );
        assert!(
            section.contains("tool was not started because the turn was stopping"),
            "{issue}"
        );
        assert!(
            issue
                .split("## 实际")
                .nth(1)
                .unwrap()
                .contains("the shell call was interrupted"),
            "{issue}"
        );
        assert!(
            issue.contains("tool calls: 1 (readonly_shell x1)"),
            "{issue}"
        );

        let metadata: Value =
            serde_json::from_str(&fs::read_to_string(out.join("metadata.json")).unwrap()).unwrap();
        assert_eq!(metadata["errors"][0]["code"], "ABORTED_BEFORE_DISPATCH");
        assert_eq!(metadata["errors"][1]["code"], "AGENT_TOOL_EXECUTOR");
        let _ = fs::remove_dir_all(base);
    }

    #[test]
    fn redacts_error_messages_and_caps_the_error_list() {
        let base = temp_dir("errors-redacted");
        let out = base.join("out");
        let journal = many_error_journal();
        let mut request = request(&out, journal.as_bytes());
        request.workspace = Some("/tmp/example-project");
        let outcome = write_bundle(request).unwrap();

        assert!(outcome.complete, "{:?}", outcome.hard_failures);
        let issue = fs::read_to_string(out.join("issue.md")).unwrap();
        assert!(!issue.contains("sk-abcdefghijklmnopqrstuv"), "{issue}");
        assert!(!issue.contains("/tmp/example-project"), "{issue}");
        assert!(issue.contains("<redacted>"), "{issue}");
        assert!(issue.contains("<workspace>"), "{issue}");
        assert!(issue.contains("older error(s) omitted"), "{issue}");
        let section = issue
            .split("## 关键错误")
            .nth(1)
            .expect("error section")
            .split("## 期望")
            .next()
            .expect("error body");
        assert_eq!(
            section
                .lines()
                .filter(|line| line.starts_with("- ["))
                .count(),
            8,
            "{section}"
        );
        assert!(section.contains("`TURN_ERROR` (turn 1)"), "{section}");
        assert!(!section.contains("`TOOL_0`"), "{section}");

        let metadata: Value =
            serde_json::from_str(&fs::read_to_string(out.join("metadata.json")).unwrap()).unwrap();
        let errors = metadata["errors"].as_array().unwrap();
        assert_eq!(errors.len(), 11);
        assert!(
            errors
                .iter()
                .all(|error| !error["message"].as_str().unwrap().contains("sk-")),
            "{metadata}"
        );
        let _ = fs::remove_dir_all(base);
    }

    #[test]
    fn reports_a_seq_gap_as_incomplete() {
        let base = temp_dir("seq-gap");
        let journal = journal().replace(
            "{\"type\":\"assistant/message\",\"seq\":2",
            "{\"type\":\"assistant/message\",\"seq\":9",
        );
        let outcome = write_bundle(request(&base, journal.as_bytes())).unwrap();
        assert!(!outcome.complete);
        assert!(
            outcome
                .hard_failures
                .iter()
                .any(|reason| reason.contains("seq gap")),
            "{:?}",
            outcome.hard_failures
        );
        let issue = fs::read_to_string(base.join("issue.md")).unwrap();
        assert!(issue.contains("INCOMPLETE BUNDLE"), "{issue}");
        assert!(issue.contains("complete: `false`"), "{issue}");
        let manifest: Value =
            serde_json::from_str(&fs::read_to_string(base.join("manifest.json")).unwrap()).unwrap();
        assert_eq!(manifest["complete"], false);
        let _ = fs::remove_dir_all(base);
    }

    #[test]
    fn reports_a_torn_tail_as_incomplete() {
        let base = temp_dir("torn-tail");
        let journal = journal();
        let journal = journal.trim_end_matches('\n');
        let outcome = write_bundle(request(&base, journal.as_bytes())).unwrap();
        assert!(!outcome.complete);
        assert!(
            outcome
                .hard_failures
                .iter()
                .any(|reason| reason.contains("torn tail")),
            "{:?}",
            outcome.hard_failures
        );
        let _ = fs::remove_dir_all(base);
    }

    #[test]
    fn reports_a_bad_line_as_incomplete() {
        let base = temp_dir("bad-line");
        let journal = journal().replace(
            "{\"type\":\"request/header\"",
            "not json {\"type\":\"request/header\"",
        );
        let outcome = write_bundle(request(&base, journal.as_bytes())).unwrap();
        assert!(!outcome.complete);
        assert!(
            outcome
                .hard_failures
                .iter()
                .any(|reason| reason.contains("not valid JSON")),
            "{:?}",
            outcome.hard_failures
        );
        let _ = fs::remove_dir_all(base);
    }

    #[test]
    fn rejects_a_different_session_identity() {
        let base = temp_dir("mismatch");
        let journal = journal();
        let mut request = request(&base, journal.as_bytes());
        request.session_id = "session-44444444-4444-4444-8444-444444444444";
        assert!(matches!(
            write_bundle(request),
            Err(FeedbackError::SessionMismatch { .. })
        ));
        assert!(!base.join("issue.md").exists());
        let _ = fs::remove_dir_all(base);
    }

    #[test]
    fn soft_warnings_do_not_block_completeness() {
        let base = temp_dir("soft");
        let journal = journal();
        let journal = journal
            .lines()
            .filter(|line| !line.contains("request/header"))
            .map(|line| format!("{line}\n"))
            .collect::<String>();
        // Removing the request header leaves a hole in the sequence, so renumber
        // the remaining events to keep the hard gate satisfied.
        let journal = journal
            .replace("\"seq\":1", "\"seq\":0")
            .replace("\"seq\":2", "\"seq\":1")
            .replace("\"seq\":3", "\"seq\":2");
        let outcome = write_bundle(request(&base, journal.as_bytes())).unwrap();
        assert!(outcome.complete, "{:?}", outcome.hard_failures);
        assert!(
            outcome
                .warnings
                .iter()
                .any(|warning| warning.contains("request/header")),
            "{:?}",
            outcome.warnings
        );
        let _ = fs::remove_dir_all(base);
    }

    #[test]
    fn derives_the_title_from_the_first_note_line() {
        assert_eq!(
            derive_title(Some("\n  the plan never paused\nmore detail")),
            Some("the plan never paused".to_owned())
        );
        assert_eq!(derive_title(Some("   ")), None);
        assert_eq!(derive_title(None), None);

        let long = "a".repeat(80);
        let title = derive_title(Some(&long)).expect("title");
        assert_eq!(title.chars().count(), 63);
        assert!(title.ends_with("..."));
    }

    #[test]
    fn free_text_drives_the_title_before_the_session_title() {
        let base = temp_dir("title");
        let out = base.join("out");
        let journal = titled_journal();
        let mut request = request(&out, journal.as_bytes());
        request.title = None;
        request.note = Some("the shell call was interrupted".to_owned());

        assert_eq!(
            preview(&request).unwrap().title,
            "the shell call was interrupted"
        );

        request.title = Some("explicit title".to_owned());
        assert_eq!(preview(&request).unwrap().title, "explicit title");
        let _ = fs::remove_dir_all(base);
    }

    #[test]
    fn preview_reports_the_bundle_without_writing() {
        let base = temp_dir("preview");
        let out = base.join("out");
        let bytes = journal().into_bytes();
        let mut request = request(&out, &bytes);
        request.title = None;
        request.note = Some("the plan never paused".to_owned());

        let preview = preview(&request).unwrap();
        assert_eq!(preview.title, "the plan never paused");
        assert!(preview.complete);
        assert_eq!(preview.events, 5);
        assert!(
            !out.exists(),
            "preview must not create the output directory"
        );

        std::fs::create_dir_all(&out).unwrap();
        request.out_dir = out.clone();
        let outcome = write_bundle(request).unwrap();
        assert!(outcome.complete);
        let issue = fs::read_to_string(out.join("issue.md")).unwrap();
        assert!(
            issue.contains("# [feedback] the plan never paused"),
            "{issue}"
        );
        let _ = fs::remove_dir_all(base);
    }
}
