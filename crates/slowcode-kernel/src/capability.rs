//! Capability seams filled by role fibers.
//!
//! The kernel stays independent of Cordis. A role runtime adapts its fibers to
//! [`CapabilityProvider`]; [`CapabilitySet`] merges the contributed tools,
//! prompt fragments, and skill roots into the shapes the agent loop already
//! accepts.

use std::{collections::BTreeMap, path::PathBuf, sync::Arc};

use thiserror::Error;

use crate::{
    agent::{
        ToolClaimProfile, ToolExecutionFuture, ToolExecutionMode, ToolExecutionRequest,
        ToolExecutor, ToolExecutorError, ToolPreparationFuture, ToolShutdownFuture,
    },
    model::ToolSchema,
};

/// One tool declaration paired with the executor that answers it.
#[derive(Clone)]
pub struct CapabilityTool {
    schema: ToolSchema,
    executor: Arc<dyn ToolExecutor>,
}

impl CapabilityTool {
    #[must_use]
    pub fn new(schema: ToolSchema, executor: Arc<dyn ToolExecutor>) -> Self {
        Self { schema, executor }
    }

    #[must_use]
    pub fn schema(&self) -> &ToolSchema {
        &self.schema
    }

    #[must_use]
    pub fn executor(&self) -> &Arc<dyn ToolExecutor> {
        &self.executor
    }
}

/// A role's contribution to the kernel.
///
/// Implement this in the role layer, not in the kernel. The kernel only knows
/// the bundle shape, so it can be tested without a role runtime.
pub trait CapabilityProvider: Send + Sync {
    /// Stable role name, unique within one capability set.
    fn name(&self) -> &str;

    /// Tools this role makes available to the model.
    fn tools(&self) -> Vec<CapabilityTool> {
        Vec::new()
    }

    /// Optional system-prompt fragment contributed by this role.
    fn prompt_fragment(&self) -> Option<String> {
        None
    }

    /// Extra directories scanned for skills while this role is active.
    fn skill_roots(&self) -> Vec<PathBuf> {
        Vec::new()
    }
}

/// Capability assembly failure.
#[derive(Debug, Error, Eq, PartialEq)]
pub enum CapabilityError {
    #[error("capability provider name is empty")]
    EmptyProviderName,
    #[error("duplicate capability provider name: {0}")]
    DuplicateProviderName(String),
    #[error("provider {provider} contributed a tool with an empty name")]
    EmptyToolName { provider: String },
    #[error("tool {tool} is contributed by both {first} and {second}")]
    DuplicateToolName {
        tool: String,
        first: String,
        second: String,
    },
}

/// Merged capabilities ready to be handed to the agent loop.
#[derive(Clone)]
pub struct CapabilitySet {
    provider_names: Vec<String>,
    schemas: Vec<ToolSchema>,
    router: Arc<CapabilityRouter>,
    system_prompt: Option<String>,
    skill_roots: Vec<PathBuf>,
}

impl CapabilitySet {
    /// Merge providers in the order given. Later providers cannot shadow an
    /// earlier provider's tool name.
    pub fn from_providers(
        providers: &[Arc<dyn CapabilityProvider>],
    ) -> Result<Self, CapabilityError> {
        let mut provider_names = Vec::with_capacity(providers.len());
        let mut schemas = Vec::new();
        let mut routes: BTreeMap<String, Arc<dyn ToolExecutor>> = BTreeMap::new();
        let mut tool_owners: BTreeMap<String, String> = BTreeMap::new();
        let mut prompt_fragments = Vec::new();
        let mut skill_roots = Vec::new();

        for provider in providers {
            let name = provider.name();
            if name.is_empty() {
                return Err(CapabilityError::EmptyProviderName);
            }
            if provider_names.iter().any(|existing| existing == name) {
                return Err(CapabilityError::DuplicateProviderName(name.to_owned()));
            }
            provider_names.push(name.to_owned());

            for tool in provider.tools() {
                let tool_name = tool.schema.name().to_owned();
                if tool_name.is_empty() {
                    return Err(CapabilityError::EmptyToolName {
                        provider: name.to_owned(),
                    });
                }
                if let Some(first) = tool_owners.get(&tool_name) {
                    return Err(CapabilityError::DuplicateToolName {
                        tool: tool_name.to_owned(),
                        first: first.clone(),
                        second: name.to_owned(),
                    });
                }
                tool_owners.insert(tool_name.to_owned(), name.to_owned());
                schemas.push(tool.schema);
                routes.insert(tool_name.to_owned(), tool.executor);
            }

            if let Some(fragment) = provider.prompt_fragment() {
                let fragment = fragment.trim();
                if !fragment.is_empty() {
                    prompt_fragments.push(fragment.to_owned());
                }
            }

            for root in provider.skill_roots() {
                if !skill_roots.contains(&root) {
                    skill_roots.push(root);
                }
            }
        }

        let system_prompt = if prompt_fragments.is_empty() {
            None
        } else {
            Some(prompt_fragments.join("\n\n"))
        };

        Ok(Self {
            provider_names,
            schemas,
            router: Arc::new(CapabilityRouter { routes }),
            system_prompt,
            skill_roots,
        })
    }

    #[must_use]
    pub fn provider_names(&self) -> &[String] {
        &self.provider_names
    }

    #[must_use]
    pub fn schemas(&self) -> &[ToolSchema] {
        &self.schemas
    }

    #[must_use]
    pub fn executor(&self) -> Arc<dyn ToolExecutor> {
        self.router.clone()
    }

    #[must_use]
    pub fn system_prompt(&self) -> Option<&str> {
        self.system_prompt.as_deref()
    }

    #[must_use]
    pub fn skill_roots(&self) -> &[PathBuf] {
        &self.skill_roots
    }

    /// Executor registered for one tool name, if any.
    #[must_use]
    pub fn route(&self, tool_name: &str) -> Option<Arc<dyn ToolExecutor>> {
        self.router.routes.get(tool_name).map(Arc::clone)
    }

    /// Borrow the executor for a routed tool name.
    ///
    /// Dispatching `execute` and `prepare` needs a reference whose lifetime is
    /// tied to the capability set; cloning the `Arc` would tie the returned
    /// future to a temporary instead.
    #[must_use]
    pub fn route_ref(&self, tool_name: &str) -> Option<&dyn ToolExecutor> {
        self.router
            .routes
            .get(tool_name)
            .map(|executor| executor.as_ref())
    }
}

/// Dispatch by tool name across the executors contributed by roles.
struct CapabilityRouter {
    routes: BTreeMap<String, Arc<dyn ToolExecutor>>,
}

impl ToolExecutor for CapabilityRouter {
    fn claim_profile(&self, tool_name: &str) -> ToolClaimProfile {
        self.routes
            .get(tool_name)
            .map_or_else(ToolClaimProfile::standard, |executor| {
                executor.claim_profile(tool_name)
            })
    }

    fn execution_mode(&self, tool_name: &str) -> ToolExecutionMode {
        self.routes
            .get(tool_name)
            .map_or(ToolExecutionMode::Exclusive, |executor| {
                executor.execution_mode(tool_name)
            })
    }

    fn execute(
        &self,
        request: ToolExecutionRequest,
        cancellation: tokio_util::sync::CancellationToken,
    ) -> ToolExecutionFuture<'_> {
        match self.routes.get(request.name()) {
            Some(executor) => executor.execute(request, cancellation),
            None => Box::pin(async { Err(ToolExecutorError::new("unknown capability tool")) }),
        }
    }

    fn prepare(
        &self,
        request: ToolExecutionRequest,
        cancellation: tokio_util::sync::CancellationToken,
    ) -> ToolPreparationFuture<'_> {
        match self.routes.get(request.name()) {
            Some(executor) => executor.prepare(request, cancellation),
            None => Box::pin(async { Err(ToolExecutorError::new("unknown capability tool")) }),
        }
    }

    fn shutdown(&self) -> ToolShutdownFuture<'_> {
        Box::pin(async move {
            let mut first_error = None;
            for executor in self.routes.values() {
                if let Err(error) = executor.shutdown().await
                    && first_error.is_none()
                {
                    first_error = Some(error);
                }
            }
            match first_error {
                Some(error) => Err(error),
                None => Ok(()),
            }
        })
    }
}

#[cfg(test)]
mod tests {
    use std::sync::{Arc, Mutex};

    use serde_json::json;

    use super::*;
    use crate::model::JsonValue;

    struct Recorder {
        label: &'static str,
        calls: Arc<Mutex<Vec<&'static str>>>,
        fail: bool,
    }

    impl ToolExecutor for Recorder {
        fn execute(
            &self,
            _request: ToolExecutionRequest,
            _cancellation: tokio_util::sync::CancellationToken,
        ) -> ToolExecutionFuture<'_> {
            Box::pin(async { Err(ToolExecutorError::new("unused")) })
        }

        fn shutdown(&self) -> ToolShutdownFuture<'_> {
            self.calls.lock().unwrap().push(self.label);
            let fail = self.fail;
            Box::pin(async move {
                if fail {
                    Err(ToolExecutorError::new("shutdown failed"))
                } else {
                    Ok(())
                }
            })
        }
    }

    struct Provider {
        name: &'static str,
        tool: &'static str,
        executor: Arc<Recorder>,
    }

    impl CapabilityProvider for Provider {
        fn name(&self) -> &str {
            self.name
        }

        fn tools(&self) -> Vec<CapabilityTool> {
            let schema = ToolSchema::new(
                self.tool,
                "test tool",
                JsonValue::new(json!({"type": "object"})).unwrap(),
            )
            .unwrap();
            vec![CapabilityTool::new(schema, self.executor.clone())]
        }
    }

    #[tokio::test]
    async fn shutdown_attempts_every_route_and_returns_the_first_error() {
        let calls = Arc::new(Mutex::new(Vec::new()));
        let set = CapabilitySet::from_providers(&[
            Arc::new(Provider {
                name: "one",
                tool: "one",
                executor: Arc::new(Recorder {
                    label: "one",
                    calls: Arc::clone(&calls),
                    fail: true,
                }),
            }),
            Arc::new(Provider {
                name: "two",
                tool: "two",
                executor: Arc::new(Recorder {
                    label: "two",
                    calls: Arc::clone(&calls),
                    fail: false,
                }),
            }),
        ])
        .expect("providers merge");

        let error = set.executor().shutdown().await.expect_err("first fails");
        assert!(!error.to_string().is_empty());
        assert_eq!(calls.lock().unwrap().as_slice(), ["one", "two"]);
    }

    #[tokio::test]
    async fn shutdown_reports_success_when_every_route_succeeds() {
        let calls = Arc::new(Mutex::new(Vec::new()));
        let set = CapabilitySet::from_providers(&[Arc::new(Provider {
            name: "one",
            tool: "one",
            executor: Arc::new(Recorder {
                label: "one",
                calls: Arc::clone(&calls),
                fail: false,
            }),
        })])
        .expect("providers merge");

        set.executor().shutdown().await.expect("shutdown succeeds");
        assert_eq!(calls.lock().unwrap().as_slice(), ["one"]);
    }
}
