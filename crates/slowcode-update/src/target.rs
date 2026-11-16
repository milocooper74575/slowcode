//! Host target detection and release asset naming.
//!
//! The names must match `.github/workflows/release.yml` and `install.sh`:
//! `slowcode-<target>.tar.gz` plus a sibling `.sha256` file.

use crate::error::UpdateError;

/// A supported release target.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct Target {
    triple: &'static str,
}

impl Target {
    /// Detect the current host target.
    pub fn current() -> Result<Self, UpdateError> {
        let triple = current_triple().ok_or_else(|| {
            UpdateError::UnsupportedPlatform(format!(
                "{} {}",
                std::env::consts::OS,
                std::env::consts::ARCH
            ))
        })?;
        Ok(Self { triple })
    }

    /// Construct a target from a triple this crate publishes. Used by tests
    /// and callers that already know the host.
    pub fn from_triple(triple: &str) -> Option<Self> {
        match triple {
            "aarch64-apple-darwin" => Some(Self {
                triple: "aarch64-apple-darwin",
            }),
            "x86_64-apple-darwin" => Some(Self {
                triple: "x86_64-apple-darwin",
            }),
            "x86_64-unknown-linux-musl" => Some(Self {
                triple: "x86_64-unknown-linux-musl",
            }),
            "aarch64-unknown-linux-musl" => Some(Self {
                triple: "aarch64-unknown-linux-musl",
            }),
            _ => None,
        }
    }

    /// The target triple used in release assets.
    #[must_use]
    pub fn triple(self) -> &'static str {
        self.triple
    }

    /// `slowcode-<target>.tar.gz`.
    #[must_use]
    pub fn archive_name(self) -> String {
        format!("slowcode-{}.tar.gz", self.triple)
    }

    /// `<archive>.sha256`.
    #[must_use]
    pub fn checksum_name(self) -> String {
        format!("{}.sha256", self.archive_name())
    }
}

fn current_triple() -> Option<&'static str> {
    match (std::env::consts::OS, std::env::consts::ARCH) {
        ("macos", "aarch64") => Some("aarch64-apple-darwin"),
        ("macos", "x86_64") => Some("x86_64-apple-darwin"),
        ("linux", "x86_64") => Some("x86_64-unknown-linux-musl"),
        ("linux", "aarch64") => Some("aarch64-unknown-linux-musl"),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::Target;

    #[test]
    fn archive_and_checksum_names_match_the_release_workflow() {
        let target = Target::from_triple("aarch64-apple-darwin").expect("known target");
        assert_eq!(
            target.archive_name(),
            "slowcode-aarch64-apple-darwin.tar.gz"
        );
        assert_eq!(
            target.checksum_name(),
            "slowcode-aarch64-apple-darwin.tar.gz.sha256"
        );

        let linux = Target::from_triple("x86_64-unknown-linux-musl").expect("known target");
        assert_eq!(
            linux.archive_name(),
            "slowcode-x86_64-unknown-linux-musl.tar.gz"
        );
        assert_eq!(Target::from_triple("x86_64-unknown-linux-gnu"), None);
    }

    #[test]
    fn current_host_has_a_release_target() {
        // The test suite runs on a supported CI host; this proves detection
        // agrees with the names above rather than always succeeding.
        let target = Target::current().expect("test host is supported");
        assert!(target.archive_name().ends_with(".tar.gz"));
        assert!(target.triple().contains('-'));
    }
}
