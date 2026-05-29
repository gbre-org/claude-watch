# claude-event system

The `claude-event` bus is a source-agnostic JSON event queue that lets
arbitrary producers (cron jobs, alertmanager webhooks, queue lifecycle,
torrent pipelines, security scans, manual emissions, etc.) deliver
recurring or one-off tasks to a Claude Code main loop.

## Flow

```
producer ──► claude-event "msg" --tag T --source S [...]
              └── atomic write ──► $CLAUDE_EVENT_QUEUE/<ns>_<tag>.json

                    ▼  inotify

claude-event-watch (running as a watcher in the main loop's bg-task list)
   1. Drains anything pre-pending immediately (fast path, no debounce).
   2. Otherwise blocks on `inotifywait`.
   3. After the first new event, sleeps DEBOUNCE_SECONDS (default 30) so
      additional events from the same burst (e.g. 45 torrent-completed
      events finishing roughly together) accumulate.
   4. Drains the queue, prints one line per event, deletes each file.
   5. Appends each event (compact JSON, one per line) to
      $CLAUDE_EVENT_LOG_DIR/consumed.jsonl (rotated at 10k lines).
   6. Prints the WATCHER EXITED restart banner and exits.

main loop reads stdout, dispatches per-tag, restarts the watcher.
```

## Stdout shape

Each consumed event surfaces as exactly one line:

```
EVENT[<source>/<tag>] <first-60-chars-of-message>…
```

The shape is intentionally narrow — the main loop's bash-task view stays
small. To see details, use `claude-event-tail` against the ring buffer.

## Debounce / cooloff

The cooloff window is the primary lever that keeps the main loop from
cycling rapidly through event floods. All events that land between the
first event firing and the cooloff expiring are emitted in a SINGLE
captured-output batch — the main loop sees one watcher restart per
~30s window, not one per event.

Configure via:

- `--debounce SECONDS` — CLI flag (takes precedence). `0` disables it
  (falls back to a short 2s settle).
- `$EVENT_WATCH_DEBOUNCE_SECONDS` — environment variable.

The cooloff only applies AFTER the watcher has been blocking on inotify.
If events are already pending when the watcher starts up, they're
surfaced immediately on the fast path with no cooloff — debounce is for
fresh bursts, not backlog drain.

## Source types

Every event carries a `source` field:

| Source | When to use |
|--------|-------------|
| `cron` | Recurring cron job (default) |
| `alertmanager` | Prometheus alertmanager webhook |
| `queue` | `session-task` queue lifecycle emission |
| `torrent` | Torrent-related pipeline event |
| `security` | Security-scan / patcher pipeline |
| `claude-watch` | Emitted by the `claude-watch` daemon (e.g. watcher-down) |
| `manual` | Hand-emitted for testing / ad-hoc use |

The JSON also records `source_name` (originating script/job) and `tag`
(routing key). Dispatch decisions use `tag`; `source` is for filtering
and dashboards.

## CLIs

### Emit — `claude-event`

```
claude-event "message" \
    [--tag TAG] \
    [--priority low|normal|high|urgent] \
    [--source cron|alertmanager|queue|torrent|security|claude-watch|manual] \
    [--source-name NAME] \
    [--data KEY=VAL ...]
```

Validates `--source` against the allowlist; rejects unknown values with
rc=2. Generates a filename of shape `<ns_ts>_<safe_tag>.json` (slashes /
colons in the tag are sanitised to `_`).

The legacy name `cron-event` is preserved as a shim that prints a
deprecation warning to stderr.

### Read — `claude-event-tail`

```
claude-event-tail [-n N] [--tag PAT] [--source SRC] [--since DUR] [--json]
```

Read-only viewer for the ring-buffered consumed log. Default: pretty
table, last 20 entries, newest first.

Filters:

- `--tag PAT` — case-insensitive substring match
- `--source SRC` — exact source match
- `--since DUR` — `5m | 1h | 1d | 7d` (suffix `s/m/h/d/w`)

Never deletes from the log or the queue.

### Watch — `claude-event-watch`

```
claude-event-watch [--debounce SECONDS] [-h]
```

Bash watcher. Drains pending events, then blocks on `inotifywait` for
new ones. See "Flow" above. Output goes to stdout (one-liners) and the
ring-buffer log (compact JSON).

## File locations

- **Queue**: `$CLAUDE_EVENT_QUEUE`, default `~/claude-events/`. Legacy
  `$CRON_EVENT_QUEUE` is honored.
- **Consumed log**: `$CLAUDE_EVENT_LOG_DIR/consumed.jsonl` (default
  `~/.config/claude-events/`), rotated to `.1`/`.2`/`.3`.
- **Ring buffer max lines**: `$CLAUDE_EVENT_LOG_MAX_LINES` (default 10000).

## Schema

```json
{
  "timestamp":     1735776000.123,
  "timestamp_iso": "2026-01-01T12:00:00-05:00",
  "hostname":      "...",
  "source":        "cron",
  "source_name":   "cron-my-task",
  "tag":           "my-tag",
  "priority":      "normal",
  "message":       "...",
  "data":          {"key": "val"},
  "pid":           12345,
  "user":          "..."
}
```

## Adding a new claude-event

1. Write an emitter (typically a one-line wrapper around `claude-event`):
   ```bash
   #!/bin/bash
   exec claude-event "Description of what to do" \
       --tag my-tag \
       --source cron \
       --source-name cron-my-task \
       --priority normal
   ```
2. Wire it into your scheduler (cron, systemd timer, alertmanager
   webhook, etc.).
3. Update the per-host routing table that maps `tag` → action.
4. Test: run the emitter manually — `claude-event-watch` should pick
   it up on its next inotify wakeup.

## Tests

```
make test-claude-event     # 11 cases (emit + tail round-trip)
make test-watchers         # claude-event-watch fast-path smoke test
```

## Enforcing that events get read

The `claude-event` bus moves events into the main loop's context, but
nothing on the bus itself ensures the LLM actually triages them. The
companion enforcement layer is `event_must_act` — an obligation-gate
that classifies events into four tiers (ambient / actionable / Signal /
unknown), routes them to the right queue, and DENIES Bash tool calls
after the main loop misses N consecutive opportunities to act on an
actionable event. See [event-must-act.md](event-must-act.md) for the
tier model, the `event-classify` + `event-ack` CLIs, and the
container-baked deploy + smoke-test path.
