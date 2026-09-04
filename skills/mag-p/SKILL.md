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

`<seat>` = your own seat name (`mag-monica` for Monica). This is the tmux session abang's
been typing into directly. The snapshot shows his prompt, hostname, cwd, and last commands —
read those before assuming anything about where "the VPS" is.

## Procedure

1. Snapshot `mag-<seat>` (60 lines is usually enough to see the current hostname + last
   command). If abang references a specific tool/session name, use that instead.
2. Read the actual hostname/prompt (`root@<hostname>:~#`) — that tells you which box, not
   whichever IP you assumed from memory or an old doc.
3. Act *in that same pane* via `terminal: {"action": "send", ...}` + `{"action": "snapshot"}`
   to read the result — don't open a parallel `ssh` session unless abang's pane genuinely
   isn't where the work needs to happen.
4. If nothing useful shows (`status=missing`, empty pane), say so plainly and ask which box
   / session he means — don't fall back to opening your own connection silently.

## Why this matters

Docs and memory drift (an IP in a `.md` file can be stale, a project can have moved to a
different host since it was last written up). Abang's live pane is never stale — it's
whatever he's actually looking at right now. Trust that over a remembered fact.
