//! Skills installed through `slowcode-skills` must appear in the role catalog,
//! load by their installed name, and follow the role switch.

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
use slowcode_skills::{InstallOptions, SkillInstaller, SkillScope};
use tokio_util::sync::CancellationToken;

struct FakeProvider {
    attempts: Mutex<VecDeque<Vec<Result<StreamChunk, ProviderStreamError>>>>,
    seen: Mutex<Vec<SeenRequest>>,
}

#[derive(Clone, Debug)]
struct SeenRequest {
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

fn temp_workspace() -> PathBuf {
    static COUNTER: AtomicU64 = AtomicU64::new(0);
    let nonce = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("clock is after epoch")
        .as_nanos();
    let unique = COUNTER.fetch_add(1, Ordering::Relaxed);
    let path = std::env::temp_dir().join(format!(
        "slowcode-installed-skills-{}-{nonce}-{unique}",
        std::process::id()
    ));
    fs::create_dir_all(&path).expect("temp workspace is created");
    path
}

fn source_skill(workspace: &Path, directory: &str, name: &str, body: &str) -> PathBuf {
    let path = workspace.join("sources").join(directory);
    fs::create_dir_all(&path).expect("source skill directory");
    fs::write(
        path.join("SKILL.md"),
        format!("---\nname: {name}\ndescription: Test instructions.\n---\n{body}\n"),
    )
    .expect("source skill file");
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
async fn installed_skills_reach_the_catalog_and_follow_role_switches() {
    let workspace = temp_workspace();
    let installer = SkillInstaller::new(&workspace).with_home(&workspace);
    let shared = source_skill(&workspace, "shared", "shared-guide", "Shared body.");
    let renamed_source = source_skill(&workspace, "rename", "plain-name", "Renamed body.");
    let copy_source = source_skill(&workspace, "copy", "shared-guide", "Copy body.");
    let nav = source_skill(&workspace, "nav", "nav-only", "Navigator body.");
    let drive = source_skill(&workspace, "drive", "drive-only", "Driver body.");

    installer
        .install(&shared, &SkillScope::Project, &InstallOptions::default())
        .expect("shared skill installs");
    installer
        .install(
            &renamed_source,
            &SkillScope::Project,
            &InstallOptions {
                name: Some("renamed-guide".to_owned()),
                ..InstallOptions::default()
            },
        )
        .expect("renamed skill installs");
    installer
        .install(
            &copy_source,
            &SkillScope::Project,
            &InstallOptions {
                name: Some("shared-copy".to_owned()),
                ..InstallOptions::default()
            },
        )
        .expect("a duplicate frontmatter name installs under a new name");
    installer
        .install(
            &nav,
            &SkillScope::Role(RoleName::Navigator),
            &InstallOptions::default(),
        )
        .expect("navigator skill installs");
    installer
        .install(
            &drive,
            &SkillScope::Role(RoleName::Driver),
            &InstallOptions::default(),
        )
        .expect("driver skill installs");

    let lock = fs::read_to_string(workspace.join(".slowcode/skills-lock.json")).expect("lock file");
    for key in [
        "\"project:shared-guide\"",
        "\"project:renamed-guide\"",
        "\"project:shared-copy\"",
        "\"role:navigator:nav-only\"",
        "\"role:driver:drive-only\"",
    ] {
        assert!(lock.contains(key), "lock must record {key}: {lock}");
    }
    assert!(
        !lock.contains("plain-name"),
        "the lock must use the installed name: {lock}"
    );
    let renamed = fs::read_to_string(workspace.join(".slowcode/skills/renamed-guide/SKILL.md"))
        .expect("renamed skill file");
    assert!(renamed.contains("name: renamed-guide"));
    assert!(!renamed.contains("name: plain-name"));
    let copy = fs::read_to_string(workspace.join(".slowcode/skills/shared-copy/SKILL.md"))
        .expect("copied skill file");
    assert!(copy.contains("name: shared-copy"));
    assert!(!copy.contains("name: shared-guide"));
    assert!(
        workspace
            .join(".slowcode/roles/navigator/skills/nav-only/SKILL.md")
            .is_file()
    );
    assert!(
        workspace
            .join(".slowcode/roles/driver/skills/drive-only/SKILL.md")
            .is_file()
    );

    let config = RoleSessionConfig {
        workspace: workspace.clone(),
        call: LlmCallConfig::new("mock", "mock-model").expect("fake route is valid"),
        read_only: false,
        base_system_prompt: "base system".to_owned(),
    };
    let provider = Arc::new(FakeProvider::new(vec![
        text_response("navigator-catalog"),
        tool_call_response("call-skill-1", "skill", r#"{"name":"renamed-guide"}"#),
        text_response("navigator-loaded"),
        tool_call_response("call-skill-2", "skill", r#"{"name":"shared-copy"}"#),
        text_response("copy-loaded"),
        text_response("driver-catalog"),
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
        let catalog = latest_catalog(&seen[0].messages).expect("navigator catalog is present");
        assert!(catalog.contains("shared-guide"));
        assert!(catalog.contains("shared-copy"));
        assert!(catalog.contains("renamed-guide"));
        assert!(catalog.contains("nav-only"));
        assert!(
            !catalog.contains("plain-name"),
            "the installed file must use the renamed frontmatter: {catalog}"
        );
        assert!(!catalog.contains("drive-only"));
    }

    session
        .run_turn(
            user("user-2", "load the renamed skill"),
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
                .any(|text| text.contains("Renamed body.")),
            "the installed body must reach the model: {:?}",
            seen[2].messages
        );
    }

    session
        .run_turn(
            user("user-3", "load the renamed copy"),
            CancellationToken::new(),
        )
        .await
        .expect("duplicate-name skill call completes");

    {
        let seen = provider.seen.lock().unwrap();
        assert!(
            seen[4]
                .messages
                .iter()
                .any(|text| text.contains("Copy body.")),
            "the renamed copy body must reach the model: {:?}",
            seen[4].messages
        );
    }

    session
        .switch(RoleName::Driver)
        .await
        .expect("switch succeeds");
    let driver_turn = session
        .run_turn(user("user-4", "drive"), CancellationToken::new())
        .await
        .expect("driver turn completes");
    assert_eq!(driver_turn.reason(), &TurnEndReason::Completed);

    {
        let seen = provider.seen.lock().unwrap();
        let catalog = latest_catalog(&seen[5].messages).expect("driver catalog is present");
        assert!(catalog.contains("shared-guide"));
        assert!(catalog.contains("shared-copy"));
        assert!(catalog.contains("renamed-guide"));
        assert!(catalog.contains("drive-only"));
        assert!(
            !catalog.contains("nav-only"),
            "the navigator catalog must disappear after the switch: {catalog}"
        );
    }

    session.shutdown().await.expect("session shuts down");
    let _ = fs::remove_dir_all(&workspace);
}
