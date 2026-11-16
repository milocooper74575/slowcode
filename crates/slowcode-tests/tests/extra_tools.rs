//! Extra tool seams, as used by MCP, are advertised and dispatched.

use std::{
    collections::VecDeque,
    fs,
    path::PathBuf,
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, AtomicU64, Ordering},
    },
    time::{SystemTime, UNIX_EPOCH},
};

use futures_util::stream;
use serde_json::json;
use slowcode_kernel::{
    agent::{ToolExecutionFuture, ToolExecutionRequest, ToolExecutionResult, ToolExecutor},
    model::{
        ContentBlock, ContentBlockKind, ContentBlockType, FinishReason, JsonValue, LlmCallConfig,
        LlmCallConfigAdapterDefaults, Message, MessageSource, NonNegativeSafeInteger, StreamChunk,
        TokenUsage, ToolSchema,
    },
    provider::{
        ModelProvider, PreparedProviderCall, PreparedRequestPreflight, ProviderPreflightError,
        ProviderPrepareError, ProviderRequest, ProviderRequestDraft, ProviderStream,
        ProviderStreamError,
    },
    session::TurnEndReason,
};
use slowcode_roles::{ExtraToolSlot, RoleName, RoleSession, RoleSessionConfig};
use tokio_util::sync::CancellationToken;

struct FakeProvider {
    attempts: Mutex<VecDeque<Vec<Result<StreamChunk, ProviderStreamError>>>>,
    seen: Mutex<Vec<SeenRequest>>,
}

#[derive(Clone, Debug)]
struct SeenRequest {
    tools: Vec<String>,
    messages: Vec<String>,
}

impl FakeProvider {
    fn new(attempts: Vec<Vec<StreamChunk>>) -> Self {
        Self {
            attempts: Mutex::new(
                attempts
                    .into_iter()
                    .map(|attempt| attempt.into_iter().map(Ok).collect())
                    .collect(),
            ),
            seen: Mutex::new(Vec::new()),
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

    fn stream(&self, request: ProviderRequest, _cancel: CancellationToken) -> ProviderStream {
        self.seen.lock().unwrap().push(SeenRequest {
            tools: request
                .tools()
                .iter()
                .map(|tool| tool.name().to_owned())
                .collect(),
            messages: request.messages().iter().map(message_text).collect(),
        });
        let chunks = self
            .attempts
            .lock()
            .unwrap()
            .pop_front()
            .expect("fake provider has one scripted attempt");
        Box::pin(stream::iter(chunks))
    }
}

struct FakeExtraTool {
    calls: Arc<Mutex<Vec<String>>>,
}

impl ToolExecutor for FakeExtraTool {
    fn execute(
        &self,
        request: ToolExecutionRequest,
        _cancellation: CancellationToken,
    ) -> ToolExecutionFuture<'_> {
        self.calls.lock().unwrap().push(request.name().to_owned());
        Box::pin(async move {
            Ok(ToolExecutionResult::success(vec![
                ContentBlock::text("extra-ok").expect("text fits"),
            ])
            .expect("result is valid"))
        })
    }
}

fn message_text(message: &Message) -> String {
    message
        .content()
        .iter()
        .map(|block| match block.kind() {
            ContentBlockKind::Text { text } => text.clone(),
            _ => block.raw().as_value().to_string(),
        })
        .collect::<Vec<_>>()
        .join("\n")
}

fn user(id: &str, text: &str) -> Message {
    Message::user(
        id,
        vec![ContentBlock::text(text).unwrap()],
        MessageSource::user().unwrap(),
    )
    .unwrap()
}

fn usage(input: u64) -> TokenUsage {
    TokenUsage::from_parts(
        NonNegativeSafeInteger::new(input).unwrap(),
        NonNegativeSafeInteger::new(10).unwrap(),
        Some(NonNegativeSafeInteger::new(0).unwrap()),
        None,
        None,
    )
    .unwrap()
}

fn text_response(text: &str) -> Vec<StreamChunk> {
    vec![
        StreamChunk::block_start(0, ContentBlockType::Text).unwrap(),
        StreamChunk::text_delta(0, text).unwrap(),
        StreamChunk::block_end(0, ContentBlock::text(text).unwrap()).unwrap(),
        StreamChunk::usage(usage(100)).unwrap(),
        StreamChunk::finish(FinishReason::stop().unwrap(), None).unwrap(),
    ]
}

fn tool_call_response(name: &str) -> Vec<StreamChunk> {
    vec![
        StreamChunk::block_start(0, ContentBlockType::ToolCall).unwrap(),
        StreamChunk::tool_call_delta(0, "call-1", Some(name.to_owned()), "{}").unwrap(),
        StreamChunk::block_end(0, ContentBlock::tool_call("call-1", name, "{}").unwrap()).unwrap(),
        StreamChunk::finish(FinishReason::tool_calls().unwrap(), None).unwrap(),
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
        "slowcode-extra-tools-{}-{nonce}-{unique}",
        std::process::id()
    ));
    fs::create_dir_all(&path).expect("temp workspace is created");
    path
}

#[tokio::test]
async fn extra_tools_are_advertised_and_dispatched() {
    let workspace = temp_workspace();
    let config = RoleSessionConfig {
        workspace: workspace.clone(),
        call: LlmCallConfig::new("mock", "mock-model").expect("fake route is valid"),
        read_only: false,
        base_system_prompt: "base system".to_owned(),
    };
    let provider = Arc::new(FakeProvider::new(vec![
        tool_call_response("mcp__demo__echo"),
        text_response("tool done"),
    ]));
    let mut session = RoleSession::new(config, provider.clone(), RoleName::Driver)
        .await
        .expect("role session opens");
    let calls = Arc::new(Mutex::new(Vec::new()));
    let schema = ToolSchema::new(
        "mcp__demo__echo",
        "echo from a demo MCP server",
        JsonValue::new(json!({"type": "object"})).unwrap(),
    )
    .unwrap();
    session
        .install_extra_tools(
            ExtraToolSlot::Mcp,
            Arc::new(FakeExtraTool {
                calls: Arc::clone(&calls),
            }),
            vec![schema],
        )
        .await
        .expect("extra tools are installed");

    let outcome = session
        .run_turn(
            user("user-1", "use the extra tool"),
            CancellationToken::new(),
        )
        .await
        .expect("turn completes");
    assert_eq!(outcome.reason(), &TurnEndReason::Completed);
    assert_eq!(
        calls.lock().unwrap().as_slice(),
        ["mcp__demo__echo".to_owned()]
    );

    {
        let seen = provider.seen.lock().unwrap();
        assert!(
            seen.iter()
                .any(|request| request.tools.iter().any(|tool| tool == "mcp__demo__echo"))
        );
        assert!(seen.iter().any(|request| {
            request
                .messages
                .iter()
                .any(|text| text.contains("extra-ok"))
        }));
    }

    session.shutdown().await.expect("session shuts down");
    let _ = fs::remove_dir_all(workspace);
}

/// A stand-in for a shared system executor such as the MCP toolset.
struct SharedExtraTool {
    shutdowns: Arc<AtomicU64>,
    closed: Arc<AtomicBool>,
}

impl ToolExecutor for SharedExtraTool {
    fn execute(
        &self,
        _request: ToolExecutionRequest,
        _cancellation: CancellationToken,
    ) -> ToolExecutionFuture<'_> {
        let closed = Arc::clone(&self.closed);
        Box::pin(async move {
            if closed.load(Ordering::SeqCst) {
                return Err(slowcode_kernel::agent::ToolExecutorError::new(
                    "MCP server is unavailable",
                ));
            }
            Ok(ToolExecutionResult::success(vec![
                ContentBlock::text("extra-ok").expect("text fits"),
            ])
            .expect("result is valid"))
        })
    }

    fn shutdown(&self) -> slowcode_kernel::agent::ToolShutdownFuture<'_> {
        self.shutdowns.fetch_add(1, Ordering::SeqCst);
        self.closed.store(true, Ordering::SeqCst);
        Box::pin(async { Ok(()) })
    }
}

#[tokio::test]
async fn role_switch_keeps_shared_extra_tools_alive() {
    let workspace = temp_workspace();
    let config = RoleSessionConfig {
        workspace: workspace.clone(),
        call: LlmCallConfig::new("mock", "mock-model").expect("fake route is valid"),
        read_only: false,
        base_system_prompt: "base system".to_owned(),
    };
    let provider = Arc::new(FakeProvider::new(vec![
        text_response("driver-ready"),
        text_response("navigator-ready"),
        text_response("driver-again"),
    ]));
    let mut session = RoleSession::new(config, provider, RoleName::Driver)
        .await
        .expect("role session opens");
    let shutdowns = Arc::new(AtomicU64::new(0));
    let closed = Arc::new(AtomicBool::new(false));
    let schema = ToolSchema::new(
        "mcp__demo__echo",
        "echo from a demo MCP server",
        JsonValue::new(json!({"type": "object"})).unwrap(),
    )
    .unwrap();
    session
        .install_extra_tools(
            ExtraToolSlot::Mcp,
            Arc::new(SharedExtraTool {
                shutdowns: Arc::clone(&shutdowns),
                closed: Arc::clone(&closed),
            }),
            vec![schema],
        )
        .await
        .expect("shared extra tools are installed");

    session
        .run_turn(user("user-1", "start"), CancellationToken::new())
        .await
        .expect("driver turn completes");
    session
        .switch(RoleName::Navigator)
        .await
        .expect("switch to navigator");
    session
        .switch(RoleName::Driver)
        .await
        .expect("switch back to driver");
    session
        .run_turn(user("user-2", "continue"), CancellationToken::new())
        .await
        .expect("driver turn completes after switch");

    assert_eq!(
        shutdowns.load(Ordering::SeqCst),
        0,
        "a role switch must not shut down a shared executor"
    );
    assert!(!closed.load(Ordering::SeqCst));

    session.shutdown().await.expect("session shuts down");
    let _ = fs::remove_dir_all(workspace);
}
