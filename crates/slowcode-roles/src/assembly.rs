//! Agent assembly for one active role.
//!
//! Role switching happens at turn boundaries by releasing the old agent,
//! rebuilding it around the same [`Session`], and disposing the old fiber.
//! Conversation state lives in the Session, so only loop-local transient state
//! is reset.

use std::{collections::BTreeMap, path::PathBuf, sync::Arc};

use slowcode_kernel::{
    agent::{
        AgentBuildError, AgentLoop, AgentLoopError, ApprovalProvider, FileChangePolicy,
        NoApprovalProvider, ShellPolicy, ToolExecutor, ToolExecutorError, TurnOutcome,
        TurnProposal,
    },
    capability::CapabilityError,
    model::{ContentBlock, LlmCallConfig, Message, MessageSource, ModelError, ToolSchema},
    provider::ModelProvider,
    session::{Session, SessionError, SessionIoError},
    skill_roots::{SkillRootError, SkillRoots},
    tools::{ToolRegistryBuildError, readonly_shell::READONLY_SHELL_TOOL_NAME},
    workspace_instructions::WorkspaceInstructionRuntime,
};
use thiserror::Error;
use tokio_util::sync::CancellationToken;

use crate::{RoleFiber, RoleFiberError, RoleName, agent::ToolOwnership};

/// One independent extra tool set slot.
///
/// MCP is a system resource shared across role assemblies. The read-only shell
/// belongs to the current role assembly and closes with it.
#[derive(Clone, Copy, Debug, Eq, Ord, PartialEq, PartialOrd)]
pub enum ExtraToolSlot {
    Mcp,
    ReadonlyShell,
}

impl ExtraToolSlot {
    /// Stable wire name used in status output and tests.
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Mcp => "mcp",
            Self::ReadonlyShell => "readonly_shell",
        }
    }

    /// Who closes this slot when the agent is released.
    #[must_use]
    pub fn ownership(self) -> ToolOwnership {
        match self {
            Self::Mcp => ToolOwnership::Shared,
            Self::ReadonlyShell => ToolOwnership::Owned,
        }
    }
}

/// Static assembly inputs that survive a role switch.
#[derive(Clone, Debug)]
pub struct RoleSessionConfig {
    pub workspace: PathBuf,
    pub call: LlmCallConfig,
    pub read_only: bool,
    pub base_system_prompt: String,
}

/// One live role, its Cordis fiber, and the agent loop built around them.
pub struct RoleSession {
    config: RoleSessionConfig,
    provider: Arc<dyn ModelProvider>,
    role: RoleName,
    fiber: Option<RoleFiber>,
    agent: Option<AgentLoop>,
    workspace_instructions: Option<WorkspaceInstructionRuntime>,
    approval_provider: Option<Arc<dyn ApprovalProvider>>,
    approval_required: bool,
    extra_tools: BTreeMap<ExtraToolSlot, ExtraTools>,
    pending_role_notice: Option<String>,
}

impl std::fmt::Debug for RoleSession {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("RoleSession")
            .field("role", &self.role)
            .field(
                "session_id",
                &self.agent.as_ref().map(|agent| agent.session().id()),
            )
            .field("agent_ready", &self.agent.is_some())
            .finish_non_exhaustive()
    }
}

impl RoleSession {
    /// Spawn the initial role fiber and build the first agent loop.
    pub async fn new(
        config: RoleSessionConfig,
        provider: Arc<dyn ModelProvider>,
        initial_role: RoleName,
    ) -> Result<Self, RoleSessionError> {
        let session = Session::new("slowcode-cli")?;
        Self::new_with_session(config, provider, initial_role, session).await
    }

    /// Spawn the initial role fiber around a caller-owned Session.
    ///
    /// The CLI uses this to pass a durable Session opened through
    /// `SessionStore` instead of an in-memory one.
    pub async fn new_with_session(
        config: RoleSessionConfig,
        provider: Arc<dyn ModelProvider>,
        initial_role: RoleName,
        session: Session,
    ) -> Result<Self, RoleSessionError> {
        let fiber = RoleFiber::spawn(Arc::new(initial_role.spec())).await?;
        let no_extra_tools = BTreeMap::new();
        let agent = match build_agent(
            &config,
            Arc::clone(&provider),
            session,
            &fiber,
            &no_extra_tools,
        )
        .await
        {
            Ok(agent) => agent,
            Err(error) => {
                let _ = fiber.dispose().await;
                return Err(error);
            }
        };
        Ok(Self {
            config,
            provider,
            role: initial_role,
            fiber: Some(fiber),
            agent: Some(agent),
            workspace_instructions: None,
            approval_provider: None,
            approval_required: false,
            extra_tools: BTreeMap::new(),
            pending_role_notice: None,
        })
    }

    /// The currently active role.
    #[must_use]
    pub fn role(&self) -> RoleName {
        self.role
    }

    /// The durable session shared across every role.
    #[must_use]
    pub fn session(&self) -> &Session {
        self.agent
            .as_ref()
            .expect("role session owns an agent while idle")
            .session()
    }

    /// Flush committed session events to the durable journal.
    ///
    /// Callers take a feedback snapshot after this returns so the bundle never
    /// omits an event that was already committed in memory.
    pub async fn flush_journal(&mut self) -> Result<(), RoleSessionError> {
        let agent = self
            .agent
            .as_mut()
            .ok_or(RoleSessionError::AgentUnavailable)?;
        agent
            .session_mut()
            .flush_barrier()
            .await
            .map_err(|error| RoleSessionError::Journal(error.to_string()))
    }

    /// The provider handle used by plan commands in the REPL.
    #[must_use]
    pub fn provider(&self) -> Arc<dyn ModelProvider> {
        Arc::clone(&self.provider)
    }

    /// Rename the current durable session.
    pub async fn rename_session(
        &mut self,
        title: &str,
    ) -> Result<Option<String>, RoleSessionError> {
        let agent = self
            .agent
            .as_mut()
            .ok_or(RoleSessionError::AgentUnavailable)?;
        Ok(agent.rename_session_title(title).await?)
    }

    /// Install workspace instruction discovery for the active agent and every
    /// future role rebuild.
    pub fn install_workspace_instructions(&mut self, runtime: WorkspaceInstructionRuntime) {
        if let Some(agent) = self.agent.as_mut() {
            agent.install_workspace_instruction_runtime(runtime.clone());
        }
        self.workspace_instructions = Some(runtime);
    }

    /// Install the approval provider used by the active agent and every future
    /// role rebuild.
    pub fn install_approval_provider(&mut self, provider: Arc<dyn ApprovalProvider>) {
        self.approval_required = true;
        let role = self.role;
        let read_only = self.config.read_only;
        if let Some(agent) = self.agent.as_mut() {
            agent.install_approval_provider(Arc::clone(&provider));
            if !read_only && !role.spec().is_read_only() {
                agent.install_approval_policy(FileChangePolicy::Ask, ShellPolicy::Ask);
            }
        }
        self.approval_provider = Some(provider);
    }

    /// Install a provider for extension tools without enabling file and shell
    /// prompts.
    pub fn install_approval_provider_passive(&mut self, provider: Arc<dyn ApprovalProvider>) {
        if let Some(agent) = self.agent.as_mut() {
            agent.install_approval_provider(Arc::clone(&provider));
        }
        self.approval_provider = Some(provider);
    }

    /// Install one extra tool set into its slot and rebuild the agent around
    /// the same Session. Other slots are untouched.
    pub async fn install_extra_tools(
        &mut self,
        slot: ExtraToolSlot,
        tools: Arc<dyn ToolExecutor>,
        schemas: Vec<ToolSchema>,
    ) -> Result<(), RoleSessionError> {
        self.ensure_idle()?;
        check_reserved_names(slot, &schemas)?;
        let previous = self.extra_tools.insert(slot, ExtraTools { tools, schemas });
        let parts = match self.prepare_active_parts() {
            Ok(parts) => parts,
            Err(error) => {
                restore_slot(&mut self.extra_tools, slot, previous);
                return Err(error);
            }
        };
        self.swap_parts(parts, move |session| {
            restore_slot(&mut session.extra_tools, slot, previous);
        })
        .await
    }

    /// Remove one slot and rebuild the current agent. Other slots stay.
    pub async fn clear_extra_tools(&mut self, slot: ExtraToolSlot) -> Result<(), RoleSessionError> {
        self.ensure_idle()?;
        let Some(previous) = self.extra_tools.remove(&slot) else {
            return Ok(());
        };
        let parts = match self.prepare_active_parts() {
            Ok(parts) => parts,
            Err(error) => {
                self.extra_tools.insert(slot, previous);
                return Err(error);
            }
        };
        self.swap_parts(parts, move |session| {
            session.extra_tools.insert(slot, previous);
        })
        .await
    }

    /// Run one turn in the current role.
    pub async fn run_turn(
        &mut self,
        message: Message,
        cancellation: CancellationToken,
    ) -> Result<TurnOutcome, RoleSessionError> {
        let notice = self.pending_role_notice.take();
        let agent = self
            .agent
            .as_mut()
            .ok_or(RoleSessionError::AgentUnavailable)?;
        let mut messages = Vec::with_capacity(if notice.is_some() { 2 } else { 1 });
        if let Some(text) = &notice {
            messages.push(role_notice_message(
                agent.session().messages().len(),
                text.clone(),
            )?);
        }
        messages.push(message);
        match agent
            .run_turn(TurnProposal::Enter(messages), cancellation)
            .await
        {
            Ok(outcome) => Ok(outcome),
            Err(error) => {
                // The provider may fail before the notice is committed. Keep it
                // pending so the next turn still explains the role change.
                self.pending_role_notice = notice;
                Err(error.into())
            }
        }
    }

    /// Switch roles at a turn boundary while keeping the same Session.
    ///
    /// Returns `false` when the requested role is already active.
    pub async fn switch(&mut self, target: RoleName) -> Result<bool, RoleSessionError> {
        if target == self.role {
            return Ok(false);
        }
        self.ensure_idle()?;

        let new_fiber = RoleFiber::spawn(Arc::new(target.spec())).await?;
        let parts = match prepare_agent_parts(&self.config, &new_fiber, &self.extra_tools) {
            Ok(parts) => parts,
            Err(error) => {
                let _ = new_fiber.dispose().await;
                return Err(error);
            }
        };

        let session = match self.release_or_restore().await {
            Ok(session) => session,
            Err(error) => {
                let _ = new_fiber.dispose().await;
                return Err(error);
            }
        };

        let mut new_agent =
            match assemble_agent(&self.config, Arc::clone(&self.provider), session, parts).await {
                Ok(agent) => agent,
                Err(failure) => {
                    if let Some(session) = failure.session {
                        self.restore_agent(*session).await?;
                    }
                    let _ = new_fiber.dispose().await;
                    return Err(failure.error);
                }
            };
        self.install_runtime_settings(&mut new_agent, target);
        self.promote_role(new_fiber, target, new_agent).await
    }

    /// Release the active agent and lend its Session to the caller.
    ///
    /// The caller must return the Session through [`Self::restore_session`]
    /// before running another role turn. This seam exists for command-scoped
    /// agents such as Plan commands that need a different system prompt and
    /// tool registry but must keep the user's conversation transcript.
    pub async fn take_session(&mut self) -> Result<Session, RoleSessionError> {
        self.ensure_idle()?;
        self.release_or_restore().await
    }

    /// Rebuild the active role around a Session returned by [`Self::take_session`].
    pub async fn restore_session(&mut self, session: Session) -> Result<(), RoleSessionError> {
        self.restore_agent(session).await
    }

    /// Replace the model call configuration while keeping the same Session
    /// and rebuilding the agent around the active role.
    pub async fn set_call(&mut self, call: LlmCallConfig) -> Result<(), RoleSessionError> {
        self.ensure_idle()?;
        let previous = self.config.call.clone();
        self.config.call = call;
        let parts = match self.prepare_active_parts() {
            Ok(parts) => parts,
            Err(error) => {
                self.config.call = previous;
                return Err(error);
            }
        };
        self.swap_parts(parts, move |session| session.config.call = previous)
            .await
    }

    /// Stop the agent's tools, drop the Session, and dispose the active fiber.
    pub async fn shutdown(mut self) -> Result<(), RoleSessionError> {
        if let Some(agent) = self.agent.take() {
            let mut session = agent
                .shutdown_into_session()
                .await
                .map_err(|error| RoleSessionError::Release(Box::new(error)))?;
            session
                .shutdown()
                .await
                .map_err(RoleSessionError::SessionShutdown)?;
        }
        if let Some(fiber) = self.fiber.take() {
            fiber.dispose().await?;
        }
        Ok(())
    }

    fn ensure_idle(&self) -> Result<(), RoleSessionError> {
        if self.session().state().open_turn().is_some() {
            return Err(RoleSessionError::SessionNotIdle);
        }
        Ok(())
    }

    /// Prepare assembly inputs for the active role without releasing its agent.
    fn prepare_active_parts(&self) -> Result<crate::agent::PreparedAgent, RoleSessionError> {
        let fiber = self
            .fiber
            .as_ref()
            .ok_or(RoleSessionError::AgentUnavailable)?;
        prepare_agent_parts(&self.config, fiber, &self.extra_tools)
    }

    /// Release the active agent and return its Session.
    async fn release_agent(&mut self) -> Result<Session, ReleaseFailure> {
        let Some(agent) = self.agent.take() else {
            return Err(ReleaseFailure::Unavailable);
        };
        match agent.shutdown_into_session().await {
            Ok(session) => Ok(session),
            Err(error) => {
                let (source, session) = error.into_parts();
                Err(ReleaseFailure::Shutdown {
                    error: RoleSessionError::ToolShutdown(source),
                    session: Box::new(session),
                })
            }
        }
    }

    /// Release the agent and rebuild the same role when cleanup half-fails.
    async fn release_or_restore(&mut self) -> Result<Session, RoleSessionError> {
        match self.release_agent().await {
            Ok(session) => Ok(session),
            Err(ReleaseFailure::Unavailable) => Err(RoleSessionError::AgentUnavailable),
            Err(ReleaseFailure::Shutdown { error, session }) => {
                self.restore_agent(*session).await?;
                Err(error)
            }
        }
    }

    /// Replace the active agent with a pre-built catalogue around the same
    /// Session. On any failure the old agent is rebuilt from the recovered
    /// Session after `rollback` restores the previous configuration.
    async fn swap_parts<F>(
        &mut self,
        parts: crate::agent::PreparedAgent,
        rollback: F,
    ) -> Result<(), RoleSessionError>
    where
        F: FnOnce(&mut Self),
    {
        let session = match self.release_agent().await {
            Ok(session) => session,
            Err(ReleaseFailure::Unavailable) => return Err(RoleSessionError::AgentUnavailable),
            Err(ReleaseFailure::Shutdown { error, session }) => {
                rollback(self);
                self.restore_agent(*session).await?;
                return Err(error);
            }
        };
        let mut agent =
            match assemble_agent(&self.config, Arc::clone(&self.provider), session, parts).await {
                Ok(agent) => agent,
                Err(failure) => {
                    rollback(self);
                    if let Some(session) = failure.session {
                        self.restore_agent(*session).await?;
                    }
                    return Err(failure.error);
                }
            };
        self.install_runtime_settings(&mut agent, self.role);
        self.agent = Some(agent);
        Ok(())
    }

    async fn restore_agent(&mut self, session: Session) -> Result<(), RoleSessionError> {
        let fiber = self
            .fiber
            .as_ref()
            .ok_or(RoleSessionError::AgentUnavailable)?;
        let agent = build_agent(
            &self.config,
            Arc::clone(&self.provider),
            session,
            fiber,
            &self.extra_tools,
        )
        .await?;
        let mut agent = agent;
        self.install_runtime_settings(&mut agent, self.role);
        self.agent = Some(agent);
        Ok(())
    }

    async fn promote_role(
        &mut self,
        fiber: RoleFiber,
        target: RoleName,
        agent: AgentLoop,
    ) -> Result<bool, RoleSessionError> {
        let old_fiber = self.fiber.replace(fiber);
        self.role = target;
        self.agent = Some(agent);
        self.pending_role_notice = Some(role_transition_notice(
            target,
            self.config.read_only || target.spec().is_read_only(),
        ));
        if let Some(old_fiber) = old_fiber {
            old_fiber.dispose().await?;
        }
        Ok(true)
    }

    fn install_runtime_settings(&self, agent: &mut AgentLoop, role: RoleName) {
        if let Some(runtime) = &self.workspace_instructions {
            agent.install_workspace_instruction_runtime(runtime.clone());
        }
        if let Some(provider) = &self.approval_provider {
            agent.install_approval_provider(Arc::clone(provider));
            self.apply_approval_policy(agent, role);
        }
    }

    fn apply_approval_policy(&self, agent: &mut AgentLoop, role: RoleName) {
        if self.approval_required && !self.config.read_only && !role.spec().is_read_only() {
            agent.install_approval_policy(FileChangePolicy::Ask, ShellPolicy::Ask);
        }
    }
}

struct ExtraTools {
    tools: Arc<dyn ToolExecutor>,
    schemas: Vec<ToolSchema>,
}

fn check_reserved_names(
    slot: ExtraToolSlot,
    schemas: &[ToolSchema],
) -> Result<(), RoleSessionError> {
    if slot == ExtraToolSlot::ReadonlyShell {
        return Ok(());
    }
    if let Some(schema) = schemas
        .iter()
        .find(|schema| schema.name() == READONLY_SHELL_TOOL_NAME)
    {
        return Err(RoleSessionError::ReservedToolName(schema.name().to_owned()));
    }
    Ok(())
}

fn restore_slot(
    slots: &mut BTreeMap<ExtraToolSlot, ExtraTools>,
    slot: ExtraToolSlot,
    previous: Option<ExtraTools>,
) {
    match previous {
        Some(previous) => {
            slots.insert(slot, previous);
        }
        None => {
            slots.remove(&slot);
        }
    }
}

/// Why the active agent could not hand back its Session.
enum ReleaseFailure {
    /// No agent was active.
    Unavailable,
    /// Tool cleanup failed but the Session was recovered.
    Shutdown {
        error: RoleSessionError,
        session: Box<Session>,
    },
}

/// Build one agent loop from the active fiber and the workspace policy.
async fn build_agent(
    config: &RoleSessionConfig,
    provider: Arc<dyn ModelProvider>,
    session: Session,
    fiber: &RoleFiber,
    extra_tools: &BTreeMap<ExtraToolSlot, ExtraTools>,
) -> Result<AgentLoop, RoleSessionError> {
    let parts = prepare_agent_parts(config, fiber, extra_tools)?;
    let (agent, _tools) = crate::agent::finish(parts, session, provider)
        .await
        .map_err(|failure| RoleSessionError::Builder(failure.error))?;
    Ok(agent)
}

/// A replacement agent that could not be fully assembled.
///
/// `session` is present when the failure left the agent intact and it can be
/// shut down to recover the conversation. `AgentLoop::new` does not return the
/// Session, so that failure cannot be recovered without a kernel change.
struct AgentAssemblyError {
    error: RoleSessionError,
    session: Option<Box<Session>>,
}

/// Build the replacement agent from pre-built parts and install the Skill
/// runtime. A Skill failure still owns the Session and hands it back.
async fn assemble_agent(
    _config: &RoleSessionConfig,
    provider: Arc<dyn ModelProvider>,
    session: Session,
    parts: crate::agent::PreparedAgent,
) -> Result<AgentLoop, AgentAssemblyError> {
    match crate::agent::finish(parts, session, provider).await {
        Ok((agent, _tools)) => Ok(agent),
        Err(failure) => {
            // `AgentLoop::new` consumes the Session when it fails, so the old
            // agent cannot be rebuilt. Skill failures still hand the Session
            // back and callers restore the old agent.
            let error = if failure.session.is_none() {
                RoleSessionError::ReassemblyFailed(failure.error)
            } else {
                RoleSessionError::Builder(failure.error)
            };
            Err(AgentAssemblyError {
                error,
                session: failure.session,
            })
        }
    }
}

/// Build everything that does not need the Session, so a switch can fail
/// before the old agent releases it.
fn prepare_agent_parts(
    config: &RoleSessionConfig,
    fiber: &RoleFiber,
    extra_tools: &BTreeMap<ExtraToolSlot, ExtraTools>,
) -> Result<crate::agent::PreparedAgent, RoleSessionError> {
    let bundle = fiber.bundle()?;
    let capabilities = bundle.capabilities()?;
    let skill_roots = SkillRoots::new(capabilities.skill_roots().iter().cloned())?;
    let read_only = bundle.read_only() || config.read_only;
    let system = compose_system(&config.base_system_prompt, capabilities.system_prompt());
    let registry = if read_only {
        crate::agent::RegistryMode::ReadOnly
    } else {
        crate::agent::RegistryMode::Local
    };
    let mut spec = crate::agent::AgentSpec::new(
        config.workspace.clone(),
        config.call.clone(),
        system,
        registry,
    );
    spec.skill_roots = skill_roots;
    spec.read_only = read_only;
    spec.file_change = FileChangePolicy::Allow;
    spec.shell = ShellPolicy::Allow;
    spec.approval = Arc::new(NoApprovalProvider);
    spec.extra_tools = Vec::new();
    if !read_only && !capabilities.schemas().is_empty() {
        spec.extra_tools.push(crate::agent::ExtraToolSet::shared(
            capabilities.executor(),
            capabilities.schemas().to_vec(),
        ));
    }
    for (slot, extra) in extra_tools {
        let set = match slot.ownership() {
            ToolOwnership::Owned => {
                crate::agent::ExtraToolSet::new(Arc::clone(&extra.tools), extra.schemas.clone())
            }
            ToolOwnership::Shared => {
                crate::agent::ExtraToolSet::shared(Arc::clone(&extra.tools), extra.schemas.clone())
            }
        };
        spec.extra_tools.push(set);
    }
    crate::agent::prepare(spec).map_err(|error| match error {
        crate::agent::AgentBuildError::DuplicateTool(name) => {
            RoleSessionError::DuplicateToolName(name)
        }
        other => RoleSessionError::Builder(other),
    })
}

/// Stable prefix first, role fragment second.
fn compose_system(base: &str, role: Option<&str>) -> String {
    let base = base.trim();
    let role = role.unwrap_or_default().trim();
    match (base.is_empty(), role.is_empty()) {
        (true, true) => String::new(),
        (true, false) => role.to_owned(),
        (false, true) => base.to_owned(),
        (false, false) => format!("{base}\n\n{role}"),
    }
}

fn role_transition_notice(role: RoleName, read_only: bool) -> String {
    let tools = if read_only {
        "read-only inspection tools; write and shell tools are unavailable in this role"
    } else {
        "read, file-change, and shell tools when the current request lists them"
    };
    format!(
        "Role transition: the active role is now {role}. Tool set for this role: {tools}. \
         Use only the tools listed in the current request. Statements in earlier messages about \
         which tools exist describe a previous role and must not be repeated as if they were mistakes."
    )
}

fn role_notice_message(sequence: usize, text: String) -> Result<Message, RoleSessionError> {
    let content = ContentBlock::text(text)?;
    let source = MessageSource::plugin_notice("roles", "role transition")?;
    Ok(Message::user(
        format!("role-notice-{sequence}"),
        vec![content],
        source,
    )?)
}

/// Settle both executors and report the first failure in call order, even when
/// both fail.
#[cfg(test)]
fn merge_shutdown_results(
    first: Result<(), ToolExecutorError>,
    second: Result<(), ToolExecutorError>,
) -> Result<(), ToolExecutorError> {
    match (first, second) {
        (Err(error), _) => Err(error),
        (_, Err(error)) => Err(error),
        (Ok(()), Ok(())) => Ok(()),
    }
}

/// Role session assembly failure.
#[derive(Debug, Error)]
pub enum RoleSessionError {
    #[error("the session has an open turn; switch roles at a turn boundary")]
    SessionNotIdle,
    #[error("the role session has no active agent")]
    AgentUnavailable,
    #[error("the old role tools did not shut down: {0}")]
    ToolShutdown(#[source] ToolExecutorError),
    #[error("tool {0} is contributed by both the role and the workspace registry")]
    DuplicateToolName(String),
    #[error("tool {0} uses a reserved system tool name")]
    ReservedToolName(String),
    #[error(transparent)]
    Fiber(#[from] RoleFiberError),
    #[error(transparent)]
    Capability(#[from] CapabilityError),
    #[error(transparent)]
    Registry(#[from] ToolRegistryBuildError),
    #[error(transparent)]
    Build(#[from] AgentBuildError),
    #[error(transparent)]
    Builder(#[from] crate::agent::AgentBuildError),
    #[error(
        "the previous agent was released and the new role could not be assembled; the old agent was not restored: {0}"
    )]
    ReassemblyFailed(#[source] crate::agent::AgentBuildError),
    #[error(transparent)]
    Session(#[from] SessionError),
    #[error("the session writer did not close cleanly: {0}")]
    SessionShutdown(#[source] SessionIoError),
    #[error("the session journal could not be flushed: {0}")]
    Journal(String),
    #[error(transparent)]
    SkillRoot(#[from] SkillRootError),
    #[error("the skill runtime could not be opened: {0}")]
    SkillRuntime(#[source] std::io::Error),
    #[error(transparent)]
    Agent(#[from] AgentLoopError),
    #[error("the old agent tools did not shut down cleanly: {0}")]
    Release(#[source] Box<slowcode_kernel::agent::AgentReleaseError>),
    #[error(transparent)]
    Model(#[from] ModelError),
}

#[cfg(test)]
mod tests {
    use std::{
        collections::{BTreeSet, VecDeque},
        fs,
        path::PathBuf,
        sync::{Arc, Mutex},
        time::{SystemTime, UNIX_EPOCH},
    };

    use futures_util::stream;
    use serde_json::json;
    use slowcode_kernel::agent::{
        MAX_AGENT_FIXED_REQUEST_BYTES, ToolExecutionFuture, ToolExecutionRequest,
        ToolExecutionResult, ToolExecutor, ToolExecutorError, ToolShutdownFuture, TurnProposal,
    };
    use slowcode_kernel::capability::CapabilityTool;
    use slowcode_kernel::model::{
        ContentBlock, FinishReason, JsonValue, LlmCallConfig, LlmCallConfigAdapterDefaults,
        Message, MessageSource, NonNegativeSafeInteger, StreamChunk, TokenUsage, ToolSchema,
    };
    use slowcode_kernel::provider::{
        ModelProvider, PreparedProviderCall, PreparedRequestPreflight, ProviderPreflightError,
        ProviderPrepareError, ProviderRequest, ProviderRequestDraft, ProviderStream,
        ProviderStreamError,
    };
    use slowcode_kernel::session::{
        EventKind, NewEvent, Session, StepId, SurfaceIntent, TurnEndReason, TurnId,
    };
    use slowcode_kernel::tools::readonly_shell::READONLY_SHELL_TOOL_NAME;
    use tokio_util::sync::CancellationToken;

    use super::{
        ExtraToolSlot, ExtraTools, RoleSession, RoleSessionConfig, RoleSessionError,
        assemble_agent, compose_system, merge_shutdown_results, prepare_agent_parts,
        role_transition_notice,
    };
    use crate::agent::CompositeTools;
    use crate::{RoleFiber, RoleFiberError, RoleName};

    struct StubExecutor {
        label: &'static str,
        order: Arc<Mutex<Vec<&'static str>>>,
        fail_shutdown: bool,
    }

    impl ToolExecutor for StubExecutor {
        fn execute(
            &self,
            _request: ToolExecutionRequest,
            _cancellation: CancellationToken,
        ) -> ToolExecutionFuture<'_> {
            Box::pin(async { Err(ToolExecutorError::new("not used")) })
        }

        fn shutdown(&self) -> ToolShutdownFuture<'_> {
            let order = Arc::clone(&self.order);
            let label = self.label;
            let fail_shutdown = self.fail_shutdown;
            Box::pin(async move {
                order.lock().expect("order lock").push(label);
                if fail_shutdown {
                    return Err(ToolExecutorError::new("shutdown failed"));
                }
                Ok(())
            })
        }
    }

    /// A provider that fails loudly if a test ever reaches the model boundary.
    /// The failure tests below only need assembly and turn admission, which a
    /// cancelled turn settles without calling the provider.
    struct PanicProvider;

    impl ModelProvider for PanicProvider {
        fn prepare_call(
            &self,
            _config: LlmCallConfig,
        ) -> Result<PreparedProviderCall, ProviderPrepareError> {
            unreachable!("the behaviour tests never reach the provider")
        }

        fn preflight_request(
            &self,
            _draft: ProviderRequestDraft<'_>,
        ) -> Result<PreparedRequestPreflight, ProviderPreflightError> {
            unreachable!("the behaviour tests never reach the provider")
        }

        fn stream(
            &self,
            _request: ProviderRequest,
            _cancellation: CancellationToken,
        ) -> ProviderStream {
            unreachable!("the behaviour tests never reach the provider")
        }
    }

    fn temp_workspace() -> PathBuf {
        let nonce = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("clock is after epoch")
            .as_nanos();
        let path =
            std::env::temp_dir().join(format!("slowcode-roles-{}-{nonce}", std::process::id()));
        fs::create_dir_all(&path).expect("temp workspace is created");
        fs::write(path.join("hello.txt"), "hello\n").expect("fixture is written");
        path
    }

    fn session_config(workspace: PathBuf) -> RoleSessionConfig {
        RoleSessionConfig {
            workspace,
            call: LlmCallConfig::new("mock", "mock-model").expect("fake route is valid"),
            read_only: false,
            base_system_prompt: "base system".to_owned(),
        }
    }

    fn user_message(text: &str) -> Message {
        Message::user(
            "user-1",
            vec![ContentBlock::text(text).expect("text fits")],
            MessageSource::user().expect("user source is valid"),
        )
        .expect("user message is valid")
    }

    /// An extra tool schema that collides with the built-in `read` tool, so
    /// role assembly rejects the catalogue with `DuplicateToolName`.
    fn colliding_schema() -> ToolSchema {
        ToolSchema::new(
            "read",
            "duplicate of the built-in read tool",
            JsonValue::new(json!({"type": "object"})).expect("schema object is valid"),
        )
        .expect("schema is valid")
    }

    fn stub_extra_tools() -> ExtraTools {
        ExtraTools {
            tools: Arc::new(StubExecutor {
                label: "extra",
                order: Arc::new(Mutex::new(Vec::new())),
                fail_shutdown: false,
            }),
            schemas: vec![colliding_schema()],
        }
    }

    #[tokio::test]
    async fn a_failed_switch_keeps_the_old_role_and_its_tools() {
        let workspace = temp_workspace();
        let mut session = RoleSession::new(
            session_config(workspace.clone()),
            Arc::new(PanicProvider),
            RoleName::Navigator,
        )
        .await
        .expect("role session opens");
        let session_id = session.session().id().clone();

        // Assembly of the driver role fails after the new fiber is admitted:
        // the injected catalogue collides with the built-in `read` schema.
        session
            .extra_tools
            .insert(ExtraToolSlot::Mcp, stub_extra_tools());
        let error = session
            .switch(RoleName::Driver)
            .await
            .expect_err("a colliding catalogue must fail the switch");
        assert!(matches!(error, RoleSessionError::DuplicateToolName(_)));

        assert_eq!(session.role(), RoleName::Navigator);
        assert_eq!(session.session().id(), &session_id);
        assert!(
            session.agent.is_some(),
            "the old agent must survive a failed switch"
        );
        assert!(
            session.fiber.as_ref().expect("old fiber").is_active(),
            "the old role fiber must stay active"
        );

        // The old agent still accepts a turn. A cancelled turn settles without
        // calling the provider, so this proves the loop was not torn down.
        let cancellation = CancellationToken::new();
        cancellation.cancel();
        session
            .run_turn(user_message("still here"), cancellation)
            .await
            .expect("the old role still runs a turn");

        session.shutdown().await.expect("session shuts down");
        let _ = fs::remove_dir_all(&workspace);
    }

    #[tokio::test]
    async fn a_switched_away_role_bundle_stops_advertising_its_resources() {
        let workspace = temp_workspace();
        let mut session = RoleSession::new(
            session_config(workspace.clone()),
            Arc::new(PanicProvider),
            RoleName::Driver,
        )
        .await
        .expect("role session opens");
        let stale = session
            .fiber
            .as_ref()
            .expect("active fiber")
            .capability_provider()
            .expect("active role exposes its bundle");
        assert!(stale.prompt_fragment().is_some());
        assert!(!stale.skill_roots().is_empty());

        session
            .switch(RoleName::Navigator)
            .await
            .expect("switch succeeds");

        assert!(
            stale.tools().is_empty(),
            "tools from the previous role must not stay reachable"
        );
        assert!(
            stale.prompt_fragment().is_none(),
            "the previous role prompt must not stay reachable"
        );
        assert!(
            stale.skill_roots().is_empty(),
            "the previous role skill roots must not stay reachable"
        );
        let current = session
            .fiber
            .as_ref()
            .expect("new fiber")
            .capability_provider()
            .expect("the new role is active");
        assert!(current.prompt_fragment().is_some());
        assert!(!current.skill_roots().is_empty());

        session.shutdown().await.expect("session shuts down");
        let _ = fs::remove_dir_all(&workspace);
    }

    #[tokio::test]
    async fn an_agent_reassembly_failure_names_the_lost_old_agent() {
        let workspace = temp_workspace();
        let config = session_config(workspace.clone());
        let fiber = RoleFiber::spawn(Arc::new(RoleName::Driver.spec()))
            .await
            .expect("role fiber spawns");
        let parts = prepare_agent_parts(&config, &fiber, &std::collections::BTreeMap::new())
            .expect("parts prepare");

        let mut raw = Session::new("slowcode-reassembly").expect("session opens");
        let turn = TurnId::new(1).expect("turn id");
        let step = StepId::new(1).expect("step id");
        let call = ContentBlock::tool_call("call-1", "echo", "{}").expect("tool call block");
        let message = Message::assistant("assistant-1", vec![call], "mock", "mock-model")
            .expect("assistant message");
        raw.append(NewEvent::log(EventKind::turn_start(turn)))
            .expect("turn opens");
        raw.append(NewEvent::log(EventKind::step_start(turn, step)))
            .expect("step opens");
        raw.append(NewEvent::surface(
            EventKind::assistant_message(turn, step, message),
            SurfaceIntent::append(),
        ))
        .expect("unresolved tool call is appended");
        raw.append(NewEvent::log(EventKind::step_end(turn, step)))
            .expect("step closes");
        raw.append(NewEvent::log(EventKind::turn_end(
            turn,
            TurnEndReason::Completed,
        )))
        .expect("turn closes");

        let failure = match assemble_agent(&config, Arc::new(PanicProvider), raw, parts).await {
            Ok(_) => panic!("an unresolved tool call must fail reassembly"),
            Err(failure) => failure,
        };
        assert!(
            failure.session.is_none(),
            "AgentLoop::new consumes the Session on failure"
        );
        let text = failure.error.to_string();
        assert!(
            text.contains("previous agent"),
            "the failure must name the lost agent: {text}"
        );
        assert!(
            text.contains("not restored") || text.contains("cannot be restored"),
            "the failure must not claim a restore: {text}"
        );

        fiber.dispose().await.expect("role fiber disposes");
        let _ = fs::remove_dir_all(&workspace);
    }

    struct CountingRoleTool {
        name: &'static str,
        calls: Arc<Mutex<Vec<String>>>,
        shutdowns: Arc<Mutex<Vec<&'static str>>>,
        fail_shutdown: bool,
    }

    impl ToolExecutor for CountingRoleTool {
        fn execute(
            &self,
            request: ToolExecutionRequest,
            _cancellation: CancellationToken,
        ) -> ToolExecutionFuture<'_> {
            self.calls
                .lock()
                .expect("calls lock")
                .push(request.name().to_owned());
            Box::pin(async {
                Ok(ToolExecutionResult::success(vec![
                    ContentBlock::text("role-tool-ok").expect("text fits"),
                ])
                .expect("result is valid"))
            })
        }

        fn shutdown(&self) -> ToolShutdownFuture<'_> {
            self.shutdowns
                .lock()
                .expect("shutdown lock")
                .push(self.name);
            let fail = self.fail_shutdown;
            Box::pin(async move {
                if fail {
                    Err(ToolExecutorError::new("role tool shutdown failed"))
                } else {
                    Ok(())
                }
            })
        }
    }

    /// A provider that scripts a fixed stream per attempt and records the tool
    /// catalogue of every request.
    struct ScriptedProvider {
        attempts: Mutex<VecDeque<Vec<StreamChunk>>>,
        seen_tools: Mutex<Vec<Vec<String>>>,
    }

    impl ScriptedProvider {
        fn new(attempts: Vec<Vec<StreamChunk>>) -> Self {
            Self {
                attempts: Mutex::new(attempts.into_iter().collect()),
                seen_tools: Mutex::new(Vec::new()),
            }
        }
    }

    impl ModelProvider for ScriptedProvider {
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
            self.seen_tools.lock().expect("seen tools lock").push(
                request
                    .tools()
                    .iter()
                    .map(|tool| tool.name().to_owned())
                    .collect(),
            );
            let chunks: Vec<Result<StreamChunk, ProviderStreamError>> = self
                .attempts
                .lock()
                .expect("attempts lock")
                .pop_front()
                .expect("scripted attempt")
                .into_iter()
                .map(Ok)
                .collect();
            Box::pin(stream::iter(chunks))
        }
    }

    fn completed_text(text: &str) -> Vec<StreamChunk> {
        vec![
            StreamChunk::block_start(0, slowcode_kernel::model::ContentBlockType::Text)
                .expect("text start"),
            StreamChunk::text_delta(0, text).expect("text delta"),
            StreamChunk::block_end(0, ContentBlock::text(text).expect("text block"))
                .expect("text end"),
            StreamChunk::usage(TokenUsage::new(10, 10).expect("usage fits")).expect("usage"),
            StreamChunk::finish(FinishReason::stop().expect("stop reason"), None).expect("finish"),
        ]
    }

    fn requested_tool(id: &str, name: &str) -> Vec<StreamChunk> {
        let arguments = "{}";
        vec![
            StreamChunk::block_start(0, slowcode_kernel::model::ContentBlockType::ToolCall)
                .expect("tool start"),
            StreamChunk::tool_call_delta(0, id, Some(name.to_owned()), arguments)
                .expect("tool delta"),
            StreamChunk::block_end(
                0,
                ContentBlock::tool_call(id, name, arguments).expect("tool block"),
            )
            .expect("tool end"),
            StreamChunk::usage(TokenUsage::new(10, 10).expect("usage fits")).expect("usage"),
            StreamChunk::finish(FinishReason::tool_calls().expect("tool reason"), None)
                .expect("finish"),
        ]
    }

    #[tokio::test]
    async fn a_role_tool_routes_through_the_agent_and_survives_agent_release() {
        let workspace = temp_workspace();
        let config = session_config(workspace.clone());
        let calls = Arc::new(Mutex::new(Vec::new()));
        let shutdowns = Arc::new(Mutex::new(Vec::new()));
        let role = RoleName::Driver.spec().with_tool(CapabilityTool::new(
            named_schema("role_echo"),
            Arc::new(CountingRoleTool {
                name: "role_echo",
                calls: Arc::clone(&calls),
                shutdowns: Arc::clone(&shutdowns),
                fail_shutdown: false,
            }),
        ));
        let fiber = RoleFiber::spawn(Arc::new(role))
            .await
            .expect("role fiber spawns");
        let parts = prepare_agent_parts(&config, &fiber, &std::collections::BTreeMap::new())
            .expect("parts prepare");
        let provider = Arc::new(ScriptedProvider::new(vec![
            requested_tool("call-1", "role_echo"),
            completed_text("done"),
        ]));
        let session = Session::new("role-tool-turn").expect("session opens");
        let (mut agent, _tools) = match crate::agent::finish(parts, session, provider.clone()).await
        {
            Ok(agent) => agent,
            Err(_) => panic!("the agent finishes"),
        };

        let outcome = agent
            .run_turn(
                TurnProposal::Enter(vec![user_message("use the role tool")]),
                CancellationToken::new(),
            )
            .await
            .expect("turn completes");
        assert_eq!(outcome.reason(), &TurnEndReason::Completed);
        assert_eq!(*calls.lock().expect("calls lock"), ["role_echo"]);
        assert!(
            provider.seen_tools.lock().expect("seen tools lock")[0]
                .contains(&"role_echo".to_owned()),
            "the role tool must be advertised to the model"
        );
        assert!(
            shutdowns.lock().expect("shutdown lock").is_empty(),
            "the agent must not close the role bundle executor"
        );

        let _session = agent.shutdown_into_session().await.expect("agent releases");
        assert!(
            shutdowns.lock().expect("shutdown lock").is_empty(),
            "a shared role tool must survive agent release"
        );

        fiber.dispose().await.expect("role fiber disposes");
        assert_eq!(*shutdowns.lock().expect("shutdown lock"), ["role_echo"]);
        let _ = fs::remove_dir_all(&workspace);
    }

    #[tokio::test]
    async fn a_process_read_only_config_hides_role_tools_from_the_agent() {
        let workspace = temp_workspace();
        let mut config = session_config(workspace.clone());
        config.read_only = true;
        let calls = Arc::new(Mutex::new(Vec::new()));
        let shutdowns = Arc::new(Mutex::new(Vec::new()));
        let role = RoleName::Driver.spec().with_tool(CapabilityTool::new(
            named_schema("role_echo"),
            Arc::new(CountingRoleTool {
                name: "role_echo",
                calls: Arc::clone(&calls),
                shutdowns: Arc::clone(&shutdowns),
                fail_shutdown: false,
            }),
        ));
        let fiber = RoleFiber::spawn(Arc::new(role))
            .await
            .expect("role fiber spawns");
        let parts = prepare_agent_parts(&config, &fiber, &std::collections::BTreeMap::new())
            .expect("parts prepare");
        let provider = Arc::new(ScriptedProvider::new(vec![completed_text("done")]));
        let session = Session::new("role-tool-read-only").expect("session opens");
        let (mut agent, _tools) = match crate::agent::finish(parts, session, provider.clone()).await
        {
            Ok(agent) => agent,
            Err(_) => panic!("the agent finishes"),
        };

        agent
            .run_turn(
                TurnProposal::Enter(vec![user_message("inspect only")]),
                CancellationToken::new(),
            )
            .await
            .expect("turn completes");
        assert!(
            !provider.seen_tools.lock().expect("seen tools lock")[0]
                .contains(&"role_echo".to_owned()),
            "process read-only must hide role tools from the catalogue"
        );
        assert!(calls.lock().expect("calls lock").is_empty());

        let _session = agent.shutdown_into_session().await.expect("agent releases");
        fiber.dispose().await.expect("role fiber disposes");
        assert_eq!(
            *shutdowns.lock().expect("shutdown lock"),
            ["role_echo"],
            "a filtered role tool is still closed exactly once"
        );
        let _ = fs::remove_dir_all(&workspace);
    }

    #[tokio::test]
    async fn role_bundle_shutdown_is_idempotent_and_reports_the_first_failure() {
        let calls = Arc::new(Mutex::new(Vec::new()));
        let shutdowns = Arc::new(Mutex::new(Vec::new()));
        let role = RoleName::Driver.spec().with_tool(CapabilityTool::new(
            named_schema("role_echo"),
            Arc::new(CountingRoleTool {
                name: "role_echo",
                calls,
                shutdowns: Arc::clone(&shutdowns),
                fail_shutdown: true,
            }),
        ));
        let fiber = RoleFiber::spawn(Arc::new(role))
            .await
            .expect("role fiber spawns");
        let bundle = fiber.bundle().expect("active bundle");

        assert!(matches!(
            bundle.shutdown().await,
            Err(RoleFiberError::Shutdown(_))
        ));
        assert!(matches!(
            bundle.shutdown().await,
            Err(RoleFiberError::Shutdown(_))
        ));
        assert_eq!(*shutdowns.lock().expect("shutdown lock"), ["role_echo"]);

        assert!(matches!(
            fiber.dispose().await,
            Err(RoleFiberError::Shutdown(_))
        ));
        assert_eq!(
            *shutdowns.lock().expect("shutdown lock"),
            ["role_echo"],
            "dispose must not replay a completed shutdown"
        );
    }

    #[tokio::test]
    async fn a_failed_extra_tools_install_keeps_the_old_agent() {
        let workspace = temp_workspace();
        let mut session = RoleSession::new(
            session_config(workspace.clone()),
            Arc::new(PanicProvider),
            RoleName::Navigator,
        )
        .await
        .expect("role session opens");
        let session_id = session.session().id().clone();

        let error = session
            .install_extra_tools(
                ExtraToolSlot::Mcp,
                Arc::new(StubExecutor {
                    label: "extra",
                    order: Arc::new(Mutex::new(Vec::new())),
                    fail_shutdown: false,
                }),
                vec![colliding_schema()],
            )
            .await
            .expect_err("a colliding catalogue must be rejected");
        assert!(matches!(error, RoleSessionError::DuplicateToolName(_)));

        assert_eq!(session.role(), RoleName::Navigator);
        assert_eq!(session.session().id(), &session_id);
        assert!(
            session.agent.is_some(),
            "the old agent must survive a failed tool install"
        );

        let cancellation = CancellationToken::new();
        cancellation.cancel();
        session
            .run_turn(user_message("still here"), cancellation)
            .await
            .expect("the old role still runs a turn");

        session.shutdown().await.expect("session shuts down");
        let _ = fs::remove_dir_all(&workspace);
    }

    #[tokio::test]
    async fn a_failed_set_call_keeps_the_old_agent() {
        let workspace = temp_workspace();
        let mut session = RoleSession::new(
            session_config(workspace.clone()),
            Arc::new(PanicProvider),
            RoleName::Navigator,
        )
        .await
        .expect("role session opens");
        let session_id = session.session().id().clone();

        let oversized = LlmCallConfig::from_parts(
            "mock".to_owned(),
            "m".repeat(MAX_AGENT_FIXED_REQUEST_BYTES + 1),
            None,
            None,
            None,
            None,
        )
        .expect("an oversized route is still a valid value");
        session
            .set_call(oversized)
            .await
            .expect_err("a route that exceeds the fixed request budget must fail");

        assert_eq!(session.role(), RoleName::Navigator);
        assert_eq!(session.session().id(), &session_id);
        assert!(
            session.agent.is_some(),
            "the old agent must survive a failed call change"
        );

        let cancellation = CancellationToken::new();
        cancellation.cancel();
        session
            .run_turn(user_message("still here"), cancellation)
            .await
            .expect("the old role still runs a turn");

        session.shutdown().await.expect("session shuts down");
        let _ = fs::remove_dir_all(&workspace);
    }

    #[test]
    fn composite_routes_by_extra_name() {
        let order = Arc::new(Mutex::new(Vec::new()));
        let primary = Arc::new(StubExecutor {
            label: "primary",
            order: Arc::clone(&order),
            fail_shutdown: false,
        });
        let extra = Arc::new(StubExecutor {
            label: "extra",
            order,
            fail_shutdown: false,
        });
        let composite = CompositeTools {
            primary: primary.clone(),
            extra: extra.clone(),
            extra_names: BTreeSet::from(["mcp_tool".to_owned()]),
            shutdown_extra: true,
        };

        let routed: &dyn ToolExecutor = composite.select("mcp_tool");
        let expected_extra: &dyn ToolExecutor = extra.as_ref();
        assert!(std::ptr::eq(routed, expected_extra));

        let builtin: &dyn ToolExecutor = composite.select("read");
        let expected_primary: &dyn ToolExecutor = primary.as_ref();
        assert!(std::ptr::eq(builtin, expected_primary));
    }

    #[tokio::test]
    async fn composite_shuts_down_both_even_when_both_fail() {
        let order = Arc::new(Mutex::new(Vec::new()));
        let composite = CompositeTools {
            primary: Arc::new(StubExecutor {
                label: "primary",
                order: Arc::clone(&order),
                fail_shutdown: true,
            }),
            extra: Arc::new(StubExecutor {
                label: "extra",
                order: Arc::clone(&order),
                fail_shutdown: true,
            }),
            extra_names: BTreeSet::new(),
            shutdown_extra: true,
        };

        assert!(composite.shutdown().await.is_err());
        assert_eq!(*order.lock().expect("order lock"), ["primary", "extra"]);
    }

    #[test]
    fn shutdown_merge_reports_a_failure_from_either_side() {
        assert!(merge_shutdown_results(Ok(()), Ok(())).is_ok());
        assert!(merge_shutdown_results(Err(ToolExecutorError::new("first")), Ok(())).is_err());
        assert!(merge_shutdown_results(Ok(()), Err(ToolExecutorError::new("second"))).is_err());
    }

    fn named_schema(name: &str) -> ToolSchema {
        ToolSchema::new(
            name,
            "test tool",
            JsonValue::new(json!({"type": "object"})).expect("schema object is valid"),
        )
        .expect("schema is valid")
    }

    fn stub_executor(order: &Arc<Mutex<Vec<&'static str>>>) -> Arc<StubExecutor> {
        Arc::new(StubExecutor {
            label: "extra",
            order: Arc::clone(order),
            fail_shutdown: false,
        })
    }

    #[tokio::test]
    async fn composite_skips_shutdown_for_shared_extras() {
        let order = Arc::new(Mutex::new(Vec::new()));
        let composite = CompositeTools {
            primary: Arc::new(StubExecutor {
                label: "primary",
                order: Arc::clone(&order),
                fail_shutdown: false,
            }),
            extra: stub_executor(&order),
            extra_names: BTreeSet::from(["mcp_tool".to_owned()]),
            shutdown_extra: false,
        };

        composite.shutdown().await.expect("shutdown succeeds");
        assert_eq!(*order.lock().expect("order lock"), ["primary"]);
    }

    #[tokio::test]
    async fn slots_are_independent_and_clear_touches_one_slot() {
        let workspace = temp_workspace();
        let mut session = RoleSession::new(
            session_config(workspace.clone()),
            Arc::new(PanicProvider),
            RoleName::Driver,
        )
        .await
        .expect("role session opens");
        let order = Arc::new(Mutex::new(Vec::new()));

        session
            .install_extra_tools(
                ExtraToolSlot::Mcp,
                stub_executor(&order),
                vec![named_schema("mcp_tool")],
            )
            .await
            .expect("mcp installs");
        session
            .install_extra_tools(
                ExtraToolSlot::ReadonlyShell,
                stub_executor(&order),
                vec![named_schema(READONLY_SHELL_TOOL_NAME)],
            )
            .await
            .expect("shell installs");
        assert_eq!(session.extra_tools.len(), 2);

        session
            .clear_extra_tools(ExtraToolSlot::ReadonlyShell)
            .await
            .expect("shell clears");
        assert!(session.extra_tools.contains_key(&ExtraToolSlot::Mcp));
        assert!(
            !session
                .extra_tools
                .contains_key(&ExtraToolSlot::ReadonlyShell)
        );

        session.shutdown().await.expect("session shuts down");
        let _ = fs::remove_dir_all(&workspace);
    }

    #[tokio::test]
    async fn reserved_tool_name_is_rejected_for_the_mcp_slot() {
        let workspace = temp_workspace();
        let mut session = RoleSession::new(
            session_config(workspace.clone()),
            Arc::new(PanicProvider),
            RoleName::Driver,
        )
        .await
        .expect("role session opens");
        let order = Arc::new(Mutex::new(Vec::new()));

        let error = session
            .install_extra_tools(
                ExtraToolSlot::Mcp,
                stub_executor(&order),
                vec![named_schema(READONLY_SHELL_TOOL_NAME)],
            )
            .await
            .expect_err("reserved names cannot enter the mcp slot");
        assert!(matches!(error, RoleSessionError::ReservedToolName(_)));
        assert!(session.extra_tools.is_empty());

        session.shutdown().await.expect("session shuts down");
        let _ = fs::remove_dir_all(&workspace);
    }

    #[tokio::test]
    async fn failed_slot_install_restores_only_that_slot() {
        let workspace = temp_workspace();
        let mut session = RoleSession::new(
            session_config(workspace.clone()),
            Arc::new(PanicProvider),
            RoleName::Driver,
        )
        .await
        .expect("role session opens");
        let order = Arc::new(Mutex::new(Vec::new()));
        session
            .install_extra_tools(
                ExtraToolSlot::Mcp,
                stub_executor(&order),
                vec![named_schema("mcp_tool")],
            )
            .await
            .expect("mcp installs");

        let error = session
            .install_extra_tools(
                ExtraToolSlot::ReadonlyShell,
                stub_executor(&order),
                vec![colliding_schema()],
            )
            .await
            .expect_err("colliding shell schema fails");
        assert!(matches!(error, RoleSessionError::DuplicateToolName(_)));
        assert!(session.extra_tools.contains_key(&ExtraToolSlot::Mcp));
        assert!(
            !session
                .extra_tools
                .contains_key(&ExtraToolSlot::ReadonlyShell)
        );

        session.shutdown().await.expect("session shuts down");
        let _ = fs::remove_dir_all(&workspace);
    }

    #[tokio::test]
    async fn owned_slot_is_shut_down_on_role_switch() {
        let workspace = temp_workspace();
        let mut session = RoleSession::new(
            session_config(workspace.clone()),
            Arc::new(PanicProvider),
            RoleName::Driver,
        )
        .await
        .expect("role session opens");
        let order = Arc::new(Mutex::new(Vec::new()));
        session
            .install_extra_tools(
                ExtraToolSlot::ReadonlyShell,
                stub_executor(&order),
                vec![named_schema(READONLY_SHELL_TOOL_NAME)],
            )
            .await
            .expect("shell installs");

        session
            .switch(RoleName::Navigator)
            .await
            .expect("switch succeeds");
        assert_eq!(*order.lock().expect("order lock"), ["extra"]);

        session.shutdown().await.expect("session shuts down");
        let _ = fs::remove_dir_all(&workspace);
    }

    #[tokio::test]
    async fn set_call_keeps_extra_tool_slots() {
        let workspace = temp_workspace();
        let mut session = RoleSession::new(
            session_config(workspace.clone()),
            Arc::new(PanicProvider),
            RoleName::Driver,
        )
        .await
        .expect("role session opens");
        let order = Arc::new(Mutex::new(Vec::new()));
        session
            .install_extra_tools(
                ExtraToolSlot::Mcp,
                stub_executor(&order),
                vec![named_schema("mcp_tool")],
            )
            .await
            .expect("mcp installs");

        let call = LlmCallConfig::new("mock", "mock-model-2").expect("call is valid");
        session.set_call(call).await.expect("call changes");
        assert!(session.extra_tools.contains_key(&ExtraToolSlot::Mcp));

        session.shutdown().await.expect("session shuts down");
        let _ = fs::remove_dir_all(&workspace);
    }

    #[test]
    fn system_prefix_is_base_then_role() {
        assert_eq!(compose_system("base", Some("role")), "base\n\nrole");
        assert_eq!(compose_system(" base ", Some("")), "base");
        assert_eq!(compose_system("", Some("role")), "role");
        assert_eq!(compose_system("", None), "");
    }

    #[test]
    fn transition_notice_states_the_effective_tool_boundary() {
        let read_only = role_transition_notice(RoleName::Navigator, true);
        assert!(read_only.contains("active role is now navigator"));
        assert!(read_only.contains("read-only inspection tools"));
        assert!(read_only.contains("Statements in earlier messages"));

        let writable = role_transition_notice(RoleName::Driver, false);
        assert!(writable.contains("active role is now driver"));
        assert!(writable.contains("file-change"));
    }
}
