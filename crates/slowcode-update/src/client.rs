//! GitHub release source and download client.
//!
//! The API is called without a token and with a short timeout. Assets are only
//! fetched from the release base URL or from `SLOWCODE_UPDATE_URL`, which is a
//! test seam. Non-HTTPS URLs are rejected except for loopback hosts.

use std::time::Duration;

use serde::Deserialize;

use crate::{
    error::UpdateError,
    policy::Environment,
    source::{BoxFuture, Release, UpdateTransport},
    version::Version,
};

const USER_AGENT: &str = concat!("slowcode/", env!("CARGO_PKG_VERSION"));
const CHECK_TIMEOUT: Duration = Duration::from_secs(2);
const DOWNLOAD_TIMEOUT: Duration = Duration::from_secs(60);

/// Where latest metadata and release assets live.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ReleaseLocations {
    latest_url: String,
    download_base: String,
}

impl ReleaseLocations {
    /// Resolve from the environment. `SLOWCODE_UPDATE_URL` replaces the GitHub
    /// release base and serves `latest.json` next to the assets.
    pub fn from_environment(environment: &Environment) -> Result<Self, UpdateError> {
        if let Some(base) = environment.update_base() {
            let base = base.trim_end_matches('/').to_owned();
            validate_url(&base)?;
            return Ok(Self {
                latest_url: format!("{base}/latest.json"),
                download_base: base,
            });
        }
        let repo = environment.repo();
        validate_repo(&repo)?;
        Ok(Self {
            latest_url: format!("https://api.github.com/repos/{repo}/releases/latest"),
            download_base: format!("https://github.com/{repo}/releases/download"),
        })
    }

    /// The latest release metadata URL.
    #[must_use]
    pub fn latest_url(&self) -> &str {
        &self.latest_url
    }

    /// The URL for one asset of one tag.
    pub fn asset_url(&self, tag: &str, file: &str) -> Result<String, UpdateError> {
        validate_tag(tag)?;
        if !is_safe_segment(file) {
            return Err(UpdateError::InsecureUrl(file.to_owned()));
        }
        let url = format!("{}/{tag}/{file}", self.download_base);
        validate_url(&url)?;
        Ok(url)
    }
}

/// A real HTTP transport.
#[derive(Clone, Debug)]
pub struct HttpTransport {
    check_client: reqwest::Client,
    download_client: reqwest::Client,
    locations: ReleaseLocations,
}

impl HttpTransport {
    /// Build a transport for the process environment.
    pub fn from_environment(environment: &Environment) -> Result<Self, UpdateError> {
        let locations = ReleaseLocations::from_environment(environment)?;
        Self::new(locations)
    }

    fn new(locations: ReleaseLocations) -> Result<Self, UpdateError> {
        let check_client = reqwest::Client::builder()
            .timeout(CHECK_TIMEOUT)
            .user_agent(USER_AGENT)
            .build()?;
        let download_client = reqwest::Client::builder()
            .timeout(DOWNLOAD_TIMEOUT)
            .user_agent(USER_AGENT)
            .build()?;
        Ok(Self {
            check_client,
            download_client,
            locations,
        })
    }

    /// The locations this transport reads.
    #[must_use]
    pub fn locations(&self) -> &ReleaseLocations {
        &self.locations
    }
}

#[derive(Deserialize)]
struct LatestRelease {
    tag_name: Option<String>,
}

impl UpdateTransport for HttpTransport {
    fn latest_release(&self) -> BoxFuture<'_, Result<Release, UpdateError>> {
        Box::pin(async move {
            let url = self.locations.latest_url();
            validate_url(url)?;
            let response = self.check_client.get(url).send().await?;
            let status = response.status().as_u16();
            if !(200..300).contains(&status) {
                return Err(UpdateError::Status(status));
            }
            let body = response.bytes().await?;
            let parsed: LatestRelease = serde_json::from_slice(&body)?;
            let tag = parsed.tag_name.ok_or(UpdateError::MissingTag)?;
            let version =
                Version::parse(&tag).ok_or_else(|| UpdateError::InvalidVersion(tag.clone()))?;
            Ok(Release { tag, version })
        })
    }

    fn download(&self, url: &str) -> BoxFuture<'_, Result<Vec<u8>, UpdateError>> {
        let url = url.to_owned();
        Box::pin(async move {
            validate_url(&url)?;
            let response = self.download_client.get(&url).send().await?;
            let status = response.status().as_u16();
            if !(200..300).contains(&status) {
                return Err(UpdateError::Status(status));
            }
            Ok(response.bytes().await?.to_vec())
        })
    }

    fn asset_url(&self, tag: &str, file: &str) -> Result<String, UpdateError> {
        self.locations.asset_url(tag, file)
    }
}

/// Reject URLs that would send the updater over cleartext. Loopback HTTP is
/// allowed only so tests can serve releases locally.
pub fn validate_url(url: &str) -> Result<(), UpdateError> {
    if url.starts_with("https://") {
        return Ok(());
    }
    if let Some(rest) = url.strip_prefix("http://")
        && is_loopback(rest)
    {
        return Ok(());
    }
    Err(UpdateError::InsecureUrl(url.to_owned()))
}

fn is_loopback(rest: &str) -> bool {
    let host_end = rest.find('/').unwrap_or(rest.len());
    let authority = &rest[..host_end];
    let host = authority
        .rsplit_once(':')
        .map(|(host, _port)| host)
        .unwrap_or(authority);
    matches!(host, "127.0.0.1" | "localhost" | "[::1]")
}

fn validate_repo(repo: &str) -> Result<(), UpdateError> {
    let mut parts = repo.split('/');
    let owner = parts.next().unwrap_or_default();
    let name = parts.next().unwrap_or_default();
    if parts.next().is_some()
        || !is_safe_segment(owner)
        || !is_safe_segment(name)
        || owner.is_empty()
        || name.is_empty()
    {
        return Err(UpdateError::InsecureUrl(format!(
            "SLOWCODE_REPO must be owner/repo, got {repo}"
        )));
    }
    Ok(())
}

fn validate_tag(tag: &str) -> Result<(), UpdateError> {
    if is_safe_segment(tag) {
        Ok(())
    } else {
        Err(UpdateError::InvalidVersion(tag.to_owned()))
    }
}

fn is_safe_segment(value: &str) -> bool {
    !value.is_empty()
        && value
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'.' | b'-' | b'_' | b'+'))
}

#[cfg(test)]
mod tests {
    use crate::policy::Environment;

    use super::{ReleaseLocations, validate_url};

    #[test]
    fn only_https_or_loopback_http_is_allowed() {
        assert!(validate_url("https://github.com/x/y").is_ok());
        assert!(validate_url("http://127.0.0.1:8080/releases").is_ok());
        assert!(validate_url("http://localhost:8080/releases").is_ok());
        assert!(validate_url("http://[::1]:8080/releases").is_ok());
        assert!(validate_url("http://example.com/releases").is_err());
        assert!(validate_url("ftp://example.com/releases").is_err());
        assert!(validate_url("file:///tmp/dist").is_err());
    }

    #[test]
    fn github_locations_use_the_configured_repo() {
        let environment = Environment {
            repo: Some("example/slowcode".to_owned()),
            ..Environment::default()
        };
        let locations = ReleaseLocations::from_environment(&environment).expect("locations");
        assert_eq!(
            locations.latest_url(),
            "https://api.github.com/repos/example/slowcode/releases/latest"
        );
        assert_eq!(
            locations
                .asset_url("v0.2.0", "slowcode.tar.gz")
                .expect("url"),
            "https://github.com/example/slowcode/releases/download/v0.2.0/slowcode.tar.gz"
        );
    }

    #[test]
    fn update_url_override_replaces_both_endpoints() {
        let environment = Environment {
            update_url: Some("http://127.0.0.1:9000/releases/".to_owned()),
            ..Environment::default()
        };
        let locations = ReleaseLocations::from_environment(&environment).expect("locations");
        assert_eq!(
            locations.latest_url(),
            "http://127.0.0.1:9000/releases/latest.json"
        );
        assert_eq!(
            locations
                .asset_url("v0.2.0", "slowcode-test.tar.gz")
                .expect("url"),
            "http://127.0.0.1:9000/releases/v0.2.0/slowcode-test.tar.gz"
        );
    }

    #[test]
    fn repo_and_tag_cannot_escape_the_url() {
        let bad_repo = Environment {
            repo: Some("../../evil".to_owned()),
            ..Environment::default()
        };
        assert!(ReleaseLocations::from_environment(&bad_repo).is_err());
        let locations = ReleaseLocations::from_environment(&Environment::default()).expect("ok");
        assert!(
            locations
                .asset_url("v0.2.0/../../x", "slowcode.tar.gz")
                .is_err()
        );
        assert!(locations.asset_url("v0.2.0", "../slowcode").is_err());
        assert!(locations.asset_url("v0.2.0", "slowcode.tar.gz").is_ok());
    }
}
