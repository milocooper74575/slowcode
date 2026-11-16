//! Slowcode update awareness and user-confirmed self-update.
//!
//! This crate owns version comparison, the 24 hour release-check cache, the
//! opt-out policy, the GitHub release source, checksum verification, and the
//! atomic replacement of the running executable. It never updates silently and
//! never runs from a model tool: the CLI and TUI call it only after a user
//! action.
//!
//! The network and filesystem seams are injectable through
//! [`source::UpdateTransport`], so unit tests never touch the network.

pub mod cache;
pub mod check;
pub mod client;
pub mod error;
pub mod install;
pub mod policy;
pub mod source;
pub mod target;
pub mod version;

pub use check::UpdateInfo;
pub use client::HttpTransport;
pub use error::UpdateError;
pub use install::{InstallOutcome, InstallRequest, install as install_executable};
pub use policy::{CheckDecision, Environment, SkipReason, startup_decision};
pub use source::{Release, UpdateTransport};
pub use target::Target;
pub use version::Version;

/// Check for a newer release, reusing the cache. Does not apply startup opt-outs.
pub async fn check_now(environment: &Environment) -> Result<UpdateInfo, UpdateError> {
    let transport = HttpTransport::from_environment(environment)?;
    check::check_default(&transport, environment.cache_path().as_deref()).await
}

/// The startup check: opt-outs apply and only a newer release is returned.
pub async fn startup_check(environment: &Environment) -> Result<Option<UpdateInfo>, UpdateError> {
    if startup_decision(environment, cfg!(debug_assertions)) != CheckDecision::Check {
        return Ok(None);
    }
    let info = check_now(environment).await?;
    Ok(info.is_available().then_some(info))
}

/// Download and replace the current executable. Callers must have obtained
/// explicit user confirmation first.
pub async fn install_now(environment: &Environment) -> Result<InstallOutcome, UpdateError> {
    if let Some(pin) = environment
        .pinned_version
        .as_deref()
        .map(str::trim)
        .filter(|pin| !pin.is_empty())
    {
        return Err(UpdateError::PinnedVersion(pin.to_owned()));
    }
    let exe_path = std::env::current_exe()?;
    let target = Target::current()?;
    let transport = HttpTransport::from_environment(environment)?;
    let request = InstallRequest {
        transport: &transport,
        exe_path,
        target,
        current: Version::current(),
    };
    install_executable(&request).await
}

/// `startup_check` against the real process environment.
pub async fn startup_check_from_env() -> Result<Option<UpdateInfo>, UpdateError> {
    startup_check(&Environment::from_process()).await
}

/// `check_now` against the real process environment.
pub async fn check_from_env() -> Result<UpdateInfo, UpdateError> {
    check_now(&Environment::from_process()).await
}

/// `install_now` against the real process environment.
pub async fn install_from_env() -> Result<InstallOutcome, UpdateError> {
    install_now(&Environment::from_process()).await
}
