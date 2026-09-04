use crate::AppResult;
use crate::cli::{CommandResult, ExecOutcome, LimitedOutput};
use crate::diet::{DietRegistry, apply_filter, never_worse};
use crate::remote::build_remote_script;
use std::collections::BTreeMap;
use std::process::{Child, Command, ExitStatus, Stdio};
use std::thread;
use std::time::{Duration, Instant};

pub(crate) fn run_exec(
    argv: &[String],
    cwd: Option<&str>,
    env: &BTreeMap<String, String>,
    stdin_text: Option<&str>,
    timeout_ms: u64,
) -> ExecOutcome {
    if argv.is_empty() {
        return ExecOutcome {
            mode: "exec".to_string(),
            exit_code: None,
            timed_out: false,
            duration_ms: 0,
            stdout: String::new(),
            stderr: String::new(),
            error: Some("argv must not be empty".to_string()),
        };
    }

    let mut command = Command::new(&argv[0]);
    if argv.len() > 1 {
        command.args(&argv[1..]);
    }
    if let Some(path) = cwd {
        command.current_dir(path);
    }
    apply_command_env(&mut command, env);
    command
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());

    run_spawned(command, "exec", stdin_text, timeout_ms)
}

pub(crate) fn run_fresh_shell(
    command_text: &str,
    cwd: Option<&str>,
    env: &BTreeMap<String, String>,
    stdin_text: Option<&str>,
    timeout_ms: u64,
) -> ExecOutcome {
    let mut command = Command::new("bash");
    command
        .arg("--noprofile")
        .arg("--norc")
        .arg("-lc")
        .arg(command_text)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    if let Some(path) = cwd {
        command.current_dir(path);
    }
    apply_command_env(&mut command, env);

    run_spawned(command, "fresh-shell", stdin_text, timeout_ms)
}

pub(crate) fn run_remote_shell(
    remote: &str,
    command_text: &str,
    cwd: Option<&str>,
    env: &BTreeMap<String, String>,
    stdin_text: Option<&str>,
    timeout_ms: u64,
) -> ExecOutcome {
    if remote.trim().is_empty() {
        return ExecOutcome {
            mode: "validation".to_string(),
            exit_code: None,
            timed_out: false,
            duration_ms: 0,
            stdout: String::new(),
            stderr: String::new(),
            error: Some("remote must not be empty".to_string()),
        };
    }
    if stdin_text.is_some() {
        return ExecOutcome {
            mode: "validation".to_string(),
            exit_code: None,
            timed_out: false,
            duration_ms: 0,
            stdout: String::new(),
            stderr: String::new(),
            error: Some("remote jobs do not support `stdin` yet; inline data into the remote `script` for now".to_string()),
        };
    }

    let remote_script = build_remote_script(command_text, cwd, env);
    let mut command = Command::new("ssh");
    command
        .arg("-T")
        .arg(remote)
        .arg("bash")
        .arg("--noprofile")
        .arg("--norc")
        .arg("-s")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    run_spawned(command, "remote-shell", Some(&remote_script), timeout_ms)
}

pub(crate) fn run_spawned(
    mut command: Command,
    mode: &str,
    stdin_text: Option<&str>,
    timeout_ms: u64,
) -> ExecOutcome {
    let started = Instant::now();
    let mut child = match command.spawn() {
        Ok(child) => child,
        Err(err) => {
            return ExecOutcome {
                mode: mode.to_string(),
                exit_code: None,
                timed_out: false,
                duration_ms: 0,
                stdout: String::new(),
                stderr: String::new(),
                error: Some(format!("spawn failed: {}", err)),
            };
        }
    };

    if let Some(mut stdin) = child.stdin.take() {
        if let Some(input) = stdin_text
            && let Err(err) = std::io::Write::write_all(&mut stdin, input.as_bytes())
        {
            let _ = child.kill();
            return ExecOutcome {
                mode: mode.to_string(),
                exit_code: None,
                timed_out: false,
                duration_ms: started.elapsed().as_millis(),
                stdout: String::new(),
                stderr: String::new(),
                error: Some(format!("stdin write failed: {}", err)),
            };
        }
        drop(stdin);
    }

    let (output, timed_out) = match wait_with_timeout(child, timeout_ms) {
        Ok(value) => value,
        Err(err) => {
            return ExecOutcome {
                mode: mode.to_string(),
                exit_code: None,
                timed_out: false,
                duration_ms: started.elapsed().as_millis(),
                stdout: String::new(),
                stderr: String::new(),
                error: Some(format!("execution failed: {}", err)),
            };
        }
    };

    ExecOutcome {
        mode: mode.to_string(),
        exit_code: exit_code_from_status(&output.status, timed_out),
        timed_out,
        duration_ms: started.elapsed().as_millis(),
        stdout: String::from_utf8_lossy(&output.stdout).to_string(),
        stderr: String::from_utf8_lossy(&output.stderr).to_string(),
        error: None,
    }
}

pub(crate) fn wait_with_timeout(
    mut child: Child,
    timeout_ms: u64,
) -> AppResult<(std::process::Output, bool)> {
    if timeout_ms == 0 {
        return Ok((child.wait_with_output()?, false));
    }

    let started = Instant::now();
    let mut timed_out = false;
    loop {
        if child.try_wait()?.is_some() {
            break;
        }
        if started.elapsed() >= Duration::from_millis(timeout_ms) {
            timed_out = true;
            let _ = child.kill();
            break;
        }
        thread::sleep(Duration::from_millis(10));
    }

    Ok((child.wait_with_output()?, timed_out))
}

pub(crate) fn exit_code_from_status(status: &ExitStatus, timed_out: bool) -> Option<i32> {
    status
        .code()
        .or(if timed_out { Some(124) } else { Some(-1) })
}

/// Token-diet knobs, threaded through from EngineConfig (daemon config).
/// `echo_max_bytes` is the hard per-stream cap applied after the filter pass.
#[derive(Debug, Clone, Copy)]
pub(crate) struct DietConfig {
    pub enabled: bool,
    pub echo_max_bytes: usize,
}

/// The single chokepoint every lane funnels through (local warm/fresh/exec,
/// remote). Token-diet lives here: rtk-style filter on stdout+stderr, the
/// never_worse guard as the hard safety floor, then the echo cap, then the
/// capture limit. Order matters — the filter shrinks before any cap cuts,
/// so a truncation marker is only ever a real overflow, not diet damage.
pub(crate) fn outcome_to_command_result(
    index: usize,
    id: Option<String>,
    capture_limit_bytes: usize,
    outcome: ExecOutcome,
    command_text: Option<&str>,
    diet: DietConfig,
) -> CommandResult {
    let mut diet_applied = false;
    let mut stdout_raw = outcome.stdout.as_str();
    let mut stderr_raw = outcome.stderr.as_str();
    // Filtered candidates live outside the branch so never_worse's pick
    // (either side of the guard) borrows something that outlives limit_output.
    let stdout_filtered;
    let stderr_filtered;

    if diet.enabled
        && let Some(command) = command_text
        && let Some(filter) = DietRegistry::shared().find_filter(command)
    {
        stdout_filtered = apply_filter(filter, stdout_raw);
        stdout_raw = never_worse(stdout_raw, &stdout_filtered);
        stderr_filtered = apply_filter(filter, stderr_raw);
        stderr_raw = never_worse(stderr_raw, &stderr_filtered);
        diet_applied = true;
    }

    let stdout = limit_output(stdout_raw, capture_limit_bytes, diet.echo_max_bytes);
    let stderr = limit_output(stderr_raw, capture_limit_bytes, diet.echo_max_bytes);
    let success = outcome.error.is_none() && outcome.exit_code == Some(0) && !outcome.timed_out;

    CommandResult {
        index,
        id,
        mode: outcome.mode,
        success,
        exit_code: outcome.exit_code,
        timed_out: outcome.timed_out,
        duration_ms: outcome.duration_ms,
        stdout: stdout.value,
        stderr: stderr.value,
        stdout_truncated: stdout.truncated,
        stderr_truncated: stderr.truncated,
        diet_applied,
        exec_user: None,
        error: outcome.error,
    }
}

/// Two caps, one marker. `capture_limit_bytes` is the old safety ceiling
/// (64 KiB default); `echo_max_bytes` is the token-diet echo cap (8 KiB
/// default). Whichever bites first writes the byte-count marker, so the seat
/// always sees how much was held back (rtk's tee-recovery hint, inlined).
pub(crate) fn limit_output(
    value: &str,
    capture_limit_bytes: usize,
    echo_max_bytes: usize,
) -> LimitedOutput {
    if value.is_empty() {
        return LimitedOutput {
            value: None,
            truncated: false,
        };
    }
    let limit_bytes = capture_limit_bytes.min(echo_max_bytes).max(1);
    if value.len() <= limit_bytes {
        return LimitedOutput {
            value: Some(value.to_string()),
            truncated: false,
        };
    }

    let mut end = 0usize;
    for (idx, _) in value.char_indices() {
        if idx > limit_bytes {
            break;
        }
        end = idx;
    }
    if end == 0 {
        end = value
            .char_indices()
            .next()
            .map(|(idx, ch)| idx + ch.len_utf8())
            .unwrap_or(0);
    }
    let held = value.len() - end;
    let mut truncated = value[..end].to_string();
    truncated.push_str(&format!("\n[truncated by mag — {} more bytes]", held));
    LimitedOutput {
        value: Some(truncated),
        truncated: true,
    }
}

pub(crate) fn merge_env(
    batch_env: &BTreeMap<String, String>,
    command_env: &BTreeMap<String, String>,
) -> BTreeMap<String, String> {
    let mut merged = batch_env.clone();
    for (key, value) in command_env {
        merged.insert(key.clone(), value.clone());
    }
    merged
}

pub(crate) fn apply_command_env(command: &mut Command, env: &BTreeMap<String, String>) {
    for (key, value) in env {
        command.env(key, value);
    }
}
