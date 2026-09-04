use crate::AppResult;
use anyhow::{Context, anyhow};
use std::env;
use std::os::unix::ffi::OsStrExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

const DEFAULT_SESSION: &str = "mag-house";
const DEFAULT_START_DIR: &str = "/home/juxtapo";
const WATCH_PANE_MARKER: &str = "@mag-term-watch";
/// Fingerprint of the watch pane's own command, used to recognise a watch pane
/// that predates titling (or was left by an older build).
const WATCH_COMMAND_MARKER: &str = "mag watch pane:";

/// tmux resolves its server from the CALLER's uid (`/tmp/tmux-<uid>/default`).
/// A mag daemon running as root therefore lives on `/tmp/tmux-0/default` — but
/// seat panes like `mag-celeste` are created by a juxtapo boot gadget and live
/// on `/tmp/tmux-1000/default`. Same session name, two different servers, and
/// a bare `has-session` answers "missing" for a pane a human is attached to.
/// Resolution order: explicit `socket` param (CLI `--socket` / MCP `socket`)
/// → `MAG_TERM_SOCKET` env → probe every live `/tmp/tmux-*/default` for the
/// session → bare tmux (own-uid server, preserving old behaviour for sessions
/// nobody else hosts).
pub(crate) struct TmuxServer {
    args: Vec<String>,
    /// Set when the session was found by probing another uid's server. Used
    /// to refuse actions that would silently fork a twin (open, send-create).
    pub(crate) probed: bool,
}

impl TmuxServer {
    pub(crate) fn for_session(session: &str, explicit: Option<&str>) -> AppResult<Self> {
        if let Some(socket) = explicit {
            let socket = socket.trim();
            if socket.is_empty() {
                return Err(anyhow!("terminal socket path is empty"));
            }
            return Ok(Self::at(socket));
        }
        if let Ok(socket) = env::var("MAG_TERM_SOCKET") {
            let socket = socket.trim();
            if !socket.is_empty() {
                return Ok(Self::at(socket));
            }
        }
        // Bare has-session first: if the session lives on OUR uid's server the
        // probe below never runs, and nothing changes for mag-created panes.
        //
        // CRITICAL: a dead `/tmp/tmux-<uid>` dir (server killed, socket stale)
        // makes tmux START A NEW SERVER on that socket, which then finds the
        // session on ANOTHER server (uid namespace collision). So we can't
        // trust bare_has_session when the socket was just created by this call.
        // Instead: probe first. If the probe finds the session on a DIFFERENT
        // uid's server, use that server and mark probed=true. If it finds the
        // session on OUR OWN server (or nowhere), fall through to bare.
        let own_socket = PathBuf::from(format!("/tmp/tmux-{}/default", unsafe { libc::getuid() }));
        if let Some(socket) = probe_servers(session) {
            let probed = socket != own_socket;
            return Ok(Self {
                args: vec!["-S".to_string(), socket.to_string_lossy().into_owned()],
                probed,
            });
        }
        Ok(Self::bare())
    }

    fn bare() -> Self {
        Self {
            args: Vec::new(),
            probed: false,
        }
    }

    fn at(socket: &str) -> Self {
        Self {
            args: vec!["-S".to_string(), socket.to_string()],
            probed: false,
        }
    }

    pub(crate) fn has_session(&self, session: &str) -> bool {
        // `has-session` is a question, not an action: its "can't find session"
        // on stderr is the expected negative answer, so it must not surface as
        // if the command had failed.
        self.command()
            .args(["has-session", "-t", session])
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()
            .is_ok_and(|status| status.success())
    }

    fn command(&self) -> Command {
        let mut command = Command::new("tmux");
        command.args(&self.args);
        command
    }

    fn status(&self, args: &[&str], action: &str) -> AppResult<()> {
        let status = self
            .command()
            .args(args)
            .status()
            .with_context(|| format!("{action}: spawn tmux"))?;
        if status.success() {
            Ok(())
        } else {
            Err(anyhow!("{action}: tmux exited with {status}"))
        }
    }

    fn capture(&self, args: &[&str], action: &str) -> AppResult<String> {
        let output = self
            .command()
            .args(args)
            .output()
            .with_context(|| format!("{action}: spawn tmux"))?;
        if output.status.success() {
            Ok(String::from_utf8_lossy(&output.stdout).into_owned())
        } else {
            Err(anyhow!(
                "{action}: {}",
                String::from_utf8_lossy(&output.stderr).trim()
            ))
        }
    }
}

/// A tmux server only answers for sessions on ITS OWN socket — the socket path
/// is the whole identity. So "which server hosts this session" is answered by
/// asking each live `/tmp/tmux-*/default` in turn; root can open every uid's
/// socket dir, a non-root daemon simply fails the ones it can't reach and
/// moves on.
fn probe_servers(session: &str) -> Option<PathBuf> {
    let mut dirs: Vec<PathBuf> = std::fs::read_dir("/tmp")
        .ok()?
        .filter_map(std::result::Result::ok)
        .map(|entry| entry.path())
        .filter(|path| {
            path.file_name().is_some_and(|name| {
                let bytes = name.as_bytes();
                bytes.starts_with(b"tmux-") && bytes[5..].iter().all(u8::is_ascii_digit)
            })
        })
        .collect();
    dirs.sort_unstable();
    for dir in dirs {
        let socket = dir.join("default");
        if socket.exists()
            && Command::new("tmux")
                .args(["-S", &socket.to_string_lossy(), "has-session", "-t", session])
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .status()
                .is_ok_and(|status| status.success())
        {
            return Some(socket);
        }
    }
    None
}

fn bare_has_session(session: &str) -> bool {
    Command::new("tmux")
        .args(["has-session", "-t", session])
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .is_ok_and(|status| status.success())
}

pub(crate) struct TermOpenOptions {
    pub(crate) session: Option<String>,
    pub(crate) cwd: Option<String>,
    pub(crate) no_wezterm: bool,
    pub(crate) no_watch: bool,
    pub(crate) socket: Option<String>,
}

pub(crate) struct TermTargetOptions {
    pub(crate) session: Option<String>,
    pub(crate) socket: Option<String>,
}

pub(crate) struct TermSendOptions {
    pub(crate) session: Option<String>,
    pub(crate) input: String,
    pub(crate) enter: bool,
    pub(crate) socket: Option<String>,
}

pub(crate) struct TermSnapshotOptions {
    pub(crate) session: Option<String>,
    pub(crate) lines: usize,
    pub(crate) socket: Option<String>,
}

pub(crate) fn open(options: &TermOpenOptions) -> AppResult<()> {
    let session = session_name(options.session.as_deref());
    let cwd = start_dir(options.cwd.as_deref())?;
    let server = TmuxServer::for_session(&session, options.socket.as_deref())?;
    if server.probed {
        // The session already lives on another uid's server. Creating it here
        // would spawn a blind twin — two same-named sessions, a human on one,
        // mag driving the other. Point at the real one instead.
        return Err(anyhow!(
            "tmux session `{session}` already exists on another server; \
             pass --socket/-S (or MCP `socket`) to attach to it instead of creating a twin"
        ));
    }
    ensure_session(&server, &session, &cwd, !options.no_watch)?;

    if options.no_wezterm {
        println!("tmux session ready: {session}");
        println!("attach: tmux attach -t {session}");
        return Ok(());
    }

    if command_exists("wezterm") {
        spawn_wezterm(&session)?;
        println!("opened wezterm for tmux session: {session}");
    } else {
        println!("wezterm not found; attach manually: tmux attach -t {session}");
    }

    Ok(())
}

pub(crate) fn attach(options: &TermTargetOptions) -> AppResult<()> {
    let session = session_name(options.session.as_deref());
    let server = TmuxServer::for_session(&session, options.socket.as_deref())?;
    ensure_session(&server, &session, Path::new(DEFAULT_START_DIR), true)?;
    replace_with_tmux_attach(&server, &session)
}

pub(crate) fn status(options: &TermTargetOptions) -> AppResult<()> {
    print!("{}", status_text(options)?);
    Ok(())
}

pub(crate) fn status_text(options: &TermTargetOptions) -> AppResult<String> {
    let session = session_name(options.session.as_deref());
    let server = TmuxServer::for_session(&session, options.socket.as_deref())?;
    if !server.has_session(&session) {
        return Ok(format!("session={session} status=missing\n"));
    }

    let cwd = server.capture(
        &[
            "display-message",
            "-p",
            "-t",
            &operator_pane(&server, &session)?,
            "#{pane_current_path}",
        ],
        "read tmux pane cwd",
    )?;
    let panes = server.capture(
        &[
            "list-panes",
            "-s",
            "-t",
            &session,
            "-F",
            "#{pane_id}:#{pane_title}:#{pane_current_path}",
        ],
        "list tmux panes",
    )?;

    Ok(format!(
        "session={session} status=running cwd={}\n{panes}",
        cwd.trim()
    ))
}

pub(crate) fn send(options: &TermSendOptions) -> AppResult<()> {
    let session = session_name(options.session.as_deref());
    let server = TmuxServer::for_session(&session, options.socket.as_deref())?;
    if server.probed {
        // The pane already exists (that's how the probe found it) — driving it
        // is the whole point of the probe. Just don't create anything new.
        send_keys(&server, &session, options)?;
        return Ok(());
    }
    ensure_session(&server, &session, Path::new(DEFAULT_START_DIR), true)?;
    send_keys(&server, &session, options)
}

fn send_keys(server: &TmuxServer, session: &str, options: &TermSendOptions) -> AppResult<()> {
    let operator = operator_pane(server, session)?;
    server.status(
        &["send-keys", "-t", &operator, "--", &options.input],
        "send tmux input",
    )?;
    if options.enter {
        server.status(&["send-keys", "-t", &operator, "C-m"], "send enter")?;
    }
    Ok(())
}

pub(crate) fn snapshot(options: &TermSnapshotOptions) -> AppResult<()> {
    print!("{}", snapshot_text(options)?);
    Ok(())
}

pub(crate) fn snapshot_text(options: &TermSnapshotOptions) -> AppResult<String> {
    let session = session_name(options.session.as_deref());
    let server = TmuxServer::for_session(&session, options.socket.as_deref())?;
    if !server.has_session(&session) {
        return Err(anyhow!("tmux session `{session}` is not running"));
    }

    let start = format!("-{}", options.lines.max(1));
    server.capture(
        &[
            "capture-pane",
            "-p",
            "-t",
            &operator_pane(&server, &session)?,
            "-S",
            &start,
        ],
        "capture tmux pane",
    )
}

pub(crate) fn close(options: &TermTargetOptions) -> AppResult<()> {
    let session = session_name(options.session.as_deref());
    let server = TmuxServer::for_session(&session, options.socket.as_deref())?;
    if server.has_session(&session) {
        server.status(&["kill-session", "-t", &session], "close tmux session")?;
        println!("closed tmux session: {session}");
    } else {
        println!("session already missing: {session}");
    }
    Ok(())
}

fn ensure_session(server: &TmuxServer, session: &str, cwd: &Path, watch: bool) -> AppResult<()> {
    require_command("tmux")?;
    if !server.has_session(session) {
        server.status(
            &[
                "new-session",
                "-d",
                "-s",
                session,
                "-n",
                "house",
                "-c",
                path_str(cwd)?,
            ],
            "create tmux session",
        )?;
        server.status(
            &[
                "send-keys",
                "-t",
                &operator_pane(server, session)?,
                "printf 'mag terminal ready: shared cwd/env stays in this pane\\n'",
                "C-m",
            ],
            "prime tmux pane",
        )?;
    }

    if watch {
        ensure_watch_pane(server, session)?;
    }

    Ok(())
}

fn ensure_watch_pane(server: &TmuxServer, session: &str) -> AppResult<()> {
    if list_session_panes(server, session)?
        .iter()
        .any(PaneInfo::is_watch)
    {
        return Ok(());
    }
    let operator = operator_pane(server, session)?;

    // `-P -F` hands back the new pane's own id, so the pane we title is the pane
    // we just made — no window-name path to get wrong, and nothing can be left
    // behind half-built between the split and the title.
    let pane_id = server.capture(
        &[
            "split-window",
            "-v",
            "-t",
            &operator,
            "-p",
            "30",
            "-c",
            DEFAULT_START_DIR,
            "-P",
            "-F",
            "#{pane_id}",
            watch_command(),
        ],
        "create watch pane",
    )?;
    let pane_id = pane_id.trim().to_string();
    if pane_id.is_empty() {
        return Err(anyhow!("tmux did not report a pane id for the watch pane"));
    }

    if let Err(err) = server.status(
        &["select-pane", "-t", &pane_id, "-T", WATCH_PANE_MARKER],
        "title watch pane",
    ) {
        // Titling is the only step left that can fail; roll the pane back rather
        // than leave a stray one in a window someone is looking at.
        let _ = server.status(&["kill-pane", "-t", &pane_id], "roll back watch pane");
        return Err(err);
    }

    server.status(&["select-pane", "-t", &operator], "select operator pane")?;
    Ok(())
}

fn watch_command() -> &'static str {
    "bash -lc 'printf \"mag watch pane: /var/lib/mag/audit.jsonl + Jaga feeds\\n\"; while true; do tail -n 40 -F /var/lib/mag/audit.jsonl /home/juxtapo/Server_files/Jaga-Rust/logs/*.feed.jsonl 2>/dev/null; sleep 2; done'"
}

fn spawn_wezterm(session: &str) -> AppResult<()> {
    Command::new("wezterm")
        .args(["start", "--", "tmux", "attach", "-t", session])
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .context("spawn wezterm")?;
    Ok(())
}

fn replace_with_tmux_attach(server: &TmuxServer, session: &str) -> AppResult<()> {
    server
        .command()
        .args(["attach", "-t", session])
        .status()
        .context("attach tmux")?
        .success()
        .then_some(())
        .ok_or_else(|| anyhow!("tmux attach exited unsuccessfully"))
}

fn session_name(value: Option<&str>) -> String {
    if let Some(name) = value.filter(|name| !name.trim().is_empty()) {
        return name.to_string();
    }
    env::var("MAG_TERM_SESSION")
        .ok()
        .filter(|name| !name.trim().is_empty())
        .unwrap_or_else(|| DEFAULT_SESSION.to_string())
}

fn start_dir(value: Option<&str>) -> AppResult<std::path::PathBuf> {
    let raw = value
        .filter(|path| !path.trim().is_empty())
        .map_or_else(|| DEFAULT_START_DIR.to_string(), ToString::to_string);
    let path = std::path::PathBuf::from(raw);
    if path.is_dir() {
        Ok(path)
    } else {
        Err(anyhow!(
            "terminal cwd is not a directory: {}",
            path.display()
        ))
    }
}

/// One tmux pane, addressed by its own id (`%12`) rather than by a
/// `<session>:<window>.<index>` path. Ids are stable and unambiguous; the name
/// path only ever worked for sessions mag created itself (it hardcoded the
/// window name `house`), so every `term` call against a pre-existing session
/// failed with `can't find window: house`.
struct PaneInfo {
    id: String,
    title: String,
    start_command: String,
}

impl PaneInfo {
    /// The watch pane is recognised by its title, or — for panes created before
    /// titling existed, or by an older build — by the command it is running.
    /// Matching on either keeps us from stacking a second watch pane onto a
    /// session that already has one.
    fn is_watch(&self) -> bool {
        self.title.trim() == WATCH_PANE_MARKER || self.start_command.contains(WATCH_COMMAND_MARKER)
    }
}

/// Every pane in the session, across all its windows. `-s` matters: without it
/// tmux lists only the *current* window, so an existence check silently looks
/// somewhere other than where it means to.
fn list_session_panes(server: &TmuxServer, session: &str) -> AppResult<Vec<PaneInfo>> {
    let raw = server.capture(
        &[
            "list-panes",
            "-s",
            "-t",
            session,
            "-F",
            "#{pane_id}\t#{pane_title}\t#{pane_start_command}",
        ],
        "list tmux panes",
    )?;

    Ok(raw
        .lines()
        .filter_map(|line| {
            let mut fields = line.split('\t');
            let id = fields.next()?.trim();
            if id.is_empty() {
                return None;
            }
            Some(PaneInfo {
                id: id.to_string(),
                title: fields.next().unwrap_or_default().to_string(),
                start_command: fields.next().unwrap_or_default().to_string(),
            })
        })
        .collect())
}

/// The pane a human types into: the session's first pane that isn't the watch
/// pane. For a mag-created session that is the primed shell; for a pre-existing
/// session it is whatever was already there, which is the right target.
fn operator_pane(server: &TmuxServer, session: &str) -> AppResult<String> {
    let panes = list_session_panes(server, session)?;
    panes
        .iter()
        .find(|pane| !pane.is_watch())
        .or_else(|| panes.first())
        .map(|pane| pane.id.clone())
        .ok_or_else(|| anyhow!("tmux session `{session}` has no panes"))
}

fn require_command(name: &str) -> AppResult<()> {
    if command_exists(name) {
        Ok(())
    } else {
        Err(anyhow!("required command not found in PATH: {name}"))
    }
}

fn command_exists(name: &str) -> bool {
    Command::new("sh")
        .args(["-c", &format!("command -v {name} >/dev/null 2>&1")])
        .status()
        .is_ok_and(|status| status.success())
}

fn path_str(path: &Path) -> AppResult<&str> {
    path.to_str()
        .ok_or_else(|| anyhow!("path is not valid UTF-8: {}", path.display()))
}
