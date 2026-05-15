Start the in-container background watchers documented under `/etc/claude-code/watchers/`. This is the container equivalent of the host's watcher-startup workflow.

## Current state — one baked watcher (`claude-event-tail`)

The container ships **one** long-running watcher today: `claude-event-tail`, which surfaces JSON event drops in `~/claude-events/` to the in-container session via stdout. It uses the generic re-arming `lib/dir-watch.sh` primitive — future watchers that just need "fire callback per new file" should plug in via the same primitive rather than reimplementing inotify / poll / state.

Host-side watchers (`signal-wait-dm`, host `claude-event-watch`, torrent watchers, podcast watchers, etc.) still live on the operator's host and are not installed here — the container scope is intentionally narrow.

This skill exists so the in-container agent has a single canonical place to:
1. Discover whatever watchers DO ship in a given image build (the set may grow as concrete container-scoped use-cases emerge).
2. Wire them up at session start with consistent semantics (logging path, restart policy, ownership).
3. Report honestly when the watcher dir is otherwise empty (instead of inventing a host-style answer).

## Steps

1. **Probe the baked watcher catalogue**:

   ```sh
   ls -1 /etc/claude-code/watchers/*.toml 2>/dev/null
   ```

   Each `.toml` is one watcher's metadata file (name, description, restart-policy, log-path); each is paired with a launcher script at `/etc/claude-code/watchers/<name>.sh` (or whatever the metadata's `launcher` key references). See `/etc/claude-code/watchers/README.md` for the on-disk schema.

2. **If the listing is empty**: report back to the operator:
   > "No watchers are baked into this container image. The convention dir at `/etc/claude-code/watchers/` exists for future watcher integrations; no concrete watchers ship today. If you need a host-side watcher's behaviour from this session, run it on the host and bridge events into the container via the documented `claude-event` JSON path or `host-bash` MCP."

   Do NOT invent a watcher to start. Do NOT try to launch host-side watcher scripts via `host-bash` unless the operator explicitly asks (those processes belong on the host, owned by the host's session manager, not by the container's main loop).

3. **If the listing has entries**: for each `<name>.toml`, read the metadata, then launch the corresponding `<name>.sh` as a backgrounded subprocess **only via Claude Code's `run_in_background: true` Bash invocation** — never via shell `&` or `nohup` (matches the host's cardinal watcher rule: watchers can ONLY be started by Claude Code's main loop, never via systemd-run, never via nohup). Capture the resulting `bash_id` so the operator can monitor / kill the watcher later.

4. **Report**: list every watcher you started, its `bash_id`, and the log path from its `.toml`. If any watcher's launcher script is missing or non-executable, report it and skip — do not silently fail.

## Why this skill is intentionally thin today

The container's session-start checklist (in `/etc/claude-code/CLAUDE.md`) explicitly states: "There are no long-running watchers inside this container. This is deliberate — the container is a code-writing sandbox, not a host automation hub." This skill enforces that contract: when the watcher dir is empty, the honest answer is "nothing to start", not a hand-waved "checking…" The skill is wired in now so phase-2 PRs that add container-scoped watchers (e.g. an in-container queue-event tail, an MCP-bridge health pinger) have a discovered, conventional place to land.

## Adding a new watcher

To add a container-baked watcher in a future PR:

1. Drop `<name>.sh` (executable launcher; should run in foreground forever, exit non-zero on failure) and `<name>.toml` (metadata) under [`container/watchers/`](https://github.com/hndrewaall/claude-watch/tree/main/container/watchers) in the claude-watch repo.
2. The `Dockerfile` `COPY` line for `container/watchers/` already lands them at `/etc/claude-code/watchers/<name>.{sh,toml}` (this skill auto-discovers them).
3. Update `container/watchers/README.md` to document what the watcher does and what events it produces.
4. Rebuild the image and `cwsr` (or `docker compose up -d --force-recreate` if entrypoint-time wiring changed).

## Important

- This skill never starts host-side watchers and never schedules host cron jobs. For host-side scheduled work, see the "Host-side scheduled tasks (via `host-bash`)" section of `/etc/claude-code/CLAUDE.md`.
- The `container/watchers/README.md` documents the on-disk schema (`name`, `description`, `launcher`, `restart_policy`, `log_path`); change there first, then bump the consumers.
- Source dir in repo: [`container/watchers/`](https://github.com/hndrewaall/claude-watch/tree/main/container/watchers). Baked path inside the container: `/etc/claude-code/watchers/`.
