# cw-clear (prototype)

A Claude Code mod that does what claude-watch's tmux-driven "self-clear then
resume prompt" does, from inside the session. Spike only: it is not installed,
enabled or deployed anywhere, and with `enabled` false (the default) it
registers nothing.

What it provides when `enabled` is true:

- `/cw-clear`: reads the resume prompt (from `resumeFile`, else the stdout of
  `resumeCommand`, default `session-task show`), runs the real built-in
  `/clear` through `$.command.run`, then queues the resume prompt with
  `$.prompt.submit` so it is the first turn of the fresh conversation. With no
  resume prompt it refuses to clear.
- A heartbeat file (`stateFile`, JSON: `ts`, `event`, `busy`, `turns`)
  rewritten on `session.start`, `session.end`, `turn.start` and `turn.complete`.
- With `probe` true (tests only): a 2 s `$.clock.every` tick written to
  `<stateFile>.log`, and a Bash `tool.check` hook that logs the engine's verdict
  and allows one specific `rm` the engine would ask about.

Options are set under `pluginConfigs["cw-clear"].options` in settings. Load it
with `claude --plugin-dir mods/cw-clear`. End-to-end test:
`make test-cw-clear-mod-e2e` (needs Docker, credentials and a Claude Code
binary of version 2.1.331 or later; it skips otherwise, so CI skips it).

## Slice 1: use from claude-watch (default OFF)

`self-clear` (the single choke point for daemon-, skill- and operator-driven
context clears) prefers this mod only when ALL hold: `[cw_clear_mod] enabled =
true` in claude-watch's config.toml, the heartbeat file is no older than
`heartbeat_max_age_secs` (default 60; the mod ticks every `heartbeatSecs`,
default 15), the heartbeat says `ready: true` (this mod instance registered
`/cw-clear`) and its last event is not `session.end`. It then writes the resume
prompt to the mod's `resumeFile`, types `/cw-clear`, and waits for the token
drop plus a new turn in the heartbeat. Otherwise, or if the clear is not
confirmed (command unavailable, mod refused), the tmux `/clear` + resume path
runs unchanged. If the mod cleared but no resume turn appears, only the resume
prompt is injected over tmux. Every run logs `PATH-USED: mod|tmux|...` with the
reason in the self-clear log.

Enable (nothing happens until you do both, and merging changes nothing):

1. `mods/cw-clear/cw-clear-setup` prints the settings.json snippet, the
   `--plugin-dir` flag and the config.toml lines;
   `cw-clear-setup --apply --settings FILE [--dry-run]` merges the options.
   To keep the mod loaded across every relaunch, set
   `[claude] plugin_dirs = ["<path to mods/cw-clear>"]` in config.toml
   (claude-watch then appends `--plugin-dir` on each restart/relaunch it
   builds) instead of restarting by hand with `--plugin-dir mods/cw-clear`.
2. Set `[cw_clear_mod] enabled = true` (see config.toml).
3. `self-clear --print` shows the selected path without touching the pane.

Disable: set the toml flag false (instant; `self-clear` reads it per run).

Hot reload: the mod's in-memory state (`turns`, `busy`, `ready`, the log
buffer) resets when it is reloaded. The clear flow itself is stateless (the
resume prompt is read from `resumeFile` on every `/cw-clear`). The heartbeat
carries a per-instance `instance` id and `loadedAt`. `ready` and the heartbeat
timer only start in `session.start`; if a reload does not re-fire it, the
heartbeat goes stale and `self-clear` falls back to tmux. The `turns` counter
restarting at 0 can make a resume look unseen, which only results in a
harmless tmux resume injection.
