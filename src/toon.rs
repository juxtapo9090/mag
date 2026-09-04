use crate::AppResult;
use crate::cli::{BatchRequest, CommandSpec};
use crate::pool::is_valid_env_name;
use anyhow::anyhow;
use rmcp::schemars;
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;

#[derive(Debug, Clone, Deserialize, Serialize, schemars::JsonSchema)]
pub struct ToonRequest {
    #[serde(default)]
    pub script: Option<String>,
    #[serde(default)]
    pub raw: Option<String>,
    #[serde(default)]
    pub remote: Option<String>,
    #[serde(default)]
    pub session_id: Option<String>,
    /// Same shorthand as the simple lane. Carried here so a close-only call
    /// (`{"sticky": true, "close": true}` with no command) can name the shell
    /// it is releasing — without it, that request has no `session_id` and is
    /// rejected as "close requires session_id".
    #[serde(default)]
    pub sticky: Option<bool>,
    #[serde(default)]
    pub close: Option<bool>,
    #[serde(default)]
    pub env: BTreeMap<String, String>,
    #[serde(default)]
    pub default_cwd: Option<String>,
    #[serde(default)]
    pub default_timeout_ms: Option<u64>,
    #[serde(default)]
    pub max_parallel: Option<usize>,
    #[serde(default)]
    pub continue_on_error: Option<bool>,
    #[serde(default)]
    pub capture_limit_bytes: Option<usize>,
    #[serde(
        default,
        alias = "g0",
        alias = "g_0",
        alias = "g-0",
        alias = "ground_zero"
    )]
    pub unfiltered: Option<bool>,
    #[serde(default)]
    pub warm: Option<bool>,
}

pub(crate) fn parse_toon_script(script: &str) -> AppResult<BatchRequest> {
    let mut request = BatchRequest {
        commands: Vec::new(),
        env: BTreeMap::new(),
        remote: None,
        default_cwd: None,
        default_timeout_ms: None,
        max_parallel: None,
        continue_on_error: None,
        capture_limit_bytes: None,
        unfiltered: None,
    };
    let mut current: Option<CommandSpec> = None;

    let lines = script.lines().collect::<Vec<_>>();
    let mut index = 0usize;
    while index < lines.len() {
        let raw_line = lines[index];
        let line_no = index;
        let trimmed = raw_line.trim();
        let line_indent = raw_line.chars().take_while(|ch| ch.is_whitespace()).count();
        if trimmed.is_empty() || trimmed.starts_with('#') {
            index += 1;
            continue;
        }

        if let Some(rest) = trimmed.strip_prefix("job ") {
            if let Some(spec) = current.take() {
                request.commands.push(finalize_toon_command(spec, line_no)?);
            }
            let (job_name, inline_command) = rest
                .split_once(':')
                .ok_or_else(|| anyhow!("line {}: job header must contain ':'", line_no + 1))?;
            let job_name = job_name.trim();
            if job_name.is_empty() {
                return Err(anyhow!("line {}: job name must not be empty", line_no + 1));
            }
            let mut spec = CommandSpec {
                id: Some(job_name.to_string()),
                command: None,
                argv: None,
                remote: None,
                cwd: None,
                env: BTreeMap::new(),
                stdin: None,
                timeout_ms: None,
                warm: None,
            };
            if !inline_command.trim().is_empty() {
                spec.command = Some(inline_command.trim().to_string());
                spec.warm = Some(true);
            }
            current = Some(spec);
            index += 1;
            continue;
        }

        let (key, value) = parse_toon_kv(trimmed, line_no)?;
        let resolved_value = if value == "|" {
            let (block, next_index) = parse_toon_block(&lines, index + 1, line_no, line_indent)?;
            index = next_index;
            block
        } else {
            index += 1;
            value
        };
        if let Some(spec) = current.as_mut() {
            apply_toon_command_field(spec, &key, &resolved_value, line_no)?;
        } else {
            apply_toon_batch_field(&mut request, &key, &resolved_value, line_no)?;
        }
    }

    if let Some(spec) = current.take() {
        request
            .commands
            .push(finalize_toon_command(spec, script.lines().count())?);
    }

    if request.commands.is_empty() {
        return Err(anyhow!(
            "TOON script must define at least one `job <name>:` block"
        ));
    }

    Ok(request)
}

pub(crate) fn parse_raw_script(raw: &str, warm_default: bool) -> AppResult<BatchRequest> {
    let mut commands = Vec::new();

    for (line_no, raw_line) in raw.lines().enumerate() {
        let trimmed = raw_line.trim();
        if trimmed.is_empty() || trimmed.starts_with('#') {
            continue;
        }

        let (command, argv, warm) = if let Some(value) = trimmed.strip_prefix("exec:") {
            let argv = shlex::split(value.trim())
                .ok_or_else(|| anyhow!("line {}: invalid exec shellwords", line_no + 1))?;
            if argv.is_empty() {
                return Err(anyhow!("line {}: exec line must not be empty", line_no + 1));
            }
            (None, Some(argv), false)
        } else if let Some(value) = trimmed.strip_prefix("shell:") {
            let value = value.trim();
            if value.is_empty() {
                return Err(anyhow!(
                    "line {}: shell line must not be empty",
                    line_no + 1
                ));
            }
            (Some(value.to_string()), None, true)
        } else {
            (Some(trimmed.to_string()), None, warm_default)
        };

        commands.push(CommandSpec {
            id: Some(format!("raw-{}", line_no + 1)),
            command,
            argv,
            remote: None,
            cwd: None,
            env: BTreeMap::new(),
            stdin: None,
            timeout_ms: None,
            warm: Some(warm),
        });
    }

    if commands.is_empty() {
        return Err(anyhow!(
            "raw input must contain at least one non-empty command line"
        ));
    }

    Ok(BatchRequest {
        commands,
        env: BTreeMap::new(),
        remote: None,
        default_cwd: None,
        default_timeout_ms: None,
        max_parallel: None,
        continue_on_error: None,
        capture_limit_bytes: None,
        unfiltered: None,
    })
}

fn parse_toon_block(
    lines: &[&str],
    start_index: usize,
    line_no: usize,
    parent_indent: usize,
) -> AppResult<(String, usize)> {
    let mut index = start_index;
    let mut collected = Vec::new();
    let mut min_indent: Option<usize> = None;

    while index < lines.len() {
        let raw_line = lines[index];
        let trimmed = raw_line.trim();
        let indent = raw_line.chars().take_while(|ch| ch.is_whitespace()).count();

        if trimmed.is_empty() {
            collected.push(raw_line.to_string());
            index += 1;
            continue;
        }

        if indent <= parent_indent {
            if !trimmed.contains(':') {
                return Err(anyhow!(
                    "line {}: block started with `|` at line {} ended here; expected an indented continuation or a new `key: value` field",
                    index + 1,
                    line_no + 1
                ));
            }
            break;
        }

        min_indent = Some(match min_indent {
            Some(current) => current.min(indent),
            None => indent,
        });
        collected.push(raw_line.to_string());
        index += 1;
    }

    let Some(indent) = min_indent else {
        return Err(anyhow!(
            "line {}: block value `|` must be followed by at least one indented line",
            line_no + 1
        ));
    };

    let rendered = collected
        .into_iter()
        .map(|line| {
            if line.trim().is_empty() {
                String::new()
            } else {
                line.chars().skip(indent).collect::<String>()
            }
        })
        .collect::<Vec<_>>()
        .join("\n");

    Ok((rendered, index))
}

fn parse_toon_kv(line: &str, line_no: usize) -> AppResult<(String, String)> {
    let (key, value) = line
        .split_once(':')
        .ok_or_else(|| anyhow!("line {}: expected `key: value`", line_no + 1))?;
    let key = key.trim().to_string();
    let value = value.trim().to_string();
    if key.is_empty() {
        return Err(anyhow!("line {}: key must not be empty", line_no + 1));
    }
    Ok((key, value))
}

fn apply_toon_batch_field(
    request: &mut BatchRequest,
    key: &str,
    value: &str,
    line_no: usize,
) -> AppResult<()> {
    if let Some(env_name) = key.strip_prefix("env.") {
        ensure_env_name(env_name, line_no)?;
        request.env.insert(env_name.to_string(), value.to_string());
        return Ok(());
    }
    match key {
        "remote" => request.remote = Some(value.to_string()),
        "cwd" | "default_cwd" => request.default_cwd = Some(value.to_string()),
        "timeout_ms" | "default_timeout_ms" => {
            request.default_timeout_ms = Some(parse_u64(value, "default_timeout_ms", line_no)?);
        }
        "parallel" | "max_parallel" => {
            request.max_parallel = Some(parse_usize(value, "max_parallel", line_no)?);
        }
        "continue_on_error" => {
            request.continue_on_error = Some(parse_bool(value, "continue_on_error", line_no)?);
        }
        "capture_limit_bytes" => {
            request.capture_limit_bytes = Some(parse_usize(value, "capture_limit_bytes", line_no)?);
        }
        "unfiltered" | "g0" | "g_0" | "g-0" | "ground_zero" => {
            request.unfiltered = Some(parse_bool(value, "unfiltered", line_no)?);
        }
        "session_id" | "close" => return Err(envelope_field_error(key, line_no)),
        _ => {
            return Err(anyhow!(
                "line {}: unsupported batch field `{}`",
                line_no + 1,
                key
            ));
        }
    }
    Ok(())
}

fn apply_toon_command_field(
    spec: &mut CommandSpec,
    key: &str,
    value: &str,
    line_no: usize,
) -> AppResult<()> {
    if let Some(env_name) = key.strip_prefix("env.") {
        ensure_env_name(env_name, line_no)?;
        spec.env.insert(env_name.to_string(), value.to_string());
        return Ok(());
    }
    match key {
        "command" | "shell" => spec.command = Some(value.to_string()),
        "argv" => {
            spec.argv = Some(
                shlex::split(value)
                    .ok_or_else(|| anyhow!("line {}: invalid argv shellwords", line_no + 1))?,
            );
        }
        "remote" => spec.remote = Some(value.to_string()),
        "cwd" => spec.cwd = Some(value.to_string()),
        "stdin" => spec.stdin = Some(value.to_string()),
        "timeout_ms" => spec.timeout_ms = Some(parse_u64(value, "timeout_ms", line_no)?),
        "warm" => spec.warm = Some(parse_bool(value, "warm", line_no)?),
        "id" => spec.id = Some(value.to_string()),
        "session_id" | "close" => return Err(envelope_field_error(key, line_no)),
        _ => {
            return Err(anyhow!(
                "line {}: unsupported job field `{}`",
                line_no + 1,
                key
            ));
        }
    }
    Ok(())
}

fn finalize_toon_command(mut spec: CommandSpec, line_no: usize) -> AppResult<CommandSpec> {
    if spec.command.is_none() && spec.argv.is_none() {
        return Err(anyhow!(
            "line {}: each job needs either `command:`/`shell:` or `argv:`",
            line_no
        ));
    }
    if spec.command.is_some() && spec.argv.is_some() {
        return Err(anyhow!(
            "line {}: a job cannot set both `command` and `argv`",
            line_no
        ));
    }
    if spec.command.is_some() && spec.warm.is_none() {
        spec.warm = Some(true);
    }
    Ok(spec)
}

fn envelope_field_error(field: &str, line_no: usize) -> anyhow::Error {
    anyhow!(
        "line {}: `{}` is an envelope field; pass it as a top-level MCP parameter \
         sibling to `script`, not inside the TOON script",
        line_no + 1,
        field
    )
}

fn parse_bool(value: &str, field: &str, line_no: usize) -> AppResult<bool> {
    match value {
        "true" | "yes" | "on" => Ok(true),
        "false" | "no" | "off" => Ok(false),
        _ => Err(anyhow!(
            "line {}: {} must be true/false",
            line_no + 1,
            field
        )),
    }

    /* disabled misplaced test; moved below into cfg(test) module.
    fn unindented_block_body_explains_the_indentation_boundary() {
        let err = parse_toon_script("job work:\n  command: |\n    cat <<EOF\nhello\nEOF")
            .unwrap_err()
            .to_string();
        assert!(err.contains("line 4"));
        assert!(err.contains("expected an indented continuation"));
        assert!(err.contains("new `key: value` field"));
    }
    */
}

fn parse_u64(value: &str, field: &str, line_no: usize) -> AppResult<u64> {
    value.parse::<u64>().map_err(|_| {
        anyhow!(
            "line {}: {} must be an unsigned integer",
            line_no + 1,
            field
        )
    })
}

fn parse_usize(value: &str, field: &str, line_no: usize) -> AppResult<usize> {
    value.parse::<usize>().map_err(|_| {
        anyhow!(
            "line {}: {} must be an unsigned integer",
            line_no + 1,
            field
        )
    })
}

fn ensure_env_name(name: &str, line_no: usize) -> AppResult<()> {
    if is_valid_env_name(name) {
        Ok(())
    } else {
        Err(anyhow!("line {}: invalid env name `{}`", line_no + 1, name))
    }
}

#[cfg(test)]
mod tests {
    use super::parse_toon_script;

    #[test]
    fn session_id_batch_field_points_to_envelope() {
        let err = parse_toon_script(
            r"session_id: main

job work:
  command: echo hi",
        )
        .unwrap_err()
        .to_string();

        assert!(err.contains("line 1: `session_id` is an envelope field"));
        assert!(err.contains("top-level MCP parameter sibling to `script`"));
    }

    #[test]
    fn close_job_field_points_to_envelope() {
        let err = parse_toon_script(
            r"job work:
  command: echo hi
  close: true",
        )
        .unwrap_err()
        .to_string();

        assert!(err.contains("line 3: `close` is an envelope field"));
        assert!(err.contains("not inside the TOON script"));
    }

    #[test]
    fn unindented_block_body_explains_the_indentation_boundary() {
        let err = parse_toon_script("job work:\n  command: |\n    cat <<EOF\nhello\nEOF")
            .unwrap_err()
            .to_string();
        assert!(err.contains("line 4"));
        assert!(err.contains("expected an indented continuation"));
    }
}
