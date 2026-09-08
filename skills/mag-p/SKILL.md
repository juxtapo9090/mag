---
name: mag-p
description: Trigger phrase "mag persistence" / "/mag-p" — abang is telling you he's already inside some box (a VPS, a remote host) through his own live terminal, not asking you to open a fresh connection. Check his actual pane via mag's tmux terminal branch before doing anything else. Use whenever abang says "dah masuk", "already in vps/[host]", or invokes mag-p directly.
---

# mag-p — check where abang actually is before acting

Born 2026-08-31 from a real mix-up: abang said "dah masuk dalam vps" (already in the VPS)
meaning Meridian's box. Monica assumed and opened a fresh `ssh` to the wrong IP (`vpseu`,
`.85`) while abang was sitting in a different one (`vps`/celsjux-sg) the whole time — burned
several turns on a phantom host-key security scare before checking the obvious place.

## The rule

When abang says he's already somewhere, **don't open a new connection and don't guess which
box.** Look at his own pane first — it's cheap and it's ground truth.

```
mcp__mag__mag terminal: {"action": "snapshot", "lines": 60, "session": "mag-<seat>"}
```

`<seat>` = the seat name abang specifies (e.g. `mag-celeste`, `mag-monica`). This is the
tmux session abang's been typing into directly. The snapshot shows his prompt, hostname, cwd,
and last commands — read those before assuming anything about where "the VPS" is.

## Awareness (2026-09-08)

The UserPromptSubmit hook now injects **`🖥️ mag terminals: ...`** into every prompt — you
already know which sessions are alive before this skill even fires. No discovery step needed.

## Procedure

1. **You already know which sessions are up** from the hook injection. If the target session
   is listed, go straight to step 2.
2. Snapshot `mag-<seat>` (60 lines) — read the hostname/prompt/cwd.
3. **To run a command and get stdout back**, use `mag-term-exec`:
   ```bash
   mag-term-exec 'command here' --seat <seat> [--timeout SECS]
   ```
   This sends the command to the live tmux pane, waits for completion, and pipes stdout back.
   No timeout limit from the terminal side — polls until done or `--timeout` (default 120s).
   Works with fish, bash, zsh.
4. For fire-and-forget (don't need stdout): use `terminal: {"action": "send", ...}` directly.
5. If nothing useful shows (`status=missing`, empty pane), say so plainly and ask which box
   / session he means — don't fall back to opening your own connection silently.

## mag-term-exec (2026-09-08)

Installed at `/usr/local/bin/mag-term-exec`. The stdout pipe that was missing from the mag
terminal lane — fire a command on abang's live terminal, wait, get the result back.

```bash
mag-term-exec 'uname -a'                     # runs on mag-house
mag-term-exec 'cargo build' --seat celeste    # runs on mag-celeste
mag-term-exec 'make test' --timeout 300       # 5-minute timeout
```

How it works: echo start-marker → command → echo end-marker, all on one line. Polls
`mag term snapshot` until end marker appears, extracts output between markers via awk.
Shell-agnostic (detects fish `$status` vs bash `$?` for exit codes).

## Why this matters

Docs and memory drift (an IP in a `.md` file can be stale, a project can have moved to a
different host since it was last written up). Abang's live pane is never stale — it's
whatever he's actually looking at right now. Trust that over a remembered fact.
