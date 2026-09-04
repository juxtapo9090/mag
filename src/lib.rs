//! mag — magazine. Lean systemd-resident warm-pool shell runner.
//! Forked from aerondight (sir_lancelot_V2) — trim + re-home, not a rewrite.

pub mod audit;
pub mod cli;
pub mod client;
pub mod config;
pub mod diet;
pub mod engine;
pub mod exec;
pub mod hint;
pub mod mcp;
pub mod pool;
pub mod remote;
pub mod render;
pub mod serve;
pub mod term;
pub mod toon;

use anyhow::Result;
use std::process::ExitCode;

pub const DEFAULT_WORKERS: usize = 10;
pub const DEFAULT_TIMEOUT_MS: u64 = 60_000;
pub const DEFAULT_CAPTURE_LIMIT_BYTES: usize = 64 * 1024;
/// Token-diet: hard echo cap per stream, applied after the rtk-style filter
/// pass. Filtered output above this is cut with a byte-count marker.
pub const DEFAULT_DIET_ECHO_MAX_BYTES: usize = 8 * 1024;
pub const MAX_COMMANDS: usize = 20;
/// Session id used by `sticky: true`. Warm sessions are namespaced
/// `<seat>:<session_id>`, so one shared name is still private per seat — the
/// caller gets continuity without inventing and tracking id strings.
pub const DEFAULT_STICKY_SESSION: &str = "default";
pub const SERVER_NAME: &str = "mag";
pub const SERVER_VERSION: &str = env!("CARGO_PKG_VERSION");
pub const SERVER_DESCRIPTION: &str = "mag — resident warm-pool shell runner";

pub(crate) type AppResult<T> = anyhow::Result<T>;

fn init_tracing() {
    let _ = tracing_subscriber::fmt()
        // Respect RUST_LOG exactly; mag is commonly a transparent wrapper.
        .with_env_filter(tracing_subscriber::EnvFilter::from_default_env())
        .with_writer(std::io::stderr)
        .try_init();
}

/// Returns the process exit status rather than exiting itself: CLI one-shots
/// (`run-batch` / `run-toon`) hand back the wrapped job's own code, so `$?`
/// means "did the command succeed", while mag's own failures still travel the
/// `Err` path and exit 1.
pub fn run_main() -> Result<ExitCode> {
    init_tracing();
    tokio_run()
}

#[tokio::main]
async fn tokio_run() -> Result<ExitCode> {
    cli::dispatch().await
}
