//! End-to-end seam test: a static role contributes a tool and a prompt, and
//! the kernel's agent loop sends both to a fake provider.

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
    agent::{AgentLoop, AgentLoopConfig, ToolExecutor, TurnProposal},
    capability::CapabilitySet,
    model::{
        ContentBlock, ContentBlockKind, ContentBlockType, FinishReason, LlmCallConfig,
        LlmCallConfigAdapterDefaults, Message, MessageSource, NonNegativeSafeInteger, StreamChunk,
    },
    provider::{
        ModelProvider, PreparedProviderCall, PreparedRequestPreflight, ProviderPreflightError,
        ProviderPrepareError, ProviderRequest, ProviderRequestDraft, ProviderStream,
        ProviderStreamError,
    },
    session::{Session, TurnEndReason},
    skill_roots::SkillRoots,
    tools::ReadOnlyToolRegistry,
    workspace_instructions::WorkspaceInstructionRuntime,
};
use slowcode_roles::{echo_tool, navigator};
use tokio_util::sync::CancellationToken;

struct FakeProvider {
    attempts: Mutex<VecDeque<Vec<Result<StreamChunk, ProviderStreamError>>>>,
    seen: Mutex<Vec<SeenRequest>>,
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

fn user(text: &str) -> Message {
    Message::user(
        "user-1",
        vec![ContentBlock::text(text).unwrap()],
        MessageSource::user().unwrap(),
    )
    .unwrap()
}

fn text_response(text: &str) -> Vec<StreamChunk> {
    vec![
        StreamChunk::block_start(0, ContentBlockType::Text).unwrap(),
        StreamChunk::text_delta(0, text).unwrap(),
        StreamChunk::block_end(0, ContentBlock::text(text).unwrap()).unwrap(),
        StreamChunk::finish(FinishReason::stop().unwrap(), None).unwrap(),
    ]
}

#[tokio::test]
async fn capability_provider_reaches_the_agent_loop() {
    let role = navigator().with_tool(echo_tool());
    let capabilities =
        CapabilitySet::from_providers(&[Arc::new(role)]).expect("role capability set merges");

    let provider = Arc::new(FakeProvider::new(vec![text_response("ok")]));
    let call = LlmCallConfig::new("mock", "mock-model").expect("fake route is valid");
    let config = AgentLoopConfig::new(call)
        .with_system(capabilities.system_prompt().expect("navigator prompt"))
        .expect("system prompt fits")
        .with_tools(capabilities.schemas().to_vec())
        .expect("echo schema fits");

    let mut agent = AgentLoop::new(
        Session::new("seam-session").expect("session opens"),
        provider.clone(),
        capabilities.executor(),
        config,
    )
    .expect("agent loop builds");

    let outcome = agent
        .run_turn(
            TurnProposal::Enter(vec![user("hello")]),
            CancellationToken::new(),
        )
        .await
        .expect("turn completes");

    assert_eq!(outcome.reason(), &TurnEndReason::Completed);

    let seen = provider.seen.lock().unwrap();
    assert_eq!(seen.len(), 1);
    assert!(
        seen[0]
            .system
            .as_deref()
            .expect("system prompt reached the provider")
            .contains("You are the navigator.")
    );
    assert_eq!(seen[0].tools, ["echo"]);
}

fn temp_workspace() -> PathBuf {
    static COUNTER: AtomicU64 = AtomicU64::new(0);
    let nonce = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("clock is after epoch")
        .as_nanos();
    let unique = COUNTER.fetch_add(1, Ordering::Relaxed);
    let path = std::env::temp_dir().join(format!(
        "slowcode-agent-skill-{}-{nonce}-{unique}",
        std::process::id(),
    ));
    fs::create_dir_all(&path).expect("temp workspace is created");
    path
}

#[tokio::test]
async fn install_skill_roots_injects_the_role_catalog() {
    let workspace = temp_workspace();
    let skill_dir = workspace.join(".slowcode/roles/navigator/skills/demo");
    fs::create_dir_all(&skill_dir).expect("skill dir is created");
    fs::write(
        skill_dir.join("SKILL.md"),
        "---\nname: demo-skill\ndescription: Installed through the public seam.\n---\nDemo body.\n",
    )
    .expect("skill file is written");

    let roots =
        SkillRoots::new([".slowcode/roles/navigator/skills".into()]).expect("roots are valid");
    let registry = ReadOnlyToolRegistry::open_with_skill_roots(&workspace, &roots)
        .expect("read-only registry opens");
    let provider = Arc::new(FakeProvider::new(vec![text_response("ok")]));
    let call = LlmCallConfig::new("mock", "mock-model").expect("fake route is valid");
    let config = AgentLoopConfig::new(call)
        .with_system("base system")
        .expect("system prompt fits")
        .with_tools(registry.schemas().to_vec())
        .expect("tool schemas fit");

    let mut agent = AgentLoop::new(
        Session::new("skill-seam").expect("session opens"),
        provider.clone(),
        Arc::new(registry),
        config,
    )
    .expect("agent loop builds");
    agent
        .install_skill_roots(&workspace, &roots)
        .expect("skill roots install on the public seam");

    let outcome = agent
        .run_turn(
            TurnProposal::Enter(vec![user("hello")]),
            CancellationToken::new(),
        )
        .await
        .expect("turn completes");
    assert_eq!(outcome.reason(), &TurnEndReason::Completed);

    {
        let seen = provider.seen.lock().unwrap();
        assert!(seen[0].tools.iter().any(|tool| tool == "skill"));
        assert!(
            seen[0]
                .messages
                .iter()
                .any(|text| text.contains("demo-skill")),
            "the catalog must contain the role root skill: {:?}",
            seen[0].messages
        );
    }

    agent.shutdown().await.expect("agent shuts down");
    let _ = fs::remove_dir_all(&workspace);
}

#[tokio::test]
async fn workspace_instruction_runtime_reaches_the_provider() {
    let workspace = temp_workspace();
    fs::write(
        workspace.join("AGENTS.md"),
        "Project convention: always mention the seam.\n",
    )
    .expect("AGENTS.md is written");
    let registry = ReadOnlyToolRegistry::open(&workspace).expect("registry opens");
    let schemas = registry.schemas().to_vec();
    let tools: Arc<dyn ToolExecutor> = Arc::new(registry);
    let provider = Arc::new(FakeProvider::new(vec![text_response("ok")]));
    let call = LlmCallConfig::new("mock", "mock-model").expect("fake route is valid");
    let config = AgentLoopConfig::new(call)
        .with_system("base system")
        .expect("system prompt fits")
        .with_tools(schemas)
        .expect("tool schemas fit");
    let mut agent = AgentLoop::new(
        Session::new("instruction-seam").expect("session opens"),
        provider.clone(),
        tools,
        config,
    )
    .expect("agent loop builds");
    let runtime =
        WorkspaceInstructionRuntime::from_workspace(&workspace).expect("instruction runtime opens");
    agent.install_workspace_instruction_runtime(runtime);

    let outcome = agent
        .run_turn(
            TurnProposal::Enter(vec![user("hello")]),
            CancellationToken::new(),
        )
        .await
        .expect("turn completes");
    assert_eq!(outcome.reason(), &TurnEndReason::Completed);
    let seen = provider.seen.lock().unwrap();
    assert!(
        seen[0]
            .messages
            .iter()
            .any(|message| message.contains("Project convention"))
    );
    let _ = fs::remove_dir_all(workspace);
}
