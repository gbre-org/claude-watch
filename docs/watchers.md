# Background tasks + watcher hygiene

Watchers and other long-lived background tasks are the canonical way to
surface external state changes to a Claude Code main loop. They MUST be
spawned, supervised, and restarted following the rules below — drift
silently turns watchers into orphans that fire into the void.

> **Writing a new watcher?** See [`adding-watchers.md`](adding-watchers.md)
> for the authoring walkthrough — the on-disk file layout, the
> fire-and-exit lifecycle contract, the `watchers.conf` schema (host)
> and `<name>.toml` schema (container), and a fully worked Jenkins-
> build-failure example for either surface. This file covers the
> operator-side hygiene rules; the authoring doc covers how to write
> the watcher itself.

## Cardinal rule: watchers belong to the main loop

> **Watchers can ONLY ever be started by Claude Code's main loop**, via the
> Bash tool's `run_in_background: true`. Never via systemd-run, never via
> nohup, never by the `claude-watch` daemon, never by a subagent.

The daemon's only emergency action is **tmux-injecting** a
`watcher-ctl run <name>` line into the main loop's pane, so the main
loop re-spawns the watcher itself. If anything else spawns the watcher,
the main loop has no handle on it and the watcher's stdout disappears.

## 30-second rule (variable-latency ops)

Any Bash command that **might** take >30s MUST use `run_in_background:
true`. No exceptions — SSH, `gsutil` / `aws s3` uploads, long ffmpeg,
big rsyncs, etc. Blocking the foreground prevents message processing.

(Per-deployment policy may layer a stricter foreground ceiling on top —
e.g. a 15-second cap. The 30-second rule is the floor.)

## Never use `&` in background commands

`run_in_background: true` already handles backgrounding. Adding `&`
double-forks: the shell exits → Claude Code loses the task handle →
`watcher-status` sees the process but Claude Code thinks the task
completed. The watcher runs as an orphan that can never deliver results.

After starting watchers, verify with a non-blocking `TaskOutput` peek
that tasks show `status: running`. If they show `completed`, the handle
was lost — kill and restart without `&`.

## Minimize background tasks — chain instead

Chain related work with `&&` rather than spawning separate background
tasks. Each background task costs a handle slot, and three sequential
tasks running on the same data can usually be expressed as one chain.

Within a single watcher domain (e.g. event surfacing) keep ONE watcher
running, not multiple. Duplicates race for inotify events and silently
drop deliveries.

## Watcher restart on resume

- **On every resume** — boot, `/clear`, restart, compaction — **kill and
  restart ALL watchers**. Background tasks survive the resume, but the
  main loop loses its handles, so the watchers become orphans that
  cannot deliver results to this session.
- **Cleanup**: first `TaskStop` every known task id, THEN run
  `watcher-restart` to kill any remaining orphaned processes (reads from
  config, handles all watchers in one shot). Never use bare `pgrep -f` /
  `kill` for watcher cleanup — it misses the right children and clobbers
  the wrong ones.

## Restart watchers BEFORE acting on results

When a watcher returns results, restart it **immediately** as the first
action — before replying, processing, or doing anything else. Otherwise
the watcher is dead during the time it takes you to act on the previous
fire, and any new event in that window is lost.

For event-surfacing watchers (e.g. `claude-event-watch`), the canonical
shape is: receive the watcher's output, fire `watcher-ctl run <name>`
in parallel with the action that consumes the output, and only after
the watcher is back up should you decide what to do with the events.

## Foreground-blocking forbidden

- NEVER use blocking waits in the foreground — no `sleep 60 && ...`, no
  `TaskOutput block:true` with timeouts greater than the per-deployment
  ceiling. These freeze the CLI.
- Let background completion notifications arrive naturally (Claude Code
  auto-notifies on task end).
- Only use `TaskOutput block:true` for tasks you KNOW completed quickly.
  Otherwise `block:false` to peek, or wait for the auto-notify.
- If you need to poll something, do it in a background task — never in
  the foreground main loop.

## Self-clear and resume-prompt injection

`tools/watchers/self-clear` is the canonical helper for "inject `/clear`
plus a resume-prompt into the Claude Code tmux pane". Called as the
final step of a compact-prep procedure; eliminates the wait for the
daemon's resume-injection path to fire on its own. See
[`watchers.md`](../tools/watchers/README.md) for config.

## Tests

```
make test-watchers         # claude-event-watch fast-path + self-clear config
```
