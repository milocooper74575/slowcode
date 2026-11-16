//! Role fibers and capability bundles for slowcode.
//!
//! A role is a capability bundle guarded by one Cordis fiber. Static role
//! definitions remain available for tests and for assembly, while
//! [`RoleFiber`] proves the lifecycle seam.

use std::path::PathBuf;
use std::sync::Arc;

use serde_json::json;
use slowcode_kernel::{
    agent::{
        ToolExecutionFuture, ToolExecutionRequest, ToolExecutionResult, ToolExecutor,
        ToolExecutorError,
    },
    capability::{CapabilityProvider, CapabilityTool},
    model::{ContentBlock, JsonValue, ToolSchema},
};
use tokio_util::sync::CancellationToken;

pub mod agent;
mod assembly;
mod bundle;
mod cache;
mod fiber;

pub use agent::{
    AgentBuildError, AgentSpec, ExtraToolSet, FinishError, PreparedAgent, RegistryMode,
    ToolOwnership, build, finish, prepare,
};
pub use assembly::{ExtraToolSlot, RoleSession, RoleSessionConfig, RoleSessionError};
pub use bundle::RoleBundle;
pub use cache::CacheReport;
pub use fiber::{RoleFiber, RoleFiberError};

pub const NAVIGATOR: &str = "navigator";
pub const DRIVER: &str = "driver";

pub const NAVIGATOR_PROMPT: &str = "\
You are the navigator. Make directional decisions: choose an approach, adjust constraints, and mark cognitive gaps for the driver. Do not implement the user's core logic yourself.
This role is read-only: only inspection tools are available. If a task needs a file change or a shell command, describe the step for the driver instead of attempting it. A missing write or shell tool is the expected role boundary, not an error.";
pub const DRIVER_PROMPT: &str = "\
You are the driver. Execute the chosen direction with tools, keep changes small, and stop at a cognitive gap instead of guessing.
This role may edit files and run commands when the current request lists those tools. Use them to carry out the chosen direction rather than re-deciding it.";

/// The only two roles slowcode installs.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum RoleName {
    Navigator,
    Driver,
}

impl RoleName {
    /// Parse a user-facing role name.
    #[must_use]
    pub fn parse(value: &str) -> Option<Self> {
        match value.trim().to_ascii_lowercase().as_str() {
            NAVIGATOR => Some(Self::Navigator),
            DRIVER => Some(Self::Driver),
            _ => None,
        }
    }

    /// Stable wire and prompt name.
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Navigator => NAVIGATOR,
            Self::Driver => DRIVER,
        }
    }

    /// Build the static role definition for this name.
    ///
    /// Each role contributes one workspace-relative Skill root. Shared Skills
    /// live in `.slowcode/skills` and are scanned for both roles.
    #[must_use]
    pub fn spec(self) -> StaticRole {
        match self {
            Self::Navigator => {
                navigator().with_skill_root(format!(".slowcode/roles/{NAVIGATOR}/skills"))
            }
            Self::Driver => driver().with_skill_root(format!(".slowcode/roles/{DRIVER}/skills")),
        }
    }
}

impl std::fmt::Display for RoleName {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(self.as_str())
    }
}

/// A role assembled from static prompt text and optional tools.
#[derive(Clone)]
pub struct StaticRole {
    name: &'static str,
    prompt: &'static str,
    tools: Vec<CapabilityTool>,
    skill_roots: Vec<PathBuf>,
    read_only: bool,
}

impl StaticRole {
    #[must_use]
    pub fn new(name: &'static str, prompt: &'static str) -> Self {
        Self {
            name,
            prompt,
            tools: Vec::new(),
            skill_roots: Vec::new(),
            read_only: false,
        }
    }

    #[must_use]
    pub fn with_tool(mut self, tool: CapabilityTool) -> Self {
        self.tools.push(tool);
        self
    }

    #[must_use]
    pub fn with_skill_root(mut self, root: impl Into<PathBuf>) -> Self {
        self.skill_roots.push(root.into());
        self
    }

    /// Whether this role may only use read-only tools.
    #[must_use]
    pub fn is_read_only(&self) -> bool {
        self.read_only
    }

    #[must_use]
    pub fn with_read_only(mut self, read_only: bool) -> Self {
        self.read_only = read_only;
        self
    }

    /// A copy with the same identity, prompt and Skill roots but no tools.
    ///
    /// Read-only roles use this projection until a contributed tool can carry
    /// a read-only marker.
    #[must_use]
    pub(crate) fn without_tools(&self) -> Self {
        Self {
            tools: Vec::new(),
            ..self.clone()
        }
    }
}

impl CapabilityProvider for StaticRole {
    fn name(&self) -> &str {
        self.name
    }

    fn tools(&self) -> Vec<CapabilityTool> {
        self.tools.clone()
    }

    fn prompt_fragment(&self) -> Option<String> {
        Some(self.prompt.to_owned())
    }

    fn skill_roots(&self) -> Vec<PathBuf> {
        self.skill_roots.clone()
    }
}

#[must_use]
pub fn navigator() -> StaticRole {
    StaticRole::new(NAVIGATOR, NAVIGATOR_PROMPT).with_read_only(true)
}

#[must_use]
pub fn driver() -> StaticRole {
    StaticRole::new(DRIVER, DRIVER_PROMPT).with_read_only(false)
}

/// Minimal capability tool used to prove that a role can inject executable
/// tools into the kernel. It echoes one `text` argument.
pub struct EchoTool;

impl ToolExecutor for EchoTool {
    fn execute(
        &self,
        request: ToolExecutionRequest,
        _cancellation: CancellationToken,
    ) -> ToolExecutionFuture<'_> {
        Box::pin(async move {
            let text = request
                .arguments()
                .as_value()
                .get("text")
                .and_then(serde_json::Value::as_str)
                .unwrap_or_default();
            let block = ContentBlock::text(text)
                .map_err(|_| ToolExecutorError::new("echo text is not a valid content block"))?;
            ToolExecutionResult::success(vec![block])
                .map_err(|_| ToolExecutorError::new("echo result is not valid"))
        })
    }
}

#[must_use]
pub fn echo_tool() -> CapabilityTool {
    let schema = ToolSchema::new(
        "echo",
        "Echo the provided text back to the model.",
        JsonValue::new(json!({
            "type": "object",
            "properties": {
                "text": { "type": "string" }
            },
            "required": ["text"],
            "additionalProperties": false
        }))
        .expect("static echo schema is valid"),
    )
    .expect("static echo tool is valid");
    CapabilityTool::new(schema, Arc::new(EchoTool))
}

#[cfg(test)]
mod tests {
    use super::*;
    use slowcode_kernel::{
        agent::AgentLoopConfig,
        capability::{CapabilityError, CapabilitySet},
        model::LlmCallConfig,
        skill_roots::{SkillRootError, SkillRoots},
    };

    #[test]
    fn role_set_is_only_navigator_and_driver() {
        assert_eq!(NAVIGATOR, "navigator");
        assert_eq!(DRIVER, "driver");
        assert_eq!(RoleName::parse("navigator"), Some(RoleName::Navigator));
        assert_eq!(RoleName::parse("Navigator"), Some(RoleName::Navigator));
        assert_eq!(RoleName::parse(" DRIVER "), Some(RoleName::Driver));
        assert_eq!(RoleName::parse(""), None);
        assert_eq!(RoleName::parse("explorer"), None);
        assert!(navigator().is_read_only());
        assert!(!driver().is_read_only());
    }

    #[test]
    fn static_roles_merge_prompts_without_tools() {
        let set = CapabilitySet::from_providers(&[Arc::new(navigator()), Arc::new(driver())])
            .expect("roles merge");
        assert_eq!(set.provider_names(), [NAVIGATOR, DRIVER]);
        assert!(set.schemas().is_empty());
        let prompt = set.system_prompt().expect("prompt is present");
        assert!(prompt.contains("You are the navigator."));
        assert!(prompt.contains("You are the driver."));
    }

    #[test]
    fn role_prompts_declare_their_tool_boundary() {
        assert!(NAVIGATOR_PROMPT.contains("read-only"));
        assert!(NAVIGATOR_PROMPT.contains("not an error"));
        assert!(DRIVER_PROMPT.contains("may edit files and run commands"));
        assert!(!NAVIGATOR_PROMPT.contains("Use write and edit"));
        assert!(!DRIVER_PROMPT.contains("Use write and edit"));
    }

    #[test]
    fn role_can_inject_a_tool() {
        let role = navigator().with_tool(echo_tool());
        let set = CapabilitySet::from_providers(&[Arc::new(role)]).expect("role merges");
        assert_eq!(set.schemas().len(), 1);
        assert_eq!(set.schemas()[0].name(), "echo");
        assert!(set.route("echo").is_some());
        assert!(set.route("missing").is_none());
    }

    #[test]
    fn duplicate_tool_names_are_rejected() {
        let navigator = navigator().with_tool(echo_tool());
        let driver = driver().with_tool(echo_tool());
        let error = match CapabilitySet::from_providers(&[Arc::new(navigator), Arc::new(driver)]) {
            Ok(_) => panic!("duplicate tool name must fail"),
            Err(error) => error,
        };
        assert!(matches!(error, CapabilityError::DuplicateToolName { .. }));
    }

    #[test]
    fn absolute_role_skill_root_is_rejected_at_assembly() {
        let role = navigator().with_skill_root("/tmp/skills");
        let set = CapabilitySet::from_providers(&[Arc::new(role)]).expect("role merges");
        let error = SkillRoots::new(set.skill_roots().iter().cloned())
            .expect_err("absolute role roots must be rejected");
        assert!(matches!(error, SkillRootError::OutsideWorkspace { .. }));
    }

    #[test]
    fn merged_capabilities_fit_the_agent_config() {
        let role = navigator().with_tool(echo_tool());
        let set = CapabilitySet::from_providers(&[Arc::new(role)]).expect("role merges");
        let call = LlmCallConfig::new("fake", "fake-model").expect("fake route is valid");
        let config = AgentLoopConfig::new(call)
            .with_system(set.system_prompt().expect("prompt"))
            .expect("system prompt fits")
            .with_tools(set.schemas().to_vec())
            .expect("tool schemas fit");
        let debug = format!("{config:?}");
        assert!(debug.contains("tool_count: 1"), "{debug}");
        assert!(debug.contains("system_bytes:"), "{debug}");
    }
}
