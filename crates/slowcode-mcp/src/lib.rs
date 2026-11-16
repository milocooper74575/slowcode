//! MCP configuration, transport and a minimal client bridge.
//!
//! P0 supports stdio and Streamable HTTP. Configuration is merged from the
//! user and project scopes. Connection handshakes are bounded and never block
//! the caller's startup path.

mod install;
mod trust;

use std::{
    collections::BTreeMap,
    path::{Path, PathBuf},
    process::Stdio,
    sync::Arc,
    time::Duration,
};

use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use slowcode_kernel::{
    agent::{
        ApprovalRequest, ToolExecutionFuture, ToolExecutionMode, ToolExecutionRequest,
        ToolExecutionResult, ToolExecutor, ToolExecutorError, ToolPreparationFuture,
        ToolShutdownFuture,
    },
    model::{ContentBlock, JsonValue, ToolSchema},
    session::ToolFailure,
};
use thiserror::Error;
use tokio::{
    io::{AsyncBufReadExt, AsyncWriteExt, BufReader},
    process::{Child, ChildStdin, ChildStdout, Command},
};

pub use install::{
    McpAddRequest, McpResolver, is_github_shorthand, normalize_github_repo, parse_add_request,
    short_server_name,
};
pub use trust::{
    McpApprovalError, McpApprovalGate, McpTrustStore, approval_id_for_call, approval_preview,
    scope_label, server_fingerprint,
};

const HANDSHAKE_TIMEOUT: Duration = Duration::from_secs(30);
const CALL_TIMEOUT: Duration = Duration::from_secs(30);

/// Configuration scope.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum McpScope {
    User,
    Project,
}

/// One configured MCP server.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct McpServerConfig {
    pub name: String,
    pub transport: McpTransport,
    pub trusted: bool,
    pub enabled: bool,
    pub scope: Option<McpScope>,
    pub source: Option<McpSource>,
}

/// Where a server configuration came from.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct McpSource {
    pub kind: String,
    pub repo: String,
    pub package: String,
    pub version: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub integrity: Option<String>,
    pub bin: String,
}

impl McpSource {
    pub fn label(&self) -> String {
        format!("{}@{}, npm {}", self.repo, self.version, self.package)
    }
}

/// Supported P0 transports.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum McpTransport {
    Stdio {
        command: String,
        args: Vec<String>,
        env: BTreeMap<String, String>,
    },
    Http {
        url: String,
        headers: BTreeMap<String, String>,
    },
}

/// One discovered MCP tool.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct McpTool {
    pub server: String,
    pub name: String,
    pub description: String,
    pub input_schema: Value,
    pub annotations: McpToolAnnotations,
}

/// Risk hints published by an MCP tool.
///
/// Missing hints are deliberately treated as unknown, which routes the tool to
/// the asking path.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct McpToolAnnotations {
    pub read_only: Option<bool>,
    pub destructive: Option<bool>,
    pub idempotent: Option<bool>,
    pub open_world: Option<bool>,
}

impl McpToolAnnotations {
    pub fn is_read_only(&self) -> bool {
        self.read_only == Some(true)
    }

    /// Read-only is accepted only when it does not conflict with destructive.
    pub fn is_safe_read_only(&self) -> bool {
        self.is_read_only() && self.destructive != Some(true)
    }

    pub fn is_destructive(&self) -> bool {
        self.destructive == Some(true)
    }
}

impl McpTool {
    /// Kernel-facing tool name.
    pub fn qualified_name(&self) -> String {
        format!("mcp__{}__{}", sanitize(&self.server), sanitize(&self.name))
    }
}

/// One connected MCP session.
pub struct McpSession {
    server: String,
    transport: SessionTransport,
    tools: Vec<McpTool>,
    next_id: u64,
}

enum SessionTransport {
    Stdio {
        child: Child,
        stdin: ChildStdin,
        stdout: BufReader<ChildStdout>,
    },
    Http {
        client: reqwest::Client,
        url: String,
        headers: BTreeMap<String, String>,
        session_id: Option<String>,
    },
}

/// Merged MCP configuration.
#[derive(Clone, Debug, Default)]
pub struct McpConfig {
    pub servers: BTreeMap<String, McpServerConfig>,
    user_path: PathBuf,
    project_path: PathBuf,
}

impl McpConfig {
    pub fn load(workspace: &Path) -> Result<Self, McpError> {
        let home = std::env::var_os("HOME")
            .map(PathBuf::from)
            .ok_or_else(|| McpError::Config("HOME is not available".to_owned()))?;
        Self {
            servers: BTreeMap::new(),
            user_path: home.join(".slowcode/mcp.json"),
            project_path: workspace.join(".slowcode/mcp.json"),
        }
        .reload()
    }

    pub fn reload(mut self) -> Result<Self, McpError> {
        let mut servers = BTreeMap::new();
        for (scope, path) in [
            (McpScope::User, &self.user_path),
            (McpScope::Project, &self.project_path),
        ] {
            for (name, mut server) in read_file(path, scope)? {
                server.scope = Some(scope);
                servers.insert(name, server);
            }
        }
        self.servers = servers;
        Ok(self)
    }

    pub fn list(&self) -> Vec<&McpServerConfig> {
        self.servers.values().collect()
    }

    /// Add or replace one server in the chosen scope without rewriting the
    /// other scope's file.
    pub fn upsert(&self, scope: McpScope, server: McpServerConfig) -> Result<(), McpError> {
        let path = self.path(scope);
        let mut file = read_raw(path)?;
        let name = server.name.clone();
        let mut raw = RawServer::from(server);
        if let Some(existing) = file.servers.get(&name) {
            raw.extra = existing.extra.clone();
        }
        file.servers.insert(name, raw);
        write_raw(path, &file)
    }

    pub fn remove(&self, scope: McpScope, name: &str) -> Result<(), McpError> {
        let path = self.path(scope);
        let mut file = read_raw(path)?;
        file.servers.remove(name);
        write_raw(path, &file)
    }

    pub fn path(&self, scope: McpScope) -> &Path {
        match scope {
            McpScope::User => &self.user_path,
            McpScope::Project => &self.project_path,
        }
    }
}

impl McpSession {
    /// Connect, initialize and list tools.
    pub async fn connect(server: &McpServerConfig) -> Result<Self, McpError> {
        let mut session = match &server.transport {
            McpTransport::Stdio { command, args, env } => {
                spawn_stdio_session(&server.name, command, args, env)?
            }
            McpTransport::Http { url, headers } => http_session(&server.name, url, headers),
        };
        session.tools = session.handshake(&server.name).await?;
        Ok(session)
    }

    async fn handshake(&mut self, server: &str) -> Result<Vec<McpTool>, McpError> {
        self.request(
            "initialize",
            json!({
                "protocolVersion": "2024-11-05",
                "capabilities": {},
                "clientInfo": {"name": "slowcode", "version": env!("CARGO_PKG_VERSION")}
            }),
        )
        .await?;
        self.notify("notifications/initialized", json!({})).await?;
        let tools = self.request("tools/list", json!({})).await?;
        parse_tools(server, &tools)
    }

    pub fn server(&self) -> &str {
        &self.server
    }

    pub fn tools(&self) -> &[McpTool] {
        &self.tools
    }

    /// Call one tool and flatten text content.
    pub async fn call(&mut self, name: &str, arguments: Value) -> Result<String, McpError> {
        let result = self
            .request("tools/call", json!({"name": name, "arguments": arguments}))
            .await?;
        if result
            .get("isError")
            .and_then(Value::as_bool)
            .unwrap_or(false)
        {
            return Err(McpError::Tool(
                result
                    .get("content")
                    .map(flatten_content)
                    .unwrap_or_else(|| "tool returned an error".to_owned()),
            ));
        }
        Ok(result
            .get("content")
            .map(flatten_content)
            .unwrap_or_else(|| result.to_string()))
    }

    async fn request(&mut self, method: &str, params: Value) -> Result<Value, McpError> {
        let id = self.next_id;
        self.next_id = self.next_id.saturating_add(1);
        let payload = json!({
            "jsonrpc": "2.0",
            "id": id,
            "method": method,
            "params": params,
        });
        let timeout = if method == "tools/call" {
            CALL_TIMEOUT
        } else {
            HANDSHAKE_TIMEOUT
        };
        let response = tokio::time::timeout(timeout, self.exchange(&payload, id))
            .await
            .map_err(|_| McpError::Timeout)??;
        if let Some(error) = response.get("error") {
            return Err(McpError::Protocol(error.to_string()));
        }
        Ok(response.get("result").cloned().unwrap_or(Value::Null))
    }

    async fn notify(&mut self, method: &str, params: Value) -> Result<(), McpError> {
        let payload = json!({
            "jsonrpc": "2.0",
            "method": method,
            "params": params,
        });
        match &mut self.transport {
            SessionTransport::Stdio { stdin, .. } => write_stdio_frame(stdin, &payload).await,
            SessionTransport::Http {
                client,
                url,
                headers,
                session_id,
            } => {
                http_request(
                    client,
                    url.as_str(),
                    headers,
                    session_id.as_deref(),
                    &payload,
                )?
                .send()
                .await
                .map_err(|error| McpError::Transport(error.to_string()))?;
                Ok(())
            }
        }
    }

    async fn exchange(&mut self, payload: &Value, id: u64) -> Result<Value, McpError> {
        match &mut self.transport {
            SessionTransport::Stdio { stdin, stdout, .. } => {
                write_stdio_frame(stdin, payload).await?;
                read_stdio_reply(stdout, id).await
            }
            SessionTransport::Http {
                client,
                url,
                headers,
                session_id,
            } => {
                let response = http_request(
                    client,
                    url.as_str(),
                    headers,
                    session_id.as_deref(),
                    payload,
                )?
                .send()
                .await
                .map_err(|error| McpError::Transport(error.to_string()))?;
                if let Some(id) = response
                    .headers()
                    .get("mcp-session-id")
                    .and_then(|value| value.to_str().ok())
                {
                    *session_id = Some(id.to_owned());
                }
                let content_type = response
                    .headers()
                    .get(reqwest::header::CONTENT_TYPE)
                    .and_then(|value| value.to_str().ok())
                    .unwrap_or_default()
                    .to_ascii_lowercase();
                let text = response
                    .text()
                    .await
                    .map_err(|error| McpError::Transport(error.to_string()))?;
                parse_http_body(&text, &content_type, id)
            }
        }
    }

    pub async fn shutdown(mut self) {
        if let SessionTransport::Stdio { child, .. } = &mut self.transport {
            let _ = child.kill().await;
        }
    }
}

fn spawn_stdio_session(
    server: &str,
    command: &str,
    args: &[String],
    env: &BTreeMap<String, String>,
) -> Result<McpSession, McpError> {
    let mut child = Command::new(command)
        .args(args)
        .envs(env)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .kill_on_drop(true)
        .spawn()
        .map_err(|error| McpError::Transport(error.to_string()))?;
    let stdin = child
        .stdin
        .take()
        .ok_or_else(|| McpError::Transport("missing stdin".to_owned()))?;
    let stdout = child
        .stdout
        .take()
        .ok_or_else(|| McpError::Transport("missing stdout".to_owned()))?;
    Ok(McpSession {
        server: server.to_owned(),
        transport: SessionTransport::Stdio {
            child,
            stdin,
            stdout: BufReader::new(stdout),
        },
        tools: Vec::new(),
        next_id: 1,
    })
}

fn http_session(server: &str, url: &str, headers: &BTreeMap<String, String>) -> McpSession {
    McpSession {
        server: server.to_owned(),
        transport: SessionTransport::Http {
            client: reqwest::Client::new(),
            url: url.to_owned(),
            headers: headers.clone(),
            session_id: None,
        },
        tools: Vec::new(),
        next_id: 1,
    }
}

fn jsonrpc_frame(payload: &Value) -> Result<Vec<u8>, McpError> {
    let mut line =
        serde_json::to_vec(payload).map_err(|error| McpError::Protocol(error.to_string()))?;
    line.push(b'\n');
    Ok(line)
}

async fn write_stdio_frame<W>(stdin: &mut W, payload: &Value) -> Result<(), McpError>
where
    W: tokio::io::AsyncWrite + Unpin,
{
    let line = jsonrpc_frame(payload)?;
    stdin
        .write_all(&line)
        .await
        .map_err(|error| McpError::Transport(error.to_string()))?;
    stdin
        .flush()
        .await
        .map_err(|error| McpError::Transport(error.to_string()))
}

async fn read_stdio_reply<R>(stdout: &mut R, id: u64) -> Result<Value, McpError>
where
    R: tokio::io::AsyncBufRead + Unpin,
{
    loop {
        let mut response = String::new();
        let read = stdout
            .read_line(&mut response)
            .await
            .map_err(|error| McpError::Transport(error.to_string()))?;
        if read == 0 {
            return Err(McpError::Transport("server closed stdout".to_owned()));
        }
        let Ok(value) = serde_json::from_str::<Value>(&response) else {
            continue;
        };
        if value.get("id").and_then(Value::as_u64) == Some(id) {
            return Ok(value);
        }
    }
}

fn expanded_headers(headers: &BTreeMap<String, String>) -> Result<Vec<(String, String)>, McpError> {
    headers
        .iter()
        .map(|(key, value)| Ok((key.clone(), expand(value)?)))
        .collect()
}

fn http_request(
    client: &reqwest::Client,
    url: &str,
    headers: &BTreeMap<String, String>,
    session_id: Option<&str>,
    payload: &Value,
) -> Result<reqwest::RequestBuilder, McpError> {
    let mut request = client.post(url).json(payload);
    for (key, value) in expanded_headers(headers)? {
        request = request.header(key, value);
    }
    if let Some(id) = session_id {
        request = request.header("Mcp-Session-Id", id);
    }
    Ok(request)
}

fn parse_http_body(text: &str, content_type: &str, id: u64) -> Result<Value, McpError> {
    if content_type.contains("text/event-stream") {
        parse_sse_response(text, id)
    } else {
        serde_json::from_str(text).map_err(|error| McpError::Protocol(error.to_string()))
    }
}

/// MCP errors with actionable messages.
#[derive(Debug, Error)]
pub enum McpError {
    #[error("config: {0}")]
    Config(String),
    #[error("transport: {0}")]
    Transport(String),
    #[error("protocol: {0}")]
    Protocol(String),
    #[error("tool: {0}")]
    Tool(String),
    #[error("install: {0}")]
    Install(String),
    #[error("timed out")]
    Timeout,
}

/// Connected MCP servers plus the tool-visible catalog.
pub struct McpToolset {
    pub executor: Arc<dyn ToolExecutor>,
    pub schemas: Vec<ToolSchema>,
    pub tools: Vec<McpTool>,
    pub errors: Vec<(String, String)>,
}

/// Runtime options for one MCP toolset.
#[derive(Clone)]
pub struct McpToolsetOptions {
    pub gate: Arc<McpApprovalGate>,
}

impl Default for McpToolsetOptions {
    fn default() -> Self {
        Self {
            gate: McpApprovalGate::new(false),
        }
    }
}

/// Connect every enabled server and build a model-callable tool surface.
///
/// A failed server is reported in `errors` and does not remove tools from the
/// servers that connected successfully.
pub async fn connect_toolset(servers: &[McpServerConfig]) -> McpToolset {
    connect_toolset_with_options(servers, McpToolsetOptions::default()).await
}

/// Connect every enabled server with an explicit approval gate.
pub async fn connect_toolset_with_options(
    servers: &[McpServerConfig],
    options: McpToolsetOptions,
) -> McpToolset {
    let server_configs = servers
        .iter()
        .map(|server| (server.name.clone(), server.clone()))
        .collect::<BTreeMap<_, _>>();
    let (sessions, tools, mut errors) = open_sessions(servers).await;

    let mut mapping = BTreeMap::new();
    let mut policies = BTreeMap::new();
    let mut schemas = Vec::new();
    let mut kept = Vec::new();
    for tool in tools {
        let server_name = tool.server.clone();
        match register_tool(tool, server_configs.get(&server_name)) {
            Ok(registered) => {
                let qualified = registered.schema.name().to_owned();
                mapping.insert(
                    qualified.clone(),
                    (registered.server.clone(), registered.name.clone()),
                );
                policies.insert(qualified, registered.policy);
                schemas.push(registered.schema);
                kept.push(registered.tool);
            }
            Err(error) => errors.push(error),
        }
    }
    schemas.sort_by(|left, right| left.name().cmp(right.name()));

    let executor = Arc::new(McpToolExecutor {
        sessions: Arc::new(tokio::sync::Mutex::new(sessions)),
        mapping,
        gate: options.gate,
        policies,
    });
    McpToolset {
        executor,
        schemas,
        tools: kept,
        errors,
    }
}

async fn open_sessions(
    servers: &[McpServerConfig],
) -> (
    BTreeMap<String, McpSession>,
    Vec<McpTool>,
    Vec<(String, String)>,
) {
    let mut sessions = BTreeMap::new();
    let mut tools = Vec::new();
    let mut errors = Vec::new();
    for server in servers.iter().filter(|server| server.enabled) {
        match McpSession::connect(server).await {
            Ok(session) => {
                tools.extend(session.tools().iter().cloned());
                sessions.insert(server.name.clone(), session);
            }
            Err(error) => errors.push((server.name.clone(), error.to_string())),
        }
    }
    (sessions, tools, errors)
}

struct RegisteredTool {
    server: String,
    name: String,
    schema: ToolSchema,
    policy: McpToolPolicy,
    tool: McpTool,
}

fn register_tool(
    tool: McpTool,
    config: Option<&McpServerConfig>,
) -> Result<RegisteredTool, (String, String)> {
    let policy = McpToolPolicy {
        server: tool.server.clone(),
        trusted: config
            .is_some_and(|server| server.trusted && server.scope == Some(McpScope::User)),
        read_only: tool.annotations.is_safe_read_only(),
        destructive: tool.annotations.is_destructive(),
        source: config.and_then(|server| server.source.as_ref().map(|source| source.label())),
    };
    let parameters = JsonValue::new(tool.input_schema.clone())
        .map_err(|error| (tool.server.clone(), error.to_string()))?;
    let schema = ToolSchema::new(tool.qualified_name(), tool.description.clone(), parameters)
        .map_err(|error| (tool.server.clone(), error.to_string()))?;
    Ok(RegisteredTool {
        server: tool.server.clone(),
        name: tool.name.clone(),
        schema,
        policy,
        tool,
    })
}

struct McpToolExecutor {
    sessions: Arc<tokio::sync::Mutex<BTreeMap<String, McpSession>>>,
    mapping: BTreeMap<String, (String, String)>,
    gate: Arc<McpApprovalGate>,
    policies: BTreeMap<String, McpToolPolicy>,
}

#[derive(Clone)]
struct McpToolPolicy {
    server: String,
    trusted: bool,
    read_only: bool,
    destructive: bool,
    source: Option<String>,
}

impl ToolExecutor for McpToolExecutor {
    fn execution_mode(&self, _tool_name: &str) -> ToolExecutionMode {
        // MCP tools may mutate external state. Keep them exclusive by default.
        ToolExecutionMode::Exclusive
    }

    fn execute(
        &self,
        request: ToolExecutionRequest,
        cancellation: tokio_util::sync::CancellationToken,
    ) -> ToolExecutionFuture<'_> {
        let sessions = Arc::clone(&self.sessions);
        let mapping = self.mapping.clone();
        let gate = Arc::clone(&self.gate);
        let policy = self.policies.get(request.name()).cloned();
        Box::pin(async move {
            let name = request.name();
            let Some((server, tool)) = mapping.get(name) else {
                return Err(ToolExecutorError::new("unknown MCP tool"));
            };
            let Some(policy) = policy else {
                return Err(ToolExecutorError::new("MCP tool policy is missing"));
            };
            let call_id = request.call_id().clone();
            let raw_arguments = request.raw_arguments().to_owned();
            if let Err(error) = ensure_mcp_tool_allowed(
                &gate,
                name,
                &policy,
                call_id,
                raw_arguments,
                cancellation.clone(),
            )
            .await
            {
                let block = ContentBlock::text(format!("MCP tool not executed: {error}"))
                    .map_err(|_| ToolExecutorError::new("MCP approval error is too large"))?;
                return ToolExecutionResult::model_error(
                    vec![block],
                    ToolFailure {
                        name: name.to_owned(),
                        code: "MCP_APPROVAL_DENIED".to_owned(),
                    },
                )
                .map_err(|_| ToolExecutorError::new("MCP approval result is invalid"));
            }
            let arguments = serde_json::from_str(request.raw_arguments())
                .unwrap_or_else(|_| Value::Object(Default::default()));
            let mut sessions = sessions.lock().await;
            let Some(session) = sessions.get_mut(server) else {
                return Err(ToolExecutorError::new("MCP server is unavailable"));
            };
            match session.call(tool, arguments).await {
                Ok(text) => {
                    let block = ContentBlock::text(text)
                        .map_err(|_| ToolExecutorError::new("MCP result is too large"))?;
                    ToolExecutionResult::success(vec![block])
                        .map_err(|_| ToolExecutorError::new("MCP result is invalid"))
                }
                Err(error) => {
                    let block = ContentBlock::text(format!("MCP tool failed: {error}"))
                        .map_err(|_| ToolExecutorError::new("MCP error is too large"))?;
                    ToolExecutionResult::model_error(
                        vec![block],
                        ToolFailure {
                            name: name.to_owned(),
                            code: "MCP_TOOL_ERROR".to_owned(),
                        },
                    )
                    .map_err(|_| ToolExecutorError::new("MCP error result is invalid"))
                }
            }
        })
    }

    fn prepare(
        &self,
        request: ToolExecutionRequest,
        cancellation: tokio_util::sync::CancellationToken,
    ) -> ToolPreparationFuture<'_> {
        let execution = self.execute(request, cancellation);
        Box::pin(async move {
            execution
                .await
                .map(slowcode_kernel::agent::ToolPreparation::Complete)
        })
    }

    fn shutdown(&self) -> ToolShutdownFuture<'_> {
        let sessions = Arc::clone(&self.sessions);
        Box::pin(async move {
            let drained = {
                let mut guard = sessions.lock().await;
                std::mem::take(&mut *guard)
            };
            for (_, session) in drained {
                session.shutdown().await;
            }
            Ok(())
        })
    }
}

async fn ensure_mcp_tool_allowed(
    gate: &McpApprovalGate,
    name: &str,
    policy: &McpToolPolicy,
    call_id: slowcode_kernel::model::CallId,
    raw_arguments: String,
    cancellation: tokio_util::sync::CancellationToken,
) -> Result<(), McpApprovalError> {
    if gate.read_only() {
        return if policy.read_only {
            Ok(())
        } else {
            Err(McpApprovalError::ReadOnlyDenied)
        };
    }
    if gate.role_read_only() {
        return if policy.read_only {
            Ok(())
        } else {
            Err(McpApprovalError::ReadOnlyDenied)
        };
    }
    if policy.trusted || policy.read_only || gate.is_granted(name) {
        return Ok(());
    }
    let preview = approval_preview(
        &policy.server,
        policy.source.as_deref(),
        name,
        &raw_arguments,
        policy.destructive,
    );
    let request = ApprovalRequest::for_tool(
        approval_id_for_call(&call_id),
        name.to_owned(),
        call_id,
        Some("MCP tool call".to_owned()),
        preview,
    )
    .map_err(|_| McpApprovalError::Failed)?;
    gate.request(request, cancellation).await?;
    gate.grant(name);
    Ok(())
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
struct RawConfig {
    #[serde(default, rename = "mcpServers")]
    servers: BTreeMap<String, RawServer>,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
struct RawServer {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    command: Option<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    args: Vec<String>,
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    env: BTreeMap<String, String>,
    #[serde(default, rename = "type", skip_serializing_if = "Option::is_none")]
    kind: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    url: Option<String>,
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    headers: BTreeMap<String, String>,
    #[serde(default, skip_serializing_if = "is_false")]
    trusted: bool,
    #[serde(default = "default_true", skip_serializing_if = "is_true")]
    enabled: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    source: Option<McpSource>,
    #[serde(flatten)]
    extra: BTreeMap<String, Value>,
}

impl From<McpServerConfig> for RawServer {
    fn from(server: McpServerConfig) -> Self {
        match server.transport {
            McpTransport::Stdio { command, args, env } => Self {
                command: Some(command),
                args,
                env,
                trusted: server.trusted,
                enabled: server.enabled,
                source: server.source,
                ..Self::default()
            },
            McpTransport::Http { url, headers } => Self {
                kind: Some("http".to_owned()),
                url: Some(url),
                headers,
                trusted: server.trusted,
                enabled: server.enabled,
                source: server.source,
                ..Self::default()
            },
        }
    }
}

impl RawServer {
    fn into_config(self, name: String, scope: McpScope) -> Result<McpServerConfig, McpError> {
        let transport = match (self.command, self.url) {
            (Some(command), _) => stdio_transport(command, self.args, self.env)?,
            (None, Some(url)) => http_transport(&name, url, self.kind.as_deref(), self.headers)?,
            (None, None) => {
                return Err(McpError::Config(format!(
                    "{name}: server has neither command nor url"
                )));
            }
        };
        Ok(McpServerConfig {
            name,
            transport,
            trusted: self.trusted,
            enabled: self.enabled,
            scope: Some(scope),
            source: self.source,
        })
    }
}

fn stdio_transport(
    command: String,
    args: Vec<String>,
    env: BTreeMap<String, String>,
) -> Result<McpTransport, McpError> {
    Ok(McpTransport::Stdio {
        command,
        args,
        env: expand_values(env)?,
    })
}

fn http_transport(
    name: &str,
    url: String,
    kind: Option<&str>,
    headers: BTreeMap<String, String>,
) -> Result<McpTransport, McpError> {
    let kind = kind.unwrap_or("http");
    if !matches!(kind, "http" | "streamable-http" | "sse") {
        return Err(McpError::Config(format!(
            "{name}: unsupported transport {kind}"
        )));
    }
    validate_remote_url(&url)?;
    Ok(McpTransport::Http {
        url,
        headers: expand_values(headers)?,
    })
}

fn expand_values(values: BTreeMap<String, String>) -> Result<BTreeMap<String, String>, McpError> {
    values
        .into_iter()
        .map(|(key, value)| Ok((key, expand(&value)?)))
        .collect()
}

fn validate_remote_url(url: &str) -> Result<(), McpError> {
    if url.starts_with("https://") {
        return Ok(());
    }
    for loopback in ["http://127.0.0.1", "http://localhost", "http://[::1]"] {
        if url == loopback
            || url.starts_with(&format!("{loopback}:"))
            || url.starts_with(&format!("{loopback}/"))
        {
            return Ok(());
        }
    }
    Err(McpError::Config(format!(
        "remote MCP URL must use HTTPS (loopback HTTP is allowed): {url}"
    )))
}

fn read_file(path: &Path, scope: McpScope) -> Result<BTreeMap<String, McpServerConfig>, McpError> {
    let raw = read_raw(path)?;
    raw.servers
        .into_iter()
        .map(|(name, server)| Ok((name.clone(), server.into_config(name, scope)?)))
        .collect()
}

fn read_raw(path: &Path) -> Result<RawConfig, McpError> {
    if !path.exists() {
        return Ok(RawConfig::default());
    }
    let text = std::fs::read_to_string(path)
        .map_err(|error| McpError::Config(format!("{}: {error}", path.display())))?;
    serde_json::from_str(&text)
        .map_err(|error| McpError::Config(format!("{}: {error}", path.display())))
}

fn write_raw(path: &Path, config: &RawConfig) -> Result<(), McpError> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).map_err(|error| McpError::Config(error.to_string()))?;
    }
    let text = serde_json::to_string_pretty(config)
        .map_err(|error| McpError::Config(error.to_string()))?;
    let temp = path.with_extension(format!("tmp-{}", std::process::id()));
    std::fs::write(&temp, text).map_err(|error| McpError::Config(error.to_string()))?;
    std::fs::rename(&temp, path).map_err(|error| McpError::Config(error.to_string()))?;
    Ok(())
}

fn expand(value: &str) -> Result<String, McpError> {
    let mut output = String::new();
    let mut rest = value;
    while let Some(start) = rest.find("${") {
        output.push_str(&rest[..start]);
        let Some(end) = rest[start..].find('}') else {
            return Err(McpError::Config(format!("unclosed variable in {value}")));
        };
        let expression = &rest[start + 2..start + end];
        let (name, default) = expression.split_once(":-").unwrap_or((expression, ""));
        let resolved = std::env::var(name).unwrap_or_else(|_| default.to_owned());
        if resolved.is_empty() {
            return Err(McpError::Config(format!(
                "environment variable {name} is not defined"
            )));
        }
        output.push_str(&resolved);
        rest = &rest[start + end + 1..];
    }
    output.push_str(rest);
    Ok(output)
}

fn parse_tools(server: &str, result: &Value) -> Result<Vec<McpTool>, McpError> {
    let tools = result
        .get("tools")
        .and_then(Value::as_array)
        .ok_or_else(|| McpError::Protocol("tools/list returned no tools array".to_owned()))?;
    Ok(tools
        .iter()
        .filter_map(|tool| {
            let name = tool.get("name")?.as_str()?.to_owned();
            let description = tool
                .get("description")
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_owned();
            let input_schema = tool
                .get("inputSchema")
                .cloned()
                .unwrap_or_else(|| json!({"type": "object"}));
            let hints = tool.get("annotations");
            Some(McpTool {
                server: server.to_owned(),
                name,
                description,
                input_schema,
                annotations: McpToolAnnotations {
                    read_only: hints.and_then(|value| value.get("readOnlyHint")?.as_bool()),
                    destructive: hints.and_then(|value| value.get("destructiveHint")?.as_bool()),
                    idempotent: hints.and_then(|value| value.get("idempotentHint")?.as_bool()),
                    open_world: hints.and_then(|value| value.get("openWorldHint")?.as_bool()),
                },
            })
        })
        .collect())
}

fn flatten_content(content: &Value) -> String {
    let Some(items) = content.as_array() else {
        return content.to_string();
    };
    items
        .iter()
        .map(|item| match item.get("type").and_then(Value::as_str) {
            Some("text") => item
                .get("text")
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_owned(),
            _ => item.to_string(),
        })
        .collect::<Vec<_>>()
        .join("\n")
}

fn parse_sse_response(text: &str, id: u64) -> Result<Value, McpError> {
    let mut data = Vec::new();
    for line in text.lines().chain(std::iter::once("")) {
        if let Some(rest) = line.strip_prefix("data:") {
            data.push(rest.strip_prefix(' ').unwrap_or(rest));
            continue;
        }
        if line.trim().is_empty() && !data.is_empty() {
            let payload = data.join("\n");
            data.clear();
            let Ok(value) = serde_json::from_str::<Value>(&payload) else {
                continue;
            };
            if matches_response_id(&value, id) {
                return Ok(value);
            }
        }
    }
    Err(McpError::Protocol(format!(
        "SSE response did not contain a JSON-RPC reply for id {id}"
    )))
}

fn matches_response_id(value: &Value, id: u64) -> bool {
    match value.get("id") {
        Some(Value::Number(number)) => number.as_u64() == Some(id),
        Some(Value::String(value)) => value == &id.to_string(),
        _ => false,
    }
}

fn sanitize(value: &str) -> String {
    value
        .chars()
        .map(|ch| {
            if ch.is_ascii_alphanumeric() || ch == '_' || ch == '-' {
                ch
            } else {
                '_'
            }
        })
        .collect()
}

fn default_true() -> bool {
    true
}

fn is_true(value: &bool) -> bool {
    *value
}

fn is_false(value: &bool) -> bool {
    !*value
}

#[cfg(test)]
mod tests {
    use super::*;
    use slowcode_kernel::{
        agent::{ApprovalFuture, ApprovalProvider, ApprovalRequest},
        model::CallId,
        session::ApprovalOutcome,
    };
    use std::sync::atomic::{AtomicUsize, Ordering};

    struct CountingApproval {
        calls: Arc<AtomicUsize>,
        outcome: ApprovalOutcome,
    }

    impl ApprovalProvider for CountingApproval {
        fn request(
            &self,
            _request: ApprovalRequest,
            _cancellation: tokio_util::sync::CancellationToken,
        ) -> ApprovalFuture<'_> {
            self.calls.fetch_add(1, Ordering::Relaxed);
            let outcome = self.outcome;
            Box::pin(async move { Ok(outcome) })
        }
    }

    fn policy(read_only: bool, trusted: bool) -> McpToolPolicy {
        McpToolPolicy {
            server: "demo".to_owned(),
            trusted,
            read_only,
            destructive: !read_only,
            source: None,
        }
    }

    #[tokio::test]
    async fn mcp_approval_asks_once_for_unknown_tools() {
        let gate = McpApprovalGate::new(false);
        let calls = Arc::new(AtomicUsize::new(0));
        gate.set_provider(Arc::new(CountingApproval {
            calls: Arc::clone(&calls),
            outcome: ApprovalOutcome::AllowedOnce,
        }));
        let call_id = CallId::new("call-1");
        ensure_mcp_tool_allowed(
            &gate,
            "mcp__demo__write",
            &policy(false, false),
            call_id.clone(),
            "{}".to_owned(),
            tokio_util::sync::CancellationToken::new(),
        )
        .await
        .unwrap();
        assert_eq!(calls.load(Ordering::Relaxed), 1);
        ensure_mcp_tool_allowed(
            &gate,
            "mcp__demo__write",
            &policy(false, false),
            call_id,
            "{}".to_owned(),
            tokio_util::sync::CancellationToken::new(),
        )
        .await
        .unwrap();
        assert_eq!(calls.load(Ordering::Relaxed), 1);
    }

    #[tokio::test]
    async fn read_only_and_trusted_tools_skip_approval() {
        let gate = McpApprovalGate::new(false);
        let call_id = CallId::new("call-1");
        ensure_mcp_tool_allowed(
            &gate,
            "mcp__demo__read",
            &policy(true, false),
            call_id.clone(),
            "{}".to_owned(),
            tokio_util::sync::CancellationToken::new(),
        )
        .await
        .unwrap();
        ensure_mcp_tool_allowed(
            &gate,
            "mcp__demo__write",
            &policy(false, true),
            call_id,
            "{}".to_owned(),
            tokio_util::sync::CancellationToken::new(),
        )
        .await
        .unwrap();
    }

    #[tokio::test]
    async fn role_read_only_denies_trusted_writes_without_asking() {
        let gate = McpApprovalGate::new(false);
        gate.set_role_read_only(true);
        let calls = Arc::new(AtomicUsize::new(0));
        gate.set_provider(Arc::new(CountingApproval {
            calls: Arc::clone(&calls),
            outcome: ApprovalOutcome::AllowedOnce,
        }));
        let error = ensure_mcp_tool_allowed(
            &gate,
            "mcp__demo__write",
            &policy(false, true),
            CallId::new("call-1"),
            "{}".to_owned(),
            tokio_util::sync::CancellationToken::new(),
        )
        .await
        .expect_err("role read-only denies trusted writes");
        assert_eq!(error, McpApprovalError::ReadOnlyDenied);
        assert_eq!(calls.load(Ordering::Relaxed), 0);
    }

    #[tokio::test]
    async fn role_read_only_allows_safe_read_only_tools() {
        let gate = McpApprovalGate::new(false);
        gate.set_role_read_only(true);
        ensure_mcp_tool_allowed(
            &gate,
            "mcp__demo__read",
            &policy(true, false),
            CallId::new("call-1"),
            "{}".to_owned(),
            tokio_util::sync::CancellationToken::new(),
        )
        .await
        .expect("safe read-only tools pass in a read-only role");
    }

    #[tokio::test]
    async fn role_read_only_ignores_existing_grants() {
        let gate = McpApprovalGate::new(false);
        gate.grant("mcp__demo__write");
        gate.set_role_read_only(true);
        let error = ensure_mcp_tool_allowed(
            &gate,
            "mcp__demo__write",
            &policy(false, false),
            CallId::new("call-1"),
            "{}".to_owned(),
            tokio_util::sync::CancellationToken::new(),
        )
        .await
        .expect_err("a grant must not bypass the role boundary");
        assert_eq!(error, McpApprovalError::ReadOnlyDenied);
    }

    #[tokio::test]
    async fn read_only_mode_denies_unknown_tools_without_asking() {
        let gate = McpApprovalGate::new(true);
        let calls = Arc::new(AtomicUsize::new(0));
        gate.set_provider(Arc::new(CountingApproval {
            calls: Arc::clone(&calls),
            outcome: ApprovalOutcome::AllowedOnce,
        }));
        let error = ensure_mcp_tool_allowed(
            &gate,
            "mcp__demo__write",
            &policy(false, false),
            CallId::new("call-1"),
            "{}".to_owned(),
            tokio_util::sync::CancellationToken::new(),
        )
        .await
        .unwrap_err();
        assert_eq!(error, McpApprovalError::ReadOnlyDenied);
        assert_eq!(calls.load(Ordering::Relaxed), 0);
    }

    #[test]
    fn expands_environment_and_defaults() {
        // Rust 2024 marks process environment mutation unsafe.
        unsafe { std::env::set_var("SLOWCODE_MCP_TEST", "value") };
        assert_eq!(expand("${SLOWCODE_MCP_TEST}").unwrap(), "value");
        assert_eq!(
            expand("${SLOWCODE_MCP_MISSING:-fallback}").unwrap(),
            "fallback"
        );
        assert!(expand("${SLOWCODE_MCP_MISSING}").is_err());
        unsafe { std::env::remove_var("SLOWCODE_MCP_TEST") };
    }

    #[test]
    fn project_scope_overrides_user_scope() {
        let workspace = std::env::temp_dir().join(format!(
            "slowcode-mcp-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(workspace.join(".slowcode")).unwrap();
        std::fs::write(
            workspace.join(".slowcode/mcp.json"),
            r#"{"mcpServers":{"echo":{"command":"cat","trusted":true}}}"#,
        )
        .unwrap();
        let config = McpConfig {
            servers: BTreeMap::new(),
            user_path: workspace.join("user.json"),
            project_path: workspace.join(".slowcode/mcp.json"),
        };
        let config = config.reload().unwrap();
        let server = config.servers.get("echo").unwrap();
        assert_eq!(server.scope, Some(McpScope::Project));
        assert!(server.trusted);
        std::fs::remove_dir_all(workspace).unwrap();
    }

    #[test]
    fn qualified_names_are_stable() {
        let tool = McpTool {
            server: "chrome devtools".to_owned(),
            name: "click".to_owned(),
            description: String::new(),
            input_schema: json!({}),
            annotations: McpToolAnnotations::default(),
        };
        assert_eq!(tool.qualified_name(), "mcp__chrome_devtools__click");
    }

    #[test]
    fn conflicting_read_only_hints_are_not_trusted() {
        let annotations = McpToolAnnotations {
            read_only: Some(true),
            destructive: Some(true),
            ..McpToolAnnotations::default()
        };
        assert!(!annotations.is_safe_read_only());
        assert!(annotations.is_destructive());
    }

    #[test]
    fn parses_sse_responses_by_request_id() {
        let text = "event: message\ndata: {\"jsonrpc\":\"2.0\",\"method\":\"notifications/progress\"}\n\ndata: {\"jsonrpc\":\"2.0\",\"id\":7,\"result\":{\"tools\":[]}}\n\n";
        let value = parse_sse_response(text, 7).unwrap();
        assert_eq!(value["result"]["tools"], json!([]));
        assert!(parse_sse_response(text, 8).is_err());
    }

    #[test]
    fn rejects_plaintext_remote_urls() {
        assert!(validate_remote_url("https://example.com/mcp").is_ok());
        assert!(validate_remote_url("http://127.0.0.1:3000/mcp").is_ok());
        assert!(validate_remote_url("http://localhost/mcp").is_ok());
        assert!(validate_remote_url("http://example.com/mcp").is_err());
        assert!(validate_remote_url("http://localhost.evil.com/mcp").is_err());
    }

    #[tokio::test]
    async fn stdio_server_lists_and_calls_tools() {
        let dir = std::env::temp_dir().join(format!(
            "slowcode-mcp-stdio-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        let script = dir.join("server.py");
        std::fs::write(
            &script,
            r#"
import json, sys
for line in sys.stdin:
    request = json.loads(line)
    if "id" not in request:
        continue
    method = request.get("method")
    if method == "initialize":
        result = {"protocolVersion": "2024-11-05", "capabilities": {}, "serverInfo": {"name": "fake", "version": "1"}}
    elif method == "tools/list":
        result = {"tools": [{"name": "echo", "description": "echo text", "inputSchema": {"type": "object", "properties": {"text": {"type": "string"}}}}]}
    elif method == "tools/call":
        arguments = request.get("params", {}).get("arguments", {})
        result = {"content": [{"type": "text", "text": arguments.get("text", "")}]}
    else:
        result = {}
    print(json.dumps({"jsonrpc": "2.0", "id": request["id"], "result": result}), flush=True)
"#,
        )
        .unwrap();
        let server = McpServerConfig {
            name: "demo".to_owned(),
            transport: McpTransport::Stdio {
                command: "python3".to_owned(),
                args: vec![script.display().to_string()],
                env: BTreeMap::new(),
            },
            trusted: false,
            enabled: true,
            scope: Some(McpScope::Project),
            source: None,
        };
        let mut session = McpSession::connect(&server).await.unwrap();
        assert_eq!(session.tools().len(), 1);
        assert_eq!(session.tools()[0].qualified_name(), "mcp__demo__echo");
        let text = session
            .call("echo", json!({"text": "hello"}))
            .await
            .unwrap();
        assert_eq!(text, "hello");
        session.shutdown().await;
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn upsert_and_remove_round_trip_through_the_config_file() {
        let dir = std::env::temp_dir().join(format!(
            "slowcode-mcp-write-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        let config = McpConfig {
            servers: BTreeMap::new(),
            user_path: dir.join("user.json"),
            project_path: dir.join("project.json"),
        };
        let server = McpServerConfig {
            name: "demo".to_owned(),
            transport: McpTransport::Stdio {
                command: "python3".to_owned(),
                args: Vec::new(),
                env: BTreeMap::new(),
            },
            trusted: false,
            enabled: true,
            scope: Some(McpScope::Project),
            source: None,
        };
        config.upsert(McpScope::Project, server).unwrap();
        let config = config.reload().unwrap();
        assert!(config.servers.contains_key("demo"));
        assert_eq!(config.servers["demo"].scope, Some(McpScope::Project));

        config.remove(McpScope::Project, "demo").unwrap();
        let config = config.reload().unwrap();
        assert!(!config.servers.contains_key("demo"));
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn source_metadata_round_trips_and_unknown_fields_survive() {
        let dir = std::env::temp_dir().join(format!(
            "slowcode-mcp-source-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        let project = dir.join("project.json");
        std::fs::write(
            &project,
            r#"{"mcpServers":{"demo":{"command":"npx","args":["-y","demo@1.0.0"],"custom":"keep","source":{"kind":"github","repo":"owner/demo","package":"demo","version":"1.0.0","bin":"demo"}}}}"#,
        )
        .unwrap();
        let config = McpConfig {
            servers: BTreeMap::new(),
            user_path: dir.join("user.json"),
            project_path: project.clone(),
        }
        .reload()
        .unwrap();
        let server = config.servers.get("demo").unwrap();
        assert_eq!(server.source.as_ref().unwrap().package, "demo");

        let other = McpServerConfig {
            name: "other".to_owned(),
            transport: McpTransport::Stdio {
                command: "cat".to_owned(),
                args: Vec::new(),
                env: BTreeMap::new(),
            },
            trusted: false,
            enabled: true,
            scope: Some(McpScope::Project),
            source: None,
        };
        config.upsert(McpScope::Project, other).unwrap();
        let text = std::fs::read_to_string(&project).unwrap();
        assert!(text.contains("\"custom\": \"keep\""));
        assert!(text.contains("\"source\""));
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[tokio::test]
    async fn connect_toolset_builds_kernel_schemas() {
        let dir = std::env::temp_dir().join(format!(
            "slowcode-mcp-toolset-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        let script = dir.join("server.py");
        std::fs::write(
            &script,
            r#"
import json, sys
for line in sys.stdin:
    request = json.loads(line)
    if "id" not in request:
        continue
    method = request.get("method")
    if method == "initialize":
        result = {"protocolVersion": "2024-11-05", "capabilities": {}, "serverInfo": {"name": "fake", "version": "1"}}
    elif method == "tools/list":
        result = {"tools": [{"name": "echo", "description": "echo text", "inputSchema": {"type": "object"}}]}
    else:
        result = {}
    print(json.dumps({"jsonrpc": "2.0", "id": request["id"], "result": result}), flush=True)
"#,
        )
        .unwrap();
        let server = McpServerConfig {
            name: "demo".to_owned(),
            transport: McpTransport::Stdio {
                command: "python3".to_owned(),
                args: vec![script.display().to_string()],
                env: BTreeMap::new(),
            },
            trusted: false,
            enabled: true,
            scope: Some(McpScope::Project),
            source: None,
        };
        let toolset = connect_toolset(&[server]).await;
        assert!(toolset.errors.is_empty(), "{:?}", toolset.errors);
        assert_eq!(toolset.schemas.len(), 1);
        assert_eq!(toolset.schemas[0].name(), "mcp__demo__echo");
        assert_eq!(toolset.tools[0].qualified_name(), "mcp__demo__echo");
        toolset.executor.shutdown().await.unwrap();
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[tokio::test]
    async fn connect_reports_missing_stdio_command() {
        let server = McpServerConfig {
            name: "missing".to_owned(),
            transport: McpTransport::Stdio {
                command: "slowcode-mcp-definitely-missing".to_owned(),
                args: Vec::new(),
                env: BTreeMap::new(),
            },
            trusted: false,
            enabled: true,
            scope: Some(McpScope::Project),
            source: None,
        };
        let error = McpSession::connect(&server)
            .await
            .err()
            .expect("connect should fail");
        assert!(matches!(error, McpError::Transport(_)), "{error}");
    }

    #[tokio::test]
    async fn stdio_exchange_skips_invalid_and_unrelated_lines() {
        let dir = std::env::temp_dir().join(format!(
            "slowcode-mcp-noise-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        let script = dir.join("server.py");
        std::fs::write(
            &script,
            r#"
import json, sys
for line in sys.stdin:
    request = json.loads(line)
    if "id" not in request:
        continue
    method = request.get("method")
    if method == "initialize":
        result = {"protocolVersion": "2024-11-05", "capabilities": {}, "serverInfo": {"name": "fake", "version": "1"}}
    elif method == "tools/list":
        result = {"tools": [{"name": "echo", "description": "echo text", "inputSchema": {"type": "object"}}]}
    elif method == "tools/call":
        result = {"content": [{"type": "text", "text": "ok"}]}
    else:
        result = {}
    print("this is not json", flush=True)
    print(json.dumps({"jsonrpc": "2.0", "method": "notifications/progress"}), flush=True)
    print(json.dumps({"jsonrpc": "2.0", "id": 999999, "result": {"ignored": True}}), flush=True)
    print(json.dumps({"jsonrpc": "2.0", "id": request["id"], "result": result}), flush=True)
"#,
        )
        .unwrap();
        let server = McpServerConfig {
            name: "demo".to_owned(),
            transport: McpTransport::Stdio {
                command: "python3".to_owned(),
                args: vec![script.display().to_string()],
                env: BTreeMap::new(),
            },
            trusted: false,
            enabled: true,
            scope: Some(McpScope::Project),
            source: None,
        };
        let mut session = McpSession::connect(&server).await.unwrap();
        assert_eq!(session.tools().len(), 1);
        assert_eq!(session.call("echo", json!({})).await.unwrap(), "ok");
        session.shutdown().await;
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn into_config_expands_env_and_validates_transports() {
        unsafe { std::env::set_var("SLOWCODE_MCP_INTO_CONFIG", "secret-token") };
        let stdio = RawServer {
            command: Some("npx".to_owned()),
            args: vec!["-y".to_owned()],
            env: BTreeMap::from([("TOKEN".to_owned(), "${SLOWCODE_MCP_INTO_CONFIG}".to_owned())]),
            ..RawServer::default()
        };
        let config = stdio
            .into_config("demo".to_owned(), McpScope::Project)
            .unwrap();
        let McpTransport::Stdio { command, env, .. } = &config.transport else {
            panic!("expected stdio transport");
        };
        assert_eq!(command, "npx");
        assert_eq!(env["TOKEN"], "secret-token");
        assert_eq!(config.scope, Some(McpScope::Project));
        unsafe { std::env::remove_var("SLOWCODE_MCP_INTO_CONFIG") };

        let http = RawServer {
            url: Some("https://example.com/mcp".to_owned()),
            kind: Some("sse".to_owned()),
            ..RawServer::default()
        };
        assert!(matches!(
            http.into_config("demo".to_owned(), McpScope::Project)
                .unwrap()
                .transport,
            McpTransport::Http { .. }
        ));

        let unsupported = RawServer {
            url: Some("https://example.com/mcp".to_owned()),
            kind: Some("websocket".to_owned()),
            ..RawServer::default()
        };
        let error = unsupported
            .into_config("demo".to_owned(), McpScope::Project)
            .unwrap_err();
        assert!(
            error.to_string().contains("unsupported transport"),
            "{error}"
        );

        let plaintext = RawServer {
            url: Some("http://example.com/mcp".to_owned()),
            ..RawServer::default()
        };
        assert!(
            plaintext
                .into_config("demo".to_owned(), McpScope::Project)
                .is_err()
        );

        let empty = RawServer::default();
        let error = empty
            .into_config("demo".to_owned(), McpScope::Project)
            .unwrap_err();
        assert!(
            error.to_string().contains("neither command nor url"),
            "{error}"
        );
    }

    #[test]
    fn jsonrpc_frame_is_one_newline_terminated_message() {
        let frame = jsonrpc_frame(&json!({"jsonrpc": "2.0", "id": 1})).unwrap();
        assert!(frame.ends_with(b"\n"));
        assert_eq!(frame.iter().filter(|byte| **byte == b'\n').count(), 1);
        let parsed: Value = serde_json::from_slice(&frame).unwrap();
        assert_eq!(parsed["id"], json!(1));
    }

    #[tokio::test]
    async fn stdio_framing_round_trips_through_a_duplex_stream() {
        let (mut writer, reader) = tokio::io::duplex(1024);
        let mut reader = BufReader::new(reader);
        let payload = json!({"jsonrpc": "2.0", "id": 4, "result": {"ok": true}});
        write_stdio_frame(&mut writer, &payload).await.unwrap();
        let reply = read_stdio_reply(&mut reader, 4).await.unwrap();
        assert_eq!(reply, payload);
    }

    #[tokio::test]
    async fn read_stdio_reply_reports_closed_stdout() {
        let (writer, reader) = tokio::io::duplex(64);
        drop(writer);
        let mut reader = BufReader::new(reader);
        let error = read_stdio_reply(&mut reader, 1).await.unwrap_err();
        assert!(
            error.to_string().contains("server closed stdout"),
            "{error}"
        );
    }

    #[test]
    fn dispatches_http_bodies_by_content_type() {
        let json =
            parse_http_body(r#"{"id":1,"result":{"ok":true}}"#, "application/json", 1).unwrap();
        assert_eq!(json["result"]["ok"], json!(true));

        let sse = parse_http_body(
            "data: {\"jsonrpc\":\"2.0\",\"id\":5,\"result\":{\"tools\":[]}}\n\n",
            "text/event-stream; charset=utf-8",
            5,
        )
        .unwrap();
        assert_eq!(sse["result"]["tools"], json!([]));

        assert!(parse_http_body("not json", "application/json", 1).is_err());
    }

    #[test]
    fn expands_header_values() {
        unsafe { std::env::set_var("SLOWCODE_MCP_HEADER_TOKEN", "abc123") };
        let headers = BTreeMap::from([(
            "Authorization".to_owned(),
            "Bearer ${SLOWCODE_MCP_HEADER_TOKEN}".to_owned(),
        )]);
        let expanded = expanded_headers(&headers).unwrap();
        assert_eq!(
            expanded,
            vec![("Authorization".to_owned(), "Bearer abc123".to_owned())]
        );
        unsafe { std::env::remove_var("SLOWCODE_MCP_HEADER_TOKEN") };
    }

    fn tool_with_schema(server: &str, input_schema: Value) -> McpTool {
        McpTool {
            server: server.to_owned(),
            name: "run".to_owned(),
            description: "run a command".to_owned(),
            input_schema,
            annotations: McpToolAnnotations::default(),
        }
    }

    fn server_config(name: &str, trusted: bool, scope: McpScope) -> McpServerConfig {
        McpServerConfig {
            name: name.to_owned(),
            transport: McpTransport::Stdio {
                command: "npx".to_owned(),
                args: Vec::new(),
                env: BTreeMap::new(),
            },
            trusted,
            enabled: true,
            scope: Some(scope),
            source: None,
        }
    }

    #[test]
    fn tool_registration_marks_only_user_scoped_trusted_servers() {
        let schema = json!({"type": "object"});
        let user = server_config("user", true, McpScope::User);
        let registered =
            register_tool(tool_with_schema("user", schema.clone()), Some(&user)).unwrap();
        assert!(registered.policy.trusted);
        assert_eq!(registered.schema.name(), "mcp__user__run");

        let project = server_config("project", true, McpScope::Project);
        let registered =
            register_tool(tool_with_schema("project", schema.clone()), Some(&project)).unwrap();
        assert!(!registered.policy.trusted);

        let registered = register_tool(tool_with_schema("unconfigured", schema), None).unwrap();
        assert!(!registered.policy.trusted);
    }

    #[test]
    fn tool_registration_rejects_non_object_schemas() {
        let error = register_tool(tool_with_schema("demo", json!(1)), None)
            .err()
            .expect("non-object schema should fail");
        assert_eq!(error.0, "demo");
        assert!(
            error.1.contains("parameters must be an object"),
            "{}",
            error.1
        );
    }

    type RecordedRequests = Arc<std::sync::Mutex<Vec<(String, Option<String>)>>>;

    struct HttpMcpFixture {
        base: String,
        shutdown: tokio::sync::oneshot::Sender<()>,
        task: tokio::task::JoinHandle<()>,
        requests: RecordedRequests,
    }

    impl HttpMcpFixture {
        async fn start() -> Self {
            let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
            let base = format!("http://{}", listener.local_addr().unwrap());
            let (shutdown, mut stop) = tokio::sync::oneshot::channel::<()>();
            let requests = Arc::new(std::sync::Mutex::new(Vec::new()));
            let recorded = Arc::clone(&requests);
            let task = tokio::spawn(async move {
                loop {
                    let accept = tokio::select! {
                        _ = &mut stop => break,
                        accept = listener.accept() => accept,
                    };
                    let Ok((mut stream, _)) = accept else {
                        break;
                    };
                    let Some((headers, body)) = read_http_request(&mut stream).await else {
                        continue;
                    };
                    respond_to_http_mcp(&mut stream, &headers, &body, &recorded).await;
                }
            });
            Self {
                base,
                shutdown,
                task,
                requests,
            }
        }

        fn base(&self) -> String {
            self.base.clone()
        }

        fn requests(&self) -> Vec<(String, Option<String>)> {
            self.requests.lock().unwrap().clone()
        }

        fn stop(self) {
            let _ = self.shutdown.send(());
            self.task.abort();
        }
    }

    async fn read_http_request(stream: &mut tokio::net::TcpStream) -> Option<(String, String)> {
        use tokio::io::AsyncReadExt;

        let mut buffer = Vec::new();
        loop {
            let mut chunk = [0_u8; 1024];
            let read = stream.read(&mut chunk).await.ok()?;
            if read == 0 {
                return None;
            }
            buffer.extend_from_slice(&chunk[..read]);
            let Some(header_end) = buffer.windows(4).position(|window| window == b"\r\n\r\n")
            else {
                continue;
            };
            let headers = String::from_utf8_lossy(&buffer[..header_end]).to_string();
            let length = headers
                .lines()
                .find_map(|line| {
                    let (name, value) = line.split_once(':')?;
                    if name.trim().eq_ignore_ascii_case("content-length") {
                        value.trim().parse::<usize>().ok()
                    } else {
                        None
                    }
                })
                .unwrap_or(0);
            let body_start = header_end + 4;
            while buffer.len() < body_start + length {
                let read = stream.read(&mut chunk).await.ok()?;
                if read == 0 {
                    break;
                }
                buffer.extend_from_slice(&chunk[..read]);
            }
            let body_end = (body_start + length).min(buffer.len());
            let body = String::from_utf8_lossy(&buffer[body_start..body_end]).to_string();
            return Some((headers, body));
        }
    }

    async fn respond_to_http_mcp(
        stream: &mut tokio::net::TcpStream,
        headers: &str,
        body: &str,
        requests: &RecordedRequests,
    ) {
        use tokio::io::AsyncWriteExt;

        let request: Value = serde_json::from_str(body).unwrap_or(Value::Null);
        let method = request
            .get("method")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_owned();
        let session = headers.lines().find_map(|line| {
            let (name, value) = line.split_once(':')?;
            if name.trim().eq_ignore_ascii_case("mcp-session-id") {
                Some(value.trim().to_owned())
            } else {
                None
            }
        });
        requests.lock().unwrap().push((method.clone(), session));
        let id = request.get("id").cloned().unwrap_or(Value::Null);
        let (content_type, session_header, response_body) = match method.as_str() {
            "initialize" => (
                "application/json",
                "mcp-session-id: test-session\r\n",
                json!({
                    "jsonrpc": "2.0",
                    "id": id,
                    "result": {
                        "protocolVersion": "2024-11-05",
                        "capabilities": {},
                        "serverInfo": {"name": "fake", "version": "1"}
                    }
                })
                .to_string(),
            ),
            "tools/list" => (
                "text/event-stream",
                "",
                format!(
                    "event: message\ndata: {{\"jsonrpc\":\"2.0\",\"method\":\"notifications/progress\"}}\n\ndata: {}\n\n",
                    json!({
                        "jsonrpc": "2.0",
                        "id": id,
                        "result": {
                            "tools": [{
                                "name": "echo",
                                "description": "echo text",
                                "inputSchema": {"type": "object"}
                            }]
                        }
                    })
                ),
            ),
            "tools/call" => {
                let text = request
                    .get("params")
                    .and_then(|params| params.get("arguments"))
                    .and_then(|arguments| arguments.get("text"))
                    .and_then(Value::as_str)
                    .unwrap_or_default();
                (
                    "application/json",
                    "",
                    json!({
                        "jsonrpc": "2.0",
                        "id": id,
                        "result": {"content": [{"type": "text", "text": text}]}
                    })
                    .to_string(),
                )
            }
            _ => ("application/json", "", "{}".to_owned()),
        };
        let response = format!(
            "HTTP/1.1 200 OK\r\ncontent-type: {content_type}\r\n{session_header}content-length: {}\r\nconnection: close\r\n\r\n{response_body}",
            response_body.len()
        );
        let _ = stream.write_all(response.as_bytes()).await;
        let _ = stream.shutdown().await;
    }

    #[tokio::test]
    async fn http_session_captures_session_id_and_parses_sse_tools() {
        let fixture = HttpMcpFixture::start().await;
        let server = McpServerConfig {
            name: "remote".to_owned(),
            transport: McpTransport::Http {
                url: format!("{}/mcp", fixture.base()),
                headers: BTreeMap::new(),
            },
            trusted: false,
            enabled: true,
            scope: Some(McpScope::Project),
            source: None,
        };
        let mut session = McpSession::connect(&server).await.unwrap();
        assert_eq!(session.tools().len(), 1);
        assert_eq!(
            session
                .call("echo", json!({"text": "hello"}))
                .await
                .unwrap(),
            "hello"
        );

        let requests = fixture.requests();
        let methods = requests
            .iter()
            .map(|(method, _)| method.as_str())
            .collect::<Vec<_>>();
        assert_eq!(
            methods,
            [
                "initialize",
                "notifications/initialized",
                "tools/list",
                "tools/call"
            ]
        );
        assert_eq!(requests[0].1, None);
        assert_eq!(requests[1].1.as_deref(), Some("test-session"));
        assert_eq!(requests[2].1.as_deref(), Some("test-session"));
        assert_eq!(requests[3].1.as_deref(), Some("test-session"));
        fixture.stop();
    }
}
