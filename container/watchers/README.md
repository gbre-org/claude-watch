# container/watchers/

Watcher source files baked into the [claude-container](https://github.com/hndrewaall/claude-watch/tree/main/container) image. Each watcher is a long-running background process the in-container session can launch via the `/start-watchers` skill (defined in [`container/skills/start-watchers.md`](../skills/start-watchers.md)).

> **Authoring a new watcher?** Read [`docs/adding-watchers.md`](../../docs/adding-watchers.md) — covers the fire-and-exit lifecycle contract, the metadata schema below, and a fully-worked Jenkins-build-failure example.

## Current state

One concrete watcher ships today: `claude-event-tail` (see "Currently shipping" below). It uses the generic `lib/dir-watch.sh` primitive — future watchers that just need "fire a callback per new file in a directory" should plug in the same way (don't reimplement inotify / poll / state).

The dir + README + skill exist so:

- The convention is documented (avoids ad-hoc one-offs).
- The Dockerfile wiring (`COPY container/watchers/` and `COPY container/watchers/lib/`) is in place — adding a new watcher requires only dropping files in this dir and rebuilding.
- The session-start surface in `/etc/claude-code/CLAUDE.md` references `/start-watchers` consistently.

## What goes here

Each watcher is a pair of files:

- `<name>.sh` — executable launcher script. MUST run in foreground forever (exit only on terminal failure, never daemonize). Standard output / error go to the log path from the metadata.
- `<name>.toml` — metadata file with the on-disk schema:

  ```toml
  name = "queue-event-tail"
  description = "Tails ~/.claude-events/ for in-container handlers"
  launcher = "/etc/claude-code/watchers/queue-event-tail.sh"
  restart_policy = "on-failure"  # or "always" / "never"
  log_path = "/tmp/claude-container-watchers/queue-event-tail.log"
  ```

  All keys are required. `launcher` is canonically the absolute baked path (`/etc/claude-code/watchers/<name>.sh`); the `/start-watchers` skill resolves it as-is.

## How they get baked in

The Dockerfile copies this directory into the image at:

- `/etc/claude-code/watchers/` — the path the supervisor + the `/claude-container:start-watchers` skill probe via `ls /etc/claude-code/watchers/*.toml`.

(Watchers do NOT land in `/etc/claude-code/plugin/` — they're not slash commands or agents, just launcher scripts the container-level supervisor runs as long-lived child processes.)

## How they get launched at container start

`entrypoint.sh` spawns `/usr/local/bin/cw-watcher-supervisor` (a Python supervisor baked into the image) before tmux launches. The supervisor:

1. Reads every `*.toml` under `/etc/claude-code/watchers/` (or `$CW_WATCHERS_DIR`).
2. Spawns each watcher's launcher script with stdout/stderr appended to its `log_path`.
3. Re-spawns on exit per `restart_policy` (`always` / `on-failure` / `never`).
4. Backoff: exponential 1s → 2s → 4s ... capped at 30s. Resets after a child runs for >= 60s.
5. Crash-loop protection: more than 5 restarts in a 60s window marks the watcher "stuck"; the supervisor stops respawning it until restart.
6. Forwards SIGTERM/SIGINT to every child on shutdown.

Result: watchers survive the entire container lifetime — they aren't bound to any one Claude Code session.

The in-session `/claude-container:start-watchers` skill is now an **informational probe** that reports which watchers exist + whether the supervisor's launcher processes are alive. It does NOT double-launch watchers when the supervisor is already running.

Operators who want to opt out of supervision (e.g. validation harnesses) set `CLAUDE_CONTAINER_WATCHER_SUPERVISOR=0` in the container env; in that case the skill falls back to its legacy `run_in_background: true` shape (session-scoped watchers).

## How to add a new watcher

1. Drop `container/watchers/<name>.sh` (executable; foreground-running) and `container/watchers/<name>.toml` (metadata) in this dir.
2. Update this README's "Currently shipping" section (below) so the catalogue stays accurate.
3. Rebuild the image (`make compose-build` or `docker compose build claude-container`).
4. `docker compose up -d --force-recreate claude-container` and re-run `/start-watchers` to pick up the new entry.

## Test conventions

- Tests live in [`container/tests/`](../tests/). The baseline [`container/tests/baked-dirs.test`](../tests/baked-dirs.test) asserts this README exists at the baked path; extend it as concrete watchers land (per-watcher: `.sh` is executable, `.toml` parses, metadata fields are present).
- The `/start-watchers` skill itself is exercised by [`container/tests/skill-restart-discovery.test`](../tests/skill-restart-discovery.test), which also covers `start-watchers.md` discoverability through `--plugin-dir`.

## Why "no watchers by default" is the deliberate design

The container is a **code-writing sandbox**, not the host's automation hub. Host-side watchers (Signal DM tail, claude-event tail, torrent watch, podcast watch, etc.) belong on the host, run by the operator's host Claude Code session, with host-side credentials and host-side state. Bringing them into the container would (a) duplicate state, (b) require credential bind-mounts that widen the blast radius, and (c) muddle the container-vs-host boundary the baked CLAUDE.md works hard to keep crisp.

When concrete container-scoped watcher use-cases emerge (e.g. an in-container queue-event tail that surfaces queue items posted by host-side cron jobs into the in-container session, or an MCP-bridge health pinger), they'll land here as proper baked entries.

## Reusable primitives

### `lib/dir-watch.sh`

Generic re-arming directory watcher. Drop a wrapper script that exports three env vars and `exec`s into this primitive — it handles inotify monitor mode, ls-mtime poll fallback, the "already seen" state file, and the never-exit re-arm loop.

```bash
#!/bin/bash
export WATCH_DIR="$HOME/my-events"
export WATCH_PATTERN='*.json'
# $1 = full path, $2 = action ("created" / "modified" / "deleted").
export WATCH_CALLBACK='echo "got $2 for $1"'
export WATCH_EVENTS='all'                # opt into the full lifecycle
exec /etc/claude-code/watchers/lib/dir-watch.sh
```

Required env:

- `WATCH_DIR` — absolute path to the directory to monitor.
- `WATCH_PATTERN` — bash glob (NOT regex), matched against the basename (e.g. `*.json`, `v[0-9]*.md`, `*`).
- `WATCH_CALLBACK` — shell snippet invoked once per matching event. Positional args inside the callback:
  - `$1` — the file's full path.
  - `$2` — the action label (one of `created` / `modified` / `deleted`).

  Callbacks that only consume `$1` continue to work unchanged — the `$2` slot is additive.

Optional env:

- `WATCH_EVENTS` — space-separated list of lifecycle events to fire on. Default: `created` (matches the pre-events behaviour — only new files fire). Allowed values:
  - `created` — new file appeared (inotify `CREATE` / `MOVED_TO`; poll: filename not in prior snapshot).
  - `modified` — existing file's content changed (inotify `CLOSE_WRITE`; poll: same filename + newer mtime). Only fires for files that previously fired `created`.
  - `deleted` — file removed or renamed out of the dir (inotify `DELETE` / `MOVED_FROM`; poll: filename gone from glob).
  - `moved-in` — alias for `created` (rename into the watched dir).
  - `moved-out` — alias for `deleted` (rename out of the watched dir).
  - `all` — shorthand for `created modified deleted`.
- `POLL_INTERVAL_SECS` — fallback poll interval when `inotifywait` is missing (default 3s).
- `WATCH_STATE_FILE` — override the state-file path (default `/tmp/dir-watch-<sha1 of WATCH_DIR>.state`). Lines are tab-separated `<filename>\t<mtime>\t<inode>`. The poll fallback diffs against this snapshot to detect created / modified / deleted. The legacy "filenames only" format is auto-migrated on startup with no spurious fires.
- `WATCH_DISABLE_INOTIFY` — set to any non-empty value to force the poll fallback even when `inotifywait` is on PATH (tests only).

The primitive prints `dir-watch: fire <action> <basename>` to stdout per fire, plus whatever the callback itself emits. It runs foreground forever — the supervisor (`/start-watchers`) keeps the process alive per the watcher's `restart_policy`.

## Currently shipping

### `claude-event-tail`

Tails `~/claude-events/*.json` and surfaces each event to the in-container session via stdout. One-liner shape: `EVENT[<source>/<tag>] <first-60-chars-of-message…>` (mirrors the host's `claude-event-watch`). Compact JSON for each event is also appended to `~/.config/claude-events/consumed.jsonl` for later inspection.

- Launcher: `/etc/claude-code/watchers/claude-event-tail.sh`
- Metadata: `/etc/claude-code/watchers/claude-event-tail.toml`
- Restart policy: `always`
- Log path: `/tmp/claude-container-watchers/claude-event-tail.log`

Implementation: thin wrapper that exports `WATCH_DIR=~/claude-events WATCH_PATTERN='*.json' WATCH_CALLBACK=<read-json-print-oneliner-delete>` and execs `lib/dir-watch.sh`.
