//! Audited read-only shell used by Plan and navigator profiles.

use std::{
    path::Path,
    sync::{Arc, Mutex},
};

use slowcode_kernel::{
    agent::{
        ToolClaimProfile, ToolExecutionFuture, ToolExecutionMode, ToolExecutionRequest,
        ToolExecutor, ToolPreparation, ToolPreparationFuture, ToolShutdownFuture,
    },
    model::ToolSchema,
    tools::{
        ToolRegistryBuildError,
        readonly_shell::{READONLY_SHELL_TOOL_NAME, ReadonlyShellTool},
    },
};
use tokio_util::sync::CancellationToken;

use super::WorkflowStore;

/// A read-only shell that writes `shell.readonly` into the active workflow.
#[derive(Clone, Debug)]
pub struct AuditedReadonlyShell {
    inner: ReadonlyShellTool,
    store: WorkflowStore,
    workflow_id: Option<String>,
    pending: Arc<Mutex<Vec<serde_json::Value>>>,
}

impl AuditedReadonlyShell {
    pub fn open(
        workspace: impl AsRef<Path>,
        store: WorkflowStore,
        workflow_id: Option<String>,
    ) -> Result<Self, ToolRegistryBuildError> {
        Self::open_with_sink(
            workspace,
            store,
            workflow_id,
            Arc::new(Mutex::new(Vec::new())),
        )
    }

    pub fn open_with_sink(
        workspace: impl AsRef<Path>,
        store: WorkflowStore,
        workflow_id: Option<String>,
        pending: Arc<Mutex<Vec<serde_json::Value>>>,
    ) -> Result<Self, ToolRegistryBuildError> {
        Ok(Self {
            inner: ReadonlyShellTool::open(workspace)?,
            store,
            workflow_id,
            pending,
        })
    }

    pub fn schema() -> Result<ToolSchema, ToolRegistryBuildError> {
        ReadonlyShellTool::schema()
    }

    pub fn tool_name() -> &'static str {
        READONLY_SHELL_TOOL_NAME
    }
}

impl ToolExecutor for AuditedReadonlyShell {
    fn claim_profile(&self, tool_name: &str) -> ToolClaimProfile {
        self.inner.claim_profile(tool_name)
    }

    fn execution_mode(&self, tool_name: &str) -> ToolExecutionMode {
        self.inner.execution_mode(tool_name)
    }

    fn execute(
        &self,
        request: ToolExecutionRequest,
        cancellation: CancellationToken,
    ) -> ToolExecutionFuture<'_> {
        let inner = self.inner.clone();
        let store = self.store.clone();
        let workflow_id = self.workflow_id.clone();
        let pending = Arc::clone(&self.pending);
        Box::pin(async move {
            let result = inner.execute(request, cancellation).await;
            if let Ok(result) = &result
                && let Some(meta) = result.meta()
                && meta.as_value().get("kind").and_then(|value| value.as_str())
                    == Some("shell.readonly")
            {
                let id = workflow_id
                    .clone()
                    .or_else(|| store.active_id().ok().flatten());
                if let Some(id) = id {
                    let _ = store.record_shell_readonly(&id, meta.as_value().clone());
                } else if let Ok(mut pending) = pending.lock() {
                    pending.push(meta.as_value().clone());
                }
            }
            result
        })
    }

    fn prepare(
        &self,
        request: ToolExecutionRequest,
        cancellation: CancellationToken,
    ) -> ToolPreparationFuture<'_> {
        let execution = self.execute(request, cancellation);
        Box::pin(async move { execution.await.map(ToolPreparation::Complete) })
    }

    fn shutdown(&self) -> ToolShutdownFuture<'_> {
        self.inner.shutdown()
    }
}

/// Convenience for tests and callers that only need the tool contract.
pub fn readonly_shell_schema() -> Result<ToolSchema, ToolRegistryBuildError> {
    ReadonlyShellTool::schema()
}

/// Shared handle type for agent composition.
pub type ReadonlyShellHandle = Arc<AuditedReadonlyShell>;
