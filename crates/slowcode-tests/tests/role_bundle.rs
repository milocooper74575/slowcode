//! Role bundles own the tools and prompt a role contributes.
//!
//! Disposing a role must close its contributed tool executors and make the
//! stale capability provider stop advertising the role's prompt, skill roots
//! and tools. A read-only role must not advertise contributed tools at all.

use std::sync::{
    Arc,
    atomic::{AtomicUsize, Ordering},
};

use serde_json::json;
use slowcode_kernel::{
    agent::{
        ToolExecutionFuture, ToolExecutionRequest, ToolExecutionResult, ToolExecutor,
        ToolShutdownFuture,
    },
    capability::{CapabilitySet, CapabilityTool},
    model::{ContentBlock, JsonValue, ToolSchema},
};
use slowcode_roles::{RoleFiber, RoleName, driver, navigator};
use tokio_util::sync::CancellationToken;

struct CountingTool {
    shutdowns: Arc<AtomicUsize>,
}

impl ToolExecutor for CountingTool {
    fn execute(
        &self,
        _request: ToolExecutionRequest,
        _cancellation: CancellationToken,
    ) -> ToolExecutionFuture<'_> {
        Box::pin(async {
            Ok(ToolExecutionResult::success(vec![
                ContentBlock::text("role-tool-ok").expect("text fits"),
            ])
            .expect("result is valid"))
        })
    }

    fn shutdown(&self) -> ToolShutdownFuture<'_> {
        self.shutdowns.fetch_add(1, Ordering::SeqCst);
        Box::pin(async { Ok(()) })
    }
}

fn schema(name: &str) -> ToolSchema {
    ToolSchema::new(
        name,
        "test tool",
        JsonValue::new(json!({"type": "object"})).expect("schema object is valid"),
    )
    .expect("schema is valid")
}

fn role_tool(name: &str, shutdowns: &Arc<AtomicUsize>) -> CapabilityTool {
    CapabilityTool::new(
        schema(name),
        Arc::new(CountingTool {
            shutdowns: Arc::clone(shutdowns),
        }),
    )
}

#[tokio::test]
async fn disposing_a_role_hides_its_resources_and_closes_its_tools() {
    let shutdowns = Arc::new(AtomicUsize::new(0));
    let role = RoleName::Driver
        .spec()
        .with_tool(role_tool("role_tool", &shutdowns));
    let fiber = RoleFiber::spawn(Arc::new(role))
        .await
        .expect("role fiber spawns");
    let provider = fiber
        .capability_provider()
        .expect("active role exposes its bundle");

    assert_eq!(provider.tools().len(), 1, "the active role owns one tool");
    assert!(
        provider.prompt_fragment().is_some(),
        "the active role owns its prompt"
    );
    assert_eq!(
        provider.skill_roots().len(),
        1,
        "the driver spec owns one role skill root"
    );

    fiber.dispose().await.expect("role fiber disposes");

    assert_eq!(
        shutdowns.load(Ordering::SeqCst),
        1,
        "disposing the role must close its contributed executor exactly once"
    );
    assert!(
        provider.tools().is_empty(),
        "a disposed role must not advertise its tools"
    );
    assert!(
        provider.prompt_fragment().is_none(),
        "a disposed role must not advertise its prompt"
    );
    assert!(
        provider.skill_roots().is_empty(),
        "a disposed role must not advertise its skill roots"
    );
}

#[tokio::test]
async fn a_read_only_role_does_not_advertise_contributed_tools() {
    let navigator_shutdowns = Arc::new(AtomicUsize::new(0));
    let navigator = navigator().with_tool(role_tool("navigator_extra", &navigator_shutdowns));
    let navigator_fiber = RoleFiber::spawn(Arc::new(navigator))
        .await
        .expect("navigator fiber spawns");
    let navigator_provider = navigator_fiber
        .capability_provider()
        .expect("navigator is active");
    let navigator_set = CapabilitySet::from_providers(&[Arc::clone(&navigator_provider)])
        .expect("navigator capabilities merge");

    assert!(
        navigator_provider.tools().is_empty(),
        "navigator must not advertise a tool without a read-only marker"
    );
    assert!(navigator_set.schemas().is_empty());
    assert!(
        navigator_set.route("navigator_extra").is_none(),
        "an unadvertised role tool must not be routable"
    );
    navigator_fiber
        .dispose()
        .await
        .expect("navigator fiber disposes");
    assert_eq!(
        navigator_shutdowns.load(Ordering::SeqCst),
        1,
        "a filtered read-only role tool is still closed exactly once"
    );

    let driver_shutdowns = Arc::new(AtomicUsize::new(0));
    let driver = driver().with_tool(role_tool("driver_extra", &driver_shutdowns));
    let driver_fiber = RoleFiber::spawn(Arc::new(driver))
        .await
        .expect("driver fiber spawns");
    let driver_provider = driver_fiber
        .capability_provider()
        .expect("driver is active");
    let driver_set = CapabilitySet::from_providers(&[Arc::clone(&driver_provider)])
        .expect("driver capabilities merge");
    assert_eq!(driver_provider.tools().len(), 1);
    assert!(driver_set.route("driver_extra").is_some());
    driver_fiber.dispose().await.expect("driver fiber disposes");
    assert_eq!(
        driver_shutdowns.load(Ordering::SeqCst),
        1,
        "the driver role tool closes exactly once"
    );
}
