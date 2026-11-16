//! MCP activation trust and extension-tool approval.

use std::{
    collections::{BTreeMap, BTreeSet},
    path::{Path, PathBuf},
    sync::{
        Arc, Mutex, RwLock,
        atomic::{AtomicBool, Ordering},
    },
};

use serde::{Deserialize, Serialize};
use serde_json::Value;
use slowcode_kernel::{
    agent::{ApprovalProvider, ApprovalRequest},
    session::{ApprovalOutcome, ApprovalRequestId},
};
use thiserror::Error;
use tokio_util::sync::CancellationToken;

use crate::{McpError, McpScope, McpServerConfig, McpTransport};

/// Why an MCP tool call did not reach the server.
#[derive(Clone, Debug, Error, Eq, PartialEq)]
pub enum McpApprovalError {
    #[error("the MCP tool call was rejected")]
    Rejected,
    #[error("the MCP tool call was cancelled")]
    Cancelled,
    #[error("MCP tool approval is unavailable")]
    Unavailable,
    #[error("MCP tool approval failed")]
    Failed,
    #[error("read-only mode allows only tools marked readOnlyHint")]
    ReadOnlyDenied,
}

/// Session-scoped approval state for MCP tools.
pub struct McpApprovalGate {
    provider: RwLock<Option<Arc<dyn ApprovalProvider>>>,
    grants: Mutex<BTreeSet<String>>,
    read_only: AtomicBool,
    role_read_only: AtomicBool,
}

impl McpApprovalGate {
    pub fn new(read_only: bool) -> Arc<Self> {
        Arc::new(Self {
            provider: RwLock::new(None),
            grants: Mutex::new(BTreeSet::new()),
            read_only: AtomicBool::new(read_only),
            role_read_only: AtomicBool::new(false),
        })
    }

    pub fn set_provider(&self, provider: Arc<dyn ApprovalProvider>) {
        *self
            .provider
            .write()
            .unwrap_or_else(|poisoned| poisoned.into_inner()) = Some(provider);
    }

    pub fn set_read_only(&self, read_only: bool) {
        self.read_only.store(read_only, Ordering::Release);
    }

    pub fn read_only(&self) -> bool {
        self.read_only.load(Ordering::Acquire)
    }

    /// Whether the active role is read-only. Process `--read-only` stays a
    /// separate, stronger gate.
    pub fn set_role_read_only(&self, read_only: bool) {
        self.role_read_only.store(read_only, Ordering::Release);
    }

    pub fn role_read_only(&self) -> bool {
        self.role_read_only.load(Ordering::Acquire)
    }

    pub fn is_granted(&self, tool: &str) -> bool {
        self.grants
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .contains(tool)
    }

    pub fn grant(&self, tool: &str) {
        self.grants
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .insert(tool.to_owned());
    }

    pub fn revoke_all(&self) {
        self.grants
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .clear();
    }

    pub async fn request(
        &self,
        request: ApprovalRequest,
        cancellation: CancellationToken,
    ) -> Result<(), McpApprovalError> {
        let provider = self
            .provider
            .read()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .clone()
            .ok_or(McpApprovalError::Unavailable)?;
        match provider.request(request, cancellation).await {
            Ok(ApprovalOutcome::AllowedOnce) => Ok(()),
            Ok(ApprovalOutcome::Rejected) => Err(McpApprovalError::Rejected),
            Ok(ApprovalOutcome::Cancelled) => Err(McpApprovalError::Cancelled),
            Ok(ApprovalOutcome::Unavailable) => Err(McpApprovalError::Unavailable),
            Err(_) => Err(McpApprovalError::Failed),
        }
    }
}

/// Long-lived trust decisions for project-level MCP activation.
pub struct McpTrustStore {
    path: PathBuf,
}

impl McpTrustStore {
    pub fn open_default() -> Result<Self, McpError> {
        let home = std::env::var_os("HOME")
            .map(PathBuf::from)
            .ok_or_else(|| McpError::Config("HOME is not available".to_owned()))?;
        Ok(Self::at(home.join(".slowcode/mcp-trust.json")))
    }

    pub fn at(path: PathBuf) -> Self {
        Self { path }
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    pub fn is_trusted(&self, server: &McpServerConfig) -> bool {
        if server.scope == Some(McpScope::User) {
            return server.trusted;
        }
        let Ok(raw) = read_trust(&self.path) else {
            return false;
        };
        raw.servers.contains_key(&server_fingerprint(server))
    }

    pub fn remember(&self, server: &McpServerConfig) -> Result<(), McpError> {
        let mut raw = read_trust(&self.path)?;
        raw.servers.insert(
            server_fingerprint(server),
            TrustedServer {
                name: server.name.clone(),
                repo: server
                    .source
                    .as_ref()
                    .map(|source| source.repo.clone())
                    .unwrap_or_default(),
            },
        );
        write_trust(&self.path, &raw)
    }

    pub fn forget(&self, server: &McpServerConfig) -> Result<(), McpError> {
        let mut raw = read_trust(&self.path)?;
        raw.servers.remove(&server_fingerprint(server));
        write_trust(&self.path, &raw)
    }
}

/// Stable fingerprint of the executable part of one server configuration.
pub fn server_fingerprint(server: &McpServerConfig) -> String {
    let mut fingerprint = Fingerprint::new();
    fingerprint.feed(&server.name);
    fingerprint.feed(scope_label(server.scope, "none"));
    match &server.transport {
        McpTransport::Stdio { command, args, env } => {
            fingerprint.feed("stdio");
            fingerprint.feed(command);
            for argument in args {
                fingerprint.feed(argument);
            }
            for (key, value) in env {
                fingerprint.feed(key);
                fingerprint.feed(value);
            }
        }
        McpTransport::Http { url, headers } => {
            fingerprint.feed("http");
            fingerprint.feed(url);
            for (key, value) in headers {
                fingerprint.feed(key);
                fingerprint.feed(value);
            }
        }
    }
    fingerprint.finish()
}

/// FNV-1a folding with a field separator, kept local to trust fingerprints.
struct Fingerprint(u64);

impl Fingerprint {
    fn new() -> Self {
        Self(0xcbf2_9ce4_8422_2325)
    }

    fn feed(&mut self, value: &str) {
        for byte in value.as_bytes() {
            self.0 ^= u64::from(*byte);
            self.0 = self.0.wrapping_mul(0x0000_0100_0000_01b3);
        }
        self.0 ^= 0xff;
        self.0 = self.0.wrapping_mul(0x0000_0100_0000_01b3);
    }

    fn finish(self) -> String {
        format!("{:016x}", self.0)
    }
}

/// Human-readable scope label. `unset` names the absent case so callers can
/// distinguish "no scope" from "unknown" without rewriting the mapping.
#[must_use]
pub fn scope_label(scope: Option<McpScope>, unset: &'static str) -> &'static str {
    match scope {
        Some(McpScope::User) => "user",
        Some(McpScope::Project) => "project",
        None => unset,
    }
}

/// Build a bounded, redacted preview of one tool call.
pub fn approval_preview(
    server: &str,
    source: Option<&str>,
    tool: &str,
    raw_arguments: &str,
    destructive: bool,
) -> String {
    let mut preview = format!("MCP tool: {tool}\nserver: {server}");
    if let Some(source) = source {
        preview.push_str(&format!("\nsource: {source}"));
    }
    if destructive {
        preview.push_str("\nrisk: destructive");
    }
    preview.push_str("\narguments: ");
    preview.push_str(&redact_arguments(raw_arguments));
    preview
}

fn redact_arguments(raw: &str) -> String {
    let rendered = match serde_json::from_str::<Value>(raw) {
        Ok(mut value) => {
            redact_value(&mut value);
            serde_json::to_string(&value).unwrap_or_else(|_| "<unprintable>".to_owned())
        }
        Err(_) => raw.to_owned(),
    };
    truncate(&rendered, 2048)
}

fn redact_value(value: &mut Value) {
    match value {
        Value::Object(map) => {
            for (key, value) in map {
                let lower = key.to_ascii_lowercase();
                if [
                    "token",
                    "secret",
                    "password",
                    "authorization",
                    "api_key",
                    "cookie",
                ]
                .iter()
                .any(|needle| lower.contains(needle))
                {
                    *value = Value::String("<redacted>".to_owned());
                } else {
                    redact_value(value);
                }
            }
        }
        Value::Array(items) => {
            for item in items {
                redact_value(item);
            }
        }
        _ => {}
    }
}

fn truncate(value: &str, maximum: usize) -> String {
    if value.len() <= maximum {
        return value.to_owned();
    }
    let mut end = maximum;
    while end > 0 && !value.is_char_boundary(end) {
        end -= 1;
    }
    format!("{}...", &value[..end])
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
struct RawTrust {
    #[serde(default)]
    servers: BTreeMap<String, TrustedServer>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
struct TrustedServer {
    name: String,
    #[serde(default)]
    repo: String,
}

fn read_trust(path: &Path) -> Result<RawTrust, McpError> {
    if !path.exists() {
        return Ok(RawTrust::default());
    }
    let text = std::fs::read_to_string(path)
        .map_err(|error| McpError::Config(format!("{}: {error}", path.display())))?;
    serde_json::from_str(&text)
        .map_err(|error| McpError::Config(format!("{}: {error}", path.display())))
}

fn write_trust(path: &Path, trust: &RawTrust) -> Result<(), McpError> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).map_err(|error| McpError::Config(error.to_string()))?;
    }
    let text =
        serde_json::to_string_pretty(trust).map_err(|error| McpError::Config(error.to_string()))?;
    let temp = path.with_extension(format!("tmp-{}", std::process::id()));
    std::fs::write(&temp, text).map_err(|error| McpError::Config(error.to_string()))?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let _ = std::fs::set_permissions(&temp, std::fs::Permissions::from_mode(0o600));
    }
    std::fs::rename(&temp, path).map_err(|error| McpError::Config(error.to_string()))?;
    Ok(())
}

/// Build one approval request id for an extension tool call.
pub fn approval_id_for_call(call_id: &slowcode_kernel::model::CallId) -> ApprovalRequestId {
    ApprovalRequestId::new(format!("mcp-{}", call_id.as_str()))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn server(name: &str, command: &str, trusted: bool, scope: McpScope) -> McpServerConfig {
        McpServerConfig {
            name: name.to_owned(),
            transport: McpTransport::Stdio {
                command: command.to_owned(),
                args: vec!["--flag".to_owned()],
                env: BTreeMap::new(),
            },
            trusted,
            enabled: true,
            scope: Some(scope),
            source: None,
        }
    }

    #[test]
    fn fingerprints_follow_the_executable_configuration() {
        let first = server("demo", "npx", false, McpScope::Project);
        let same = server("demo", "npx", false, McpScope::Project);
        let changed = server("demo", "node", false, McpScope::Project);
        assert_eq!(server_fingerprint(&first), server_fingerprint(&same));
        assert_ne!(server_fingerprint(&first), server_fingerprint(&changed));
    }

    #[test]
    fn fingerprint_stays_stable_for_the_same_configuration() {
        // Pinned value: persisted trust records are keyed by this string.
        let config = server("demo", "npx", false, McpScope::Project);
        assert_eq!(server_fingerprint(&config), "c89d3d0f37b26549");
    }

    #[test]
    fn project_trust_is_remembered_by_fingerprint() {
        let dir = std::env::temp_dir().join(format!(
            "slowcode-mcp-trust-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        let store = McpTrustStore::at(dir.join("mcp-trust.json"));
        let original = server("demo", "npx", false, McpScope::Project);
        assert!(!store.is_trusted(&original));
        store.remember(&original).unwrap();
        assert!(store.is_trusted(&original));
        let changed = server("demo", "node", false, McpScope::Project);
        assert!(!store.is_trusted(&changed));
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn project_trusted_field_does_not_grant_itself() {
        let dir = std::env::temp_dir().join(format!(
            "slowcode-mcp-trust-self-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        let store = McpTrustStore::at(dir.join("mcp-trust.json"));
        let project = server("demo", "npx", true, McpScope::Project);
        assert!(!store.is_trusted(&project));
        let user = server("demo", "npx", true, McpScope::User);
        assert!(store.is_trusted(&user));
    }

    #[test]
    fn previews_redact_secret_values() {
        let preview = approval_preview(
            "demo",
            None,
            "mcp__demo__call",
            r#"{"token":"abc","nested":{"password":"x"},"ok":"visible"}"#,
            false,
        );
        assert!(preview.contains("<redacted>"));
        assert!(preview.contains("visible"));
        assert!(!preview.contains("abc"));
        assert!(!preview.contains("\"x\""));
    }
}
