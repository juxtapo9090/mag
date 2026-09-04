use crate::audit::append_batch_audit;
use crate::cli::{BatchRequest, BatchResponse, CommandResult, CommandSpec, ExecOutcome};
use crate::exec::{DietConfig, merge_env, outcome_to_command_result, run_exec, run_fresh_shell};
use crate::mcp::{TerminalAction, TerminalRequest, ToolRequest};
use crate::pool::WarmShellPool;
use crate::remote::{NodeRegistry, run_remote_command_result};
use crate::render::{render_simple_response, render_toon_response};
use crate::serve::ServeRuntimeConfig;
use crate::serve::validate_serve_batch;
use crate::toon::{ToonRequest, parse_raw_script, parse_toon_script};
use crate::{AppResult, MAX_COMMANDS};
use anyhow::anyhow;
use std::collections::BTreeMap;
use std::path::Path;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::Instant;

fn has_session(engine: &BatchEngine, seat: &str, session_id: &str) -> bool {
    engine.warm_pool.has_session(seat, session_id)
}

/// Seat identity for session namespacing: CTX_CALLER when set, else "local".
pub(crate) fn local_seat() -> String {
    std::env::var("CTX_CALLER")
        .ok()
        .filter(|value| !value.trim().is_empty())
        .unwrap_or_else(|| "local".to_string())
}

#[derive(Clone)]
pub struct EngineConfig {
    pub workers: usize,
    pub default_timeout_ms: u64,
    pub capture_limit_bytes: usize,
    pub node_registry: Option<Arc<NodeRegistry>>,
    /// Token-diet master toggle: rtk-style filter + echo cap on stdout/stderr.
    pub token_diet_enabled: bool,
    /// Hard echo cap per stream after the filter pass.
    pub token_diet_echo_max_bytes: usize,
}

#[derive(Clone)]
pub struct BatchEngine {
    pub(crate) config: EngineConfig,
    pub(crate) warm_pool: Arc<WarmShellPool>,
}

impl BatchEngine {
    pub(crate) fn new(config: EngineConfig) -> AppResult<Self> {
        let warm_pool = Arc::new(WarmShellPool::new(config.workers.max(1))?);
        Ok(Self { config, warm_pool })
    }

    pub(crate) fn run_batch(&self, request: BatchRequest) -> BatchResponse {
        let started = Instant::now();
        let command_count = request.commands.len();
        if command_count == 0 {
            return BatchResponse {
                command_count,
                elapsed_ms: 0,
                stopped_early: false,
                results: vec![CommandResult {
                    index: 0,
                    id: Some("batch".to_string()),
                    mode: "validation".to_string(),
                    success: false,
                    exit_code: None,
                    timed_out: false,
                    duration_ms: 0,
                    stdout: None,
                    stderr: None,
                    stdout_truncated: false,
                    stderr_truncated: false,
                    diet_applied: false,
                    exec_user: None,
                    error: Some("commands must not be empty".to_string()),
                }],
            };
        }

        if command_count > MAX_COMMANDS {
            return BatchResponse {
                command_count,
                elapsed_ms: 0,
                stopped_early: false,
                results: vec![CommandResult {
                    index: 0,
                    id: Some("batch".to_string()),
                    mode: "validation".to_string(),
                    success: false,
                    exit_code: None,
                    timed_out: false,
                    duration_ms: 0,
                    stdout: None,
                    stderr: None,
                    stdout_truncated: false,
                    stderr_truncated: false,
                    diet_applied: false,
                    exec_user: None,
                    error: Some(format!("commands exceeds max of {}", MAX_COMMANDS)),
                }],
            };
        }

        let default_timeout_ms = request
            .default_timeout_ms
            .unwrap_or(self.config.default_timeout_ms);
        let capture_limit_bytes = request
            .capture_limit_bytes
            .unwrap_or(self.config.capture_limit_bytes);
        if let Some(default_cwd) = request
            .default_cwd
            .as_deref()
            .filter(|value| !value.trim().is_empty())
            && !Path::new(default_cwd).is_dir()
        {
            return BatchResponse {
                command_count,
                elapsed_ms: started.elapsed().as_millis(),
                stopped_early: true,
                results: vec![CommandResult {
                    index: 0,
                    id: Some("batch".to_string()),
                    mode: "validation".to_string(),
                    success: false,
                    exit_code: None,
                    timed_out: false,
                    duration_ms: 0,
                    stdout: None,
                    stderr: None,
                    stdout_truncated: false,
                    stderr_truncated: false,
                    diet_applied: false,
                    exec_user: None,
                    error: Some(format!(
                        "batch cwd `{}` does not exist; create it first or use absolute paths",
                        default_cwd
                    )),
                }],
            };
        }
        let diet = self.diet_config_for(request.unfiltered.unwrap_or(false));
        let available = thread::available_parallelism()
            .map(std::num::NonZeroUsize::get)
            .unwrap_or(1);
        let max_parallel = request
            .max_parallel
            .unwrap_or_else(|| available.min(command_count))
            .clamp(1, command_count);
        let continue_on_error = request.continue_on_error.unwrap_or(true);

        let commands = Arc::new(request.commands);
        let batch_env = Arc::new(request.env.clone());
        let next_index = Arc::new(AtomicUsize::new(0));
        let stop_flag = Arc::new(AtomicBool::new(false));
        let results = Arc::new(Mutex::new(vec![None; command_count]));
        let mut handles = Vec::new();

        for _ in 0..max_parallel {
            let engine = self.clone();
            let commands = Arc::clone(&commands);
            let batch_env = Arc::clone(&batch_env);
            let next_index = Arc::clone(&next_index);
            let stop_flag = Arc::clone(&stop_flag);
            let results = Arc::clone(&results);
            let default_cwd = request.default_cwd.clone();
            let default_remote = request.remote.clone();

            handles.push(thread::spawn(move || {
                loop {
                    if stop_flag.load(Ordering::Relaxed) {
                        break;
                    }

                    let index = next_index.fetch_add(1, Ordering::Relaxed);
                    if index >= commands.len() {
                        break;
                    }

                    let result = engine.execute_command(
                        index,
                        &commands[index],
                        batch_env.as_ref(),
                        default_remote.as_deref(),
                        default_cwd.as_deref(),
                        default_timeout_ms,
                        capture_limit_bytes,
                        diet,
                    );

                    if !continue_on_error && !result.success {
                        stop_flag.store(true, Ordering::Relaxed);
                    }

                    if let Ok(mut guard) = results.lock() {
                        guard[index] = Some(result);
                    }
                }
            }));
        }

        for handle in handles {
            let _ = handle.join();
        }

        let stopped_early = stop_flag.load(Ordering::Relaxed);
        let final_results = if let Ok(mut guard) = results.lock() {
            for (index, slot) in guard.iter_mut().enumerate() {
                if slot.is_none() {
                    *slot = Some(CommandResult {
                        index,
                        id: commands[index].id.clone(),
                        mode: "skipped".to_string(),
                        success: false,
                        exit_code: None,
                        timed_out: false,
                        duration_ms: 0,
                        stdout: None,
                        stderr: None,
                        stdout_truncated: false,
                        stderr_truncated: false,
                        diet_applied: false,
                        exec_user: None,
                        error: Some("skipped because an earlier command failed".to_string()),
                    });
                }
            }
            guard.iter().flatten().cloned().collect::<Vec<_>>()
        } else {
            vec![CommandResult {
                index: 0,
                id: Some("batch".to_string()),
                mode: "internal".to_string(),
                success: false,
                exit_code: None,
                timed_out: false,
                duration_ms: 0,
                stdout: None,
                stderr: None,
                stdout_truncated: false,
                stderr_truncated: false,
                diet_applied: false,
                exec_user: None,
                error: Some("results lock poisoned".to_string()),
            }]
        };

        BatchResponse {
            command_count,
            elapsed_ms: started.elapsed().as_millis(),
            stopped_early,
            results: final_results,
        }
    }

    pub(crate) fn run_tool_request(&self, request: ToonRequest) -> AppResult<BatchResponse> {
        self.run_tool_request_as(&local_seat(), request)
    }

    /// Reap named sessions idle past `idle_timeout`; returns reaped keys
    /// (`<seat>:<session_id>`) for daemon logs.
    pub(crate) fn reap_idle_sessions(&self, idle_timeout: std::time::Duration) -> Vec<String> {
        self.warm_pool.reap_idle_sessions(idle_timeout)
    }

    /// Seat-aware variant: sessions are namespaced `<seat>:<session_id>` so a
    /// shared daemon keeps seats' shells apart (and seats can only close
    /// their own).
    pub(crate) fn run_tool_request_as(
        &self,
        seat: &str,
        request: ToonRequest,
    ) -> AppResult<BatchResponse> {
        let session_id = resolve_sticky_session(request.session_id.clone(), request.sticky);
        let close = request.close.unwrap_or(false);
        let has_payload = request.script.is_some() || request.raw.is_some();

        if close && session_id.is_none() {
            return Err(anyhow!("`close` requires `session_id` or `sticky: true`"));
        }
        if !has_payload && !close {
            return Err(anyhow!("mag requires either `script` or `raw`"));
        }

        let mut response = if has_payload {
            let batch = into_batch_request(request)?;
            if let Some(id) = session_id.as_deref() {
                self.run_named_batch(seat, id, batch)?
            } else {
                self.run_batch(batch)
            }
        } else {
            self.close_session_response(session_id.as_deref().unwrap(), false)
        };

        if close {
            let session = session_id.as_deref().unwrap();
            let released = self.warm_pool.release_named(seat, session)?;
            if !has_payload {
                response = self.close_session_response(session, released);
            }
        }

        Ok(response)
    }

    /// Serve-mode TOON lane. Sessions ARE supported here (mag is the resident
    /// appliance — named sessions are its reason to exist), namespaced by the
    /// caller's seat header.
    pub(crate) fn run_served_request(
        &self,
        seat: &str,
        request: ToonRequest,
        serve: &ServeRuntimeConfig,
    ) -> AppResult<BatchResponse> {
        let session_id = resolve_sticky_session(request.session_id.clone(), request.sticky);
        let close = request.close.unwrap_or(false);
        let has_payload = request.script.is_some() || request.raw.is_some();

        if close && session_id.is_none() {
            return Err(anyhow!("`close` requires `session_id` or `sticky: true`"));
        }
        if !has_payload && !close {
            return Err(anyhow!("mag requires either `script` or `raw`"));
        }

        if let Some(id) = session_id.as_deref() {
            self.check_named_session_limit(seat, id, serve)?;
        }

        if !has_payload {
            // close-only request
            let session = session_id.as_deref().unwrap();
            let released = self.warm_pool.release_named(seat, session)?;
            return Ok(self.close_session_response(session, released));
        }

        let batch = into_batch_request(request)?;
        let mut response = if let Some(id) = session_id.as_deref() {
            self.run_served_named_batch_as(seat, id, batch, serve)?
        } else {
            self.run_served_batch_as(seat, batch, serve)?
        };

        if close {
            let session = session_id.as_deref().unwrap();
            let _ = self.warm_pool.release_named(seat, session)?;
        }
        response.command_count = response.results.len();
        Ok(response)
    }

    /// Run a batch inside a named warm shell, with serve-mode defaults,
    /// validation and audit. Shared by the power lane and the simple
    /// (`cmd`/`cmds`) lane so a `session_id` means the same thing in both.
    pub(crate) fn run_served_named_batch_as(
        &self,
        seat: &str,
        session_id: &str,
        batch: BatchRequest,
        serve: &ServeRuntimeConfig,
    ) -> AppResult<BatchResponse> {
        let batch = self.apply_serve_defaults(batch, serve);
        validate_serve_batch(&batch, serve)?;
        let response = self.run_named_batch(seat, session_id, batch.clone())?;
        if let Err(err) =
            append_batch_audit(&serve.audit, seat, Some(session_id), &batch, &response)
        {
            tracing::error!("audit append failed: {}", err);
        }
        Ok(response)
    }

    /// Every named warm shell this seat currently holds.
    pub(crate) fn session_statuses(&self, seat: &str) -> Vec<crate::pool::SessionStatus> {
        self.warm_pool.session_statuses(seat)
    }

    /// Release a named warm shell held by this seat. Returns whether a session
    /// was actually holding a slot.
    pub(crate) fn release_named_session(&self, seat: &str, session_id: &str) -> AppResult<bool> {
        self.warm_pool.release_named(seat, session_id)
    }

    /// Enforce the configured named-session ceiling. A request that would open a
    /// *new* session past the cap is refused; one that reuses a session the seat
    /// already holds is not.
    pub(crate) fn check_named_session_limit(
        &self,
        seat: &str,
        session_id: &str,
        serve: &ServeRuntimeConfig,
    ) -> AppResult<()> {
        if let Some(max_named) = serve.session_max_named {
            if self.warm_pool.named_session_count() >= max_named
                && !has_session(self, seat, session_id)
            {
                return Err(anyhow!(
                    "named session limit {} reached; close a session first",
                    max_named
                ));
            }
        }
        Ok(())
    }

    fn apply_serve_defaults(
        &self,
        mut batch: BatchRequest,
        serve: &ServeRuntimeConfig,
    ) -> BatchRequest {
        if batch.default_cwd.is_none() {
            batch.default_cwd = serve.default_cwd.clone();
        }
        if batch.default_timeout_ms.is_none() {
            batch.default_timeout_ms = serve.default_timeout_ms;
        }
        if batch.capture_limit_bytes.is_none() {
            batch.capture_limit_bytes = serve.max_capture_bytes;
        }
        batch
    }

    pub(crate) fn run_served_batch_as(
        &self,
        seat: &str,
        batch: BatchRequest,
        serve: &ServeRuntimeConfig,
    ) -> AppResult<BatchResponse> {
        if batch.remote.is_some() || batch.commands.iter().any(|spec| spec.remote.is_some()) {
            return Err(anyhow!(
                "serve mode does not support `remote` forwarding yet"
            ));
        }

        let batch = self.apply_serve_defaults(batch, serve);
        validate_serve_batch(&batch, serve)?;
        let response = self.run_batch(batch.clone());
        if let Err(err) = append_batch_audit(&serve.audit, seat, None, &batch, &response) {
            tracing::error!("audit append failed: {}", err);
        }
        Ok(response)
    }

    fn run_named_batch(
        &self,
        seat: &str,
        session_id: &str,
        request: BatchRequest,
    ) -> AppResult<BatchResponse> {
        let started = Instant::now();
        let command_count = request.commands.len();
        if command_count == 0 {
            return Ok(BatchResponse {
                command_count,
                elapsed_ms: 0,
                stopped_early: false,
                results: vec![CommandResult {
                    index: 0,
                    id: Some(session_id.to_string()),
                    mode: "validation".to_string(),
                    success: false,
                    exit_code: None,
                    timed_out: false,
                    duration_ms: 0,
                    stdout: None,
                    stderr: None,
                    stdout_truncated: false,
                    stderr_truncated: false,
                    diet_applied: false,
                    exec_user: None,
                    error: Some("commands must not be empty".to_string()),
                }],
            });
        }
        if command_count > MAX_COMMANDS {
            return Ok(BatchResponse {
                command_count,
                elapsed_ms: 0,
                stopped_early: false,
                results: vec![CommandResult {
                    index: 0,
                    id: Some(session_id.to_string()),
                    mode: "validation".to_string(),
                    success: false,
                    exit_code: None,
                    timed_out: false,
                    duration_ms: 0,
                    stdout: None,
                    stderr: None,
                    stdout_truncated: false,
                    stderr_truncated: false,
                    diet_applied: false,
                    exec_user: None,
                    error: Some(format!("commands exceeds max of {}", MAX_COMMANDS)),
                }],
            });
        }

        let slot = self.warm_pool.claim_named(seat, session_id)?;
        let default_timeout_ms = request
            .default_timeout_ms
            .unwrap_or(self.config.default_timeout_ms);
        let capture_limit_bytes = request
            .capture_limit_bytes
            .unwrap_or(self.config.capture_limit_bytes);
        let diet = self.diet_config_for(request.unfiltered.unwrap_or(false));
        let continue_on_error = request.continue_on_error.unwrap_or(true);
        let default_cwd = request.default_cwd.clone();
        let default_remote = request.remote.clone();
        let batch_env = request.env.clone();
        let mut results = Vec::with_capacity(command_count);
        let mut stopped_early = false;

        for (index, spec) in request.commands.iter().enumerate() {
            let result = self.execute_command_on_slot(
                index,
                spec,
                &batch_env,
                default_remote.as_deref(),
                default_cwd.as_deref(),
                default_timeout_ms,
                capture_limit_bytes,
                diet,
                slot,
            );
            let failed = !result.success;
            results.push(result);
            if failed && !continue_on_error {
                stopped_early = true;
                for skipped_index in (index + 1)..command_count {
                    results.push(CommandResult {
                        index: skipped_index,
                        id: request.commands[skipped_index].id.clone(),
                        mode: "skipped".to_string(),
                        success: false,
                        exit_code: None,
                        timed_out: false,
                        duration_ms: 0,
                        stdout: None,
                        stderr: None,
                        stdout_truncated: false,
                        stderr_truncated: false,
                        diet_applied: false,
                        exec_user: None,
                        error: Some("skipped because an earlier command failed".to_string()),
                    });
                }
                break;
            }
        }

        Ok(BatchResponse {
            command_count,
            elapsed_ms: started.elapsed().as_millis(),
            stopped_early,
            results,
        })
    }

    fn close_session_response(&self, session_id: &str, released: bool) -> BatchResponse {
        BatchResponse {
            command_count: 1,
            elapsed_ms: 0,
            stopped_early: false,
            results: vec![CommandResult {
                index: 0,
                id: Some(session_id.to_string()),
                mode: "session-close".to_string(),
                success: released,
                exit_code: None,
                timed_out: false,
                duration_ms: 0,
                stdout: None,
                stderr: None,
                stdout_truncated: false,
                stderr_truncated: false,
                diet_applied: false,
                exec_user: None,
                error: if released {
                    None
                } else {
                    Some(format!("named session `{}` was not claimed", session_id))
                },
            }],
        }
    }

    fn diet_config(&self) -> DietConfig {
        DietConfig {
            enabled: self.config.token_diet_enabled,
            echo_max_bytes: self.config.token_diet_echo_max_bytes,
        }
    }

    fn diet_config_for(&self, unfiltered: bool) -> DietConfig {
        if unfiltered {
            DietConfig {
                enabled: false,
                echo_max_bytes: usize::MAX,
            }
        } else {
            self.diet_config()
        }
    }

    #[allow(clippy::too_many_arguments)]
    fn execute_command(
        &self,
        index: usize,
        spec: &CommandSpec,
        batch_env: &BTreeMap<String, String>,
        default_remote: Option<&str>,
        default_cwd: Option<&str>,
        default_timeout_ms: u64,
        capture_limit_bytes: usize,
        diet: DietConfig,
    ) -> CommandResult {
        let effective_remote = spec.remote.as_deref().or(default_remote);
        if let Some(remote) = effective_remote {
            return run_remote_command_result(
                index,
                spec,
                batch_env,
                remote,
                default_cwd,
                default_timeout_ms,
                capture_limit_bytes,
                self.config.node_registry.as_deref(),
                diet,
            );
        }

        let effective_cwd = spec.cwd.as_deref().or(default_cwd);
        let effective_timeout_ms = spec.timeout_ms.unwrap_or(default_timeout_ms);
        let effective_env = merge_env(batch_env, &spec.env);
        let effective_stdin = spec.stdin.as_deref();
        let outcome = match (&spec.argv, &spec.command) {
            (Some(argv), None) => run_exec(
                argv,
                effective_cwd,
                &effective_env,
                effective_stdin,
                effective_timeout_ms,
            ),
            (None, Some(command)) => {
                if spec.warm.unwrap_or(true) {
                    self.warm_pool.run(
                        command,
                        effective_cwd,
                        &effective_env,
                        effective_stdin,
                        effective_timeout_ms,
                    )
                } else {
                    run_fresh_shell(
                        command,
                        effective_cwd,
                        &effective_env,
                        effective_stdin,
                        effective_timeout_ms,
                    )
                }
            }
            (None, None) => ExecOutcome {
                mode: "validation".to_string(),
                exit_code: None,
                timed_out: false,
                duration_ms: 0,
                stdout: String::new(),
                stderr: String::new(),
                error: Some("command needs either `command` or `argv`".to_string()),
            },
            (Some(_), Some(_)) => ExecOutcome {
                mode: "validation".to_string(),
                exit_code: None,
                timed_out: false,
                duration_ms: 0,
                stdout: String::new(),
                stderr: String::new(),
                error: Some("command cannot set both `command` and `argv`".to_string()),
            },
        };

        outcome_to_command_result(
            index,
            spec.id.clone(),
            capture_limit_bytes,
            outcome,
            spec.command.as_deref(),
            diet,
        )
    }

    #[allow(clippy::too_many_arguments)]
    fn execute_command_on_slot(
        &self,
        index: usize,
        spec: &CommandSpec,
        batch_env: &BTreeMap<String, String>,
        default_remote: Option<&str>,
        default_cwd: Option<&str>,
        default_timeout_ms: u64,
        capture_limit_bytes: usize,
        diet: DietConfig,
        slot: usize,
    ) -> CommandResult {
        let effective_remote = spec.remote.as_deref().or(default_remote);
        if let Some(remote) = effective_remote {
            return run_remote_command_result(
                index,
                spec,
                batch_env,
                remote,
                default_cwd,
                default_timeout_ms,
                capture_limit_bytes,
                self.config.node_registry.as_deref(),
                diet,
            );
        }

        let effective_cwd = spec.cwd.as_deref().or(default_cwd);
        let effective_timeout_ms = spec.timeout_ms.unwrap_or(default_timeout_ms);
        let effective_env = merge_env(batch_env, &spec.env);
        let effective_stdin = spec.stdin.as_deref();
        let outcome = match (&spec.argv, &spec.command) {
            (Some(argv), None) => run_exec(
                argv,
                effective_cwd,
                &effective_env,
                effective_stdin,
                effective_timeout_ms,
            ),
            (None, Some(command)) => self.warm_pool.run_named_on_slot(
                slot,
                command,
                effective_cwd,
                &effective_env,
                effective_stdin,
                effective_timeout_ms,
            ),
            (None, None) => ExecOutcome {
                mode: "validation".to_string(),
                exit_code: None,
                timed_out: false,
                duration_ms: 0,
                stdout: String::new(),
                stderr: String::new(),
                error: Some("command needs either `command` or `argv`".to_string()),
            },
            (Some(_), Some(_)) => ExecOutcome {
                mode: "validation".to_string(),
                exit_code: None,
                timed_out: false,
                duration_ms: 0,
                stdout: String::new(),
                stderr: String::new(),
                error: Some("command cannot set both `command` and `argv`".to_string()),
            },
        };

        outcome_to_command_result(
            index,
            spec.id.clone(),
            capture_limit_bytes,
            outcome,
            spec.command.as_deref(),
            diet,
        )
    }
}

#[derive(Debug, Clone)]
pub(crate) enum PreparedToolRequest {
    Terminal(TerminalRequest),
    /// A question about warm shells, not a command to run.
    Sessions {
        session_id: Option<String>,
    },
    Simple {
        batch: BatchRequest,
        commands: Vec<String>,
        session_id: Option<String>,
        close: bool,
    },
    Toon(ToonRequest),
}

pub(crate) fn into_batch_request(request: ToonRequest) -> AppResult<BatchRequest> {
    let ToonRequest {
        script,
        raw,
        remote,
        session_id: _,
        sticky: _,
        close: _,
        env,
        default_cwd,
        default_timeout_ms,
        max_parallel,
        continue_on_error,
        capture_limit_bytes,
        unfiltered,
        warm,
    } = request;

    if remote.is_some() && raw.is_some() {
        return Err(anyhow!(
            "`remote` currently requires `script`; top-level `raw` stays local and line-oriented"
        ));
    }

    let mut batch = match (script, raw) {
        (Some(script), None) => parse_toon_script(&script)?,
        (None, Some(raw)) => parse_raw_script(&raw, warm.unwrap_or(true))?,
        (Some(_), Some(_)) => {
            return Err(anyhow!("mag accepts either `script` or `raw`, not both"));
        }
        (None, None) => return Err(anyhow!("mag requires either `script` or `raw`")),
    };

    for (key, value) in env {
        batch.env.insert(key, value);
    }
    if let Some(value) = remote {
        batch.remote = Some(value);
    }
    if let Some(value) = default_cwd {
        batch.default_cwd = Some(value);
    }
    if let Some(value) = default_timeout_ms {
        batch.default_timeout_ms = Some(value);
    }
    if let Some(value) = max_parallel {
        batch.max_parallel = Some(value);
    }
    if let Some(value) = continue_on_error {
        batch.continue_on_error = Some(value);
    }
    if let Some(value) = capture_limit_bytes {
        batch.capture_limit_bytes = Some(value);
    }
    if let Some(value) = unfiltered {
        batch.unfiltered = Some(value);
    }

    Ok(batch)
}

pub(crate) fn execute_tool_request(engine: BatchEngine, request: ToolRequest) -> AppResult<String> {
    match prepare_tool_request(request)? {
        PreparedToolRequest::Terminal(request) => execute_terminal_request(request),
        PreparedToolRequest::Sessions { session_id } => {
            let statuses = engine.session_statuses(&local_seat());
            Ok(crate::render::render_session_statuses(
                &statuses,
                session_id.as_deref(),
            ))
        }
        PreparedToolRequest::Simple {
            batch,
            commands,
            session_id,
            close,
        } => {
            let seat = local_seat();
            let response = match session_id.as_deref() {
                Some(id) => {
                    let response = engine.run_named_batch(&seat, id, batch)?;
                    if close {
                        engine.release_named_session(&seat, id)?;
                    }
                    response
                }
                None => engine.run_batch(batch),
            };
            Ok(render_simple_response(&response, &commands))
        }
        PreparedToolRequest::Toon(request) => {
            let mut response = engine.run_tool_request(request)?;
            Ok(render_toon_response(&mut response))
        }
    }
}

/// An explicit `session_id` always wins; `sticky: true` falls back to the
/// per-seat default. Blank ids are treated as absent rather than as a session
/// literally named "".
fn resolve_sticky_session(session_id: Option<String>, sticky: Option<bool>) -> Option<String> {
    session_id
        .map(|value| value.trim().to_string())
        .filter(|value| !value.is_empty())
        .or_else(|| {
            sticky
                .unwrap_or(false)
                .then(|| crate::DEFAULT_STICKY_SESSION.to_string())
        })
}

pub(crate) fn prepare_tool_request(request: ToolRequest) -> AppResult<PreparedToolRequest> {
    let ToolRequest {
        terminal,
        cmd,
        cmds,
        cwd,
        script,
        raw,
        remote,
        session_id,
        sticky,
        close,
        env,
        default_cwd,
        default_timeout_ms,
        max_parallel,
        continue_on_error,
        capture_limit_bytes,
        unfiltered,
        warm,
        sessions,
    } = request;

    if sessions.unwrap_or(false) {
        return Ok(PreparedToolRequest::Sessions {
            session_id: session_id.filter(|value| !value.trim().is_empty()),
        });
    }

    let has_non_terminal_lane = cmd.is_some()
        || cmds.is_some()
        || cwd.is_some()
        || script.is_some()
        || raw.is_some()
        || remote.is_some()
        || session_id.is_some()
        || sticky.is_some()
        || close.unwrap_or(false)
        || !env.is_empty()
        || default_cwd.is_some()
        || default_timeout_ms.is_some()
        || max_parallel.is_some()
        || continue_on_error.is_some()
        || capture_limit_bytes.is_some()
        || unfiltered.is_some()
        || warm.is_some();

    if let Some(terminal) = terminal {
        if has_non_terminal_lane {
            return Err(anyhow!(
                "use either `terminal` or the command lanes, not both"
            ));
        }
        return Ok(PreparedToolRequest::Terminal(terminal));
    }

    if cwd.is_some() && default_cwd.is_some() {
        return Err(anyhow!("use either `cwd` or `default_cwd`, not both"));
    }

    let effective_cwd = cwd.or(default_cwd);
    let use_simple_lane = cmd.is_some() || cmds.is_some();
    // `session_id` / `close` are deliberately NOT power-lane markers: pinning a
    // warm shell is orthogonal to which lane you write in, and the daemon has
    // always accepted them here (it just discarded them). Rejecting them
    // locally while silently dropping them remotely made the same request mean
    // two different things depending on the backend.
    let use_power_lane = script.is_some()
        || raw.is_some()
        || remote.is_some()
        || !env.is_empty()
        || default_timeout_ms.is_some()
        || warm.is_some();

    if use_simple_lane && use_power_lane {
        return Err(anyhow!(
            "use either the simple lane (`cmd` / `cmds`) or the power lane (`raw` / `script`), not both"
        ));
    }

    if use_simple_lane {
        let (batch, commands) = into_simple_batch_request(SimpleBatchOptions {
            cmd,
            cmds,
            cwd: effective_cwd,
            default_timeout_ms,
            max_parallel,
            continue_on_error,
            capture_limit_bytes,
            unfiltered,
        })?;
        return Ok(PreparedToolRequest::Simple {
            batch,
            commands,
            session_id: resolve_sticky_session(session_id, sticky),
            close: close.unwrap_or(false),
        });
    }

    let session_id = resolve_sticky_session(session_id, sticky);

    if script.is_none() && raw.is_none() && session_id.is_none() && !close.unwrap_or(false) {
        return Err(anyhow!(
            "mag requires `cmd`, `cmds`, `raw`, `script`, or `close` with `session_id`"
        ));
    }

    Ok(PreparedToolRequest::Toon(ToonRequest {
        script,
        raw,
        remote,
        session_id,
        // already folded into session_id above
        sticky: None,
        close,
        env,
        default_cwd: effective_cwd,
        default_timeout_ms,
        max_parallel,
        continue_on_error,
        capture_limit_bytes,
        unfiltered,
        warm,
    }))
}

fn execute_terminal_request(request: TerminalRequest) -> AppResult<String> {
    let action = request.action.unwrap_or(TerminalAction::Status);
    let session = terminal_session(request.session, request.seat);
    let socket = request.socket;
    match action {
        TerminalAction::Open => {
            let resolved = session
                .clone()
                .unwrap_or_else(|| "mag-house".to_string());
            // Naming a socket asks for the pane on THAT server; creating it on
            // our own uid's server would silently fork a twin of whatever the
            // caller is looking at. Resolve first, refuse to diverge.
            let server = crate::term::TmuxServer::for_session(&resolved, socket.as_deref())?;
            if server.probed {
                return Err(anyhow!(
                    "tmux session `{resolved}` already exists on another server; \
                     it is already drivable — no open needed (omit `socket` to use the probe)"
                ));
            }
            if socket.is_some() && !server.has_session(&resolved) {
                return Err(anyhow!(
                    "tmux session `{resolved}` not found on the requested socket; \
                     refusing to create a twin there"
                ));
            }
            crate::term::open(&crate::term::TermOpenOptions {
                session,
                cwd: request.cwd,
                no_wezterm: request.no_wezterm.unwrap_or(true),
                no_watch: request.no_watch.unwrap_or(false),
                socket,
            })?;
            Ok("terminal opened".to_string())
        }
        TerminalAction::Status => capture_stdout(|| {
            crate::term::status_text(&crate::term::TermTargetOptions { session, socket })
        }),
        TerminalAction::Send => {
            let input = request
                .input
                .ok_or_else(|| anyhow!("terminal action `send` requires `input`"))?;
            crate::term::send(&crate::term::TermSendOptions {
                session,
                input,
                enter: request.enter.unwrap_or(true),
                socket,
            })?;
            Ok("terminal input sent".to_string())
        }
        TerminalAction::Snapshot => capture_stdout(|| {
            crate::term::snapshot_text(&crate::term::TermSnapshotOptions {
                session,
                lines: request.lines.unwrap_or(80),
                socket,
            })
        }),
        TerminalAction::Close => {
            crate::term::close(&crate::term::TermTargetOptions { session, socket })?;
            Ok("terminal closed".to_string())
        }
    }
}

fn terminal_session(session: Option<String>, seat: Option<String>) -> Option<String> {
    session.or_else(|| {
        seat.map(|value| {
            let slug: String = value
                .chars()
                .map(|ch| {
                    if ch.is_ascii_alphanumeric() || ch == '-' || ch == '_' {
                        ch.to_ascii_lowercase()
                    } else {
                        '-'
                    }
                })
                .collect();
            format!("mag-{}", slug.trim_matches('-'))
        })
        .filter(|value| value != "mag-")
    })
}

fn capture_stdout<F>(f: F) -> AppResult<String>
where
    F: FnOnce() -> AppResult<String>,
{
    f()
}

struct SimpleBatchOptions {
    cmd: Option<String>,
    cmds: Option<Vec<String>>,
    cwd: Option<String>,
    default_timeout_ms: Option<u64>,
    max_parallel: Option<usize>,
    continue_on_error: Option<bool>,
    capture_limit_bytes: Option<usize>,
    unfiltered: Option<bool>,
}

fn into_simple_batch_request(
    options: SimpleBatchOptions,
) -> AppResult<(BatchRequest, Vec<String>)> {
    let commands = match (options.cmd, options.cmds) {
        (Some(command), None) => vec![validate_simple_command(command, "cmd")?],
        (None, Some(commands)) => {
            if commands.is_empty() {
                return Err(anyhow!("`cmds` must not be empty"));
            }
            if commands.len() > MAX_COMMANDS {
                return Err(anyhow!("`cmds` exceeds max of {}", MAX_COMMANDS));
            }

            let mut validated = Vec::with_capacity(commands.len());
            for (index, command) in commands.into_iter().enumerate() {
                validated.push(validate_simple_command(
                    command,
                    &format!("cmds[{}]", index),
                )?);
            }
            validated
        }
        (Some(_), Some(_)) => {
            return Err(anyhow!("mag accepts either `cmd` or `cmds`, not both"));
        }
        (None, None) => return Err(anyhow!("mag requires `cmd` or `cmds`")),
    };

    let batch = BatchRequest {
        commands: commands
            .iter()
            .enumerate()
            .map(|(index, command)| CommandSpec {
                id: Some(format!("cmd-{}", index + 1)),
                command: Some(command.clone()),
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
        default_cwd: options.cwd.filter(|value| !value.trim().is_empty()),
        default_timeout_ms: options.default_timeout_ms,
        max_parallel: options.max_parallel.or(Some(1)),
        continue_on_error: options.continue_on_error.or(Some(true)),
        capture_limit_bytes: options.capture_limit_bytes,
        unfiltered: options.unfiltered,
    };

    Ok((batch, commands))
}

fn validate_simple_command(command: String, field: &str) -> AppResult<String> {
    if command.trim().is_empty() {
        return Err(anyhow!("`{}` must not be blank", field));
    }

    Ok(command)
}

#[cfg(test)]
mod tests {
    use super::{BatchEngine, EngineConfig, execute_tool_request, terminal_session};
    use crate::cli::{BatchRequest, CommandSpec};
    use crate::mcp::ToolRequest;
    use std::collections::BTreeMap;

    fn engine() -> BatchEngine {
        BatchEngine::new(EngineConfig {
            workers: 1,
            default_timeout_ms: 10_000,
            capture_limit_bytes: 64 * 1024,
            node_registry: None,
            token_diet_enabled: true,
            token_diet_echo_max_bytes: 8 * 1024,
        })
        .unwrap()
    }

    fn noisy_batch(unfiltered: Option<bool>) -> BatchRequest {
        BatchRequest {
            commands: vec![CommandSpec {
                id: Some("noise".to_string()),
                command: Some("head -c 10000 /dev/zero | tr '\\0' x".to_string()),
                argv: None,
                remote: None,
                cwd: None,
                env: BTreeMap::new(),
                stdin: None,
                timeout_ms: None,
                warm: Some(false),
            }],
            env: BTreeMap::new(),
            remote: None,
            default_cwd: None,
            default_timeout_ms: None,
            max_parallel: Some(1),
            continue_on_error: Some(true),
            capture_limit_bytes: Some(12_000),
            unfiltered,
        }
    }

    #[test]
    fn default_diet_echo_cap_still_truncates_large_output() {
        let response = engine().run_batch(noisy_batch(None));
        let result = &response.results[0];

        assert!(result.success);
        assert!(result.stdout_truncated);
        assert!(result.stdout.as_deref().unwrap().len() < 9_000);
    }

    #[test]
    fn simple_lane_honors_capture_limit_bytes() {
        let request = serde_json::from_value::<ToolRequest>(serde_json::json!({
            "cmd": "seq 1 5000",
            "capture_limit_bytes": 300,
            "unfiltered": true
        }))
        .unwrap();
        let output = execute_tool_request(engine(), request).unwrap();

        assert!(output.contains("[truncated by mag"));
        assert!(output.len() < 700, "output was too large: {}", output.len());
    }

    #[test]
    fn batch_default_cwd_missing_fails_once() {
        let response = engine().run_batch(BatchRequest {
            commands: vec![
                CommandSpec {
                    id: Some("mkdir".to_string()),
                    command: Some("mkdir -p /tmp/mag-never-created-by-cwd-test".to_string()),
                    argv: None,
                    remote: None,
                    cwd: None,
                    env: BTreeMap::new(),
                    stdin: None,
                    timeout_ms: None,
                    warm: Some(false),
                },
                CommandSpec {
                    id: Some("write".to_string()),
                    command: Some("printf ok > file".to_string()),
                    argv: None,
                    remote: None,
                    cwd: None,
                    env: BTreeMap::new(),
                    stdin: None,
                    timeout_ms: None,
                    warm: Some(false),
                },
            ],
            env: BTreeMap::new(),
            remote: None,
            default_cwd: Some("/tmp/mag-cwd-does-not-exist-for-test".to_string()),
            default_timeout_ms: None,
            max_parallel: Some(1),
            continue_on_error: Some(true),
            capture_limit_bytes: None,
            unfiltered: Some(true),
        });

        assert_eq!(response.results.len(), 1);
        assert_eq!(response.results[0].mode, "validation");
        assert!(
            response.results[0]
                .error
                .as_deref()
                .unwrap()
                .contains("batch cwd")
        );
    }

    #[test]
    fn terminal_seat_maps_to_named_session() {
        assert_eq!(
            terminal_session(None, Some("Monica".to_string())).as_deref(),
            Some("mag-monica")
        );
        assert_eq!(
            terminal_session(None, Some("rogue seat".to_string())).as_deref(),
            Some("mag-rogue-seat")
        );
        assert_eq!(
            terminal_session(Some("custom".to_string()), Some("monica".to_string())).as_deref(),
            Some("custom")
        );
    }

    #[test]
    fn unfiltered_disables_diet_echo_cap_for_ground_zero_reads() {
        let response = engine().run_batch(noisy_batch(Some(true)));
        let result = &response.results[0];

        assert!(result.success);
        assert!(!result.stdout_truncated);
        assert_eq!(result.stdout.as_deref().unwrap().len(), 10_000);
    }
}
