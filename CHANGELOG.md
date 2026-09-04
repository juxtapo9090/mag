# Changelog

## 2026-08-08

### Changed

- `run-batch` CLI now prints the **humane** `[mag] jobs=N ...` rendering by
  default (via `render_toon_response`, the same renderer `run-toon` uses).
  Previously it always printed the full pretty JSON — expensive for agents to
  read.
- The humane rendering is **non-lossy**: the per-job header folds in
  `trunc-out` / `trunc-err` / `diet` / `timeout` / `failed`, and truncation
  markers still appear at the tail. Nothing hidden.

### Added

- New flag `run-batch --json-out` — keeps the old pretty-JSON output for
  scripts that parse it. (Named `--json-out` to avoid clashing with the
  existing `--json` **input** spec flag, which carries the batch request, not
  the output format.)

### Deployed

- Built release binary.
- Atomic-replaced `/usr/local/bin/mag` (backup
  `/usr/local/bin/mag.bak-pre-humane-runbatch-20260808`).
- Restarted **both** per-seat daemon instances: `mag@juxtapo.service`
  (kim/luc/zet) + `mag@root.service` (celeste/fable/juxtap0/monica/umbra).
  (Note: the single `mag.service` is dead; the live topology is per-seat
  socket units, both `ExecStart=/usr/local/bin/mag --serve`.)
- Verified live on the deployed binary from two seats (kim + celeste,
  independently).

### Validation

- `cargo build --release` clean (4 pre-existing `unsafe` warnings in
  serve.rs, not from this change).
- `run-batch` default: `[mag] jobs=2` + per-job headers; error case
  (`ls /nonexistent`) shows `exit=2` + `stderr:` block + `diet` flag.
- `run-batch --json-out`: old pretty JSON intact (`diet_applied: true` on the
  error job).
- Truncation: `seq 1 200000` with a 2 KB cap → header carries `trunc-out` +
  tail marker `[truncated by mag — N more bytes]`.
- celeste independently confirmed from her seat: humane default +
  `--json-out` intact, `exec_user=root` confirms the seat-socket move held
  through the deploy.

### Notes

- **Gotcha (next `/usr/local/bin` swap):** `sudo cp` of the new binary landed
  it at mode **750** — the old one was **755**, seats couldn't exec. Restored
  `chmod 755` before the daemon restart. Watch the mode on any future
  root-owned binary replace from a user-built artifact.
- Task ticket: `/root/Opus/Task/Nokia/celeste.mag-humane-runbatch.md`
  (from celeste, passed from zet).

## 2026-07-28

### Added

- Added per-call ground-zero raw mode for hard-debug output.
- New top-level request field: `unfiltered: true`.
- Accepted aliases: `g0`, `g_0`, `g-0`, `ground_zero`.
- The flag disables token-diet filtering and the diet echo cap for that request.
- `capture_limit_bytes` still applies as the safety ceiling.

### Usage

Simple lane:

```json
{
  "cmd": "your noisy command",
  "g0": true
}
```

Power/raw lane:

```json
{
  "raw": "shell: your noisy command",
  "g0": true,
  "capture_limit_bytes": 200000
}
```

TOON batch field:

```text
unfiltered: true
capture_limit_bytes: 200000

job crash:
  command: |
    your noisy command
```

### Deployed

- Built release binary.
- Installed to `/usr/local/bin/mag`.
- Restarted `mag.service`.
- Updated live manual at `/etc/mag/mag-how-to.md`.

### Validation

- `cargo check`
- `cargo test` passed: 34 tests.
- Live daemon `/toon` smoke:
  - default path capped a 10 KB output at about 8.5 KB.
  - `g0:true` returned about 10.3 KB.
- CLI `run-toon --g0` smoke:
  - default path returned about 8.2 KB.
  - `--g0` returned about 10 KB.

### Notes

- `cargo clippy -- -D warnings` still fails on pre-existing crate-wide pedantic findings.
- Durable vault note:
  `/mnt/playground/Server_files/Debug-Logs/2026-07-28_mag-ground-zero-unfiltered.md`
