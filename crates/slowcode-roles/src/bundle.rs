//! Role-owned resources behind one Cordis fiber.
//!
//! A [`RoleBundle`] computes the role capability set once and owns its
//! contributed tool executors. Disposal closes those executors and revokes the
//! bundle before any await, so a handle kept across a switch can no longer
//! advertise prompt, Skill roots or tools.
//!
//! Resources that survive a role switch, such as MCP connections, stay with the
//! Runtime and are installed as shared extra tools.

use std::{
    path::PathBuf,
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, Ordering},
    },
};

use slowcode_kernel::agent::ToolExecutor;
use slowcode_kernel::capability::{CapabilityProvider, CapabilitySet, CapabilityTool};

use crate::{RoleFiberError, StaticRole};

/// One role's prompt, Skill roots, contributed tools and lifecycle state.
pub struct RoleBundle {
    role: Arc<StaticRole>,
    capabilities: CapabilitySet,
    executor: Arc<dyn ToolExecutor>,
    tools: Vec<CapabilityTool>,
    read_only: bool,
    active: AtomicBool,
    shutdown: Mutex<ShutdownState>,
}

#[derive(Default)]
struct ShutdownState {
    closed: bool,
    error: Option<String>,
}

impl std::fmt::Debug for RoleBundle {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("RoleBundle")
            .field("role", &self.role.name())
            .field("read_only", &self.read_only)
            .field("active", &self.is_active())
            .field("tools", &self.capabilities.schemas().len())
            .finish_non_exhaustive()
    }
}

impl RoleBundle {
    /// Compute the capability set for one role.
    ///
    /// Read-only roles keep their prompt and Skill roots but contribute no
    /// tools until a tool exposes a read-only marker.
    pub fn open(role: Arc<StaticRole>) -> Result<Self, RoleFiberError> {
        let read_only = role.is_read_only();
        // The full set is still owned by the role, so its executors are closed
        // even when the role does not advertise them.
        let owned =
            CapabilitySet::from_providers(&[Arc::clone(&role) as Arc<dyn CapabilityProvider>])?;
        let capabilities = if read_only {
            CapabilitySet::from_providers(&[Arc::new(role.without_tools())])?
        } else {
            owned.clone()
        };
        let executor = owned.executor();
        let tools = capabilities
            .schemas()
            .iter()
            .filter_map(|schema| {
                capabilities
                    .route(schema.name())
                    .map(|executor| CapabilityTool::new(schema.clone(), executor))
            })
            .collect();
        Ok(Self {
            role,
            capabilities,
            executor,
            tools,
            read_only,
            active: AtomicBool::new(false),
            shutdown: Mutex::new(ShutdownState::default()),
        })
    }

    /// The static role definition this bundle was built from.
    #[must_use]
    pub fn role(&self) -> &StaticRole {
        &self.role
    }

    /// Whether the role only permits read-only tools.
    #[must_use]
    pub fn read_only(&self) -> bool {
        self.read_only
    }

    /// Whether the bundle is admitted and has not been closed.
    #[must_use]
    pub fn is_active(&self) -> bool {
        self.active.load(Ordering::SeqCst)
    }

    /// Reject access once the bundle has been revoked.
    pub(crate) fn ensure_active(&self) -> Result<(), RoleFiberError> {
        if self.is_active() {
            Ok(())
        } else {
            Err(RoleFiberError::Inactive)
        }
    }

    /// The capability set owned by this bundle.
    pub fn capabilities(&self) -> Result<&CapabilitySet, RoleFiberError> {
        self.ensure_active()?;
        Ok(&self.capabilities)
    }

    pub(crate) fn activate(&self) {
        self.active.store(true, Ordering::SeqCst);
    }

    /// Revoke the bundle and close its contributed executors once.
    ///
    /// The bundle stops advertising its resources before the first await, so a
    /// failing shutdown cannot leave callable tools behind.
    pub async fn shutdown(&self) -> Result<(), RoleFiberError> {
        let first = {
            let mut state = self.shutdown.lock().expect("bundle shutdown lock");
            if state.closed {
                return state
                    .error
                    .clone()
                    .map_or(Ok(()), |error| Err(RoleFiberError::Shutdown(error)));
            }
            state.closed = true;
            self.active.store(false, Ordering::SeqCst);
            Arc::clone(&self.executor)
        };
        match first.shutdown().await {
            Ok(()) => Ok(()),
            Err(error) => {
                let message = error.to_string();
                self.shutdown.lock().expect("bundle shutdown lock").error = Some(message.clone());
                Err(RoleFiberError::Shutdown(message))
            }
        }
    }

    /// Close resources without reporting the failure.
    ///
    /// The Cordis cleanup path has no error sink, so it uses this form. Explicit
    /// [`crate::RoleFiber::dispose`] awaits [`Self::shutdown`] and reports.
    pub(crate) async fn close_resources(&self) {
        let _ = self.shutdown().await;
    }
}

impl CapabilityProvider for RoleBundle {
    fn name(&self) -> &str {
        self.role.name()
    }

    fn tools(&self) -> Vec<CapabilityTool> {
        if self.is_active() {
            self.tools.clone()
        } else {
            Vec::new()
        }
    }

    fn prompt_fragment(&self) -> Option<String> {
        if self.is_active() {
            self.capabilities.system_prompt().map(str::to_owned)
        } else {
            None
        }
    }

    fn skill_roots(&self) -> Vec<PathBuf> {
        if self.is_active() {
            self.capabilities.skill_roots().to_vec()
        } else {
            Vec::new()
        }
    }
}
