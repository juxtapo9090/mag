# mag — how to run the fast lane

*The resident warm-pool shell runner. This manual is read on demand — it never
crosses the wire per call. Pool stats: `GET /health` on the daemon.*

## The two lanes

**Simple lane** — `cmd` (one command) or `cmds` (array). Sequential, plain-text
output, warm pool. Optional `cwd`. Use this 95% of the time.

**Power lane** — `script` (TOON) or `raw` (line-oriented). Adds: `remote`,
`env`, `default_timeout_ms`, `max_parallel`, `continue_on_error`,
`capture_limit_bytes`, `unfiltered`, `warm`. Simple + power fields together =
error, except `unfiltered` may be used with simple `cmd`/`cmds`.

**Session fields work on BOTH lanes.** `session_id` / `sticky` / `close` are not
power-lane fields — pinning a warm shell is orthogonal to which lane you write
in, so you never have to switch to TOON just to keep continuity. *(Before
2026-08-03 the daemon accepted `session_id` on the simple lane and silently
discarded it: every call looked continuous and got a fresh shell. Reported by
Monica.)*

**Envelope vs script — where fields live.** The MCP tool call is an
*envelope*; the TOON text inside `script` is a *script*. Session fields
(`session_id`, `close`) are **envelope fields** — top-level MCP params,
siblings of `script`, never lines inside it. TOON scripts describe *jobs*;
the envelope describes *which warm shell runs them*. Putting `session_id`
or `close` inside a TOON script is a parser error (the error message will
point here).

## Response format (envelope-slim)

One compact routing marker, then raw stdout. Nothing else.

```
::MAG
<stdout raw>
```

## Shared terminal

`mag term` creates a persistent tmux-backed house terminal. tmux owns the
state; WezTerm is the visible window. The default session is `mag-house`.

```bash
mag term open
mag term open --no-wezterm
mag term status
mag term send 'pwd'
mag term snapshot --lines 80
mag term attach
mag term close
```

The operator pane is persistent: `cd` and exported variables stay until the
session is closed. The watcher pane tails `/var/lib/mag/audit.jsonl` plus Jaga
seat feed logs when they exist.

The MCP surface stays one tool: `mag`. Terminal access is a top-level envelope
branch and cannot be mixed with `cmd` / `cmds` / `raw` / `script` in the same
call.

```json
{"terminal":{"action":"status"}}
{"terminal":{"action":"send","input":"pwd"}}
{"terminal":{"action":"snapshot","lines":80}}
{"terminal":{"action":"open","no_wezterm":true}}
{"terminal":{"action":"close"}}
```

Supported terminal actions: `open`, `status`, `send`, `snapshot`, `close`.
Plain terminal calls target the shared `mag-house` session. For visible
parallel per-seat terminals, pass `seat`; Mag maps it to tmux session
`mag-<seat>` without changing the global default.

```json
{"terminal":{"seat":"monica","action":"open","cwd":"/root/Monica","no_wezterm":false}}
{"terminal":{"seat":"monica","action":"send","input":"pwd"}}
{"terminal":{"seat":"monica","action":"snapshot","lines":80}}
```

Use `session` only when you need an explicit tmux session name.

Header flags appear only when true: `timeout`, `trunc-out`, `trunc-err`,
`diet`, `failed`. Multi-job runs get a batch first line:
`[mag] jobs=3 elapsed=11ms`. `stderr:` / `error:` blocks appear only when
non-empty. Pool stats are on the daemon's `/health`, not in responses.

## Token-diet (rtk floor)

Final resort raw mode: pass top-level `unfiltered: true` to disable the
rtk-style filter and diet echo cap for that request. Aliases: `g0`, `g_0`,
`g-0`, `ground_zero`. The normal `capture_limit_bytes` ceiling still applies.

Matched commands (git status/diff/log, cargo build/check/test, grep/rg/find,
ls, docker, systemctl, logs — `filters.toml` in the crate) get an rtk-style
filter pass before rendering: hint boilerplate dropped, floods capped, clean
runs short-circuit (`ok — working tree clean`). The `never_worse` guard means
output is never bigger than raw; the `diet` header flag shows a filter ran.
Unmatched commands pass through untouched. Per-stream echo cap (default
8 KiB) writes `[truncated by mag — N more bytes]`. Toggle in mag.yaml:
`execution.token_diet_enabled` / `token_diet_echo_max_bytes`.

## TOON script

Named jobs, parallel or sequential, multiline-safe.

```
job shadow1: echo "single line"

job build:
  command: |
    python3 script.py
    echo done
  cwd: /root
  timeout_ms: 30000
```

Batch fields (before any job): `max_parallel`, `continue_on_error`,
`default_cwd`, `default_timeout_ms`, `capture_limit_bytes`, `env.<NAME>`,
`remote`.

Job fields: `command`/`shell` (alias), `argv` (shellwords-split direct exec),
`cwd`, `stdin`, `timeout_ms`, `warm`, `remote`, `id`, `env.<NAME>`.
`key: |` opens an indented multiline block.

## RAW

One command per line, parallel by default. `#` = comment, blanks ignored.
`exec:` → argv mode, `shell:` → command, bare → command. No multiline, no
remote.

## Named sessions

`session_id` (an **envelope** field — see above) pins a warm shell to you:
env, cwd, functions persist across calls. Sessions are namespaced per seat
(`CTX_CALLER`) — `session_id: work` from kim and from cel are different
shells.

The shape — session fields sit *next to* the script, not in it:

```json
{
  "script": "job work:\n  command: |\n    export STATE=1\n    echo $STATE",
  "session_id": "work"
}
```

A second call with the same `session_id: "work"` recalls `$STATE`.

The same thing on the simple lane — no TOON, no boilerplate:

```json
{"cmd": "export STATE=1", "session_id": "work"}
{"cmd": "echo $STATE",    "session_id": "work"}
```

**`sticky: true`** is the no-bookkeeping form: it resolves to a per-seat default
session, so you get continuity without inventing and tracking id strings.
Sessions are already namespaced per seat, so everyone's `sticky` shell is their
own. Use `session_id` when you want *several* independent shells; use `sticky`
when you just want your shell to stay put.

```json
{"cmd": "cd /var/log", "sticky": true}
{"cmd": "pwd",         "sticky": true}
```

- `close: true` + `session_id` (or `sticky: true`) releases the shell
  (restarted fresh). Works on both lanes, and with or without a command:
  `{"sticky": true, "close": true}` on its own is a valid close.
- Sessions idle longer than the daemon's `session.idle_timeout` are reaped
  automatically — the slot returns to the pool.
- Per-command timeouts apply inside sessions too (watchdog in-band).

### Is my session still alive?

```json
{"sessions": true}                      // every session this seat holds
{"sessions": true, "session_id": "work"} // just that one
```

Returns slot, pid, age and idle time per session — no need to run a command and
read back `$$`. Asking about an id you don't hold says so explicitly; that is a
different answer from holding none at all.

```
2 named session(s)
  default slot=0 pid=1972052 age=4m12s idle=8s
  work    slot=2 pid=1972054 age=6m01s idle=2m30s
```

`pid=gone` means the bookkeeping outlived the shell — the honest answer, not a
guess.

### Why there is a ceiling

A named session **pins** a warm shell until it is closed or reaped; unnamed
calls can only run in a slot nobody has pinned. So `session.max_named` is
clamped to `workers - 2`, always leaving room for the plain `cmd`/`cmds` lane.
A config asking for more is clamped with a warning rather than honoured — one
that permits more sessions than there are workers cannot deliver them anyway,
and the pool would starve first, reporting the wrong reason. Hitting the ceiling
gives `named session limit N reached; close a session first`, which tells you
what to do.

## Remote

Per-job `remote: <name|host:port|url>` forwards via node registry (HTTP) or
SSH fallback (`ssh -T host bash -s`). Registry names resolve through
`--nodes <yaml>` or `/etc/mag/nodes.yaml`. SSH fallback: `command` only —
`argv` and `stdin` are rejected. Top-level `remote` + `raw` is rejected by
design.

## Daemon

`mag --serve --config /etc/mag/mag.yaml` — resident mode, loopback or
WireGuard bind, token optional on trusted lanes. Routes: `GET /health`,
`POST /toon`, `POST /toon/async`, `GET /job/{id}`.

CLI one-shots: `mag run-toon --script "job hi: echo hello"`,
`mag run-toon --raw $'echo one\necho two'`,
`mag run-batch --json '{"commands":[{"command":"echo hi"}]}'`.

**`run-batch` envelope ≠ MCP tool envelope.** The MCP tool (`mcp__mag__mag`) uses
`cmd` (one) / `cmds` (array) — the simple lane. `run-batch --json` takes the batch
spec: `{"commands":[{"command":"..."}]}`, each job `command` (shell) or `argv`
(direct-exec). `cmd`/`cmds` will not work here — it errors `missing field commands`.
The one-shots run a private in-process engine (smoke tests), not the resident lane;
reach for the MCP tool for normal work.

## Two processes, and a deploy that lands in one of them

mag is **not** one process. A tool call crosses a uid boundary:

```
mag --lane forward   ← per-seat, spawned by that seat's client, inherits its uid
                       (often root). Deserializes the daemon's JSON and RENDERS.
        │ HTTP
mag --serve          ← the resident daemon, User=juxtapo. Owns the warm pool
                       and actually RUNS the command.
```

So `systemctl restart mag` restarts **only the daemon**. Every live seat keeps
its old forwarder — and therefore its old rendering code — until that seat's
MCP connection is re-established. A change to the render layer will appear
instantly via `mag run-toon` (fresh process each time) and *silently not
appear* in live MCP lanes. That asymmetry has already cost one debugging
session: the fix was correct and looked broken.

**Deploying a render-layer change:**

```bash
sudo cp /usr/local/bin/mag /usr/local/bin/mag.bak-<what>   # undo button
sudo install -m 755 <repo>/target/release/mag /usr/local/bin/mag
sudo systemctl restart mag                                  # daemon only
# then each seat: reconnect its MCP server, or it keeps the old forwarder
```

Verify in the lane you actually care about, not the convenient one. `ps -eo
pid,lstart,args | grep 'mag --lane forward'` shows each forwarder's start
time — anything older than the install is stale. Stale forwarders from dead
sessions accumulate; they are harmless but they are also still running old
code.

## Refusal hints

A failed command whose output looks like a refusal gets one extra line. It
fires only on failure, so a working session pays nothing.

- **`Permission denied` and friends** → names the uid that *ran* the command
  and states that `sudo -n` is passwordless for it. Printed **only** after
  `sudo -n true` has actually been run and returned 0.
- **`dubious ownership`** → git's `safe.directory` guard, not a permission.
  Says so, because sudo does not fix it.
- Silent when sudo itself was what got refused, and silent when escalation
  cannot be proven.

The identity comes from `GET /health` (`exec_user`, `exec_can_escalate`),
published by the daemon and installed into the forwarder once per process.
It is deliberately not probed locally: **the forwarder is usually root while
the runner is `juxtapo`**, so a local probe describes the wrong process. If
the probe fails, no identity is installed and the hint stays silent rather
than guessing.

Two rules this feature must keep, both learned expensively:

1. **Never manufacture evidence.** Don't assert a capability you haven't
   just verified. (Same law as the beacon heartbeat.)
2. **Scope the claim to the syscall.** A refusal describes one operation on
   one path. `ls /root/` failing does not mean `/root/Rogue` is unreadable —
   an ACL grants `--x` traverse without `r` list, so the subtree is open
   while the listing is not.
