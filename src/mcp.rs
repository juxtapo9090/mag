use crate::engine::{BatchEngine, execute_tool_request};
use rmcp::{
    ErrorData as McpError, ServerHandler,
    handler::server::{router::tool::ToolRouter, wrapper::Parameters},
    model::{CallToolResult, Content, ServerCapabilities, ServerInfo},
    schemars, tool, tool_handler, tool_router,
};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;

/// How the stdio tool actually executes: private in-process engine (classic
/// aerondight behaviour, for debugging) or forward to the resident daemon
/// (the default — one shared pool for the whole house).
#[derive(Clone)]
pub enum ToolBackend {
    Local(BatchEngine),
    Forward { seat: String },
}

#[derive(Debug, Clone, Deserialize, Serialize, schemars::JsonSchema)]
pub struct ToolRequest {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub terminal: Option<TerminalRequest>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cmd: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cmds: Option<Vec<String>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cwd: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub script: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub raw: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub remote: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub session_id: Option<String>,
    /// Shorthand for "keep my shell between calls" without naming it — resolves
    /// to a per-seat default `session_id`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub sticky: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub close: Option<bool>,
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub env: BTreeMap<String, String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub default_cwd: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub default_timeout_ms: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_parallel: Option<usize>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub continue_on_error: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub capture_limit_bytes: Option<usize>,
    #[serde(
        default,
        alias = "g0",
        alias = "g_0",
        alias = "g-0",
        alias = "ground_zero",
        skip_serializing_if = "Option::is_none"
    )]
    pub unfiltered: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub warm: Option<bool>,
    /// Ask which warm sessions this seat is holding, instead of running
    /// anything. Optionally narrowed by `session_id`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub sessions: Option<bool>,
}

#[derive(Debug, Clone, Deserialize, Serialize, schemars::JsonSchema)]
pub struct TerminalRequest {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub action: Option<TerminalAction>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub seat: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub session: Option<String>,
    /// Explicit tmux socket path (e.g. `/tmp/tmux-1000/default`). Overrides
    /// server auto-detection for this call — use it when the pane lives on a
    /// specific uid's server and you don't want the probe guessing.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub socket: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cwd: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub input: Option<String>,
    #[serde(default)]
    pub enter: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub lines: Option<usize>,
    #[serde(default)]
    pub no_wezterm: Option<bool>,
    #[serde(default)]
    pub no_watch: Option<bool>,
}

#[derive(Debug, Clone, Deserialize, Serialize, schemars::JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum TerminalAction {
    Open,
    Status,
    Send,
    Snapshot,
    Close,
}

#[derive(Clone)]
pub struct MagServer {
    backend: ToolBackend,
    // Read by the rmcp `tool_handler` macro when dispatching; the dead-code
    // lint can't see through the macro, same as in aerondight's bin target.
    #[allow(dead_code)]
    tool_router: ToolRouter<Self>,
}

#[tool_router]
impl MagServer {
    pub fn new(backend: ToolBackend) -> Self {
        Self {
            backend,
            tool_router: Self::tool_router(),
        }
    }

    #[tool(
        description = "Shell runner (resident warm pool). cmd/cmds simple lane; raw/script power lane; terminal branch controls shared tmux/WezTerm terminal. Manual: read /etc/mag/mag-how-to.md or run `mag --how`."
    )]
    async fn mag(
        &self,
        Parameters(request): Parameters<ToolRequest>,
    ) -> Result<CallToolResult, McpError> {
        let backend = self.backend.clone();
        let payload = tokio::task::spawn_blocking(move || match backend {
            ToolBackend::Local(engine) => execute_tool_request(engine, request),
            ToolBackend::Forward { seat } => crate::client::forward_tool_request(&request, &seat),
        })
        .await
        .map_err(|err| McpError::internal_error(err.to_string(), None))?;
        let text = payload.map_err(|err| McpError::invalid_params(err.to_string(), None))?;
        Ok(CallToolResult::success(vec![Content::text(text)]))
    }
}

#[tool_handler]
impl ServerHandler for MagServer {
    fn get_info(&self) -> ServerInfo {
        ServerInfo::new(ServerCapabilities::builder().enable_tools().build()).with_instructions(
            "mag — resident warm-pool shell runner. Manual: /etc/mag/mag-how-to.md".to_string(),
        )
    }
}
