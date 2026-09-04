//! mag token-diet — rtk-style output filtering, resident in the daemon.
//!
//! Port of rtk-0.44.0's generic TOML filter core (`core/toml_filter.rs`) plus
//! the `never_worse` guard (`core/guard.rs`), trimmed to what a shell runner
//! needs: match the command, run the 8-stage pipeline, and return raw
//! whenever "compressed" would be bigger. No trust gating, no telemetry —
//! filters ship embedded (`filters.toml` next to this file) and are compiled
//! once per daemon via `OnceLock`.
//!
//! Lossiness is intentionally dropped: the only recoverable-loss shape rtk
//! carries is `Tail { tee_payload, .. }`, and mag's byte cap keeps the full
//! pre-truncation capture in-process anyway. A truncation is already
//! signposted by the `[truncated by mag …]` marker (rtk's tee hint, inlined).

use regex::{Regex, RegexSet};
use serde::Deserialize;
use std::collections::BTreeMap;
use std::sync::OnceLock;

const BUILTIN_TOML: &str = include_str!("../filters.toml");

// ---------------------------------------------------------------------------
// TOML schema (same shape as rtk's TomlFilterDef, minus filter_stderr)
// ---------------------------------------------------------------------------

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct MatchOutputRule {
    pattern: String,
    message: String,
    #[serde(default)]
    unless: Option<String>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct ReplaceRule {
    pattern: String,
    replacement: String,
}

#[derive(Debug, Deserialize)]
struct TomlFilterFile {
    schema_version: u32,
    #[serde(default)]
    filters: BTreeMap<String, TomlFilterDef>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct TomlFilterDef {
    #[allow(dead_code)]
    description: Option<String>,
    match_command: String,
    #[serde(default)]
    strip_ansi: bool,
    #[serde(default)]
    replace: Vec<ReplaceRule>,
    #[serde(default)]
    match_output: Vec<MatchOutputRule>,
    #[serde(default)]
    strip_lines_matching: Vec<String>,
    #[serde(default)]
    keep_lines_matching: Vec<String>,
    truncate_lines_at: Option<usize>,
    head_lines: Option<usize>,
    tail_lines: Option<usize>,
    max_lines: Option<usize>,
    on_empty: Option<String>,
}

// ---------------------------------------------------------------------------
// Compiled types
// ---------------------------------------------------------------------------

#[derive(Debug)]
struct CompiledMatchOutputRule {
    pattern: Regex,
    message: String,
    unless: Option<Regex>,
}

#[derive(Debug)]
struct CompiledReplaceRule {
    pattern: Regex,
    replacement: String,
}

#[derive(Debug)]
enum LineFilter {
    None,
    Strip(RegexSet),
    Keep(RegexSet),
}

/// A filter that has been parsed and compiled — all regexes ready.
#[derive(Debug)]
pub struct CompiledFilter {
    #[allow(dead_code)]
    pub name: String,
    match_regex: Regex,
    strip_ansi: bool,
    replace: Vec<CompiledReplaceRule>,
    match_output: Vec<CompiledMatchOutputRule>,
    line_filter: LineFilter,
    truncate_lines_at: Option<usize>,
    head_lines: Option<usize>,
    tail_lines: Option<usize>,
    max_lines: Option<usize>,
    on_empty: Option<String>,
}

fn compile_filter(name: String, def: TomlFilterDef) -> Result<CompiledFilter, String> {
    let match_regex =
        Regex::new(&def.match_command).map_err(|e| format!("match_command: {}", e))?;

    let replace = def
        .replace
        .into_iter()
        .map(|rule| {
            Regex::new(&rule.pattern)
                .map(|pattern| CompiledReplaceRule {
                    pattern,
                    replacement: rule.replacement,
                })
                .map_err(|e| format!("replace /{}/: {}", rule.pattern, e))
        })
        .collect::<Result<Vec<_>, _>>()?;

    let match_output = def
        .match_output
        .into_iter()
        .map(|rule| {
            let pattern = Regex::new(&rule.pattern)
                .map_err(|e| format!("match_output /{}/: {}", rule.pattern, e))?;
            let unless = rule
                .unless
                .map(|u| Regex::new(&u).map_err(|e| format!("unless /{}/: {}", u, e)))
                .transpose()?;
            Ok(CompiledMatchOutputRule {
                pattern,
                message: rule.message,
                unless,
            })
        })
        .collect::<Result<Vec<_>, String>>()?;

    let line_filter = match (def.strip_lines_matching, def.keep_lines_matching) {
        (strip, keep) if !strip.is_empty() && !keep.is_empty() => {
            return Err(
                "strip_lines_matching and keep_lines_matching are mutually exclusive".to_string(),
            );
        }
        (strip, _) if !strip.is_empty() => LineFilter::Strip(
            RegexSet::new(&strip).map_err(|e| format!("strip_lines_matching: {}", e))?,
        ),
        (_, keep) if !keep.is_empty() => LineFilter::Keep(
            RegexSet::new(&keep).map_err(|e| format!("keep_lines_matching: {}", e))?,
        ),
        _ => LineFilter::None,
    };

    Ok(CompiledFilter {
        name,
        match_regex,
        strip_ansi: def.strip_ansi,
        replace,
        match_output,
        line_filter,
        truncate_lines_at: def.truncate_lines_at,
        head_lines: def.head_lines,
        tail_lines: def.tail_lines,
        max_lines: def.max_lines,
        on_empty: def.on_empty,
    })
}

// ---------------------------------------------------------------------------
// Registry — compiled once per daemon, shared by every lane
// ---------------------------------------------------------------------------

pub struct DietRegistry {
    filters: Vec<CompiledFilter>,
}

impl DietRegistry {
    fn load() -> Self {
        let mut filters = Vec::new();
        match toml::from_str::<TomlFilterFile>(BUILTIN_TOML) {
            Ok(file) => {
                if file.schema_version != 1 {
                    tracing::warn!(
                        "mag diet: unsupported schema_version {} in builtin filters (expected 1)",
                        file.schema_version
                    );
                } else {
                    for (name, def) in file.filters {
                        match compile_filter(name.clone(), def) {
                            Ok(filter) => filters.push(filter),
                            Err(e) => {
                                tracing::warn!("mag diet: filter '{}' skipped: {}", name, e)
                            }
                        }
                    }
                }
            }
            Err(e) => tracing::warn!("mag diet: builtin filters parse error: {}", e),
        }
        DietRegistry { filters }
    }

    pub fn shared() -> &'static DietRegistry {
        static REGISTRY: OnceLock<DietRegistry> = OnceLock::new();
        REGISTRY.get_or_init(DietRegistry::load)
    }

    pub fn find_filter<'a>(&'a self, command: &str) -> Option<&'a CompiledFilter> {
        self.filters
            .iter()
            .find(|f| f.match_regex.is_match(command))
    }
}

// ---------------------------------------------------------------------------
// Pipeline — the 8 stages, in rtk's order
// ---------------------------------------------------------------------------

/// Apply a compiled filter pipeline to raw stdout. Pure String -> String.
///
/// Stages, in order:
///   1. strip_ansi           — remove ANSI escape codes
///   2. replace              — regex substitutions, line-by-line, chainable
///   3. match_output         — short-circuit if blob matches a pattern
///   4. strip/keep_lines     — filter lines by regex
///   5. truncate_lines_at    — truncate each line to N chars
///   6. head/tail_lines      — keep first/last N lines
///   7. max_lines            — absolute line cap
///   8. on_empty             — message if result is empty
pub fn apply_filter(filter: &CompiledFilter, stdout: &str) -> String {
    let mut lines: Vec<String> = stdout.lines().map(String::from).collect();

    // 1. strip_ansi
    if filter.strip_ansi {
        lines = lines.into_iter().map(|l| strip_ansi(&l)).collect();
    }

    // 2. replace — line-by-line, rules chained sequentially
    if !filter.replace.is_empty() {
        lines = lines
            .into_iter()
            .map(|mut line| {
                for rule in &filter.replace {
                    line = rule
                        .pattern
                        .replace_all(&line, rule.replacement.as_str())
                        .into_owned();
                }
                line
            })
            .collect();
    }

    // 3. match_output — short-circuit on full blob match (first rule wins).
    //    `unless` keeps errors/warnings from being swallowed.
    if !filter.match_output.is_empty() {
        let blob = lines.join("\n");
        for rule in &filter.match_output {
            if rule.pattern.is_match(&blob) {
                if let Some(ref unless_re) = rule.unless
                    && unless_re.is_match(&blob)
                {
                    continue;
                }
                return rule.message.clone();
            }
        }
    }

    // 4. strip OR keep (mutually exclusive, enforced at compile time)
    match &filter.line_filter {
        LineFilter::Strip(set) => lines.retain(|l| !set.is_match(l)),
        LineFilter::Keep(set) => lines.retain(|l| set.is_match(l)),
        LineFilter::None => {}
    }

    // 5. truncate_lines_at — unicode-safe
    if let Some(max_chars) = filter.truncate_lines_at {
        lines = lines
            .into_iter()
            .map(|line| truncate_chars(&line, max_chars))
            .collect();
    }

    // 6. head + tail
    let total = lines.len();
    if let (Some(head), Some(tail)) = (filter.head_lines, filter.tail_lines) {
        if total > head + tail {
            let mut result = lines[..head].to_vec();
            result.push(format!("... ({} lines omitted)", total - head - tail));
            result.extend_from_slice(&lines[total - tail..]);
            lines = result;
        }
    } else if let Some(head) = filter.head_lines {
        if total > head {
            lines.truncate(head);
            lines.push(format!("... ({} lines omitted)", total - head));
        }
    } else if let Some(tail) = filter.tail_lines {
        if total > tail {
            let omitted = total - tail;
            lines = lines[omitted..].to_vec();
            lines.insert(0, format!("... ({} lines omitted)", omitted));
        }
    }

    // 7. max_lines — absolute cap after head/tail
    if let Some(max) = filter.max_lines
        && lines.len() > max
    {
        let dropped = lines.len() - max;
        lines.truncate(max);
        lines.push(format!("... ({} lines truncated)", dropped));
    }

    // 8. on_empty
    let result = lines.join("\n");
    if result.trim().is_empty()
        && let Some(ref msg) = filter.on_empty
    {
        return msg.clone();
    }

    result
}

// ---------------------------------------------------------------------------
// Guard + estimator — the hard safety floor
// ---------------------------------------------------------------------------

/// ~4 chars per token on average (rtk's estimate; good enough for a guard).
pub fn estimate_tokens(text: &str) -> usize {
    text.len().div_ceil(4)
}

/// Returns `filtered`, or `raw` when `filtered` would emit more tokens.
/// Compression can never make things worse.
pub fn never_worse<'a>(raw: &'a str, filtered: &'a str) -> &'a str {
    if estimate_tokens(filtered) > estimate_tokens(raw) {
        raw
    } else {
        filtered
    }
}

// ---------------------------------------------------------------------------
// Small helpers (ported from rtk core/utils.rs)
// ---------------------------------------------------------------------------

pub fn strip_ansi(text: &str) -> String {
    let ansi_re = DietRegistry::shared_ansi();
    ansi_re.replace_all(text, "").to_string()
}

impl DietRegistry {
    fn shared_ansi() -> &'static Regex {
        static ANSI_RE: OnceLock<Regex> = OnceLock::new();
        ANSI_RE.get_or_init(|| Regex::new(r"\x1b\[[0-9;]*[a-zA-Z]").unwrap())
    }
}

/// Unicode-safe char-count truncation with an ellipsis.
pub fn truncate_chars(s: &str, max_len: usize) -> String {
    let char_count = s.chars().count();
    if char_count <= max_len {
        s.to_string()
    } else if max_len < 3 {
        "...".to_string()
    } else {
        format!("{}...", s.chars().take(max_len - 3).collect::<String>())
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    fn registry() -> &'static DietRegistry {
        DietRegistry::shared()
    }

    fn filter_for(command: &str) -> &'static CompiledFilter {
        registry()
            .find_filter(command)
            .unwrap_or_else(|| panic!("expected a filter for `{}`", command))
    }

    #[test]
    fn git_status_drops_hint_lines() {
        let raw = "On branch main\nYour branch is up to date with 'origin/main'.\n\nChanges not staged for commit:\n  (use \"git add <file>...\" to update what will be committed)\n  (use \"git restore <file>...\" to discard changes in working directory)\n\tmodified:   src/main.rs\n\nno changes added to commit (use \"git add\" and/or \"git commit -a\")\n";
        let out = apply_filter(filter_for("git status"), raw);
        assert!(!out.contains("(use \""));
        assert!(out.contains("modified:   src/main.rs"));
        assert!(out.contains("On branch main"));
    }

    #[test]
    fn git_status_clean_short_circuits() {
        let raw = "On branch main\nYour branch is up to date with 'origin/main'.\n\nnothing to commit, working tree clean\n";
        let out = apply_filter(filter_for("git status"), raw);
        assert_eq!(out, "ok — working tree clean");
    }

    #[test]
    fn cargo_build_clean_short_circuits_unless_warnings() {
        let raw = "   Compiling mag v0.1.0 (/home/juxtapo/Obito/workshop/mag)\n    Finished `dev` profile [unoptimized + debuginfo] target(s) in 6.12s\n";
        let out = apply_filter(filter_for("cargo build"), raw);
        assert_eq!(out, "ok — build clean");

        let warned = "   Compiling mag v0.1.0\nwarning: unused variable `x`\n    Finished `dev` profile target(s) in 6.12s\n";
        let out2 = apply_filter(filter_for("cargo build"), warned);
        assert!(out2.contains("warning: unused variable `x`"));
        assert_ne!(out2, "ok — build clean");
    }

    #[test]
    fn never_worse_falls_back_to_raw() {
        let raw = "{}";
        let filtered = "{\n  \"pretty\": true\n}";
        assert_eq!(never_worse(raw, filtered), raw);
        assert_eq!(never_worse("aaaaaaaa", "ok"), "ok");
    }

    #[test]
    fn unmatched_command_passes_through_find() {
        assert!(registry().find_filter("my-weird-tool --flag").is_none());
    }

    #[test]
    fn grep_like_caps_lines() {
        let mut raw = String::new();
        for i in 0..50 {
            raw.push_str(&format!("src/main.rs:{}: match here\n", i));
        }
        let out = apply_filter(filter_for("grep -rn match src/"), &raw);
        assert!(out.contains("lines truncated"));
        assert!(out.lines().count() <= 41);
    }

    #[test]
    fn ls_la_preserves_long_listing() {
        let raw = "total 48\ndrwxr-xr-x  5 kim kim 4096 Jul 28 10:00 .\n-rw-r--r--  1 kim kim  816 Jul 28 10:00 Cargo.toml\n";
        assert!(registry().find_filter("ls -la").is_none());
        assert_eq!(raw.trim_end(), raw.trim_end());
    }

    #[test]
    fn ls_long_options_are_not_filtered() {
        assert!(registry().find_filter("ls -l").is_none());
        assert!(registry().find_filter("ls -la /tmp").is_none());
        assert!(registry().find_filter("ls -alh").is_none());
        assert!(registry().find_filter("ls -a /tmp").is_some());
        assert!(registry().find_filter("ls /tmp").is_some());
    }

    #[test]
    fn estimate_tokens_scales_with_chars() {
        assert_eq!(estimate_tokens(""), 0);
        assert_eq!(estimate_tokens("abcd"), 1);
        assert_eq!(estimate_tokens("abcde"), 2);
    }
}
