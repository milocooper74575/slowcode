//! Durable session CLI wiring: create, resume, duplicate id and corrupt logs.
//!
//! The test sets `DSH_SESSION_ROOT` for its own process. Keep every durable
//! case in this single test so parallel tests cannot race on that variable.

use std::{
    collections::VecDeque,
    env, fs,
    path::{Path, PathBuf},
    sync::{Arc, Mutex},
    time::{SystemTime, UNIX_EPOCH},
};

use futures_util::stream;
use serde_json::json;
use slowcode_cli::{CliOptions, run_with_provider};
use slowcode_kernel::{
    model::{
        ContentBlock, ContentBlockKind, ContentBlockType, FinishReason, LlmCallConfig,
        LlmCallConfigAdapterDefaults, Message, NonNegativeSafeInteger, StreamChunk,
    },
    provider::{
        ModelProvider, PreparedProviderCall, PreparedRequestPreflight, ProviderPreflightError,
        ProviderPrepareError, ProviderRequest, ProviderRequestDraft, ProviderStream,
        ProviderStreamError,
    },
};
use tokio_util::sync::CancellationToken;

struct FakeProvider {
    attempts: Mutex<VecDeque<Vec<Result<StreamChunk, ProviderStreamError>>>>,
    seen: Mutex<Vec<Vec<String>>>,
}

impl FakeProvider {
    fn new(texts: Vec<&str>) -> Self {
        Self {
            attempts: Mutex::new(texts.into_iter().map(text_response).collect()),
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
        self.seen
            .lock()
            .unwrap()
            .push(request.messages().iter().map(message_text).collect());
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

fn text_response(text: &str) -> Vec<Result<StreamChunk, ProviderStreamError>> {
    vec![
        Ok(StreamChunk::block_start(0, ContentBlockType::Text).unwrap()),
        Ok(StreamChunk::text_delta(0, text).unwrap()),
        Ok(StreamChunk::block_end(0, ContentBlock::text(text).unwrap()).unwrap()),
        Ok(StreamChunk::finish(FinishReason::stop().unwrap(), None).unwrap()),
    ]
}

fn temp_base(label: &str) -> PathBuf {
    let nonce = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("clock is after epoch")
        .as_nanos();
    let path = std::env::temp_dir().join(format!(
        "slowcode-session-{label}-{}-{nonce}",
        std::process::id()
    ));
    fs::create_dir_all(&path).expect("temp base is created");
    path
}

fn options(workspace: &Path, prompt: &str, session: Option<&str>, resume: bool) -> CliOptions {
    CliOptions {
        workspace: workspace.to_path_buf(),
        model: "fake-model".to_owned(),
        plan: None,
        max_tokens: None,
        context_window: None,
        read_only: false,
        ask: false,
        session: session.map(str::to_owned),
        resume,
        prompt: Some(prompt.to_owned()),
    }
}

#[tokio::test]
async fn durable_session_create_resume_duplicate_and_corrupt() {
    let base = temp_base("roundtrip");
    let root = base.join("store");
    let workspace = base.join("workspace");
    fs::create_dir_all(&workspace).expect("workspace is created");
    // SAFETY: this integration test binary owns its process environment, and
    // every durable case lives in this single test.
    unsafe {
        env::set_var("DSH_SESSION_ROOT", &root);
    }

    let id = "session-11111111-1111-4111-8111-111111111111";
    let provider = Arc::new(FakeProvider::new(vec!["alpha noted"]));
    run_with_provider(
        options(&workspace, "remember alpha", Some(id), false),
        provider,
    )
    .await
    .expect("first run creates the session");

    let journal = root.join(format!("{id}.jsonl"));
    assert!(
        journal.is_file(),
        "journal was written at {}",
        journal.display()
    );
    let original = fs::read_to_string(&journal).expect("journal is readable");
    assert!(original.contains("remember alpha"), "{original}");

    let fake = Arc::new(FakeProvider::new(vec!["beta noted"]));
    let provider: Arc<dyn ModelProvider> = fake.clone();
    run_with_provider(
        options(&workspace, "remember beta", Some(id), true),
        provider,
    )
    .await
    .expect("resume continues the session");
    let resumed_messages = fake
        .seen
        .lock()
        .unwrap()
        .first()
        .cloned()
        .expect("resume called the provider");
    assert!(
        resumed_messages
            .iter()
            .any(|message| message.contains("remember alpha")),
        "{resumed_messages:?}"
    );
    assert!(
        resumed_messages
            .iter()
            .any(|message| message.contains("remember beta")),
        "{resumed_messages:?}"
    );

    let error = run_with_provider(
        options(&workspace, "start again", Some(id), false),
        Arc::new(FakeProvider::new(vec!["unused"])),
    )
    .await
    .expect_err("creating an existing id fails");
    assert!(error.to_string().contains("already exists"), "{error}");

    let corrupt_id = "session-22222222-2222-4222-8222-222222222222";
    run_with_provider(
        options(&workspace, "remember gamma", Some(corrupt_id), false),
        Arc::new(FakeProvider::new(vec!["gamma noted"])),
    )
    .await
    .expect("second session is created");
    let corrupt_journal = root.join(format!("{corrupt_id}.jsonl"));
    let original_corrupt = fs::read(&corrupt_journal).expect("journal is readable");
    let newline = original_corrupt
        .iter()
        .position(|byte| *byte == b'\n')
        .expect("journal has a header line");
    let mut damaged = b"{\"type\":\"not-a-session-header\"}\n".to_vec();
    damaged.extend_from_slice(&original_corrupt[newline + 1..]);
    fs::write(&corrupt_journal, &damaged).expect("damage is written");

    let error = run_with_provider(
        options(&workspace, "continue", Some(corrupt_id), true),
        Arc::new(FakeProvider::new(vec!["unused"])),
    )
    .await
    .expect_err("a corrupt journal is rejected");
    let message = error.to_string();
    assert!(message.contains("CLI_SESSION_CORRUPT"), "{message}");
    assert!(message.contains("left unchanged"), "{message}");
    assert_eq!(
        fs::read(&corrupt_journal).expect("journal is readable"),
        damaged,
        "the corrupt journal must not be rebuilt or truncated"
    );

    let _ = fs::remove_dir_all(&base);
}
