Start the in-container background watchers documented under `/opt/claude-container/watchers/`. Watchers are **session-scoped `run_in_background` tasks** following the block-print-exit contract — this skill is the canonical launcher and is step 7 of the session-start checklist. (A container-level `cw-watcher-supervisor` used to own this lifecycle; it was removed — the session owns watchers now.)

## Current state — watchers are session-scoped

The container ships **one** fire-and-exit watcher today: `claude-event-watch`, which blocks until a JSON event arrives in `~/claude-events/`, prints all pending events as one-liners, and exits. The session gets notified on each exit (batch delivery) and must restart it immediately.

**Lifecycle**: each watcher is launched via Claude Code's `Bash` tool with `run_in_background: true`. It blocks until its trigger condition fires, prints its output, and exits; Claude Code delivers the stdout back as a task-completion notification. Watchers die when the session ends — they must be (re)started on every session start, `/clear`, resume, or context compaction.

Host-side watchers (`signal-wait-dm`, host `claude-event-watch`, torrent watchers, podcast watchers, etc.) still live on the operator's host and are not installed here — the container scope is intentionally narrow.

This skill exists so the in-container agent has a single canonical place to:
1. Discover whatever watchers DO ship in a given image build.
2. Launch each watcher (or verify its launcher process is already alive).
3. Report honestly when the watcher dir is empty.

## Steps

1. **Probe the baked watcher catalogue**:

   ```sh
   ls -1 /opt/claude-container/watchers/*.toml 2>/dev/null
   ```

   Each `.toml` is one watcher's metadata file (name, description, restart-policy, log-path); each is paired with a launcher script at `/opt/claude-container/watchers/<name>.sh` (or whatever the metadata's `launcher` key references). See `/opt/claude-container/watchers/README.md` for the on-disk schema.

2. **If the listing is empty**: report back to the operator:
   > "No watchers are baked into this container image. The convention dir at `/opt/claude-container/watchers/` exists for future watcher integrations; no concrete watchers ship today."

3. **If the listing has entries**: for each `<name>.toml`, check whether the watcher is already running via its **pidfile liveness** (NOT `pgrep`):

   ```sh
   watcher-ctl status
   ```

   `watcher-ctl status` reads the PID the watcher records in `<name>.lock` / `<name>.pid` under the runtime dir and probes it for liveness — it does NOT `pgrep` the launcher path. This matters: the launcher `<name>.sh` does `exec /usr/local/bin/<name>`, which replaces the process argv with the exec'd binary's, so the `.sh` path is GONE from the live cmdline and a `pgrep -f '/opt/claude-container/watchers/<name>.sh'` could NEVER match a healthy watcher (it would always report DOWN). Use `watcher-ctl status` (or read the pidfile) — never pgrep the launcher path.

   - **Already running** (e.g. an earlier launch in this same session — `watcher-ctl status` shows `ok`): do NOT double-launch.
   - **Not running** (`watcher-ctl status` shows `DOWN`): launch via `watcher-ctl run <name>`, invoked **only through Claude Code's `run_in_background: true` Bash invocation** — never via shell `&` or `nohup` (matches the host's cardinal watcher rule: watchers can ONLY be started by Claude Code's main loop). `watcher-ctl run` writes the `<name>.pid` / `<name>.runlock` files (so the daemon's pidfile-based liveness check can see the watcher) and is idempotent (it no-ops if a live instance already holds the slot). Capture the resulting `bash_id` so the watcher can be monitored / killed later.

   > Do NOT launch the raw `bash /opt/claude-container/watchers/<name>.sh` directly: a raw launch skips the pidfile/runlock bookkeeping `watcher_run` performs, so the daemon's watcher_monitor (pidfile-based since PR #339) can't see it and will report a false DOWN.

4. **Report** which watchers were started (or were already alive), their `restart_policy`, and the path of each watcher's log (`/var/log/claude-watch/watchers/<name>.log` by convention).

## On watcher exit

When a watcher's background task completes (Claude Code surfaces its stdout), **restart the watcher immediately, then process the events**. A watcher that has exited is deaf — events that arrive while it's down are only caught on the next launch's catch-up scan (if the watcher implements one).

The claude-watch daemon's `[watcher_monitor]` is the fallback alert layer: if a watcher's recorded PID (its `<name>.lock` / `<name>.pid` file) is missing or names a dead/recycled process for several consecutive checks, the daemon fires a `[CLAUDE-WATCH] WATCHER(S) DOWN` alert into the session. (Liveness is pidfile-based, NOT pgrep — the launcher `exec`s the bare binary, so the `.sh` argv that pgrep would match is gone; see PR #339.)

## Adding a new watcher

To add a container-baked watcher in a future PR:

1. Drop `<name>.sh` (executable launcher; MUST follow block-print-exit: block until trigger, print results, exit — do NOT loop forever) and `<name>.toml` (metadata) under [`container/watchers/`](https://github.com/hndrewaall/claude-watch/tree/main/container/watchers) in the claude-watch repo. Authoring guide: `docs/adding-watchers.md`.
2. The `Dockerfile` `COPY` line for `container/watchers/` already lands them at `/opt/claude-container/watchers/<name>.{sh,toml}` (this skill auto-discovers them).
3. Update `container/watchers/README.md` to document what the watcher does and what events it produces, and register it in `container/watchers.conf` so the daemon's watcher_monitor can alert when it's down.
4. Rebuild the image and `cwsr` (or `docker compose up -d --force-recreate` if entrypoint-time wiring changed).

## Important

- This skill never starts host-side watchers and never schedules host cron jobs. For host-side scheduled work, see the "Host-side scheduled tasks (via `host-bash`)" section of `/etc/claude-code/CLAUDE.md`.
- The `container/watchers/README.md` documents the on-disk schema (`name`, `description`, `launcher`, `restart_policy`, `log_path`); change there first, then bump the consumers.
- Source dir in repo: [`container/watchers/`](https://github.com/hndrewaall/claude-watch/tree/main/container/watchers). Baked path inside the container: `/opt/claude-container/watchers/`.
