use crate::cli::{BatchResponse, CommandResult};
use crate::hint::denial_hint;

/// Envelope-slim: one header line per result, then raw stdout, then optional
/// stderr/error blocks. Nothing crosses the wire that doesn't earn its tokens
/// — pool stats live on GET /health, the manual lives in mag-how-to.md.
pub(crate) fn render_toon_response(response: &mut BatchResponse) -> String {
    // Fill the executing user onto any result that doesn't already carry it.
    // The serve path stamps this server-side before serialization; this covers
    // the direct CLI path (run-batch/run-toon, lane: local) where results are
    // built in-process and rendered without crossing the wire.
    let exec_user = crate::hint::exec_identity().user;
    for result in &mut response.results {
        if result.exec_user.is_none() {
            result.exec_user = Some(exec_user.clone());
        }
    }
    let mut out = String::new();
    let multiple = response.results.len() > 1;
    if multiple {
        out.push_str("::MAG\n");
    }

    for result in &response.results {
        if !out.is_empty() {
            out.push('\n');
        }
        out.push_str(&mag_header(result));
        out.push('\n');

        if let Some(stdout) = non_empty_text(result.stdout.as_deref()) {
            out.push_str(stdout);
            if !stdout.ends_with('\n') {
                out.push('\n');
            }
        }
        if let Some(stderr) = non_empty_text(result.stderr.as_deref()) {
            out.push_str("stderr:\n");
            out.push_str(stderr);
            if !stderr.ends_with('\n') {
                out.push('\n');
            }
        }
        if let Some(error) = non_empty_text(result.error.as_deref()) {
            out.push_str("error:\n");
            out.push_str(error);
            if !error.ends_with('\n') {
                out.push('\n');
            }
        }
        if let Some(hint) = denial_hint(result) {
            out.push_str(&hint);
            out.push('\n');
        }
    }

    out.trim_end_matches('\n').to_string()
}

/// `[mag <id-or-#index>] exit=<n> dur=<ms> exec_user=<u> [timeout] [trunc-out] [trunc-err] [diet]`
fn mag_header(result: &CommandResult) -> String {
    let label = result
        .id
        .clone()
        .unwrap_or_else(|| format!("#{}", result.index + 1));
    let exit = result
        .exit_code
        .map_or_else(|| "-".to_string(), |code| code.to_string());
    let user = result.exec_user.as_deref().unwrap_or("?");
    let mut flags = String::new();
    if result.timed_out {
        flags.push_str(" timeout");
    }
    if result.stdout_truncated {
        flags.push_str(" trunc-out");
    }
    if result.stderr_truncated {
        flags.push_str(" trunc-err");
    }
    if result.diet_applied {
        flags.push_str(" diet");
    }
    if !result.success && result.error.is_some() {
        flags.push_str(" failed");
    }
    let _ = (label, exit, user, flags, result.duration_ms);
    "::MAG".to_string()
}

/// Render warm-session status. `filter` narrows to a single id; asking about an
/// id that isn't held says so explicitly rather than returning an empty list —
/// "no sessions at all" and "not that one" are different answers.
pub(crate) fn render_session_statuses(
    statuses: &[crate::pool::SessionStatus],
    filter: Option<&str>,
) -> String {
    let selected: Vec<&crate::pool::SessionStatus> = match filter {
        Some(id) => statuses
            .iter()
            .filter(|status| status.session_id == id)
            .collect(),
        None => statuses.iter().collect(),
    };

    if selected.is_empty() {
        return match filter {
            Some(id) => format!("session `{id}` is not held (no warm shell pinned to it)"),
            None => "no named sessions held".to_string(),
        };
    }

    let mut out = Vec::with_capacity(selected.len() + 1);
    out.push(format!("{} named session(s)", selected.len()));
    for status in selected {
        let pid = status
            .pid
            .map_or_else(|| "gone".to_string(), |pid| pid.to_string());
        out.push(format!(
            "  {} slot={} pid={} age={} idle={}",
            status.session_id,
            status.slot,
            pid,
            human_ms(status.age_ms),
            human_ms(status.idle_ms),
        ));
    }
    out.join("\n")
}

fn human_ms(ms: u128) -> String {
    let secs = ms / 1000;
    if secs < 60 {
        return format!("{secs}s");
    }
    let mins = secs / 60;
    if mins < 60 {
        return format!("{}m{}s", mins, secs % 60);
    }
    format!("{}h{}m", mins / 60, mins % 60)
}

pub(crate) fn render_simple_response(response: &BatchResponse, commands: &[String]) -> String {
    let mut out = Vec::new();
    let multiple = response.results.len() > 1;

    for (index, result) in response.results.iter().enumerate() {
        if multiple {
            if !out.is_empty() {
                out.push(String::new());
            }
            out.push(simple_command_header(
                index,
                commands.get(index).map(String::as_str),
            ));
        }

        let rendered = render_simple_result(result);
        out.extend(rendered.lines().map(ToString::to_string));
    }

    out.join("\n")
}

fn simple_command_header(index: usize, command: Option<&str>) -> String {
    let Some(command) = command else {
        return format!("$ cmd {}", index + 1);
    };
    let mut lines = command.lines();
    let Some(first_line) = lines.next() else {
        return format!("$ cmd {}", index + 1);
    };

    if lines.next().is_some() || first_line.trim().is_empty() || first_line.chars().count() > 80 {
        format!("$ cmd {}", index + 1)
    } else {
        format!("$ {}", first_line.trim())
    }
}

fn render_simple_result(result: &CommandResult) -> String {
    let stdout = non_empty_text(result.stdout.as_deref());
    let stderr = non_empty_text(result.stderr.as_deref());
    let error = non_empty_text(result.error.as_deref());
    let mut out = Vec::new();

    if let Some(stdout) = stdout {
        out.push(stdout.to_string());
    }
    if let Some(stderr) = stderr {
        if stdout.is_some() || error.is_some() {
            out.push("stderr:".to_string());
        }
        out.push(stderr.to_string());
    }
    if let Some(error) = error {
        if stdout.is_some() || stderr.is_some() {
            out.push("error:".to_string());
        }
        out.push(error.to_string());
    }
    if result.stdout_truncated {
        out.push("[stdout truncated]".to_string());
    }
    if result.stderr_truncated {
        out.push("[stderr truncated]".to_string());
    }
    if result.timed_out {
        out.push(format!("[timed out after {} ms]", result.duration_ms));
    } else if !result.success {
        if let Some(exit_code) = result.exit_code {
            out.push(format!("[exit {}]", exit_code));
        } else if error.is_none() {
            out.push("[failed]".to_string());
        }
    }
    if out.is_empty() {
        out.push("[no output]".to_string());
    }
    if let Some(hint) = denial_hint(result) {
        out.push(hint);
    }

    out.join("\n")
}

fn non_empty_text(value: Option<&str>) -> Option<&str> {
    value.filter(|text| !text.is_empty())
}
