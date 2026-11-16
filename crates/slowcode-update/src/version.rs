//! Semantic-ish version parsing and ordering for release tags.
//!
//! Release tags are `v<major>.<minor>.<patch>` optionally followed by a
//! pre-release suffix. Missing components compare as zero, so `v0.2` is equal
//! to `v0.2.0`. A pre-release sorts before its release, and build metadata is
//! ignored.

use std::{cmp::Ordering, fmt};

/// One parsed release version.
#[derive(Clone, Debug, Eq)]
pub struct Version {
    parts: Vec<u64>,
    pre: Option<String>,
}

impl PartialEq for Version {
    fn eq(&self, other: &Self) -> bool {
        self.cmp(other) == std::cmp::Ordering::Equal
    }
}

impl Version {
    /// Parse a tag such as `v1.2.3`, `1.2`, or `v1.2.3-rc.1`.
    #[must_use]
    pub fn parse(input: &str) -> Option<Self> {
        let input = input.trim();
        if input.is_empty() {
            return None;
        }
        let input = input
            .strip_prefix('v')
            .or_else(|| input.strip_prefix('V'))
            .unwrap_or(input);
        let input = input.split('+').next().unwrap_or(input);
        let (core, pre) = match input.split_once('-') {
            Some((core, pre)) if !pre.is_empty() => (core, Some(pre.to_owned())),
            Some(_) => return None,
            None => (input, None),
        };
        let mut parts = Vec::new();
        for part in core.split('.') {
            if part.is_empty() || !part.bytes().all(|byte| byte.is_ascii_digit()) {
                return None;
            }
            parts.push(part.parse::<u64>().ok()?);
        }
        if parts.is_empty() || parts.len() > 4 {
            return None;
        }
        Some(Self { parts, pre })
    }

    /// The crate's compiled version.
    #[must_use]
    pub fn current() -> Self {
        Self::parse(env!("CARGO_PKG_VERSION")).expect("CARGO_PKG_VERSION is a valid version")
    }

    fn part(&self, index: usize) -> u64 {
        self.parts.get(index).copied().unwrap_or(0)
    }

    /// True when `self` is strictly newer than `other`.
    #[must_use]
    pub fn is_newer_than(&self, other: &Self) -> bool {
        self.cmp(other) == Ordering::Greater
    }
}

impl Ord for Version {
    fn cmp(&self, other: &Self) -> Ordering {
        let core = (0..4)
            .map(|index| self.part(index).cmp(&other.part(index)))
            .find(|ordering| ordering.is_ne());
        if let Some(ordering) = core {
            return ordering;
        }
        match (&self.pre, &other.pre) {
            (None, None) => Ordering::Equal,
            (None, Some(_)) => Ordering::Greater,
            (Some(_), None) => Ordering::Less,
            (Some(left), Some(right)) => compare_prerelease(left, right),
        }
    }
}

impl PartialOrd for Version {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}

fn compare_prerelease(left: &str, right: &str) -> Ordering {
    let mut left = left.split('.');
    let mut right = right.split('.');
    loop {
        match (left.next(), right.next()) {
            (None, None) => return Ordering::Equal,
            (None, Some(_)) => return Ordering::Less,
            (Some(_), None) => return Ordering::Greater,
            (Some(left), Some(right)) => {
                let ordering = compare_identifier(left, right);
                if ordering.is_ne() {
                    return ordering;
                }
            }
        }
    }
}

fn compare_identifier(left: &str, right: &str) -> Ordering {
    match (left.parse::<u64>(), right.parse::<u64>()) {
        (Ok(left), Ok(right)) => left.cmp(&right),
        (Ok(_), Err(_)) => Ordering::Less,
        (Err(_), Ok(_)) => Ordering::Greater,
        (Err(_), Err(_)) => left.cmp(right),
    }
}

impl fmt::Display for Version {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        let core = self
            .parts
            .iter()
            .map(u64::to_string)
            .collect::<Vec<_>>()
            .join(".");
        match &self.pre {
            Some(pre) => write!(formatter, "{core}-{pre}"),
            None => write!(formatter, "{core}"),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::Version;

    fn version(value: &str) -> Version {
        Version::parse(value).expect("valid version")
    }

    #[test]
    fn parses_tags_with_and_without_prefix() {
        assert_eq!(version("v1.2.3").to_string(), "1.2.3");
        assert_eq!(version("1.2").to_string(), "1.2");
        assert_eq!(version(" V0.1.0 ").to_string(), "0.1.0");
        assert_eq!(version("v1.2.3-rc.1+build.5").to_string(), "1.2.3-rc.1");
        assert!(Version::parse("").is_none());
        assert!(Version::parse("v").is_none());
        assert!(Version::parse("v1.x").is_none());
        assert!(Version::parse("v1..2").is_none());
        assert!(Version::parse("release-1").is_none());
    }

    #[test]
    fn compares_numeric_components_not_strings() {
        assert!(version("0.10.0").is_newer_than(&version("0.9.0")));
        assert!(version("1.0.0").is_newer_than(&version("0.99.99")));
        assert!(!version("0.2.0").is_newer_than(&version("0.2")));
        assert_eq!(version("0.2"), version("0.2.0.0"));
        assert!(version("1.2.10").is_newer_than(&version("1.2.9")));
    }

    #[test]
    fn prereleases_sort_before_the_release() {
        assert!(version("1.0.0").is_newer_than(&version("1.0.0-rc.1")));
        assert!(version("1.0.0-rc.2").is_newer_than(&version("1.0.0-rc.1")));
        assert!(version("1.0.0-beta").is_newer_than(&version("1.0.0-alpha")));
        assert!(version("1.0.0-alpha.1").is_newer_than(&version("1.0.0-alpha")));
        assert!(!version("1.0.0-rc.1").is_newer_than(&version("1.0.0")));
    }

    #[test]
    fn current_version_parses() {
        assert!(Version::current().to_string().contains('.'));
    }
}
