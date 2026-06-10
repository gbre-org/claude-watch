Report on the in-container background watchers documented under `/opt/claude-container/watchers/`. As of the cw-watcher-supervisor PR, watchers are **auto-launched by the entrypoint** and supervised for the container's entire lifetime — this skill is now an informational probe, not a launcher.

## Current state — watchers are container-supervised

The container ships **one** fire-and-exit watcher today: `claude-event-watch`, which blocks until a JSON event arrives in `~/claude-events/`, prints all pending events as one-liners, and exits. The main loop gets notified on each exit (batch delivery) and restarts it immediately.

**Lifecycle change**: previously this skill launched watchers via Claude Code's `Bash` tool with `run_in_background: true`, so watchers died when the session ended and the next session had to re-run the skill. The new shape: `cw-watcher-supervisor` (baked at `/usr/local/bin/cw-watcher-supervisor`, launched by `entrypoint.sh` before tmux) reads each watcher's `.toml` and respawns the launcher on exit per `restart_policy`. Watchers survive the entire container lifetime, not just one session.

Host-side watchers (`signal-wait-dm`, host `claude-event-watch`, torrent watchers, podcast watchers, etc.) still live on the operator's host and are not installed here — the container scope is intentionally narrow.

This skill exists so the in-container agent has a single canonical place to:
1. Discover whatever watchers DO ship in a given image build.
2. Verify the supervisor is up and that each watcher's launcher process is alive.
3. Report honestly when the watcher dir is empty.

## Steps

1. **Probe the baked watcher catalogue**:

   ```sh
   ls -1 /opt/claude-container/watchers/*.toml 2>/dev/null
   ```

   Each `.toml` is one watcher's metadata file (name, description, restart-policy, log-path); each is paired with a launcher script at `/opt/claude-container/watchers/<name>.sh` (or whatever the metadata's `launcher` key references). See `/opt/claude-container/watchers/README.md` for the on-disk schema.

2. **Check whether the container-level supervisor is up**:

   ```sh
   pgrep -af cw-watcher-supervisor
   ls -la /tmp/claude-container-watchers/supervisor.log
   tail -n 20 /tmp/claude-container-watchers/supervisor.log
   ```

   If the supervisor is running (entrypoint default), report `already supervised by cw-watcher-supervisor` and the supervisor PID. Do NOT double-launch watchers via `run_in_background: true` — the supervisor owns them. If the supervisor is not running (operator opted out via `CLAUDE_CONTAINER_WATCHER_SUPERVISOR=0`), fall back to the legacy in-session launch path (see "Legacy fallback" below).

3. **If the listing is empty**: report back to the operator:
   > "No watchers are baked into this container image. The convention dir at `/opt/claude-container/watchers/` exists for future watcher integrations; no concrete watchers ship today."

4. **If the listing has entries AND the supervisor is up**: report which watchers exist, their `restart_policy`, the path of each watcher's log, and whether the launcher process is currently alive. Example check per watcher:

   ```sh
   pgrep -af '/opt/claude-container/watchers/<name>.sh'
   tail -n 10 /tmp/claude-container-watchers/<name>.log
   ```

## Legacy fallback (operator opted out of supervision)

When `CLAUDE_CONTAINER_WATCHER_SUPERVISOR=0` AND the listing has entries: for each `<name>.toml`, read the metadata, then launch the corresponding `<name>.sh` as a backgrounded subprocess **only via Claude Code's `run_in_background: true` Bash invocation** — never via shell `&` or `nohup` (matches the host's cardinal watcher rule: in-session watchers can ONLY be started by Claude Code's main loop). Capture the resulting `bash_id` so the operator can monitor / kill the watcher later. These session-scoped watchers die when the session ends; opting out of supervision is the operator's explicit choice.

## To force-restart a supervised watcher

The supervisor respawns automatically on exit (per `restart_policy`). To roll a watcher manually:

```sh
pkill -f '/opt/claude-container/watchers/<name>.sh'
```

The supervisor's poll loop notices the exit within ~0.5s and respawns it (after the configured backoff if the previous run was short-lived).

To temporarily disable a watcher, edit its `.toml` to set `restart_policy = "never"` and restart the container — the supervisor will spawn it once and not respawn.

## Adding a new watcher

To add a container-baked watcher in a future PR:

1. Drop `<name>.sh` (executable launcher; should run in foreground forever, exit non-zero on failure) and `<name>.toml` (metadata) under [`container/watchers/`](https://github.com/hndrewaall/claude-watch/tree/main/container/watchers) in the claude-watch repo.
2. The `Dockerfile` `COPY` line for `container/watchers/` already lands them at `/opt/claude-container/watchers/<name>.{sh,toml}` (the supervisor auto-discovers them).
3. Update `container/watchers/README.md` to document what the watcher does and what events it produces.
4. Rebuild the image and `cwsr` (or `docker compose up -d --force-recreate` if entrypoint-time wiring changed).

## Important

- This skill never starts host-side watchers and never schedules host cron jobs. For host-side scheduled work, see the "Host-side scheduled tasks (via `host-bash`)" section of `/etc/claude-code/CLAUDE.md`.
- The `container/watchers/README.md` documents the on-disk schema (`name`, `description`, `launcher`, `restart_policy`, `log_path`); change there first, then bump the consumers.
- Source dir in repo: [`container/watchers/`](https://github.com/hndrewaall/claude-watch/tree/main/container/watchers). Baked path inside the container: `/opt/claude-container/watchers/`.
