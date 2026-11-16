//! Session assembly shared by the TUI, headless run and REPL.

use std::{
    collections::{BTreeMap, BTreeSet},
    path::PathBuf,
    sync::Arc,
};

use slowcode_kernel::{
    agent::{ApprovalProvider, ToolExecutor, TurnOutcome},
    model::{ContentBlock, LlmCallConfig, Message, MessageSource, ToolSchema},
    provider::{
        ModelProvider,
        deepseek::{DEEPSEEK_PROVIDER, DeepSeekConfig, DeepSeekModelConfig, DeepSeekProvider},
    },
    session::{
        Session, SessionMetadata, SessionStore, StoreError, new_session_id, validate_session_id,
    },
    ui::UiEventReceiver,
    workspace_instructions::{WorkspaceInstructionError, WorkspaceInstructionRuntime},
};
use slowcode_roles::{ExtraToolSlot, RoleName, RoleSession, RoleSessionConfig, RoleSessionError};
use thiserror::Error;
use tokio_util::sync::CancellationToken;

use crate::plan::{self, PlanCommand, PlanConfig, PlanOutcome};
use crate::workflow::{AuditedReadonlyShell, WorkflowStore};

/// Static inputs for one runtime.
#[derive(Clone, Debug)]
pub struct RuntimeConfig {
    pub workspace: PathBuf,
    pub model: String,
    pub read_only: bool,
    pub max_tokens: Option<u64>,
    pub context_window: Option<u64>,
}

impl RuntimeConfig {
    pub fn new(workspace: PathBuf) -> Self {
        Self {
            workspace,
            model: "deepseek-flash".to_owned(),
            read_only: false,
            max_tokens: None,
            context_window: None,
        }
    }
}

/// How the runtime obtains its first session.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum SessionChoice {
    New,
    Create(String),
    Resume(String),
}

/// Outcome of one role switch attempt.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct RoleSwitchReport {
    pub from: RoleName,
    pub to: RoleName,
    pub changed: bool,
    pub role_committed: bool,
    pub slots: Vec<String>,
    pub warnings: Vec<String>,
}

/// A started runtime plus the UI event stream attached before the first event.
pub struct StartedRuntime {
    pub runtime: Runtime,
    pub events: UiEventReceiver,
    pub session_id: String,
    pub history: Vec<slowcode_kernel::ui::UiEvent>,
}

/// Runtime failures with stable, user-facing messages.
#[derive(Debug, Error)]
pub enum RuntimeError {
    #[error("provider: {0}")]
    Provider(String),
    #[error(transparent)]
    Store(#[from] StoreError),
    #[error(transparent)]
    Role(#[from] RoleSessionError),
    #[error(transparent)]
    WorkspaceInstruction(#[from] WorkspaceInstructionError),
    #[error("invalid session id: {0}")]
    InvalidSession(String),
    #[error("model: {0}")]
    Model(String),
    #[error("message: {0}")]
    Message(String),
    #[error("the runtime has no active session")]
    Unavailable,
    #[error("the UI observer could not attach: {0}")]
    Ui(String),
    #[error("mcp: {0}")]
    Mcp(String),
    #[error("tools: {0}")]
    Tools(String),
    #[error("{0}")]
    Plan(String),
}

/// Outcome of connecting the configured MCP servers.
#[derive(Clone, Debug, Default)]
pub struct McpInstallReport {
    pub servers: usize,
    pub tools: usize,
    pub dormant: usize,
    pub errors: Vec<(String, String)>,
}

/// One durable snapshot for a feedback bundle.
#[derive(Clone, Debug)]
pub struct FeedbackSnapshot {
    pub session_id: String,
    pub journal: Vec<u8>,
    pub workspace: PathBuf,
    pub model: String,
}

impl McpInstallReport {
    pub fn summary(&self) -> String {
        let mut summary = if self.servers == 0 && self.dormant > 0 {
            format!("MCP: {} dormant server(s)", self.dormant)
        } else {
            format!(
                "MCP: {} tool(s) from {} server(s)",
                self.tools, self.servers
            )
        };
        if self.dormant > 0 && self.servers > 0 {
            summary.push_str(&format!("; {} dormant", self.dormant));
        }
        if !self.errors.is_empty() {
            summary.push_str(&format!(
                "; {} error(s): {}",
                self.errors.len(),
                self.errors
                    .iter()
                    .map(|(server, error)| format!("{server}: {error}"))
                    .collect::<Vec<_>>()
                    .join(", ")
            ));
        }
        summary
    }
}

/// One line in the `/context` breakdown.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ContextSlice {
    pub name: &'static str,
    pub tokens: u64,
    pub note: Option<String>,
}

/// Token estimate for every context source the TUI displays.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct ContextBreakdown {
    pub window: Option<u64>,
    pub slices: Vec<ContextSlice>,
}

/// One live role session plus its store and static configuration.
pub struct Runtime {
    config: RuntimeConfig,
    provider: Arc<dyn ModelProvider>,
    store: SessionStore,
    session_id: String,
    role_session: Option<RoleSession>,
    workspace_instructions: WorkspaceInstructionRuntime,
    approval_provider: Option<Arc<dyn ApprovalProvider>>,
    approval_required: bool,
    mcp_gate: Arc<slowcode_mcp::McpApprovalGate>,
    mcp_active: BTreeSet<String>,
    mcp_tools: Option<Arc<dyn ToolExecutor>>,
    mcp_schemas: Vec<ToolSchema>,
    extra_schema_tokens: BTreeMap<ExtraToolSlot, u64>,
    turn: u64,
    readonly_shell_installed: bool,
}

impl Runtime {
    /// Build the provider and create or resume the first durable session.
    pub async fn start(
        config: RuntimeConfig,
        choice: SessionChoice,
    ) -> Result<StartedRuntime, RuntimeError> {
        let _ = dotenvy::dotenv();
        let _ = dotenvy::from_path(config.workspace.join(".env"));
        let provider = build_provider(&config)?;
        Self::start_with(config, choice, provider).await
    }

    /// Start with an injected provider. Used by tests and callers that build
    /// their own provider.
    pub async fn start_with(
        config: RuntimeConfig,
        choice: SessionChoice,
        provider: Arc<dyn ModelProvider>,
    ) -> Result<StartedRuntime, RuntimeError> {
        let store = SessionStore::open_default()?;
        let (mut session, session_id) = open_session(&store, &config.workspace, choice).await?;
        let events = session
            .attach_ui_events()
            .map_err(|error| RuntimeError::Ui(error.to_string()))?;
        let history = session.replay_ui_events();
        let workspace_instructions =
            WorkspaceInstructionRuntime::from_workspace(&config.workspace)?;

        let role_config = role_config(&config)?;
        let mut role_session = RoleSession::new_with_session(
            role_config,
            Arc::clone(&provider),
            RoleName::Driver,
            session,
        )
        .await?;
        role_session.install_workspace_instructions(workspace_instructions.clone());
        let mcp_gate = slowcode_mcp::McpApprovalGate::new(config.read_only);
        let mcp_active = startup_active_servers(&config.workspace);

        let runtime = Self {
            config,
            provider,
            store,
            session_id: session_id.clone(),
            role_session: Some(role_session),
            workspace_instructions,
            approval_provider: None,
            approval_required: false,
            mcp_gate,
            mcp_active,
            mcp_tools: None,
            mcp_schemas: Vec::new(),
            extra_schema_tokens: BTreeMap::new(),
            turn: 0,
            readonly_shell_installed: false,
        };
        Ok(StartedRuntime {
            runtime,
            events,
            session_id,
            history,
        })
    }

    /// Install the approval provider used by every current and future turn.
    pub fn install_approval_provider(&mut self, provider: Arc<dyn ApprovalProvider>) {
        self.approval_required = true;
        self.mcp_gate.set_provider(Arc::clone(&provider));
        if let Some(session) = self.role_session.as_mut() {
            session.install_approval_provider(Arc::clone(&provider));
        }
        self.approval_provider = Some(provider);
    }

    /// Install a provider for MCP tool approval without enabling file or shell
    /// prompts.
    pub fn install_approval_provider_passive(&mut self, provider: Arc<dyn ApprovalProvider>) {
        self.approval_required = false;
        self.mcp_gate.set_provider(Arc::clone(&provider));
        if let Some(session) = self.role_session.as_mut() {
            session.install_approval_provider_passive(Arc::clone(&provider));
        }
        self.approval_provider = Some(provider);
    }

    /// Install one extra tool slot and rebuild the agent around the same
    /// session. Shared slots are also retained by the runtime so a session
    /// switch can reinstall them.
    pub async fn install_extra_tools(
        &mut self,
        slot: ExtraToolSlot,
        tools: Arc<dyn ToolExecutor>,
        schemas: Vec<ToolSchema>,
    ) -> Result<(), RuntimeError> {
        let session = self
            .role_session
            .as_mut()
            .ok_or(RuntimeError::Unavailable)?;
        let tokens = schema_tokens(&schemas);
        session
            .install_extra_tools(slot, Arc::clone(&tools), schemas.clone())
            .await?;
        self.extra_schema_tokens.insert(slot, tokens);
        if slot == ExtraToolSlot::Mcp {
            if let Some(previous) = self.mcp_tools.replace(tools) {
                let _ = previous.shutdown().await;
            }
            self.mcp_schemas = schemas;
        }
        Ok(())
    }

    /// Remove one extra tool slot and rebuild the agent.
    pub async fn clear_extra_tools(&mut self, slot: ExtraToolSlot) -> Result<(), RuntimeError> {
        let session = self
            .role_session
            .as_mut()
            .ok_or(RuntimeError::Unavailable)?;
        session.clear_extra_tools(slot).await?;
        self.extra_schema_tokens.remove(&slot);
        if slot == ExtraToolSlot::ReadonlyShell {
            self.readonly_shell_installed = false;
        }
        if slot == ExtraToolSlot::Mcp {
            self.mcp_schemas.clear();
            if let Some(tools) = self.mcp_tools.take() {
                let _ = tools.shutdown().await;
            }
        }
        Ok(())
    }

    /// Connect every enabled MCP server and replace the extra tool snapshot.
    pub async fn install_mcp_tools(&mut self) -> Result<McpInstallReport, RuntimeError> {
        let config = slowcode_mcp::McpConfig::load(&self.config.workspace)
            .map_err(|error| RuntimeError::Mcp(error.to_string()))?;
        self.sync_active_mcp(&config);
        let servers = config
            .servers
            .values()
            .filter(|server| server.enabled && self.mcp_active.contains(&server.name))
            .cloned()
            .collect::<Vec<_>>();
        let toolset = slowcode_mcp::connect_toolset_with_options(
            &servers,
            slowcode_mcp::McpToolsetOptions {
                gate: Arc::clone(&self.mcp_gate),
            },
        )
        .await;
        let report = McpInstallReport {
            servers: servers.len(),
            tools: toolset.schemas.len(),
            dormant: config
                .servers
                .values()
                .filter(|server| server.enabled && !self.mcp_active.contains(&server.name))
                .count(),
            errors: toolset.errors.clone(),
        };
        if servers.is_empty() && toolset.schemas.is_empty() {
            self.clear_extra_tools(ExtraToolSlot::Mcp).await?;
            return Ok(report);
        }
        self.install_extra_tools(ExtraToolSlot::Mcp, toolset.executor, toolset.schemas)
            .await?;
        Ok(report)
    }

    pub fn activate_mcp_server(&mut self, name: &str) {
        self.mcp_active.insert(name.to_owned());
    }

    pub fn deactivate_mcp_server(&mut self, name: &str) {
        self.mcp_active.remove(name);
    }

    pub fn is_mcp_active(&self, name: &str) -> bool {
        self.mcp_active.contains(name)
    }

    fn sync_active_mcp(&mut self, config: &slowcode_mcp::McpConfig) {
        self.mcp_active
            .retain(|name| config.servers.contains_key(name));
        for server in config.servers.values() {
            if server.enabled && server.scope == Some(slowcode_mcp::McpScope::User) {
                self.mcp_active.insert(server.name.clone());
            }
        }
    }

    pub fn session_id(&self) -> &str {
        &self.session_id
    }

    /// Flush the live journal and read back its exact durable prefix.
    ///
    /// The flush makes the snapshot include events that were committed in
    /// memory but not yet written when the command ran.
    pub async fn feedback_snapshot(&mut self) -> Result<FeedbackSnapshot, RuntimeError> {
        let role_session = self
            .role_session
            .as_mut()
            .ok_or(RuntimeError::Unavailable)?;
        role_session.flush_journal().await?;
        let id = validate_session_id(&self.session_id)?;
        let journal = self.store.read_journal(&id)?;
        Ok(FeedbackSnapshot {
            session_id: self.session_id.clone(),
            journal,
            workspace: self.config.workspace.clone(),
            model: self.config.model.clone(),
        })
    }

    pub fn model(&self) -> &str {
        &self.config.model
    }

    pub fn workspace(&self) -> &std::path::Path {
        &self.config.workspace
    }

    pub fn read_only(&self) -> bool {
        self.config.read_only
    }

    /// Effective context window for the active model, when known.
    pub fn context_window(&self) -> Option<u64> {
        self.config.context_window.or_else(|| {
            known_models()
                .into_iter()
                .find(|model| model.id() == self.config.model)
                .and_then(|model| model.context_window())
                .map(|window| window.get())
        })
    }

    /// Prompt-context estimate grouped the way `/context` renders it.
    pub fn context_breakdown(&self) -> ContextBreakdown {
        let workspace = &self.config.workspace;
        let base = base_system_prompt(workspace);
        let role = self.role().unwrap_or(RoleName::Driver);
        let role_prompt = match role {
            RoleName::Navigator => slowcode_roles::NAVIGATOR_PROMPT,
            RoleName::Driver => slowcode_roles::DRIVER_PROMPT,
        };
        let environment = format!(
            "workspace: {}\nplatform: {} {}\n",
            workspace.display(),
            std::env::consts::OS,
            std::env::consts::ARCH
        );
        let environment_tokens = estimate_tokens(&environment);
        let system_tokens = estimate_tokens(&base).saturating_sub(environment_tokens)
            + estimate_tokens(role_prompt);

        ContextBreakdown {
            window: self.context_window(),
            slices: vec![
                ContextSlice {
                    name: "System prompt",
                    tokens: system_tokens,
                    note: None,
                },
                ContextSlice {
                    name: "Environment info",
                    tokens: environment_tokens,
                    note: Some("workspace and platform".to_owned()),
                },
                ContextSlice {
                    name: "MCP tools",
                    tokens: self
                        .extra_schema_tokens
                        .get(&ExtraToolSlot::Mcp)
                        .copied()
                        .unwrap_or(0),
                    note: None,
                },
                ContextSlice {
                    name: "Readonly shell",
                    tokens: self
                        .extra_schema_tokens
                        .get(&ExtraToolSlot::ReadonlyShell)
                        .copied()
                        .unwrap_or(0),
                    note: None,
                },
                ContextSlice {
                    name: "Skill descriptions",
                    tokens: skill_catalog_tokens(workspace, role),
                    note: None,
                },
                ContextSlice {
                    name: "AGENTS.md",
                    tokens: file_tokens(&workspace.join("AGENTS.md")),
                    note: None,
                },
                ContextSlice {
                    name: "GLOSSARY.md",
                    tokens: file_tokens(&workspace.join("GLOSSARY.md")),
                    note: None,
                },
                ContextSlice {
                    name: "HANDOFF.md",
                    tokens: 0,
                    note: Some("pointer, read on demand".to_owned()),
                },
            ],
        }
    }

    pub fn role(&self) -> Option<RoleName> {
        self.role_session.as_ref().map(RoleSession::role)
    }

    fn installed_slots(&self) -> Vec<String> {
        let mut slots = Vec::new();
        if self.mcp_tools.is_some() {
            slots.push(ExtraToolSlot::Mcp.as_str().to_owned());
        }
        if self.readonly_shell_installed {
            slots.push(ExtraToolSlot::ReadonlyShell.as_str().to_owned());
        }
        slots
    }

    async fn install_shared_slots(&self, session: &mut RoleSession) -> Result<(), RuntimeError> {
        let Some(tools) = &self.mcp_tools else {
            return Ok(());
        };
        session
            .install_extra_tools(
                ExtraToolSlot::Mcp,
                Arc::clone(tools),
                self.mcp_schemas.clone(),
            )
            .await?;
        Ok(())
    }

    /// Run one ordinary prompt turn.
    pub async fn run_turn(
        &mut self,
        text: String,
        cancellation: CancellationToken,
    ) -> Result<TurnOutcome, RuntimeError> {
        self.turn = self.turn.saturating_add(1);
        let message = Message::user(
            format!("user-{}", self.turn),
            vec![
                ContentBlock::text(text)
                    .map_err(|error| RuntimeError::Message(error.to_string()))?,
            ],
            MessageSource::user().map_err(|error| RuntimeError::Message(error.to_string()))?,
        )
        .map_err(|error| RuntimeError::Message(error.to_string()))?;
        let session = self
            .role_session
            .as_mut()
            .ok_or(RuntimeError::Unavailable)?;
        Ok(session.run_turn(message, cancellation).await?)
    }

    /// Run one Plan command in-process, sharing this runtime's provider and
    /// approval policy. Plan turns keep their own ephemeral sessions.
    pub async fn run_plan_command(
        &mut self,
        command: PlanCommand,
        cancellation: CancellationToken,
    ) -> Result<PlanOutcome, RuntimeError> {
        let config = PlanConfig {
            workspace: self.config.workspace.clone(),
            model: self.config.model.clone(),
            plan: None,
            max_tokens: self.config.max_tokens,
            read_only: self.config.read_only,
        };
        let approval = if self.approval_required {
            self.approval_provider.clone()
        } else {
            None
        };
        plan::run_command(
            command,
            &config,
            Arc::clone(&self.provider),
            approval,
            cancellation,
        )
        .await
        .map_err(|error| RuntimeError::Plan(error.to_string()))
    }

    /// Switch roles at a turn boundary and report what actually happened.
    ///
    /// A readonly-shell installation failure happens after the role commit, so
    /// it is reported as a warning instead of hiding a committed role change.
    pub async fn switch_role(&mut self, role: RoleName) -> Result<RoleSwitchReport, RuntimeError> {
        let from = self.role().unwrap_or(RoleName::Driver);
        let changed = {
            let session = self
                .role_session
                .as_mut()
                .ok_or(RuntimeError::Unavailable)?;
            session.switch(role).await?
        };
        self.mcp_gate.set_role_read_only(role.spec().is_read_only());
        let mut warnings = Vec::new();
        let tools_changed = {
            let session = self
                .role_session
                .as_mut()
                .ok_or(RuntimeError::Unavailable)?;
            match sync_readonly_shell(&self.config, session, &mut self.readonly_shell_installed)
                .await
            {
                Ok(changed) => changed,
                Err(error) => {
                    warnings.push(format!("readonly shell: {error}"));
                    false
                }
            }
        };
        Ok(RoleSwitchReport {
            from,
            to: role,
            changed: changed || tools_changed,
            role_committed: true,
            slots: self.installed_slots(),
            warnings,
        })
    }

    pub async fn set_model(&mut self, model: &str) -> Result<(), RuntimeError> {
        let call = self.call_for(model)?;
        let session = self
            .role_session
            .as_mut()
            .ok_or(RuntimeError::Unavailable)?;
        session.set_call(call).await?;
        self.config.model = model.to_owned();
        Ok(())
    }

    /// Rename the active durable session.
    pub async fn rename_session(&mut self, title: &str) -> Result<Option<String>, RuntimeError> {
        let session = self
            .role_session
            .as_mut()
            .ok_or(RuntimeError::Unavailable)?;
        Ok(session.rename_session(title).await?)
    }

    /// List stored sessions for the current workspace, newest first.
    pub fn list_sessions(&self) -> Result<Vec<SessionMetadata>, RuntimeError> {
        let mut sessions = self
            .store
            .list_for_workspace(Some(&self.config.workspace))?;
        sessions.sort_by_key(|session| std::cmp::Reverse(session.created_at().get()));
        Ok(sessions)
    }

    /// Resume another stored session and return its fresh UI event stream.
    pub async fn switch_session(
        &mut self,
        id: &str,
    ) -> Result<(UiEventReceiver, Vec<slowcode_kernel::ui::UiEvent>), RuntimeError> {
        let session_id = validate_session_id(id)
            .map_err(|error| RuntimeError::InvalidSession(error.to_string()))?;
        let mut session = self
            .store
            .resume_session(session_id.clone(), &self.config.workspace)
            .await?;
        let events = session
            .attach_ui_events()
            .map_err(|error| RuntimeError::Ui(error.to_string()))?;
        let history = session.replay_ui_events();
        let mut role_session = RoleSession::new_with_session(
            role_config(&self.config)?,
            Arc::clone(&self.provider),
            RoleName::Driver,
            session,
        )
        .await?;
        role_session.install_workspace_instructions(self.workspace_instructions.clone());
        self.install_approval_on(&mut role_session);
        self.install_shared_slots(&mut role_session).await?;
        self.mcp_gate.revoke_all();
        self.mcp_gate.set_role_read_only(false);
        let old = self.role_session.replace(role_session);
        self.session_id = session_id.to_string();
        self.readonly_shell_installed = false;
        self.extra_schema_tokens
            .remove(&ExtraToolSlot::ReadonlyShell);
        if let Some(old) = old {
            old.shutdown().await?;
        }
        Ok((events, history))
    }

    /// Create a new durable session and return its fresh UI event stream.
    pub async fn new_session(&mut self) -> Result<UiEventReceiver, RuntimeError> {
        let session_id = new_session_id();
        let mut session = self
            .store
            .open_new_session(session_id.clone(), &self.config.workspace)
            .await?;
        let events = session
            .attach_ui_events()
            .map_err(|error| RuntimeError::Ui(error.to_string()))?;
        let mut role_session = RoleSession::new_with_session(
            role_config(&self.config)?,
            Arc::clone(&self.provider),
            RoleName::Driver,
            session,
        )
        .await?;
        role_session.install_workspace_instructions(self.workspace_instructions.clone());
        self.install_approval_on(&mut role_session);
        self.install_shared_slots(&mut role_session).await?;
        self.mcp_gate.revoke_all();
        self.mcp_gate.set_role_read_only(false);
        let old = self.role_session.replace(role_session);
        self.session_id = session_id.to_string();
        self.readonly_shell_installed = false;
        self.extra_schema_tokens
            .remove(&ExtraToolSlot::ReadonlyShell);
        if let Some(old) = old {
            old.shutdown().await?;
        }
        Ok(events)
    }

    pub async fn shutdown(mut self) -> Result<(), RuntimeError> {
        if let Some(session) = self.role_session.take() {
            session.shutdown().await?;
        }
        if let Some(tools) = self.mcp_tools.take() {
            tools
                .shutdown()
                .await
                .map_err(|error| RuntimeError::Mcp(error.to_string()))?;
        }
        Ok(())
    }

    fn install_approval_on(&self, session: &mut RoleSession) {
        let Some(provider) = &self.approval_provider else {
            return;
        };
        if self.approval_required {
            session.install_approval_provider(Arc::clone(provider));
        } else {
            session.install_approval_provider_passive(Arc::clone(provider));
        }
    }

    fn call_for(&self, model: &str) -> Result<LlmCallConfig, RuntimeError> {
        let call = LlmCallConfig::new(DEEPSEEK_PROVIDER, model)
            .map_err(|error| RuntimeError::Model(error.to_string()))?;
        apply_max_tokens(call, self.config.max_tokens)
            .map_err(|error| RuntimeError::Model(error.to_string()))
    }
}

fn startup_active_servers(workspace: &std::path::Path) -> BTreeSet<String> {
    slowcode_mcp::McpConfig::load(workspace)
        .map(|config| {
            config
                .servers
                .values()
                .filter(|server| {
                    server.enabled && server.scope == Some(slowcode_mcp::McpScope::User)
                })
                .map(|server| server.name.clone())
                .collect()
        })
        .unwrap_or_default()
}

/// Build the DeepSeek provider from environment configuration.
pub fn build_provider(config: &RuntimeConfig) -> Result<Arc<dyn ModelProvider>, RuntimeError> {
    let mut deepseek = DeepSeekConfig::from_process_environment()
        .map_err(|error| RuntimeError::Provider(error.to_string()))?;
    deepseek = deepseek
        .with_models(known_models())
        .map_err(|error| RuntimeError::Provider(error.to_string()))?;
    if let Some(context_window) = config.context_window {
        deepseek = deepseek
            .with_default_context_window(context_window)
            .map_err(|error| RuntimeError::Provider(error.to_string()))?;
    }
    let provider = DeepSeekProvider::from_environment(deepseek)
        .map_err(|error| RuntimeError::Provider(error.to_string()))?;
    Ok(Arc::new(provider))
}

/// Fixed model catalog used by the TUI picker.
pub fn known_models() -> Vec<DeepSeekModelConfig> {
    [
        ("deepseek-flash", "DeepSeek-V4.1-Flash"),
        ("deepseek-v4-pro", "DeepSeek-V4-Pro"),
        ("deepseek-v4-flash", "DeepSeek-V4-Flash (legacy)"),
        (
            "deepseek-v4-flash-vision-exp",
            "DeepSeek-V4-Flash Vision (legacy)",
        ),
    ]
    .into_iter()
    .map(|(id, name)| {
        DeepSeekModelConfig::new(id)
            .and_then(|model| model.with_name(name))
            .and_then(|model| model.with_context_window(1_000_000))
            .expect("fixed model catalog entries are valid")
    })
    .collect()
}

/// Stable slowcode system prompt with project-context pointers.
pub fn system_prompt(workspace: &std::path::Path) -> String {
    let base = base_system_prompt(workspace);
    let config = slowcode_context::ContextConfig::new(workspace);
    let bundle = slowcode_context::compose_system_prompt(&base, &config);
    format!("{base}{}", bundle.system_suffix)
}

/// Stable system prompt before the glossary suffix is appended.
pub fn base_system_prompt(workspace: &std::path::Path) -> String {
    format!(
        "You are slowcode, a coding agent working inside {}.\n\
         Use only the tools listed in the current request. The listed tools are authoritative: do not assume a tool is available or unavailable because of an earlier turn.\n\
         AGENTS.md and GLOSSARY.md are loaded automatically when present.\n\
         HANDOFF.md is the handoff document. Read it when you need current progress, verified facts, and next steps.\n\
         Keep changes minimal and confined to the workspace. When the task is complete, reply with a short summary of what you changed and what you ran.",
        workspace.display()
    )
}

fn estimate_tokens(text: &str) -> u64 {
    (text.len() / 4) as u64
}

fn schema_tokens(schemas: &[ToolSchema]) -> u64 {
    schemas
        .iter()
        .map(|schema| estimate_tokens(&schema.raw().as_value().to_string()))
        .sum()
}

fn file_tokens(path: &std::path::Path) -> u64 {
    std::fs::metadata(path)
        .map(|metadata| metadata.len() / 4)
        .unwrap_or(0)
}

fn skill_catalog_tokens(workspace: &std::path::Path, role: RoleName) -> u64 {
    let roots = [
        workspace.join(format!(".slowcode/roles/{}/skills", role.as_str())),
        workspace.join(".slowcode/skills"),
        workspace.join(".agents/skills"),
    ];
    let mut seen = std::collections::BTreeSet::new();
    let mut tokens = 0;
    for root in roots {
        let Ok(entries) = std::fs::read_dir(root) else {
            continue;
        };
        for entry in entries.flatten() {
            let path = entry.path();
            if !path.is_dir() {
                continue;
            }
            let name = path
                .file_name()
                .and_then(|name| name.to_str())
                .unwrap_or_default()
                .to_owned();
            if name.is_empty() || !seen.insert(name) {
                continue;
            }
            let Ok(Some(metadata)) =
                slowcode_kernel::skills::read_skill_metadata(&path.join("SKILL.md"))
            else {
                continue;
            };
            if !metadata.model_invocable {
                continue;
            }
            tokens += estimate_tokens(&format!(
                "- `{}`: {}\n",
                metadata.name, metadata.description
            ));
        }
    }
    tokens
}

fn role_config(config: &RuntimeConfig) -> Result<RoleSessionConfig, RuntimeError> {
    let call = LlmCallConfig::new(DEEPSEEK_PROVIDER, &config.model)
        .map_err(|error| RuntimeError::Model(error.to_string()))?;
    let call = apply_max_tokens(call, config.max_tokens)
        .map_err(|error| RuntimeError::Model(error.to_string()))?;
    Ok(RoleSessionConfig {
        workspace: config.workspace.clone(),
        call,
        read_only: config.read_only,
        base_system_prompt: system_prompt(&config.workspace),
    })
}

async fn sync_readonly_shell(
    config: &RuntimeConfig,
    session: &mut RoleSession,
    installed: &mut bool,
) -> Result<bool, RuntimeError> {
    let want = session.role().spec().is_read_only();
    if want == *installed {
        return Ok(false);
    }
    if want {
        let shell = AuditedReadonlyShell::open(
            &config.workspace,
            WorkflowStore::new(&config.workspace),
            None,
        )
        .map_err(|error| RuntimeError::Tools(error.to_string()))?;
        let schema = AuditedReadonlyShell::schema()
            .map_err(|error| RuntimeError::Tools(error.to_string()))?;
        session
            .install_extra_tools(ExtraToolSlot::ReadonlyShell, Arc::new(shell), vec![schema])
            .await?;
    } else {
        session
            .clear_extra_tools(ExtraToolSlot::ReadonlyShell)
            .await?;
    }
    *installed = want;
    Ok(true)
}

pub fn apply_max_tokens(
    call: LlmCallConfig,
    max_tokens: Option<u64>,
) -> Result<LlmCallConfig, serde_json::Error> {
    let Some(max_tokens) = max_tokens else {
        return Ok(call);
    };
    let mut raw = call.raw().as_value().clone();
    if let Some(object) = raw.as_object_mut() {
        object.insert("maxTokens".to_owned(), serde_json::json!(max_tokens));
    }
    serde_json::from_value(raw)
}

async fn open_session(
    store: &SessionStore,
    workspace: &std::path::Path,
    choice: SessionChoice,
) -> Result<(Session, String), RuntimeError> {
    match choice {
        SessionChoice::New => {
            let id = new_session_id();
            let session = store.open_new_session(id.clone(), workspace).await?;
            Ok((session, id.to_string()))
        }
        SessionChoice::Create(value) => {
            let id = validate_session_id(&value)
                .map_err(|error| RuntimeError::InvalidSession(error.to_string()))?;
            let session = store.open_new_session(id.clone(), workspace).await?;
            Ok((session, id.to_string()))
        }
        SessionChoice::Resume(value) => {
            let id = validate_session_id(&value)
                .map_err(|error| RuntimeError::InvalidSession(error.to_string()))?;
            let session = store.resume_session(id.clone(), workspace).await?;
            Ok((session, id.to_string()))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::{SystemTime, UNIX_EPOCH};

    #[test]
    fn system_prompt_includes_glossary_and_handoff_pointer() {
        let workspace = std::env::temp_dir().join(format!(
            "slowcode-app-context-{}-{}",
            std::process::id(),
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&workspace).unwrap();
        std::fs::write(
            workspace.join("GLOSSARY.md"),
            "**Term**: a project-specific meaning\n",
        )
        .unwrap();
        std::fs::write(workspace.join("HANDOFF.md"), "secret progress\n").unwrap();

        let prompt = system_prompt(&workspace);
        assert!(prompt.contains("Domain glossary"));
        assert!(prompt.contains("**Term**: a project-specific meaning"));
        assert!(prompt.contains("HANDOFF.md"));
        assert!(!prompt.contains("secret progress"));

        std::fs::remove_dir_all(workspace).unwrap();
    }

    #[test]
    fn base_prompt_defers_tool_availability_to_the_request() {
        let prompt = base_system_prompt(std::path::Path::new("/work"));
        assert!(prompt.contains("Use only the tools listed in the current request."));
        assert!(!prompt.contains("Use write and edit"));
        assert!(!prompt.contains("Use bash"));
    }

    use std::sync::atomic::{AtomicU64, Ordering};

    use slowcode_kernel::{
        agent::{ToolExecutionFuture, ToolExecutionRequest, ToolExecutorError, ToolShutdownFuture},
        model::JsonValue,
        provider::{
            PreparedProviderCall, PreparedRequestPreflight, ProviderPreflightError,
            ProviderPrepareError, ProviderRequest, ProviderRequestDraft, ProviderStream,
        },
    };

    /// Env-mutating runtime tests run one at a time in this binary.
    static HOME_LOCK: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

    struct PanicProvider;

    impl ModelProvider for PanicProvider {
        fn prepare_call(
            &self,
            _config: LlmCallConfig,
        ) -> Result<PreparedProviderCall, ProviderPrepareError> {
            unreachable!("runtime lifecycle tests never reach the provider")
        }

        fn preflight_request(
            &self,
            _draft: ProviderRequestDraft<'_>,
        ) -> Result<PreparedRequestPreflight, ProviderPreflightError> {
            unreachable!("runtime lifecycle tests never reach the provider")
        }

        fn stream(
            &self,
            _request: ProviderRequest,
            _cancellation: CancellationToken,
        ) -> ProviderStream {
            unreachable!("runtime lifecycle tests never reach the provider")
        }
    }

    struct CountingTool {
        shutdowns: Arc<AtomicU64>,
    }

    impl ToolExecutor for CountingTool {
        fn execute(
            &self,
            _request: ToolExecutionRequest,
            _cancellation: CancellationToken,
        ) -> ToolExecutionFuture<'_> {
            Box::pin(async { Err(ToolExecutorError::new("unused")) })
        }

        fn shutdown(&self) -> ToolShutdownFuture<'_> {
            self.shutdowns.fetch_add(1, Ordering::SeqCst);
            Box::pin(async { Ok(()) })
        }
    }

    fn temp_dir(label: &str) -> PathBuf {
        let path = std::env::temp_dir().join(format!(
            "slowcode-runtime-{label}-{}-{}",
            std::process::id(),
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&path).unwrap();
        path
    }

    fn mcp_schema(name: &str) -> ToolSchema {
        ToolSchema::new(
            name,
            "test tool",
            JsonValue::new(serde_json::json!({"type": "object"})).unwrap(),
        )
        .unwrap()
    }

    async fn runtime_with_temp_home(label: &str) -> (Runtime, PathBuf, PathBuf) {
        let home = temp_dir(&format!("{label}-home"));
        let workspace = temp_dir(&format!("{label}-workspace"));
        let previous = std::env::var_os("HOME");
        // SAFETY: this test holds HOME_LOCK and is the only env mutator here.
        unsafe {
            std::env::set_var("HOME", &home);
        }
        let started = Runtime::start_with(
            RuntimeConfig::new(workspace.clone()),
            SessionChoice::New,
            Arc::new(PanicProvider),
        )
        .await
        .expect("runtime starts");
        if let Some(previous) = previous {
            // SAFETY: same lock and statement scope as the set above.
            unsafe {
                std::env::set_var("HOME", previous);
            }
        }
        (started.runtime, home, workspace)
    }

    #[tokio::test]
    async fn runtime_shutdown_closes_mcp_once_and_new_session_keeps_it() {
        let _guard = HOME_LOCK.lock().await;
        let (mut runtime, home, workspace) = runtime_with_temp_home("shutdown").await;
        let shutdowns = Arc::new(AtomicU64::new(0));
        runtime
            .install_extra_tools(
                ExtraToolSlot::Mcp,
                Arc::new(CountingTool {
                    shutdowns: Arc::clone(&shutdowns),
                }),
                vec![mcp_schema("mcp__probe__x")],
            )
            .await
            .expect("mcp installs");

        runtime.new_session().await.expect("new session opens");
        assert_eq!(shutdowns.load(Ordering::SeqCst), 0);
        assert!(runtime.installed_slots().iter().any(|slot| slot == "mcp"));

        runtime.shutdown().await.expect("runtime shuts down");
        assert_eq!(shutdowns.load(Ordering::SeqCst), 1);
        let _ = std::fs::remove_dir_all(home);
        let _ = std::fs::remove_dir_all(workspace);
    }

    #[tokio::test]
    async fn reinstalling_mcp_closes_the_previous_executor_once() {
        let _guard = HOME_LOCK.lock().await;
        let (mut runtime, home, workspace) = runtime_with_temp_home("reinstall").await;
        let first = Arc::new(AtomicU64::new(0));
        let second = Arc::new(AtomicU64::new(0));
        runtime
            .install_extra_tools(
                ExtraToolSlot::Mcp,
                Arc::new(CountingTool {
                    shutdowns: Arc::clone(&first),
                }),
                vec![mcp_schema("mcp__probe__one")],
            )
            .await
            .expect("first mcp installs");
        runtime
            .install_extra_tools(
                ExtraToolSlot::Mcp,
                Arc::new(CountingTool {
                    shutdowns: Arc::clone(&second),
                }),
                vec![mcp_schema("mcp__probe__two")],
            )
            .await
            .expect("second mcp installs");

        assert_eq!(first.load(Ordering::SeqCst), 1);
        assert_eq!(second.load(Ordering::SeqCst), 0);
        runtime.shutdown().await.expect("runtime shuts down");
        assert_eq!(second.load(Ordering::SeqCst), 1);
        let _ = std::fs::remove_dir_all(home);
        let _ = std::fs::remove_dir_all(workspace);
    }

    #[tokio::test]
    async fn model_rename_and_context_breakdown_report_current_state() {
        let _guard = HOME_LOCK.lock().await;
        let (mut runtime, home, workspace) = runtime_with_temp_home("state").await;
        let model = runtime.model().to_owned();
        runtime.set_model(&model).await.expect("model is accepted");
        assert_eq!(runtime.model(), model);

        let renamed = runtime
            .rename_session("renamed by test")
            .await
            .expect("rename succeeds");
        assert_eq!(renamed.as_deref(), Some("renamed by test"));

        let breakdown = runtime.context_breakdown();
        let system = breakdown
            .slices
            .iter()
            .find(|slice| slice.name == "System prompt")
            .expect("system prompt slice");
        assert!(system.tokens > 0);
        assert!(breakdown.slices.iter().any(|slice| slice.tokens > 0));
        let _ = std::fs::remove_dir_all(home);
        let _ = std::fs::remove_dir_all(workspace);
    }

    #[tokio::test]
    async fn switching_sessions_restores_the_previous_id() {
        let _guard = HOME_LOCK.lock().await;
        let (mut runtime, home, workspace) = runtime_with_temp_home("sessions").await;
        let first = runtime.session_id().to_owned();
        runtime.new_session().await.expect("second session opens");
        assert_ne!(runtime.session_id(), first);

        runtime
            .switch_session(&first)
            .await
            .expect("first session resumes");
        assert_eq!(runtime.session_id(), first);
        let _ = std::fs::remove_dir_all(home);
        let _ = std::fs::remove_dir_all(workspace);
    }

    #[tokio::test]
    async fn role_switch_and_slot_cleanup_update_runtime_state() {
        let _guard = HOME_LOCK.lock().await;
        let (mut runtime, home, workspace) = runtime_with_temp_home("roles").await;
        let report = runtime
            .switch_role(RoleName::Navigator)
            .await
            .expect("navigator switch");
        assert_eq!(report.to, RoleName::Navigator);
        assert_eq!(runtime.role(), Some(RoleName::Navigator));

        let shutdowns = Arc::new(AtomicU64::new(0));
        runtime
            .install_extra_tools(
                ExtraToolSlot::ReadonlyShell,
                Arc::new(CountingTool {
                    shutdowns: Arc::clone(&shutdowns),
                }),
                vec![mcp_schema("readonly_shell")],
            )
            .await
            .expect("slot installs");
        runtime
            .clear_extra_tools(ExtraToolSlot::ReadonlyShell)
            .await
            .expect("slot clears");
        assert!(
            !runtime
                .installed_slots()
                .iter()
                .any(|slot| slot == "readonly_shell")
        );
        assert_eq!(shutdowns.load(Ordering::SeqCst), 1);
        let _ = std::fs::remove_dir_all(home);
        let _ = std::fs::remove_dir_all(workspace);
    }

    #[tokio::test]
    async fn installing_mcp_without_config_reports_no_servers() {
        let _guard = HOME_LOCK.lock().await;
        let (mut runtime, home, workspace) = runtime_with_temp_home("mcp-empty").await;
        let report = runtime.install_mcp_tools().await.expect("empty install");
        assert_eq!(report.servers, 0);
        assert_eq!(report.tools, 0);
        assert!(report.errors.is_empty(), "{:?}", report.errors);
        let _ = std::fs::remove_dir_all(home);
        let _ = std::fs::remove_dir_all(workspace);
    }

    #[test]
    fn skill_catalog_tokens_count_model_invocable_skills() {
        let workspace = temp_dir("skills");
        assert_eq!(skill_catalog_tokens(&workspace, RoleName::Driver), 0);

        let skill = workspace.join(".slowcode/skills/demo");
        std::fs::create_dir_all(&skill).unwrap();
        std::fs::write(
            skill.join("SKILL.md"),
            "---\nname: demo\ndescription: demo skill\n---\nBody.\n",
        )
        .unwrap();
        assert!(skill_catalog_tokens(&workspace, RoleName::Driver) > 0);
        let _ = std::fs::remove_dir_all(&workspace);
    }
}
