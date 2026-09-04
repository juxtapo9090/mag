//! Forwarding client: posts tool requests to the resident mag daemon so all
//! seats share one warm pool. Blocking (reqwest) — callers wrap in
//! spawn_blocking, same as the engine lanes.
//!
//! Two transports, selected by `MAG_URL`:
//! - `http://host:port` (default `http://127.0.0.1:7734`) — the legacy TCP
//!   lane. Still the only path that works before Phase 3 cutover is complete.
//! - `unix:///run/mag/<seat>.sock` — a per-seat unix domain socket. Identity
//!   comes from *which socket was dialed*, not from a header the caller writes
//!   about itself. `x-mag-seat` is still sent (the server reads it on the TCP
//!   fallback path) but the socket is the source of truth in production.

use crate::AppResult;
use crate::cli::BatchResponse;
use crate::mcp::ToolRequest;
use anyhow::anyhow;
use std::time::Duration;

pub const DEFAULT_DAEMON_URL: &str = "http://127.0.0.1:7734";

/// The daemon endpoint, as configured by `MAG_URL` (or the loopback default).
///
/// A `unix:///run/mag/<seat>.sock` value means "dial this socket path";
/// an `http://host:port` value is the legacy TCP lane.
pub fn daemon_url() -> String {
    std::env::var("MAG_URL")
        .ok()
        .map(|value| value.trim().trim_end_matches('/').to_string())
        .filter(|value| !value.is_empty())
        .unwrap_or_else(|| DEFAULT_DAEMON_URL.to_string())
}

/// The socket path for a `unix:///path/to/sock` URL, with the `unix://` prefix
/// and any trailing slash stripped. Returns `None` for HTTP URLs.
fn unix_path(url: &str) -> Option<&str> {
    url.strip_prefix("unix://")
        .map(|rest| rest.trim_end_matches('/'))
}

/// The URL to pass to reqwest for a given request path (`/toon`, `/health`).
///
/// reqwest requires a parseable `http://` URL even when dialing a unix socket —
/// the `unix_socket()` connector overrides *where* the connection goes, but the
/// URL still has to look like HTTP. So for a `unix://` `MAG_URL` we synthesize
/// `http://localhost<path>`; the host is irrelevant (the socket decides), and
/// `localhost` keeps the URL well-formed. For a real HTTP `MAG_URL` we append
/// the path as before.
fn request_url(path: &str) -> String {
    let base = daemon_url();
    if unix_path(&base).is_some() {
        format!("http://localhost{}", path)
    } else {
        format!("{}{}", base, path)
    }
}

/// Build a blocking client wired for the configured transport. A `unix://`
/// `MAG_URL` installs a unix-socket connector; everything else is plain HTTP.
/// One builder path serves both call sites so the identity probe and the
/// forwarder share the same dialer.
fn build_client() -> AppResult<reqwest::blocking::Client> {
    let mut builder = reqwest::blocking::Client::builder().timeout(Duration::from_secs(330));
    let url = daemon_url();
    if let Some(path) = unix_path(&url) {
        builder = builder.unix_socket(path);
    }
    Ok(builder.build()?)
}

/// Ask the daemon who *it* runs as, once, so refusal hints describe the process
/// that earned the refusal rather than this forwarder — which is spawned by each
/// seat's own client and is often root while the daemon is `juxtapo`.
///
/// Best-effort by design: if the probe fails we install nothing, and the hint
/// falls back to local facts or stays silent. It must never invent an identity.
fn learn_exec_identity(client: &reqwest::blocking::Client) {
    static PROBED: std::sync::Once = std::sync::Once::new();
    PROBED.call_once(|| {
        let Ok(response) = client
            .get(request_url("/health"))
            .timeout(Duration::from_secs(2))
            .send()
        else {
            return;
        };
        let Ok(health) = response.json::<serde_json::Value>() else {
            return;
        };
        let (Some(user), Some(can_escalate)) = (
            health.get("exec_user").and_then(serde_json::Value::as_str),
            health
                .get("exec_can_escalate")
                .and_then(serde_json::Value::as_bool),
        ) else {
            return;
        };
        crate::hint::set_exec_identity(crate::hint::ExecIdentity {
            user: user.to_string(),
            can_escalate,
        });
    });
}

/// POST a tool request to the daemon and return the slim-rendered text.
/// The seat travels as `x-mag-seat` so named sessions stay namespaced on the
/// TCP fallback path. Over a unix socket the seat is read from the socket
/// itself; the header is harmless surplus there.
pub fn forward_tool_request(request: &ToolRequest, seat: &str) -> AppResult<String> {
    let url = request_url("/toon");
    let client = build_client()?;
    learn_exec_identity(&client);

    let response = client
        .post(&url)
        .header("content-type", "application/json")
        .header("x-mag-seat", seat)
        .json(request)
        .send()
        .map_err(|err| anyhow!("mag daemon unreachable at {}: {}", url, err))?;

    let status = response.status();
    let body = response.text()?;
    if !status.is_success() {
        let detail = serde_json::from_str::<serde_json::Value>(&body)
            .ok()
            .and_then(|value| {
                value
                    .get("error")
                    .and_then(|e| e.as_str().map(str::to_owned))
            })
            .unwrap_or(body);
        return Err(anyhow!("mag daemon {}: {}", status.as_u16(), detail));
    }

    let mut batch: BatchResponse = serde_json::from_str(&body)?;
    Ok(crate::render::render_toon_response(&mut batch))
}
