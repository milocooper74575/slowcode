//! Cordis-backed role fibers.
//!
//! One role is one Cordis fiber guarding one [`RoleBundle`]. The bundle and its
//! capability set are built before admission. Cleanup revokes the bundle first,
//! then closes its contributed tool executors, so a stale handle cannot keep
//! calling role resources.

use std::{convert::Infallible, sync::Arc};

use cordis_core::{
    Context, Plugin, PreparedPlugin,
    effect::EffectRegistrationError,
    lifecycle::{FiberHandle, LifecycleRecursion, SpawnError},
};
use slowcode_kernel::capability::{CapabilityError, CapabilityProvider};
use thiserror::Error;

use crate::{RoleBundle, StaticRole};

/// A live role fiber and the capability bundle it guards.
pub struct RoleFiber {
    context: Context,
    handle: FiberHandle,
    bundle: Arc<RoleBundle>,
}

impl std::fmt::Debug for RoleFiber {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("RoleFiber")
            .field("role", &self.bundle.role().name())
            .field("active", &self.is_active())
            .finish_non_exhaustive()
    }
}

impl RoleFiber {
    /// Prepare the role plugin and admit it to a fresh Cordis runtime.
    ///
    /// Spawn returns only after `apply` has completed, so a successful result
    /// is already active and its capability bundle is ready for assembly. A
    /// duplicate role tool name fails here, before any fiber exists.
    pub async fn spawn(role: Arc<StaticRole>) -> Result<Self, RoleFiberError> {
        let bundle = Arc::new(RoleBundle::open(role)?);
        let plugin = RolePlugin {
            bundle: Arc::clone(&bundle),
        };
        let input = match plugin.prepare(Arc::clone(&bundle)) {
            Ok(input) => input,
            Err(never) => match never {},
        };
        let prepared = PreparedPlugin::from_input(plugin, input);
        let context = Context::new();
        let handle = context.spawn(prepared).await?;
        if !bundle.is_active() {
            let _ = handle.dispose().await;
            return Err(RoleFiberError::InactiveAfterSpawn);
        }
        Ok(Self {
            context,
            handle,
            bundle,
        })
    }

    /// The static role definition owned by this fiber.
    #[must_use]
    pub fn role(&self) -> &StaticRole {
        self.bundle.role()
    }

    /// Whether `apply` has run and the fiber has not been disposed.
    #[must_use]
    pub fn is_active(&self) -> bool {
        self.bundle.is_active()
    }

    /// Hand the role's capability bundle to assembly.
    pub fn capability_provider(&self) -> Result<Arc<dyn CapabilityProvider>, RoleFiberError> {
        self.bundle.ensure_active()?;
        Ok(Arc::clone(&self.bundle) as Arc<dyn CapabilityProvider>)
    }

    /// Borrow the active bundle for assembly.
    pub(crate) fn bundle(&self) -> Result<&RoleBundle, RoleFiberError> {
        self.bundle.ensure_active()?;
        Ok(&self.bundle)
    }

    /// Dispose the fiber and wait for its cleanup to finish.
    ///
    /// Resource shutdown is attempted before the lifecycle drain, so its error
    /// is returned to the caller. The registered cleanup stays as a safety net
    /// and sees the already-closed bundle.
    pub async fn dispose(self) -> Result<(), RoleFiberError> {
        let Self {
            context,
            handle,
            bundle,
        } = self;
        let shutdown = bundle.shutdown().await;
        let disposed = handle.dispose().await;
        drop(context);
        disposed?;
        if bundle.is_active() {
            return Err(RoleFiberError::CleanupDidNotRun);
        }
        shutdown
    }
}

struct RoleFiberInput {
    bundle: Arc<RoleBundle>,
}

struct RolePlugin {
    bundle: Arc<RoleBundle>,
}

impl Plugin for RolePlugin {
    type Config = Arc<RoleBundle>;
    type Input = RoleFiberInput;
    type PrepareError = Infallible;
    type ApplyError = EffectRegistrationError;

    fn prepare(&self, bundle: Self::Config) -> Result<Self::Input, Self::PrepareError> {
        Ok(RoleFiberInput { bundle })
    }

    async fn apply(&self, ctx: Context, input: &Self::Input) -> Result<(), Self::ApplyError> {
        let _ = input.bundle.role().name();
        let _ = self.bundle.role().name();
        let bundle = Arc::clone(&self.bundle);
        let _cleanup = ctx.effect(move || async move {
            bundle.close_resources().await;
        })?;
        self.bundle.activate();
        Ok(())
    }
}

/// Role fiber lifecycle failure.
#[derive(Debug, Error)]
pub enum RoleFiberError {
    #[error("role fiber could not be spawned: {0}")]
    Spawn(#[from] SpawnError),
    #[error("role fiber could not be disposed: {0}")]
    Dispose(#[from] LifecycleRecursion),
    #[error("role fiber apply did not activate the capability bundle")]
    InactiveAfterSpawn,
    #[error("role fiber is not active")]
    Inactive,
    #[error("role fiber cleanup did not mark the capability bundle inactive")]
    CleanupDidNotRun,
    #[error("role capability bundle could not be created: {0}")]
    Capability(#[from] CapabilityError),
    #[error("role capability tools did not shut down: {0}")]
    Shutdown(String),
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{NAVIGATOR, navigator};

    #[tokio::test]
    async fn fiber_spawns_apply_runs_and_dispose_cleans_up() {
        let fiber = RoleFiber::spawn(Arc::new(navigator()))
            .await
            .expect("role fiber spawns");
        assert!(fiber.is_active());
        assert_eq!(
            fiber.capability_provider().expect("active").name(),
            NAVIGATOR
        );

        let bundle = Arc::clone(&fiber.bundle);
        fiber.dispose().await.expect("role fiber disposes");
        assert!(!bundle.is_active());
    }

    #[tokio::test]
    async fn fibers_are_independent_and_the_runtime_can_be_reused() {
        let first = RoleFiber::spawn(Arc::new(navigator()))
            .await
            .expect("first fiber spawns");
        let second = RoleFiber::spawn(Arc::new(crate::driver()))
            .await
            .expect("second fiber spawns");
        assert_eq!(first.role().name(), NAVIGATOR);
        assert_eq!(second.role().name(), crate::DRIVER);

        let first_bundle = Arc::clone(&first.bundle);
        first.dispose().await.expect("first fiber disposes");
        assert!(!first_bundle.is_active());
        assert!(
            second.is_active(),
            "disposing one fiber must not stop another"
        );

        second.dispose().await.expect("second fiber disposes");
        let third = RoleFiber::spawn(Arc::new(navigator()))
            .await
            .expect("a new fiber spawns after disposal");
        assert!(third.is_active());
        third.dispose().await.expect("third fiber disposes");
    }

    #[tokio::test]
    async fn an_invalid_role_bundle_fails_before_any_fiber_is_admitted() {
        let role = StaticRole::new("", "prompt");
        let error = RoleFiber::spawn(Arc::new(role))
            .await
            .expect_err("an empty provider name must fail");
        assert!(matches!(error, RoleFiberError::Capability(_)));
    }
}
