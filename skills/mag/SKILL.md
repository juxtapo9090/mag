---
name: mag
description: How to drive the mag shell runner (mcp__mag__mag) correctly — the two lanes, when mag beats Bash, the token-diet that only fires on the simple lane, persistent sessions, the per-seat watchable terminal branch (terminal.seat, a real tmux session), and the gotchas that bite (raw is line-oriented, no heredocs, mag's uid depends on which of the two per-uid instances your seat's socket reaches, terminal is sequential not parallel). Invoke when using the mag MCP tool, when a mag call errored ("requires script or raw", diet not firing, heredoc mangled), when deciding between Bash and mag for shell work, or when you want a persistent shell someone can actually watch.
---

# ⚙️ mag — the resident warm-pool shell runner

`mcp__mag__mag` is a pair of systemd-resident daemons (`mag@root` + `mag@juxtapo`) holding
warm bash shells, reached over a per-seat unix socket in `/run/mag/`. Runs shell jobs at
~5–10ms latency, with batch/parallel orchestration, persistent sessions, and an rtk-style
**token-diet** on known commands. Authoritative manual: `/etc/mag/mag-how-to.md` (or
`mag --how`) — read it if this skill and reality disagree.

## mag vs Bash

- **mag is the default shell lane.** It bypasses the rtk hook entirely (it's an MCP tool,
  not a Bash call), and its diet **marks itself** (`diet` flag in the header) so you always
  know when output was compressed.
- **Bash's rtk hook rewrites output with no marker that it fired** — a compressed `grep -c`
  result or a rewritten `diff` line looks identical to the real thing. Fidelity trap on any
  command where you parse the output.
- **Use `{"cmd": "..."}` for known commands** — the diet fires (and says so). For
  **verification work** where output must be exact (`diff`, `grep -c`, `stat`, anything you
  parse precisely), pass `"unfiltered": true` or use the `raw` lane.
- 🔴 **Which uid mag runs as depends on your seat.** Two instances split by uid:

  | instance | your commands run as |
  |---|---|
  | `mag@root` | `root` |
  | `mag@juxtapo` | `juxtapo` |

  **Never assume; measure with `{"cmd": "id"}`** — 6ms, the only honest answer.
- **From a juxtapo-instance seat**, `sudo` is `NOPASSWD: all`:
  ```
  {"cmd": "sudo -n ls /root/"}
  ```
  Use `-n` (never prompt) so a misconfigured sudo fails fast instead of hanging. From a
  **root**-instance seat you already are root — mag is not a safer lane there, just faster.
  **Still prefer the Write tool for file content** — mag's `raw` lane shreds heredocs
  line-by-line.
- **Fall back to Bash / `rtk proxy`** when mag is down (`systemctl is-active mag@root
  mag@juxtapo`), you need a one-shot the MCP envelope can't express, or mag itself errors.

### `Permission denied` is a symptom, not a diagnosis

An error is scoped to **the exact syscall that produced it**. Read/list/write/traverse are
four separate bits — a directory can be traversable but not listable (`ls` fails, `cat` on
a known file inside works fine). Don't assume a wider wall than the one syscall showed you.

Four cheap probes, ~30ms, before concluding anything:
```
{"cmds": ["id", "sudo -n true && echo CAN-ESCALATE",
          "stat -c '%A %U:%G' <path>", "getfacl -p <path>"]}
```
`ls -l` marks an ACL with a trailing `+` — easy to miss. mag adds a hint line to
`Permission denied` refusals naming the uid that ran the command and whether `sudo -n` was
actually verified (never asserted). If no hint appears, escalation genuinely couldn't be
proven.

## The two lanes — never mix them

Simple-lane and power-lane fields in the same call = **error**.

| Lane | Fields | Shape | Order |
|---|---|---|---|
| **Simple** | `cmd` (one) / `cmds` (array), `cwd` | plain command string(s) | **sequential** |
| **Power** | `raw` / `script`, + `env` `remote` `max_parallel` `continue_on_error` `capture_limit_bytes` `default_timeout_ms` `warm` | see below | `raw`=**parallel**, `script`=as declared |

**Use the simple lane 95% of the time.** `{"cmd": "git status", "cwd": "/path"}`.

## ⚠️ The token-diet ONLY fires on the simple lane

The diet (matched commands → rtk filter: `git status`→`ok — working tree clean`, floods
capped, hint boilerplate dropped) needs to **parse the command**:

- **`cmd` / `cmds`** → mag sees the real command → **diet fires** (header shows `diet`).
- **`raw` / `script`** → runs as `bash -c "<your line>"` → opaque shell script, mag can't
  match it → **only the byte-cap fires, no semantic diet.**

**For known commands (git/cargo/grep/ls/docker/systemctl/logs…), use `cmd`/`cmds`, not
`raw`** — otherwise you leave tokens on the floor. (`never_worse` guard: diet output is
never bigger than raw.)

## ⚠️ `raw` is line-oriented — NO heredocs, NO multiline

Each line of `raw` is a **separate command**, run **in parallel**, `#`=comment:

- **Heredocs do NOT work** — `cat <<EOF … EOF` gets shredded line-by-line, each line runs
  as its own command. This mangles every multi-line write.
- **No multiline commands, no `cd` state carried between lines** (each line is its own
  shell). A compound on ONE line is fine: `cd /path && git status`.

**Need multiline / a file write / persistent cwd?** Options, best first:
1. **Write tool** for creating/editing files.
2. **`script`** (TOON) — multiline-safe, `command: |` opens an indented block.
3. A single-line compound: `printf '%s\n' 'line1' 'line2' > file` (no heredoc).

## Response format (envelope-slim)

One header per result, then raw stdout:

```
[mag cmd-1] exit=0 dur=7ms
<stdout>
```

Flags appear only when true: `diet` (filter ran), `trunc-out`/`trunc-err` (echo cap hit),
`timeout`, `failed`. Multi-job runs prepend `[mag] jobs=3 elapsed=11ms`. `stderr:`/`error:`
blocks show only when non-empty.

## Throttle fat output — `capture_limit_bytes`

Default per-stream echo cap is **8 KiB** → `[truncated by mag — N more bytes]`. When you
expect a flood (`seq`, big logs), pass a tight `capture_limit_bytes` (e.g. `2000`) so you
get a peek, not the firehose.

## TOON script (power lane, multiline-safe)

```
job build:
  command: |
    python3 script.py
    echo done
  cwd: /root
  timeout_ms: 30000
```

Batch fields go **before** any job (`max_parallel`, `continue_on_error`, `default_cwd`,
`env.NAME`, `remote`). Job fields: `command`/`shell`, `argv` (shellwords direct-exec),
`cwd`, `stdin`, `timeout_ms`, `id`, `env.NAME`.

## Persistent sessions

`session_id` pins a warm shell to you — env, cwd, functions persist across calls. It is an
**envelope field** (top-level MCP param, sibling of `script`), **never a line inside the
script**. Sessions are namespaced per seat.

```json
{ "script": "job w:\n  command: |\n    export STATE=1\n    echo $STATE", "session_id": "work" }
```

A later call with the same `session_id` recalls `$STATE`. `close: true` + `session_id`
releases the shell. Idle sessions are reaped automatically.

## `terminal` branch — persistent, *watchable* shell (a real tmux session)

`session_id` above pins a warm shell for you, but it's invisible. The `terminal` action is
a real tmux session, real pane, running live where someone can attach and see it:

```json
{"terminal":{"action":"status"}}
{"terminal":{"action":"send","input":"echo hello"}}
{"terminal":{"action":"snapshot","lines":20}}
```

**Per-seat rooms:** add `"seat":"<yourname>"` and every action targets *your own* isolated
tmux session (`mag-<seat>`) instead of the shared global `mag-house`:

```json
{"terminal":{"seat":"monica","action":"open","cwd":"/some/dir","no_wezterm":true}}
{"terminal":{"seat":"monica","action":"send","input":"pwd"}}
{"terminal":{"seat":"monica","action":"snapshot","lines":20}}
```

`cwd` on `open` doesn't reliably take — send an explicit `cd <path>` right after `open`
rather than trusting the `cwd` param.

**Sequential, not parallel.** A tmux pane is exactly like a human typing: fire two `send`
calls back-to-back and the second one's keystrokes queue at the PTY until the first
command's shell is free. Compare to `raw`/`cmd`/`cmds` (backend pool): genuinely concurrent,
fresh shell per line.

**Pick by what you actually need:**
- Something to *watch happen live*, state that should persist visibly across a session →
  `terminal` (+ `seat` for your own isolated room).
- Fire-and-forget speed, or real parallelism, nobody needs to watch it → `cmd`/`cmds`/
  `raw`/`script`.

`snapshot` strips ANSI — colored output reads as plain text even though it renders in
color live in the pane. Attach to the pane to verify color/decoration.

## Two processes — know which one you're checking

```
mag --lane forward   per-seat, spawned by YOUR client, inherits YOUR uid.
      │ HTTP over a UNIX SOCKET — /run/mag/<seat>.sock
mag@root  /  mag@juxtapo    two resident instances, split by uid. Run the command.
```

Identity comes from the socket, not a header — unforgeable.

- **Live check:** `systemctl is-active mag@root mag@juxtapo` plus `ls /run/mag/`. (`:7734`
  is dead; don't `curl` it.)
- **Restart the right unit:** `mag@root` / `mag@juxtapo`, not `mag`. A restart only touches
  the daemon — your forwarder keeps its old binary until the MCP connection re-establishes.
  A render-layer change appears instantly in `mag run-toon` (fresh process) and silently
  not at all in your live lane.
- **Never infer the runner's identity from your own process, or from `mag.service`.** Both
  can lie. `{"cmd": "id"}` is the answer, measured at the far end.
- Check for staleness: `ps -eo pid,lstart,args | grep 'mag --lane forward'`. Anything older
  than the last install is running old code.

## Quick recipes

- One known command, diet on: `{"cmd": "git status", "cwd": "/repo"}`
- A few sequential: `{"cmds": ["cargo fmt --check", "cargo clippy", "cargo test"], "cwd": "/repo"}`
- Fat output, throttled: `{"cmd": "journalctl -u foo -n 500", "capture_limit_bytes": 2000}`
- Parallel independent probes: `{"raw": "curl -s a\ncurl -s b\ncurl -s c"}` (no diet — caps only)
- Multiline / build with state: use `script` (TOON) or the Write tool.

### ⚠️ MCP tool vs `mag run-batch --json` — different envelopes

The recipes above are for the **MCP tool** (`mcp__mag__mag`), which uses `cmd` (one) /
`cmds` (array). The **CLI subcommand** `mag run-batch --json` is a *different surface* with
a *different envelope* — same shape the TOON engine uses internally:

```bash
mag run-batch --json '{"commands":[{"command":"echo hi"}]}'
# each job: {"command":"..."} or {"argv":["..."]}; see mag-how-to.md for full fields
```

`cmd`/`cmds` will **not** work here — `--json` wants `{"commands":[...]}`, and a job
without `command` or `argv` errors `command needs either 'command' or 'argv'`. The CLI
one-shots (`run-batch`, `run-toon`) are smoke tests that run a private in-process engine,
not the resident lane — reach for the MCP tool for normal work.

*Manual of record: `/etc/mag/mag-how-to.md`. If mag 400s with "requires script or raw", you
sent an unrecognized field shape — check you're not mixing lanes.*
