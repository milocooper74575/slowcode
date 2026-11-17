//! Offline audit of one slowcode session journal.
//!
//! The crate is read-only. It parses the JSONL a session already writes,
//! normalizes the events into turns, steps, tool calls, and usage, then
//! reports rule-based waste signals. It never edits the journal and never
//! calls a model.

use std::{
    collections::{BTreeMap, BTreeSet, HashMap},
    path::Path,
};

use serde_json::Value;

#[derive(Debug, thiserror::Error)]
pub enum AuditError {
    #[error("cannot read session journal: {0}")]
    Io(#[from] std::io::Error),
}

/// Token counters as recorded on each assistant message.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct Usage {
    pub input_tokens: u64,
    pub output_tokens: u64,
    pub cache_read_tokens: u64,
    pub reasoning_tokens: u64,
}

impl Usage {
    fn add(&mut self, other: Self) {
        self.input_tokens = self.input_tokens.saturating_add(other.input_tokens);
        self.output_tokens = self.output_tokens.saturating_add(other.output_tokens);
        self.cache_read_tokens = self
            .cache_read_tokens
            .saturating_add(other.cache_read_tokens);
        self.reasoning_tokens = self.reasoning_tokens.saturating_add(other.reasoning_tokens);
    }

    #[must_use]
    pub fn total(&self) -> u64 {
        self.input_tokens
            .saturating_add(self.output_tokens)
            .saturating_add(self.cache_read_tokens)
            .saturating_add(self.reasoning_tokens)
    }
}

/// Where a user-role message came from.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum HumanSource {
    Human,
    Context { plugin: String },
    Other { kind: String },
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct HumanMessage {
    pub seq: u64,
    pub text: String,
    pub source: HumanSource,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ToolResult {
    pub seq: u64,
    pub is_error: bool,
    pub text: String,
    pub diff: Option<String>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ToolCall {
    pub seq: u64,
    pub turn: u64,
    pub step: u64,
    pub call_id: String,
    pub name: String,
    pub arguments: Value,
    pub arguments_key: String,
    pub result: Option<ToolResult>,
}

#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct TurnView {
    pub turn: u64,
    pub reason: String,
    pub steps: usize,
    pub tool_calls: usize,
    pub usage: Usage,
}

#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct StepView {
    pub turn: u64,
    pub step: u64,
    pub tool_calls: usize,
    pub usage: Usage,
}

/// One assistant message that contains a Plan task table.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct PlanOutput {
    pub seq: u64,
    pub turn: u64,
    pub rows: Vec<String>,
    pub evidence: Vec<String>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Approval {
    pub seq: u64,
    pub id: String,
    pub outcome: String,
}

#[derive(Clone, Debug, Default)]
pub struct SessionView {
    pub id: String,
    pub cwd: Option<String>,
    pub event_count: usize,
    pub invalid_lines: usize,
    pub turns: Vec<TurnView>,
    pub steps: Vec<StepView>,
    pub calls: Vec<ToolCall>,
    pub human_messages: Vec<HumanMessage>,
    pub plan_outputs: Vec<PlanOutput>,
    pub approvals: Vec<Approval>,
    pub models: BTreeSet<String>,
    pub usage: Usage,
}

/// One group of reads that repeated a path before the next write.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct RepeatedRead {
    pub path: String,
    pub arguments: String,
    pub redundant: usize,
    pub calls: Vec<CallRef>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct CallRef {
    pub seq: u64,
    pub turn: u64,
    pub step: u64,
    pub call_id: String,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct IdleRetry {
    pub name: String,
    pub arguments: String,
    pub first: CallRef,
    pub retry: CallRef,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct PlanChurn {
    pub first: PlanRef,
    pub second: PlanRef,
    pub rows: usize,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct PlanRef {
    pub seq: u64,
    pub turn: u64,
}

#[derive(Clone, Debug, Default)]
pub struct Signals {
    pub repeated_reads: Vec<RepeatedRead>,
    pub idle_retries: Vec<IdleRetry>,
    pub plan_churn: Vec<PlanChurn>,
}

impl Signals {
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.repeated_reads.is_empty() && self.idle_retries.is_empty() && self.plan_churn.is_empty()
    }
}

/// Load and parse one session journal.
pub fn load_session(path: impl AsRef<Path>) -> Result<SessionView, AuditError> {
    let bytes = std::fs::read(path)?;
    Ok(parse_session(&bytes))
}

/// Parse a session journal. Malformed lines are counted, not fatal.
#[must_use]
pub fn parse_session(journal: &[u8]) -> SessionView {
    let mut view = SessionView::default();
    let mut turns: BTreeMap<u64, TurnView> = BTreeMap::new();
    let mut steps: BTreeMap<(u64, u64), StepView> = BTreeMap::new();
    let mut calls_by_id: HashMap<String, usize> = HashMap::new();

    for line in journal.split(|byte| *byte == b'\n') {
        if line.iter().all(|byte| byte.is_ascii_whitespace()) {
            continue;
        }
        let Ok(text) = std::str::from_utf8(line) else {
            view.invalid_lines += 1;
            continue;
        };
        let Ok(value) = serde_json::from_str::<Value>(text) else {
            view.invalid_lines += 1;
            continue;
        };
        view.event_count += 1;

        let kind = value
            .get("type")
            .and_then(Value::as_str)
            .unwrap_or_default();
        let seq = value.get("seq").and_then(Value::as_u64).unwrap_or(0);
        let data = value.get("data").unwrap_or(&Value::Null);

        match kind {
            "session" => {
                // The header keeps its identity at the top level, not under
                // `data`, and older journals may nest it.
                if let Some(id) = value
                    .get("id")
                    .or_else(|| data.get("id"))
                    .and_then(Value::as_str)
                {
                    view.id = id.to_owned();
                }
                if let Some(cwd) = value
                    .get("cwd")
                    .or_else(|| data.get("cwd"))
                    .and_then(Value::as_str)
                {
                    view.cwd = Some(cwd.to_owned());
                }
            }
            "user/message" => {
                let text = blocks_text(data.get("content"));
                if text.is_empty() {
                    continue;
                }
                let source = user_source(data.get("source"));
                view.human_messages.push(HumanMessage { seq, text, source });
            }
            "assistant/message" => {
                let turn = data.get("turn").and_then(Value::as_u64).unwrap_or(0);
                let step = data.get("step").and_then(Value::as_u64).unwrap_or(0);
                let usage = parse_usage(data.get("usage"));
                apply_usage(&mut turns, &mut steps, turn, step, usage);
                view.usage.add(usage);
                if let Some(model) = data
                    .get("message")
                    .and_then(|message| message.get("source"))
                    .and_then(|source| source.get("model"))
                    .and_then(Value::as_str)
                {
                    view.models.insert(model.to_owned());
                }
                let text = blocks_text(data.get("message").and_then(|m| m.get("content")));
                if text.contains("## 认知任务分配表") {
                    view.plan_outputs.push(PlanOutput {
                        seq,
                        turn,
                        rows: plan_rows(&text),
                        evidence: plan_evidence(&text),
                    });
                }
            }
            "tool/call" => {
                let turn = data.get("turn").and_then(Value::as_u64).unwrap_or(0);
                let step = data.get("step").and_then(Value::as_u64).unwrap_or(0);
                let call_id = data
                    .get("callId")
                    .and_then(Value::as_str)
                    .unwrap_or_default()
                    .to_owned();
                let name = data
                    .get("name")
                    .and_then(Value::as_str)
                    .unwrap_or_default()
                    .to_owned();
                let raw = data
                    .get("arguments")
                    .and_then(Value::as_str)
                    .unwrap_or_default();
                let arguments = serde_json::from_str::<Value>(raw)
                    .unwrap_or_else(|_| Value::String(raw.into()));
                let arguments_key =
                    serde_json::to_string(&arguments).unwrap_or_else(|_| raw.to_owned());
                calls_by_id.insert(call_id.clone(), view.calls.len());
                view.calls.push(ToolCall {
                    seq,
                    turn,
                    step,
                    call_id,
                    name,
                    arguments,
                    arguments_key,
                    result: None,
                });
                if let Some(entry) = steps.get_mut(&(turn, step)) {
                    entry.tool_calls += 1;
                } else {
                    steps.insert(
                        (turn, step),
                        StepView {
                            turn,
                            step,
                            tool_calls: 1,
                            usage: Usage::default(),
                        },
                    );
                }
                if let Some(entry) = turns.get_mut(&turn) {
                    entry.tool_calls += 1;
                }
            }
            "tool/result" => {
                let diff = data
                    .get("meta")
                    .and_then(|meta| meta.get("diff"))
                    .and_then(Value::as_str)
                    .map(str::to_owned);
                let Some(blocks) = data
                    .get("message")
                    .and_then(|message| message.get("content"))
                    .and_then(Value::as_array)
                else {
                    continue;
                };
                for block in blocks {
                    let Some(call_id) = block.get("toolCallId").and_then(Value::as_str) else {
                        continue;
                    };
                    let Some(index) = calls_by_id.get(call_id) else {
                        continue;
                    };
                    let call = &mut view.calls[*index];
                    call.result = Some(ToolResult {
                        seq,
                        is_error: block
                            .get("isError")
                            .and_then(Value::as_bool)
                            .unwrap_or(false),
                        text: blocks_text(block.get("content")),
                        diff: diff.clone(),
                    });
                }
            }
            "turn/start" => {
                let turn = data.get("turn").and_then(Value::as_u64).unwrap_or(0);
                turns.entry(turn).or_insert_with(|| TurnView {
                    turn,
                    ..TurnView::default()
                });
            }
            "turn/end" => {
                let turn = data.get("turn").and_then(Value::as_u64).unwrap_or(0);
                let reason = data
                    .get("reason")
                    .and_then(|reason| reason.get("kind"))
                    .and_then(Value::as_str)
                    .unwrap_or_default()
                    .to_owned();
                turns
                    .entry(turn)
                    .or_insert_with(|| TurnView {
                        turn,
                        ..TurnView::default()
                    })
                    .reason = reason;
            }
            "step/start" | "step/end" => {
                let turn = data.get("turn").and_then(Value::as_u64).unwrap_or(0);
                let step = data.get("step").and_then(Value::as_u64).unwrap_or(0);
                steps.entry((turn, step)).or_insert_with(|| StepView {
                    turn,
                    step,
                    tool_calls: 0,
                    usage: Usage::default(),
                });
            }
            "approval/decided" => {
                let id = data
                    .get("id")
                    .and_then(Value::as_str)
                    .unwrap_or_default()
                    .to_owned();
                let outcome = data
                    .get("outcome")
                    .and_then(Value::as_str)
                    .unwrap_or_default()
                    .to_owned();
                view.approvals.push(Approval { seq, id, outcome });
            }
            _ => {}
        }
    }

    view.turns = turns.into_values().collect();
    view.steps = steps.into_values().collect();
    view
}

fn apply_usage(
    turns: &mut BTreeMap<u64, TurnView>,
    steps: &mut BTreeMap<(u64, u64), StepView>,
    turn: u64,
    step: u64,
    usage: Usage,
) {
    let turn_entry = turns.entry(turn).or_insert_with(|| TurnView {
        turn,
        ..TurnView::default()
    });
    turn_entry.usage.add(usage);
    let step_entry = steps.entry((turn, step)).or_insert_with(|| StepView {
        turn,
        step,
        tool_calls: 0,
        usage: Usage::default(),
    });
    step_entry.usage.add(usage);
    // Count each step that produced an assistant message, even when no tool
    // call follows it.
    turn_entry.steps = turn_entry.steps.max(usize::try_from(step).unwrap_or(0));
}

fn parse_usage(value: Option<&Value>) -> Usage {
    let field = |name: &str| {
        value
            .and_then(|usage| usage.get(name))
            .and_then(Value::as_u64)
            .unwrap_or(0)
    };
    Usage {
        input_tokens: field("inputTokens"),
        output_tokens: field("outputTokens"),
        cache_read_tokens: field("cacheReadTokens"),
        reasoning_tokens: field("reasoningTokens"),
    }
}

fn user_source(value: Option<&Value>) -> HumanSource {
    let kind = value
        .and_then(|source| source.get("kind"))
        .and_then(Value::as_str)
        .unwrap_or_default();
    match kind {
        "user" => HumanSource::Human,
        "plugin" => HumanSource::Context {
            plugin: value
                .and_then(|source| source.get("plugin"))
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_owned(),
        },
        other => HumanSource::Other {
            kind: other.to_owned(),
        },
    }
}

fn blocks_text(content: Option<&Value>) -> String {
    let Some(items) = content.and_then(Value::as_array) else {
        return String::new();
    };
    let mut text = String::new();
    for item in items {
        if item.get("type").and_then(Value::as_str) != Some("text") {
            continue;
        }
        let Some(part) = item.get("text").and_then(Value::as_str) else {
            continue;
        };
        if !text.is_empty() {
            text.push('\n');
        }
        text.push_str(part);
    }
    text
}

fn plan_rows(text: &str) -> Vec<String> {
    let mut rows = Vec::new();
    for line in text.lines() {
        let trimmed = line.trim();
        if !trimmed.starts_with('|') {
            continue;
        }
        let cells: Vec<&str> = trimmed
            .split('|')
            .map(str::trim)
            .filter(|cell| !cell.is_empty())
            .collect();
        if cells.is_empty() {
            continue;
        }
        if cells
            .iter()
            .all(|cell| cell.chars().all(|ch| ch == '-' || ch == ':'))
        {
            continue;
        }
        if cells.contains(&"任务") && cells.contains(&"认知属性") {
            continue;
        }
        rows.push(cells.join(" | "));
    }
    rows.sort();
    rows.dedup();
    rows
}

fn plan_evidence(text: &str) -> Vec<String> {
    let mut evidence: Vec<String> = text
        .lines()
        .map(str::trim)
        .filter(|line| line.starts_with("- "))
        .map(str::to_owned)
        .collect();
    evidence.sort();
    evidence.dedup();
    evidence
}

/// Rule-based waste signals. No model calls and no journal writes.
#[must_use]
pub fn signals(view: &SessionView) -> Signals {
    Signals {
        repeated_reads: repeated_reads(view),
        idle_retries: idle_retries(view),
        plan_churn: plan_churn(view),
    }
}

fn repeated_reads(view: &SessionView) -> Vec<RepeatedRead> {
    // Key by path and arguments so a different slice of the same file is not
    // counted as a repeated read.
    let mut open: HashMap<(String, String), Vec<&ToolCall>> = HashMap::new();
    let mut groups: Vec<RepeatedRead> = Vec::new();

    for call in &view.calls {
        match call.name.as_str() {
            "read" => {
                if let Some(path) = file_path(call) {
                    open.entry((path.to_owned(), call.arguments_key.clone()))
                        .or_default()
                        .push(call);
                }
            }
            "write" | "edit" => {
                if let Some(path) = file_path(call) {
                    let keys: Vec<(String, String)> = open
                        .keys()
                        .filter(|(key_path, _)| key_path == path)
                        .cloned()
                        .collect();
                    for key in keys {
                        if let Some(group) = open.remove(&key) {
                            push_read_group(&mut groups, &key, group);
                        }
                    }
                }
            }
            _ => {}
        }
    }
    for (key, group) in open {
        push_read_group(&mut groups, &key, group);
    }
    groups.sort_by_key(|group| group.calls.first().map(|call| call.seq).unwrap_or(0));
    groups
}

fn push_read_group(
    groups: &mut Vec<RepeatedRead>,
    (path, arguments): &(String, String),
    group: Vec<&ToolCall>,
) {
    if group.len() < 2 {
        return;
    }
    let calls: Vec<CallRef> = group.iter().map(|call| call_ref(call)).collect();
    groups.push(RepeatedRead {
        path: path.to_owned(),
        arguments: arguments.clone(),
        redundant: group.len() - 1,
        calls,
    });
}

fn idle_retries(view: &SessionView) -> Vec<IdleRetry> {
    let mut retries = Vec::new();
    for pair in view.calls.windows(2) {
        let first = &pair[0];
        let retry = &pair[1];
        let failed = first.result.as_ref().is_some_and(|result| result.is_error);
        if failed && first.name == retry.name && first.arguments_key == retry.arguments_key {
            retries.push(IdleRetry {
                name: retry.name.clone(),
                arguments: retry.arguments_key.clone(),
                first: call_ref(first),
                retry: call_ref(retry),
            });
        }
    }
    retries
}

fn plan_churn(view: &SessionView) -> Vec<PlanChurn> {
    let mut churn = Vec::new();
    for pair in view.plan_outputs.windows(2) {
        let first = &pair[0];
        let second = &pair[1];
        let same_rows = first.rows == second.rows;
        let no_new_evidence = second
            .evidence
            .iter()
            .all(|item| first.evidence.contains(item));
        if same_rows && no_new_evidence {
            churn.push(PlanChurn {
                first: PlanRef {
                    seq: first.seq,
                    turn: first.turn,
                },
                second: PlanRef {
                    seq: second.seq,
                    turn: second.turn,
                },
                rows: second.rows.len(),
            });
        }
    }
    churn
}

fn file_path(call: &ToolCall) -> Option<&str> {
    call.arguments
        .get("file_path")
        .or_else(|| call.arguments.get("path"))
        .and_then(Value::as_str)
}

fn call_ref(call: &ToolCall) -> CallRef {
    CallRef {
        seq: call.seq,
        turn: call.turn,
        step: call.step,
        call_id: call.call_id.clone(),
    }
}

/// Render one Markdown audit report.
#[must_use]
pub fn render_report(view: &SessionView, signals: &Signals) -> String {
    let mut out = String::new();

    out.push_str(&format!(
        "# Session audit: {}\n\n",
        if view.id.is_empty() {
            "unknown"
        } else {
            &view.id
        }
    ));
    if let Some(cwd) = &view.cwd {
        out.push_str(&format!("- workspace: `{cwd}`\n"));
    }
    out.push_str(&format!(
        "- events: {} ({} invalid lines)\n",
        view.event_count, view.invalid_lines
    ));
    out.push_str(&format!(
        "- models: {}\n",
        if view.models.is_empty() {
            "unknown".to_owned()
        } else {
            view.models.iter().cloned().collect::<Vec<_>>().join(", ")
        }
    ));
    out.push_str(&format!(
        "- tokens: input {}, output {}, cache read {}, reasoning {}, total {}\n\n",
        view.usage.input_tokens,
        view.usage.output_tokens,
        view.usage.cache_read_tokens,
        view.usage.reasoning_tokens,
        view.usage.total()
    ));

    let total = view.usage.total();
    if total > 0 {
        let share =
            |part: u64| (part.saturating_mul(100).saturating_add(total / 2) / total).min(100);
        out.push_str(&format!(
            "Token mix: cache reads {}%, input {}%, output {}%, reasoning {}%. Cache reads are cheap per token; a high share still means the same prefix is being resent every step.\n\n",
            share(view.usage.cache_read_tokens),
            share(view.usage.input_tokens),
            share(view.usage.output_tokens),
            share(view.usage.reasoning_tokens)
        ));
    }

    out.push_str("## Turns\n\n");
    if view.turns.is_empty() {
        out.push_str("No turns recorded.\n\n");
    } else {
        out.push_str("| turn | steps | tool calls | input | output | end |\n");
        out.push_str("|---|---|---|---|---|---|\n");
        for turn in &view.turns {
            out.push_str(&format!(
                "| {} | {} | {} | {} | {} | {} |\n",
                turn.turn,
                turn.steps,
                turn.tool_calls,
                turn.usage.input_tokens,
                turn.usage.output_tokens,
                if turn.reason.is_empty() {
                    "-"
                } else {
                    &turn.reason
                }
            ));
        }
        out.push('\n');
    }

    out.push_str("## Tools\n\n");
    let tools = tool_counts(view);
    if tools.is_empty() {
        out.push_str("No tool calls recorded.\n\n");
    } else {
        out.push_str("| tool | calls | errors |\n|---|---|---|\n");
        for (name, calls, errors) in tools {
            out.push_str(&format!("| `{name}` | {calls} | {errors} |\n"));
        }
        out.push('\n');
    }

    out.push_str("## Human input\n\n");
    let human: Vec<&HumanMessage> = view
        .human_messages
        .iter()
        .filter(|message| message.source == HumanSource::Human)
        .collect();
    if human.is_empty() {
        out.push_str("No human message was recorded. Plan commands before the\n");
        out.push_str("human-source change are invisible here.\n\n");
    } else {
        for message in human {
            out.push_str(&format!(
                "- seq {}: {}\n",
                message.seq,
                clip(&message.text, 240)
            ));
        }
        out.push('\n');
    }

    out.push_str("## Signals\n\n");
    if signals.is_empty() {
        out.push_str("No rule-based waste signal fired.\n\n");
    }

    out.push_str(&format!(
        "### Repeated reads ({})\n\n",
        signals.repeated_reads.len()
    ));
    if signals.repeated_reads.is_empty() {
        out.push_str("No path was read twice without an intervening write.\n\n");
    } else {
        for group in &signals.repeated_reads {
            let locations: Vec<String> = group
                .calls
                .iter()
                .map(|call| format!("t{}/s{} seq {}", call.turn, call.step, call.seq))
                .collect();
            out.push_str(&format!(
                "- `{}` {}: {} identical reads, {} redundant ({}). Reuse the first result.\n",
                group.path,
                group.arguments,
                group.calls.len(),
                group.redundant,
                locations.join(", ")
            ));
        }
        out.push('\n');
    }

    out.push_str(&format!(
        "### Idle retries ({})\n\n",
        signals.idle_retries.len()
    ));
    if signals.idle_retries.is_empty() {
        out.push_str("No identical tool call followed a failure.\n\n");
    } else {
        for retry in &signals.idle_retries {
            out.push_str(&format!(
                "- `{}` at t{}/s{} seq {} failed; the same arguments were retried at t{}/s{} seq {}. Change one input before retrying.\n",
                retry.name,
                retry.first.turn,
                retry.first.step,
                retry.first.seq,
                retry.retry.turn,
                retry.retry.step,
                retry.retry.seq
            ));
        }
        out.push('\n');
    }

    out.push_str(&format!(
        "### Plan revisions without new constraints ({})\n\n",
        signals.plan_churn.len()
    ));
    if signals.plan_churn.is_empty() {
        out.push_str("Every repeated plan output changed its task table or evidence.\n\n");
    } else {
        for churn in &signals.plan_churn {
            out.push_str(&format!(
                "- plan at seq {} (turn {}) repeated at seq {} (turn {}) with {} identical task rows and no new evidence. Ask what changed before spending another turn.\n",
                churn.first.seq, churn.first.turn, churn.second.seq, churn.second.turn, churn.rows
            ));
        }
        out.push('\n');
    }

    out.push_str("## Notes\n\n");
    out.push_str("- Rule-based only: no model judge, no counterfactual replay.\n");
    out.push_str("- Token counts come from each assistant message's `usage`.\n");
    out.push_str(
        "- Repeated reads inspect the `read` tool only; bash-mediated reads are not counted.\n",
    );
    out.push_str("- The journal is opened read-only.\n");

    out
}

fn tool_counts(view: &SessionView) -> Vec<(String, usize, usize)> {
    let mut counts: BTreeMap<String, (usize, usize)> = BTreeMap::new();
    for call in &view.calls {
        let entry = counts.entry(call.name.clone()).or_default();
        entry.0 += 1;
        if call.result.as_ref().is_some_and(|result| result.is_error) {
            entry.1 += 1;
        }
    }
    let mut rows: Vec<(String, usize, usize)> = counts
        .into_iter()
        .map(|(name, (calls, errors))| (name, calls, errors))
        .collect();
    rows.sort_by(|a, b| b.1.cmp(&a.1).then_with(|| a.0.cmp(&b.0)));
    rows
}

#[must_use]
pub fn clip(text: &str, limit: usize) -> String {
    if text.chars().count() <= limit {
        return text.to_owned();
    }
    let mut clipped: String = text.chars().take(limit).collect();
    clipped.push('…');
    clipped
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn journal(events: Vec<Value>) -> Vec<u8> {
        let mut text = String::new();
        for event in events {
            text.push_str(&event.to_string());
            text.push('\n');
        }
        text.into_bytes()
    }

    fn header() -> Value {
        json!({"type": "session", "id": "session-audit", "cwd": "/workspace", "version": 0})
    }

    #[test]
    fn a_nested_session_header_still_parses() {
        let view = parse_session(&journal(vec![json!({
            "type": "session",
            "data": {"id": "session-old", "cwd": "/old"}
        })]));
        assert_eq!(view.id, "session-old");
        assert_eq!(view.cwd.as_deref(), Some("/old"));
    }

    fn user(seq: u64, text: &str, source: Value) -> Value {
        json!({"type": "user/message", "seq": seq, "data": {"content": [{"type": "text", "text": text}], "source": source}})
    }

    fn assistant(seq: u64, turn: u64, step: u64, text: &str, input: u64, output: u64) -> Value {
        json!({
            "type": "assistant/message",
            "seq": seq,
            "data": {
                "turn": turn,
                "step": step,
                "usage": {"inputTokens": input, "outputTokens": output, "cacheReadTokens": 0, "reasoningTokens": 0},
                "message": {
                    "content": [{"type": "text", "text": text}],
                    "source": {"kind": "model", "model": "test-model", "provider": "test"}
                }
            }
        })
    }

    fn call(seq: u64, turn: u64, step: u64, id: &str, name: &str, args: Value) -> Value {
        json!({
            "type": "tool/call",
            "seq": seq,
            "data": {"turn": turn, "step": step, "callId": id, "name": name, "arguments": args.to_string()}
        })
    }

    fn result(seq: u64, id: &str, text: &str, is_error: bool) -> Value {
        json!({
            "type": "tool/result",
            "seq": seq,
            "data": {"message": {"content": [{"type": "tool-result", "toolCallId": id, "isError": is_error, "content": [{"type": "text", "text": text}]}]}}
        })
    }

    #[test]
    fn parses_human_and_plugin_sources() {
        let view = parse_session(&journal(vec![
            header(),
            user(1, "count lines", json!({"kind": "user"})),
            user(
                2,
                "先执行 Explore",
                json!({"kind": "plugin", "plugin": "slowcode-plan"}),
            ),
            assistant(3, 1, 1, "ok", 100, 10),
            json!({
                "type": "approval/decided",
                "seq": 4,
                "data": {"id": "approval-1", "outcome": "allowed-once"}
            }),
        ]));
        assert_eq!(view.id, "session-audit");
        assert_eq!(view.human_messages.len(), 2);
        assert_eq!(view.human_messages[0].source, HumanSource::Human);
        assert_eq!(
            view.human_messages[1].source,
            HumanSource::Context {
                plugin: "slowcode-plan".into()
            }
        );
        assert_eq!(view.usage.input_tokens, 100);
        assert_eq!(view.usage.output_tokens, 10);
        assert_eq!(view.turns[0].turn, 1);
        assert_eq!(view.steps[0].step, 1);
        assert_eq!(view.approvals.len(), 1);
        assert_eq!(view.approvals[0].outcome, "allowed-once");
    }

    #[test]
    fn counts_repeated_reads_until_the_next_write() {
        let view = parse_session(&journal(vec![
            header(),
            call(1, 1, 1, "c1", "read", json!({"file_path": "a.rs"})),
            result(2, "c1", "fn a", false),
            call(3, 1, 2, "c2", "read", json!({"file_path": "a.rs"})),
            result(4, "c2", "fn a", false),
            call(5, 1, 3, "c3", "read", json!({"file_path": "a.rs"})),
            result(6, "c3", "fn a", false),
            call(7, 1, 4, "c4", "edit", json!({"file_path": "a.rs"})),
            result(8, "c4", "edited", false),
            call(9, 1, 5, "c5", "read", json!({"file_path": "a.rs"})),
            result(10, "c5", "fn b", false),
        ]));
        let signals = signals(&view);
        assert_eq!(signals.repeated_reads.len(), 1);
        assert_eq!(signals.repeated_reads[0].path, "a.rs");
        assert_eq!(signals.repeated_reads[0].redundant, 2);
        assert_eq!(signals.repeated_reads[0].calls.len(), 3);
    }

    #[test]
    fn a_write_resets_the_read_group() {
        let view = parse_session(&journal(vec![
            header(),
            call(1, 1, 1, "c1", "read", json!({"file_path": "a.rs"})),
            result(2, "c1", "fn a", false),
            call(3, 1, 2, "c2", "write", json!({"file_path": "a.rs"})),
            result(4, "c2", "written", false),
            call(5, 1, 3, "c3", "read", json!({"file_path": "a.rs"})),
            result(6, "c3", "fn b", false),
        ]));
        assert!(signals(&view).repeated_reads.is_empty());
    }

    #[test]
    fn different_slices_of_one_file_are_not_repeated_reads() {
        let view = parse_session(&journal(vec![
            header(),
            call(
                1,
                1,
                1,
                "c1",
                "read",
                json!({"file_path": "a.rs", "limit": 400}),
            ),
            result(2, "c1", "lines 1-400", false),
            call(
                3,
                1,
                2,
                "c2",
                "read",
                json!({"file_path": "a.rs", "offset": 400, "limit": 450}),
            ),
            result(4, "c2", "lines 400-850", false),
        ]));
        assert!(signals(&view).repeated_reads.is_empty());
    }

    #[test]
    fn flags_identical_retry_after_a_failure() {
        let view = parse_session(&journal(vec![
            header(),
            call(1, 1, 1, "c1", "bash", json!({"command": "false"})),
            result(2, "c1", "exit 1", true),
            call(3, 1, 2, "c2", "bash", json!({"command": "false"})),
            result(4, "c2", "exit 1", true),
            call(5, 1, 3, "c3", "bash", json!({"command": "true"})),
            result(6, "c3", "ok", false),
        ]));
        let signals = signals(&view);
        assert_eq!(signals.idle_retries.len(), 1);
        assert_eq!(signals.idle_retries[0].name, "bash");
        assert_eq!(signals.idle_retries[0].first.seq, 1);
        assert_eq!(signals.idle_retries[0].retry.seq, 3);
    }

    #[test]
    fn changed_arguments_are_not_an_idle_retry() {
        let view = parse_session(&journal(vec![
            header(),
            call(1, 1, 1, "c1", "bash", json!({"command": "false"})),
            result(2, "c1", "exit 1", true),
            call(3, 1, 2, "c2", "bash", json!({"command": "true"})),
            result(4, "c2", "ok", false),
        ]));
        assert!(signals(&view).idle_retries.is_empty());
    }

    #[test]
    fn flags_a_plan_revision_without_new_rows_or_evidence() {
        let plan = "\
## 证据包
- read src/main.rs

## 认知任务分配表
| 任务 | 认知属性 | 判定依据 | 执行状态 | Agent 行为 | 用户角色 | 最小深度链路 | 验收 |
|---|---|---|---|---|---|---|---|
| 读取输入 | 已知域 | src/main.rs | 可执行 | 自主执行 | 审查者 | read | 打印内容 |
";
        let view = parse_session(&journal(vec![
            header(),
            assistant(1, 1, 1, plan, 10, 10),
            assistant(2, 1, 2, plan, 10, 10),
        ]));
        let signals = signals(&view);
        assert_eq!(signals.plan_churn.len(), 1);
        assert_eq!(signals.plan_churn[0].rows, 1);
    }

    #[test]
    fn a_new_task_row_is_not_plan_churn() {
        let first = "\
## 证据包
- read src/main.rs

## 认知任务分配表
| 任务 | 认知属性 | 判定依据 | 执行状态 | Agent 行为 | 用户角色 | 最小深度链路 | 验收 |
|---|---|---|---|---|---|---|---|
| 读取输入 | 已知域 | src/main.rs | 可执行 | 自主执行 | 审查者 | read | 打印内容 |
";
        let second = "\
## 证据包
- read src/main.rs

## 认知任务分配表
| 任务 | 认知属性 | 判定依据 | 执行状态 | Agent 行为 | 用户角色 | 最小深度链路 | 验收 |
|---|---|---|---|---|---|---|---|
| 读取输入 | 已知域 | src/main.rs | 可执行 | 自主执行 | 审查者 | read | 打印内容 |
| 处理错误 | 半已知域 | src/main.rs | 待决策 | 读取 | 决策者 | read | 打印错误 |
";
        let view = parse_session(&journal(vec![
            header(),
            assistant(1, 1, 1, first, 10, 10),
            assistant(2, 2, 1, second, 10, 10),
        ]));
        assert!(signals(&view).plan_churn.is_empty());
    }

    #[test]
    fn report_carries_the_human_input_and_signal_sections() {
        let view = parse_session(&journal(vec![
            header(),
            user(1, "count lines", json!({"kind": "user"})),
            call(2, 1, 1, "c1", "read", json!({"file_path": "a.rs"})),
            result(3, "c1", "fn a", false),
            call(4, 1, 2, "c2", "read", json!({"file_path": "a.rs"})),
            result(5, "c2", "fn a", false),
            assistant(6, 1, 3, "done", 100, 10),
        ]));
        let report = render_report(&view, &signals(&view));
        assert!(report.contains("# Session audit: session-audit"));
        assert!(report.contains("count lines"));
        assert!(report.contains("## Signals"));
        assert!(report.contains("### Repeated reads (1)"));
        assert!(report.contains("`a.rs`"));
        assert!(report.contains("## Tools"));
        assert!(report.contains("| `read` | 2 | 0 |"));
        assert!(report.contains("Token mix:"));
        assert!(report.contains("read-only"));
    }

    #[test]
    fn invalid_lines_are_counted_not_fatal() {
        let mut bytes = journal(vec![header()]);
        bytes.extend_from_slice(b"{not json}\n");
        let view = parse_session(&bytes);
        assert_eq!(view.invalid_lines, 1);
        assert_eq!(view.id, "session-audit");
    }
}
