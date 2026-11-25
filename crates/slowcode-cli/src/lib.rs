//! Linear CLI assembly for the slowcode agent.
//!
//! The CLI deliberately stays small: one workspace, one model, one agent loop,
//! and the read/write/edit/bash tool registry. It does not implement a TUI or
//! token streaming.

pub mod audit;
pub mod feedback;
pub mod update;

use std::{
    error::Error,
    io::{self, BufRead, IsTerminal, Write},
    path::{Path, PathBuf},
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
};

use slowcode_app::{
    CredentialSourceKind, command_requires_model, command_specs, credential_status, remove_api_key,
    save_api_key,
};
use slowcode_kernel::{
    agent::{
        ApprovalFuture, ApprovalProvider, ApprovalProviderError, ApprovalRequest, TurnOutcome,
    },
    model::{ContentBlock, LlmCallConfig, Message, MessageSource},
    provider::{
        ModelProvider,
        deepseek::{DEEPSEEK_PROVIDER, DeepSeekConfig, DeepSeekProvider},
    },
    session::{
        ApprovalOutcome, Session, SessionStore, StoreError, new_session_id, validate_session_id,
    },
};
use slowcode_roles::{CacheReport, RoleName, RoleSession, RoleSessionConfig};
use tokio_util::sync::CancellationToken;

pub const USAGE: &str = "\
slowcode - linear coding agent

Usage:
  slowcode [OPTIONS]                 Start the TUI
  slowcode run [OPTIONS] PROMPT...   Run one headless task
  slowcode repl [OPTIONS]            Start the line-based REPL
  slowcode update [--check]          Check for or install a newer release
  slowcode feedback SESSION-ID       Write an issue-ready feedback bundle
  slowcode audit rebuild [ID|--all]  Rebuild local workflow audit sites
  slowcode [OPTIONS] [PROMPT...]     Legacy headless form

Options:
  -w, --workspace PATH     Workspace root (default: current directory)
  -m, --model MODEL        DeepSeek model id (default: deepseek-flash)
      --plan PATH          Plan artifact for /answer, /scaffold, /execute, /distill
      --max-tokens N       Max output tokens for this call
      --context-window N   Fallback context window for unknown models
      --read-only          Disable file writes and shell commands
      --ask                Ask before file writes and shell commands
      --session ID         Persist this run under a durable session id; new generates one
      --resume             Resume the session named by --session
  -h, --help               Show this help
  -V, --version            Show version

Without PROMPT, slowcode starts the TUI. Use `slowcode repl` for the line-based
REPL; type `exit` or `quit` there to leave.

Commands:
  /help                    List the commands available in the REPL
  /status [--workflow ID]  Show the active workflow phase and next action
  /next                    Show the next question or phase action
  /login                   Save a DeepSeek API key without echoing it
  /logout                  Remove the workspace API key
  /plan [--new] <task>     Explore + Navigate, write a plan artifact, then pause
  /answer <text>           Record decisions and reclassify the plan
  /approve                 Accept the review table and continue
  /workflow [selector]     Show workflow state or set a test selector
  /scaffold                Generate scaffolds for 待用户实现 tasks
  /continue                Check the core implementation and enter test fix
  /execute                 Execute only 可执行 tasks
  /distill <explanation>   Record a Teach-back and produce an understanding-gap report
  /role [navigator|driver] Show or switch the REPL role without losing the session
";

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct CliOptions {
    pub workspace: PathBuf,
    pub model: String,
    pub plan: Option<PathBuf>,
    pub max_tokens: Option<u64>,
    pub context_window: Option<u64>,
    pub read_only: bool,
    pub ask: bool,
    pub session: Option<String>,
    pub resume: bool,
    pub prompt: Option<String>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum Command {
    Run(CliOptions),
    Help,
    Version,
}

pub fn parse_args<I>(args: I) -> Result<Command, String>
where
    I: IntoIterator<Item = String>,
{
    let mut args = args.into_iter().peekable();
    let mut workspace = std::env::current_dir().map_err(|error| error.to_string())?;
    let mut model = "deepseek-flash".to_owned();
    let mut plan = None;
    let mut max_tokens = None;
    let mut context_window = None;
    let mut read_only = false;
    let mut ask = false;
    let mut session = None;
    let mut resume = false;
    let mut positional = Vec::new();

    while let Some(argument) = args.next() {
        let (flag, inline_value) = match argument.split_once('=') {
            Some((flag, value)) => (flag, Some(value.to_owned())),
            None => (argument.as_str(), None),
        };
        let mut value = |name: &str| -> Result<String, String> {
            if let Some(value) = inline_value.clone() {
                return Ok(value);
            }
            args.next()
                .ok_or_else(|| format!("{name} requires a value"))
        };

        match flag {
            "-h" | "--help" => return Ok(Command::Help),
            "-V" | "--version" => return Ok(Command::Version),
            "-w" | "--workspace" => workspace = PathBuf::from(value("--workspace")?),
            "-m" | "--model" => model = value("--model")?,
            "--plan" => plan = Some(PathBuf::from(value("--plan")?)),
            "--max-tokens" => {
                max_tokens = Some(parse_positive("--max-tokens", &value("--max-tokens")?)?)
            }
            "--context-window" => {
                context_window = Some(parse_positive(
                    "--context-window",
                    &value("--context-window")?,
                )?)
            }
            "--read-only" => read_only = true,
            "--ask" => ask = true,
            "--session" => session = Some(value("--session")?),
            "--resume" => resume = true,
            _ if flag.starts_with('-') => return Err(format!("unknown option: {flag}")),
            _ => positional.push(argument),
        }
    }

    if model.is_empty() {
        return Err("--model must not be empty".to_owned());
    }
    if resume && session.is_none() {
        return Err("--resume requires --session <id>".to_owned());
    }
    if let Some(id) = session.as_deref() {
        if id == "new" {
            if resume {
                return Err("--resume cannot be used with --session new".to_owned());
            }
        } else if validate_session_id(id).is_err() {
            return Err(format!(
                "invalid --session id: {id}; expected session-<uuid> or new"
            ));
        }
    }

    Ok(Command::Run(CliOptions {
        workspace,
        model,
        plan,
        max_tokens,
        context_window,
        read_only,
        ask,
        session,
        resume,
        prompt: (!positional.is_empty()).then(|| positional.join(" ")),
    }))
}

fn parse_positive(name: &str, value: &str) -> Result<u64, String> {
    let parsed = value
        .parse::<u64>()
        .map_err(|_| format!("{name} must be a positive integer"))?;
    if parsed == 0 {
        return Err(format!("{name} must be greater than zero"));
    }
    Ok(parsed)
}

pub async fn run(options: CliOptions) -> Result<(), Box<dyn Error>> {
    let provider = build_provider(&options)?;
    run_with_provider(options, Arc::new(provider)).await
}

fn build_provider(options: &CliOptions) -> Result<DeepSeekProvider, Box<dyn Error>> {
    let mut config = DeepSeekConfig::from_process_environment()?;
    config = config.with_models(slowcode_app::known_models())?;
    if let Some(context_window) = options.context_window {
        config = config.with_default_context_window(context_window)?;
    }
    Ok(DeepSeekProvider::new(
        config,
        Arc::new(slowcode_app::WorkspaceCredentials::new(
            options.workspace.clone(),
        )),
    )?)
}

pub async fn run_with_provider(
    options: CliOptions,
    provider: Arc<dyn ModelProvider>,
) -> Result<(), Box<dyn Error>> {
    let approval_signal = ApprovalSignal::default();
    let command = options.prompt.as_deref().and_then(split_command);
    let plan_command = matches!(
        command,
        Some(
            SlashCommand::Plan(_)
                | SlashCommand::Answer(_)
                | SlashCommand::Scaffold
                | SlashCommand::Continue
                | SlashCommand::Test
                | SlashCommand::Fix
                | SlashCommand::Execute
                | SlashCommand::Distill(_)
                | SlashCommand::Approve
                | SlashCommand::Workflow(_)
        )
    );
    if plan_command && options.session.is_some() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "session persistence applies to normal turns and /role; Plan commands keep using the artifact",
        )
        .into());
    }

    match command {
        Some(SlashCommand::Help) => {
            print!("{USAGE}");
            return Ok(());
        }
        Some(SlashCommand::Login) => {
            repl_login(&options.workspace);
            return Ok(());
        }
        Some(SlashCommand::Logout) => {
            repl_logout(&options.workspace);
            return Ok(());
        }
        Some(SlashCommand::Plan(task)) => {
            let outcome = Box::pin(run_plan_with_provider(&options, provider, task)).await?;
            println!("{}", outcome.output);
            println!("\n[plan saved to {}]", outcome.path.display());
            return Ok(());
        }
        Some(SlashCommand::Answer(answer)) => {
            let outcome =
                Box::pin(run_answer_with_provider(&options, provider, None, answer)).await?;
            println!("{}", outcome.output);
            return Ok(());
        }
        Some(SlashCommand::Scaffold) => {
            let outcome = Box::pin(run_scaffold_with_provider(&options, provider, None)).await?;
            println!("{}", outcome.output);
            return Ok(());
        }
        Some(SlashCommand::Continue) => {
            let outcome = Box::pin(run_continue_with_provider(&options, provider, None)).await?;
            println!("{}", outcome.output);
            return Ok(());
        }
        Some(SlashCommand::Test) => {
            let outcome = Box::pin(run_test_with_provider(&options, provider, None)).await?;
            println!("{}", outcome.output);
            return Ok(());
        }
        Some(SlashCommand::Fix) => {
            let outcome = Box::pin(run_fix_with_provider(&options, provider, None)).await?;
            println!("{}", outcome.output);
            return Ok(());
        }
        Some(SlashCommand::Execute) => {
            let outcome = Box::pin(run_execute_with_provider(&options, provider, None)).await?;
            println!("{}", outcome.output);
            return Ok(());
        }
        Some(SlashCommand::Distill(explanation)) => {
            let outcome = Box::pin(run_distill_with_provider(
                &options,
                provider,
                None,
                explanation,
            ))
            .await?;
            println!("{}", outcome.output);
            return Ok(());
        }
        Some(SlashCommand::Approve) => {
            let outcome = Box::pin(run_plan_command(
                slowcode_app::plan::PlanCommand::Approve,
                &options,
                None,
                provider,
            ))
            .await?;
            println!("{}", outcome.output);
            return Ok(());
        }
        Some(SlashCommand::Status(argument)) => {
            let argument = (!argument.is_empty()).then(|| argument.to_owned());
            let outcome = Box::pin(run_plan_command(
                slowcode_app::plan::PlanCommand::Status(argument),
                &options,
                None,
                provider,
            ))
            .await?;
            println!("{}", outcome.output);
            return Ok(());
        }
        Some(SlashCommand::Next) => {
            let outcome = Box::pin(run_plan_command(
                slowcode_app::plan::PlanCommand::Next,
                &options,
                None,
                provider,
            ))
            .await?;
            println!("{}", outcome.output);
            return Ok(());
        }
        Some(SlashCommand::Workflow(argument)) => {
            let argument = (!argument.is_empty()).then(|| argument.to_owned());
            let outcome = Box::pin(run_plan_command(
                slowcode_app::plan::PlanCommand::Workflow(argument),
                &options,
                None,
                provider,
            ))
            .await?;
            println!("{}", outcome.output);
            return Ok(());
        }
        Some(SlashCommand::Role(argument)) => {
            let mut session =
                open_role_session(&options, provider, approval_signal.clone()).await?;
            let result = run_role_command(&mut session, argument).await;
            let shutdown = session.shutdown().await;
            result?;
            shutdown?;
            if approval_signal.is_unavailable() {
                return Err(Box::new(approval_required_error()));
            }
            return Ok(());
        }
        None => {}
    }

    let mut session = open_role_session(&options, provider, approval_signal.clone()).await?;

    if !options.read_only {
        eprintln!(
            "slowcode: writes and shell commands are allowed inside {} (use --read-only to disable)",
            options.workspace.display()
        );
    }

    let result = match options.prompt.as_deref() {
        Some(prompt) => run_prompt(&mut session, prompt, 1).await,
        None => repl(&mut session, &options).await,
    };

    let shutdown = session.shutdown().await;
    result?;
    shutdown?;
    if approval_signal.is_unavailable() {
        return Err(Box::new(approval_required_error()));
    }
    Ok(())
}

fn role_session_config(options: &CliOptions) -> Result<RoleSessionConfig, Box<dyn Error>> {
    let call = LlmCallConfig::new(DEEPSEEK_PROVIDER, &options.model)?;
    let call = slowcode_app::apply_max_tokens(call, options.max_tokens)?;
    Ok(RoleSessionConfig {
        workspace: options.workspace.clone(),
        call,
        read_only: options.read_only,
        base_system_prompt: system_prompt(&options.workspace),
    })
}

async fn open_role_session(
    options: &CliOptions,
    provider: Arc<dyn ModelProvider>,
    approval_signal: ApprovalSignal,
) -> Result<RoleSession, Box<dyn Error>> {
    let config = role_session_config(options)?;
    let mut session = match open_durable_session(options).await? {
        Some(session) => {
            RoleSession::new_with_session(config, provider, RoleName::Driver, session).await?
        }
        None => RoleSession::new(config, provider, RoleName::Driver).await?,
    };
    if options.ask {
        session.install_approval_provider(Arc::new(CliApprovalProvider {
            unavailable: approval_signal,
        }));
    }
    Ok(session)
}

/// Shared flag for non-interactive `--ask` runs.
#[derive(Clone, Default)]
struct ApprovalSignal(Arc<AtomicBool>);

impl ApprovalSignal {
    fn mark_unavailable(&self) {
        self.0.store(true, Ordering::SeqCst);
    }

    fn is_unavailable(&self) -> bool {
        self.0.load(Ordering::SeqCst)
    }
}

fn approval_required_error() -> io::Error {
    io::Error::other("approval_required: --ask needs an interactive terminal")
}

/// Terminal approval used by `--ask` in headless runs and the REPL.
struct CliApprovalProvider {
    unavailable: ApprovalSignal,
}

impl ApprovalProvider for CliApprovalProvider {
    fn request(
        &self,
        request: ApprovalRequest,
        cancellation: CancellationToken,
    ) -> ApprovalFuture<'_> {
        let unavailable = self.unavailable.clone();
        Box::pin(async move {
            if !io::stdin().is_terminal() {
                unavailable.mark_unavailable();
                return Err(ApprovalProviderError::new(
                    "approval_required: --ask needs an interactive terminal",
                ));
            }
            let mut prompt = format!("[approval] {}", request.tool_name());
            if let Some(reason) = request.reason() {
                prompt.push_str(&format!("\nreason: {reason}"));
            }
            prompt.push_str("\n--- preview ---\n");
            let preview = slowcode_app::escape_terminal(request.preview());
            for line in preview.lines().take(40) {
                prompt.push_str(line);
                prompt.push('\n');
            }
            prompt.push_str("[y] allow once  [n] reject  [c] cancel > ");
            eprint!("{prompt}");
            let _ = io::stderr().flush();

            let read = tokio::task::spawn_blocking(|| {
                let mut line = String::new();
                io::stdin().read_line(&mut line).map(|_| line)
            });
            let answer = tokio::select! {
                _ = cancellation.cancelled() => return Ok(ApprovalOutcome::Cancelled),
                result = read => result
                    .map_err(|_| ApprovalProviderError::new("approval prompt failed"))?
                    .map_err(|_| ApprovalProviderError::new("approval prompt failed"))?,
            };
            Ok(match answer.trim().to_ascii_lowercase().as_str() {
                "y" | "yes" | "a" | "allow" => ApprovalOutcome::AllowedOnce,
                "c" | "cancel" => ApprovalOutcome::Cancelled,
                _ => ApprovalOutcome::Rejected,
            })
        })
    }
}

async fn open_durable_session(options: &CliOptions) -> Result<Option<Session>, Box<dyn Error>> {
    let Some(value) = options.session.as_deref() else {
        return Ok(None);
    };
    let store = SessionStore::open_default()
        .map_err(|error| session_error("open the store for", value, error))?;

    if options.resume {
        let id =
            validate_session_id(value).map_err(|error| session_error("resume", value, error))?;
        let session = store
            .resume_session(id, &options.workspace)
            .await
            .map_err(|error| session_error("resume", value, error))?;
        eprintln!("[session: {value}]");
        return Ok(Some(session));
    }

    let id = if value == "new" {
        new_session_id()
    } else {
        validate_session_id(value).map_err(|error| session_error("create", value, error))?
    };
    let session = store
        .open_new_session(id.clone(), &options.workspace)
        .await
        .map_err(|error| session_error("create", id.as_str(), error))?;
    eprintln!("[session: {id}]");
    Ok(Some(session))
}

fn session_error(action: &str, id: &str, error: StoreError) -> io::Error {
    let hint = match error {
        StoreError::Busy => "the session already exists; use --resume or another id",
        StoreError::NotFound => "the session does not exist; create it without --resume",
        StoreError::Corrupt => "the journal is corrupt or truncated; it was left unchanged",
        StoreError::WorkspaceMismatch => {
            "the workspace does not match the session header; resume from the original workspace"
        }
        StoreError::InvalidSessionId => "expected session-<uuid> or new",
        _ => return io::Error::other(format!("cannot {action} session {id}: {error}")),
    };
    io::Error::other(format!("cannot {action} session {id}: {error} ({hint})"))
}

fn system_prompt(workspace: &Path) -> String {
    slowcode_app::base_system_prompt(workspace)
}

async fn run_prompt(
    session: &mut RoleSession,
    prompt: &str,
    turn: u64,
) -> Result<(), Box<dyn Error>> {
    let message = Message::user(
        format!("user-{turn}"),
        vec![ContentBlock::text(prompt)?],
        MessageSource::user()?,
    )?;
    let outcome = session.run_turn(message, CancellationToken::new()).await?;
    let cache = CacheReport::from_turn(session.session(), outcome.turn());
    print_outcome(&outcome, Some(&cache));
    Ok(())
}

async fn run_role_command(session: &mut RoleSession, argument: &str) -> Result<(), Box<dyn Error>> {
    let argument = argument.trim();
    if argument.is_empty() {
        println!("role: {}", session.role());
        return Ok(());
    }
    let role = RoleName::parse(argument).ok_or_else(|| {
        io::Error::new(
            io::ErrorKind::InvalidInput,
            format!("unknown role: {argument}; expected navigator or driver"),
        )
    })?;
    if session.switch(role).await? {
        println!("role: {role}");
    } else {
        println!("role unchanged: {role}");
    }
    Ok(())
}

/// Run one `/plan` turn in a read-only agent and persist the result.
#[derive(Clone, Debug)]
pub struct PlanCommandOutcome {
    pub path: PathBuf,
    pub output: String,
}

/// Run one `/plan` turn in a read-only agent and persist the result.
pub async fn run_plan_with_provider(
    options: &CliOptions,
    provider: Arc<dyn ModelProvider>,
    task: &str,
) -> Result<PlanCommandOutcome, Box<dyn Error>> {
    run_plan_command(
        slowcode_app::plan::PlanCommand::Plan(task.to_owned()),
        options,
        None,
        provider,
    )
    .await
}

/// `/answer` records decisions and reclassifies the plan.
pub async fn run_answer_with_provider(
    options: &CliOptions,
    provider: Arc<dyn ModelProvider>,
    pending: Option<&Path>,
    answer: &str,
) -> Result<PlanCommandOutcome, Box<dyn Error>> {
    run_plan_command(
        slowcode_app::plan::PlanCommand::Answer(answer.to_owned()),
        options,
        pending,
        provider,
    )
    .await
}

/// `/scaffold` writes scaffolds for tasks still marked 待用户实现.
pub async fn run_scaffold_with_provider(
    options: &CliOptions,
    provider: Arc<dyn ModelProvider>,
    pending: Option<&Path>,
) -> Result<PlanCommandOutcome, Box<dyn Error>> {
    run_plan_command(
        slowcode_app::plan::PlanCommand::Scaffold,
        options,
        pending,
        provider,
    )
    .await
}

/// `/continue` checks the user's core implementation against the contract.
pub async fn run_continue_with_provider(
    options: &CliOptions,
    provider: Arc<dyn ModelProvider>,
    pending: Option<&Path>,
) -> Result<PlanCommandOutcome, Box<dyn Error>> {
    run_plan_command(
        slowcode_app::plan::PlanCommand::Continue,
        options,
        pending,
        provider,
    )
    .await
}

/// `/execute` runs only effective 可执行 tasks.
pub async fn run_execute_with_provider(
    options: &CliOptions,
    provider: Arc<dyn ModelProvider>,
    pending: Option<&Path>,
) -> Result<PlanCommandOutcome, Box<dyn Error>> {
    run_plan_command(
        slowcode_app::plan::PlanCommand::Execute,
        options,
        pending,
        provider,
    )
    .await
}

/// `/test` runs the fixed test contract.
pub async fn run_test_with_provider(
    options: &CliOptions,
    provider: Arc<dyn ModelProvider>,
    pending: Option<&Path>,
) -> Result<PlanCommandOutcome, Box<dyn Error>> {
    run_plan_command(
        slowcode_app::plan::PlanCommand::Test,
        options,
        pending,
        provider,
    )
    .await
}

/// `/fix` fixes known failures from the last test run.
pub async fn run_fix_with_provider(
    options: &CliOptions,
    provider: Arc<dyn ModelProvider>,
    pending: Option<&Path>,
) -> Result<PlanCommandOutcome, Box<dyn Error>> {
    run_plan_command(
        slowcode_app::plan::PlanCommand::Fix,
        options,
        pending,
        provider,
    )
    .await
}

/// `/distill` records a Teach-back and returns an understanding-gap report.
pub async fn run_distill_with_provider(
    options: &CliOptions,
    provider: Arc<dyn ModelProvider>,
    pending: Option<&Path>,
    explanation: &str,
) -> Result<PlanCommandOutcome, Box<dyn Error>> {
    run_plan_command(
        slowcode_app::plan::PlanCommand::Distill(explanation.to_owned()),
        options,
        pending,
        provider,
    )
    .await
}

fn plan_config(options: &CliOptions, pending: Option<&Path>) -> slowcode_app::plan::PlanConfig {
    let mut config = slowcode_app::plan::PlanConfig {
        workspace: options.workspace.clone(),
        model: options.model.clone(),
        plan: options.plan.clone(),
        max_tokens: options.max_tokens,
        read_only: options.read_only,
    };
    if let Some(pending) = pending {
        config.plan = Some(pending.to_owned());
    }
    config
}

async fn run_plan_command(
    command: slowcode_app::plan::PlanCommand,
    options: &CliOptions,
    pending: Option<&Path>,
    provider: Arc<dyn ModelProvider>,
) -> Result<PlanCommandOutcome, Box<dyn Error>> {
    run_plan_command_with_optional_session(command, options, pending, provider, None).await
}

async fn run_plan_command_with_optional_session(
    command: slowcode_app::plan::PlanCommand,
    options: &CliOptions,
    pending: Option<&Path>,
    provider: Arc<dyn ModelProvider>,
    session: Option<&mut RoleSession>,
) -> Result<PlanCommandOutcome, Box<dyn Error>> {
    let approval = options.ask.then(|| {
        Arc::new(CliApprovalProvider {
            unavailable: ApprovalSignal::default(),
        }) as Arc<dyn ApprovalProvider>
    });
    let config = plan_config(options, pending);
    let outcome = match session {
        Some(session) => {
            let mut plan_session = Some(session.take_session().await?);
            let result = slowcode_app::plan::run_command_with_session(
                command,
                &config,
                provider,
                approval,
                CancellationToken::new(),
                &mut plan_session,
            )
            .await;
            if let Some(plan_session) = plan_session.take() {
                session.restore_session(plan_session).await?;
            }
            result.map_err(|error| io::Error::other(error.to_string()))?
        }
        None => slowcode_app::plan::run_command(
            command,
            &config,
            provider,
            approval,
            CancellationToken::new(),
        )
        .await
        .map_err(|error| io::Error::other(error.to_string()))?,
    };
    Ok(PlanCommandOutcome {
        path: outcome.path,
        output: outcome.output,
    })
}

enum SlashCommand<'a> {
    Help,
    Login,
    Logout,
    Plan(&'a str),
    Answer(&'a str),
    Scaffold,
    Continue,
    Test,
    Fix,
    Execute,
    Distill(&'a str),
    Approve,
    Status(&'a str),
    Next,
    Workflow(&'a str),
    Role(&'a str),
}

fn split_command(input: &str) -> Option<SlashCommand<'_>> {
    let input = input.trim();
    let mut parts = input.splitn(2, char::is_whitespace);
    let name = parts.next().unwrap_or("");
    let rest = parts.next().unwrap_or("").trim();
    match name {
        "/help" => Some(SlashCommand::Help),
        "/login" => Some(SlashCommand::Login),
        "/logout" => Some(SlashCommand::Logout),
        "/plan" => Some(SlashCommand::Plan(rest)),
        "/answer" => Some(SlashCommand::Answer(rest)),
        "/scaffold" => Some(SlashCommand::Scaffold),
        "/continue" => Some(SlashCommand::Continue),
        "/test" => Some(SlashCommand::Test),
        "/fix" => Some(SlashCommand::Fix),
        "/execute" => Some(SlashCommand::Execute),
        "/distill" => Some(SlashCommand::Distill(rest)),
        "/approve" => Some(SlashCommand::Approve),
        "/status" => Some(SlashCommand::Status(rest)),
        "/next" => Some(SlashCommand::Next),
        "/workflow" => Some(SlashCommand::Workflow(rest)),
        "/role" => Some(SlashCommand::Role(rest)),
        _ => None,
    }
}

impl SlashCommand<'_> {
    fn name(&self) -> &'static str {
        match self {
            Self::Help => "/help",
            Self::Login => "/login",
            Self::Logout => "/logout",
            Self::Plan(_) => "/plan",
            Self::Answer(_) => "/answer",
            Self::Scaffold => "/scaffold",
            Self::Continue => "/continue",
            Self::Test => "/test",
            Self::Fix => "/fix",
            Self::Execute => "/execute",
            Self::Distill(_) => "/distill",
            Self::Approve => "/approve",
            Self::Status(_) => "/status",
            Self::Next => "/next",
            Self::Workflow(_) => "/workflow",
            Self::Role(_) => "/role",
        }
    }

    fn requires_model(&self) -> bool {
        command_requires_model(self.name())
    }
}

fn command_updates_plan(command: &SlashCommand<'_>) -> bool {
    matches!(
        command,
        SlashCommand::Plan(_) | SlashCommand::Answer(_) | SlashCommand::Distill(_)
    )
}

/// Whether this prompt needs a model credential before it can start.
///
/// Plain text is a model turn. A recognized local command is not. Unknown
/// slash commands are treated as plain text because the REPL forwards them.
#[must_use]
pub fn prompt_requires_model(prompt: &str) -> bool {
    match split_command(prompt) {
        Some(command) => command.requires_model(),
        None => true,
    }
}

async fn repl(session: &mut RoleSession, options: &CliOptions) -> Result<(), Box<dyn Error>> {
    let stdin = io::stdin();
    let mut turn = 0_u64;
    let mut pending_plan: Option<PathBuf> = None;
    let plan_provider = session.provider();
    let credential = credential_status(&options.workspace);
    if !credential.configured() {
        eprintln!(
            "credential: not configured; local commands work, model commands will fail. \
             Use /login or set {}.",
            slowcode_app::API_KEY_ENV
        );
    }
    loop {
        print!("slowcode> ");
        io::stdout().flush()?;
        let mut line = String::new();
        if stdin.lock().read_line(&mut line)? == 0 {
            break;
        }
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        if matches!(line, "exit" | "quit") {
            break;
        }
        if let Some(command) = split_command(line) {
            if command.requires_model() && !credential_status(&options.workspace).configured() {
                eprintln!(
                    "error: {} needs a model credential. Use /login or set {}.",
                    command.name(),
                    slowcode_app::API_KEY_ENV
                );
                continue;
            }
            let updates_plan = command_updates_plan(&command);
            let result = match command {
                SlashCommand::Help => {
                    print_repl_help();
                    continue;
                }
                SlashCommand::Login => {
                    repl_login(&options.workspace);
                    continue;
                }
                SlashCommand::Logout => {
                    repl_logout(&options.workspace);
                    continue;
                }
                SlashCommand::Plan(task) => {
                    run_plan_command_with_optional_session(
                        slowcode_app::plan::PlanCommand::Plan(task.to_owned()),
                        options,
                        pending_plan.as_deref(),
                        Arc::clone(&plan_provider),
                        Some(session),
                    )
                    .await
                }
                SlashCommand::Answer(answer) => {
                    run_plan_command_with_optional_session(
                        slowcode_app::plan::PlanCommand::Answer(answer.to_owned()),
                        options,
                        pending_plan.as_deref(),
                        Arc::clone(&plan_provider),
                        Some(session),
                    )
                    .await
                }
                SlashCommand::Scaffold => {
                    run_plan_command_with_optional_session(
                        slowcode_app::plan::PlanCommand::Scaffold,
                        options,
                        pending_plan.as_deref(),
                        Arc::clone(&plan_provider),
                        Some(session),
                    )
                    .await
                }
                SlashCommand::Continue => {
                    run_plan_command_with_optional_session(
                        slowcode_app::plan::PlanCommand::Continue,
                        options,
                        pending_plan.as_deref(),
                        Arc::clone(&plan_provider),
                        Some(session),
                    )
                    .await
                }
                SlashCommand::Test => {
                    run_plan_command_with_optional_session(
                        slowcode_app::plan::PlanCommand::Test,
                        options,
                        pending_plan.as_deref(),
                        Arc::clone(&plan_provider),
                        Some(session),
                    )
                    .await
                }
                SlashCommand::Fix => {
                    run_plan_command_with_optional_session(
                        slowcode_app::plan::PlanCommand::Fix,
                        options,
                        pending_plan.as_deref(),
                        Arc::clone(&plan_provider),
                        Some(session),
                    )
                    .await
                }
                SlashCommand::Execute => {
                    run_plan_command_with_optional_session(
                        slowcode_app::plan::PlanCommand::Execute,
                        options,
                        pending_plan.as_deref(),
                        Arc::clone(&plan_provider),
                        Some(session),
                    )
                    .await
                }
                SlashCommand::Distill(explanation) => {
                    run_plan_command_with_optional_session(
                        slowcode_app::plan::PlanCommand::Distill(explanation.to_owned()),
                        options,
                        pending_plan.as_deref(),
                        Arc::clone(&plan_provider),
                        Some(session),
                    )
                    .await
                }
                SlashCommand::Approve => {
                    run_plan_command_with_optional_session(
                        slowcode_app::plan::PlanCommand::Approve,
                        options,
                        pending_plan.as_deref(),
                        Arc::clone(&plan_provider),
                        Some(session),
                    )
                    .await
                }
                SlashCommand::Status(argument) => {
                    let argument = (!argument.is_empty()).then(|| argument.to_owned());
                    run_plan_command_with_optional_session(
                        slowcode_app::plan::PlanCommand::Status(argument),
                        options,
                        None,
                        Arc::clone(&plan_provider),
                        Some(session),
                    )
                    .await
                }
                SlashCommand::Next => {
                    run_plan_command_with_optional_session(
                        slowcode_app::plan::PlanCommand::Next,
                        options,
                        None,
                        Arc::clone(&plan_provider),
                        Some(session),
                    )
                    .await
                }
                SlashCommand::Workflow(argument) => {
                    let argument = (!argument.is_empty()).then(|| argument.to_owned());
                    run_plan_command_with_optional_session(
                        slowcode_app::plan::PlanCommand::Workflow(argument),
                        options,
                        None,
                        Arc::clone(&plan_provider),
                        Some(session),
                    )
                    .await
                }
                SlashCommand::Role(argument) => {
                    if let Err(error) = run_role_command(session, argument).await {
                        eprintln!("error: {error}");
                    }
                    continue;
                }
            };
            match result {
                Ok(outcome) => {
                    println!("{}", outcome.output);
                    if updates_plan {
                        pending_plan = Some(outcome.path);
                    }
                }
                Err(error) => eprintln!("error: {error}"),
            }
            continue;
        }
        turn += 1;
        if !credential_status(&options.workspace).configured() {
            eprintln!(
                "error: a model turn needs a model credential. Use /login or set {}.",
                slowcode_app::API_KEY_ENV
            );
            continue;
        }
        run_prompt(session, line, turn).await?;
    }
    Ok(())
}

/// Commands the line-based REPL understands.
const REPL_COMMANDS: &[&str] = &[
    "/help",
    "/status",
    "/next",
    "/workflow",
    "/role",
    "/login",
    "/logout",
    "/plan",
    "/answer",
    "/scaffold",
    "/continue",
    "/test",
    "/fix",
    "/execute",
    "/distill",
    "/approve",
];

fn print_repl_help() {
    println!("REPL commands:");
    for spec in command_specs()
        .iter()
        .filter(|spec| REPL_COMMANDS.contains(&spec.name))
    {
        let usage = if spec.args.is_empty() {
            spec.name.to_owned()
        } else {
            format!("{} {}", spec.name, spec.args)
        };
        println!("  {usage:<28} {}", spec.description);
    }
    println!("  {:<28} leave the REPL", "exit | quit");
}

fn repl_login(workspace: &Path) {
    let status = credential_status(workspace);
    let source = match status.source {
        CredentialSourceKind::WorkspaceEnv => "workspace .env",
        CredentialSourceKind::Environment => "environment",
        CredentialSourceKind::Missing => "not configured",
    };
    println!("credential: {source}");
    let key = match read_hidden_key() {
        Ok(key) => key,
        Err(error) => {
            eprintln!("login: {error}");
            return;
        }
    };
    if key.is_empty() {
        eprintln!("login: empty key; nothing saved");
        return;
    }
    match save_api_key(workspace, &key) {
        Ok(path) => println!(
            "login: saved to {} with mode 0600; the key is never printed",
            path.display()
        ),
        Err(error) => eprintln!("login: {error}"),
    }
}

fn repl_logout(workspace: &Path) {
    match remove_api_key(workspace) {
        Ok(true) => println!(
            "logout: removed {} from {}",
            slowcode_app::API_KEY_ENV,
            workspace.join(".env").display()
        ),
        Ok(false) => {
            let status = credential_status(workspace);
            if status.configured() {
                println!(
                    "logout: no key in the workspace .env; the current key comes from the \
                     environment. Unset {} to remove it.",
                    slowcode_app::API_KEY_ENV
                );
            } else {
                println!("logout: no credential is configured");
            }
        }
        Err(error) => eprintln!("logout: {error}"),
    }
}

fn read_hidden_key() -> io::Result<String> {
    use rustix::termios::{LocalModes, OptionalActions};

    let stdin = rustix::stdio::stdin();
    if !rustix::termios::isatty(stdin) {
        return Err(io::Error::other(
            "cannot read a hidden key without a terminal; set DEEPSEEK_API_KEY or edit the \
             workspace .env",
        ));
    }
    let original = rustix::termios::tcgetattr(stdin).map_err(termios_error)?;
    let mut hidden = original.clone();
    hidden.local_modes.remove(LocalModes::ECHO);
    rustix::termios::tcsetattr(stdin, OptionalActions::Now, &hidden).map_err(termios_error)?;

    eprint!("DeepSeek API key (input hidden): ");
    io::stderr().flush()?;
    let mut line = String::new();
    let read = io::stdin().read_line(&mut line);
    let restore =
        rustix::termios::tcsetattr(stdin, OptionalActions::Now, &original).map_err(termios_error);
    eprintln!();
    read?;
    restore?;
    Ok(line.trim().to_owned())
}

fn termios_error(error: rustix::io::Errno) -> io::Error {
    io::Error::other(error.to_string())
}

fn print_outcome(outcome: &TurnOutcome, cache: Option<&CacheReport>) {
    let text = slowcode_app::outcome_text(outcome);
    if text.is_empty() {
        println!("[turn ended: {:?}]", outcome.reason());
    } else {
        println!("{text}");
    }
    if let Some(report) = cache.and_then(CacheReport::render) {
        println!("{report}");
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn args(values: &[&str]) -> Vec<String> {
        values.iter().map(|value| (*value).to_owned()).collect()
    }

    #[test]
    fn parses_defaults_and_prompt() {
        let Command::Run(options) = parse_args(args(&["read hello.txt"])).expect("parses") else {
            panic!("expected run");
        };
        assert_eq!(options.model, "deepseek-flash");
        assert_eq!(options.prompt.as_deref(), Some("read hello.txt"));
        assert!(!options.read_only);
    }

    #[test]
    fn parses_flags() {
        let Command::Run(options) = parse_args(args(&[
            "--workspace",
            "/tmp/work",
            "--model=deepseek-v4-pro",
            "--plan",
            "/tmp/plan.md",
            "--max-tokens",
            "1024",
            "--context-window=200000",
            "--read-only",
            "do",
            "it",
        ]))
        .expect("parses") else {
            panic!("expected run");
        };
        assert_eq!(options.workspace, PathBuf::from("/tmp/work"));
        assert_eq!(options.model, "deepseek-v4-pro");
        assert_eq!(options.plan, Some(PathBuf::from("/tmp/plan.md")));
        assert_eq!(options.max_tokens, Some(1024));
        assert_eq!(options.context_window, Some(200_000));
        assert!(options.read_only);
        assert_eq!(options.prompt.as_deref(), Some("do it"));
    }

    #[test]
    fn parses_ask_flag() {
        let Command::Run(options) =
            parse_args(args(&["--ask", "--workspace", ".", "task"])).expect("parses")
        else {
            panic!("expected run");
        };
        assert!(options.ask);
    }

    #[test]
    fn rejects_unknown_flags() {
        assert!(parse_args(args(&["--wat"])).is_err());
        assert!(parse_args(args(&["--max-tokens", "0"])).is_err());
    }

    #[test]
    fn parses_session_flags() {
        let Command::Run(options) = parse_args(args(&[
            "--session",
            "session-11111111-1111-4111-8111-111111111111",
            "--resume",
            "continue",
        ]))
        .expect("parses") else {
            panic!("expected run");
        };
        assert_eq!(
            options.session.as_deref(),
            Some("session-11111111-1111-4111-8111-111111111111")
        );
        assert!(options.resume);
    }

    #[test]
    fn rejects_invalid_session_combinations() {
        assert!(parse_args(args(&["--resume"])).is_err());
        assert!(parse_args(args(&["--session", "new", "--resume"])).is_err());
        assert!(parse_args(args(&["--session", "not-a-session"])).is_err());
        assert!(
            parse_args(args(&[
                "--session",
                "session-11111111-1111-4111-8111-111111111111",
            ]))
            .is_ok()
        );
    }

    #[test]
    fn role_session_config_carries_read_only() {
        let Command::Run(options) =
            parse_args(args(&["--read-only", "--workspace", "/tmp/work"])).expect("parses")
        else {
            panic!("expected run");
        };
        let config = role_session_config(&options).expect("role config builds");
        assert!(config.read_only);
        assert_eq!(config.workspace, PathBuf::from("/tmp/work"));
        assert!(config.base_system_prompt.contains("slowcode"));
    }

    #[test]
    fn only_model_prompts_need_a_credential_up_front() {
        assert!(prompt_requires_model("read hello.txt"));
        assert!(prompt_requires_model("/plan do it"));
        assert!(!prompt_requires_model("/status"));
        assert!(!prompt_requires_model("/role navigator"));
        assert!(!prompt_requires_model("/login"));
        assert!(!prompt_requires_model("/logout"));
        assert!(!prompt_requires_model("/help"));
    }

    #[test]
    fn splits_slash_commands() {
        assert!(matches!(
            split_command("/plan do it"),
            Some(SlashCommand::Plan("do it"))
        ));
        assert!(matches!(
            split_command("/answer yes"),
            Some(SlashCommand::Answer("yes"))
        ));
        assert!(matches!(
            split_command("/scaffold"),
            Some(SlashCommand::Scaffold)
        ));
        assert!(matches!(split_command("/test"), Some(SlashCommand::Test)));
        assert!(matches!(split_command("/fix"), Some(SlashCommand::Fix)));
        assert!(matches!(
            split_command("/execute"),
            Some(SlashCommand::Execute)
        ));
        assert!(matches!(
            split_command("/distill because"),
            Some(SlashCommand::Distill("because"))
        ));
        assert!(matches!(
            split_command("/approve"),
            Some(SlashCommand::Approve)
        ));
        assert!(matches!(
            split_command("/status --workflow plan-1"),
            Some(SlashCommand::Status("--workflow plan-1"))
        ));
        assert!(matches!(split_command("/next"), Some(SlashCommand::Next)));
        assert!(matches!(
            split_command("/workflow selector"),
            Some(SlashCommand::Workflow("selector"))
        ));
        assert!(matches!(
            split_command("/workflow"),
            Some(SlashCommand::Workflow(""))
        ));
        assert!(matches!(
            split_command("/role navigator"),
            Some(SlashCommand::Role("navigator"))
        ));
        assert!(split_command("/planet").is_none());
        assert!(split_command("plan it").is_none());

        assert!(command_updates_plan(&SlashCommand::Plan("x")));
        assert!(command_updates_plan(&SlashCommand::Answer("x")));
        assert!(command_updates_plan(&SlashCommand::Distill("x")));
        assert!(!command_updates_plan(&SlashCommand::Approve));
        assert!(!command_updates_plan(&SlashCommand::Workflow("selector")));
        assert!(!command_updates_plan(&SlashCommand::Status("")));
    }

    struct PlanProvider {
        attempts: std::sync::Mutex<
            std::collections::VecDeque<
                Vec<
                    Result<
                        slowcode_kernel::model::StreamChunk,
                        slowcode_kernel::provider::ProviderStreamError,
                    >,
                >,
            >,
        >,
    }

    impl PlanProvider {
        fn new(texts: &[&str]) -> Self {
            Self {
                attempts: std::sync::Mutex::new(
                    texts
                        .iter()
                        .map(|text| text_response(text))
                        .collect::<std::collections::VecDeque<_>>(),
                ),
            }
        }
    }

    impl ModelProvider for PlanProvider {
        fn prepare_call(
            &self,
            config: LlmCallConfig,
        ) -> Result<
            slowcode_kernel::provider::PreparedProviderCall,
            slowcode_kernel::provider::ProviderPrepareError,
        > {
            let mut raw = config.raw().as_value().clone();
            raw.as_object_mut()
                .expect("call config is an object")
                .insert("maxTokens".to_owned(), serde_json::json!(1_024));
            let config = serde_json::from_value(raw).expect("max tokens produce a valid config");
            Ok(slowcode_kernel::provider::PreparedProviderCall::new(
                config,
                slowcode_kernel::model::LlmCallConfigAdapterDefaults::default(),
                Some(
                    slowcode_kernel::model::NonNegativeSafeInteger::new(16_384)
                        .expect("context window fits"),
                ),
            ))
        }

        fn preflight_request(
            &self,
            draft: slowcode_kernel::provider::ProviderRequestDraft<'_>,
        ) -> Result<
            slowcode_kernel::provider::PreparedRequestPreflight,
            slowcode_kernel::provider::ProviderPreflightError,
        > {
            let prepared = self.prepare_call(draft.config().clone())?;
            draft.finish(prepared, 1)
        }

        fn stream(
            &self,
            _request: slowcode_kernel::provider::ProviderRequest,
            _cancel: CancellationToken,
        ) -> slowcode_kernel::provider::ProviderStream {
            let chunks = self
                .attempts
                .lock()
                .unwrap()
                .pop_front()
                .expect("fake provider has one scripted attempt");
            Box::pin(futures_util::stream::iter(chunks))
        }
    }

    fn text_response(
        text: &str,
    ) -> Vec<
        Result<slowcode_kernel::model::StreamChunk, slowcode_kernel::provider::ProviderStreamError>,
    > {
        use slowcode_kernel::model::{ContentBlock, ContentBlockType, FinishReason, StreamChunk};
        vec![
            Ok(StreamChunk::block_start(0, ContentBlockType::Text).unwrap()),
            Ok(StreamChunk::text_delta(0, text).unwrap()),
            Ok(StreamChunk::block_end(0, ContentBlock::text(text).unwrap()).unwrap()),
            Ok(StreamChunk::finish(FinishReason::stop().unwrap(), None).unwrap()),
        ]
    }

    #[tokio::test]
    async fn repl_plan_command_reuses_the_role_session() {
        let workspace = std::env::temp_dir().join(format!(
            "slowcode-cli-plan-session-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&workspace).unwrap();
        let call = LlmCallConfig::new(DEEPSEEK_PROVIDER, "deepseek-flash").unwrap();
        let config = RoleSessionConfig {
            workspace: workspace.clone(),
            call,
            read_only: false,
            base_system_prompt: "test".to_owned(),
        };
        let mut session = RoleSession::new(
            config,
            Arc::new(PlanProvider::new(&[
                "\
## 证据包
- read src/main.rs

## 认知任务分配表
| 任务 | 认知属性 | 判定依据 | 执行状态 | Agent 行为 | 用户角色 | 最小深度链路 | 验收 |
|---|---|---|---|---|---|---|---|
| 读取输入 | 已知域 | src/main.rs | 可执行 | 自主执行 | 审查者 | read | 打印内容 |
",
                "normal answer after plan",
            ])),
            RoleName::Driver,
        )
        .await
        .expect("role session starts");
        let session_id = session.session().id().as_str().to_owned();
        let options = CliOptions {
            workspace: workspace.clone(),
            model: "deepseek-flash".to_owned(),
            plan: None,
            max_tokens: None,
            context_window: None,
            read_only: false,
            ask: false,
            session: None,
            resume: false,
            prompt: None,
        };

        let outcome = run_plan_command_with_optional_session(
            slowcode_app::plan::PlanCommand::Plan("count lines".to_owned()),
            &options,
            None,
            session.provider(),
            Some(&mut session),
        )
        .await
        .expect("plan succeeds");

        assert_eq!(session.session().id().as_str(), session_id);
        assert!(outcome.output.contains("## 证据包"));
        let message = Message::user(
            "user-1",
            vec![ContentBlock::text("next").expect("content block")],
            MessageSource::user().expect("user source"),
        )
        .expect("user message");
        let outcome = session
            .run_turn(message, CancellationToken::new())
            .await
            .expect("normal turn succeeds after plan");
        assert!(
            slowcode_app::outcome_text(&outcome).contains("normal answer after plan"),
            "the restored agent serves the next turn"
        );
        let _ = std::fs::remove_dir_all(&workspace);
    }
}
