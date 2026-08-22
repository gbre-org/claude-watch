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

## Agent liveness: silence is not death

Subagents run in-process and share the parent Claude Code PID, so there is
no per-agent PID to probe. `claude-watch active-agents` infers liveness
from the agent's JSONL transcript mtime, and an agent normally touches
that transcript every tool call — hence the 120s default window.

That assumption breaks in exactly one place, and it is a place agents are
*told* to go: a long job is supposed to run as ONE long foreground call.
For the whole of that call the agent writes nothing, so a maximally busy
agent looks maximally dead. Measured 2026-08-22: an agent driving a
150-minute render sat in a single ~10-minute wait loop, its transcript
aged to 443s, `has_live_owner` went to 0 and `WorkQueueOrphaned` fired —
then resolved when the call returned, then fired again on the next one.

The fix reads the END of the transcript when (and only when) the mtime
check already said "stale". If the last record is an assistant frame
carrying a `tool_use` block, no `tool_result` has come back yet: the agent
is INSIDE that call. Those agents get the longer
`DEFAULT_AGENT_TOOL_CALL_MAX_AGE_SECS` (900s) window and publish
`in_flight_tool_use: true` in `active-agents.json`, which is why a record
can read `alive: true` with a ten-minute-old transcript.

Two properties this deliberately keeps:

- **The grace is bounded.** Past 900s the normal verdict stands, so an
  agent killed mid-call still surfaces as dead — just later.
- **The other death mode is untouched.** When the MODEL turn dies (API
  5xx, dropped stream) the last record is a `tool_result`, not a
  `tool_use`, so that agent trips the 120s window on schedule.

`WorkQueueStuckSoft` carries the matching exclusion on the alert side:
an item whose owning agent's transcript is younger than 900s is
progressing, and is subtracted from the alert the same way a workload
with a fresh progress heartbeat is. Without it every agent-owned item
alerted purely for running longer than 30 minutes, since agents have no
heartbeat file to match the workload clause.

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
