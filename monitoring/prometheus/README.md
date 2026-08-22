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

The exporter and queue-minisite resolve a running item's owner through the
**same three-step ladder**, in the same order, so the dashboard and the
alert can never disagree about who owns an item:

1. an `active-agents.json` record keyed on **this item's `queue_id`**
   (parsed from the agent transcript's `Queue item: q-XXXX` marker);
2. the item's **register-time `agent_id` stamp**, written by
   `session-task queue register`. This is the only owner signal for an
   agent RESUMED onto a rotated queue id: its transcript still carries the
   ORIGINAL marker, so active-agents keys it under the OLD qid and the
   arm-hook binding names the OLD qid too;
3. the arm-hook **`agent-queue-bindings.json`** binding
   (`queue_id -> agent_id`, written synchronously at spawn by
   `post-tool-agent-arm-hook`), which beats the 60s active-agents poll.

Steps 2 and 3 name an owner that was definitively spawned; their liveness
is recovered from any active-agents record carrying that `agent_id`, and
when none resolves the exporter emits `has_live_owner=1` (owner known,
liveness ambiguous) rather than page on a live agent. So `has_live_owner=0`
fires only for GENUINELY owner-less items — the never-spawned
(`agent_id=""`) or died-after-spawn (`agent_id` set) cases
`WorkQueueOrphaned` is meant to catch.

**Liveness is never a pid.** Subagents share the parent Claude Code PID,
and a container-spawned agent has no pid the exporter could resolve at all,
so any pid-shaped check fails exactly the agents it most needs to see. The
only liveness input is active-agents' `alive` flag, which already folds in
the in-flight-tool-use grace described below.

The exporter carried steps 1 and 3 but **not** step 2 until 2026-08-22.
A container-spawned agent whose only owner signal was the register-time
stamp therefore fell all the way through to the never-spawned-orphan
fallback and published
`worktask_queue_item_has_live_owner{agent_id="",status="running"} = 0`
while the minisite — which had honoured the stamp since #3615/#3617 —
showed the same agent alive.

### Missing inputs are a deployment fault, not an orphan storm

Both owner inputs are files the exporter must be able to SEE; in a
container that means bind mounts:

| Env var | Container default | Host source |
| --- | --- | --- |
| `AGENT_STATE_JSON` | `/agents-state/active-agents.json` | the dir claude-watch writes `active-agents.json` into, mounted `:ro` |
| `AGENT_QUEUE_BINDINGS_JSON` | `/queue-home/.config/claude/agent-queue-bindings.json` | the user's `.config/claude` dir, where the arm-hook writes bindings |

An unmounted path yields an empty map that is **indistinguishable from
"nothing is running"**, so a deployment fault used to surface as every
running item going orphaned at once, silently. Now:

- each input's readability is published as
  `worktask_queue_owner_input_available{input="agent_state"|"agent_queue_bindings"}`
  (1 = readable and well-formed, 0 = missing / unreadable / malformed);
- a **WARNING is logged on every state change**, naming the path AND the
  env var to fix (once per transition, not once per scrape);
- while NEITHER input is readable the never-spawned-orphan fallback is
  **suppressed**: `worktask_queue_item_has_live_owner` goes ABSENT rather
  than 0. One readable input is enough to keep the fallback live.

Alert on `worktask_queue_owner_input_available == 0`. Never read the orphan
metric's silence as health.

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
`AgentStateFileMissing`, `WorkQueueOwnerInputMissing`,
`ClaudeEventsBacklogStale`, `ClaudeWatchDown`, `ClaudeWatchersMissing`,
`ClaudeMainLoopHeartbeatStale`.

Metric provenance:
- `worktask_queue_*` → `exporters/work-queue-exporter/work_queue_exporter.py`
- `claude_events_*` → `exporters/claude-events-exporter/claude_events_exporter.py`
- `claude_watch_*` / `claude_code_*` → `claude-watch metrics` textfile (`src/metrics.rs`)
