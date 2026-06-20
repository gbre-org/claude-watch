# tools/event-must-act — heartbeat / event-response ecosystem (shared)

This directory holds the **deployment-agnostic** scripts behind claude-watch's
event-response tiers and the cron-driven dead-watcher recovery injector. They
were originally container-only (`container/bin/`); they now live here so BOTH
deployments use one copy:

- **Container** (`container/Dockerfile`) bakes each via `COPY tools/event-must-act/<name> /usr/local/bin/<name>`.
- **Non-container / systemd host** symlinks each into the operator's `~/bin`
  (or any PATH dir). Nothing here is container-specific — all state lives under
  `~/.config/claude-events/` (override with `$CLAUDE_EVENT_STATE_DIR`), and the
  one container-pane default in `cw-watcher-health-check` is now env-driven.

## Scripts

### Event-response tiers (the `event_must_act` obligation chain)

The claude-event model has three tiers — **ambient** (info-only context),
**actionable** (demands a response), **excluded** (Signal, owned by its own
ack-gate). The tier of a `source/tag` pair is DATA, in `event-classify`.

- **`event-classify`** — data-driven `source/tag → tier` classifier. Inspect
  with `event-classify --list-rules`. Add a new event source = append a row.
  `heartbeat-tick` is classified **actionable** here (touch the heartbeat file).
- **`event-ack`** — CLI managing the response surface:
  - `event-ack ingest --source S --tag T --message M` — classify + route an
    event into `pending-actions.json` (actionable) or `ambient-context.json`
    (ambient); Signal-tagged events are no-op.
  - `event-ack ack "<key>" --action "<text>"` — clear a pending entry; resets
    the N-tool-call counter.
  - `event-ack list | clear | drain-ambient | reset-counter`.
- **`eval-event-must-act`** — obligations `evaluator` predicate. While
  `pending-actions.json` is non-empty, it bumps a counter on each non-exempt
  Bash tool call and DENIES once the counter reaches `N` (default 3, override
  `$EVENT_MUST_ACT_N`). Default-open on missing/corrupt state.
- **`user-prompt-ambient-inject-hook`** — `UserPromptSubmit` hook; drains the
  ambient queue into the next prompt's context (no gate).

> **Wiring note:** the *ingest* step (`event-ack ingest` per event line) is
> driven by the main loop / `CLAUDE.md` discipline, not automatically by the
> watcher — `claude-event-watch` only prints `EVENT[...]` lines. The forced
> "touch the heartbeat every tick" behavior therefore requires BOTH the
> `event_must_act` obligation seeded (`tools/obligations/obligations-init`
> `seed_event_must_act`, pointing at `/usr/local/bin/eval-event-must-act`) AND
> the loop running `event-ack ingest`. Enable deliberately per deployment.

### Cron-driven recovery injector

- **`cw-watcher-health-check`** — runs from cron (once/min by default). If
  `*.json` event files have sat unconsumed in the spool dir past
  `CW_WATCHER_HEALTH_STALE_MIN` (default 2) minutes, the event watcher is dead
  or stuck, so it injects a `[CLAUDE-WATCH] WATCHER DOWN…` alert into the
  Claude pane via `claude-watch inject` (the ONE verified type-and-submit
  path). A per-condition cooldown (`CW_WATCHER_HEALTH_COOLDOWN_SECS`, default
  600s) prevents re-injecting every tick while a stale window persists.

  This is the **recovery** path for a dead event *watcher*. It is distinct from
  the claude-watch **daemon's** own heartbeat-stale inject, which fires on a
  stale host *heartbeat-file mtime* (the loop stopped touching it). Different
  signals → they don't double-fire on the same condition.

  Env:
  - `CLAUDE_EVENT_QUEUE` (default `~/claude-events`)
  - `CW_WATCHER_HEALTH_STALE_MIN` (default 2)
  - `CW_WATCHER_HEALTH_PANE` — pin a tmux pane; UNSET = let `claude-watch
    inject` resolve it (`$CLAUDE_WATCH_PANE` → `[tmux] dashboard_pane` config →
    auto-detect).
  - `CW_WATCHER_HEALTH_RESTART_HINT` — operator-specific "how to restart the
    watcher" text appended to the alert.
  - `CW_WATCHER_HEALTH_COOLDOWN_SECS` (default 600; 0 disables).
  - `CW_WATCHER_HEALTH_STATE` — cooldown-stamp path.

## Tests

`make test-event-must-act` runs the three Python `--self-test` suites plus
`tests/cw-watcher-health-check.test` (stubs `claude-watch` so nothing injects
into a real pane). Also exercised by `container/tests/event-must-act-wired.test`
and `container/tests/cron-default-baked.test`.
