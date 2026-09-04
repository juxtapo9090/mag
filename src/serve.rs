use crate::audit::{AuditRuntimeConfig, ServeAuditConfig};
use crate::cli::{BatchRequest, BatchResponse, Cli, CommandSpec};
use crate::config::load_serve_runtime;
use crate::engine::{BatchEngine, execute_tool_request};
use crate::mcp::ToolRequest;
use crate::toon::ToonRequest;
use crate::{AppResult, SERVER_NAME, SERVER_VERSION};
use anyhow::{Context, anyhow};
use axum::{
    Extension, Json, Router,
    extract::{Path, State},
    http::{HeaderMap, StatusCode},
    routing::{get, post},
};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::net::{IpAddr, SocketAddr};
use std::os::fd::FromRawFd;
use std::sync::Arc;
use std::sync::Mutex;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

#[derive(Debug, Clone, Deserialize)]
pub struct ServeFileConfig {
    pub(crate) node: ServeNodeConfig,
    pub(crate) serve: ServeListenConfig,
    #[serde(default)]
    pub(crate) execution: ServeExecutionConfig,
    #[serde(default)]
    pub(crate) callback: ServeCallbackConfig,
    #[serde(default)]
    pub(crate) session: ServeSessionConfig,
    #[serde(default)]
    pub(crate) audit: ServeAuditConfig,
}

#[derive(Debug, Clone, Deserialize)]
pub(crate) struct ServeNodeConfig {
    pub(crate) name: String,
    pub(crate) id: String,
    pub(crate) region: String,
    pub(crate) role: String,
}

#[derive(Debug, Clone, Deserialize)]
pub(crate) struct ServeListenConfig {
    pub(crate) bind: String,
    pub(crate) port: u16,
    #[serde(default)]
    pub(crate) workers: Option<usize>,
    #[serde(default)]
    pub(crate) auth_token: String,
}

#[derive(Debug, Clone, Deserialize, Default)]
pub(crate) struct ServeExecutionConfig {
    #[serde(default)]
    pub(crate) default_timeout_ms: Option<u64>,
    #[serde(default)]
    pub(crate) max_capture_bytes: Option<usize>,
    #[serde(default)]
    pub(crate) default_cwd: Option<String>,
    #[serde(default)]
    pub(crate) allowed_command_prefixes: Vec<String>,
    #[serde(default)]
    pub(crate) allowed_argv: Vec<String>,
    /// Token-diet master toggle (default on). Off = raw passthrough, old behaviour.
    #[serde(default = "default_token_diet_enabled")]
    pub(crate) token_diet_enabled: bool,
    /// Hard echo cap per stream after the filter pass (default 8 KiB).
    #[serde(default)]
    pub(crate) token_diet_echo_max_bytes: Option<usize>,
}

fn default_token_diet_enabled() -> bool {
    true
}

/// Session lifecycle knobs — the reaper's tuning (brief: CONFIG, not hardcode).
#[derive(Debug, Clone, Deserialize)]
pub(crate) struct ServeSessionConfig {
    #[serde(default = "default_session_idle_timeout_ms")]
    pub(crate) idle_timeout_ms: u64,
    #[serde(default = "default_session_reap_interval_ms")]
    pub(crate) reap_interval_ms: u64,
    #[serde(default)]
    pub(crate) max_named: Option<usize>,
}

impl Default for ServeSessionConfig {
    fn default() -> Self {
        Self {
            idle_timeout_ms: default_session_idle_timeout_ms(),
            reap_interval_ms: default_session_reap_interval_ms(),
            max_named: None,
        }
    }
}

fn default_session_idle_timeout_ms() -> u64 {
    300_000 // 5 min, per brief
}

fn default_session_reap_interval_ms() -> u64 {
    30_000
}

#[derive(Debug, Clone, Deserialize, Default)]
pub(crate) struct ServeCallbackConfig {
    #[serde(default)]
    pub(crate) notify_url: String,
    #[serde(default = "default_callback_notify_on")]
    pub(crate) notify_on: String,
    #[serde(default)]
    pub(crate) auth_token: String,
}

#[derive(Debug, Clone)]
pub struct ServeRuntimeConfig {
    pub node_name: String,
    pub node_id: String,
    pub node_region: String,
    pub node_role: String,
    pub bind_ip: IpAddr,
    pub port: u16,
    pub workers: usize,
    pub auth_token: Option<String>,
    pub default_timeout_ms: Option<u64>,
    pub max_capture_bytes: Option<usize>,
    pub default_cwd: Option<String>,
    pub allowed_command_prefixes: Vec<String>,
    pub allowed_argv: Vec<String>,
    pub callback_notify_url: Option<String>,
    pub callback_notify_on: CallbackNotifyOn,
    pub callback_auth_token: Option<String>,
    pub session_idle_timeout_ms: u64,
    pub session_reap_interval_ms: u64,
    pub session_max_named: Option<usize>,
    pub audit: AuditRuntimeConfig,
    pub token_diet_enabled: bool,
    pub token_diet_echo_max_bytes: usize,
}

#[derive(Clone)]
struct ServeAppState {
    engine: BatchEngine,
    config: ServeRuntimeConfig,
    active_requests: Arc<AtomicUsize>,
    async_jobs: Arc<Mutex<BTreeMap<String, AsyncJobState>>>,
}

#[derive(Debug, Serialize)]
struct ServeHealthResponse {
    ok: bool,
    server: &'static str,
    mode: &'static str,
    node: String,
    node_id: String,
    region: String,
    role: String,
    bind: String,
    workers: usize,
    active_requests: usize,
    async_jobs: usize,
    callback_enabled: bool,
    auth_required: bool,
    allowed_command_prefixes: usize,
    allowed_argv: usize,
    /// The uid commands actually run as here, and whether it can escalate.
    /// Published because the process that *renders* a refusal is usually not
    /// the process that *earned* it — see `hint::ExecIdentity`.
    exec_user: String,
    exec_can_escalate: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CallbackNotifyOn {
    Completed,
    Failed,
    Both,
}

#[derive(Debug, Clone)]
enum AsyncJobState {
    Queued,
    Running,
    Completed(BatchResponse),
    Failed(String),
}

#[derive(Debug, Serialize)]
struct ServeAsyncAcceptedResponse {
    ok: bool,
    job_id: String,
    status: &'static str,
    node: String,
}

#[derive(Debug, Serialize)]
struct ServeJobResponse {
    ok: bool,
    job_id: String,
    node: String,
    status: &'static str,
    finished: bool,
    result: Option<BatchResponse>,
    error: Option<String>,
}

#[derive(Debug, Clone, Serialize)]
struct ServeCallbackPayload {
    ok: bool,
    job_id: String,
    node: String,
    node_id: String,
    region: String,
    role: String,
    status: &'static str,
    finished: bool,
    result: Option<BatchResponse>,
    error: Option<String>,
}

struct ActiveRequestGuard {
    active_requests: Arc<AtomicUsize>,
}

impl Drop for ActiveRequestGuard {
    fn drop(&mut self) {
        self.active_requests.fetch_sub(1, Ordering::Relaxed);
    }
}

/// Content-based dispatch, not untagged ordering: old forward clients send
/// explicit `null`s for every unset field (`{"cmd": ..., "script": null, …}`),
/// which defeats `deny_unknown_fields` discrimination. Look at the keys that
/// are actually present and non-null — that signal survives every client.
#[derive(Debug, Clone)]
enum ServeRequestPayload {
    Batch(BatchRequest),
    Simple(SimplePayload),
    Toon(ToonRequest),
    Tool(ToolRequest),
    Sessions(SessionsPayload),
}

/// `{"sessions": true}` — a question about warm shells, not a command to run.
#[derive(Debug, Clone, Deserialize)]
struct SessionsPayload {
    #[serde(default)]
    session_id: Option<String>,
}

impl<'de> Deserialize<'de> for ServeRequestPayload {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        let value = serde_json::Value::deserialize(deserializer)?;
        let has_terminal = value
            .as_object()
            .and_then(|obj| obj.get("terminal"))
            .is_some_and(|v| !v.is_null());
        if has_terminal {
            let request: ToolRequest =
                serde_json::from_value(value).map_err(serde::de::Error::custom)?;
            return Ok(ServeRequestPayload::Tool(request));
        }

        let asks_sessions = value
            .as_object()
            .and_then(|obj| obj.get("sessions"))
            .and_then(serde_json::Value::as_bool)
            .unwrap_or(false);
        if asks_sessions {
            let request: SessionsPayload =
                serde_json::from_value(value).map_err(serde::de::Error::custom)?;
            return Ok(ServeRequestPayload::Sessions(request));
        }

        let is_simple = value
            .as_object()
            .map(|obj| {
                let has_cmd = obj.get("cmd").is_some_and(|v| !v.is_null())
                    || obj.get("cmds").is_some_and(|v| !v.is_null());
                let has_power = obj.get("script").is_some_and(|v| !v.is_null())
                    || obj.get("raw").is_some_and(|v| !v.is_null())
                    || obj.get("commands").is_some_and(|v| !v.is_null());
                has_cmd && !has_power
            })
            .unwrap_or(false);

        if is_simple {
            let simple: SimplePayload =
                serde_json::from_value(value).map_err(serde::de::Error::custom)?;
            return Ok(ServeRequestPayload::Simple(simple));
        }

        #[derive(Deserialize)]
        #[serde(untagged)]
        enum BatchOrToon {
            Batch(BatchRequest),
            Toon(ToonRequest),
        }
        match serde_json::from_value::<BatchOrToon>(value).map_err(serde::de::Error::custom)? {
            BatchOrToon::Batch(batch) => Ok(ServeRequestPayload::Batch(batch)),
            BatchOrToon::Toon(toon) => Ok(ServeRequestPayload::Toon(toon)),
        }
    }
}

#[derive(Debug, Clone, Deserialize)]
struct SimplePayload {
    #[serde(default)]
    cmd: Option<String>,
    #[serde(default)]
    cmds: Option<Vec<String>>,
    #[serde(default)]
    cwd: Option<String>,
    /// Pins this call to a warm shell, exactly as on the power lane. Before
    /// this field existed the value was accepted by the wire format and then
    /// dropped on the floor — every simple-lane call got a fresh shell while
    /// looking like it had continuity.
    #[serde(default)]
    session_id: Option<String>,
    #[serde(default)]
    sticky: Option<bool>,
    #[serde(default)]
    close: Option<bool>,
    #[serde(default)]
    default_timeout_ms: Option<u64>,
    #[serde(default)]
    max_parallel: Option<usize>,
    #[serde(default)]
    continue_on_error: Option<bool>,
    #[serde(default)]
    capture_limit_bytes: Option<usize>,
    #[serde(
        default,
        alias = "g0",
        alias = "g_0",
        alias = "g-0",
        alias = "ground_zero"
    )]
    unfiltered: Option<bool>,
}

impl SimplePayload {
    /// Which warm shell this call belongs to, if any. `sticky: true` is the
    /// no-bookkeeping form — sessions are already namespaced `<seat>:<id>`, so
    /// a single shared id is per-seat private without the caller inventing and
    /// tracking name strings.
    fn resolved_session(&self) -> Option<String> {
        self.session_id
            .as_deref()
            .map(str::trim)
            .filter(|value| !value.is_empty())
            .map(ToString::to_string)
            .or_else(|| {
                self.sticky
                    .unwrap_or(false)
                    .then(|| crate::DEFAULT_STICKY_SESSION.to_string())
            })
    }

    fn close_requested(&self) -> bool {
        self.close.unwrap_or(false)
    }

    fn into_batch(self) -> AppResult<BatchRequest> {
        let commands: Vec<String> = match (self.cmd, self.cmds) {
            (Some(command), None) => vec![command],
            (None, Some(commands)) if !commands.is_empty() => commands,
            (None, Some(_)) => return Err(anyhow!("`cmds` must not be empty")),
            (Some(_), Some(_)) => {
                return Err(anyhow!("mag accepts either `cmd` or `cmds`, not both"));
            }
            (None, None) => return Err(anyhow!("mag requires `cmd` or `cmds`")),
        };
        if commands.len() > crate::MAX_COMMANDS {
            return Err(anyhow!("`cmds` exceeds max of {}", crate::MAX_COMMANDS));
        }
        if commands.iter().any(|c| c.trim().is_empty()) {
            return Err(anyhow!("`cmd`/`cmds` entries must not be blank"));
        }
        Ok(BatchRequest {
            commands: commands
                .into_iter()
                .enumerate()
                .map(|(index, command)| crate::cli::CommandSpec {
                    id: Some(format!("cmd-{}", index + 1)),
                    command: Some(command),
                    argv: None,
                    remote: None,
                    cwd: None,
                    env: BTreeMap::new(),
                    stdin: None,
                    timeout_ms: None,
                    warm: Some(true),
                })
                .collect(),
            env: BTreeMap::new(),
            remote: None,
            default_cwd: self.cwd.filter(|value| !value.trim().is_empty()),
            default_timeout_ms: self.default_timeout_ms,
            max_parallel: self.max_parallel.or(Some(1)),
            continue_on_error: self.continue_on_error.or(Some(true)),
            capture_limit_bytes: self.capture_limit_bytes,
            unfiltered: self.unfiltered,
        })
    }
}

fn default_callback_notify_on() -> String {
    "completed".to_string()
}

pub(crate) fn validate_serve_batch(
    batch: &BatchRequest,
    serve: &ServeRuntimeConfig,
) -> AppResult<()> {
    for spec in &batch.commands {
        validate_serve_command(spec, serve)?;
    }
    Ok(())
}

fn validate_serve_command(spec: &CommandSpec, serve: &ServeRuntimeConfig) -> AppResult<()> {
    // Empty allowlist = open lane (trusted local appliance, per brief:
    // "don't blanket-lock the local box"). Non-empty = enforced.
    if let Some(command) = spec.command.as_deref() {
        if !serve.allowed_command_prefixes.is_empty() {
            let trimmed = command.trim_start();
            if !serve
                .allowed_command_prefixes
                .iter()
                .any(|prefix| trimmed.starts_with(prefix))
            {
                return Err(anyhow!(
                    "serve mode rejected command `{}`; not in allowlist",
                    command_preview(trimmed)
                ));
            }
        }
    }

    if let Some(argv) = spec.argv.as_ref() {
        if !serve.allowed_argv.is_empty() {
            let first = argv
                .first()
                .ok_or_else(|| anyhow!("serve mode rejected empty `argv`"))?;
            if !serve.allowed_argv.iter().any(|allowed| allowed == first) {
                return Err(anyhow!(
                    "serve mode rejected argv `{}`; not in allowlist",
                    first
                ));
            }
        }
    }

    Ok(())
}

fn command_preview(input: &str) -> String {
    let compact = input.split_whitespace().collect::<Vec<_>>().join(" ");
    if compact.len() <= 72 {
        compact
    } else {
        format!("{}...", &compact[..72])
    }
}

fn request_is_authorized(headers: &HeaderMap, serve: &ServeRuntimeConfig) -> bool {
    let Some(token) = serve.auth_token.as_deref() else {
        return true;
    };

    let auth_matches = headers
        .get("authorization")
        .and_then(|value| value.to_str().ok())
        .map(|value| value.trim() == format!("Bearer {}", token))
        .unwrap_or(false);
    if auth_matches {
        return true;
    }

    headers
        .get("x-mag-token")
        .and_then(|value| value.to_str().ok())
        .map(|value| value.trim() == token)
        .unwrap_or(false)
}

fn execute_serve_payload(
    engine: BatchEngine,
    serve: ServeRuntimeConfig,
    seat: String,
    request: ServeRequestPayload,
) -> AppResult<BatchResponse> {
    let exec_user = crate::hint::exec_identity().user;
    let mut response = match request {
        ServeRequestPayload::Toon(request) => engine.run_served_request(&seat, request, &serve),
        ServeRequestPayload::Batch(batch) => engine.run_served_batch_as(&seat, batch, &serve),
        ServeRequestPayload::Simple(simple) => {
            let session = simple.resolved_session();
            let close = simple.close_requested();
            let batch = simple.into_batch()?;
            let Some(session) = session else {
                return engine.run_served_batch_as(&seat, batch, &serve);
            };
            engine.check_named_session_limit(&seat, &session, &serve)?;
            let response = engine.run_served_named_batch_as(&seat, &session, batch, &serve)?;
            if close {
                engine.release_named_session(&seat, &session)?;
            }
            Ok(response)
        }
        ServeRequestPayload::Sessions(request) => {
            let statuses = engine.session_statuses(&seat);
            let text =
                crate::render::render_session_statuses(&statuses, request.session_id.as_deref());
            Ok(single_text_response("sessions", text))
        }
        ServeRequestPayload::Tool(request) => {
            let output = execute_tool_request(engine, request)?;
            Ok(single_text_response("terminal", output))
        }
    }?;
    // Stamp the executing user onto every result so it shows in the success
    // header, not only in refusal hints. The identity is a property of the
    // daemon we're bound to (or this process for direct CLI paths).
    for result in &mut response.results {
        result.exec_user = Some(exec_user.clone());
    }
    Ok(response)
}

/// Wrap a plain answer (terminal output, session status) in the batch shape the
/// wire format expects, so non-command replies travel the same path as results.
fn single_text_response(kind: &str, text: String) -> BatchResponse {
    BatchResponse {
        command_count: 1,
        elapsed_ms: 0,
        stopped_early: false,
        results: vec![crate::cli::CommandResult {
            index: 0,
            id: Some(kind.to_string()),
            mode: kind.to_string(),
            success: true,
            exit_code: Some(0),
            timed_out: false,
            duration_ms: 0,
            stdout: Some(text),
            stderr: None,
            stdout_truncated: false,
            stderr_truncated: false,
            diet_applied: false,
            exec_user: None,
            error: None,
        }],
    }
}

/// The seat a connection belongs to, stamped onto the request extension by the
/// per-listener middleware layer. Identity comes from **the socket the connection
/// arrived on** (systemd `FileDescriptorName=` = seat id), never from a header the
/// caller writes about itself.
///
/// Honest limit: socket perms separate root from juxtapo, not two root seats. This
/// is correct-by-construction *attribution*, not enforcement. A root seat can still
/// reach another root seat's socket by path — expected, documented, bounded later by
/// the per-seat in-flight cap (Phase 2).
#[derive(Clone, Debug)]
struct SeatTag(String);

impl SeatTag {
    fn as_str(&self) -> &str {
        &self.0
    }
}

/// Take ownership of systemd-passed listen fds and map each to its `FileDescriptorName`
/// (the seat id). Returns `(seat, UnixListener)` pairs.
///
/// systemd protocol: `LISTEN_FDS` = count, fds start at `SD_LISTEN_FDS_START` (3),
/// `LISTEN_FDNAMES` = colon-separated names in fd order. We unset both env vars after
/// taking ownership so any child process we spawn doesn't inherit the magic fds.
///
/// Returns `Ok(vec![])` when `LISTEN_FDS` is unset — the caller falls back to the TCP
/// path (dev/test instance on `:9925`). An empty `LISTEN_FDNAMES` with a nonzero count,
/// or a name/fd count mismatch, is a hard error: silence there would mean every
/// connection on that listener gets attributed to `anon`, which is the exact defect
/// this redesign exists to kill.
fn take_systemd_seat_listeners() -> anyhow::Result<Vec<(String, tokio::net::UnixListener)>> {
    let Some(count_str) = std::env::var_os("LISTEN_FDS") else {
        return Ok(Vec::new());
    };
    let count: usize = count_str
        .to_string_lossy()
        .parse()
        .context("LISTEN_FDS is not a number")?;
    if count == 0 {
        return Ok(Vec::new());
    }

    let names_str = std::env::var_os("LISTEN_FDNAMES")
        .context("LISTEN_FDS is set but LISTEN_FDNAMES is missing — cannot map fds to seats")?
        .to_string_lossy()
        .into_owned();
    let names: Vec<&str> = names_str.split(':').collect();
    if names.len() != count {
        return Err(anyhow!(
            "LISTEN_FDS ({}) and LISTEN_FDNAMES ({}) disagree on fd count — \
             refusing to attribute connections to the wrong seat",
            count,
            names.len()
        ));
    }

    // Clear the magic env vars so spawned children don't re-inherit them.
    // `set_var`/`remove_var` are `unsafe` in edition 2024 (they're not thread-safe
    // against concurrent reads); mag is single-threaded at startup, so this is sound.
    unsafe {
        std::env::remove_var("LISTEN_FDS");
        std::env::remove_var("LISTEN_FDNAMES");
        std::env::remove_var("LISTEN_PID");
    }

    const SD_LISTEN_FDS_START: std::os::fd::RawFd = 3;
    let mut listeners = Vec::with_capacity(count);
    for (i, name) in names.into_iter().enumerate() {
        if name.is_empty() {
            return Err(anyhow!(
                "LISTEN_FDNAMES entry {} is empty — every listener needs a seat name",
                i
            ));
        }
        let raw = SD_LISTEN_FDS_START + i as std::os::fd::RawFd;
        // Take ownership of the fd. CLOEXEC is NOT set by systemd's bare
        // LISTEN_FDS protocol (only by sd_listen_fds() with the C macro + flag);
        // our hand-rolled parse inherits fds plain. Without CLOEXEC every warm
        // shell in the pool inherits all of its instance's listening fds — a leak
        // AND a shell holding an accept()-capable fd on a lane it must never see
        // (celeste's verification, 2026-08-05). Set it explicitly before the pool
        // ever spawns.
        let owned = unsafe { std::os::fd::OwnedFd::from_raw_fd(raw) };
        let std_listener: std::os::unix::net::UnixListener = owned.into();
        // FD_CLOEXEC on the listener fd so exec'd warm shells don't inherit it.
        // libc is already in the dep tree (tokio); a direct fcntl avoids a new dep.
        set_fd_cloexec(&std_listener)?;
        std_listener.set_nonblocking(true)?;
        let tokio_listener = tokio::net::UnixListener::from_std(std_listener)?;
        listeners.push((name.to_string(), tokio_listener));
    }
    Ok(listeners)
}

/// Set `FD_CLOEXEC` on a fd so it isn't inherited across `exec` — specifically so
/// mag's warm-shell pool (spawned bash processes) does not hold the listening seat
/// sockets. See `take_systemd_seat_listeners` for why this matters.
fn set_fd_cloexec<F: std::os::fd::AsRawFd>(fd: &F) -> anyhow::Result<()> {
    const FD_CLOEXEC: i32 = 1;
    const F_GETFD: i32 = 1;
    const F_SETFD: i32 = 2;
    // SAFETY: fcntl(fd, F_GETFD, 0) / fcntl(fd, F_SETFD, flags) are safe given a
    // valid fd; the only mutation is the close-on-exec flag.
    let raw = fd.as_raw_fd();
    let cur = unsafe { libc::fcntl(raw, F_GETFD) };
    if cur < 0 {
        return Err(std::io::Error::last_os_error()).context("fcntl(F_GETFD) on seat socket");
    }
    if unsafe { libc::fcntl(raw, F_SETFD, cur | FD_CLOEXEC) } < 0 {
        return Err(std::io::Error::last_os_error())
            .context("fcntl(F_SETFD, CLOEXEC) on seat socket");
    }
    Ok(())
}

/// Seat identity for session namespacing on the shared daemon.
///
/// **Phase 1 (per-seat sockets):** seat comes from the `SeatTag` stamped on the
/// request extension by the per-listener `Extension` layer — i.e. the socket the
/// connection arrived on. The `x-mag-seat` header and `CTX_CALLER` are **no longer
/// inputs**; they cannot be, because every root seat inherited `CTX_CALLER=celeste`
/// from one user-scope entry, which misattributed 345 audit rows and let two root
/// seats share one named shell.
///
/// Handlers read `Extension<SeatTag>` directly. `seat_from_headers` stays only for
/// the TCP dev/test fallback where no socket identity exists.

/// TCP dev/test fallback only — the production path is `Extension<SeatTag>`.
fn seat_from_headers(headers: &HeaderMap) -> String {
    headers
        .get("x-mag-seat")
        .and_then(|value| value.to_str().ok())
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(ToOwned::to_owned)
        .or_else(|| {
            std::env::var("CTX_CALLER")
                .ok()
                .filter(|value| !value.trim().is_empty())
        })
        .unwrap_or_else(|| "anon".to_string())
}

fn make_job_id() -> String {
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|value| value.as_nanos())
        .unwrap_or_default();
    format!("job_{}", nanos)
}

fn async_jobs_count(async_jobs: &Arc<Mutex<BTreeMap<String, AsyncJobState>>>) -> usize {
    async_jobs.lock().map(|guard| guard.len()).unwrap_or(0)
}

fn should_notify_callback(serve: &ServeRuntimeConfig, status: &'static str) -> bool {
    if serve.callback_notify_url.is_none() {
        return false;
    }

    matches!(
        (serve.callback_notify_on, status),
        (CallbackNotifyOn::Completed, "completed")
            | (CallbackNotifyOn::Failed, "failed")
            | (CallbackNotifyOn::Both, "completed" | "failed")
    )
}

fn send_callback_notification(
    serve: &ServeRuntimeConfig,
    payload: &ServeCallbackPayload,
) -> AppResult<()> {
    let Some(notify_url) = serve.callback_notify_url.as_deref() else {
        return Ok(());
    };

    let client = reqwest::blocking::Client::builder()
        .timeout(Duration::from_secs(10))
        .build()?;
    let mut request = client
        .post(notify_url)
        .header("content-type", "application/json")
        .header("x-mag-node", serve.node_name.as_str())
        .json(payload);
    if let Some(token) = serve.callback_auth_token.as_deref() {
        request = request.header("x-mag-inbox-token", token);
    }

    let response = request.send()?;
    if !response.status().is_success() {
        return Err(anyhow!(
            "callback notify failed with HTTP {}",
            response.status().as_u16()
        ));
    }

    Ok(())
}

fn update_async_job(
    async_jobs: &Arc<Mutex<BTreeMap<String, AsyncJobState>>>,
    job_id: &str,
    state: AsyncJobState,
) {
    if let Ok(mut guard) = async_jobs.lock() {
        guard.insert(job_id.to_string(), state);
    }
}

fn snapshot_async_job(
    async_jobs: &Arc<Mutex<BTreeMap<String, AsyncJobState>>>,
    job_id: &str,
) -> Option<AsyncJobState> {
    async_jobs
        .lock()
        .ok()
        .and_then(|guard| guard.get(job_id).cloned())
}

fn unauthorized_json() -> (StatusCode, Json<serde_json::Value>) {
    (
        StatusCode::UNAUTHORIZED,
        Json(serde_json::json!({
            "error": "unauthorized"
        })),
    )
}

async fn serve_health(State(state): State<ServeAppState>) -> Json<ServeHealthResponse> {
    // Computed here, in the process that actually runs the commands.
    let identity = crate::hint::local_identity();
    Json(ServeHealthResponse {
        ok: true,
        server: SERVER_NAME,
        mode: "serve",
        node: state.config.node_name.clone(),
        node_id: state.config.node_id.clone(),
        region: state.config.node_region.clone(),
        role: state.config.node_role.clone(),
        bind: format!("{}:{}", state.config.bind_ip, state.config.port),
        workers: state.config.workers,
        active_requests: state.active_requests.load(Ordering::Relaxed),
        async_jobs: async_jobs_count(&state.async_jobs),
        callback_enabled: state.config.callback_notify_url.is_some(),
        auth_required: state.config.auth_token.is_some(),
        allowed_command_prefixes: state.config.allowed_command_prefixes.len(),
        allowed_argv: state.config.allowed_argv.len(),
        exec_user: identity.user,
        exec_can_escalate: identity.can_escalate,
    })
}

async fn serve_toon(
    State(state): State<ServeAppState>,
    headers: HeaderMap,
    Extension(seat_tag): Extension<SeatTag>,
    Json(request): Json<ServeRequestPayload>,
) -> (StatusCode, Json<serde_json::Value>) {
    if !request_is_authorized(&headers, &state.config) {
        tracing::info!("[{}] POST /toon 401 unauthorized", state.config.node_name);
        return unauthorized_json();
    }

    state.active_requests.fetch_add(1, Ordering::Relaxed);
    let _guard = ActiveRequestGuard {
        active_requests: Arc::clone(&state.active_requests),
    };

    let engine = state.engine.clone();
    let serve = state.config.clone();
    let seat = seat_tag.as_str().to_owned();
    match tokio::task::spawn_blocking(move || execute_serve_payload(engine, serve, seat, request))
        .await
    {
        Ok(Ok(response)) => {
            tracing::info!("[{}] POST /toon 200", state.config.node_name);
            (StatusCode::OK, Json(serde_json::json!(response)))
        }
        Ok(Err(err)) => {
            tracing::info!("[{}] POST /toon 400 {}", state.config.node_name, err);
            (
                StatusCode::BAD_REQUEST,
                Json(serde_json::json!({
                    "error": err.to_string()
                })),
            )
        }
        Err(err) => {
            tracing::error!(
                "[{}] POST /toon 500 serve worker join failed: {}",
                state.config.node_name,
                err
            );
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(serde_json::json!({
                    "error": format!("serve worker join failed: {}", err)
                })),
            )
        }
    }
}

async fn serve_toon_async(
    State(state): State<ServeAppState>,
    headers: HeaderMap,
    Extension(seat_tag): Extension<SeatTag>,
    Json(request): Json<ServeRequestPayload>,
) -> (StatusCode, Json<serde_json::Value>) {
    if !request_is_authorized(&headers, &state.config) {
        tracing::info!(
            "[{}] POST /toon/async 401 unauthorized",
            state.config.node_name
        );
        return unauthorized_json();
    }

    let job_id = make_job_id();
    update_async_job(&state.async_jobs, &job_id, AsyncJobState::Queued);

    let engine = state.engine.clone();
    let serve = state.config.clone();
    let seat = seat_tag.as_str().to_owned();
    let async_jobs = Arc::clone(&state.async_jobs);
    let active_requests = Arc::clone(&state.active_requests);
    let node_name = state.config.node_name.clone();
    let spawned_job_id = job_id.clone();
    let callback_serve = state.config.clone();

    tokio::spawn(async move {
        update_async_job(&async_jobs, &spawned_job_id, AsyncJobState::Running);
        active_requests.fetch_add(1, Ordering::Relaxed);
        let _guard = ActiveRequestGuard { active_requests };

        match tokio::task::spawn_blocking(move || {
            execute_serve_payload(engine, serve, seat, request)
        })
        .await
        {
            Ok(Ok(response)) => {
                tracing::info!(
                    "[{}] POST /toon/async 200 job={}",
                    node_name,
                    spawned_job_id
                );
                let callback_payload = ServeCallbackPayload {
                    ok: true,
                    job_id: spawned_job_id.clone(),
                    node: callback_serve.node_name.clone(),
                    node_id: callback_serve.node_id.clone(),
                    region: callback_serve.node_region.clone(),
                    role: callback_serve.node_role.clone(),
                    status: "completed",
                    finished: true,
                    result: Some(response.clone()),
                    error: None,
                };
                update_async_job(
                    &async_jobs,
                    &spawned_job_id,
                    AsyncJobState::Completed(response),
                );
                if should_notify_callback(&callback_serve, "completed") {
                    let notify_serve = callback_serve.clone();
                    let notify_job_id = spawned_job_id.clone();
                    match tokio::task::spawn_blocking(move || {
                        send_callback_notification(&notify_serve, &callback_payload)
                    })
                    .await
                    {
                        Ok(Ok(())) => {}
                        Ok(Err(err)) => tracing::error!(
                            "[{}] callback failed for job={}: {}",
                            callback_serve.node_name,
                            notify_job_id,
                            err
                        ),
                        Err(err) => tracing::error!(
                            "[{}] callback join failed for job={}: {}",
                            callback_serve.node_name,
                            notify_job_id,
                            err
                        ),
                    }
                }
            }
            Ok(Err(err)) => {
                tracing::info!(
                    "[{}] POST /toon/async 400 job={} {}",
                    node_name,
                    spawned_job_id,
                    err
                );
                let error_text = err.to_string();
                let callback_payload = ServeCallbackPayload {
                    ok: false,
                    job_id: spawned_job_id.clone(),
                    node: callback_serve.node_name.clone(),
                    node_id: callback_serve.node_id.clone(),
                    region: callback_serve.node_region.clone(),
                    role: callback_serve.node_role.clone(),
                    status: "failed",
                    finished: true,
                    result: None,
                    error: Some(error_text.clone()),
                };
                update_async_job(
                    &async_jobs,
                    &spawned_job_id,
                    AsyncJobState::Failed(error_text),
                );
                if should_notify_callback(&callback_serve, "failed") {
                    let notify_serve = callback_serve.clone();
                    let notify_job_id = spawned_job_id.clone();
                    match tokio::task::spawn_blocking(move || {
                        send_callback_notification(&notify_serve, &callback_payload)
                    })
                    .await
                    {
                        Ok(Ok(())) => {}
                        Ok(Err(err)) => tracing::error!(
                            "[{}] callback failed for job={}: {}",
                            callback_serve.node_name,
                            notify_job_id,
                            err
                        ),
                        Err(err) => tracing::error!(
                            "[{}] callback join failed for job={}: {}",
                            callback_serve.node_name,
                            notify_job_id,
                            err
                        ),
                    }
                }
            }
            Err(err) => {
                tracing::error!(
                    "[{}] POST /toon/async 500 job={} serve worker join failed: {}",
                    node_name,
                    spawned_job_id,
                    err
                );
                let error_text = format!("serve worker join failed: {}", err);
                let callback_payload = ServeCallbackPayload {
                    ok: false,
                    job_id: spawned_job_id.clone(),
                    node: callback_serve.node_name.clone(),
                    node_id: callback_serve.node_id.clone(),
                    region: callback_serve.node_region.clone(),
                    role: callback_serve.node_role.clone(),
                    status: "failed",
                    finished: true,
                    result: None,
                    error: Some(error_text.clone()),
                };
                update_async_job(
                    &async_jobs,
                    &spawned_job_id,
                    AsyncJobState::Failed(error_text),
                );
                if should_notify_callback(&callback_serve, "failed") {
                    let notify_serve = callback_serve.clone();
                    let notify_job_id = spawned_job_id.clone();
                    match tokio::task::spawn_blocking(move || {
                        send_callback_notification(&notify_serve, &callback_payload)
                    })
                    .await
                    {
                        Ok(Ok(())) => {}
                        Ok(Err(err)) => tracing::error!(
                            "[{}] callback failed for job={}: {}",
                            callback_serve.node_name,
                            notify_job_id,
                            err
                        ),
                        Err(err) => tracing::error!(
                            "[{}] callback join failed for job={}: {}",
                            callback_serve.node_name,
                            notify_job_id,
                            err
                        ),
                    }
                }
            }
        }
    });

    tracing::info!(
        "[{}] POST /toon/async 202 job={}",
        state.config.node_name,
        job_id
    );
    (
        StatusCode::ACCEPTED,
        Json(serde_json::json!(ServeAsyncAcceptedResponse {
            ok: true,
            job_id,
            status: "queued",
            node: state.config.node_name.clone(),
        })),
    )
}

async fn serve_job_status(
    State(state): State<ServeAppState>,
    headers: HeaderMap,
    Path(job_id): Path<String>,
) -> (StatusCode, Json<serde_json::Value>) {
    if !request_is_authorized(&headers, &state.config) {
        tracing::info!(
            "[{}] GET /job/{{id}} 401 unauthorized",
            state.config.node_name
        );
        return unauthorized_json();
    }

    let Some(job) = snapshot_async_job(&state.async_jobs, &job_id) else {
        tracing::info!(
            "[{}] GET /job/{{id}} 404 job={}",
            state.config.node_name,
            job_id
        );
        return (
            StatusCode::NOT_FOUND,
            Json(serde_json::json!({
                "error": format!("unknown job `{}`", job_id)
            })),
        );
    };

    let response = match job {
        AsyncJobState::Queued => ServeJobResponse {
            ok: true,
            job_id: job_id.clone(),
            node: state.config.node_name.clone(),
            status: "queued",
            finished: false,
            result: None,
            error: None,
        },
        AsyncJobState::Running => ServeJobResponse {
            ok: true,
            job_id: job_id.clone(),
            node: state.config.node_name.clone(),
            status: "running",
            finished: false,
            result: None,
            error: None,
        },
        AsyncJobState::Completed(result) => ServeJobResponse {
            ok: true,
            job_id: job_id.clone(),
            node: state.config.node_name.clone(),
            status: "completed",
            finished: true,
            result: Some(result),
            error: None,
        },
        AsyncJobState::Failed(error) => ServeJobResponse {
            ok: false,
            job_id: job_id.clone(),
            node: state.config.node_name.clone(),
            status: "failed",
            finished: true,
            result: None,
            error: Some(error),
        },
    };

    tracing::info!(
        "[{}] GET /job/{{id}} 200 job={}",
        state.config.node_name,
        job_id
    );
    (StatusCode::OK, Json(serde_json::json!(response)))
}

pub(crate) async fn run_serve_mode(cli: &Cli) -> anyhow::Result<()> {
    let (serve, engine_config) = load_serve_runtime(cli)?;
    let engine = BatchEngine::new(engine_config)?;
    let reaper_engine = engine.clone();
    let app = Router::new()
        .route("/health", get(serve_health))
        .route("/toon", post(serve_toon))
        .route("/toon/async", post(serve_toon_async))
        .route("/job/{id}", get(serve_job_status))
        .with_state(ServeAppState {
            engine,
            config: serve.clone(),
            active_requests: Arc::new(AtomicUsize::new(0)),
            async_jobs: Arc::new(Mutex::new(BTreeMap::new())),
        });

    // Idle-session reaper: evict named sessions past their idle timeout so
    // pinned shells return to the shared pool instead of leaking into the
    // 84-process swamp the brief warns about.
    let idle_timeout = Duration::from_millis(serve.session_idle_timeout_ms);
    let reap_interval = Duration::from_millis(serve.session_reap_interval_ms);
    tokio::spawn(async move {
        let mut tick = tokio::time::interval(reap_interval);
        loop {
            tick.tick().await;
            let engine = reaper_engine.clone();
            let reaped =
                tokio::task::spawn_blocking(move || engine.reap_idle_sessions(idle_timeout))
                    .await
                    .unwrap_or_default();
            for key in reaped {
                tracing::info!("reaped idle session `{}`", key);
            }
        }
    });

    tracing::info!(
        "[{}] starting {} {} serve mode with {} workers ({}/{})",
        serve.node_name,
        SERVER_NAME,
        SERVER_VERSION,
        serve.workers,
        serve.node_id,
        serve.node_role
    );

    // Seat listeners: systemd socket activation (production). One unix socket per
    // seat, fd→seat name via LISTEN_FDNAMES. Each connection is attributed to the
    // seat whose socket it arrived on — the kernel cannot copy-paste the wrong uid,
    // which is the whole point of the per-seat-socket redesign.
    let seat_listeners = take_systemd_seat_listeners()?;

    if seat_listeners.is_empty() {
        // Dev/test fallback: single TCP listener, no socket identity. A middleware
        // layer stamps `SeatTag` per-request from the `x-mag-seat` header (falling
        // back to CTX_CALLER, then anon) so the handler signature stays uniform with
        // the socket path. The :9925 test instance and any non-socket-activated run
        // land here. Production is socket-activated.
        let bind = SocketAddr::new(serve.bind_ip, serve.port);
        tracing::info!(
            "[{}] no systemd seat sockets (LISTEN_FDS unset) — TCP fallback on {}",
            serve.node_name,
            bind
        );
        let app = app.layer(axum::middleware::from_fn(
            move |mut req: axum::extract::Request, next: axum::middleware::Next| async move {
                let seat = seat_from_headers(req.headers());
                req.extensions_mut().insert(SeatTag(seat));
                next.run(req).await
            },
        ));
        let listener = tokio::net::TcpListener::bind(bind).await?;
        axum::serve(listener, app).await?;
        return Ok(());
    }

    // One axum task per seat listener. The Router is cloned (cheap — state is all
    // Arc) and each clone gets its own `Extension<SeatTag>` layer stamping that
    // listener's seat onto every request it accepts. Handlers read the extension,
    // not a header.
    let mut join_set: tokio::task::JoinSet<anyhow::Result<()>> = tokio::task::JoinSet::new();
    for (seat, listener) in seat_listeners {
        let seat_name = seat.clone();
        tracing::info!("[{}] listening on seat socket `{}`", serve.node_name, seat);
        let seat_app = app.clone().layer(Extension(SeatTag(seat)));
        join_set.spawn(async move {
            axum::serve(listener, seat_app)
                .await
                .map_err(|e| anyhow!("seat listener `{}` failed: {}", seat_name, e))
        });
    }

    // Any listener dying is fatal — surface the first error and let the rest drop.
    // systemd will restart us.
    while let Some(res) = join_set.join_next().await {
        match res {
            Ok(Ok(())) => {}
            Ok(Err(e)) => return Err(e),
            Err(e) => return Err(anyhow!("seat listener task panicked: {}", e)),
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cmd_body_routes_to_simple_lane() {
        let payload: ServeRequestPayload =
            serde_json::from_str(r#"{"cmd": "echo hello"}"#).unwrap();
        match payload {
            ServeRequestPayload::Simple(simple) => {
                let batch = simple.into_batch().unwrap();
                assert_eq!(batch.commands.len(), 1);
                assert_eq!(batch.commands[0].command.as_deref(), Some("echo hello"));
            }
            _ => panic!("cmd body must route to Simple"),
        }
    }

    #[test]
    fn cmds_body_routes_to_simple_lane() {
        let payload: ServeRequestPayload =
            serde_json::from_str(r#"{"cmds": ["echo a", "echo b"]}"#).unwrap();
        match payload {
            ServeRequestPayload::Simple(simple) => {
                let batch = simple.into_batch().unwrap();
                assert_eq!(batch.commands.len(), 2);
            }
            _ => panic!("cmds body must route to Simple"),
        }
    }

    #[test]
    fn script_body_still_routes_to_toon() {
        let payload: ServeRequestPayload =
            serde_json::from_str(r#"{"script": "job a: echo hi"}"#).unwrap();
        assert!(matches!(payload, ServeRequestPayload::Toon(_)));
    }

    #[test]
    fn raw_body_still_routes_to_toon() {
        let payload: ServeRequestPayload = serde_json::from_str(r#"{"raw": "echo hi"}"#).unwrap();
        assert!(matches!(payload, ServeRequestPayload::Toon(_)));
    }

    #[test]
    fn session_close_body_routes_to_toon() {
        let payload: ServeRequestPayload =
            serde_json::from_str(r#"{"session_id": "work", "close": true}"#).unwrap();
        assert!(matches!(payload, ServeRequestPayload::Toon(_)));
    }

    #[test]
    fn batch_body_still_routes_to_batch() {
        let payload: ServeRequestPayload =
            serde_json::from_str(r#"{"commands": [{"command": "echo hi"}]}"#).unwrap();
        assert!(matches!(payload, ServeRequestPayload::Batch(_)));
    }

    #[test]
    fn cmd_with_legacy_null_fields_routes_to_simple() {
        // Old forward clients serialize every unset Option as explicit null.
        // That's the body the daemon actually receives from pre-fix stdio lanes.
        let payload: ServeRequestPayload = serde_json::from_str(
            r#"{"cmd": "echo hi", "cmds": null, "cwd": null, "script": null, "raw": null, "remote": null, "session_id": null, "close": null, "env": {}, "default_cwd": null, "default_timeout_ms": null, "max_parallel": null, "continue_on_error": null, "capture_limit_bytes": null, "warm": null}"#,
        )
        .unwrap();
        match payload {
            ServeRequestPayload::Simple(simple) => {
                let batch = simple.into_batch().unwrap();
                assert_eq!(batch.commands[0].command.as_deref(), Some("echo hi"));
            }
            _ => panic!("legacy-null cmd body must route to Simple"),
        }
    }
}
