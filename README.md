<p align="center">
  <strong>mag</strong><br>
  <em>Resident warm-pool shell runner for AI coding agents</em>
</p>

<p align="center">
  <img alt="Language" src="https://img.shields.io/badge/Rust-2024_edition-b7410e?style=flat-square&logo=rust">
  <img alt="License" src="https://img.shields.io/badge/license-MIT-blue?style=flat-square">
  <img alt="MCP" src="https://img.shields.io/badge/MCP-compatible-8B5CF6?style=flat-square">
</p>

---

**mag** keeps a pool of warm bash shells behind a single MCP tool. One call, ~5 ms, with built-in token-diet compression that cuts agent output by up to 85% on known commands — without losing facts.

Born from a simple frustration: every shell command an AI agent runs spawns a cold process, waits for it, and dumps the full raw output back into context. mag eliminates all three costs.

## Why mag

| Problem | mag's answer |
|---------|-------------|
| Cold shell spawn per command | Pre-warmed pool, ~5–10 ms round-trip |
| Raw output floods agent context | Token-diet: rtk-style filters on known commands (git, cargo, docker, systemctl…) |
| No persistent shell state | Named sessions — env, cwd, functions survive across calls |
| Can't watch what the agent is doing | Per-seat tmux terminals with live attach |
| Parallel jobs need scripting | TOON format: named jobs, parallel or sequential, multiline-safe |
| Agent can't tell if output was compressed | `diet` flag in every response header — always honest |

## Quick start

```bash
# Build
cargo build --release

# Run the daemon
mag --serve --config mag.yaml.example

# Smoke test (CLI)
mag run-toon --script 'job hi: echo hello'
```

As an MCP tool (`mcp__mag__mag`), a single JSON object drives everything:

```json
{"cmd": "git status", "cwd": "/your/repo"}
```

## The two lanes

mag has two lanes. Pick one per call — mixing them is an error.

### Simple lane (use this 95% of the time)

```json
{"cmd": "cargo test"}
{"cmds": ["cargo fmt --check", "cargo clippy", "cargo test"]}
```

Sequential execution. Token-diet fires automatically on known commands. The `diet` flag in the response header tells you when it did.

### Power lane

For parallel jobs, multiline scripts, remote execution, or when you need fine control:

**TOON** — named jobs, multiline-safe:

```
max_parallel: 3

job lint:
  command: |
    cargo fmt --check
    cargo clippy
  cwd: /repo

job test:
  command: cargo test
  timeout_ms: 60000
```

**Raw** — one command per line, parallel by default:

```
curl -s https://api.a.com/health
curl -s https://api.b.com/health
curl -s https://api.c.com/health
```

## Token diet

Known commands (git, cargo, grep, rg, find, ls, docker, systemctl, journalctl — see [`filters.toml`](filters.toml)) get an rtk-style filter pass:

- `git status` on a clean repo → `ok — working tree clean` (not 4 lines of boilerplate)
- Build output with 200 warnings → capped flood, key errors preserved
- Per-stream echo cap (default 8 KiB) with `[truncated by mag — N more bytes]`

The `never_worse` guard means filtered output is **never larger** than raw. Unmatched commands pass through untouched.

Need the raw truth? Pass `"unfiltered": true` (or `"g0": true`) to bypass everything except the byte cap.

## Persistent sessions

Pin a warm shell to your seat — env, cwd, functions persist across calls:

```json
{"cmd": "export DB=prod", "session_id": "deploy"}
{"cmd": "echo $DB",       "session_id": "deploy"}
```

Or use `"sticky": true` for a no-bookkeeping default session. Sessions are namespaced per seat, so everyone's shell is their own.

```json
{"cmd": "cd /var/log", "sticky": true}
{"cmd": "pwd",         "sticky": true}
```

Close with `{"session_id": "deploy", "close": true}`. Idle sessions are reaped automatically.

## Watchable terminals

A real tmux session that humans can attach to and watch live:

```json
{"terminal": {"action": "open", "seat": "celeste"}}
{"terminal": {"seat": "celeste", "action": "send", "input": "htop"}}
{"terminal": {"seat": "celeste", "action": "snapshot", "lines": 40}}
```

The terminal branch is sequential (one PTY, like a human typing). Use `cmd`/`cmds` when you need speed or parallelism — use `terminal` when someone needs to watch.

## Response format

Clean, one header per result:

```
[mag cmd-1] exit=0 dur=7ms diet
hello world
```

Flags appear only when true: `diet`, `trunc-out`, `trunc-err`, `timeout`, `failed`. Multi-job runs get a batch summary: `[mag] jobs=3 elapsed=11ms`.

## Architecture

```
AI agent (Claude Code, Codex, etc.)
    │
    │  MCP tool call
    ▼
mag --lane forward          ← per-seat forwarder, inherits agent's uid
    │
    │  HTTP over unix socket (/run/mag/<seat>.sock)
    ▼
mag --serve                 ← resident daemon, owns the warm pool
    │
    ├── pool.rs             warm shell pool (pre-spawned, recycled)
    ├── diet.rs             token-diet filters (rtk-style)
    ├── toon.rs             TOON script parser
    ├── term.rs             tmux terminal integration
    ├── audit.rs            JSONL audit log
    └── hint.rs             refusal hints (permission denied → who ran it, can they sudo?)
```

Two processes by design. Identity comes from the socket, not a header — unforgeable. The daemon runs commands; the forwarder renders output. A restart of the daemon does **not** update live forwarders — reconnect the MCP server to pick up render-layer changes.

## Deployment

mag is socket-activated via systemd. The template unit runs one instance per uid:

```bash
sudo install -m 755 target/release/mag /usr/local/bin/mag
sudo cp mag.yaml.example /etc/mag/mag.yaml    # edit to taste
sudo systemctl enable --now mag@root.service
sudo systemctl enable --now mag@juxtapo.service
```

See [`mag.service`](mag.service) for the unit template and [`mag.yaml.example`](mag.yaml.example) for config.

## Claude Code integration

mag ships with skills and tools for [Claude Code](https://docs.anthropic.com/en/docs/claude-code):

| Skill | What it does |
|-------|-------------|
| [`skills/mag`](skills/mag/SKILL.md) | Full reference — two lanes, diet, sessions, terminals, gotchas |
| [`skills/mag-p`](skills/mag-p/SKILL.md) | Persistent terminal check + stdout pipe — "I'm already SSH'd in, look at my pane first" |

### mag-term-exec

Companion script that closes the terminal lane's stdout gap: fire a command on a live `mag term` tmux session, wait for it to finish, and pipe the output back to the agent.

```bash
mag-term-exec 'cargo build --release'              # runs on mag-house
mag-term-exec 'make test' --seat celeste            # runs on mag-celeste
mag-term-exec 'npm run build' --timeout 300         # 5-minute timeout
```

The agent sees the result; the human watches it live in their tmux pane. No timeout limit from the terminal side — the default Bash tool's 2-minute kill doesn't apply.

**How it works:** wraps the command in echo start/end markers, sends via `mag term send`, polls `mag term snapshot` until the end marker appears, extracts output with awk. Shell-agnostic (fish, bash, zsh).

Install: `cp mag-term-exec /usr/local/bin/ && chmod +x /usr/local/bin/mag-term-exec`

### Hook: mag terminal awareness

Add to your Claude Code `UserPromptSubmit` hook to inject live terminal session status into every prompt — the agent knows which `mag-<seat>` sessions are up before responding:

```
🖥️ mag terminals: celeste, house, monica
```

Zero discovery steps. See [`skills/mag-p/SKILL.md`](skills/mag-p/SKILL.md) for the snippet.

Copy skills to `~/.claude/skills/` and they load on demand.

## Project structure

```
src/
├── main.rs          entry point
├── lib.rs           library root
├── serve.rs         MCP server + HTTP listener
├── engine.rs        core execution engine
├── pool.rs          warm shell pool manager
├── diet.rs          token-diet output filtering
├── toon.rs          TOON batch script parser
├── term.rs          tmux terminal integration
├── cli.rs           CLI argument parsing
├── exec.rs          command execution
├── config.rs        config loading (YAML)
├── audit.rs         JSONL audit logging
├── hint.rs          refusal hint system
├── render.rs        output rendering
├── remote.rs        remote node forwarding
├── mcp.rs           MCP protocol helpers
├── client.rs        client mode
└── bin/
    └── mag-client.rs
filters.toml         token-diet filter definitions
mag.yaml.example     sample daemon config
mag.service          systemd unit template
mag-how-to.md        full manual
examples/            TOON samples + smoke tests
skills/              Claude Code skills
```

## License

[MIT](LICENSE)

---

<sub>Built by [juxtapo9090](https://github.com/juxtapo9090). mag started as a weekend hack to stop watching AI agents waste tokens on `git status` boilerplate. It grew.</sub>
