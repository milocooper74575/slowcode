//! Offline `/plan` test: a fake provider returns a valid plan, and the CLI
//! persists the artifact under `.scratch/plans/`.

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
use slowcode_cli::{CliOptions, run_plan_with_provider};
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

const PLAN: &str = "\
## 证据包
- 候选 A: 在 CLI 层解析 /plan，只读执行。

## 认知任务分配表
| 任务 | 认知属性 | 判定依据 | 执行状态 | Agent 行为 | 用户角色 | 最小深度链路 | 验收 |
|---|---|---|---|---|---|---|---|
| 解析 /plan | 已知域 | CLI 现有参数解析 | 可执行 | 自主执行 | 审查者 | split_plan | 单元测试通过 |
| 选择暂停存储 | 半已知域 | 仓库有 artifact，但存储策略未定 | 待决策 | 给出 A/B 与推荐 | 决策者 | 决策后实现 | 用户确认 |
| 设计暂停语义 | 未知域 | 仓库没有暂停状态约定 | 待用户实现 | 搭建脚手架 | 创造者 | 暂停状态核心留空 | 人工确认 |

## 半已知域问题
| 任务 | 问题 | 选项 | 推荐 | 推荐理由 | 判定标准 |
|---|---|---|---|---|---|
| 选择暂停存储 | 暂停存储选内存还是文件？ | 内存 / 文件 | 文件 | 需要重启后恢复 | 必须二选一并说明重启行为 |

## 未知域暂停
- 暂停后是否需要持久化计划？
";

struct FakeProvider {
    attempts: Mutex<VecDeque<Vec<Result<StreamChunk, ProviderStreamError>>>>,
}

impl FakeProvider {
    fn new(text: &str) -> Self {
        Self::with_attempts(vec![text_response(text)])
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
            .insert("maxTokens".to_owned(), json!(1_024));
        let config = serde_json::from_value(raw).expect("max tokens produce a valid config");
        Ok(PreparedProviderCall::new(
            config,
            LlmCallConfigAdapterDefaults::default(),
            Some(NonNegativeSafeInteger::new(4_096).expect("context window fits")),
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
            .expect("fake provider has one scripted attempt");
        Box::pin(stream::iter(chunks))
    }
}

fn temp_workspace() -> PathBuf {
    static COUNTER: AtomicU64 = AtomicU64::new(0);
    let nonce = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("clock is after epoch")
        .as_nanos();
    let unique = COUNTER.fetch_add(1, Ordering::Relaxed);
    let path = std::env::temp_dir().join(format!(
        "slowcode-plan-{}-{nonce}-{unique}",
        std::process::id()
    ));
    fs::create_dir_all(&path).expect("temp workspace is created");
    path
}

fn text_response(text: &str) -> Vec<Result<StreamChunk, ProviderStreamError>> {
    vec![
        Ok(StreamChunk::block_start(0, ContentBlockType::Text).unwrap()),
        Ok(StreamChunk::text_delta(0, text).unwrap()),
        Ok(StreamChunk::block_end(0, ContentBlock::text(text).unwrap()).unwrap()),
        Ok(StreamChunk::finish(FinishReason::stop().unwrap(), None).unwrap()),
    ]
}

fn read_call_response(
    file_path: &str,
    index: usize,
) -> Vec<Result<StreamChunk, ProviderStreamError>> {
    let arguments = json!({ "file_path": file_path }).to_string();
    let call_id = format!("call-{index}");
    vec![
        Ok(StreamChunk::block_start(0, ContentBlockType::ToolCall).unwrap()),
        Ok(
            StreamChunk::tool_call_delta(0, call_id.as_str(), Some("read".to_owned()), &arguments)
                .unwrap(),
        ),
        Ok(StreamChunk::block_end(
            0,
            ContentBlock::tool_call(call_id.as_str(), "read", &arguments).unwrap(),
        )
        .unwrap()),
        Ok(StreamChunk::finish(FinishReason::tool_calls().unwrap(), None).unwrap()),
    ]
}

#[tokio::test]
async fn plan_writes_an_artifact_and_pauses() {
    let workspace = temp_workspace();
    let options = CliOptions {
        workspace: workspace.clone(),
        model: "fake-model".to_owned(),
        plan: None,
        max_tokens: None,
        context_window: None,
        read_only: false,
        ask: false,
        session: None,
        resume: false,
        prompt: None,
    };

    run_plan_with_provider(
        &options,
        Arc::new(FakeProvider::new(PLAN)),
        "add /plan command",
    )
    .await
    .expect("plan succeeds");

    let directory = workspace.join(".scratch/plans");
    let entries: Vec<_> = fs::read_dir(&directory)
        .expect("plan directory exists")
        .collect::<Result<_, _>>()
        .expect("plan entries are readable");
    assert_eq!(entries.len(), 1);
    let artifact = fs::read_to_string(entries[0].path()).expect("artifact is readable");
    assert!(artifact.contains("status: paused"), "{artifact}");
    assert!(artifact.contains("认知任务分配表"), "{artifact}");
    assert!(artifact.contains("未知域暂停"), "{artifact}");
    assert!(artifact.contains("add /plan command"), "{artifact}");

    let _ = fs::remove_dir_all(Path::new(&workspace));
}

#[tokio::test]
async fn plan_can_use_more_than_the_default_sixteen_steps() {
    let workspace = temp_workspace();
    let options = CliOptions {
        workspace: workspace.clone(),
        model: "fake-model".to_owned(),
        plan: None,
        max_tokens: None,
        context_window: None,
        read_only: false,
        ask: false,
        session: None,
        resume: false,
        prompt: None,
    };
    let mut attempts = Vec::new();
    for index in 0..17 {
        let name = format!("note-{index}.txt");
        fs::write(workspace.join(&name), format!("note {index}\n")).expect("note is written");
        attempts.push(read_call_response(&name, index));
    }
    attempts.push(text_response(PLAN));

    run_plan_with_provider(
        &options,
        Arc::new(FakeProvider::with_attempts(attempts)),
        "analyze the notes",
    )
    .await
    .expect("plan succeeds past the old step limit");

    let directory = workspace.join(".scratch/plans");
    assert!(directory.is_dir(), "plan artifact is written");
    let _ = fs::remove_dir_all(Path::new(&workspace));
}
