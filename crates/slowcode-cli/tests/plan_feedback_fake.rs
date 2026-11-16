//! Feedback bundle after a durable Plan command.
//!
//! The original feedback symptom was a bundle with only the session header
//! because Plan ran on an ephemeral Session. This test runs a real Plan command
//! through the Runtime, then builds a feedback bundle from the durable journal.

use std::{
    collections::VecDeque,
    env, fs,
    os::unix::fs::PermissionsExt,
    path::{Path, PathBuf},
    sync::{Arc, Mutex},
    time::{SystemTime, UNIX_EPOCH},
};

use futures_util::stream;
use serde_json::json;
use slowcode_app::{Runtime, RuntimeConfig, SessionChoice, plan::PlanCommand};
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
    fn new(text: &str) -> Self {
        Self {
            attempts: Mutex::new(VecDeque::from([text_response(text)])),
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
            .expect("fake provider has one scripted attempt");
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

fn temp_dir(label: &str) -> PathBuf {
    let path = env::temp_dir().join(format!(
        "slowcode-plan-feedback-{label}-{}-{}",
        std::process::id(),
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ));
    fs::create_dir_all(&path).expect("temp dir");
    path
}

fn read_issue(path: &Path) -> String {
    fs::read_to_string(path).expect("issue.md")
}

#[tokio::test]
async fn feedback_bundle_contains_a_durable_plan_turn() {
    let root = temp_dir("bundle");
    let workspace = root.join("workspace");
    let sessions = root.join("sessions");
    let out_dir = root.join("bundle-out");
    fs::create_dir_all(&workspace).expect("workspace");
    fs::create_dir_all(&sessions).expect("sessions");
    fs::set_permissions(&root, fs::Permissions::from_mode(0o700)).expect("private root");
    fs::set_permissions(&sessions, fs::Permissions::from_mode(0o700)).expect("private sessions");
    // SAFETY: this test binary has one test, so no other test mutates this
    // process environment concurrently.
    unsafe {
        env::set_var("DSH_SESSION_ROOT", &sessions);
    }

    let started = Runtime::start_with(
        RuntimeConfig::new(workspace.clone()),
        SessionChoice::New,
        Arc::new(FakeProvider::new(PLAN)),
    )
    .await
    .expect("runtime starts");
    let session_id = started.session_id.clone();
    let mut runtime = started.runtime;

    runtime
        .run_plan_command(
            PlanCommand::Plan("count lines".to_owned()),
            CancellationToken::new(),
        )
        .await
        .expect("plan succeeds");
    runtime.shutdown().await.expect("runtime shuts down");

    slowcode_cli::feedback::run(vec![
        session_id.clone(),
        "--out".to_owned(),
        out_dir.display().to_string(),
    ])
    .expect("feedback bundle is complete");

    let issue = read_issue(&out_dir.join("issue.md"));
    assert!(
        issue.contains("- turns: 1"),
        "the plan turn is counted:\n{issue}"
    );
    assert!(
        issue.contains("- assistant messages: 1"),
        "the plan assistant message is counted:\n{issue}"
    );
    assert!(
        !issue.contains("- events: 1 (seq none..none)"),
        "the journal is no longer just a header:\n{issue}"
    );

    let _ = fs::remove_dir_all(&root);
}
