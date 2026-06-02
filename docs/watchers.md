# Background tasks + watcher hygiene

Watchers are the canonical way to surface external state changes to a Claude
Code main loop. A **watcher** here is the precise thing: a *one-shot tool the
main loop invokes* (`watcher-ctl run <name>`) that blocks until its event
fires, prints it to stdout, and **exits** — the supervisor respawns a fresh
instance for the next burst (see [`adding-watchers.md`](adding-watchers.md) for
the lifecycle contract). It is *not* a long-lived poll loop, and it is distinct
from an **event producer** (a cron job, alertmanager, the queue) that merely
*emits* a claude-event onto the bus for the `claude-event-watch` watcher to
surface — producers are not watchers. Watchers MUST be spawned, supervised, and
restarted following the rules below — drift silently turns them into orphans
that fire into the void.

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

## Watcher vs. producer (cron) — pick the right tool

This is a choice between the two roles in the terminology above: a **watcher**
(a supervised, main-loop-owned tool that blocks-prints-exits and gets respawned
each burst) versus an **event producer** (most often a cron job — a single-shot
script that emits a claude-event and exits, surfaced by the *one*
`claude-event-watch` watcher).

**Default to a cron producer.** Each watcher you stand up — even though every
single invocation is short-lived — costs a *supervised slot*: supervisor
overhead, restart cycles on every resume / `/clear` / compaction, DOWN-state
alerts, and mental load to track. A cron producer has none of that persistent
footprint; it just emits onto the bus that the existing watcher already
surfaces. Prefer the producer unless the criteria below genuinely require a
dedicated watcher.

### When cron is the right choice

- **Reactivity requirement is loose.** Cron's minimum resolution is one
  minute. For most health checks, promotion scans, index ticks, and
  periodic event emitters, one-minute granularity is more than sufficient.
- **Script is stateless or diffs against a tiny state file.** A script that
  runs, compares current state to a saved cursor, emits events for any delta,
  and exits cleanly is easy to reason about and safe to restart at any time.
- **Failure alerting is built in.** Wrap cron jobs with `event-cron-wrapper`
  to automatically emit a `cron-failure` claude-event on non-zero exit. No
  extra supervision logic needed.
- **Representative examples**: `cron-promote-candidates`, `tv-check`,
  `index-tick`, `cron-security-check-daily`, `cron-queue-check` — all
  periodic, stateless, fine at one-minute or coarser resolution.

### When you need a watcher instead

A dedicated watcher is justified when BOTH of these are true:

1. **Sub-minute reactivity is required** — you need to react within seconds
   of an external state change, AND
2. **No kernel event mechanism fits** — inotify, systemd path units, and
   similar facilities are not applicable for the event source.

If the event source exposes a kernel mechanism (filesystem changes, socket
events, etc.) prefer that over polling at any granularity.

### Alternatives to a new watcher process

Even when sub-minute reactivity is genuinely needed, reach for these before
spawning a new supervised watcher:

- **Kernel event facilities** (`inotifywait`, `fswatch`, systemd path units,
  eBPF) — react to filesystem or socket events with zero polling. The
  canonical `claude-event-watch` watcher is built on `inotifywait` for
  exactly this reason.
- **Extend an existing daemon** — `claude-watch` itself emits claude-events
  for queue state changes, watcher-down alerts, stale-ready detections, and
  more. If the event you need fits inside claude-watch's monitor loop,
  extend it rather than spawning a peer process.
- **Cron + internal poll loop** — a cron job that runs at the top of every
  minute and internally sleeps-and-polls for up to 59 seconds achieves
  sub-minute resolution without a new supervised process. Appropriate for
  cases where a few extra seconds of latency are tolerable and the event
  source has no kernel mechanism.

### Watchers are a tax, not a feature

Each watcher you add:

- Consumes a Claude Code background-task handle slot.
- Generates restart noise on every resume, `/clear`, and compaction.
- Triggers DOWN-state alerts when it crashes unexpectedly.
- Requires mental load to track across sessions.

Start with cron + state-diff. Convert to a watcher only when you have
empirical evidence that cron's one-minute resolution is insufficient and
none of the above alternatives apply.

**Concrete example:** `subtorrent-watch` was originally a long-running
watcher polling Transmission RPC every few seconds. It was replaced with a
`*/5` cron job: same event coverage, zero supervisor overhead, no DOWN-state
alerts, restarts trivially on resume.

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
