use crate::AppResult;
use crate::cli::ExecOutcome;
use anyhow::anyhow;
use std::collections::BTreeMap;
use std::fs;
use std::process::{Child, ChildStdin, ChildStdout, Command, Stdio};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

pub(crate) struct WarmShellPool {
    workers: Vec<Arc<Mutex<WarmShellWorker>>>,
    cursor: AtomicUsize,
    named_sessions: Mutex<BTreeMap<String, NamedSession>>,
}

/// A named session pins one worker slot to one seat. `last_used` feeds the
/// idle reaper (step 4): sessions idle past the configured timeout are
/// evicted and their workers restarted fresh.
#[derive(Debug, Clone, Copy)]
pub(crate) struct NamedSession {
    pub slot: usize,
    pub last_used: Instant,
    /// When the session first claimed its slot. Distinct from `last_used`: age
    /// says the shell has survived, idle says how close it is to being reaped.
    pub created_at: Instant,
}

/// Read-only view of one named session — enough to answer "is this alive, and
/// what is it?" without running a command in it and reading back `$$`.
#[derive(Debug, Clone, serde::Serialize)]
pub struct SessionStatus {
    pub session_id: String,
    pub slot: usize,
    /// The warm shell's pid, or `None` if the shell is gone (which is itself
    /// the answer: the session key outlived its process).
    pub pid: Option<u32>,
    pub age_ms: u128,
    pub idle_ms: u128,
}

struct WarmShellWorker {
    slot: usize,
    child: Child,
    stdin: ChildStdin,
    stdout: std::io::BufReader<ChildStdout>,
}

#[derive(Debug)]
struct WarmShellMarker {
    exit_code: i32,
    timed_out: bool,
    stdout_path: String,
    stderr_path: String,
}

impl WarmShellPool {
    pub(crate) fn new(count: usize) -> AppResult<Self> {
        let mut workers: Vec<Arc<Mutex<WarmShellWorker>>> = Vec::with_capacity(count);
        for slot in 0..count {
            match WarmShellWorker::new(slot) {
                Ok(worker) => workers.push(Arc::new(Mutex::new(worker))),
                Err(err) => {
                    // Q3: don't orphan already-spawned bash children on partial
                    // spawn failure — kill them before returning the error.
                    for worker in &workers {
                        if let Ok(mut guard) = worker.lock() {
                            guard.kill_reap();
                        }
                    }
                    return Err(err.context(format!(
                        "warm pool spawn failed at slot {slot}/{count} (earlier workers reaped)"
                    )));
                }
            }
        }
        Ok(Self {
            workers,
            cursor: AtomicUsize::new(0),
            named_sessions: Mutex::new(BTreeMap::new()),
        })
    }

    #[allow(dead_code)] // used by /health in step 5
    pub(crate) fn len(&self) -> usize {
        self.workers.len()
    }

    #[allow(dead_code)] // used by /health + reaper in step 4/5
    pub(crate) fn named_session_count(&self) -> usize {
        self.named_sessions
            .lock()
            .map(|guard| guard.len())
            .unwrap_or(0)
    }

    /// Claim a slot for `{seat}:{session_id}` — the seat namespace keeps one
    /// seat's `work` session from colliding with another seat's `work`.
    /// Re-claiming an existing session just refreshes its idle clock.
    pub(crate) fn claim_named(&self, seat: &str, session_id: &str) -> AppResult<usize> {
        let key = session_key(seat, session_id);
        let mut guard = self
            .named_sessions
            .lock()
            .map_err(|_| anyhow!("named session map poisoned"))?;
        if let Some(session) = guard.get_mut(&key) {
            let slot = session.slot;
            session.last_used = Instant::now();
            drop(guard);
            // A timed-out command may have taken the shell down with it
            // (watchdog kill-$$). Restart transparently instead of letting
            // the next call eat a broken pipe.
            let worker = Arc::clone(&self.workers[slot]);
            if let Ok(mut worker_guard) = worker.lock() {
                worker_guard.ensure_alive()?;
            }
            return Ok(slot);
        }

        let start = self.cursor.fetch_add(1, Ordering::Relaxed) % self.workers.len();
        for offset in 0..self.workers.len() {
            let slot = (start + offset) % self.workers.len();
            if !guard.values().any(|claimed| claimed.slot == slot) {
                let now = Instant::now();
                guard.insert(
                    key,
                    NamedSession {
                        slot,
                        last_used: now,
                        created_at: now,
                    },
                );
                return Ok(slot);
            }
        }

        Err(anyhow!(
            "no free warm shell available to claim for session `{}`",
            session_id
        ))
    }

    /// Status of every session this seat holds. Seat-scoped on purpose: a seat
    /// can see its own shells and no one else's, same fence as `release_named`.
    pub(crate) fn session_statuses(&self, seat: &str) -> Vec<SessionStatus> {
        let prefix = format!("{seat}:");
        let snapshot: Vec<(String, NamedSession)> = match self.named_sessions.lock() {
            Ok(guard) => guard
                .iter()
                .filter(|(key, _)| key.starts_with(&prefix))
                .map(|(key, session)| (key[prefix.len()..].to_string(), *session))
                .collect(),
            Err(_) => return Vec::new(),
        };

        snapshot
            .into_iter()
            .map(|(session_id, session)| SessionStatus {
                session_id,
                slot: session.slot,
                pid: self.worker_pid(session.slot),
                age_ms: session.created_at.elapsed().as_millis(),
                idle_ms: session.last_used.elapsed().as_millis(),
            })
            .collect()
    }

    /// Live pid of the shell in `slot`. `None` means the child has exited — the
    /// honest answer, never a guess from the bookkeeping map.
    fn worker_pid(&self, slot: usize) -> Option<u32> {
        let worker = self.workers.get(slot)?;
        let mut guard = worker.lock().ok()?;
        match guard.child.try_wait() {
            Ok(None) => Some(guard.child.id()),
            _ => None,
        }
    }

    /// Does `<seat>:<session_id>` currently hold a slot?
    pub(crate) fn has_session(&self, seat: &str, session_id: &str) -> bool {
        let key = session_key(seat, session_id);
        self.named_sessions
            .lock()
            .map(|guard| guard.contains_key(&key))
            .unwrap_or(false)
    }

    /// Release a session, but only if the caller's seat owns it.
    pub(crate) fn release_named(&self, seat: &str, session_id: &str) -> AppResult<bool> {
        let key = session_key(seat, session_id);
        self.release_slot(&key)
    }

    fn release_slot(&self, key: &str) -> AppResult<bool> {
        let slot = {
            let mut guard = self
                .named_sessions
                .lock()
                .map_err(|_| anyhow!("named session map poisoned"))?;
            guard.remove(key).map(|session| session.slot)
        };

        let Some(slot) = slot else {
            return Ok(false);
        };

        let worker = Arc::clone(&self.workers[slot]);
        match worker.lock() {
            Ok(mut guard) => {
                guard.restart()?;
                Ok(true)
            }
            Err(_) => Err(anyhow!("warm shell lock poisoned")),
        }
    }

    /// Evict sessions idle longer than `idle_timeout`; each evicted worker is
    /// restarted so the next claimer gets a pristine shell. Returns the keys
    /// of reaped sessions (for daemon logs).
    pub(crate) fn reap_idle_sessions(&self, idle_timeout: Duration) -> Vec<String> {
        let stale: Vec<String> = match self.named_sessions.lock() {
            Ok(guard) => guard
                .iter()
                .filter(|(_, session)| session.last_used.elapsed() > idle_timeout)
                .map(|(key, _)| key.clone())
                .collect(),
            Err(_) => return Vec::new(),
        };

        let mut reaped = Vec::new();
        for key in stale {
            match self.release_slot(&key) {
                Ok(true) => reaped.push(key),
                Ok(false) | Err(_) => {}
            }
        }
        reaped
    }

    pub(crate) fn run(
        &self,
        command: &str,
        cwd: Option<&str>,
        env: &BTreeMap<String, String>,
        stdin_text: Option<&str>,
        timeout_ms: u64,
    ) -> ExecOutcome {
        let slot = match self.next_unclaimed_slot() {
            Ok(Some(slot)) => slot,
            Ok(None) => {
                return ExecOutcome {
                    mode: "warm-shell".to_string(),
                    exit_code: None,
                    timed_out: false,
                    duration_ms: 0,
                    stdout: String::new(),
                    stderr: String::new(),
                    error: Some("no unclaimed warm shell available".to_string()),
                };
            }
            Err(err) => {
                return ExecOutcome {
                    mode: "warm-shell".to_string(),
                    exit_code: None,
                    timed_out: false,
                    duration_ms: 0,
                    stdout: String::new(),
                    stderr: String::new(),
                    error: Some(err.to_string()),
                };
            }
        };
        let worker = Arc::clone(&self.workers[slot]);
        match worker.lock() {
            Ok(mut guard) => guard.run(command, cwd, env, stdin_text, timeout_ms),
            Err(_) => ExecOutcome {
                mode: "warm-shell".to_string(),
                exit_code: None,
                timed_out: false,
                duration_ms: 0,
                stdout: String::new(),
                stderr: String::new(),
                error: Some("warm shell lock poisoned".to_string()),
            },
        }
    }

    pub(crate) fn run_named_on_slot(
        &self,
        slot: usize,
        command: &str,
        cwd: Option<&str>,
        env: &BTreeMap<String, String>,
        stdin_text: Option<&str>,
        timeout_ms: u64,
    ) -> ExecOutcome {
        let worker = Arc::clone(&self.workers[slot]);
        match worker.lock() {
            Ok(mut guard) => guard.run_persistent(command, cwd, env, stdin_text, timeout_ms),
            Err(_) => ExecOutcome {
                mode: "persistent-shell".to_string(),
                exit_code: None,
                timed_out: false,
                duration_ms: 0,
                stdout: String::new(),
                stderr: String::new(),
                error: Some("warm shell lock poisoned".to_string()),
            },
        }
    }

    fn next_unclaimed_slot(&self) -> AppResult<Option<usize>> {
        let guard = self
            .named_sessions
            .lock()
            .map_err(|_| anyhow!("named session map poisoned"))?;
        if guard.len() >= self.workers.len() {
            return Ok(None);
        }

        let start = self.cursor.fetch_add(1, Ordering::Relaxed) % self.workers.len();
        for offset in 0..self.workers.len() {
            let slot = (start + offset) % self.workers.len();
            if !guard.values().any(|claimed| claimed.slot == slot) {
                return Ok(Some(slot));
            }
        }

        Ok(None)
    }
}

impl WarmShellWorker {
    fn new(slot: usize) -> AppResult<Self> {
        let mut child = Command::new("bash")
            .arg("--noprofile")
            .arg("--norc")
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()?;

        let stdin = child
            .stdin
            .take()
            .ok_or_else(|| anyhow!("missing bash stdin"))?;
        let stdout = child
            .stdout
            .take()
            .ok_or_else(|| anyhow!("missing bash stdout"))?;

        Ok(Self {
            slot,
            child,
            stdin,
            stdout: std::io::BufReader::new(stdout),
        })
    }

    /// Best-effort kill + reap of the bash child. Idempotent-ish: killing an
    /// already-dead child just errors quietly, which we ignore.
    fn kill_reap(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }

    /// If the bash child has exited (e.g. watchdog kill-$$ after a session
    /// timeout), restart it in place. Cheap no-op when alive.
    fn ensure_alive(&mut self) -> AppResult<()> {
        match self.child.try_wait() {
            Ok(Some(_)) => self.restart(), // exited — respawn
            Ok(None) => Ok(()),            // still running
            Err(_) => self.restart(),      // can't tell — safest to respawn
        }
    }

    fn restart(&mut self) -> AppResult<()> {
        self.kill_reap();
        let replacement = Self::new(self.slot)?;
        *self = replacement;
        Ok(())
    }

    fn run(
        &mut self,
        command: &str,
        cwd: Option<&str>,
        env: &BTreeMap<String, String>,
        stdin_text: Option<&str>,
        timeout_ms: u64,
    ) -> ExecOutcome {
        let started = Instant::now();
        let token = make_token(self.slot);
        let script =
            match build_warm_shell_script(&token, command, cwd, env, stdin_text, timeout_ms) {
                Ok(script) => script,
                Err(err) => {
                    return ExecOutcome {
                        mode: "warm-shell".to_string(),
                        exit_code: None,
                        timed_out: false,
                        duration_ms: started.elapsed().as_millis(),
                        stdout: String::new(),
                        stderr: String::new(),
                        error: Some(err.to_string()),
                    };
                }
            };

        if let Err(err) = std::io::Write::write_all(&mut self.stdin, script.as_bytes()) {
            let _ = self.restart();
            return ExecOutcome {
                mode: "warm-shell".to_string(),
                exit_code: None,
                timed_out: false,
                duration_ms: started.elapsed().as_millis(),
                stdout: String::new(),
                stderr: String::new(),
                error: Some(format!("failed to write to warm shell: {}", err)),
            };
        }
        if let Err(err) = std::io::Write::flush(&mut self.stdin) {
            let _ = self.restart();
            return ExecOutcome {
                mode: "warm-shell".to_string(),
                exit_code: None,
                timed_out: false,
                duration_ms: started.elapsed().as_millis(),
                stdout: String::new(),
                stderr: String::new(),
                error: Some(format!("failed to flush warm shell: {}", err)),
            };
        }

        match self.read_marker(&token) {
            Ok(marker) => {
                let stdout = fs::read_to_string(&marker.stdout_path).unwrap_or_default();
                let stderr = fs::read_to_string(&marker.stderr_path).unwrap_or_default();
                let _ = fs::remove_file(&marker.stdout_path);
                let _ = fs::remove_file(&marker.stderr_path);
                ExecOutcome {
                    mode: "warm-shell".to_string(),
                    exit_code: Some(marker.exit_code),
                    timed_out: marker.timed_out,
                    duration_ms: started.elapsed().as_millis(),
                    stdout,
                    stderr,
                    error: None,
                }
            }
            Err(err) => {
                let _ = self.restart();
                ExecOutcome {
                    mode: "warm-shell".to_string(),
                    exit_code: None,
                    timed_out: false,
                    duration_ms: started.elapsed().as_millis(),
                    stdout: String::new(),
                    stderr: String::new(),
                    error: Some(format!("warm shell failed: {}", err)),
                }
            }
        }
    }

    fn run_persistent(
        &mut self,
        command: &str,
        cwd: Option<&str>,
        env: &BTreeMap<String, String>,
        stdin_text: Option<&str>,
        timeout_ms: u64,
    ) -> ExecOutcome {
        let started = Instant::now();
        let token = make_token(self.slot);
        let script = match build_persistent_shell_script(
            &token, command, cwd, env, stdin_text, timeout_ms,
        ) {
            Ok(script) => script,
            Err(err) => {
                return ExecOutcome {
                    mode: "persistent-shell".to_string(),
                    exit_code: None,
                    timed_out: false,
                    duration_ms: started.elapsed().as_millis(),
                    stdout: String::new(),
                    stderr: String::new(),
                    error: Some(err.to_string()),
                };
            }
        };

        if let Err(err) = std::io::Write::write_all(&mut self.stdin, script.as_bytes()) {
            let _ = self.restart();
            return ExecOutcome {
                mode: "persistent-shell".to_string(),
                exit_code: None,
                timed_out: false,
                duration_ms: started.elapsed().as_millis(),
                stdout: String::new(),
                stderr: String::new(),
                error: Some(format!("failed to write to warm shell: {}", err)),
            };
        }
        if let Err(err) = std::io::Write::flush(&mut self.stdin) {
            let _ = self.restart();
            return ExecOutcome {
                mode: "persistent-shell".to_string(),
                exit_code: None,
                timed_out: false,
                duration_ms: started.elapsed().as_millis(),
                stdout: String::new(),
                stderr: String::new(),
                error: Some(format!("failed to flush warm shell: {}", err)),
            };
        }

        match self.read_marker(&token) {
            Ok(marker) => {
                let stdout = fs::read_to_string(&marker.stdout_path).unwrap_or_default();
                let stderr = fs::read_to_string(&marker.stderr_path).unwrap_or_default();
                let _ = fs::remove_file(&marker.stdout_path);
                let _ = fs::remove_file(&marker.stderr_path);
                ExecOutcome {
                    mode: "persistent-shell".to_string(),
                    exit_code: Some(marker.exit_code),
                    timed_out: marker.timed_out,
                    duration_ms: started.elapsed().as_millis(),
                    stdout,
                    stderr,
                    error: None,
                }
            }
            Err(err) => {
                let _ = self.restart();
                ExecOutcome {
                    mode: "persistent-shell".to_string(),
                    exit_code: None,
                    timed_out: false,
                    duration_ms: started.elapsed().as_millis(),
                    stdout: String::new(),
                    stderr: String::new(),
                    error: Some(format!("persistent shell failed: {}", err)),
                }
            }
        }
    }

    fn read_marker(&mut self, token: &str) -> AppResult<WarmShellMarker> {
        loop {
            let mut line = String::new();
            let read = std::io::BufRead::read_line(&mut self.stdout, &mut line)?;
            if read == 0 {
                return Err(anyhow!("warm shell closed its stdout"));
            }
            if let Some(marker) = parse_marker(&line, token) {
                return Ok(marker);
            }
        }
    }
}

impl Drop for WarmShellWorker {
    /// Q4: daemon shutdown / pool drop must never orphan bash children.
    /// (bash would exit on stdin EOF anyway, but a crashed/hung worker
    /// deserves an explicit reaping.)
    fn drop(&mut self) {
        self.kill_reap();
    }
}

fn build_warm_shell_script(
    token: &str,
    command: &str,
    cwd: Option<&str>,
    env: &BTreeMap<String, String>,
    stdin_text: Option<&str>,
    timeout_ms: u64,
) -> AppResult<String> {
    let heredoc_cwd = build_heredoc("__sir_cwd", token, cwd.unwrap_or(""));
    let heredoc_cmd = build_heredoc("__sir_cmd", token, command);
    let env_exports = build_env_exports(env)?;
    let stdin_setup = if let Some(input) = stdin_text {
        build_file_heredoc("__sir_in", token, input)
    } else {
        "__sir_in=''\n".to_string()
    };
    let stdin_redirect = if stdin_text.is_some() {
        "<\"$__sir_in\" "
    } else {
        ""
    };
    let stdin_cleanup = if stdin_text.is_some() {
        "rm -f \"$__sir_in\"\n"
    } else {
        ""
    };
    let timeout_secs = if timeout_ms == 0 {
        "0".to_string()
    } else {
        format!("{:.3}", timeout_ms as f64 / 1000.0)
    };

    Ok(format!(
        "{heredoc_cwd}{heredoc_cmd}{stdin_setup}__sir_out=$(mktemp)\n\
__sir_err=$(mktemp)\n\
__sir_flag=$(mktemp)\n\
rm -f \"$__sir_flag\"\n\
(\n\
  if [ -n \"$__sir_cwd\" ]; then\n\
    cd \"$__sir_cwd\" || exit 200\n\
  fi\n\
{env_exports}  eval \"$__sir_cmd\"\n\
) {stdin_redirect}>\"$__sir_out\" 2>\"$__sir_err\" &\n\
__sir_job=$!\n\
__sir_watch=''\n\
if [ {timeout_ms} -gt 0 ]; then\n\
  (\n\
    sleep {timeout_secs}\n\
    printf '1' >\"$__sir_flag\"\n\
    kill -TERM \"$__sir_job\" 2>/dev/null || true\n\
    sleep 0.2\n\
    kill -KILL \"$__sir_job\" 2>/dev/null || true\n\
  ) &\n\
  __sir_watch=$!\n\
fi\n\
wait \"$__sir_job\"\n\
__sir_rc=$?\n\
if [ -n \"$__sir_watch\" ]; then\n\
  kill \"$__sir_watch\" 2>/dev/null || true\n\
  wait \"$__sir_watch\" 2>/dev/null || true\n\
fi\n\
__sir_timed_out=0\n\
if [ -s \"$__sir_flag\" ]; then\n\
  __sir_timed_out=1\n\
fi\n\
rm -f \"$__sir_flag\"\n\
{stdin_cleanup}\
printf '__SIR_DONE__ {token} %s %s %s %s\\n' \"$__sir_rc\" \"$__sir_timed_out\" \"$__sir_out\" \"$__sir_err\"\n",
        timeout_ms = timeout_ms,
        timeout_secs = timeout_secs,
        env_exports = env_exports,
        stdin_cleanup = stdin_cleanup,
        stdin_redirect = stdin_redirect,
        stdin_setup = stdin_setup,
        token = token,
    ))
}

fn build_persistent_shell_script(
    token: &str,
    command: &str,
    cwd: Option<&str>,
    env: &BTreeMap<String, String>,
    stdin_text: Option<&str>,
    timeout_ms: u64,
) -> AppResult<String> {
    let heredoc_cwd = build_heredoc("__sir_cwd", token, cwd.unwrap_or(""));
    let heredoc_cmd = build_heredoc("__sir_cmd", token, command);
    let env_exports = build_env_exports(env)?;
    let stdin_setup = if let Some(input) = stdin_text {
        build_file_heredoc("__sir_in", token, input)
    } else {
        "__sir_in=''\n".to_string()
    };
    let stdin_redirect = if stdin_text.is_some() {
        "<\"$__sir_in\" "
    } else {
        ""
    };
    let stdin_cleanup = if stdin_text.is_some() {
        "rm -f \"$__sir_in\"\n"
    } else {
        ""
    };
    let timeout_secs = if timeout_ms == 0 {
        "0".to_string()
    } else {
        format!("{:.3}", timeout_ms as f64 / 1000.0)
    };

    // Q1: persistent lane now carries the same in-band watchdog as the
    // ephemeral lane — a `sleep infinity` in a named session can no longer
    // pin the worker forever.
    Ok(format!(
        "{heredoc_cwd}{heredoc_cmd}{stdin_setup}__sir_out=$(mktemp)\n\
__sir_err=$(mktemp)\n\
__sir_flag=$(mktemp)\n\
rm -f \"$__sir_flag\"\n\
__sir_rc=0\n\
__sir_done=0\n\
if [ -n \"$__sir_cwd\" ]; then\n\
  cd \"$__sir_cwd\" >\"$__sir_out\" 2>\"$__sir_err\" || __sir_rc=$?\n\
fi\n\
if [ \"$__sir_rc\" -eq 0 ]; then\n\
  __sir_watch=''\n\
  if [ {timeout_ms} -gt 0 ]; then\n\
    (\n\
      sleep {timeout_secs}\n\
      printf '1' >\"$__sir_flag\"\n\
      printf '__SIR_DONE__ {token} %s %s %s %s\\n' 124 1 \"$__sir_out\" \"$__sir_err\"\n\
      kill -KILL $$ 2>/dev/null || true\n\
    ) &\n\
    __sir_watch=$!\n\
  fi\n\
{env_exports}  {{ eval \"$__sir_cmd\" ; }} {stdin_redirect}>\"$__sir_out\" 2>\"$__sir_err\"\n\
  __sir_rc=$?\n\
  if [ -s \"$__sir_flag\" ]; then\n\
    __sir_rc=124\n\
  fi\n\
  if [ -n \"$__sir_watch\" ]; then\n\
    kill \"$__sir_watch\" 2>/dev/null || true\n\
    wait \"$__sir_watch\" 2>/dev/null || true\n\
  fi\n\
fi\n\
__sir_timed_out=0\n\
if [ -s \"$__sir_flag\" ]; then\n\
  __sir_timed_out=1\n\
fi\n\
rm -f \"$__sir_flag\"\n\
{stdin_cleanup}\
printf '__SIR_DONE__ {token} %s %s %s %s\\n' \"$__sir_rc\" \"$__sir_timed_out\" \"$__sir_out\" \"$__sir_err\"\n",
        timeout_ms = timeout_ms,
        timeout_secs = timeout_secs,
        env_exports = env_exports,
        stdin_cleanup = stdin_cleanup,
        stdin_redirect = stdin_redirect,
        stdin_setup = stdin_setup,
        token = token,
    ))
}

fn build_heredoc(var_name: &str, token: &str, body: &str) -> String {
    format!(
        "{var_name}=$(cat <<'__{var_name}_{token}__'\n{body}\n__{var_name}_{token}__\n)\n",
        var_name = var_name,
        token = token,
        body = body,
    )
}

fn build_file_heredoc(var_name: &str, token: &str, body: &str) -> String {
    format!(
        "{var_name}=$(mktemp)\ncat <<'__{var_name}_{token}__' > \"$${var_name}\"\n{body}\n__{var_name}_{token}__\n",
        var_name = var_name,
        token = token,
        body = body,
    )
    .replace("$$", "$")
}

pub(crate) fn build_env_exports(env: &BTreeMap<String, String>) -> AppResult<String> {
    let mut lines = String::new();
    for (key, value) in env {
        if !is_valid_env_name(key) {
            return Err(anyhow!("invalid env name `{}`", key));
        }
        lines.push_str("  export ");
        lines.push_str(key);
        lines.push('=');
        lines.push_str(&shell_single_quote(value));
        lines.push('\n');
    }
    Ok(lines)
}

pub(crate) fn is_valid_env_name(name: &str) -> bool {
    let mut chars = name.chars();
    match chars.next() {
        Some(ch) if ch == '_' || ch.is_ascii_alphabetic() => {}
        _ => return false,
    }
    chars.all(|ch| ch == '_' || ch.is_ascii_alphanumeric())
}

pub(crate) fn shell_single_quote(value: &str) -> String {
    format!("'{}'", value.replace('\'', "'\"'\"'"))
}

/// Session map key: `<seat>:<session_id>`. Seat comes from CTX_CALLER
/// (or the HTTP peer identity); "local" when unset.
pub(crate) fn session_key(seat: &str, session_id: &str) -> String {
    format!("{}:{}", seat, session_id)
}

fn make_token(slot: usize) -> String {
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|value| value.as_nanos())
        .unwrap_or_default();
    format!("{}_{}", slot, nanos)
}

fn parse_marker(line: &str, token: &str) -> Option<WarmShellMarker> {
    let parts = line.split_whitespace().collect::<Vec<_>>();
    if parts.len() != 6 || parts[0] != "__SIR_DONE__" || parts[1] != token {
        return None;
    }
    Some(WarmShellMarker {
        exit_code: parts[2].parse().ok()?,
        timed_out: parts[3] == "1",
        stdout_path: parts[4].to_string(),
        stderr_path: parts[5].to_string(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    // ---- Q14: adversarial inputs for the shell-string builders ----

    #[test]
    fn single_quote_escapes_embedded_quotes() {
        assert_eq!(shell_single_quote("plain"), "'plain'");
        assert_eq!(shell_single_quote("it's"), "'it'\"'\"'s'");
        assert_eq!(shell_single_quote("'"), "''\"'\"''");
    }

    #[test]
    fn heredoc_uses_unique_token_delimiter() {
        let doc = build_heredoc("__sir_cmd", "3_12345", "echo $(rm -rf /)");
        assert!(doc.contains("<<'____sir_cmd_3_12345__'"));
        assert!(doc.contains("echo $(rm -rf /)"));
        // quoted delimiter => no expansion of $(...) inside the body
        assert!(doc.ends_with("____sir_cmd_3_12345__\n)\n"));
    }

    #[test]
    fn env_exports_reject_bad_names() {
        let mut env = BTreeMap::new();
        env.insert("GOOD_NAME_1".to_string(), "v".to_string());
        assert!(build_env_exports(&env).is_ok());

        for bad in ["1BAD", "EVIL;rm", "A B", "A$B", "", "A=B"] {
            let mut env = BTreeMap::new();
            env.insert(bad.to_string(), "x".to_string());
            assert!(build_env_exports(&env).is_err(), "accepted {bad:?}");
        }
    }

    #[test]
    fn env_exports_quote_values() {
        let mut env = BTreeMap::new();
        env.insert("K".to_string(), "it's $(bad)".to_string());
        let out = build_env_exports(&env).unwrap();
        assert!(out.contains("export K='it'\"'\"'s $(bad)'"));
    }

    #[test]
    fn persistent_script_carries_watchdog_when_timeout_set() {
        let script =
            build_persistent_shell_script("tok", "sleep 5", None, &BTreeMap::new(), None, 2000)
                .unwrap();
        assert!(script.contains("__sir_flag=$(mktemp)"));
        assert!(
            script.contains("kill -KILL $$"),
            "watchdog must reap the wedged warm shell itself"
        );
        assert!(
            script.contains("printf '__SIR_DONE__"),
            "watchdog prints its own marker before dying"
        );
        assert!(
            script.contains("{ eval \"$__sir_cmd\" ; }"),
            "persistent lane must eval in-shell or exports die with the subshell"
        );
        assert!(script.contains("sleep 2.000"));
        assert!(script.contains("\"$__sir_timed_out\""));
    }

    #[test]
    fn persistent_script_without_timeout_has_no_watchdog() {
        let script =
            build_persistent_shell_script("tok", "true", None, &BTreeMap::new(), None, 0).unwrap();
        assert!(script.contains("if [ 0 -gt 0 ]"));
    }

    #[test]
    fn marker_parser_matches_only_full_token() {
        let marker = parse_marker("__SIR_DONE__ 9_42 0 0 /tmp/a /tmp/b", "9_42").unwrap();
        assert_eq!(marker.exit_code, 0);
        assert!(!marker.timed_out);
        assert!(parse_marker("__SIR_DONE__ 9_43 0 0 /tmp/a /tmp/b", "9_42").is_none());
        assert!(parse_marker("random noise", "9_42").is_none());
    }
}

#[cfg(test)]
mod session_tests {
    use super::*;
    use std::thread::sleep;

    #[test]
    fn sessions_are_namespaced_per_seat() {
        let pool = WarmShellPool::new(4).unwrap();
        let kim_slot = pool.claim_named("kim", "work").unwrap();
        let cel_slot = pool.claim_named("cel", "work").unwrap();
        assert_ne!(
            kim_slot, cel_slot,
            "same name, different seats must not share a slot"
        );
        // re-claim by same seat returns the same slot
        assert_eq!(pool.claim_named("kim", "work").unwrap(), kim_slot);
    }

    #[test]
    fn seats_cannot_release_each_others_sessions() {
        let pool = WarmShellPool::new(4).unwrap();
        pool.claim_named("kim", "work").unwrap();
        assert!(
            !pool.release_named("cel", "work").unwrap(),
            "cel must not close kim's session"
        );
        assert!(pool.release_named("kim", "work").unwrap());
    }

    #[test]
    fn idle_sessions_are_reaped_and_slots_reusable() {
        let pool = WarmShellPool::new(2).unwrap();
        pool.claim_named("kim", "s1").unwrap();
        pool.claim_named("cel", "s2").unwrap();
        assert_eq!(pool.named_session_count(), 2);
        // zero-timeout reaper: everything is stale immediately
        let reaped = pool.reap_idle_sessions(Duration::ZERO);
        assert_eq!(reaped.len(), 2);
        assert!(reaped.contains(&"kim:s1".to_string()));
        assert_eq!(pool.named_session_count(), 0);
        // slots are free again
        pool.claim_named("monica", "s3").unwrap();
    }

    #[test]
    fn fresh_sessions_survive_the_reaper() {
        let pool = WarmShellPool::new(2).unwrap();
        pool.claim_named("kim", "hot").unwrap();
        sleep(Duration::from_millis(5));
        let reaped = pool.reap_idle_sessions(Duration::from_secs(60));
        assert!(reaped.is_empty());
        assert_eq!(pool.named_session_count(), 1);
    }
}
