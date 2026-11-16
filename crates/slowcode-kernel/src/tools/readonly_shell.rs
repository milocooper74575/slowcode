//! Strict read-only shell for navigator, coach and auditor profiles.
//!
//! The tool accepts argv, never a shell string. Only a small command allowlist
//! is accepted, paths stay inside the workspace, and git output is forced
//! through no-pager and no-textconv settings.

use std::{
    path::{Component, Path, PathBuf},
    process::Stdio,
    sync::Arc,
    time::{Duration, Instant},
};

use serde_json::json;
use tokio::{
    io::AsyncReadExt,
    process::Command,
    time::{sleep, timeout},
};
use tokio_util::sync::CancellationToken;

use crate::{
    agent::{
        ToolExecutionFuture, ToolExecutionRequest, ToolExecutionResult, ToolExecutor,
        ToolExecutorError,
    },
    model::{ContentBlock, JsonValue, ToolSchema},
    tools::ToolRegistryBuildError,
};

/// Tool name exposed to read-only profiles.
pub const READONLY_SHELL_TOOL_NAME: &str = "readonly_shell";

const MAX_ARGS: usize = 16;
const MAX_ARG_BYTES: usize = 256;
const MAX_OUTPUT_BYTES: usize = 16 * 1024;
const TIMEOUT: Duration = Duration::from_secs(10);

/// A workspace-bound read-only shell.
#[derive(Clone, Debug)]
pub struct ReadonlyShellTool {
    workspace: Arc<PathBuf>,
}

impl ReadonlyShellTool {
    /// Bind the tool to one existing workspace directory.
    pub fn open(workspace: impl AsRef<Path>) -> Result<Self, ToolRegistryBuildError> {
        let path = workspace.as_ref();
        let workspace =
            path.canonicalize()
                .map_err(|source| ToolRegistryBuildError::InvalidWorkspace {
                    path: path.to_owned(),
                    source,
                })?;
        Ok(Self {
            workspace: Arc::new(workspace),
        })
    }

    /// Tool schema for composition into a registry.
    pub fn schema() -> Result<ToolSchema, ToolRegistryBuildError> {
        let schema = json!({
            "type": "object",
            "properties": {
                "argv": {
                    "type": "array",
                    "minItems": 1,
                    "maxItems": MAX_ARGS,
                    "items": {
                        "type": "string",
                        "minLength": 1,
                        "maxLength": MAX_ARG_BYTES
                    },
                    "description": "Program and arguments. Supported: git status, git diff, git log, git show, ls."
                }
            },
            "required": ["argv"],
            "additionalProperties": false
        });
        let schema =
            JsonValue::new(schema).map_err(|source| ToolRegistryBuildError::InvalidSchema {
                tool: READONLY_SHELL_TOOL_NAME,
                source: crate::model::ModelError::Json(source),
            })?;
        ToolSchema::new(
            READONLY_SHELL_TOOL_NAME,
            "Run one read-only inspection command inside the workspace. No pipes, redirects, or write arguments.",
            schema,
        )
        .map_err(|source| ToolRegistryBuildError::InvalidSchema {
            tool: READONLY_SHELL_TOOL_NAME,
            source,
        })
    }
}

impl ToolExecutor for ReadonlyShellTool {
    fn execute(
        &self,
        request: ToolExecutionRequest,
        cancellation: CancellationToken,
    ) -> ToolExecutionFuture<'_> {
        let workspace = Arc::clone(&self.workspace);
        Box::pin(async move {
            if request.name() != READONLY_SHELL_TOOL_NAME {
                return Err(ToolExecutorError::new(
                    "readonly_shell received another tool name",
                ));
            }
            let argv = parse_argv(request.arguments().as_value())?;
            validate_argv(&argv, &workspace)?;
            run(&argv, &workspace, cancellation).await
        })
    }
}

fn parse_argv(value: &serde_json::Value) -> Result<Vec<String>, ToolExecutorError> {
    let args = value
        .get("argv")
        .and_then(serde_json::Value::as_array)
        .ok_or_else(|| ToolExecutorError::new("argv must be a non-empty array"))?;
    if args.is_empty() || args.len() > MAX_ARGS {
        return Err(ToolExecutorError::new("argv length is out of bounds"));
    }
    let mut argv = Vec::with_capacity(args.len());
    for arg in args {
        let arg = arg
            .as_str()
            .ok_or_else(|| ToolExecutorError::new("argv entries must be strings"))?;
        if arg.is_empty()
            || arg.len() > MAX_ARG_BYTES
            || arg.chars().any(|ch| ch == '\0' || ch.is_control())
        {
            return Err(ToolExecutorError::new("argv entry is invalid"));
        }
        argv.push(arg.to_owned());
    }
    Ok(argv)
}

fn validate_argv(argv: &[String], workspace: &Path) -> Result<(), ToolExecutorError> {
    match argv[0].as_str() {
        "ls" => validate_ls(&argv[1..], workspace),
        "git" => validate_git(&argv[1..], workspace),
        other => Err(ToolExecutorError::new(format!(
            "command `{other}` is not in the read-only allowlist"
        ))),
    }
}

fn validate_ls(args: &[String], workspace: &Path) -> Result<(), ToolExecutorError> {
    let mut paths = 0;
    for arg in args {
        match arg.as_str() {
            "-a" | "-l" | "-la" | "-al" | "-1" => {}
            value if value.starts_with('-') => {
                return Err(ToolExecutorError::new(format!(
                    "ls flag `{value}` is not allowed"
                )));
            }
            value => {
                paths += 1;
                if paths > 1 {
                    return Err(ToolExecutorError::new("ls accepts at most one path"));
                }
                validate_relative_path(value, workspace)?;
            }
        }
    }
    Ok(())
}

fn validate_git(args: &[String], workspace: &Path) -> Result<(), ToolExecutorError> {
    let Some(subcommand) = args.first() else {
        return Err(ToolExecutorError::new("git subcommand is required"));
    };
    match subcommand.as_str() {
        "status" => validate_git_status(&args[1..]),
        "diff" => validate_git_diff(&args[1..], workspace),
        "log" => validate_git_log(&args[1..]),
        "show" => validate_git_show(&args[1..], workspace),
        other => Err(ToolExecutorError::new(format!(
            "git subcommand `{other}` is not in the read-only allowlist"
        ))),
    }
}

fn validate_git_status(args: &[String]) -> Result<(), ToolExecutorError> {
    for arg in args {
        if !matches!(
            arg.as_str(),
            "-s" | "--short" | "--porcelain" | "--branch" | "-sb" | "--porcelain=v1"
        ) {
            return Err(ToolExecutorError::new(format!(
                "git status argument `{arg}` is not allowed"
            )));
        }
    }
    Ok(())
}

fn validate_git_diff(args: &[String], workspace: &Path) -> Result<(), ToolExecutorError> {
    let mut after_separator = false;
    for arg in args {
        if after_separator {
            validate_relative_path(arg, workspace)?;
            continue;
        }
        match arg.as_str() {
            "--" => after_separator = true,
            "--stat" | "--name-only" | "--name-status" | "--cached" | "--staged" | "--no-color" => {
            }
            value if value.starts_with('-') => {
                return Err(ToolExecutorError::new(format!(
                    "git diff argument `{value}` is not allowed"
                )));
            }
            value => validate_relative_path(value, workspace)?,
        }
    }
    Ok(())
}

fn validate_git_log(args: &[String]) -> Result<(), ToolExecutorError> {
    let mut index = 0;
    while index < args.len() {
        match args[index].as_str() {
            "--oneline" | "--stat" | "--no-color" => index += 1,
            "-n" => {
                let Some(value) = args.get(index + 1) else {
                    return Err(ToolExecutorError::new("git log -n needs a count"));
                };
                validate_count(value)?;
                index += 2;
            }
            value if value.starts_with("--max-count=") => {
                validate_count(&value["--max-count=".len()..])?;
                index += 1;
            }
            value => {
                return Err(ToolExecutorError::new(format!(
                    "git log argument `{value}` is not allowed"
                )));
            }
        }
    }
    Ok(())
}

fn validate_git_show(args: &[String], workspace: &Path) -> Result<(), ToolExecutorError> {
    let mut revisions = 0;
    let mut after_separator = false;
    for arg in args {
        if after_separator {
            validate_relative_path(arg, workspace)?;
            continue;
        }
        match arg.as_str() {
            "--" => after_separator = true,
            "--stat" | "--oneline" | "--no-color" => {}
            value if value.starts_with('-') => {
                return Err(ToolExecutorError::new(format!(
                    "git show argument `{value}` is not allowed"
                )));
            }
            value => {
                revisions += 1;
                if revisions > 1 || !is_safe_revision(value) {
                    return Err(ToolExecutorError::new(format!(
                        "git show revision `{value}` is not allowed"
                    )));
                }
            }
        }
    }
    Ok(())
}

fn validate_count(value: &str) -> Result<(), ToolExecutorError> {
    let count = value
        .parse::<u32>()
        .map_err(|_| ToolExecutorError::new("git log count must be a number"))?;
    if count == 0 || count > 200 {
        return Err(ToolExecutorError::new("git log count is out of bounds"));
    }
    Ok(())
}

fn is_safe_revision(value: &str) -> bool {
    if value == "HEAD" {
        return true;
    }
    let Some(rest) = value.strip_prefix("HEAD") else {
        return false;
    };
    let rest = rest
        .strip_prefix('~')
        .or_else(|| rest.strip_prefix('^'))
        .unwrap_or(rest);
    if rest.is_empty() {
        return value.starts_with("HEAD^");
    }
    rest.chars().all(|ch| ch.is_ascii_digit())
}

fn validate_relative_path(value: &str, workspace: &Path) -> Result<(), ToolExecutorError> {
    if value.is_empty() || value.starts_with('/') || value.starts_with(':') {
        return Err(ToolExecutorError::new("path must be workspace-relative"));
    }
    let path = Path::new(value);
    if path.components().any(|component| {
        matches!(
            component,
            Component::ParentDir | Component::RootDir | Component::Prefix(_)
        )
    }) {
        return Err(ToolExecutorError::new("path cannot escape the workspace"));
    }
    let resolved = workspace.join(path);
    let canonical = resolved.canonicalize().map_err(|_| {
        ToolExecutorError::new(format!("path `{value}` does not exist in the workspace"))
    })?;
    if !canonical.starts_with(workspace) {
        return Err(ToolExecutorError::new("path escapes the workspace"));
    }
    Ok(())
}

async fn run(
    argv: &[String],
    workspace: &Path,
    cancellation: CancellationToken,
) -> Result<ToolExecutionResult, ToolExecutorError> {
    let (program, args) = build_command(argv);
    let started = Instant::now();
    let mut child = Command::new(&program)
        .args(&args)
        .current_dir(workspace)
        .env_clear()
        .env("PATH", std::env::var_os("PATH").unwrap_or_default())
        .env("HOME", workspace)
        .env("TMPDIR", std::env::temp_dir())
        .env("LANG", "C")
        .env("LC_ALL", "C")
        .env("NO_COLOR", "1")
        .env("TERM", "dumb")
        .env("PAGER", "cat")
        .env("GIT_PAGER", "cat")
        .env("GIT_TERMINAL_PROMPT", "0")
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true)
        .spawn()
        .map_err(|error| ToolExecutorError::new(format!("command could not start: {error}")))?;

    let stdout = child
        .stdout
        .take()
        .ok_or_else(|| ToolExecutorError::new("stdout pipe is unavailable"))?;
    let stderr = child
        .stderr
        .take()
        .ok_or_else(|| ToolExecutorError::new("stderr pipe is unavailable"))?;
    let mut out = Vec::new();
    let mut err = Vec::new();
    let read = async {
        let mut stdout = stdout.take(MAX_OUTPUT_BYTES as u64 + 1);
        let mut stderr = stderr.take(MAX_OUTPUT_BYTES as u64 + 1);
        let (out_result, err_result) =
            tokio::join!(stdout.read_to_end(&mut out), stderr.read_to_end(&mut err),);
        out_result?;
        err_result?;
        Ok::<(), std::io::Error>(())
    };
    let status = tokio::select! {
        _ = cancellation.cancelled() => {
            let _ = child.start_kill();
            let _ = child.wait().await;
            return Err(ToolExecutorError::new("read-only shell was cancelled"));
        }
        result = timeout(TIMEOUT, async { read.await?; child.wait().await }) => {
            match result {
                Ok(Ok(status)) => status,
                Ok(Err(error)) => {
                    let _ = child.start_kill();
                    return Err(ToolExecutorError::new(format!("command failed: {error}")));
                }
                Err(_) => {
                    let _ = child.start_kill();
                    let _ = sleep(Duration::from_millis(20)).await;
                    let _ = child.wait().await;
                    return Err(ToolExecutorError::new("command timed out"));
                }
            }
        }
    };
    let duration_ms = started.elapsed().as_millis() as u64;
    let truncated = out.len() > MAX_OUTPUT_BYTES || err.len() > MAX_OUTPUT_BYTES;
    out.truncate(MAX_OUTPUT_BYTES);
    err.truncate(MAX_OUTPUT_BYTES);
    let mut text = String::new();
    text.push_str(&format!("$ {}\n", argv.join(" ")));
    text.push_str(&String::from_utf8_lossy(&out));
    if !err.is_empty() {
        text.push_str("\n[stderr]\n");
        text.push_str(&String::from_utf8_lossy(&err));
    }
    if truncated {
        text.push_str("\n[output truncated]\n");
    }
    text.push_str(&format!("\nexit: {}", status.code().unwrap_or(-1)));
    let content = ContentBlock::text(text)
        .map_err(|error| ToolExecutorError::new(format!("output is not valid text: {error}")))?;
    ToolExecutionResult::new(
        vec![content],
        false,
        None,
        Some(
            JsonValue::new(json!({
                "kind": "shell.readonly",
                "argv": argv,
                "exit_code": status.code(),
                "duration_ms": duration_ms,
                "truncated": truncated,
            }))
            .map_err(|error| ToolExecutorError::new(error.to_string()))?,
        ),
        false,
    )
    .map_err(|error| ToolExecutorError::new(error.to_string()))
}

fn build_command(argv: &[String]) -> (String, Vec<String>) {
    match argv[0].as_str() {
        "git" => {
            let mut args = vec![
                "--no-pager".to_owned(),
                "-c".to_owned(),
                "core.fsmonitor=false".to_owned(),
                "-c".to_owned(),
                "diff.external=".to_owned(),
                "-c".to_owned(),
                "core.pager=cat".to_owned(),
            ];
            let subcommand = argv[1].clone();
            args.push(subcommand.clone());
            if matches!(subcommand.as_str(), "diff" | "log" | "show") {
                args.push("--no-ext-diff".to_owned());
                args.push("--no-textconv".to_owned());
            }
            args.extend(argv[2..].iter().cloned());
            ("git".to_owned(), args)
        }
        program => (program.to_owned(), argv[1..].to_vec()),
    }
}

#[cfg(test)]
mod tests {
    use std::time::{SystemTime, UNIX_EPOCH};
    use std::{
        fs,
        sync::atomic::{AtomicU64, Ordering},
    };

    use crate::{
        agent::ToolDispatchBinding,
        model::{CallId, LlmCallConfig},
    };

    use super::*;

    fn temp_workspace() -> PathBuf {
        static COUNTER: AtomicU64 = AtomicU64::new(0);
        let nonce = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("clock")
            .as_nanos();
        let unique = COUNTER.fetch_add(1, Ordering::Relaxed);
        let path = std::env::temp_dir().join(format!(
            "slowcode-readonly-shell-{}-{nonce}-{unique}",
            std::process::id(),
        ));
        fs::create_dir_all(&path).expect("workspace");
        path
    }

    fn request(argv: &[&str]) -> ToolExecutionRequest {
        let arguments = json!({ "argv": argv });
        let raw = serde_json::to_string(&arguments).expect("raw json");
        ToolExecutionRequest::new(
            CallId::new("readonly-shell-call"),
            READONLY_SHELL_TOOL_NAME.to_owned(),
            raw,
            JsonValue::new(arguments).expect("json"),
            LlmCallConfig::new("test", "test").expect("call"),
            ToolDispatchBinding::new(),
        )
    }

    #[tokio::test]
    async fn lists_the_workspace() {
        let workspace = temp_workspace();
        fs::write(workspace.join("hello.txt"), "hi").expect("file");
        let tool = ReadonlyShellTool::open(&workspace).expect("tool");
        let result = tool
            .execute(request(&["ls", "-a"]), CancellationToken::new())
            .await
            .expect("ls runs");
        assert!(!result.is_error());
        let text = match result.content()[0].kind() {
            crate::model::ContentBlockKind::Text { text } => text,
            _ => panic!("text result"),
        };
        assert!(text.contains("hello.txt"), "{text}");
        assert_eq!(
            result.meta().expect("meta").as_value()["kind"],
            "shell.readonly"
        );
        let _ = fs::remove_dir_all(&workspace);
    }

    #[tokio::test]
    async fn rejects_unknown_programs_and_paths() {
        let workspace = temp_workspace();
        let tool = ReadonlyShellTool::open(&workspace).expect("tool");
        for argv in [
            vec!["bash", "-c", "ls"],
            vec!["ls", "/tmp"],
            vec!["git", "diff", "../escape"],
            vec!["git", "diff", "--output", "out.txt"],
        ] {
            assert!(
                tool.execute(request(&argv), CancellationToken::new())
                    .await
                    .is_err(),
                "{argv:?} should be rejected"
            );
        }
        let _ = fs::remove_dir_all(&workspace);
    }

    #[tokio::test]
    async fn runs_git_status_in_a_repository() {
        let workspace = temp_workspace();
        let status = std::process::Command::new("git")
            .args(["init", "-q"])
            .current_dir(&workspace)
            .status()
            .expect("git init");
        assert!(status.success());
        let tool = ReadonlyShellTool::open(&workspace).expect("tool");
        let result = tool
            .execute(
                request(&["git", "status", "--short"]),
                CancellationToken::new(),
            )
            .await
            .expect("git status runs");
        assert_eq!(result.meta().expect("meta").as_value()["exit_code"], 0);
        let _ = fs::remove_dir_all(&workspace);
    }

    #[tokio::test]
    async fn runs_all_allowed_git_inspection_commands() {
        let workspace = temp_workspace();
        let git = |args: &[&str]| {
            std::process::Command::new("git")
                .args(args)
                .current_dir(&workspace)
                .status()
                .expect("git command")
        };
        assert!(git(&["init", "-q"]).success());
        fs::write(workspace.join("hello.txt"), "hi").expect("file");
        assert!(git(&["add", "hello.txt"]).success());
        assert!(
            git(&[
                "-c",
                "user.email=test@example.com",
                "-c",
                "user.name=Test",
                "commit",
                "-qm",
                "initial",
            ])
            .success()
        );

        let tool = ReadonlyShellTool::open(&workspace).expect("tool");
        for argv in [
            vec!["git", "diff", "--stat"],
            vec!["git", "log", "--oneline"],
            vec!["git", "show", "--stat", "HEAD"],
        ] {
            let result = tool
                .execute(request(&argv), CancellationToken::new())
                .await
                .unwrap_or_else(|error| panic!("{argv:?} failed: {error}"));
            assert_eq!(
                result.meta().expect("meta").as_value()["exit_code"],
                0,
                "{argv:?}"
            );
        }
        let _ = fs::remove_dir_all(&workspace);
    }
}
