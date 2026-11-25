//! Offline integration test for `/plan --new` and the stale-workflow guard.

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
use slowcode_app::workflow::{WorkflowStatus, WorkflowStore};
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
- read src/main.rs

## 认知任务分配表
| 任务 | 认知属性 | 判定依据 | 执行状态 | Agent 行为 | 用户角色 | 最小深度链路 | 验收 |
|---|---|---|---|---|---|---|---|
| 读取输入 | 已知域 | src/main.rs | 可执行 | 自主执行 | 审查者 | read | 打印内容 |
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
            .expect("attempts lock")
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

fn temp_workspace() -> PathBuf {
    static COUNTER: AtomicU64 = AtomicU64::new(0);
    let nonce = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("clock is after epoch")
        .as_nanos();
    let unique = COUNTER.fetch_add(1, Ordering::Relaxed);
    let path = std::env::temp_dir().join(format!(
        "slowcode-plan-new-{}-{nonce}-{unique}",
        std::process::id()
    ));
    fs::create_dir_all(&path).expect("temp workspace is created");
    path
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
async fn plan_new_pauses_the_active_workflow_and_starts_another() {
    let workspace = temp_workspace();
    let store = WorkflowStore::new(&workspace);
    let old = store
        .create_with_tasks("old task", None, Vec::new())
        .expect("active workflow");
    let provider: Arc<dyn ModelProvider> = Arc::new(FakeProvider::new(&[PLAN]));

    let outcome = run_plan_with_provider(&options(&workspace), provider, "--new count lines")
        .await
        .expect("plan --new succeeds");

    assert!(outcome.output.contains("认知任务分配表"));
    let new_id = store.active_id().expect("index").expect("new active");
    assert_ne!(new_id, old.workflow_id);
    assert_eq!(
        store.load(&old.workflow_id).expect("old state").status,
        WorkflowStatus::Paused
    );
    assert_eq!(
        store.load(&new_id).expect("new state").task,
        "count lines",
        "the --new flag is not part of the task"
    );
    let audit = workspace
        .join(".slowcode")
        .join("audit")
        .join(&new_id)
        .join("index.html");
    assert!(audit.is_file(), "missing audit site at {}", audit.display());
    let html = fs::read_to_string(&audit).expect("audit html");
    assert!(html.contains("count lines"), "{html}");
    assert!(html.contains("sources: state"), "{html}");
    let _ = fs::remove_dir_all(&workspace);
}

#[tokio::test]
async fn plan_without_new_reports_the_active_task_and_the_flag() {
    let workspace = temp_workspace();
    let store = WorkflowStore::new(&workspace);
    store
        .create_with_tasks("old task", None, Vec::new())
        .expect("active workflow");

    let error = run_plan_with_provider(
        &options(&workspace),
        Arc::new(NoCallProvider),
        "another task",
    )
    .await
    .expect_err("an active workflow blocks /plan");

    let text = error.to_string();
    assert!(text.contains("old task"), "{text}");
    assert!(text.contains("/plan --new"), "{text}");
    let _ = fs::remove_dir_all(&workspace);
}
