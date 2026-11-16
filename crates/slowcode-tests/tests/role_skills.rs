//! Role-contributed Skill roots: catalog visibility, on-demand loading, and
//! disappearance of the old role's roots after a switch.

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
    session::TurnEndReason,
};
use slowcode_roles::{RoleName, RoleSession, RoleSessionConfig};
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
        None,
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
        StreamChunk::usage(usage(10)).unwrap(),
        StreamChunk::finish(FinishReason::stop().unwrap(), None).unwrap(),
    ]
}

fn tool_call_response(id: &str, name: &str, arguments: &str) -> Vec<StreamChunk> {
    vec![
        StreamChunk::block_start(0, ContentBlockType::ToolCall).unwrap(),
        StreamChunk::tool_call_delta(0, id, Some(name.to_owned()), arguments).unwrap(),
        StreamChunk::block_end(0, ContentBlock::tool_call(id, name, arguments).unwrap()).unwrap(),
        StreamChunk::usage(usage(10)).unwrap(),
        StreamChunk::finish(FinishReason::tool_calls().unwrap(), None).unwrap(),
    ]
}

fn skill(
    root: &std::path::Path,
    directory: &str,
    name: &str,
    description: &str,
    body: &str,
) -> PathBuf {
    let path = root.join(directory).join("SKILL.md");
    fs::create_dir_all(path.parent().expect("skill parent")).unwrap();
    fs::write(
        &path,
        format!("---\nname: {name}\ndescription: {description}\n---\n{body}\n"),
    )
    .unwrap();
    path
}

fn temp_workspace() -> PathBuf {
    static COUNTER: AtomicU64 = AtomicU64::new(0);
    let nonce = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("clock is after epoch")
        .as_nanos();
    let unique = COUNTER.fetch_add(1, Ordering::Relaxed);
    let path = std::env::temp_dir().join(format!(
        "slowcode-skills-{}-{nonce}-{unique}",
        std::process::id()
    ));
    fs::create_dir_all(&path).expect("temp workspace is created");
    skill(
        &path,
        ".slowcode/skills/shared-guide",
        "shared-guide",
        "Shared instructions.",
        "Shared body.",
    );
    skill(
        &path,
        ".agents/skills/agents-guide",
        "agents-guide",
        "Compatibility instructions.",
        "Compatibility body.",
    );
    skill(
        &path,
        ".slowcode/skills/overlap",
        "overlap",
        "Shared overlap description.",
        "Shared overlap body.",
    );
    skill(
        &path,
        ".slowcode/roles/navigator/skills/overlap",
        "overlap",
        "Navigator overlap description.",
        "Navigator overlap body.",
    );
    skill(
        &path,
        ".slowcode/roles/navigator/skills/nav-only",
        "nav-only",
        "Navigator only.",
        "Navigator body.",
    );
    skill(
        &path,
        ".slowcode/roles/driver/skills/drive-only",
        "drive-only",
        "Driver only.",
        "Driver body.",
    );
    skill(
        &path,
        ".dsh/skills/legacy",
        "legacy",
        "Legacy root.",
        "Legacy body.",
    );
    path
}

fn latest_catalog(messages: &[String]) -> Option<&str> {
    messages
        .iter()
        .rev()
        .find(|text| text.contains("<available_skills>"))
        .map(String::as_str)
}

#[tokio::test]
async fn role_skill_roots_are_visible_loadable_and_disposed_on_switch() {
    let workspace = temp_workspace();
    let config = RoleSessionConfig {
        workspace: workspace.clone(),
        call: LlmCallConfig::new("mock", "mock-model").expect("fake route is valid"),
        read_only: false,
        base_system_prompt: "base system".to_owned(),
    };
    let provider = Arc::new(FakeProvider::new(vec![
        text_response("navigator-catalog"),
        tool_call_response("call-skill-1", "skill", r#"{"name":"nav-only"}"#),
        text_response("navigator-loaded"),
        text_response("driver-catalog"),
        tool_call_response("call-skill-2", "skill", r#"{"name":"drive-only"}"#),
        text_response("driver-loaded"),
        tool_call_response("call-skill-3", "skill", r#"{"name":"nav-only"}"#),
        text_response("driver-blocked"),
        text_response("navigator-back"),
    ]));

    let mut session = RoleSession::new(config, provider.clone(), RoleName::Navigator)
        .await
        .expect("role session opens");

    let navigator_turn = session
        .run_turn(user("user-1", "navigate"), CancellationToken::new())
        .await
        .expect("navigator turn completes");
    assert_eq!(navigator_turn.reason(), &TurnEndReason::Completed);

    {
        let seen = provider.seen.lock().unwrap();
        let request = &seen[0];
        assert!(
            request.tools.iter().any(|tool| tool == "skill"),
            "navigator must expose the skill tool: {:?}",
            request.tools
        );
        let catalog = latest_catalog(&request.messages).expect("navigator catalog is present");
        assert!(catalog.contains("shared-guide"));
        assert!(catalog.contains("agents-guide"));
        assert!(catalog.contains("nav-only"));
        assert!(catalog.contains("Navigator overlap description."));
        assert!(!catalog.contains("Shared overlap description."));
        assert!(!catalog.contains("drive-only"));
        assert!(
            !catalog.contains("legacy"),
            "the removed .dsh/skills root must not be scanned"
        );
    }

    session
        .run_turn(
            user("user-2", "load the navigator skill"),
            CancellationToken::new(),
        )
        .await
        .expect("navigator skill call completes");

    {
        let seen = provider.seen.lock().unwrap();
        assert!(
            seen[2]
                .messages
                .iter()
                .any(|text| text.contains("Navigator body.")),
            "the skill body must reach the model after the tool call: {:?}",
            seen[2].messages
        );
    }

    session
        .switch(RoleName::Driver)
        .await
        .expect("switch succeeds");
    let driver_turn = session
        .run_turn(user("user-3", "drive"), CancellationToken::new())
        .await
        .expect("driver turn completes");
    assert_eq!(driver_turn.reason(), &TurnEndReason::Completed);

    {
        let seen = provider.seen.lock().unwrap();
        let request = &seen[3];
        assert!(request.tools.iter().any(|tool| tool == "skill"));
        let catalog = latest_catalog(&request.messages).expect("driver catalog is present");
        assert!(catalog.contains("shared-guide"));
        assert!(catalog.contains("agents-guide"));
        assert!(catalog.contains("drive-only"));
        assert!(catalog.contains("Shared overlap description."));
        assert!(!catalog.contains("Navigator overlap description."));
        assert!(
            !catalog.contains("nav-only"),
            "the previous role's catalog must disappear: {catalog}"
        );
    }

    session
        .run_turn(
            user("user-4", "load the driver skill"),
            CancellationToken::new(),
        )
        .await
        .expect("driver skill call completes");

    {
        let seen = provider.seen.lock().unwrap();
        assert!(
            seen[5]
                .messages
                .iter()
                .any(|text| text.contains("Driver body.")),
            "the driver role root body must reach the model: {:?}",
            seen[5].messages
        );
    }

    session
        .run_turn(
            user("user-5", "load the navigator skill again"),
            CancellationToken::new(),
        )
        .await
        .expect("driver skill call settles");

    {
        let seen = provider.seen.lock().unwrap();
        assert!(
            seen[7]
                .messages
                .iter()
                .any(|text| text.contains("unknown or no longer available")),
            "the previous role's skill body must be unavailable: {:?}",
            seen[7].messages
        );
    }

    session
        .switch(RoleName::Navigator)
        .await
        .expect("switch back succeeds");
    session
        .run_turn(user("user-6", "navigate again"), CancellationToken::new())
        .await
        .expect("navigator turn completes again");

    {
        let seen = provider.seen.lock().unwrap();
        let request = &seen[8];
        let catalog = latest_catalog(&request.messages).expect("navigator catalog returns");
        assert!(catalog.contains("nav-only"));
        assert!(!catalog.contains("drive-only"));
        assert!(catalog.contains("shared-guide"));
        assert!(catalog.contains("agents-guide"));
    }

    session.shutdown().await.expect("session shuts down");
    let _ = fs::remove_dir_all(&workspace);
}
