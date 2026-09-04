use crate::AppResult;
use crate::cli::{BatchRequest, BatchResponse, CommandResult, CommandSpec};
use serde::{Deserialize, Serialize};
use std::fs::{self, OpenOptions};
use std::io::Write;
use std::path::PathBuf;
use std::time::{SystemTime, UNIX_EPOCH};

const DEFAULT_AUDIT_CMD_BYTES: usize = 512;

#[derive(Debug, Clone, Deserialize)]
pub(crate) struct ServeAuditConfig {
    #[serde(default)]
    pub(crate) enabled: bool,
    #[serde(default = "default_audit_path")]
    pub(crate) path: PathBuf,
    #[serde(default = "default_audit_cmd_bytes")]
    pub(crate) cmd_bytes: usize,
    #[serde(default)]
    pub(crate) rotate_bytes: Option<u64>,
}

impl Default for ServeAuditConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            path: default_audit_path(),
            cmd_bytes: DEFAULT_AUDIT_CMD_BYTES,
            rotate_bytes: None,
        }
    }
}

#[derive(Debug, Clone)]
pub struct AuditRuntimeConfig {
    pub enabled: bool,
    pub path: PathBuf,
    pub cmd_bytes: usize,
    pub rotate_bytes: Option<u64>,
}

impl From<ServeAuditConfig> for AuditRuntimeConfig {
    fn from(config: ServeAuditConfig) -> Self {
        Self {
            enabled: config.enabled,
            path: config.path,
            cmd_bytes: config.cmd_bytes.max(64),
            rotate_bytes: config.rotate_bytes,
        }
    }
}

#[derive(Debug, Serialize)]
struct AuditRecord<'a> {
    ts: String,
    seat: &'a str,
    session_id: &'a str,
    job: String,
    cmd: String,
    cwd: String,
    exit: Option<i32>,
    dur_ms: u128,
    flags: Vec<&'static str>,
}

pub(crate) fn append_batch_audit(
    config: &AuditRuntimeConfig,
    seat: &str,
    session_id: Option<&str>,
    batch: &BatchRequest,
    response: &BatchResponse,
) -> AppResult<()> {
    if !config.enabled {
        return Ok(());
    }

    if let Some(parent) = config.path.parent() {
        fs::create_dir_all(parent)?;
    }
    rotate_if_needed(config)?;

    let mut file = OpenOptions::new()
        .create(true)
        .append(true)
        .open(&config.path)?;
    let ts = utc_ts_string();
    let fallback_cwd = batch
        .default_cwd
        .clone()
        .or_else(|| current_cwd_string().ok())
        .unwrap_or_else(|| "-".to_string());

    for result in &response.results {
        let spec = batch.commands.get(result.index);
        let (cmd, cmd_truncated) = spec.map_or_else(
            || ("-".to_string(), false),
            |spec| command_preview(spec, config.cmd_bytes),
        );
        let mut flags = result_flags(result);
        if cmd_truncated && !flags.contains(&"trunc") {
            flags.push("trunc");
        }
        let record = AuditRecord {
            ts: ts.clone(),
            seat,
            session_id: session_id.unwrap_or("-"),
            job: result
                .id
                .clone()
                .or_else(|| spec.and_then(|spec| spec.id.clone()))
                .unwrap_or_else(|| format!("job-{}", result.index + 1)),
            cmd,
            cwd: spec
                .and_then(|spec| spec.cwd.clone())
                .unwrap_or_else(|| fallback_cwd.clone()),
            exit: result.exit_code,
            dur_ms: result.duration_ms,
            flags,
        };
        serde_json::to_writer(&mut file, &record)?;
        file.write_all(b"\n")?;
    }

    Ok(())
}

fn default_audit_path() -> PathBuf {
    PathBuf::from("/var/lib/mag/audit.jsonl")
}

fn default_audit_cmd_bytes() -> usize {
    DEFAULT_AUDIT_CMD_BYTES
}

fn rotate_if_needed(config: &AuditRuntimeConfig) -> AppResult<()> {
    let Some(limit) = config.rotate_bytes.filter(|limit| *limit > 0) else {
        return Ok(());
    };
    let Ok(metadata) = fs::metadata(&config.path) else {
        return Ok(());
    };
    if metadata.len() < limit {
        return Ok(());
    }

    let rotated = config.path.with_extension(
        config
            .path
            .extension()
            .and_then(|value| value.to_str())
            .map_or_else(|| "1".to_string(), |ext| format!("{ext}.1")),
    );
    if rotated.exists() {
        fs::remove_file(&rotated)?;
    }
    fs::rename(&config.path, rotated)?;
    Ok(())
}

fn result_flags(result: &CommandResult) -> Vec<&'static str> {
    let mut flags = Vec::new();
    if result.timed_out {
        flags.push("timeout");
    }
    if result.stdout_truncated || result.stderr_truncated {
        flags.push("trunc");
    }
    if !result.success {
        flags.push("failed");
    }
    flags
}

fn command_preview(spec: &CommandSpec, limit: usize) -> (String, bool) {
    let raw = spec
        .command
        .as_deref()
        .map(first_nonempty_line)
        .or_else(|| spec.argv.as_ref().map(|argv| argv.join(" ")))
        .unwrap_or_else(|| "-".to_string());
    truncate_bytes(&redact_export_assignments(&raw), limit)
}

fn first_nonempty_line(command: &str) -> String {
    command
        .lines()
        .map(str::trim)
        .find(|line| !line.is_empty())
        .unwrap_or("")
        .to_string()
}

fn redact_export_assignments(line: &str) -> String {
    let trimmed = line.trim_start();
    let Some(rest) = trimmed.strip_prefix("export ") else {
        return line.to_string();
    };
    let leading = &line[..line.len() - trimmed.len()];
    let redacted = rest
        .split_whitespace()
        .map(|part| {
            if let Some((name, _)) = part.split_once('=') {
                format!("{name}=[redacted]")
            } else {
                part.to_string()
            }
        })
        .collect::<Vec<_>>()
        .join(" ");
    format!("{leading}export {redacted}")
}

fn truncate_bytes(value: &str, limit: usize) -> (String, bool) {
    if value.len() <= limit {
        return (value.to_string(), false);
    }
    let mut end = 0usize;
    for (index, ch) in value.char_indices() {
        let next = index + ch.len_utf8();
        if next > limit {
            break;
        }
        end = next;
    }
    (format!("{} [truncated]", &value[..end]), true)
}

fn utc_ts_string() -> String {
    let seconds = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|value| value.as_secs())
        .unwrap_or_default();
    format_utc_seconds(seconds)
}

fn format_utc_seconds(seconds: u64) -> String {
    let days = i64::try_from(seconds / 86_400).unwrap_or(i64::MAX);
    let seconds_of_day = seconds % 86_400;
    let (year, month, day) = civil_from_days(days);
    let hour = seconds_of_day / 3_600;
    let minute = (seconds_of_day % 3_600) / 60;
    let second = seconds_of_day % 60;
    format!("{year:04}-{month:02}-{day:02}T{hour:02}:{minute:02}:{second:02}Z")
}

fn civil_from_days(days_since_epoch: i64) -> (i64, i64, i64) {
    let days = days_since_epoch + 719_468;
    let era = if days >= 0 { days } else { days - 146_096 } / 146_097;
    let day_of_era = days - era * 146_097;
    let year_of_era =
        (day_of_era - day_of_era / 1_460 + day_of_era / 36_524 - day_of_era / 146_096) / 365;
    let day_of_year = day_of_era - (365 * year_of_era + year_of_era / 4 - year_of_era / 100);
    let month_phase = (5 * day_of_year + 2) / 153;
    let day = day_of_year - (153 * month_phase + 2) / 5 + 1;
    let month = month_phase + if month_phase < 10 { 3 } else { -9 };
    let year = year_of_era + era * 400 + i64::from(month <= 2);
    (year, month, day)
}

fn current_cwd_string() -> AppResult<String> {
    Ok(std::env::current_dir()?.display().to_string())
}

#[cfg(test)]
mod tests {
    use super::{
        AuditRuntimeConfig, append_batch_audit, command_preview, format_utc_seconds,
        redact_export_assignments, truncate_bytes,
    };
    use crate::cli::{BatchRequest, BatchResponse, CommandResult, CommandSpec};
    use std::collections::BTreeMap;
    use std::fs;
    use std::time::{SystemTime, UNIX_EPOCH};

    #[test]
    fn redact_export_assignments_hides_values() {
        assert_eq!(
            redact_export_assignments("export TOKEN=abc PATH=/bin keep"),
            "export TOKEN=[redacted] PATH=[redacted] keep"
        );
    }

    #[test]
    fn command_preview_uses_first_line_and_caps_bytes() {
        let spec = CommandSpec {
            id: Some("plant".to_string()),
            command: Some("echo first\nexport TOKEN=secret".to_string()),
            argv: None,
            remote: None,
            cwd: None,
            env: BTreeMap::new(),
            stdin: None,
            timeout_ms: None,
            warm: Some(true),
        };

        assert_eq!(
            command_preview(&spec, 80),
            ("echo first".to_string(), false)
        );
        assert_eq!(
            truncate_bytes("abcdef", 3),
            ("abc [truncated]".to_string(), true)
        );
    }

    #[test]
    fn append_batch_audit_writes_one_json_line_per_result() {
        let path = std::env::temp_dir().join(format!(
            "mag-audit-test-{}.jsonl",
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        let config = AuditRuntimeConfig {
            enabled: true,
            path: path.clone(),
            cmd_bytes: 80,
            rotate_bytes: None,
        };
        let batch = BatchRequest {
            commands: vec![CommandSpec {
                id: Some("plant".to_string()),
                command: Some("export TOKEN=secret echo ok".to_string()),
                argv: None,
                remote: None,
                cwd: Some("/tmp".to_string()),
                env: BTreeMap::new(),
                stdin: None,
                timeout_ms: None,
                warm: Some(true),
            }],
            env: BTreeMap::new(),
            remote: None,
            default_cwd: None,
            default_timeout_ms: None,
            max_parallel: None,
            continue_on_error: None,
            capture_limit_bytes: None,
            unfiltered: None,
        };
        let response = BatchResponse {
            command_count: 1,
            elapsed_ms: 4,
            stopped_early: false,
            results: vec![CommandResult {
                index: 0,
                id: Some("plant".to_string()),
                mode: "warm".to_string(),
                success: true,
                exit_code: Some(0),
                timed_out: false,
                duration_ms: 3,
                stdout: Some("ok".to_string()),
                stderr: None,
                stdout_truncated: false,
                stderr_truncated: false,
                diet_applied: false,
                exec_user: None,
                error: None,
            }],
        };

        append_batch_audit(&config, "kim", Some("brrr"), &batch, &response).unwrap();

        let raw = fs::read_to_string(&path).unwrap();
        let rows = raw.lines().collect::<Vec<_>>();
        assert_eq!(rows.len(), 1);
        let row: serde_json::Value = serde_json::from_str(rows[0]).unwrap();
        assert_eq!(row["seat"], "kim");
        assert_eq!(row["session_id"], "brrr");
        assert_eq!(row["job"], "plant");
        assert_eq!(row["cmd"], "export TOKEN=[redacted] echo ok");
        assert_eq!(row["cwd"], "/tmp");
        assert_eq!(row["exit"], 0);
        assert_eq!(row["flags"].as_array().unwrap().len(), 0);

        let _ = fs::remove_file(path);
    }

    #[test]
    fn format_utc_seconds_uses_iso_utc_shape() {
        assert_eq!(format_utc_seconds(0), "1970-01-01T00:00:00Z");
        assert_eq!(format_utc_seconds(86_400), "1970-01-02T00:00:00Z");
    }
}
