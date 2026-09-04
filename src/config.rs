use crate::cli::Cli;
use crate::engine::EngineConfig;
use crate::remote::{NodeRegistry, NodeRegistryFile, RemoteHttpTarget};
use crate::serve::{CallbackNotifyOn, ServeRuntimeConfig};
use crate::{AppResult, serve::ServeFileConfig};
use anyhow::anyhow;
use std::collections::BTreeMap;
use std::fs;
use std::net::IpAddr;
use std::path::PathBuf;
use std::sync::Arc;

pub(crate) fn load_node_registry(cli: &Cli) -> AppResult<Option<Arc<NodeRegistry>>> {
    let registry_path = if let Some(path) = cli.nodes.as_ref() {
        Some(path.clone())
    } else {
        default_node_registry_path()
    };

    let Some(path) = registry_path else {
        return Ok(None);
    };

    let raw = fs::read_to_string(&path)?;
    let parsed = serde_yaml::from_str::<NodeRegistryFile>(&raw)?;
    if parsed.nodes.is_empty() {
        return Err(anyhow!(
            "node registry `{}` does not define any `nodes` entries",
            path.display()
        ));
    }

    let mut nodes = BTreeMap::new();
    for (name, entry) in parsed.nodes {
        let key = name.trim().to_string();
        let host = entry.host.trim().to_string();
        if key.is_empty() || host.is_empty() {
            return Err(anyhow!(
                "node registry `{}` contains an entry with empty name or host",
                path.display()
            ));
        }
        if host.contains("://") {
            return Err(anyhow!(
                "node registry `{}` entry `{}` should use plain host/IP plus `port`, not a URL",
                path.display(),
                key
            ));
        }

        let auth_token = if entry.auth_token.trim().is_empty() {
            None
        } else {
            Some(entry.auth_token)
        };
        nodes.insert(
            key.clone(),
            RemoteHttpTarget {
                name: key,
                base_url: format!("http://{}:{}", host, entry.port),
                auth_token,
            },
        );
    }

    Ok(Some(Arc::new(NodeRegistry { nodes })))
}

pub(crate) fn default_node_registry_path() -> Option<PathBuf> {
    ["/etc/mag/nodes.yaml", "/etc/mag/nodes.yml"]
        .into_iter()
        .map(PathBuf::from)
        .find(|path| path.exists())
}

pub(crate) fn load_serve_runtime(cli: &Cli) -> AppResult<(ServeRuntimeConfig, EngineConfig)> {
    let config_path = cli
        .config
        .as_ref()
        .ok_or_else(|| anyhow!("--serve requires --config <path>"))?;
    let raw = fs::read_to_string(config_path)?;
    let parsed = serde_yaml::from_str::<ServeFileConfig>(&raw)?;
    let node_name = parsed.node.name.trim().to_string();
    let node_id = parsed.node.id.trim().to_string();
    let node_region = parsed.node.region.trim().to_string();
    let node_role = parsed.node.role.trim().to_string();

    if node_name.is_empty() || node_id.is_empty() || node_region.is_empty() || node_role.is_empty()
    {
        return Err(anyhow!(
            "serve mode requires non-empty node.name, node.id, node.region, and node.role"
        ));
    }

    let bind_ip = parsed
        .serve
        .bind
        .parse::<IpAddr>()
        .map_err(|err| anyhow!("invalid serve.bind `{}`: {}", parsed.serve.bind, err))?;

    match bind_ip {
        IpAddr::V4(ipv4) if ipv4.octets()[0] == 10 || ipv4.is_loopback() => {}
        IpAddr::V4(ipv4) => {
            return Err(anyhow!(
                "serve.bind must be loopback or the WireGuard 10.x lane, got `{}`",
                ipv4
            ));
        }
        IpAddr::V6(ipv6) if ipv6.is_loopback() => {}
        IpAddr::V6(ipv6) => {
            return Err(anyhow!(
                "serve.bind must be loopback or an IPv4 WireGuard address, got `{}`",
                ipv6
            ));
        }
    }

    let allowed_command_prefixes = parsed
        .execution
        .allowed_command_prefixes
        .into_iter()
        .map(|value| value.trim().to_string())
        .filter(|value| !value.is_empty())
        .collect::<Vec<_>>();
    let allowed_argv = parsed
        .execution
        .allowed_argv
        .into_iter()
        .map(|value| value.trim().to_string())
        .filter(|value| !value.is_empty())
        .collect::<Vec<_>>();

    if allowed_command_prefixes.is_empty() && allowed_argv.is_empty() {
        tracing::warn!(
            "mag serve execution allowlist EMPTY — open lane. Fine on trusted loopback;              set execution.allowed_command_prefixes / allowed_argv before exposing beyond this box."
        );
    }

    let workers = parsed.serve.workers.unwrap_or(cli.workers).max(1);
    let default_timeout_ms = parsed
        .execution
        .default_timeout_ms
        .unwrap_or(cli.default_timeout_ms);
    let max_capture_bytes = parsed
        .execution
        .max_capture_bytes
        .unwrap_or(cli.capture_limit_bytes)
        .max(256);
    let token_diet_enabled = parsed.execution.token_diet_enabled;
    let token_diet_echo_max_bytes = parsed
        .execution
        .token_diet_echo_max_bytes
        .unwrap_or(crate::DEFAULT_DIET_ECHO_MAX_BYTES)
        .max(256);
    let auth_token = if parsed.serve.auth_token.trim().is_empty() {
        tracing::warn!(
            "mag serve auth DISABLED (empty auth_token) — acceptable only on trusted loopback/WireGuard lanes"
        );
        None
    } else {
        Some(parsed.serve.auth_token)
    };
    let callback_notify_url = if parsed.callback.notify_url.trim().is_empty() {
        None
    } else {
        Some(parsed.callback.notify_url.trim().to_string())
    };
    let callback_notify_on = match parsed.callback.notify_on.trim() {
        "" | "completed" => CallbackNotifyOn::Completed,
        "failed" => CallbackNotifyOn::Failed,
        "both" => CallbackNotifyOn::Both,
        other => {
            return Err(anyhow!(
                "callback.notify_on must be `completed`, `failed`, or `both`, got `{}`",
                other
            ));
        }
    };
    let callback_auth_token = if parsed.callback.auth_token.trim().is_empty() {
        None
    } else {
        Some(parsed.callback.auth_token)
    };

    let serve = ServeRuntimeConfig {
        node_name,
        // (see clamp_max_named below — the pool must never be fully pinnable)
        node_id,
        node_region,
        node_role,
        bind_ip,
        port: parsed.serve.port,
        workers,
        auth_token,
        default_timeout_ms: Some(default_timeout_ms),
        max_capture_bytes: Some(max_capture_bytes),
        default_cwd: parsed.execution.default_cwd,
        allowed_command_prefixes,
        allowed_argv,
        callback_notify_url,
        callback_notify_on,
        callback_auth_token,
        session_idle_timeout_ms: parsed.session.idle_timeout_ms,
        session_reap_interval_ms: parsed.session.reap_interval_ms.max(1_000),
        session_max_named: Some(clamp_max_named(parsed.session.max_named, workers)),
        audit: parsed.audit.into(),
        token_diet_enabled,
        token_diet_echo_max_bytes,
    };
    let engine = EngineConfig {
        workers,
        default_timeout_ms,
        capture_limit_bytes: max_capture_bytes,
        node_registry: None,
        token_diet_enabled,
        token_diet_echo_max_bytes,
    };

    Ok((serve, engine))
}

/// Warm shells reserved for callers who did NOT ask for a session. Named
/// sessions pin a slot until closed or reaped; unnamed calls can only run in a
/// slot nobody has pinned. Without a floor, enough named sessions starve the
/// plain `cmd`/`cmds` lane completely — it fails with "no unclaimed warm shell
/// available", which is the pool talking, not an answer anyone can act on.
const RESERVED_UNNAMED_SLOTS: usize = 2;

/// Cap named sessions below the worker count so the pool can never be fully
/// pinned. A config asking for more is clamped, not honoured: `max_named: 12`
/// against `workers: 10` could never have been reached anyway — the pool ran
/// out first and reported the wrong reason.
fn clamp_max_named(requested: Option<usize>, workers: usize) -> usize {
    let ceiling = workers.saturating_sub(RESERVED_UNNAMED_SLOTS).max(1);
    match requested {
        Some(value) if value > ceiling => {
            tracing::warn!(
                "session.max_named {} exceeds what {} workers can pin while keeping {} free for \
                 unnamed calls; clamped to {}",
                value,
                workers,
                RESERVED_UNNAMED_SLOTS,
                ceiling
            );
            ceiling
        }
        Some(value) => value,
        None => ceiling,
    }
}

#[cfg(test)]
mod max_named_tests {
    use super::{RESERVED_UNNAMED_SLOTS, clamp_max_named};

    #[test]
    fn clamps_a_config_that_would_starve_the_unnamed_lane() {
        // The live config shipped exactly this shape: more sessions permitted
        // than there are shells to pin.
        assert_eq!(clamp_max_named(Some(12), 10), 8);
    }

    #[test]
    fn leaves_a_sane_config_alone() {
        assert_eq!(clamp_max_named(Some(4), 10), 4);
    }

    #[test]
    fn defaults_to_the_ceiling_when_unset() {
        assert_eq!(clamp_max_named(None, 10), 10 - RESERVED_UNNAMED_SLOTS);
    }

    #[test]
    fn always_leaves_room_to_pin_at_least_one_session() {
        // A tiny pool must still allow one named session rather than zero.
        assert_eq!(clamp_max_named(Some(5), 1), 1);
        assert_eq!(clamp_max_named(None, 2), 1);
    }
}
