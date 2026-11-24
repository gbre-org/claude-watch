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
