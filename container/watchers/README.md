# container/watchers/

Watcher source files baked into the [claude-container](https://github.com/hndrewaall/claude-watch/tree/main/container) image. Each watcher is a long-running background process the in-container session can launch via the `/start-watchers` skill (defined in [`container/skills/start-watchers.md`](../skills/start-watchers.md)).

> **Authoring a new watcher?** Read [`docs/adding-watchers.md`](../../docs/adding-watchers.md) — covers the fire-and-exit lifecycle contract, the metadata schema below, and a fully-worked Jenkins-build-failure example.

## Current state

**Empty.** No watchers ship in this container today. This dir is a stub for phase-2 watcher integrations. The `/start-watchers` skill is honest about this and reports "nothing to start" when run in a fresh image.

The dir + README + skill exist now so:

- The convention is documented before the first watcher lands (avoids ad-hoc one-offs).
- The Dockerfile wiring (`COPY container/watchers/`) is in place — adding a new watcher requires only dropping files in this dir and rebuilding.
- The session-start surface in `/etc/claude-code/CLAUDE.md` can reference `/start-watchers` consistently (currently a no-op; same skill name will start working when watchers land).

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

- `/etc/claude-code/watchers/` — the path the `/start-watchers` skill probes via `ls /etc/claude-code/watchers/*.toml`.

(Watchers do NOT land in `/etc/claude-code/plugin/` — they're not slash commands or agents, just shell scripts the agent runs via the `Bash` tool with `run_in_background: true`.)

## How a fresh container session discovers them

The agent runs `/start-watchers` (the baked skill) which:

1. `ls /etc/claude-code/watchers/*.toml` — enumerate metadata files.
2. For each entry: parse the metadata, then launch the `launcher` script via `Bash` with `run_in_background: true`. The skill captures every `bash_id` so the operator can monitor / kill them later.
3. Reports per-watcher status (started OK, missing launcher, log path).

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

## Currently shipping

(none)
