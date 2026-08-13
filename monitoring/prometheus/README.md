# claude-watch Prometheus rules (canonical, in-repo)

Version-controlled Prometheus **recording + alert rules** for claude-watch's
own metrics. Ships here (#4039) so every deployment reuses ONE source of
truth instead of forking a drifting copy.

- **`claude-watch.rules.yml`** — the rule groups. Validate with
  `promtool check rules monitoring/prometheus/claude-watch.rules.yml`.

## Why the rules live here (not only in the local stack)

The local Grafana/Prometheus **compose stack** lives out-of-tree (in
`andrew-sf-tools/monitoring/`), and the daemon itself does not run
Prometheus. But the rules encode claude-watch's OWN semantics against the
metric names its two Python exporters emit
(`exporters/work-queue-exporter/`, `exporters/claude-events-exporter/`) — so
they belong next to those exporters. Any consumer (the local stack,
gomorrah/`gb`, a future hosted Prometheus) should **symlink or copy** this
file rather than maintain its own. Load it via `rule_files:` in
`prometheus.yml`.

## Two-stage owner-orphan escalation ladder

A `running` queue item that loses its owner is surfaced fast-then-slow:

1. **Stage 1 — `queue-orphaned` claude-event (in-tree, ~2.5 min).**
   `claude-watch queue-check` (cron, every 5 min) emits an ACTIONABLE-tier
   claude-event once a running item has had no agent binding for
   `[queue_check] no_binding_grace_secs` (default 150s). This is the
   FIRST, primary signal — no Prometheus required. Gated by
   `[queue_check] emit_events` (must be `true`; the container bakes it on).

2. **Stage 2 — `WorkQueueOrphaned` Prometheus alert (external, ~15 min).**
   The work-queue-exporter emits `worktask_queue_item_has_live_owner=0` once
   the item's heartbeat is stale past `ORPHAN_HEARTBEAT_STALE_SECONDS`
   (default 600s), and the alert adds `for: 5m` — firing ~15 min after
   register, an order of magnitude past Stage 1. This is the escalation
   BACKSTOP for an orphan the main loop failed to clear after the event,
   NOT the first-line notifier. Keep the `for` + exporter stale window
   comfortably longer than `no_binding_grace_secs` so the event always leads.

## Owner attribution

The exporter and queue-minisite resolve a running item's owner from
claude-watch's `active-agents.json` AND the arm-hook
`agent-queue-bindings.json` (`queue_id -> agent_id`, written synchronously
at spawn by `post-tool-agent-arm-hook`). A **bound** item is treated as
OWNED even during active-agents poll lag, so `has_live_owner=0` fires only
for GENUINELY owner-less items — the never-spawned (`agent_id=""`) or
died-after-spawn (`agent_id` set) cases `WorkQueueOrphaned` is meant to
catch. This is what keeps a live-but-not-yet-polled agent from being
mis-reported as "owner unknown" / falsely paged.

## Rules summary

Recording (`claude-watch-work-queue.recording`):
- `worktask:queue_items_orphaned:count`
- `worktask:queue_items_owned:count`
- `worktask:queue_items_orphaned_never_spawned:count`

Alerts: `WorkQueueOrphaned`, `WorkQueueStuckSoft`, `WorkQueueReadyStuck`,
`AgentStateFileMissing`, `ClaudeEventsBacklogStale`, `ClaudeWatchDown`,
`ClaudeWatchersMissing`, `ClaudeMainLoopHeartbeatStale`.

Metric provenance:
- `worktask_queue_*` → `exporters/work-queue-exporter/work_queue_exporter.py`
- `claude_events_*` → `exporters/claude-events-exporter/claude_events_exporter.py`
- `claude_watch_*` / `claude_code_*` → `claude-watch metrics` textfile (`src/metrics.rs`)
