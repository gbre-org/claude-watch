# Operator-supplied cron entries for the claude-watch container

The standard claude-watch deployment is the container defined in
`container/compose.yml`. That container runs its own cron daemon
(baked into the image, exec'd from `container/process-compose.yml`) and
reserves `/etc/cron.d/private` as the documented bind-mount slot for
**operator-supplied recurring tasks**.

This subdirectory shows the canonical shape of a private-cron file and
how to wire it in.

## When to use this vs. `../example.crontab`

Two complementary patterns, one repo:

| Pattern                        | Where                                  | When to use                                                                                   |
| ------------------------------ | -------------------------------------- | --------------------------------------------------------------------------------------------- |
| **Host cron** (the parent dir) | `examples/cron/example.crontab`        | You run `claude-watch` directly on the host (no container). Install via host `/etc/cron.d/`.  |
| **Container cron** (this dir)  | `examples/cron/private-example/`       | You run the standard container from `container/compose.yml`. Install via `/etc/cron.d/private`. |

If you have both — a host-side cron *and* a container deployment — the
two are independent. The host cron emits into the host's
`~/claude-events/`; the container cron emits into the container's
state directory (visible from the host via the `claude-watch-state`
volume or `CW_STATE_PATH` bind-mount, both documented in
`container/compose.yml`).

## What the example does

`private-example.crontab` ships a single row:

- Fires `claude-event "index refresh tick" --tag index-refresh --source cron --data cadence=6h` every 6 hours.
- Runs as the in-container `hndrewaall` user (uid 1000 — the account
  the image bakes for claude-watch).
- Captures stderr to `/tmp/claude-watch-cron-index-refresh.log` so
  failures are inspectable via `docker compose exec`.

The shape is intentionally generic: replace the `claude-event ...`
invocation with whatever periodic command you actually want to run
(index refresh, cache rebuild, periodic re-scan, custom heartbeat).

## Install

1. Copy the file to a host directory you control:

   ```sh
   sudo mkdir -p /etc/claude-watch/private-cron.d
   sudo cp examples/cron/private-example/private-example.crontab \
           /etc/claude-watch/private-cron.d/index-refresh
   sudo chown root:root /etc/claude-watch/private-cron.d/index-refresh
   sudo chmod 644       /etc/claude-watch/private-cron.d/index-refresh
   ```

   The file inside `/etc/cron.d/` (and its subdirs) **must** be a
   regular file owned by root, mode 0644. Cron refuses to load
   symlinks or non-root-owned files with `WRONG FILE OWNER` and
   silently skips them.

2. Uncomment the bind-mount in `container/compose.yml`:

   ```yaml
   volumes:
     # ...
     - "/etc/claude-watch/private-cron.d/:/etc/cron.d/private/:ro"
   ```

   Adjust the host path to match step 1.

3. Recreate the container so cron sees the new file:

   ```sh
   docker compose up -d --force-recreate claude-watch
   ```

   (Cron reads `/etc/cron.d/` at daemon startup; a `--force-recreate`
   restarts the supervisor and re-execs cron.)

## Verify

Once the next cron tick fires, the event should land in the
container's event ring:

```sh
docker compose exec claude-watch \
    claude-event-tail -n 5 --tag index-refresh --since 1d
```

Or fire one round-trip manually to confirm the wiring:

```sh
docker compose exec -u hndrewaall claude-watch \
    claude-event "manual private-cron test" --tag index-refresh --source cron
docker compose exec claude-watch \
    claude-event-tail -n 1 --tag index-refresh --since 1m
```

Inspect any cron-side errors:

```sh
docker compose exec claude-watch ls -la /tmp/claude-watch-cron-*.log
docker compose exec claude-watch cat /tmp/claude-watch-cron-index-refresh.log
```

## How this relates to the baked defaults

The image bakes `/etc/cron.d/cw-default` with three entries (active-agents
publisher, metrics emit, stale-ready watchdog). Those are **always-on**
for every claude-watch container deployment — they don't reference
host-specific paths or operator-specific state, so it's cheaper to bake
them than to require every deployer to wire them up by hand.

`/etc/cron.d/private` is the **additive** slot for entries that *do*
reference host-specific or operator-specific things. The baked file and
the private bind-mount are read together (cron processes
`/etc/cron.d/` recursively), so anything you add here supplements rather
than replaces the defaults.

## Format reference

Standard `/etc/cron.d/` format:

```
m h dom mon dow user command
```

- `user` column is REQUIRED for `/etc/cron.d/` entries (unlike
  `crontab -e` user-crontabs, which omit it).
- In-container the user should be `hndrewaall` (uid 1000); running as
  root inside the container bypasses the permission boundaries the
  image sets up.
- `claude-watch` itself is on PATH at `/usr/local/bin/claude-watch`
  (image-baked). Cron's default `PATH` is the minimal `/usr/bin:/bin`,
  so spell out absolute paths in cron rows OR add an explicit `PATH=`
  line at the top of the file.

## See also

- `../example.crontab` — host-side cron example (3 patterns: heartbeat,
  check-and-emit, structured metadata).
- `../README.md` — host-side cron README.
- `../../tools/claude-event/README.md` — `claude-event` CLI reference.
- `../../tools/watchers/claude-event-watch` — event consumer that
  surfaces emissions into Claude Code.
