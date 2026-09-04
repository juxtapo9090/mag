//! Denial hints — one line appended under a refused command.
//!
//! mag exists to be the door: a lane with no broker and no policy gate. But it
//! runs as an ordinary uid, so when the kernel refuses something it says the
//! same `Permission denied` any shell would, and the honest-looking reading is
//! "I lack authority here". On this box that reading is usually wrong — the
//! escalation is passwordless and one word away. A caller who can't see that
//! writes the wall down as a boundary and routes around a door that was open.
//!
//! Two rules this module lives by:
//!
//! 1. **Never manufacture evidence.** The sudo hint is printed only after
//!    `sudo -n true` has actually been run and actually succeeded. A system
//!    that asserts its own capability is worth less than no system at all.
//! 2. **Scope the claim to the syscall.** A refusal describes one operation on
//!    one path. The hint says what to try next; it never says how wide the
//!    refusal is, because the error didn't say either.

use std::process::Command;
use std::sync::OnceLock;

use crate::cli::CommandResult;

/// Substrings that mean "something refused this", lowercased at match time.
const DENIAL_MARKERS: &[&str] = &[
    "permission denied",
    "operation not permitted",
    "access denied",
    "eacces",
    "must be run as root",
    "you must be root",
    "need to be root",
];

/// git's `safe.directory` guard. Looks like a permission error, is not one —
/// sudo does not fix it, so it gets its own hint.
const OWNERSHIP_MARKER: &str = "dubious ownership";

/// Did the caller already reach for sudo? Then the refusal is sudo's own and
/// telling them to use sudo is noise.
const SUDO_ALREADY_TRIED: &[&str] = &[
    "sudo: a password is required",
    "sudo: no tty present",
    "is not in the sudoers file",
    "sorry, user",
];

/// Who actually ran the command, and can that identity escalate.
///
/// This exists because mag renders in a *different process* than it executes.
/// The forwarder (`--lane forward`) is spawned by each seat's own client and
/// inherits that seat's uid — frequently root. The daemon runs the command as
/// `juxtapo`. Probing the local process therefore answers a question nobody
/// asked: it describes the renderer, not the runner.
///
/// So the executing side reports its own facts (`/health`) and the rendering
/// side installs them here before rendering. Same rule as the commit-stamp fix
/// and the beacon heartbeat: **the process that knows is the process that
/// answers.** Never infer another process's identity from your own.
#[derive(Debug, Clone)]
pub struct ExecIdentity {
    pub user: String,
    pub can_escalate: bool,
}

static REMOTE: OnceLock<ExecIdentity> = OnceLock::new();

/// Install the executing identity, learned from the daemon. First call wins;
/// it is a property of the daemon we're bound to, so it does not change.
pub fn set_exec_identity(identity: ExecIdentity) {
    let _ = REMOTE.set(identity);
}

/// Facts about *this* process — correct only when it is also the executor,
/// which is true for the direct CLI paths (`run-toon`, `run-batch`).
#[must_use]
pub fn local_identity() -> ExecIdentity {
    ExecIdentity {
        user: current_user().to_string(),
        can_escalate: !running_as_root() && sudo_is_passwordless(),
    }
}

/// Whoever actually ran the command: the daemon if we're forwarding, else us.
pub(crate) fn exec_identity() -> ExecIdentity {
    REMOTE.get().cloned().unwrap_or_else(local_identity)
}

/// Verified once per process: does passwordless escalation actually work?
/// `-n` is non-interactive — it fails immediately rather than prompting, so
/// this is safe to run from a daemon with no tty.
fn sudo_is_passwordless() -> bool {
    static CACHED: OnceLock<bool> = OnceLock::new();
    *CACHED.get_or_init(|| {
        Command::new("sudo")
            .args(["-n", "true"])
            .output()
            .is_ok_and(|out| out.status.success())
    })
}

/// Already root? Then there is nothing to escalate to and the hint would be a lie.
fn running_as_root() -> bool {
    static CACHED: OnceLock<bool> = OnceLock::new();
    *CACHED.get_or_init(|| {
        use std::os::unix::fs::MetadataExt;
        std::fs::metadata("/proc/self").is_ok_and(|meta| meta.uid() == 0)
    })
}

/// The uid we actually ran as, for the hint text. Falls back to the numeric id.
fn current_user() -> &'static str {
    static CACHED: OnceLock<String> = OnceLock::new();
    CACHED.get_or_init(|| {
        Command::new("id")
            .arg("-un")
            .output()
            .ok()
            .filter(|out| out.status.success())
            .map(|out| String::from_utf8_lossy(&out.stdout).trim().to_string())
            .filter(|name| !name.is_empty())
            .unwrap_or_else(|| "this uid".to_string())
    })
}

/// Every stream a refusal could have landed in, joined and lowercased once.
fn haystack(result: &CommandResult) -> String {
    let mut text = String::new();
    for part in [
        result.stderr.as_deref(),
        result.error.as_deref(),
        result.stdout.as_deref(),
    ]
    .into_iter()
    .flatten()
    {
        text.push_str(part);
        text.push('\n');
    }
    text.to_lowercase()
}

/// The single line to append under a refused command, or `None`.
///
/// Only ever fires on a command that failed, so a working session pays nothing.
pub(crate) fn denial_hint(result: &CommandResult) -> Option<String> {
    denial_hint_for(result, &exec_identity())
}

/// The decision itself, with the executing identity passed in rather than
/// discovered. Pure — which is the only reason it can be tested honestly.
fn denial_hint_for(result: &CommandResult, identity: &ExecIdentity) -> Option<String> {
    if result.success || result.timed_out {
        return None;
    }

    let text = haystack(result);

    if text.contains(OWNERSHIP_MARKER) {
        return Some(
            "hint: git's safe.directory guard, not a permission — sudo won't help. \
             `mohor` handles it, or: git config --global --add safe.directory <path>"
                .to_string(),
        );
    }

    if !DENIAL_MARKERS.iter().any(|marker| text.contains(marker)) {
        return None;
    }
    if SUDO_ALREADY_TRIED
        .iter()
        .any(|marker| text.contains(marker))
    {
        return None;
    }

    if !identity.can_escalate {
        return None;
    }

    Some(format!(
        "hint: refused as {} · `sudo -n` verified passwordless for that uid — retry \
         escalated before reading this as a boundary. it describes one operation on one path.",
        identity.user
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn result_with(stderr: &str, success: bool) -> CommandResult {
        CommandResult {
            index: 0,
            id: None,
            mode: "raw".to_string(),
            success,
            exit_code: Some(i32::from(!success)),
            timed_out: false,
            duration_ms: 1,
            stdout: None,
            stderr: Some(stderr.to_string()),
            stdout_truncated: false,
            stderr_truncated: false,
            diet_applied: false,
            exec_user: None,
            error: None,
        }
    }

    #[test]
    fn success_never_hints() {
        assert!(
            denial_hint_for(
                &result_with("Permission denied", true),
                &who("juxtapo", true)
            )
            .is_none()
        );
    }

    #[test]
    fn unrelated_failure_never_hints() {
        assert!(
            denial_hint_for(
                &result_with("no such file or directory", false),
                &who("juxtapo", true)
            )
            .is_none()
        );
    }

    #[test]
    fn dubious_ownership_gets_the_git_hint_not_the_sudo_one() {
        let hint = denial_hint_for(
            &result_with(
                "fatal: detected dubious ownership in repository at '/x'",
                false,
            ),
            &who("juxtapo", true),
        )
        .expect("ownership refusal should hint");
        assert!(hint.contains("safe.directory"));
        assert!(!hint.contains("sudo -n` verified"));
    }

    #[test]
    fn sudos_own_refusal_is_not_answered_with_sudo() {
        assert!(
            denial_hint_for(
                &result_with("sudo: a password is required", false),
                &who("juxtapo", true)
            )
            .is_none()
        );
    }

    fn who(user: &str, can_escalate: bool) -> ExecIdentity {
        ExecIdentity {
            user: user.to_string(),
            can_escalate,
        }
    }

    /// The load-bearing one: the hint must track reality, not assert it.
    /// An identity that cannot prove escalation gets silence, not a guess.
    #[test]
    fn stays_silent_when_escalation_is_not_proven() {
        assert!(
            denial_hint_for(
                &result_with("ls: /root/: Permission denied", false),
                &who("root", false)
            )
            .is_none()
        );
    }

    /// The bug this module shipped with: the renderer probed *itself*. Under
    /// the MCP lane the renderer is root while the runner is `juxtapo`, so the
    /// root guard suppressed a hint that was correct — the feature silently did
    /// nothing in the only lane that mattered. The hint must describe the uid
    /// that earned the refusal, never the one formatting it.
    #[test]
    fn hint_names_the_executing_uid_not_the_rendering_one() {
        let hint = denial_hint_for(
            &result_with("ls: /root/: Permission denied", false),
            &who("juxtapo", true),
        )
        .expect("a provably-escalatable uid should get the hint");
        assert!(hint.contains("juxtapo"), "must name the runner");
        assert!(!hint.contains("root"), "must not name the renderer");
    }
}
