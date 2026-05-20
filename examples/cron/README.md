# Periodic claude-event emissions via cron

End-to-end example for wiring **periodic claude-event emissions** into a host's
cron. Drop the crontab in place, make sure `claude-event-watch` is running, and
the Claude Code session attached to that watcher starts receiving one-liner
event notifications on the schedules you define.

## Files

| Path                    | What it is                                                |
| ----------------------- | --------------------------------------------------------- |
| `example.crontab`       | Three cron rows demonstrating the canonical patterns      |
| `disk-space-check.sh`   | Working check-script example used by row 2 of the crontab |
| `private-example/`      | Container-cron example for `/etc/cron.d/private` mount    |
| `README.md`             | This file                                                 |

## Host cron vs. container cron

If you're running claude-watch in the **container** (from
`container/compose.yml`), see `private-example/` instead — the canonical
install surface for recurring tasks inside the container is the
`/etc/cron.d/private` bind-mount, and `private-example/` covers
end-to-end install + verify for that path.

If you're running claude-watch **directly on the host** (no container),
read on — this directory's `example.crontab` is for you.

The two are complementary: a deployment can have host cron AND a
container with private cron entries, and they fire independently into
their respective event rings.

## What it demonstrates

Three patterns that cover the vast majority of cron-driven event use cases:

1. **Direct one-liner** — the cron row itself emits the event. Useful for
   plain heartbeats and periodic reminders. No script needed.

2. **Check-and-conditionally-emit script** — the cron row runs a script that
   evaluates some state and only emits when there's something to surface.
   See `disk-space-check.sh`; swap the check for any condition you care about
   (service health, queue depth, certificate expiry, etc.).

3. **Heartbeat with structured metadata** — same as pattern 1 but uses
   `--data KEY=VAL` (repeatable) to attach state the consumer can pivot on.

## Install

### System cron (root, recommended)

```sh
sudo cp examples/cron/example.crontab /etc/cron.d/claude-events-example
sudo chown root:root /etc/cron.d/claude-events-example
sudo chmod 644       /etc/cron.d/claude-events-example
```

Cron rejects `/etc/cron.d/` entries that are symlinks or non-root-owned with
`WRONG FILE OWNER` and silently skips them — copy the file in place, do not
symlink.

Then replace every literal `USER` in the installed file with the username that
should run each job (e.g. `alice`).

### User cron

Paste the rows from `example.crontab` into `crontab -e` and **drop the USER
column** from each row — user crontabs don't have it.

## Prerequisites

- The `claude-event` CLI must be on PATH. After cloning this repo, the common
  pattern is to symlink it into a system-wide bin dir:

  ```sh
  sudo ln -s "$PWD/tools/claude-event/claude-event" /usr/local/bin/claude-event
  ```

- The `claude-event-watch` watcher must be running. It consumes events from
  `~/claude-events/` and surfaces them to the attached Claude Code session.
  See `tools/watchers/claude-event-watch`.

- For `disk-space-check.sh`: standard POSIX `df` + `awk` (any Linux host).

## Verify it's working

After installing (and waiting for the first cron tick), tail the consumed-event
ring buffer:

```sh
claude-event-tail -n 5 --tag generic-heartbeat --since 1h
claude-event-tail -n 5 --tag morning-ping     --since 1d
```

Or fire one of the rows manually to round-trip immediately:

```sh
claude-event "manual test from cron example" --tag generic-heartbeat --source cron
claude-event-tail -n 1 --tag generic-heartbeat --since 1m
```

You should see the tag, source, and message you emitted.

## Customise

The example is intentionally generic. To wire your own periodic signal:

1. Decide whether it's a plain heartbeat (pattern 1 / 3) or a conditional
   check (pattern 2).
2. For a plain heartbeat: add a new cron row that invokes `claude-event`
   directly with your own `--tag`.
3. For a conditional check: copy `disk-space-check.sh`, replace the `df` /
   threshold logic with your own check, and point a new cron row at it.

Tag names are freeform — pick something descriptive enough that you can
filter on it in `claude-event-tail --tag`.

## See also

- `tools/claude-event/README.md` — full CLI reference (flags, sources, schema)
- `tools/watchers/claude-event-watch` — the consumer the events flow into
