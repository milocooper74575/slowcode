//! CLI `/role` wiring uses the role session without calling the provider.

use std::{
    fs,
    io::Write,
    path::{Path, PathBuf},
    process::{Command, Stdio},
    sync::{
        Arc,
        atomic::{AtomicU64, Ordering},
    },
    time::{SystemTime, UNIX_EPOCH},
};

use slowcode_cli::{CliOptions, run_with_provider};
use slowcode_kernel::{
    model::LlmCallConfig,
    provider::{
        ModelProvider, PreparedProviderCall, PreparedRequestPreflight, ProviderPreflightError,
        ProviderPrepareError, ProviderRequest, ProviderRequestDraft, ProviderStream,
    },
};
use tokio_util::sync::CancellationToken;

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

fn temp_workspace() -> PathBuf {
    static COUNTER: AtomicU64 = AtomicU64::new(0);
    let nonce = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("clock is after epoch")
        .as_nanos();
    let unique = COUNTER.fetch_add(1, Ordering::Relaxed);
    let path = std::env::temp_dir().join(format!(
        "slowcode-role-cli-{}-{nonce}-{unique}",
        std::process::id()
    ));
    fs::create_dir_all(&path).expect("temp workspace is created");
    path
}

fn options(workspace: &Path, prompt: &str) -> CliOptions {
    CliOptions {
        workspace: workspace.to_path_buf(),
        model: "fake-model".to_owned(),
        plan: None,
        max_tokens: None,
        context_window: None,
        read_only: false,
        ask: false,
        session: None,
        resume: false,
        prompt: Some(prompt.to_owned()),
    }
}

#[tokio::test]
async fn role_command_switches_without_calling_the_provider() {
    let workspace = temp_workspace();
    run_with_provider(
        options(&workspace, "/role navigator"),
        Arc::new(NoCallProvider),
    )
    .await
    .expect("role command succeeds");

    let error = run_with_provider(
        options(&workspace, "/role explorer"),
        Arc::new(NoCallProvider),
    )
    .await
    .expect_err("unknown role is rejected");
    assert!(
        error.to_string().contains("unknown role: explorer"),
        "{error}"
    );

    let _ = fs::remove_dir_all(&workspace);
}

#[test]
fn repl_role_command_switches_between_turns() {
    let workspace = temp_workspace();
    let mut child = Command::new(env!("CARGO_BIN_EXE_slowcode"))
        .arg("repl")
        .arg("--workspace")
        .arg(&workspace)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("slowcode binary starts");
    child
        .stdin
        .take()
        .expect("stdin is piped")
        .write_all(b"/role navigator\n/role\n/role driver\n/role driver\nexit\n")
        .expect("role commands are written");
    let output = child.wait_with_output().expect("slowcode exits");

    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(output.status.success(), "stdout:\n{stdout}");
    assert!(stdout.contains("role: navigator"), "{stdout}");
    assert!(stdout.contains("role: driver"), "{stdout}");
    assert!(stdout.contains("role unchanged: driver"), "{stdout}");

    let _ = fs::remove_dir_all(&workspace);
}
