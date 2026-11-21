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
fn repl_without_a_key_starts_and_reports_the_credential_hint() {
    let workspace = temp_workspace();
    let output = Command::new(env!("CARGO_BIN_EXE_slowcode"))
        .arg("repl")
        .arg("--workspace")
        .arg(&workspace)
        // Keep dotenvy from walking up into a developer's repo .env.
        .current_dir(&workspace)
        .env_remove("DEEPSEEK_API_KEY")
        .stdin(Stdio::null())
        .output()
        .expect("slowcode runs");

    assert!(
        output.status.success(),
        "stderr:\n{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(stderr.contains("credential: not configured"), "{stderr}");
    assert!(
        stderr.contains("local commands work, model commands will fail"),
        "{stderr}"
    );

    let _ = fs::remove_dir_all(&workspace);
}

#[test]
fn repl_local_commands_work_without_a_key() {
    let workspace = temp_workspace();
    let mut child = Command::new(env!("CARGO_BIN_EXE_slowcode"))
        .arg("repl")
        .arg("--workspace")
        .arg(&workspace)
        .current_dir(&workspace)
        .env_remove("DEEPSEEK_API_KEY")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("slowcode binary starts");
    child
        .stdin
        .take()
        .expect("stdin is piped")
        .write_all(b"/help\n/status\n/logout\nexit\n")
        .expect("local commands are written");
    let output = child.wait_with_output().expect("slowcode exits");

    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        output.status.success(),
        "stdout:\n{stdout}\nstderr:\n{stderr}"
    );
    assert!(stdout.contains("REPL commands:"), "{stdout}");
    assert!(stdout.contains("/login"), "{stdout}");
    assert!(
        stdout.contains("logout: no credential is configured"),
        "{stdout}"
    );
    assert!(!stderr.contains("needs a model credential"), "{stderr}");

    let _ = fs::remove_dir_all(&workspace);
}

#[test]
fn repl_model_turn_without_a_key_fails_and_stays_open() {
    let workspace = temp_workspace();
    let mut child = Command::new(env!("CARGO_BIN_EXE_slowcode"))
        .arg("repl")
        .arg("--workspace")
        .arg(&workspace)
        .current_dir(&workspace)
        .env_remove("DEEPSEEK_API_KEY")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("slowcode binary starts");
    child
        .stdin
        .take()
        .expect("stdin is piped")
        .write_all(b"hello\nexit\n")
        .expect("prompt is written");
    let output = child.wait_with_output().expect("slowcode exits");

    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        output.status.success(),
        "stdout:\n{stdout}\nstderr:\n{stderr}"
    );
    assert!(
        stderr.contains("a model turn needs a model credential"),
        "{stderr}"
    );
    // The REPL prints a fresh prompt after the refused turn.
    assert_eq!(stdout.matches("slowcode> ").count(), 2, "{stdout}");

    let _ = fs::remove_dir_all(&workspace);
}

#[test]
fn one_shot_local_command_runs_without_a_key() {
    let workspace = temp_workspace();
    let output = Command::new(env!("CARGO_BIN_EXE_slowcode"))
        .arg("run")
        .arg("--workspace")
        .arg(&workspace)
        .arg("/help")
        .current_dir(&workspace)
        .env_remove("DEEPSEEK_API_KEY")
        .output()
        .expect("slowcode runs");

    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        output.status.success(),
        "stdout:\n{stdout}\nstderr:\n{stderr}"
    );
    assert!(
        stdout.contains("slowcode - linear coding agent"),
        "{stdout}"
    );

    let _ = fs::remove_dir_all(&workspace);
}

#[test]
fn one_shot_model_prompt_without_a_key_fails_fast() {
    let workspace = temp_workspace();
    let output = Command::new(env!("CARGO_BIN_EXE_slowcode"))
        .arg("run")
        .arg("--workspace")
        .arg(&workspace)
        .arg("hello")
        .current_dir(&workspace)
        .env_remove("DEEPSEEK_API_KEY")
        .output()
        .expect("slowcode runs");

    assert_eq!(output.status.code(), Some(2));
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("no DeepSeek API key is configured"),
        "{stderr}"
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
        // The fake key keeps this test hermetic: CI has no workspace .env,
        // and /role never calls the provider.
        .env("DEEPSEEK_API_KEY", "sk-test")
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
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        output.status.success(),
        "stdout:\n{stdout}\nstderr:\n{stderr}"
    );
    assert!(stdout.contains("role: navigator"), "{stdout}");
    assert!(stdout.contains("role: driver"), "{stdout}");
    assert!(stdout.contains("role unchanged: driver"), "{stdout}");

    let _ = fs::remove_dir_all(&workspace);
}
