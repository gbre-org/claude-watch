# cw-autoresolve

A Claude Code mod that answers Claude Code's Bash safety dialogs ("Dangerous rm
operation ...", "can't be sure it read this command ...") from inside the
session, at the permission check, instead of claude-watch pressing keys in
tmux. On by default. Design and rules: `docs/autoresolve.md`.

## How it decides

A `tool.check` hook on Bash calls `next(e)` (the engine's own verdict). Only
when that verdict is `ask` does it run `claude-watch autoresolve-mod decide`
(JSON `{command, agent_id, warning}` on stdin), which uses the same rules file,
audit log, kill switch and rate limit as the tmux path:

| decider answer | hook result |
| --- | --- |
| `allow` (allow-listed command) | `{ decision: 'allow' }`, no dialog |
| `deny` (everything else) | `{ decision: 'deny', reason }`, the reason is the tool error the model rewrites from |
| `abstain` (off, killed, dry-run, tripped, failure) | the engine's verdict stands: the dialog opens and the tmux path owns it |

An engine `allow` or `deny` is never changed.

## Heartbeat

`session.start`, every `heartbeatSecs` (15) and `session.end` call
`claude-watch autoresolve-mod heartbeat`, which writes the liveness file at the
path claude-watch's config names. The daemon only waits for the mod while that
heartbeat is fresh; stale or missing means tmux-only, as before.

## Loading

claude-watch adds `--plugin-dir` for this mod on every launch it builds
(`[claude] bundled_mods`, default true). By hand:
`claude --plugin-dir mods/cw-autoresolve`. Options (settings.json
`pluginConfigs["cw-autoresolve"].options`): `enabled` (default true),
`claudeWatchBin`, `heartbeatSecs`, `deciderTimeoutSecs`.

Tests: `claude plugin test mods/cw-autoresolve`.
