use crate::AppResult;
use crate::cli::{BatchRequest, BatchResponse, CommandResult, CommandSpec, ExecOutcome};
use crate::exec::{DietConfig, merge_env, outcome_to_command_result, run_remote_shell};
use crate::pool::shell_single_quote;
use anyhow::anyhow;
use serde::Deserialize;
use std::collections::BTreeMap;
use std::time::Duration;

#[derive(Debug, Clone, Deserialize, Default)]
pub struct NodeRegistryFile {
    #[serde(default)]
    pub(crate) nodes: BTreeMap<String, NodeRegistryEntryFile>,
}

#[derive(Debug, Clone, Deserialize)]
pub(crate) struct NodeRegistryEntryFile {
    pub(crate) host: String,
    #[serde(default = "default_remote_http_port")]
    pub(crate) port: u16,
    #[serde(default)]
    pub(crate) auth_token: String,
}

#[derive(Debug, Clone)]
pub struct NodeRegistry {
    pub nodes: BTreeMap<String, RemoteHttpTarget>,
}

#[derive(Debug, Clone)]
pub struct RemoteHttpTarget {
    pub name: String,
    pub base_url: String,
    pub auth_token: Option<String>,
}

pub(crate) enum RemoteTransport {
    Http(RemoteHttpTarget),
    Ssh(String),
}

pub(crate) fn default_remote_http_port() -> u16 {
    8787
}

#[allow(clippy::too_many_arguments)]
pub(crate) fn run_remote_command_result(
    index: usize,
    spec: &CommandSpec,
    batch_env: &BTreeMap<String, String>,
    remote: &str,
    default_cwd: Option<&str>,
    default_timeout_ms: u64,
    capture_limit_bytes: usize,
    node_registry: Option<&NodeRegistry>,
    diet: DietConfig,
) -> CommandResult {
    let effective_cwd = spec.cwd.as_deref().or(default_cwd);
    let effective_timeout_ms = spec.timeout_ms.unwrap_or(default_timeout_ms);
    let effective_env = merge_env(batch_env, &spec.env);
    let effective_stdin = spec.stdin.as_deref();

    let transport = match resolve_remote_transport(remote, node_registry) {
        Ok(transport) => transport,
        Err(err) => {
            return outcome_to_command_result(
                index,
                spec.id.clone(),
                capture_limit_bytes,
                ExecOutcome {
                    mode: "validation".to_string(),
                    exit_code: None,
                    timed_out: false,
                    duration_ms: 0,
                    stdout: String::new(),
                    stderr: String::new(),
                    error: Some(err.to_string()),
                },
                None,
                diet,
            );
        }
    };

    match transport {
        RemoteTransport::Http(target) => run_remote_http_command_result(
            index,
            spec,
            batch_env,
            effective_cwd,
            effective_timeout_ms,
            capture_limit_bytes,
            target,
            diet,
        ),
        RemoteTransport::Ssh(target) => {
            let command_text = spec.command.clone();
            let outcome = match (&spec.argv, &spec.command) {
                (None, Some(command)) => run_remote_shell(
                    &target,
                    command,
                    effective_cwd,
                    &effective_env,
                    effective_stdin,
                    effective_timeout_ms,
                ),
                (Some(_), _) => ExecOutcome {
                    mode: "validation".to_string(),
                    exit_code: None,
                    timed_out: false,
                    duration_ms: 0,
                    stdout: String::new(),
                    stderr: String::new(),
                    error: Some(
                        "SSH remote jobs currently require `command`/`shell`; use node-registry or host:port HTTP forwarding for `argv`".to_string(),
                    ),
                },
                (None, None) => ExecOutcome {
                    mode: "validation".to_string(),
                    exit_code: None,
                    timed_out: false,
                    duration_ms: 0,
                    stdout: String::new(),
                    stderr: String::new(),
                    error: Some("remote job needs `command`/`shell`".to_string()),
                },
            };
            outcome_to_command_result(
                index,
                spec.id.clone(),
                capture_limit_bytes,
                outcome,
                command_text.as_deref(),
                diet,
            )
        }
    }
}

pub(crate) fn run_remote_http_command_result(
    index: usize,
    spec: &CommandSpec,
    batch_env: &BTreeMap<String, String>,
    effective_cwd: Option<&str>,
    effective_timeout_ms: u64,
    capture_limit_bytes: usize,
    target: RemoteHttpTarget,
    diet: DietConfig,
) -> CommandResult {
    let mut forwarded_spec = spec.clone();
    forwarded_spec.remote = None;
    let batch = BatchRequest {
        commands: vec![forwarded_spec],
        env: batch_env.clone(),
        remote: None,
        default_cwd: effective_cwd.map(ToOwned::to_owned),
        default_timeout_ms: Some(effective_timeout_ms),
        max_parallel: Some(1),
        continue_on_error: Some(true),
        capture_limit_bytes: Some(capture_limit_bytes),
        unfiltered: Some(!diet.enabled),
    };

    match run_remote_http_batch(&target, &batch, effective_timeout_ms) {
        Ok(response) => map_remote_batch_response(index, spec.id.clone(), &target, response),
        Err(err) => outcome_to_command_result(
            index,
            spec.id.clone(),
            capture_limit_bytes,
            ExecOutcome {
                mode: format!("remote-http:{}", target.name),
                exit_code: None,
                timed_out: false,
                duration_ms: 0,
                stdout: String::new(),
                stderr: String::new(),
                error: Some(err.to_string()),
            },
            None,
            diet,
        ),
    }
}

pub(crate) fn run_remote_http_batch(
    target: &RemoteHttpTarget,
    batch: &BatchRequest,
    timeout_ms: u64,
) -> AppResult<BatchResponse> {
    let mut builder = reqwest::blocking::Client::builder();
    if timeout_ms > 0 {
        builder = builder.timeout(Duration::from_millis(timeout_ms.saturating_add(5_000)));
    }
    let client = builder.build()?;

    let mut request = client
        .post(format!("{}/toon", target.base_url))
        .header("content-type", "application/json")
        .json(batch);
    if let Some(token) = target.auth_token.as_deref() {
        request = request.header("x-mag-token", token);
    }

    let response = request.send()?;
    let status = response.status();
    let body = response.text()?;
    if !status.is_success() {
        return Err(anyhow!(
            "remote `{}` HTTP {}: {}",
            target.name,
            status.as_u16(),
            extract_remote_http_error(&body)
        ));
    }

    Ok(serde_json::from_str::<BatchResponse>(&body)?)
}

pub(crate) fn map_remote_batch_response(
    index: usize,
    default_id: Option<String>,
    target: &RemoteHttpTarget,
    response: BatchResponse,
) -> CommandResult {
    let mut results = response.results;
    if results.len() != 1 {
        return CommandResult {
            index,
            id: default_id,
            mode: format!("remote-http:{}", target.name),
            success: false,
            exit_code: None,
            timed_out: false,
            duration_ms: response.elapsed_ms,
            stdout: None,
            stderr: None,
            stdout_truncated: false,
            stderr_truncated: false,
            diet_applied: false,
            exec_user: None,
            error: Some(format!(
                "remote `{}` returned {} results for a single forwarded command",
                target.name,
                results.len()
            )),
        };
    }

    let mut result = results.remove(0);
    result.index = index;
    if result.id.is_none() {
        result.id = default_id;
    }
    result.mode = format!("remote-http:{}:{}", target.name, result.mode);
    result
}

pub(crate) fn resolve_remote_transport(
    remote: &str,
    node_registry: Option<&NodeRegistry>,
) -> AppResult<RemoteTransport> {
    let trimmed = remote.trim();
    if trimmed.is_empty() {
        return Err(anyhow!("remote must not be empty"));
    }

    if let Some(registry) = node_registry
        && let Some(target) = registry.nodes.get(trimmed)
    {
        return Ok(RemoteTransport::Http(target.clone()));
    }

    if trimmed.starts_with("http://") || trimmed.starts_with("https://") {
        return Ok(RemoteTransport::Http(RemoteHttpTarget {
            name: trimmed.to_string(),
            base_url: trimmed.trim_end_matches('/').to_string(),
            auth_token: None,
        }));
    }

    if looks_like_host_port(trimmed) {
        return Ok(RemoteTransport::Http(RemoteHttpTarget {
            name: trimmed.to_string(),
            base_url: format!("http://{}", trimmed),
            auth_token: None,
        }));
    }

    Ok(RemoteTransport::Ssh(trimmed.to_string()))
}

pub(crate) fn looks_like_host_port(value: &str) -> bool {
    if value.contains('@') || value.contains('/') {
        return false;
    }

    let Some((host, port)) = value.rsplit_once(':') else {
        return false;
    };
    !host.is_empty() && port.parse::<u16>().is_ok()
}

pub(crate) fn extract_remote_http_error(body: &str) -> String {
    serde_json::from_str::<serde_json::Value>(body)
        .ok()
        .and_then(|value| {
            value
                .get("error")
                .and_then(|error| error.as_str())
                .map(ToOwned::to_owned)
        })
        .unwrap_or_else(|| body.trim().to_string())
}

pub(crate) fn build_remote_script(
    command_text: &str,
    cwd: Option<&str>,
    env: &BTreeMap<String, String>,
) -> String {
    let mut script = String::new();

    if let Some(path) = cwd {
        script.push_str("cd ");
        script.push_str(&shell_single_quote(path));
        script.push('\n');
    }

    for (key, value) in env {
        script.push_str("export ");
        script.push_str(key);
        script.push('=');
        script.push_str(&shell_single_quote(value));
        script.push('\n');
    }

    script.push_str(command_text);
    if !command_text.ends_with('\n') {
        script.push('\n');
    }

    script
}
