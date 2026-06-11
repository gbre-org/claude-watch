# container/watchers/

Watcher source files baked into the [claude-container](https://github.com/hndrewaall/claude-watch/tree/main/container) image. Each watcher is a background task the in-container session launches via the `/start-watchers` skill (defined in [`container/skills/start-watchers.md`](../skills/start-watchers.md)).

> **Authoring a new watcher?** Read [`docs/adding-watchers.md`](../../docs/adding-watchers.md) — covers the block-print-exit lifecycle contract, the metadata schema below, and a fully-worked example.

## Architecture: block-print-exit

Watchers are **session-scoped `run_in_background` Bash tasks**. They are NOT long-lived daemons managed by a supervisor. The lifecycle:

1. The session starts the watcher via `Bash(command="claude-event-watch", run_in_background=true)`.
2. The watcher **blocks** (typically on `inotifywait`) until its trigger condition fires.
3. The watcher **prints** its output (one-liner per event) to stdout.
4. The watcher **exits** (prints a restart banner, then terminates).
5. Claude Code delivers the stdout back to the session as a task-completion notification.
6. The session **immediately restarts** the watcher before processing the events.

This "block-print-exit" contract is fundamental. A watcher that runs forever in a loop cannot deliver results to the session — Claude Code only surfaces `run_in_background` output when the task completes (exits). The reference implementation is `tools/watchers/claude-event-watch`.

## Session lifecycle

- **Watchers must be started on every session start** (including `/clear`, resume, context compaction). They do not survive across sessions.
- **The `/claude-container:start-watchers` skill starts all watchers.** It is step 7 of the session-start checklist.
- **On watcher exit-with-output**: restart immediately, then process the events.
- **On resume after compaction**: all prior background tasks are lost. Re-run `/claude-container:start-watchers`.

## What goes here

Each watcher is a pair of files:

- `<name>.sh` — executable launcher script. MUST follow the block-print-exit contract (block until trigger, print results, exit). Do NOT loop forever.
- `<name>.toml` — metadata file:

  ```toml
  name = "claude-event-watch"
  description = "Blocks until a claude-event arrives, prints pending events, exits"
  launcher = "/opt/claude-container/watchers/claude-event-watch.sh"
  restart_policy = "session"  # restarted by the session on each exit
  log_path = "/var/log/claude-watch/watchers/claude-event-watch.log"
  ```

  All keys are required. `launcher` is the absolute baked path (`/opt/claude-container/watchers/<name>.sh`); the `/start-watchers` skill resolves it as-is.

## How they get baked in

The Dockerfile copies this directory into the image at:

- `/opt/claude-container/watchers/` — the path the `/claude-container:start-watchers` skill probes via `ls /opt/claude-container/watchers/*.toml`.

## How they get launched

The session runs `/claude-container:start-watchers` at session start (step 7 of the checklist). The skill:

1. Lists `*.toml` files under `/opt/claude-container/watchers/`.
2. For each watcher, launches the `launcher` script via `Bash(command="...", run_in_background=true)`.
3. Reports which watchers were started.

On watcher exit (task completion notification from Claude Code), the session must immediately re-run the watcher. The skill handles killing stale instances before starting fresh ones.

## How to add a new watcher

1. Drop `container/watchers/<name>.sh` (executable; block-print-exit) and `container/watchers/<name>.toml` (metadata) in this dir.
2. Update this README's "Currently shipping" section (below) so the catalogue stays accurate.
3. Rebuild the image (`make compose-build` or `docker compose build claude-container`).
4. `docker compose up -d --force-recreate claude-container` and re-run `/start-watchers` to pick up the new entry.

## The block-print-exit contract

Every watcher MUST:

1. **Block** — wait for its trigger (inotifywait, sleep, network listen, etc.). Do not busy-poll.
2. **Print** — emit results to stdout in a compact format the session can parse in one glance.
3. **Exit** — terminate cleanly (exit 0). Print a restart banner so the session knows to re-invoke.

A watcher that loops forever **breaks the delivery model**. Claude Code only surfaces `run_in_background` output on task exit. A forever-loop watcher accumulates output that never reaches the session.

## Test conventions

- Tests live in [`container/tests/`](../tests/). The baseline [`container/tests/baked-dirs.test`](../tests/baked-dirs.test) asserts this README exists at the baked path; extend it as concrete watchers land (per-watcher: `.sh` is executable, `.toml` parses, metadata fields are present).
- The `/start-watchers` skill itself is exercised by [`container/tests/skill-restart-discovery.test`](../tests/skill-restart-discovery.test).

## Why watchers are session-scoped (not supervised)

The container is a **code-writing sandbox**, not the host's automation hub. Background tasks in Claude Code are inherently session-scoped — they exist in the context of a running conversation. A watcher that outlives its session has no one to deliver results to. The block-print-exit model keeps watchers tightly coupled to the session that consumes their output.

## Currently shipping

### `claude-event-watch`

The canonical event-bus watcher. Blocks on `inotifywait` until a `.json` event file appears in `~/claude-events/` (or `$CLAUDE_EVENT_QUEUE`), debounces (default 30s), prints all pending events as one-liners (`EVENT[<source>/<tag>] <message>`), deletes processed files, and exits.

- Reference implementation: `tools/watchers/claude-event-watch`
- Baked launcher: `/opt/claude-container/watchers/claude-event-watch.sh`
- Metadata: `/opt/claude-container/watchers/claude-event-watch.toml`
- Restart policy: `session` (restarted by the session on each exit)
- Log path: `/var/log/claude-watch/watchers/claude-event-watch.log`
