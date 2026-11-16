//! Role switching keeps the durable session while changing prompt and tools.

use std::{
    collections::VecDeque,
    fs,
    path::PathBuf,
    sync::{
        Arc, Mutex,
        atomic::{AtomicU64, Ordering},
    },
    time::{SystemTime, UNIX_EPOCH},
};

use futures_util::stream;
use serde_json::json;
use slowcode_kernel::{
    agent::{ApprovalFuture, ApprovalProvider, ApprovalRequest},
    model::{
        ContentBlock, ContentBlockKind, ContentBlockType, FinishReason, LlmCallConfig,
        LlmCallConfigAdapterDefaults, Message, MessageSource, NonNegativeSafeInteger, StreamChunk,
        TokenUsage,
    },
    provider::{
        ModelProvider, PreparedProviderCall, PreparedRequestPreflight, ProviderPreflightError,
        ProviderPrepareError, ProviderRequest, ProviderRequestDraft, ProviderStream,
        ProviderStreamError,
    },
    session::{ApprovalOutcome, TurnEndReason},
};
use slowcode_roles::{
    CacheReport, DRIVER_PROMPT, NAVIGATOR_PROMPT, RoleName, RoleSession, RoleSessionConfig,
};
use tokio_util::sync::CancellationToken;

struct FakeProvider {
    attempts: Mutex<VecDeque<Vec<Result<StreamChunk, ProviderStreamError>>>>,
    seen: Mutex<Vec<SeenRequest>>,
}

struct RejectingApproval {
    count: Arc<AtomicU64>,
}

impl ApprovalProvider for RejectingApproval {
    fn request(
        &self,
        _request: ApprovalRequest,
        _cancellation: CancellationToken,
    ) -> ApprovalFuture<'_> {
        self.count.fetch_add(1, Ordering::SeqCst);
        Box::pin(async { Ok(ApprovalOutcome::Rejected) })
    }
}

#[derive(Clone, Debug)]
struct SeenRequest {
    system: Option<String>,
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
            system: request.system().map(str::to_owned),
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

fn usage(input: u64, cache_read: u64) -> TokenUsage {
    TokenUsage::from_parts(
        NonNegativeSafeInteger::new(input).unwrap(),
        NonNegativeSafeInteger::new(10).unwrap(),
        Some(NonNegativeSafeInteger::new(cache_read).unwrap()),
        None,
        None,
    )
    .unwrap()
}

fn text_response(text: &str, usage: TokenUsage) -> Vec<StreamChunk> {
    vec![
        StreamChunk::block_start(0, ContentBlockType::Text).unwrap(),
        StreamChunk::text_delta(0, text).unwrap(),
        StreamChunk::block_end(0, ContentBlock::text(text).unwrap()).unwrap(),
        StreamChunk::usage(usage).unwrap(),
        StreamChunk::finish(FinishReason::stop().unwrap(), None).unwrap(),
    ]
}

fn tool_call_response(id: &str, name: &str, arguments: &str) -> Vec<StreamChunk> {
    tool_call_response_with_usage(id, name, arguments, None)
}

fn tool_call_response_with_usage(
    id: &str,
    name: &str,
    arguments: &str,
    usage: Option<TokenUsage>,
) -> Vec<StreamChunk> {
    let mut chunks = vec![
        StreamChunk::block_start(0, ContentBlockType::ToolCall).unwrap(),
        StreamChunk::tool_call_delta(0, id, Some(name.to_owned()), arguments).unwrap(),
        StreamChunk::block_end(0, ContentBlock::tool_call(id, name, arguments).unwrap()).unwrap(),
    ];
    if let Some(usage) = usage {
        chunks.push(StreamChunk::usage(usage).unwrap());
    }
    chunks.push(StreamChunk::finish(FinishReason::tool_calls().unwrap(), None).unwrap());
    chunks
}

fn temp_workspace() -> PathBuf {
    static COUNTER: AtomicU64 = AtomicU64::new(0);
    let nonce = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("clock is after epoch")
        .as_nanos();
    let unique = COUNTER.fetch_add(1, Ordering::Relaxed);
    let path = std::env::temp_dir().join(format!(
        "slowcode-role-{}-{nonce}-{unique}",
        std::process::id()
    ));
    fs::create_dir_all(&path).expect("temp workspace is created");
    fs::write(path.join("hello.txt"), "hello\n").expect("fixture is written");
    path
}

#[tokio::test]
async fn switching_roles_keeps_the_session_and_changes_prompt_and_tools() {
    let workspace = temp_workspace();
    let config = RoleSessionConfig {
        workspace: workspace.clone(),
        call: LlmCallConfig::new("mock", "mock-model").expect("fake route is valid"),
        read_only: false,
        base_system_prompt: "base system".to_owned(),
    };
    let provider = Arc::new(FakeProvider::new(vec![
        text_response("navigator-ok", usage(100, 0)),
        text_response("driver-ok", usage(50, 150)),
        text_response("navigator-again", usage(20, 220)),
    ]));

    let mut session = RoleSession::new(config, provider.clone(), RoleName::Navigator)
        .await
        .expect("role session opens");
    let session_id = session.session().id().clone();

    let first = session
        .run_turn(user("user-1", "first question"), CancellationToken::new())
        .await
        .expect("navigator turn completes");
    assert_eq!(first.reason(), &TurnEndReason::Completed);
    let first_cache = CacheReport::from_turn(session.session(), first.turn());
    assert_eq!(first_cache.prompt_tokens(), 100);
    assert_eq!(first_cache.cache_read_tokens(), 0);

    assert!(
        session
            .switch(RoleName::Driver)
            .await
            .expect("switch succeeds")
    );
    assert_eq!(
        provider.seen.lock().unwrap().len(),
        1,
        "switching roles must not call the provider"
    );
    assert_eq!(session.session().id(), &session_id);
    assert_eq!(session.role(), RoleName::Driver);

    let second = session
        .run_turn(user("user-2", "second question"), CancellationToken::new())
        .await
        .expect("driver turn completes");
    assert_eq!(second.reason(), &TurnEndReason::Completed);
    let second_cache = CacheReport::from_turn(session.session(), second.turn());
    assert_eq!(second_cache.input_tokens(), 50);
    assert_eq!(second_cache.cache_read_tokens(), 150);
    assert_eq!(second_cache.hit_ratio(), Some(0.75));
    assert!(
        second_cache
            .render()
            .expect("cache report renders")
            .contains("150 read / 200 prompt (75%)")
    );

    assert!(
        session
            .switch(RoleName::Navigator)
            .await
            .expect("switch back")
    );
    assert_eq!(
        provider.seen.lock().unwrap().len(),
        2,
        "switching back must not call the provider"
    );
    assert!(
        !session
            .switch(RoleName::Navigator)
            .await
            .expect("same role is a no-op")
    );
    assert_eq!(session.session().id(), &session_id);
    let third = session
        .run_turn(user("user-3", "third question"), CancellationToken::new())
        .await
        .expect("navigator turn completes again");
    assert_eq!(third.reason(), &TurnEndReason::Completed);

    {
        let seen = provider.seen.lock().unwrap();
        assert_eq!(seen.len(), 3);
        let navigator = &seen[0];
        assert!(
            !navigator
                .messages
                .iter()
                .any(|text| text.contains("<available_skills>")),
            "a workspace without Skills must not inject a catalog: {:?}",
            navigator.messages
        );
        let navigator_system = navigator.system.as_deref().expect("system is present");
        assert!(
            navigator_system == format!("base system\n\n{NAVIGATOR_PROMPT}"),
            "{navigator_system}"
        );
        assert!(
            navigator_system.contains("You are the navigator."),
            "{navigator_system}"
        );
        assert!(navigator.tools.contains(&"read".to_owned()));
        assert!(!navigator.tools.iter().any(|tool| tool == "write"));
        assert!(!navigator.tools.iter().any(|tool| tool == "bash"));
        let mut navigator_tools = navigator.tools.clone();
        navigator_tools.sort();
        assert_eq!(navigator.tools, navigator_tools);

        let driver = &seen[1];
        let driver_system = driver.system.as_deref().expect("system is present");
        assert_eq!(
            driver_system,
            format!("base system\n\n{DRIVER_PROMPT}"),
            "{driver_system}"
        );
        assert!(
            driver_system.contains("You are the driver."),
            "{driver_system}"
        );
        assert!(driver.tools.contains(&"write".to_owned()));
        assert!(driver.tools.contains(&"bash".to_owned()));
        let mut sorted = driver.tools.clone();
        sorted.sort();
        assert_eq!(driver.tools, sorted, "tool catalogue is deterministic");

        assert!(
            driver
                .messages
                .iter()
                .any(|text| text.contains("first question"))
        );
        assert!(
            driver
                .messages
                .iter()
                .any(|text| text.contains("navigator-ok"))
        );
        assert!(
            driver
                .messages
                .iter()
                .any(|text| text.contains("second question"))
        );
        assert!(
            driver.messages.iter().any(|text| text
                .contains("Role transition: the active role is now driver")
                && text.contains("Statements in earlier messages")),
            "the first turn after a switch must explain the tool change: {:?}",
            driver.messages
        );

        let navigator_again = &seen[2];
        let navigator_again_system = navigator_again
            .system
            .as_deref()
            .expect("system is present");
        assert!(
            navigator_again_system.contains("You are the navigator."),
            "{navigator_again_system}"
        );
        assert_eq!(
            navigator_again_system, navigator_system,
            "returning to a role restores the exact stable prefix"
        );
        assert_eq!(
            navigator_again.tools, navigator.tools,
            "returning to a role restores the same deterministic tool catalogue"
        );
        assert!(navigator_again.tools.contains(&"read".to_owned()));
        assert!(!navigator_again.tools.iter().any(|tool| tool == "write"));
        assert!(
            navigator_again
                .messages
                .iter()
                .any(|text| text.contains("driver-ok"))
        );
        assert!(
            navigator_again
                .messages
                .iter()
                .any(|text| text.contains("second question"))
        );
        assert!(
            navigator_again
                .messages
                .iter()
                .any(|text| text.contains("third question"))
        );
        assert!(
            navigator_again.messages.iter().any(|text| text
                .contains("Role transition: the active role is now navigator")
                && text.contains("read-only inspection tools")),
            "switching back must explain the read-only boundary: {:?}",
            navigator_again.messages
        );
    }

    session.shutdown().await.expect("session shuts down");
    let _ = fs::remove_dir_all(&workspace);
}

#[tokio::test]
async fn read_only_config_forces_the_driver_to_read_only_tools() {
    let workspace = temp_workspace();
    let config = RoleSessionConfig {
        workspace: workspace.clone(),
        call: LlmCallConfig::new("mock", "mock-model").expect("fake route is valid"),
        read_only: true,
        base_system_prompt: "base system".to_owned(),
    };
    let provider = Arc::new(FakeProvider::new(vec![text_response(
        "driver-read-only",
        usage(10, 0),
    )]));

    let mut session = RoleSession::new(config, provider.clone(), RoleName::Driver)
        .await
        .expect("role session opens");
    session
        .run_turn(user("user-1", "hello"), CancellationToken::new())
        .await
        .expect("driver turn completes");

    {
        let seen = provider.seen.lock().unwrap();
        let driver = &seen[0];
        assert!(
            driver
                .system
                .as_deref()
                .expect("system is present")
                .contains("You are the driver.")
        );
        assert!(driver.tools.contains(&"read".to_owned()));
        assert!(
            driver.tools.contains(&"skill".to_owned()),
            "read-only mode must keep the Skill loader: {:?}",
            driver.tools
        );
        assert!(!driver.tools.iter().any(|tool| tool == "write"));
        assert!(!driver.tools.iter().any(|tool| tool == "bash"));
    }

    session.shutdown().await.expect("session shuts down");
    let _ = fs::remove_dir_all(&workspace);
}

#[tokio::test]
async fn a_failed_switch_keeps_the_old_role_and_session() {
    let workspace = temp_workspace();
    let config = RoleSessionConfig {
        workspace: workspace.clone(),
        call: LlmCallConfig::new("mock", "mock-model").expect("fake route is valid"),
        read_only: false,
        base_system_prompt: "base system".to_owned(),
    };
    let provider = Arc::new(FakeProvider::new(vec![
        text_response("navigator-before", usage(10, 0)),
        text_response("navigator-after", usage(10, 10)),
    ]));

    let mut session = RoleSession::new(config, provider.clone(), RoleName::Navigator)
        .await
        .expect("role session opens");
    let session_id = session.session().id().clone();
    session
        .run_turn(user("user-1", "before"), CancellationToken::new())
        .await
        .expect("first turn completes");

    // The driver registry opens the workspace fresh, so removing it makes the
    // switch fail before the old agent releases the Session.
    fs::remove_dir_all(&workspace).expect("workspace is removed");
    session
        .switch(RoleName::Driver)
        .await
        .expect_err("switch fails when the workspace disappears");

    assert_eq!(session.role(), RoleName::Navigator);
    assert_eq!(session.session().id(), &session_id);
    session
        .run_turn(user("user-2", "after"), CancellationToken::new())
        .await
        .expect("old role still runs");
    assert_eq!(provider.seen.lock().unwrap().len(), 2);

    session.shutdown().await.expect("session shuts down");
}

#[tokio::test]
async fn tool_calls_and_results_survive_a_role_switch() {
    let workspace = temp_workspace();
    let config = RoleSessionConfig {
        workspace: workspace.clone(),
        call: LlmCallConfig::new("mock", "mock-model").expect("fake route is valid"),
        read_only: false,
        base_system_prompt: "base system".to_owned(),
    };
    let provider = Arc::new(FakeProvider::new(vec![
        tool_call_response_with_usage(
            "call-read-1",
            "read",
            r#"{"file_path":"hello.txt"}"#,
            Some(usage(10, 5)),
        ),
        text_response("navigator-read", usage(30, 0)),
        text_response("driver-after", usage(20, 40)),
    ]));

    let mut session = RoleSession::new(config, provider.clone(), RoleName::Navigator)
        .await
        .expect("role session opens");
    let first = session
        .run_turn(user("user-1", "read the file"), CancellationToken::new())
        .await
        .expect("navigator tool turn completes");
    assert_eq!(first.reason(), &TurnEndReason::Completed);
    let first_cache = CacheReport::from_turn(session.session(), first.turn());
    assert_eq!(first_cache.input_tokens(), 40);
    assert_eq!(first_cache.cache_read_tokens(), 5);
    assert_eq!(first_cache.prompt_tokens(), 45);
    assert_eq!(
        provider.seen.lock().unwrap().len(),
        2,
        "one tool step and one final step"
    );

    session
        .switch(RoleName::Driver)
        .await
        .expect("switch succeeds");
    assert_eq!(
        provider.seen.lock().unwrap().len(),
        2,
        "switching roles must not call the provider"
    );
    session
        .run_turn(
            user("user-2", "what did you read"),
            CancellationToken::new(),
        )
        .await
        .expect("driver turn completes");

    {
        let seen = provider.seen.lock().unwrap();
        assert_eq!(seen.len(), 3);
        let driver = &seen[2];
        assert!(
            driver.messages.iter().any(|text| text.contains("read")),
            "driver request must contain the navigator tool call: {:?}",
            driver.messages
        );
        assert!(
            driver.messages.iter().any(|text| text.contains("hello")),
            "driver request must contain the tool result: {:?}",
            driver.messages
        );
        assert!(
            driver
                .messages
                .iter()
                .any(|text| text.contains("navigator-read"))
        );
        assert!(
            driver
                .messages
                .iter()
                .any(|text| text.contains("what did you read"))
        );
    }

    session.shutdown().await.expect("session shuts down");
    let _ = fs::remove_dir_all(&workspace);
}

#[tokio::test]
async fn navigator_cannot_write_even_if_the_model_asks() {
    let workspace = temp_workspace();
    let config = RoleSessionConfig {
        workspace: workspace.clone(),
        call: LlmCallConfig::new("mock", "mock-model").expect("fake route is valid"),
        read_only: false,
        base_system_prompt: "base system".to_owned(),
    };
    let provider = Arc::new(FakeProvider::new(vec![
        tool_call_response(
            "call-write-1",
            "write",
            r#"{"file_path":"blocked.txt","content":"nope"}"#,
        ),
        text_response("navigator-blocked", usage(10, 0)),
    ]));

    let mut session = RoleSession::new(config, provider.clone(), RoleName::Navigator)
        .await
        .expect("role session opens");
    session
        .run_turn(user("user-1", "write a file"), CancellationToken::new())
        .await
        .expect("turn settles even when the tool is rejected");

    assert!(
        !workspace.join("blocked.txt").exists(),
        "navigator must not write files"
    );
    {
        let seen = provider.seen.lock().unwrap();
        assert!(
            seen[1]
                .messages
                .iter()
                .any(|text| text.contains("blocked.txt")),
            "the rejected call must remain visible in the session: {:?}",
            seen[1].messages
        );
    }

    session.shutdown().await.expect("session shuts down");
    let _ = fs::remove_dir_all(&workspace);
}

#[tokio::test]
async fn driver_can_write_after_switching_from_navigator() {
    let workspace = temp_workspace();
    let config = RoleSessionConfig {
        workspace: workspace.clone(),
        call: LlmCallConfig::new("mock", "mock-model").expect("fake route is valid"),
        read_only: false,
        base_system_prompt: "base system".to_owned(),
    };
    let provider = Arc::new(FakeProvider::new(vec![
        text_response("navigator-ready", usage(10, 0)),
        tool_call_response(
            "call-write-1",
            "write",
            r#"{"file_path":"written.txt","content":"written after switch"}"#,
        ),
        text_response("driver-wrote", usage(10, 10)),
    ]));

    let mut session = RoleSession::new(config, provider.clone(), RoleName::Navigator)
        .await
        .expect("role session opens");
    session
        .run_turn(user("user-1", "navigate"), CancellationToken::new())
        .await
        .expect("navigator turn completes");
    session
        .switch(RoleName::Driver)
        .await
        .expect("switch succeeds");
    session
        .run_turn(user("user-2", "write it"), CancellationToken::new())
        .await
        .expect("driver turn completes");

    let written = workspace.join("written.txt");
    assert_eq!(
        fs::read_to_string(&written).expect("driver wrote the file"),
        "written after switch"
    );
    assert_eq!(provider.seen.lock().unwrap().len(), 3);

    session.shutdown().await.expect("session shuts down");
    let _ = fs::remove_dir_all(&workspace);
}

#[tokio::test]
async fn installed_approval_provider_routes_writes_and_rejects_them() {
    let workspace = temp_workspace();
    let config = RoleSessionConfig {
        workspace: workspace.clone(),
        call: LlmCallConfig::new("mock", "mock-model").expect("fake route is valid"),
        read_only: false,
        base_system_prompt: "base system".to_owned(),
    };
    let provider = Arc::new(FakeProvider::new(vec![
        tool_call_response(
            "call-write-1",
            "write",
            r#"{"file_path":"blocked.txt","content":"should not exist"}"#,
        ),
        text_response("write was rejected", usage(10, 0)),
    ]));
    let count = Arc::new(AtomicU64::new(0));
    let mut session = RoleSession::new(config, provider, RoleName::Driver)
        .await
        .expect("role session opens");
    session.install_approval_provider(Arc::new(RejectingApproval {
        count: Arc::clone(&count),
    }));

    session
        .run_turn(
            user("user-1", "write blocked.txt"),
            CancellationToken::new(),
        )
        .await
        .expect("turn completes");

    assert_eq!(count.load(Ordering::SeqCst), 1);
    assert!(!workspace.join("blocked.txt").exists());
    session.shutdown().await.expect("session shuts down");
    let _ = fs::remove_dir_all(&workspace);
}

#[tokio::test]
async fn driver_regains_write_after_a_navigator_write_attempt() {
    let workspace = temp_workspace();
    let config = RoleSessionConfig {
        workspace: workspace.clone(),
        call: LlmCallConfig::new("mock", "mock-model").expect("fake route is valid"),
        read_only: false,
        base_system_prompt: "base system".to_owned(),
    };
    let provider = Arc::new(FakeProvider::new(vec![
        text_response("driver-ready", usage(10, 0)),
        tool_call_response(
            "call-write-1",
            "write",
            r#"{"file_path":"blocked.txt","content":"navigator must not write"}"#,
        ),
        text_response("navigator-blocked", usage(10, 10)),
        tool_call_response(
            "call-write-2",
            "write",
            r#"{"file_path":"driver.txt","content":"driver wrote"}"#,
        ),
        text_response("driver-wrote", usage(10, 20)),
    ]));

    let mut session = RoleSession::new(config, provider.clone(), RoleName::Driver)
        .await
        .expect("role session opens");
    session
        .run_turn(user("user-1", "start"), CancellationToken::new())
        .await
        .expect("driver turn completes");
    session
        .switch(RoleName::Navigator)
        .await
        .expect("switch to navigator");
    session
        .run_turn(user("user-2", "write it"), CancellationToken::new())
        .await
        .expect("navigator turn settles");
    assert!(!workspace.join("blocked.txt").exists());

    session
        .switch(RoleName::Driver)
        .await
        .expect("switch back to driver");
    session
        .run_turn(user("user-3", "write it now"), CancellationToken::new())
        .await
        .expect("driver turn completes");

    assert_eq!(
        fs::read_to_string(workspace.join("driver.txt")).expect("driver wrote"),
        "driver wrote"
    );
    {
        let seen = provider.seen.lock().unwrap();
        let last = seen.last().expect("driver request");
        assert!(
            last.tools.contains(&"write".to_owned()),
            "driver request must advertise write: {:?}",
            last.tools
        );
    }

    session.shutdown().await.expect("session shuts down");
    let _ = fs::remove_dir_all(&workspace);
}
