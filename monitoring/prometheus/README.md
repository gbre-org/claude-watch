# claude-watch Prometheus rules (canonical, in-repo)

Version-controlled Prometheus **recording + alert rules** for claude-watch's
own metrics. Ships here (#4039) so every deployment reuses ONE source of
truth instead of forking a drifting copy.

- **`claude-watch.rules.yml`** — the rule groups. Validate with
  `promtool check rules monitoring/prometheus/claude-watch.rules.yml`.

## Why the rules live here (not only in the local stack)

The local Grafana/Prometheus **compose stack** lives out-of-tree (in an
external monitoring stack), and the daemon itself does not run
Prometheus. But the rules encode claude-watch's OWN semantics against the
metric names its two Python exporters emit
(`exporters/work-queue-exporter/`, `exporters/claude-events-exporter/`) — so
they belong next to those exporters. Any consumer (the local stack,
an external host, a future hosted Prometheus) should **symlink or copy** this
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

### Owner UNKNOWN vs owner ORPHANED

`WorkQueueOrphaned` is about an owner that **went away**. A running item
can also have **never had an attributable owner at all** — no
active-agents record on the qid, no register-time `agent_id` stamp, no
arm-hook binding. That is a queue entry meant for an agent that was never
assigned one, or an agent resumed onto a rotated queue id whose stamp was
never retrofitted.

`worktask_queue_item_owner_unknown_age_seconds` (per item, age measured
from registration) and `worktask_queue_owner_unknown_items` (count) carry
that case, and `WorkQueueOwnerUnknown` alerts on it at
`> 600` `for: 10m`.

The gauge deliberately has **no heartbeat-staleness precondition**. The
never-spawned branch of `has_live_owner` requires a heartbeat older than
`ORPHAN_HEARTBEAT_STALE_SECONDS`, which is exactly why a live-but-ownerless
item is invisible today: an item heartbeating happily while belonging to
nobody never trips it. Age is measured from registration for the same
reason — a heartbeat says the work is alive, never who owns it, so
resetting the clock on each beat would re-hide the case the gauge exists
to expose.

Exempt (matching the queue-check cron's own exemptions):

- items whose scope carries a `workload:` or `hostjob:` token — system
  jobs owned by a **process** in the tasks session, not by an agent;
- items with an explicit `pid` stamp, which names the owning process
  directly;
- `blocked` items, which have no live agent by design;
- everything, while no owner-attribution input is readable —
  `WorkQueueOwnerInputMissing` carries that case instead.

Remedy: stamp the real owner with
`session-task queue assign <id> --agent <agent_id>`, spawn the missing
agent, or abandon the item. `queue register` cannot do the retrofit —
`--if-absent` short-circuits at `already running` and stamps nothing, and a
bare register on a running item is (correctly) refused as a double-spawn
signal.

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

## Verifying which exporter build is deployed

`curl -s localhost:9099/metrics | grep build_info` — the work-queue-exporter
publishes `worktask_exporter_build_info{commit,version,source} 1`, so a
deploy can be confirmed against a revision instead of against a container
create time or a file mtime (which name no revision at all, and were the
only evidence available before this metric existed). `source` is `image` for
a container that baked its identity at build time and `host` for a
directly-run process.

The commit has to be handed IN rather than discovered: `.dockerignore`
prunes `.git/` from the build context and the exporter's slim base image has
no `git`, so the image cannot resolve HEAD itself — the same constraint the
Rust daemon's `claude_watch_build_info` lives under, and the same fix.
`exporters/work-queue-exporter/Dockerfile` takes `--build-arg
CW_BUILD_COMMIT` / `CW_BUILD_VERSION`; `make work-queue-exporter-build`
resolves both on the host and passes them, and the out-of-tree monitoring
compose stack should pass the same two under `build.args`. A host-run or
otherwise un-baked instance can set `WORKTASK_EXPORTER_COMMIT` /
`WORKTASK_EXPORTER_VERSION` / `WORKTASK_EXPORTER_SOURCE` in the process
environment instead; a runtime value overrides the image's baked default.

The series is emitted **unconditionally**, reading `commit="unknown"` when
nothing stamped the build. That is deliberate: had it been dropped instead,
its absence would be ambiguous between "exporter predates this metric" and
"current exporter that failed to get stamped", and resolving exactly that
ambiguity is the metric's job. So `commit="unknown"` means the build args
did not reach the image; an ABSENT series means the exporter is older than
this metric.

## Rules summary

Recording (`claude-watch-work-queue.recording`):
- `worktask:queue_items_orphaned:count`
- `worktask:queue_items_owned:count`
- `worktask:queue_items_orphaned_never_spawned:count`

Alerts: `WorkQueueOrphaned`, `WorkQueueOwnerUnknown`, `WorkQueueStuckSoft`,
`WorkQueueReadyStuck`,
`AgentStateFileMissing`, `WorkQueueOwnerInputMissing`,
`ClaudeEventsBacklogStale`, `ClaudeWatchDown`, `ClaudeWatchersMissing`,
`ClaudeMainLoopHeartbeatStale`.

Metric provenance:
- `worktask_queue_*` → `exporters/work-queue-exporter/work_queue_exporter.py`
- `worktask_exporter_build_info` → same exporter (see "Verifying which
  exporter build is deployed" above). Unalerted by design: it is a deploy
  *identity*, not a health signal.
- `claude_events_*` → `exporters/claude-events-exporter/claude_events_exporter.py`
- `claude_watch_*` / `claude_code_*` → `claude-watch metrics` textfile (`src/metrics.rs`)
