//! A cancelled turn returns promptly and leaves the session idle.

use std::{
    collections::VecDeque,
    fs,
    path::PathBuf,
    sync::{
        Arc, Mutex,
        atomic::{AtomicU64, Ordering},
    },
    time::{Duration, SystemTime, UNIX_EPOCH},
};

use futures_util::stream;
use serde_json::json;
use slowcode_kernel::{
    model::{
        ContentBlock, LlmCallConfig, LlmCallConfigAdapterDefaults, Message, MessageSource,
        NonNegativeSafeInteger, StreamChunk,
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

struct PendingProvider {
    streams: Mutex<VecDeque<ProviderStream>>,
}

impl PendingProvider {
    fn new() -> Self {
        Self {
            streams: Mutex::new(VecDeque::from([Box::pin(stream::pending::<
                Result<StreamChunk, ProviderStreamError>,
            >()) as ProviderStream])),
        }
    }
}

impl ModelProvider for PendingProvider {
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
        self.streams
            .lock()
            .unwrap()
            .pop_front()
            .expect("one pending stream")
    }
}

fn user(text: &str) -> Message {
    Message::user(
        "user-1",
        vec![ContentBlock::text(text).unwrap()],
        MessageSource::user().unwrap(),
    )
    .unwrap()
}

static WORKSPACE_COUNTER: AtomicU64 = AtomicU64::new(0);

fn temp_workspace() -> PathBuf {
    let timestamp = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("clock is after epoch")
        .as_nanos();
    let nonce = WORKSPACE_COUNTER.fetch_add(1, Ordering::Relaxed);
    let path = std::env::temp_dir().join(format!(
        "slowcode-cancel-{}-{timestamp}-{nonce}",
        std::process::id()
    ));
    fs::create_dir_all(&path).unwrap();
    path
}

#[tokio::test]
async fn cancelling_a_turn_leaves_the_session_idle() {
    let workspace = temp_workspace();
    let config = RoleSessionConfig {
        workspace: workspace.clone(),
        call: LlmCallConfig::new("mock", "mock-model").expect("fake route is valid"),
        read_only: false,
        base_system_prompt: "base system".to_owned(),
    };
    let provider = Arc::new(PendingProvider::new());
    let mut session = RoleSession::new(config, provider, RoleName::Driver)
        .await
        .expect("role session opens");
    let cancel = CancellationToken::new();
    let outcome = {
        let turn = session.run_turn(user("write a very long answer"), cancel.clone());
        tokio::pin!(turn);
        tokio::select! {
            _ = tokio::time::sleep(Duration::from_millis(50)) => cancel.cancel(),
            result = &mut turn => panic!("turn finished before cancel: {result:?}"),
        }
        turn.await.expect("cancelled turn returns")
    };
    assert!(
        matches!(
            outcome.reason(),
            TurnEndReason::Aborted { .. } | TurnEndReason::Interrupted
        ),
        "unexpected reason: {:?}",
        outcome.reason()
    );
    assert!(session.session().state().open_turn().is_none());
    session.shutdown().await.expect("session shuts down");
    let _ = fs::remove_dir_all(workspace);
}
