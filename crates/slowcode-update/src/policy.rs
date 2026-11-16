//! Opt-out rules, cache locations, and startup-check decisions.
//!
//! All process environment reads happen here so tests can construct an
//! [`Environment`] directly and never depend on the host's variables.

use std::{
    env,
    path::{Path, PathBuf},
};

/// The subset of the process environment update code reads.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct Environment {
    pub update_check: Option<String>,
    pub ci: Option<String>,
    pub pinned_version: Option<String>,
    pub repo: Option<String>,
    pub update_url: Option<String>,
    pub home: Option<String>,
    pub xdg_cache_home: Option<String>,
    pub local_app_data: Option<String>,
}

impl Environment {
    /// Read the real process environment once.
    #[must_use]
    pub fn from_process() -> Self {
        Self {
            update_check: env::var("SLOWCODE_UPDATE_CHECK").ok(),
            ci: env::var("CI").ok(),
            pinned_version: env::var("SLOWCODE_VERSION").ok(),
            repo: env::var("SLOWCODE_REPO").ok(),
            update_url: env::var("SLOWCODE_UPDATE_URL").ok(),
            home: env::var("HOME").ok(),
            xdg_cache_home: env::var("XDG_CACHE_HOME").ok(),
            local_app_data: env::var("LOCALAPPDATA").ok(),
        }
    }

    /// The repository releases are read from.
    #[must_use]
    pub fn repo(&self) -> String {
        self.repo
            .as_deref()
            .map(str::trim)
            .filter(|repo| !repo.is_empty())
            .unwrap_or("milocooper74575/slowcode")
            .to_owned()
    }

    /// Test seam: an explicit release base URL replaces GitHub.
    #[must_use]
    pub fn update_base(&self) -> Option<&str> {
        self.update_url
            .as_deref()
            .map(str::trim)
            .filter(|value| !value.is_empty())
    }

    /// Where the last successful check is cached.
    #[must_use]
    pub fn cache_path(&self) -> Option<PathBuf> {
        cache_path_for(OsKind::current(), self)
    }
}

/// Directory convention used for the cache file.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum OsKind {
    MacOs,
    Windows,
    Unix,
}

impl OsKind {
    #[must_use]
    pub fn current() -> Self {
        if cfg!(target_os = "macos") {
            Self::MacOs
        } else if cfg!(target_os = "windows") {
            Self::Windows
        } else {
            Self::Unix
        }
    }
}

/// Resolve `<cache dir>/slowcode/update-check.json` for a platform.
#[must_use]
pub fn cache_path_for(os: OsKind, environment: &Environment) -> Option<PathBuf> {
    let directory = match os {
        OsKind::MacOs => PathBuf::from(environment.home.as_deref()?)
            .join("Library")
            .join("Caches")
            .join("slowcode"),
        OsKind::Windows => PathBuf::from(environment.local_app_data.as_deref()?).join("slowcode"),
        OsKind::Unix => environment
            .xdg_cache_home
            .as_deref()
            .filter(|value| !value.is_empty())
            .map(PathBuf::from)
            .or_else(|| {
                environment
                    .home
                    .as_deref()
                    .map(|home| Path::new(home).join(".cache"))
            })?
            .join("slowcode"),
    };
    Some(directory.join("update-check.json"))
}

/// Why the startup check was skipped.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum SkipReason {
    Disabled,
    Ci,
    DebugBuild,
    PinnedVersion,
}

impl SkipReason {
    /// Short explanation suitable for `--check` output and tests.
    #[must_use]
    pub fn message(self) -> &'static str {
        match self {
            Self::Disabled => "update checks are disabled by SLOWCODE_UPDATE_CHECK",
            Self::Ci => "update checks are off in CI",
            Self::DebugBuild => "update checks are off in debug builds",
            Self::PinnedVersion => "update prompts are off because SLOWCODE_VERSION is set",
        }
    }
}

/// Whether the startup check should run.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum CheckDecision {
    Check,
    Skip(SkipReason),
}

/// Decide from the environment and build profile. Pinning wins over the
/// automatic check; an explicit `slowcode update --check` bypasses this.
#[must_use]
pub fn startup_decision(environment: &Environment, debug_build: bool) -> CheckDecision {
    if environment
        .update_check
        .as_deref()
        .is_some_and(disabled_value)
    {
        return CheckDecision::Skip(SkipReason::Disabled);
    }
    if environment
        .ci
        .as_deref()
        .is_some_and(|value| !value.trim().is_empty() && value.trim() != "0")
    {
        return CheckDecision::Skip(SkipReason::Ci);
    }
    if debug_build {
        return CheckDecision::Skip(SkipReason::DebugBuild);
    }
    if environment
        .pinned_version
        .as_deref()
        .is_some_and(|value| !value.trim().is_empty())
    {
        return CheckDecision::Skip(SkipReason::PinnedVersion);
    }
    CheckDecision::Check
}

fn disabled_value(value: &str) -> bool {
    matches!(
        value.trim().to_ascii_lowercase().as_str(),
        "0" | "false" | "no" | "off"
    )
}

#[cfg(test)]
mod tests {
    use std::path::Path;

    use super::{CheckDecision, Environment, OsKind, SkipReason, cache_path_for, startup_decision};

    fn environment() -> Environment {
        Environment {
            home: Some("/home/user".to_owned()),
            ..Environment::default()
        }
    }

    #[test]
    fn startup_check_runs_by_default_in_release_builds() {
        assert_eq!(
            startup_decision(&environment(), false),
            CheckDecision::Check
        );
    }

    #[test]
    fn every_opt_out_skips_the_startup_check() {
        for value in ["0", "false", "NO", "off"] {
            let mut environment = environment();
            environment.update_check = Some(value.to_owned());
            assert_eq!(
                startup_decision(&environment, false),
                CheckDecision::Skip(SkipReason::Disabled),
                "{value}"
            );
        }

        let mut ci = environment();
        ci.ci = Some("true".to_owned());
        assert_eq!(
            startup_decision(&ci, false),
            CheckDecision::Skip(SkipReason::Ci)
        );

        let mut debug = environment();
        debug.update_check = Some("1".to_owned());
        assert_eq!(
            startup_decision(&debug, true),
            CheckDecision::Skip(SkipReason::DebugBuild)
        );

        let mut pinned = environment();
        pinned.pinned_version = Some("v0.1.0".to_owned());
        assert_eq!(
            startup_decision(&pinned, false),
            CheckDecision::Skip(SkipReason::PinnedVersion)
        );
    }

    #[test]
    fn empty_or_zero_values_do_not_count_as_opt_outs() {
        let mut environment = environment();
        environment.update_check = Some(String::new());
        environment.ci = Some("0".to_owned());
        environment.pinned_version = Some("  ".to_owned());
        assert_eq!(startup_decision(&environment, false), CheckDecision::Check);
    }

    #[test]
    fn cache_paths_follow_platform_conventions() {
        let environment = Environment {
            home: Some("/home/user".to_owned()),
            xdg_cache_home: None,
            local_app_data: Some("/AppData/Local".to_owned()),
            ..Environment::default()
        };
        assert_eq!(
            cache_path_for(OsKind::MacOs, &environment).expect("path"),
            Path::new("/home/user/Library/Caches/slowcode/update-check.json")
        );
        assert_eq!(
            cache_path_for(OsKind::Unix, &environment).expect("path"),
            Path::new("/home/user/.cache/slowcode/update-check.json")
        );

        let xdg = Environment {
            xdg_cache_home: Some("/cache".to_owned()),
            ..environment.clone()
        };
        assert_eq!(
            cache_path_for(OsKind::Unix, &xdg).expect("path"),
            Path::new("/cache/slowcode/update-check.json")
        );
        assert_eq!(
            cache_path_for(OsKind::Windows, &environment).expect("path"),
            Path::new("/AppData/Local/slowcode/update-check.json")
        );

        assert!(cache_path_for(OsKind::MacOs, &Environment::default()).is_none());
    }
}
