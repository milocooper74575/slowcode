//! Shared agent assembly for role sessions and Plan commands.
//!
//! The builder owns mechanism wiring only: tool registry, Skill roots, Skill
//! catalog installation, AGENTS.md runtime and loop policies. Callers keep
//! their own Session lifecycle and phase policy.

use std::{
    collections::BTreeSet,
    path::{Path, PathBuf},
    sync::Arc,
};

use slowcode_kernel::{
    agent::{
        AgentLoop, AgentLoopConfig, ApprovalProvider, FileChangePolicy, NoApprovalProvider,
        NoTools, PluginPolicy, ShellPolicy, ToolClaimProfile, ToolExecutionFuture,
        ToolExecutionMode, ToolExecutionRequest, ToolExecutor, ToolPreparationFuture,
        ToolShutdownFuture,
    },
    model::{LlmCallConfig, ToolSchema},
    provider::ModelProvider,
    session::Session,
    skill_roots::SkillRoots,
    tools::{
        LocalToolRegistry, ReadOnlyToolRegistry, ToolRegistryBuildError, WorkspaceToolRegistry,
    },
    workspace_instructions::{WorkspaceInstructionError, WorkspaceInstructionRuntime},
};
use thiserror::Error;

/// Which workspace registry the agent should use.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum RegistryMode {
    /// No tools at all. Used for a coach turn that must not act.
    None,
    /// Read-only tools plus Skill loading.
    ReadOnly,
    /// Read and text-mutation tools plus Skill loading. No shell.
    Workspace,
    /// Full local tools including shell and background jobs.
    Local,
}

/// Who is responsible for shutting an extra tool set down.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ToolOwnership {
    /// The current agent assembly created it and must close it on release.
    Owned,
    /// A long-lived resource owned by the runtime, reused across assemblies.
    Shared,
}

/// One additional tool set composed over the workspace registry.
#[derive(Clone)]
pub struct ExtraToolSet {
    pub tools: Arc<dyn ToolExecutor>,
    pub schemas: Vec<ToolSchema>,
    pub ownership: ToolOwnership,
}

impl ExtraToolSet {
    /// A tool set owned by this agent assembly.
    #[must_use]
    pub fn new(tools: Arc<dyn ToolExecutor>, schemas: Vec<ToolSchema>) -> Self {
        Self {
            tools,
            schemas,
            ownership: ToolOwnership::Owned,
        }
    }

    /// A tool set owned by the runtime and reused across assemblies.
    #[must_use]
    pub fn shared(tools: Arc<dyn ToolExecutor>, schemas: Vec<ToolSchema>) -> Self {
        Self {
            tools,
            schemas,
            ownership: ToolOwnership::Shared,
        }
    }
}

/// Everything needed to prepare one agent, excluding the Session.
pub struct AgentSpec {
    pub workspace: PathBuf,
    pub call: LlmCallConfig,
    pub system: String,
    pub registry: RegistryMode,
    pub skill_roots: SkillRoots,
    pub read_only: bool,
    pub file_change: FileChangePolicy,
    pub shell: ShellPolicy,
    pub approval: Arc<dyn ApprovalProvider>,
    pub extra_tools: Vec<ExtraToolSet>,
    pub install_workspace_instructions: bool,
    /// Override the kernel's per-turn step ceiling for this agent.
    /// `None` keeps the kernel default.
    pub max_steps_per_turn: Option<usize>,
}

impl AgentSpec {
    #[must_use]
    pub fn new(
        workspace: PathBuf,
        call: LlmCallConfig,
        system: String,
        registry: RegistryMode,
    ) -> Self {
        let read_only = matches!(registry, RegistryMode::None | RegistryMode::ReadOnly);
        Self {
            workspace,
            call,
            system,
            registry,
            skill_roots: SkillRoots::empty(),
            read_only,
            file_change: FileChangePolicy::Deny,
            shell: ShellPolicy::Deny,
            approval: Arc::new(NoApprovalProvider),
            extra_tools: Vec::new(),
            install_workspace_instructions: false,
            max_steps_per_turn: None,
        }
    }
}

/// Prepared agent parts that can be finished before or after a Session release.
pub struct PreparedAgent {
    workspace: PathBuf,
    config: AgentLoopConfig,
    tools: Arc<dyn ToolExecutor>,
    skill_roots: SkillRoots,
    workspace_instructions: Option<WorkspaceInstructionRuntime>,
}

/// Build one complete agent.
pub async fn build(
    spec: AgentSpec,
    session: Session,
    provider: Arc<dyn ModelProvider>,
) -> Result<(AgentLoop, Arc<dyn ToolExecutor>), AgentBuildError> {
    let prepared = prepare(spec)?;
    finish(prepared, session, provider)
        .await
        .map_err(|failure| failure.error)
}

/// Prepare everything that does not consume a Session.
pub fn prepare(spec: AgentSpec) -> Result<PreparedAgent, AgentBuildError> {
    let AgentSpec {
        workspace,
        call,
        system,
        registry,
        skill_roots,
        read_only,
        file_change,
        shell,
        approval,
        extra_tools,
        install_workspace_instructions,
        max_steps_per_turn,
    } = spec;
    let (tools, schemas) = open_registry(&workspace, registry, &skill_roots, read_only)?;
    let (tools, schemas) = compose_tools(tools, schemas, extra_tools)?;
    let config = AgentLoopConfig::new(call)
        .with_system(system)?
        .with_tools(schemas)?
        .with_approval_provider(approval)
        .with_plugin_policy(PluginPolicy::Deny);
    let config = match max_steps_per_turn {
        Some(steps) => {
            let limits = config.limits().clone().with_max_steps_per_turn(steps)?;
            config.with_limits(limits)
        }
        None => config,
    };
    let config = if read_only {
        config
            .with_file_change_policy(FileChangePolicy::Deny)
            .with_shell_policy(ShellPolicy::Deny)
    } else {
        config
            .with_file_change_policy(file_change)
            .with_shell_policy(shell)
    };
    let workspace_instructions = if install_workspace_instructions {
        Some(WorkspaceInstructionRuntime::from_workspace(&workspace)?)
    } else {
        None
    };
    Ok(PreparedAgent {
        workspace,
        config,
        tools,
        skill_roots,
        workspace_instructions,
    })
}

/// Finish one prepared agent with a caller-owned Session.
pub async fn finish(
    prepared: PreparedAgent,
    session: Session,
    provider: Arc<dyn ModelProvider>,
) -> Result<(AgentLoop, Arc<dyn ToolExecutor>), FinishError> {
    let PreparedAgent {
        workspace,
        config,
        tools,
        skill_roots,
        workspace_instructions,
    } = prepared;
    let mut agent =
        AgentLoop::new(session, provider, Arc::clone(&tools), config).map_err(|error| {
            FinishError {
                error: error.into(),
                session: None,
            }
        })?;
    if let Err(error) = agent.install_skill_roots(&workspace, &skill_roots) {
        let session = match agent.shutdown_into_session().await {
            Ok(session) => session,
            Err(release) => release.into_parts().1,
        };
        return Err(FinishError {
            error: error.into(),
            session: Some(Box::new(session)),
        });
    }
    if let Some(runtime) = workspace_instructions {
        agent.install_workspace_instruction_runtime(runtime);
    }
    Ok((agent, tools))
}

/// Failure while finishing prepared parts.
///
/// `session` is present when the agent could be shut down after a Skill
/// installation failure and the caller can still recover the conversation.
pub struct FinishError {
    pub error: AgentBuildError,
    pub session: Option<Box<Session>>,
}

fn open_registry(
    workspace: &Path,
    mode: RegistryMode,
    skill_roots: &SkillRoots,
    read_only: bool,
) -> Result<(Arc<dyn ToolExecutor>, Vec<ToolSchema>), AgentBuildError> {
    if read_only && mode != RegistryMode::None {
        let registry = ReadOnlyToolRegistry::open_with_skill_roots(workspace, skill_roots)?;
        let schemas = registry.schemas().to_vec();
        return Ok((Arc::new(registry), schemas));
    }
    match mode {
        RegistryMode::None => Ok((Arc::new(NoTools), Vec::new())),
        RegistryMode::ReadOnly => {
            let registry = ReadOnlyToolRegistry::open_with_skill_roots(workspace, skill_roots)?;
            let schemas = registry.schemas().to_vec();
            Ok((Arc::new(registry), schemas))
        }
        RegistryMode::Workspace => {
            let registry = WorkspaceToolRegistry::open_with_skill_roots(workspace, skill_roots)?;
            let schemas = registry.schemas().to_vec();
            Ok((Arc::new(registry), schemas))
        }
        RegistryMode::Local => {
            let registry = LocalToolRegistry::open_with_skill_roots(workspace, skill_roots)?;
            let schemas = registry.schemas().to_vec();
            Ok((Arc::new(registry), schemas))
        }
    }
}

fn compose_tools(
    primary: Arc<dyn ToolExecutor>,
    mut schemas: Vec<ToolSchema>,
    extras: Vec<ExtraToolSet>,
) -> Result<(Arc<dyn ToolExecutor>, Vec<ToolSchema>), AgentBuildError> {
    let mut names = schemas
        .iter()
        .map(|schema| schema.name().to_owned())
        .collect::<BTreeSet<_>>();
    let mut tools = primary;
    for extra in extras {
        let extra_names = extra
            .schemas
            .iter()
            .map(|schema| schema.name().to_owned())
            .collect::<BTreeSet<_>>();
        for name in &extra_names {
            if !names.insert(name.clone()) {
                return Err(AgentBuildError::DuplicateTool(name.clone()));
            }
        }
        tools = Arc::new(CompositeTools {
            primary: tools,
            extra: extra.tools,
            extra_names,
            shutdown_extra: extra.ownership == ToolOwnership::Owned,
        });
        schemas.extend(extra.schemas);
    }
    // A deterministic catalogue is part of the stable prompt prefix.
    schemas.sort_by(|left, right| left.name().cmp(right.name()));
    Ok((tools, schemas))
}

pub(crate) struct CompositeTools {
    pub(crate) primary: Arc<dyn ToolExecutor>,
    pub(crate) extra: Arc<dyn ToolExecutor>,
    pub(crate) extra_names: BTreeSet<String>,
    pub(crate) shutdown_extra: bool,
}

impl CompositeTools {
    #[cfg(test)]
    pub(crate) fn select(&self, name: &str) -> &dyn ToolExecutor {
        self.route(name)
    }

    fn route(&self, name: &str) -> &dyn ToolExecutor {
        if self.extra_names.contains(name) {
            self.extra.as_ref()
        } else {
            self.primary.as_ref()
        }
    }
}

impl ToolExecutor for CompositeTools {
    fn claim_profile(&self, tool_name: &str) -> ToolClaimProfile {
        self.route(tool_name).claim_profile(tool_name)
    }

    fn execution_mode(&self, tool_name: &str) -> ToolExecutionMode {
        self.route(tool_name).execution_mode(tool_name)
    }

    fn execute(
        &self,
        request: ToolExecutionRequest,
        cancellation: tokio_util::sync::CancellationToken,
    ) -> ToolExecutionFuture<'_> {
        self.route(request.name()).execute(request, cancellation)
    }

    fn prepare(
        &self,
        request: ToolExecutionRequest,
        cancellation: tokio_util::sync::CancellationToken,
    ) -> ToolPreparationFuture<'_> {
        self.route(request.name()).prepare(request, cancellation)
    }

    fn shutdown(&self) -> ToolShutdownFuture<'_> {
        Box::pin(async move {
            let primary = self.primary.shutdown().await;
            let extra = if self.shutdown_extra {
                self.extra.shutdown().await
            } else {
                Ok(())
            };
            primary.and(extra)
        })
    }
}

/// Errors from shared agent assembly.
#[derive(Debug, Error)]
pub enum AgentBuildError {
    #[error("tool registry could not be opened: {0}")]
    Registry(#[from] ToolRegistryBuildError),
    #[error("agent configuration is invalid: {0}")]
    Config(#[from] slowcode_kernel::agent::AgentBuildError),
    #[error("Skill runtime could not be installed: {0}")]
    Skill(#[from] std::io::Error),
    #[error("workspace instructions could not be opened: {0}")]
    Instructions(#[from] WorkspaceInstructionError),
    #[error("tool {0} is declared by more than one registry")]
    DuplicateTool(String),
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn no_tools_mode_exposes_no_schemas() {
        let (_, schemas) = open_registry(
            Path::new("."),
            RegistryMode::None,
            &SkillRoots::empty(),
            true,
        )
        .expect("no-tools registry");
        assert!(schemas.is_empty());
    }
}
