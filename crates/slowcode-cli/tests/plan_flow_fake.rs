//! Offline integration test for the Plan command flow:
//! /answer -> /scaffold -> /execute -> /distill.

use std::{
    collections::VecDeque,
    fs,
    path::{Path, PathBuf},
    sync::{
        Arc, Mutex,
        atomic::{AtomicU64, Ordering},
    },
    time::{SystemTime, UNIX_EPOCH},
};

use futures_util::stream;
use serde_json::json;
use slowcode_app::{
    plan::{PlanConfig, run_approve},
    workflow::{
        Domain, ExecState, Phase, ScaffoldContract, Strategy, Task, TestContract, WorkflowStatus,
        WorkflowStore,
    },
};
use slowcode_cli::{
    CliOptions, run_answer_with_provider, run_continue_with_provider, run_distill_with_provider,
    run_execute_with_provider, run_fix_with_provider, run_plan_with_provider,
    run_scaffold_with_provider,
};
use slowcode_kernel::{
    model::{
        ContentBlock, ContentBlockType, FinishReason, LlmCallConfig, LlmCallConfigAdapterDefaults,
        NonNegativeSafeInteger, StreamChunk,
    },
    provider::{
        ModelProvider, PreparedProviderCall, PreparedRequestPreflight, ProviderPreflightError,
        ProviderPrepareError, ProviderRequest, ProviderRequestDraft, ProviderStream,
        ProviderStreamError,
    },
};
use tokio_util::sync::CancellationToken;

const ANSWER: &str = "\
## 决策记录
| 任务 | 用户决策 | 新认知属性 | 新执行状态 |
|---|---|---|---|
| 选择缓存策略 | 选 A | 已知域 | 可执行 |
";

const SCAFFOLD: &str = "\
## Scaffold manifest
| 任务 | 文件 | 留空位置 | 验收命令 |
|---|---|---|---|
| 设计解析算法 | src/lib.rs | parse 核心 | cargo test |
| 设计解析算法 | tests/parse_test.rs | | cargo test |
";

const DISTILL: &str = "\
## 理解漏洞报告
### 清晰
读取层与计数层的边界。
### 模糊
CRLF 的处理。
### 未覆盖边界
空文件。
### 追问
如果文件没有尾随换行，你的计数规则是什么？
";

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

struct NoCallProvider;

impl ModelProvider for NoCallProvider {
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

fn text_response(text: &str) -> Vec<Result<StreamChunk, ProviderStreamError>> {
    vec![
        Ok(StreamChunk::block_start(0, ContentBlockType::Text).unwrap()),
        Ok(StreamChunk::text_delta(0, text).unwrap()),
        Ok(StreamChunk::block_end(0, ContentBlock::text(text).unwrap()).unwrap()),
        Ok(StreamChunk::finish(FinishReason::stop().unwrap(), None).unwrap()),
    ]
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

fn seed_cargo_project(workspace: &Path) {
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

fn temp_workspace() -> PathBuf {
    static COUNTER: AtomicU64 = AtomicU64::new(0);
    let nonce = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("clock is after epoch")
        .as_nanos();
    let unique = COUNTER.fetch_add(1, Ordering::Relaxed);
    let path = std::env::temp_dir().join(format!(
        "slowcode-flow-{}-{nonce}-{unique}",
        std::process::id()
    ));
    fs::create_dir_all(&path).expect("temp workspace is created");
    path
}

fn write_plan(workspace: &Path, plan: &str) {
    let directory = workspace.join(".scratch/plans");
    fs::create_dir_all(&directory).expect("plan directory is created");
    fs::write(directory.join("1000-plan.md"), plan).expect("plan artifact is written");
}

fn workflow_task(
    id: &str,
    title: &str,
    domain: Domain,
    exec_state: ExecState,
    strategy: Strategy,
) -> Task {
    Task::new(
        id.to_owned(),
        title.to_owned(),
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

fn seed_review_workflow(workspace: &Path) {
    let store = WorkflowStore::new(workspace);
    let tasks = vec![
        workflow_task(
            "task-1",
            "读取输入",
            Domain::Known,
            ExecState::Executable,
            Strategy::AiImplements,
        ),
        workflow_task(
            "task-2",
            "选择缓存策略",
            Domain::SemiKnown,
            ExecState::PendingDecision,
            Strategy::Undecided,
        ),
        workflow_task(
            "task-3",
            "设计解析算法",
            Domain::Unknown,
            ExecState::PendingUser,
            Strategy::UserImplementsCore,
        ),
    ];
    store
        .create_with_tasks("test", None, tasks)
        .expect("workflow is seeded");
}

fn seed_test_fix_workflow(workspace: &Path) {
    let store = WorkflowStore::new(workspace);
    let state = store
        .create_with_tasks(
            "test",
            None,
            vec![workflow_task(
                "task-1",
                "读取输入",
                Domain::Known,
                ExecState::Executable,
                Strategy::AiImplements,
            )],
        )
        .expect("workflow is seeded");
    store
        .approve(&state.workflow_id, Some(TestContract::default()), None)
        .expect("approval");
}

fn seed_green_workflow(workspace: &Path) {
    let store = WorkflowStore::new(workspace);
    let state = store
        .create_with_tasks(
            "distill",
            None,
            vec![workflow_task(
                "task-1",
                "读取输入",
                Domain::Known,
                ExecState::Executable,
                Strategy::UserImplementsCore,
            )],
        )
        .expect("workflow is seeded");
    let state = store
        .approve(&state.workflow_id, Some(TestContract::default()), None)
        .expect("approval");
    let state = store
        .finish_scaffold(&state.workflow_id, ScaffoldContract::default(), None)
        .expect("scaffold enters await user");
    let state = store
        .complete_user_implementation(&state.workflow_id, true, None)
        .expect("continue enters test fix");
    store
        .record_test(&state.workflow_id, true, None)
        .expect("test is recorded");
}

fn plan_text(executable: &str, semi: &str, unknown: &str) -> String {
    format!(
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

## Agent output

## 证据包
- read src/main.rs

## 认知任务分配表
| 任务 | 认知属性 | 判定依据 | 执行状态 | Agent 行为 | 用户角色 | 最小深度链路 | 验收 |
|---|---|---|---|---|---|---|---|
| 读取输入 | 已知域 | src/main.rs 的 read 工具 | {executable} | 自主执行 | 审查者 | read | 打印内容 |
| 选择缓存策略 | 半已知域 | 仓库有缓存层，策略未定 | {semi} | 给出选项 | 决策者 | 决策后实现 | 用户确认 |
| 设计解析算法 | 未知域 | 仓库没有解析规则 | {unknown} | 搭建脚手架 | 创造者 | parse 核心留空 | 通过测试 |

## 半已知域决策
- 选择缓存策略：A 或 B

## 未知域暂停
- 解析算法采用哪种规则？
"
    )
}

fn options(workspace: &Path) -> CliOptions {
    CliOptions {
        workspace: workspace.to_owned(),
        model: "fake-model".to_owned(),
        plan: None,
        max_tokens: None,
        context_window: None,
        read_only: false,
        ask: false,
        session: None,
        resume: false,
        prompt: None,
    }
}

#[tokio::test]
async fn answer_scaffold_execute_distill_flow() {
    let workspace = temp_workspace();
    seed_cargo_project(&workspace);
    write_plan(&workspace, &plan_text("可执行", "待决策", "待用户实现"));
    seed_review_workflow(&workspace);

    let provider: Arc<dyn ModelProvider> = Arc::new(FakeProvider::with_attempts(vec![
        text_response(ANSWER),
        write_call_response(
            "src/lib.rs",
            "pub fn parse(value: &str) -> String {\n    // TODO(user)\n    String::new()\n}\n",
        ),
        text_response(SCAFFOLD),
        text_response(EXECUTION_REPORT),
        text_response(DISTILL),
    ]));
    let options = options(&workspace);

    run_answer_with_provider(&options, Arc::clone(&provider), None, "选 A")
        .await
        .expect("answer succeeds");
    run_approve(&PlanConfig::new(workspace.clone()))
        .await
        .expect("approval succeeds");
    run_scaffold_with_provider(&options, Arc::clone(&provider), None)
        .await
        .expect("scaffold succeeds");
    let execute = run_execute_with_provider(&options, Arc::clone(&provider), None)
        .await
        .expect_err("execute is blocked while the user owns the core");
    assert!(execute.to_string().contains("await_user"), "{execute}");
    let pending = run_continue_with_provider(&options, Arc::clone(&provider), None)
        .await
        .expect_err("continue is blocked while the marker remains");
    assert!(pending.to_string().contains("TODO(user)"), "{pending}");

    fs::write(
        workspace.join("src/lib.rs"),
        "pub fn parse(value: &str) -> String {\n    value.to_owned()\n}\n",
    )
    .expect("user implementation");
    let continued = run_continue_with_provider(&options, Arc::clone(&provider), None)
        .await
        .expect("continue succeeds");
    assert!(
        continued.output.contains("next: /execute"),
        "{}",
        continued.output
    );

    run_execute_with_provider(&options, Arc::clone(&provider), None)
        .await
        .expect("execute runs the green contract");
    let distill = run_distill_with_provider(&options, provider, None, "我实现了 parse 的核心循环")
        .await
        .expect("distill succeeds after a green run");
    assert!(distill.output.contains("## 理解漏洞报告"));

    let artifact = fs::read_to_string(workspace.join(".scratch/plans/1000-plan.md"))
        .expect("artifact is readable");
    assert!(artifact.contains("## 决策记录"), "{artifact}");
    assert!(artifact.contains("## Scaffold"), "{artifact}");
    assert!(artifact.contains("## Continue"), "{artifact}");
    assert!(artifact.contains("## Test"), "{artifact}");
    assert!(artifact.contains("## Distill"), "{artifact}");
    assert!(artifact.contains("选择缓存策略"), "{artifact}");
    assert!(artifact.contains("可执行"), "{artifact}");

    let _ = fs::remove_dir_all(&workspace);
}

#[tokio::test]
async fn fix_requires_a_red_test() {
    let workspace = temp_workspace();
    write_plan(&workspace, &plan_text("待决策", "待决策", "待用户实现"));
    seed_test_fix_workflow(&workspace);
    let options = options(&workspace);
    let error = run_fix_with_provider(&options, Arc::new(NoCallProvider), None)
        .await
        .expect_err("fix must refuse");
    assert!(error.to_string().contains("run /test first"), "{error}");
    let _ = fs::remove_dir_all(&workspace);
}

const BAD_DISTILL: &str = "\
## 理解漏洞报告

### 清晰
读取层已说明。

### 模糊

### 未覆盖边界
空文件。

### 追问
还需要确认边界。
";

const REPAIRED_DISTILL: &str = "\
## 理解漏洞报告

### 清晰
读取层与计数层分离。

### 模糊
CRLF 与空行的处理。

### 未覆盖边界
空文件和没有尾随换行的文件。

### 追问
没有尾随换行时，你的计数规则是什么？
";

#[tokio::test]
async fn distill_repairs_bad_format_once_then_marks_distilled() {
    let workspace = temp_workspace();
    write_plan(&workspace, &plan_text("可执行", "待决策", "待用户实现"));
    seed_green_workflow(&workspace);
    let options = options(&workspace);
    let provider: Arc<dyn ModelProvider> =
        Arc::new(FakeProvider::new(&[BAD_DISTILL, REPAIRED_DISTILL]));

    run_distill_with_provider(&options, provider, None, "我实现了 parse 的核心循环")
        .await
        .expect("the repaired report is accepted");

    let artifact = fs::read_to_string(workspace.join(".scratch/plans/1000-plan.md"))
        .expect("artifact is readable");
    assert!(artifact.contains("status: distilled"), "{artifact}");
    assert!(artifact.contains("CRLF 与空行的处理"), "{artifact}");
    assert!(
        artifact.contains("没有尾随换行时，你的计数规则是什么？"),
        "{artifact}"
    );
    assert!(
        !artifact.contains("### 理解漏洞报告"),
        "the report heading must not be duplicated: {artifact}"
    );

    let _ = fs::remove_dir_all(&workspace);
}

#[tokio::test]
async fn distill_keeps_the_artifact_undistilled_after_a_failed_repair() {
    let workspace = temp_workspace();
    write_plan(&workspace, &plan_text("可执行", "待决策", "待用户实现"));
    seed_green_workflow(&workspace);
    let options = options(&workspace);
    let before = fs::read_to_string(workspace.join(".scratch/plans/1000-plan.md"))
        .expect("artifact is readable");
    let provider: Arc<dyn ModelProvider> = Arc::new(FakeProvider::new(&[BAD_DISTILL, BAD_DISTILL]));

    let error = run_distill_with_provider(&options, provider, None, "我实现了 parse 的核心循环")
        .await
        .expect_err("an invalid report is rejected");
    assert!(
        error.to_string().contains("does not satisfy the contract"),
        "{error}"
    );

    let after = fs::read_to_string(workspace.join(".scratch/plans/1000-plan.md"))
        .expect("artifact is readable");
    assert_eq!(
        before, after,
        "a rejected report must not change the artifact"
    );
    assert!(after.contains("status: paused"), "{after}");

    let _ = fs::remove_dir_all(&workspace);
}

const FULL_PLAN: &str = "\
---
kind: plan
created_at_unix: 1000
model: fake
workspace: test
status: paused
phases: Explore, Navigate
validation: ok
---

# Plan: add a rule

## Agent output

## 证据包
- notes.md 存在，尚无规则小节

## 认知任务分配表
| 任务 | 认知属性 | 判定依据 | 执行状态 | Agent 行为 | 用户角色 | 最小深度链路 | 验收 |
|---|---|---|---|---|---|---|---|
| 写入规则 | 已知域 | notes.md 的现有标题结构 | 可执行 | 自主执行 | 审查者 | 编辑 notes.md | 文件包含规则 |
| 确认措辞 | 半已知域 | 仓库没有既定措辞 | 待决策 | 给出选项 | 决策者 | 回答后写入 | 一条规则 |

## 半已知域问题
| 任务 | 问题 | 选项 | 推荐 | 推荐理由 | 判定标准 |
|---|---|---|---|---|---|
| 确认措辞 | 规则写在新小节还是追加到末尾？ | 新建小节 / 追加到末尾 | 新建小节 | 可见度高 | 必须指定唯一落点 |
";

const FULL_ANSWER: &str = "\
## 回答判定
| 问题 | 结论 | 缺口 |
|---|---|---|
| q-1 | sufficient | |

## 决策记录
| 任务 | 用户决策 | 新认知属性 | 新执行状态 |
|---|---|---|---|
| 确认措辞 | 新建小节 | 已知域 | 可执行 |
";

const EXECUTION_REPORT: &str = "\
## Execution report
| 任务 | 结果 | 证据 |
|---|---|---|
| 写入规则 | done | notes.md |
| 确认措辞 | done | notes.md |
";

#[tokio::test]
async fn plan_answer_approve_execute_completes_with_file_change() {
    let workspace = temp_workspace();
    seed_cargo_project(&workspace);
    fs::write(workspace.join("notes.md"), "# Notes\n").expect("baseline notes");
    let options = options(&workspace);

    let provider: Arc<dyn ModelProvider> = Arc::new(FakeProvider::with_attempts(vec![
        text_response(FULL_PLAN),
        text_response(FULL_ANSWER),
        write_call_response(
            "notes.md",
            "# Notes\n\n## 独立判断\n- 不迎合用户，先给证据。\n",
        ),
        text_response(EXECUTION_REPORT),
    ]));

    let plan = run_plan_with_provider(&options, Arc::clone(&provider), "给 notes.md 加一条规则")
        .await
        .expect("plan succeeds");
    assert!(
        plan.output.contains("options:") && plan.output.contains("criteria:"),
        "the plan must print the full question: {}",
        plan.output
    );

    let answered = run_answer_with_provider(&options, Arc::clone(&provider), None, "新建小节")
        .await
        .expect("answer succeeds");
    assert!(
        answered.output.contains("next: /approve"),
        "answer must print the next step: {}",
        answered.output
    );

    run_approve(&PlanConfig::new(workspace.clone()))
        .await
        .expect("approval succeeds");
    let workflow_id = WorkflowStore::new(&workspace)
        .require_active()
        .expect("active workflow")
        .workflow_id;

    let executed = run_execute_with_provider(&options, Arc::clone(&provider), None)
        .await
        .expect("execute implements the tasks and runs the contract");
    assert!(
        executed.output.contains("### 报告"),
        "execute must report the implementation: {}",
        executed.output
    );

    let state = WorkflowStore::new(&workspace)
        .load(&workflow_id)
        .expect("workflow state");
    assert_eq!(state.phase, Phase::Done, "{}", executed.output);
    assert_eq!(state.status, WorkflowStatus::Completed);
    assert!(
        state.execution.is_some_and(|execution| execution.reported),
        "execution must be recorded"
    );

    let notes = fs::read_to_string(workspace.join("notes.md")).expect("notes readable");
    assert!(notes.contains("## 独立判断"), "{notes}");
    assert!(notes.contains("先给证据"), "{notes}");

    let artifact = fs::read_to_string(executed.path).expect("artifact readable");
    assert!(artifact.contains("## Execution"), "{artifact}");

    let _ = fs::remove_dir_all(&workspace);
}
