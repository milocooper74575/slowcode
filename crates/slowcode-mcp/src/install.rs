//! Resolve `/mcp add owner/repo` into an installable npm-backed server.
//!
//! Resolution never clones or builds a repository. It reads npm registry
//! metadata, optionally falls back to the repository's root `package.json`,
//! and returns one exact version pinned in the generated configuration.

use std::{collections::BTreeMap, time::Duration};

use serde::Deserialize;
use serde_json::Value;

use crate::{McpError, McpScope, McpServerConfig, McpSource, McpTransport};

const DEFAULT_NPM_REGISTRY: &str = "https://registry.npmjs.org";
const DEFAULT_NPM_SEARCH: &str = "https://registry.npmjs.org/-/v1/search";
const DEFAULT_GITHUB_API: &str = "https://api.github.com";
const DEFAULT_GITHUB_RAW: &str = "https://raw.githubusercontent.com";

/// Parsed `/mcp add` argument.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum McpAddRequest {
    Github {
        repo: String,
    },
    Stdio {
        name: String,
        command: String,
        args: Vec<String>,
    },
    Http {
        name: String,
        url: String,
    },
}

/// Parse the argument text after `/mcp add`.
pub fn parse_add_request(argument: &str) -> Result<McpAddRequest, String> {
    let tokens = argument.split_whitespace().collect::<Vec<_>>();
    const USAGE: &str = "usage: /mcp add <owner>/<repo> | /mcp add <name> <command> [args...]";
    match tokens.as_slice() {
        [] => Err(USAGE.to_owned()),
        [single] => {
            if normalize_github_repo(single).is_some() {
                Ok(McpAddRequest::Github {
                    repo: (*single).to_owned(),
                })
            } else {
                Err(format!(
                    "{USAGE}\nto run a local command, use /mcp add <name> <command>"
                ))
            }
        }
        [name, "--http", url] => Ok(McpAddRequest::Http {
            name: (*name).to_owned(),
            url: (*url).to_owned(),
        }),
        [_, "--http", ..] => Err("usage: /mcp add <name> --http <url>".to_owned()),
        [name, command, args @ ..] => Ok(McpAddRequest::Stdio {
            name: (*name).to_owned(),
            command: (*command).to_owned(),
            args: args.iter().map(|value| (*value).to_owned()).collect(),
        }),
    }
}

/// True when a single token is an owner/repo shorthand.
pub fn is_github_shorthand(token: &str) -> bool {
    let Some((owner, repo)) = token.split_once('/') else {
        return false;
    };
    if owner.is_empty() || repo.is_empty() || repo.contains('/') {
        return false;
    }
    repo != "." && repo != ".." && valid_repo_segment(owner) && valid_repo_segment(repo)
}

fn valid_repo_segment(segment: &str) -> bool {
    segment
        .chars()
        .all(|character| character.is_ascii_alphanumeric() || matches!(character, '_' | '.' | '-'))
        && segment
            .chars()
            .next()
            .is_some_and(|character| character.is_ascii_alphanumeric())
}

/// Normalize a GitHub repository reference to lowercase `owner/repo`.
pub fn normalize_github_repo(value: &str) -> Option<String> {
    github_reference(value).map(|reference| {
        let (owner, repo) = reference.split_once('/').unwrap_or((reference, ""));
        format!(
            "{}/{}",
            owner.to_ascii_lowercase(),
            repo.to_ascii_lowercase()
        )
    })
}

fn github_reference(value: &str) -> Option<&str> {
    let mut text = value.trim();
    for prefix in [
        "https://github.com/",
        "http://github.com/",
        "https://www.github.com/",
        "git+https://github.com/",
        "git://github.com/",
        "ssh://git@github.com/",
        "git@github.com:",
    ] {
        if let Some(rest) = text.strip_prefix(prefix) {
            text = rest;
            break;
        }
    }
    let text = text
        .split(['?', '#'])
        .next()
        .unwrap_or(text)
        .trim_end_matches('/');
    let text = text.strip_suffix(".git").unwrap_or(text);
    if !is_github_shorthand(text) {
        return None;
    }
    Some(text)
}

/// Derive the short server name from a package name.
///
/// `chrome-devtools-mcp` becomes `chrome-devtools`, and scoped packages lose
/// their scope. Empty results fall back to `mcp-server`.
pub fn short_server_name(package: &str) -> String {
    let unscoped = package.rsplit('/').next().unwrap_or(package);
    let lower = unscoped.to_ascii_lowercase();
    let mut name = lower.as_str();
    if let Some(rest) = name.strip_prefix("mcp-server-") {
        name = rest;
    } else if let Some(rest) = name.strip_prefix("mcp-") {
        name = rest;
    }
    if let Some(rest) = name.strip_suffix("-mcp") {
        name = rest;
    } else if let Some(rest) = name.strip_suffix("_mcp") {
        name = rest;
    }
    let sanitized = name
        .chars()
        .map(|character| {
            if character.is_ascii_alphanumeric() || character == '-' {
                character
            } else {
                '-'
            }
        })
        .collect::<String>();
    let sanitized = sanitized.trim_matches('-');
    if sanitized.is_empty() {
        "mcp-server".to_owned()
    } else {
        sanitized.to_owned()
    }
}

/// MCP source resolver with testable endpoints.
#[derive(Clone, Debug)]
pub struct McpResolver {
    client: reqwest::Client,
    npm_registry: String,
    npm_search: String,
    github_api: String,
    github_raw: String,
}

impl Default for McpResolver {
    fn default() -> Self {
        Self::new()
    }
}

impl McpResolver {
    pub fn new() -> Self {
        let client = reqwest::Client::builder()
            .timeout(Duration::from_secs(20))
            .build()
            .unwrap_or_else(|_| reqwest::Client::new());
        Self {
            client,
            npm_registry: DEFAULT_NPM_REGISTRY.to_owned(),
            npm_search: DEFAULT_NPM_SEARCH.to_owned(),
            github_api: DEFAULT_GITHUB_API.to_owned(),
            github_raw: DEFAULT_GITHUB_RAW.to_owned(),
        }
    }

    /// Override endpoints for tests or local registries.
    pub fn with_endpoints(
        mut self,
        npm_registry: impl Into<String>,
        npm_search: impl Into<String>,
        github_api: impl Into<String>,
        github_raw: impl Into<String>,
    ) -> Self {
        self.npm_registry = npm_registry.into();
        self.npm_search = npm_search.into();
        self.github_api = github_api.into();
        self.github_raw = github_raw.into();
        self
    }

    /// Resolve a GitHub shorthand into a pinned stdio server configuration.
    pub async fn resolve_repo(&self, repo: &str) -> Result<McpServerConfig, McpError> {
        let normalized = normalize_github_repo(repo)
            .ok_or_else(|| McpError::Install(format!("invalid GitHub repository: {repo}")))?;
        let display_repo = github_reference(repo)
            .unwrap_or_else(|| repo.trim())
            .to_owned();

        if let Some(package) = self.search_npm_repository(&normalized).await? {
            let metadata = self.npm_package(&package).await?;
            return self.plan(display_repo, metadata);
        }

        let package = self.github_package(&normalized).await?;
        let metadata = self.npm_package(&package).await?;
        if let Some(repository) = metadata.repository_url()
            && normalize_github_repo(&repository).as_deref() != Some(normalized.as_str())
        {
            return Err(McpError::Install(format!(
                "npm package {package} points at {repository}, not {display_repo}"
            )));
        }
        self.plan(display_repo, metadata)
    }

    async fn search_npm_repository(&self, repo: &str) -> Result<Option<String>, McpError> {
        let response = self
            .client
            .get(&self.npm_search)
            .query(&[("text", repo), ("size", "10")])
            .header("user-agent", "slowcode")
            .send()
            .await
            .map_err(|error| McpError::Install(format!("npm search failed: {error}")))?;
        let response = response
            .error_for_status()
            .map_err(|error| McpError::Install(format!("npm search failed: {error}")))?;
        let search: NpmSearchResponse = response
            .json()
            .await
            .map_err(|error| McpError::Install(format!("npm search response: {error}")))?;

        let mut matches = search
            .objects
            .into_iter()
            .filter(|object| {
                object
                    .package
                    .links
                    .repository
                    .as_deref()
                    .and_then(normalize_github_repo)
                    .as_deref()
                    == Some(repo)
            })
            .map(|object| object.package.name)
            .collect::<Vec<_>>();
        matches.sort();
        matches.dedup();
        match matches.as_slice() {
            [] => Ok(None),
            [only] => Ok(Some(only.clone())),
            many => {
                let last = repo.rsplit('/').next().unwrap_or(repo);
                if let Some(exact) = many.iter().find(|name| {
                    name.rsplit('/')
                        .next()
                        .unwrap_or(name)
                        .eq_ignore_ascii_case(last)
                }) {
                    return Ok(Some(exact.clone()));
                }
                Err(McpError::Install(format!(
                    "repository {repo} publishes several npm packages: {}",
                    many.join(", ")
                )))
            }
        }
    }

    async fn github_package(&self, repo: &str) -> Result<String, McpError> {
        let (owner, name) = repo
            .split_once('/')
            .ok_or_else(|| McpError::Install(format!("invalid repository: {repo}")))?;
        let metadata_url = format!("{}/repos/{owner}/{name}", self.github_api);
        let repository: GithubRepository = self
            .get_checked(&metadata_url, "GitHub lookup")
            .await?
            .json()
            .await
            .map_err(|error| McpError::Install(format!("GitHub response: {error}")))?;

        let package_url = format!(
            "{}/{owner}/{name}/{}/package.json",
            self.github_raw, repository.default_branch
        );
        let response = self
            .client
            .get(package_url)
            .header("user-agent", "slowcode")
            .send()
            .await
            .map_err(|error| McpError::Install(format!("package.json lookup failed: {error}")))?;
        if response.status() == reqwest::StatusCode::NOT_FOUND {
            return Err(McpError::Install(format!(
                "{repo} has no root package.json; use /mcp add <name> <command>"
            )));
        }
        let response = response
            .error_for_status()
            .map_err(|error| McpError::Install(format!("package.json lookup failed: {error}")))?;
        let package: GithubPackage = response
            .json()
            .await
            .map_err(|error| McpError::Install(format!("package.json response: {error}")))?;
        installable_package(repo, package)
    }

    async fn get_checked(&self, url: &str, failure: &str) -> Result<reqwest::Response, McpError> {
        self.client
            .get(url)
            .header("user-agent", "slowcode")
            .send()
            .await
            .map_err(|error| McpError::Install(format!("{failure}: {error}")))?
            .error_for_status()
            .map_err(|error| McpError::Install(format!("{failure}: {error}")))
    }

    async fn npm_package(&self, package: &str) -> Result<NpmPackage, McpError> {
        let encoded = package.replace('@', "%40").replace('/', "%2F");
        let url = format!("{}/{encoded}/latest", self.npm_registry);
        let response = self
            .client
            .get(url)
            .header("user-agent", "slowcode")
            .send()
            .await
            .map_err(|error| McpError::Install(format!("npm lookup failed: {error}")))?;
        if response.status() == reqwest::StatusCode::NOT_FOUND {
            return Err(McpError::Install(format!(
                "npm package {package} not found; use /mcp add <name> <command>"
            )));
        }
        response
            .error_for_status()
            .map_err(|error| McpError::Install(format!("npm lookup failed: {error}")))?
            .json()
            .await
            .map_err(|error| McpError::Install(format!("npm response: {error}")))
    }

    fn plan(&self, repo: String, metadata: NpmPackage) -> Result<McpServerConfig, McpError> {
        let bin = choose_bin(&metadata.name, metadata.bin.as_ref())?;
        let name = short_server_name(&metadata.name);
        let args = vec![
            "-y".to_owned(),
            format!("{}@{}", metadata.name, metadata.version),
        ];
        Ok(McpServerConfig {
            name,
            transport: McpTransport::Stdio {
                command: "npx".to_owned(),
                args,
                env: BTreeMap::new(),
            },
            trusted: false,
            enabled: true,
            scope: Some(McpScope::Project),
            source: Some(McpSource {
                kind: "github".to_owned(),
                repo,
                package: metadata.name,
                version: metadata.version,
                integrity: metadata.dist.and_then(|dist| dist.integrity),
                bin,
            }),
        })
    }
}

fn installable_package(repo: &str, package: GithubPackage) -> Result<String, McpError> {
    let Some(name) = package.name else {
        return Err(McpError::Install(format!(
            "{repo} has no installable root package; use /mcp add <name> <command>"
        )));
    };
    if package.private {
        return Err(McpError::Install(format!(
            "{repo} is a private package; use /mcp add <name> <command>"
        )));
    }
    Ok(name)
}

fn choose_bin(package: &str, bin: Option<&Value>) -> Result<String, McpError> {
    let unscoped = package.rsplit('/').next().unwrap_or(package);
    let names = match bin {
        None | Some(Value::Null) => Vec::new(),
        Some(Value::String(_)) => vec![unscoped.to_owned()],
        Some(Value::Object(map)) => map.keys().cloned().collect::<Vec<_>>(),
        Some(_) => Vec::new(),
    };
    if names.is_empty() {
        return Err(McpError::Install(format!(
            "npm package {package} exposes no binary; use /mcp add <name> <command>"
        )));
    }
    if let Some(exact) = names.iter().find(|name| name.as_str() == unscoped) {
        return Ok(exact.clone());
    }
    if names.len() == 1 {
        return Ok(names[0].clone());
    }
    let mut sorted = names;
    sorted.sort();
    Err(McpError::Install(format!(
        "npm package {package} exposes several binaries: {}; use /mcp add <name> <command>",
        sorted.join(", ")
    )))
}

#[derive(Deserialize)]
struct NpmSearchResponse {
    #[serde(default)]
    objects: Vec<NpmSearchObject>,
}

#[derive(Deserialize)]
struct NpmSearchObject {
    package: NpmSearchPackage,
}

#[derive(Deserialize)]
struct NpmSearchPackage {
    name: String,
    #[serde(default)]
    links: NpmLinks,
}

#[derive(Deserialize, Default)]
struct NpmLinks {
    #[serde(default)]
    repository: Option<String>,
}

#[derive(Deserialize)]
struct NpmPackage {
    name: String,
    version: String,
    #[serde(default)]
    repository: Option<Value>,
    #[serde(default)]
    bin: Option<Value>,
    #[serde(default)]
    dist: Option<NpmDist>,
}

impl NpmPackage {
    fn repository_url(&self) -> Option<String> {
        match self.repository.as_ref()? {
            Value::String(value) => Some(value.clone()),
            Value::Object(map) => map.get("url").and_then(Value::as_str).map(str::to_owned),
            _ => None,
        }
    }
}

#[derive(Deserialize)]
struct NpmDist {
    #[serde(default)]
    integrity: Option<String>,
}

#[derive(Deserialize)]
struct GithubRepository {
    default_branch: String,
}

#[derive(Deserialize)]
struct GithubPackage {
    #[serde(default)]
    name: Option<String>,
    #[serde(default)]
    private: bool,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_the_two_supported_add_forms() {
        assert_eq!(
            parse_add_request("ChromeDevTools/chrome-devtools-mcp").unwrap(),
            McpAddRequest::Github {
                repo: "ChromeDevTools/chrome-devtools-mcp".to_owned()
            }
        );
        assert_eq!(
            parse_add_request("https://github.com/ChromeDevTools/chrome-devtools-mcp").unwrap(),
            McpAddRequest::Github {
                repo: "https://github.com/ChromeDevTools/chrome-devtools-mcp".to_owned()
            }
        );
        assert_eq!(
            parse_add_request("fs npx -y @modelcontextprotocol/server-filesystem").unwrap(),
            McpAddRequest::Stdio {
                name: "fs".to_owned(),
                command: "npx".to_owned(),
                args: vec![
                    "-y".to_owned(),
                    "@modelcontextprotocol/server-filesystem".to_owned()
                ],
            }
        );
        assert_eq!(
            parse_add_request("remote --http https://example.com/mcp").unwrap(),
            McpAddRequest::Http {
                name: "remote".to_owned(),
                url: "https://example.com/mcp".to_owned(),
            }
        );
        assert!(parse_add_request("../repo").is_err());
        assert!(parse_add_request("").is_err());
    }

    #[test]
    fn normalizes_github_references() {
        for value in [
            "ChromeDevTools/chrome-devtools-mcp",
            "https://github.com/ChromeDevTools/chrome-devtools-mcp",
            "git+https://github.com/ChromeDevTools/chrome-devtools-mcp.git",
            "git@github.com:ChromeDevTools/chrome-devtools-mcp.git",
        ] {
            assert_eq!(
                normalize_github_repo(value).as_deref(),
                Some("chromedevtools/chrome-devtools-mcp")
            );
        }
        assert!(normalize_github_repo("owner/repo/extra").is_none());
        assert!(normalize_github_repo("owner").is_none());
        assert!(normalize_github_repo("../repo").is_none());
    }

    #[test]
    fn derives_short_server_names() {
        assert_eq!(short_server_name("chrome-devtools-mcp"), "chrome-devtools");
        assert_eq!(short_server_name("@scope/fs-server"), "fs-server");
        assert_eq!(short_server_name("mcp-server-github"), "github");
        assert_eq!(short_server_name("mcp-foo-mcp"), "foo");
        assert_eq!(short_server_name("___"), "mcp-server");
    }

    #[test]
    fn chooses_a_single_or_named_binary() {
        let bins = serde_json::json!({
            "chrome-devtools": "a.js",
            "chrome-devtools-mcp": "b.js"
        });
        assert_eq!(
            choose_bin("chrome-devtools-mcp", Some(&bins)).unwrap(),
            "chrome-devtools-mcp"
        );
        let single = serde_json::json!({"only": "a.js"});
        assert_eq!(choose_bin("pkg", Some(&single)).unwrap(), "only");
        let ambiguous = serde_json::json!({"a": "a.js", "b": "b.js"});
        assert!(choose_bin("pkg", Some(&ambiguous)).is_err());
        assert!(choose_bin("pkg", None).is_err());
    }

    #[test]
    fn accepts_only_a_public_named_root_package() {
        let package = |name: Option<&str>, private: bool| GithubPackage {
            name: name.map(str::to_owned),
            private,
        };
        assert_eq!(
            installable_package("owner/tool", package(Some("tool-mcp"), false)).unwrap(),
            "tool-mcp"
        );
        assert!(
            installable_package("owner/tool", package(None, false))
                .unwrap_err()
                .to_string()
                .contains("has no installable root package")
        );
        assert!(
            installable_package("owner/tool", package(Some("tool-mcp"), true))
                .unwrap_err()
                .to_string()
                .contains("is a private package")
        );
    }

    #[tokio::test]
    async fn resolves_through_npm_search_with_pinned_version() {
        let fixture = Fixture::start(vec![
            (
                "/-/v1/search",
                serde_json::json!({
                    "objects": [{
                        "package": {
                            "name": "chrome-devtools-mcp",
                            "links": {"repository": "git+https://github.com/ChromeDevTools/chrome-devtools-mcp.git"}
                        }
                    }]
                })
                .to_string(),
            ),
            (
                "/chrome-devtools-mcp/latest",
                serde_json::json!({
                    "name": "chrome-devtools-mcp",
                    "version": "1.11.0",
                    "repository": {"url": "git+https://github.com/ChromeDevTools/chrome-devtools-mcp.git"},
                    "bin": {"chrome-devtools-mcp": "build/bin.js"},
                    "dist": {"integrity": "sha512-test"}
                })
                .to_string(),
            ),
        ])
        .await;
        let resolver = McpResolver::new().with_endpoints(
            fixture.base(),
            format!("{}/-/v1/search", fixture.base()),
            format!("{}/api", fixture.base()),
            format!("{}/raw", fixture.base()),
        );
        let server = resolver
            .resolve_repo("ChromeDevTools/chrome-devtools-mcp")
            .await
            .unwrap();
        assert_eq!(server.name, "chrome-devtools");
        let McpTransport::Stdio { command, args, .. } = &server.transport else {
            panic!("expected stdio");
        };
        assert_eq!(command, "npx");
        assert_eq!(args, &["-y", "chrome-devtools-mcp@1.11.0"]);
        let source = server.source.unwrap();
        assert_eq!(source.version, "1.11.0");
        assert_eq!(source.integrity.as_deref(), Some("sha512-test"));
        fixture.stop();
    }

    #[tokio::test]
    async fn falls_back_to_the_repository_package_json() {
        let fixture = Fixture::start(vec![
            (
                "/-/v1/search",
                serde_json::json!({"objects": []}).to_string(),
            ),
            (
                "/api/repos/owner/tool",
                serde_json::json!({"default_branch": "main"}).to_string(),
            ),
            (
                "/raw/owner/tool/main/package.json",
                serde_json::json!({"name": "tool-mcp", "bin": {"tool-mcp": "index.js"}})
                    .to_string(),
            ),
            (
                "/tool-mcp/latest",
                serde_json::json!({
                    "name": "tool-mcp",
                    "version": "2.0.0",
                    "repository": {"url": "https://github.com/owner/tool"},
                    "bin": {"tool-mcp": "index.js"}
                })
                .to_string(),
            ),
        ])
        .await;
        let resolver = McpResolver::new().with_endpoints(
            fixture.base(),
            format!("{}/-/v1/search", fixture.base()),
            format!("{}/api", fixture.base()),
            format!("{}/raw", fixture.base()),
        );
        let server = resolver.resolve_repo("owner/tool").await.unwrap();
        assert_eq!(server.name, "tool");
        assert_eq!(server.source.unwrap().version, "2.0.0");
        fixture.stop();
    }

    #[tokio::test]
    async fn reports_a_missing_root_package_json() {
        let fixture = Fixture::start_with_status(vec![
            (
                "/-/v1/search",
                200,
                serde_json::json!({"objects": []}).to_string(),
            ),
            (
                "/api/repos/owner/tool",
                200,
                serde_json::json!({"default_branch": "main"}).to_string(),
            ),
            (
                "/raw/owner/tool/main/package.json",
                404,
                "not found".to_owned(),
            ),
        ])
        .await;
        let error = fixture
            .resolver()
            .resolve_repo("owner/tool")
            .await
            .unwrap_err();
        assert!(
            error.to_string().contains("has no root package.json"),
            "{error}"
        );
        fixture.stop();
    }

    #[tokio::test]
    async fn rejects_a_root_package_without_a_name() {
        let fixture = Fixture::start_with_status(vec![
            (
                "/-/v1/search",
                200,
                serde_json::json!({"objects": []}).to_string(),
            ),
            (
                "/api/repos/owner/tool",
                200,
                serde_json::json!({"default_branch": "main"}).to_string(),
            ),
            (
                "/raw/owner/tool/main/package.json",
                200,
                serde_json::json!({"private": false}).to_string(),
            ),
        ])
        .await;
        let error = fixture
            .resolver()
            .resolve_repo("owner/tool")
            .await
            .unwrap_err();
        assert!(
            error
                .to_string()
                .contains("has no installable root package"),
            "{error}"
        );
        fixture.stop();
    }

    #[tokio::test]
    async fn rejects_a_private_root_package() {
        let fixture = Fixture::start_with_status(vec![
            (
                "/-/v1/search",
                200,
                serde_json::json!({"objects": []}).to_string(),
            ),
            (
                "/api/repos/owner/tool",
                200,
                serde_json::json!({"default_branch": "main"}).to_string(),
            ),
            (
                "/raw/owner/tool/main/package.json",
                200,
                serde_json::json!({"name": "tool-mcp", "private": true}).to_string(),
            ),
        ])
        .await;
        let error = fixture
            .resolver()
            .resolve_repo("owner/tool")
            .await
            .unwrap_err();
        assert!(
            error.to_string().contains("is a private package"),
            "{error}"
        );
        fixture.stop();
    }

    #[tokio::test]
    #[ignore = "network access to npm registry"]
    async fn resolves_the_real_chrome_devtools_repository() {
        let server = McpResolver::new()
            .resolve_repo("ChromeDevTools/chrome-devtools-mcp")
            .await
            .unwrap();
        assert_eq!(server.name, "chrome-devtools");
        let source = server.source.expect("source metadata");
        assert_eq!(source.package, "chrome-devtools-mcp");
        assert_eq!(source.bin, "chrome-devtools-mcp");
        assert!(
            source
                .version
                .chars()
                .next()
                .is_some_and(|c| c.is_ascii_digit())
        );
        let McpTransport::Stdio { command, args, .. } = &server.transport else {
            panic!("expected stdio");
        };
        assert_eq!(command, "npx");
        assert!(args[1].ends_with(&format!("@{}", source.version)));
    }

    #[tokio::test]
    #[ignore = "network access and an npm download"]
    async fn connects_to_the_real_chrome_devtools_server() {
        let server = McpResolver::new()
            .resolve_repo("ChromeDevTools/chrome-devtools-mcp")
            .await
            .unwrap();
        let session = crate::McpSession::connect(&server).await.unwrap();
        assert!(
            session
                .tools()
                .iter()
                .any(|tool| tool.name == "navigate_page")
        );
        session.shutdown().await;
    }

    struct Fixture {
        base: String,
        shutdown: tokio::sync::oneshot::Sender<()>,
        task: tokio::task::JoinHandle<()>,
    }

    impl Fixture {
        async fn start(routes: Vec<(&'static str, String)>) -> Self {
            Self::start_with_status(
                routes
                    .into_iter()
                    .map(|(route, body)| (route, 200, body))
                    .collect(),
            )
            .await
        }

        async fn start_with_status(routes: Vec<(&'static str, u16, String)>) -> Self {
            let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
            let base = format!("http://{}", listener.local_addr().unwrap());
            let (shutdown, mut stop) = tokio::sync::oneshot::channel::<()>();
            let task = tokio::spawn(async move {
                loop {
                    let accept = tokio::select! {
                        _ = &mut stop => break,
                        accept = listener.accept() => accept,
                    };
                    let Ok((mut stream, _)) = accept else {
                        break;
                    };
                    let mut buffer = vec![0_u8; 4096];
                    let Ok(read) = tokio::io::AsyncReadExt::read(&mut stream, &mut buffer).await
                    else {
                        continue;
                    };
                    let request = String::from_utf8_lossy(&buffer[..read]);
                    let path = request
                        .lines()
                        .next()
                        .and_then(|line| line.split_whitespace().nth(1))
                        .unwrap_or("/");
                    let path = path.split('?').next().unwrap_or(path);
                    let (status, body) = routes
                        .iter()
                        .find(|(route, _, _)| *route == path)
                        .map(|(_, status, body)| (*status, body.clone()))
                        .unwrap_or_else(|| (200, "{}".to_owned()));
                    let response = format!(
                        "HTTP/1.1 {status} {}\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{body}",
                        status_reason(status),
                        body.len()
                    );
                    let _ =
                        tokio::io::AsyncWriteExt::write_all(&mut stream, response.as_bytes()).await;
                }
            });
            Self {
                base,
                shutdown,
                task,
            }
        }

        fn base(&self) -> String {
            self.base.clone()
        }

        fn resolver(&self) -> McpResolver {
            McpResolver::new().with_endpoints(
                self.base(),
                format!("{}/-/v1/search", self.base()),
                format!("{}/api", self.base()),
                format!("{}/raw", self.base()),
            )
        }

        fn stop(self) {
            let _ = self.shutdown.send(());
            self.task.abort();
        }
    }

    fn status_reason(status: u16) -> &'static str {
        match status {
            200 => "OK",
            404 => "Not Found",
            _ => "Unknown",
        }
    }
}
