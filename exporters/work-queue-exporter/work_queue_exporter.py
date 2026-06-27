#!/usr/bin/env python3
"""Prometheus exporter for session-task work-queue stats.

Reads ~/.config/session/queue.json on every scrape (cheap — <100KB JSON) and
exposes metrics at /metrics on PORT.

Owner-liveness model (rev 2026-05-01-v3 — claude-watch agent-state):

  Subagents share the parent Claude Code PID, so per-subagent /proc
  liveness is impossible. The previous PID-and-heartbeat schemes both
  worked around symptoms of that fact (false-positive orphan alerts
  on ephemeral shell PIDs, then heartbeat-grace windows that subagents
  had to keep refreshing). This exporter replaces both with the
  authoritative source: claude-watch's `active-agents --write-state`
  output, which lists every subagent JSONL in the active session along
  with its parsed `Queue item: q-XXXX` marker and an mtime-based
  alive flag.

  For each running queue.json item, we look up its agent record by
  `queue_id`. If found, has_live_owner reflects that record's `alive`
  field. If NOT found we normally stay silent (silence beats either
  false-alert or false-healthy when we genuinely have no signal) —
  EXCEPT for `running` items whose `last_heartbeat_at` (falling back to
  `registered_at` / `started_at`) is older than
  ORPHAN_HEARTBEAT_STALE_SECONDS: those are never-spawned / abandoned-
  without-binding orphans (a `running` item whose Agent was never fired,
  so no transcript ever existed). For them we emit has_live_owner=0 with
  agent_id="" (the empty agent_id distinguishes a no-binding
  orphan from a died-after-spawn one). `blocked` items are exempt — they
  have no live agent by design. A fresh / unparseable heartbeat stays
  silent so a just-registered item is not false-flagged before its beat.

Lock-awareness (rev 2026-05-09 — queue lock feature):

  queue.json carries a top-level `locked_scopes` dict whose keys are
  scope tokens currently parked by `session-task queue lock`. A pending
  item is effectively blocked when ANY token in its `scope` list matches
  a key in `locked_scopes`. Such items MUST NOT appear in
  `worktask_queue_item_ready_age_seconds` — they are intentionally held,
  not stuck. Instead they appear in the new
  `worktask_queue_item_locked_age_seconds` gauge (same shape, different
  name) so the lock state is visible in Grafana without triggering alerts.

Progress-vs-runtime (rev 2026-05-16 — workload heartbeat):

  Running items whose `scope` includes a `workload:<label>` token are
  long-lived fire-and-forget system jobs (stv-promote, rsync, ffmpeg)
  that the main loop has dispatched to the `tasks` tmux session via
  `workload run`. For these, raw elapsed-since-registered is a poor
  stuck signal — a healthy 90-minute rsync is not stuck, even though
  `worktask_queue_items_running_elapsed_seconds` will read 5400s.

  PR #208 / #209 in claude-watch wired a per-workload progress
  heartbeat at `/run/claude/workloads/<label>.heartbeat` — a sidecar
  re-touches the file ONLY when the workload's `.output` file grows
  (i.e. real progress, not a dumb timer). Stat that file and expose
  `now - mtime` as `worktask_queue_item_progress_age_seconds`. The
  WorkQueueStuck alert can then require BOTH long runtime AND stale
  progress before firing, eliminating false-positives on legitimately
  long-running tasks.

  Items without a `workload:*` scope token (i.e. agent tasks) do NOT
  emit this gauge — they have no progress signal of their own.
  WorkQueueStuck handles them via the `unless on(id)` join: the alert
  fires only on items WITHOUT a progress_age series (agents) OR items
  WITH stale progress (workloads). Either-or, never both timers AND'd
  against an absent metric.

Metrics:
  - worktask_queue_items_total{status}       gauge  (pending/running/done/abandoned)
  - worktask_queue_duration_seconds{phase}   histogram (wait/run/total)
  - worktask_queue_scope_conflicts_total     counter (forced_enqueue=true items)
  - worktask_queue_done_total{created_by}    counter  (done items by creator)
  - worktask_queue_group_size{group_id}      gauge (non-empty, non-done-only groups)
  - worktask_queue_items_by_priority{priority} gauge
  - worktask_queue_items_running_elapsed_seconds{id,summary} gauge (per running item)
  - worktask_queue_item_has_live_owner{id,summary,agent_id} gauge (1=alive, 0=orphaned)
        Drives the WorkQueueOrphaned alert. Source: claude-watch
        active-agents.json. Items with a matching agent record reflect
        its `alive` flag. `running` items with NO agent record but a
        `last_heartbeat_at` older than ORPHAN_HEARTBEAT_STALE_SECONDS
        emit has_live_owner=0 with agent_id="" (never-spawned /
        abandoned-without-binding orphan). All other no-record items are
        absent from the gauge entirely.
  - worktask_queue_item_agent_jsonl_age_seconds{id,summary,agent_id} gauge
        Mirror of claude-watch's per-agent jsonl_age_seconds for the
        running queue items. Useful for graphing "how stale is this
        agent's transcript" and tuning the alive threshold.
  - worktask_queue_item_ready_age_seconds{id,summary} gauge (seconds since
        `created_at` for items that are pending AND group_head=true AND
        NOT scope-locked AND have no `dep_blockers`. Drives
        WorkQueueReadyStuck.)
  - worktask_queue_item_locked_age_seconds{id,summary,lock_scope} gauge
        (seconds since `created_at` for items that are pending AND
        group_head=true AND whose scope intersects locked_scopes. These
        are intentionally held; they MUST NOT drive the ReadyStuck alert.)
  - worktask_queue_item_progress_age_seconds{id,summary,workload_label}
        gauge (seconds since the per-workload heartbeat file at
        WORKLOAD_HEARTBEAT_DIR/<label>.heartbeat was last touched.
        Emitted ONLY for running queue items with a `workload:*` scope
        token. The heartbeat is progress-driven (claude-watch PR #209):
        sidecar re-touches the file only when the workload's `.output`
        file grows, so a hung wrapped command produces a stale
        heartbeat. WorkQueueStuck uses this gauge to distinguish
        genuinely-stuck workloads from healthy long-running ones.
        Absent if the heartbeat file is missing — the alert join
        accounts for that case. )
  - worktask_queue_file_last_modified        gauge  (mtime of queue.json)
  - worktask_queue_agent_state_last_modified gauge  (mtime of active-agents.json,
        OR 0 if file missing — useful for alerting when claude-watch
        stops publishing the state file)
  - worktask_queue_scrape_errors_total       counter (reads that failed)
"""

import json
import logging
import os
import time
from datetime import datetime, timezone
from http.server import BaseHTTPRequestHandler, HTTPServer

from prometheus_client import (
    CollectorRegistry,
    Counter,
    Gauge,
    Histogram,
    generate_latest,
    CONTENT_TYPE_LATEST,
)

# Shared loader / dedup logic — lives in claude_agents.py alongside this
# exporter in claude-watch/exporters/work-queue-exporter/.
from claude_agents import agents_by_queue_id, load_agent_state

logging.basicConfig(level=logging.INFO, format="%(asctime)s %(levelname)s %(message)s")
log = logging.getLogger("work-queue-exporter")

PORT = int(os.environ.get("PORT", "9099"))
QUEUE_PATH = os.environ.get("QUEUE_JSON", "/queue/queue.json")
# Path to the JSON state file claude-watch writes via
# `claude-watch active-agents --write-state`. Container deployments
# bind-mount /var/lib/claude-watch from the host. Override for tests.
AGENT_STATE_PATH = os.environ.get(
    "AGENT_STATE_JSON", "/agents-state/active-agents.json"
)
# Directory holding per-workload progress heartbeat files written by
# claude-watch's workload wrapper (PR #208 / #209). One file per active
# workload, named `<label>.heartbeat`. Re-touched only when the wrapped
# command emits new bytes to its .output file. The exporter stats each
# file's mtime to compute `worktask_queue_item_progress_age_seconds`.
# Host path is /run/claude/workloads/; in the container we bind-mount
# it at /workload-heartbeats:ro.
WORKLOAD_HEARTBEAT_DIR = os.environ.get(
    "WORKLOAD_HEARTBEAT_DIR", "/workload-heartbeats"
)
WORKLOAD_SCOPE_PREFIX = "workload:"
# Directory holding per-hostjob progress heartbeat files written by the
# `hostjob` runner (`examples/compose/bin/hostjob`). UNLIKE workload's flat
# `<label>.heartbeat`, hostjob nests the heartbeat inside a per-label
# dir: `<HOSTJOB_HEARTBEAT_DIR>/<label>/heartbeat`. The runner touches it
# on progress; we stat its mtime to compute the same
# `worktask_queue_item_progress_age_seconds` gauge. Host path is
# ~/.cache/hostjob/; container bind-mounts it at /hostjob-heartbeats:ro.
HOSTJOB_HEARTBEAT_DIR = os.environ.get(
    "HOSTJOB_HEARTBEAT_DIR", "/hostjob-heartbeats"
)
HOSTJOB_SCOPE_PREFIX = "hostjob:"
# Staleness threshold (seconds) for flagging a `running` queue item with
# no agent record as a never-spawned / abandoned-without-binding orphan.
# 10-min default -- generous enough to avoid racing a just-registered
# item before its first heartbeat / agent-state publish. The
# WorkQueueOrphaned alert's own `for: 5m` adds further dwell on top.
ORPHAN_HEARTBEAT_STALE_SECONDS = int(
    os.environ.get("ORPHAN_HEARTBEAT_STALE_SECONDS", "600")
)

REG = CollectorRegistry()

g_items_total = Gauge(
    "worktask_queue_items_total",
    "Count of work-queue items by status",
    ["status"],
    registry=REG,
)
g_items_priority = Gauge(
    "worktask_queue_items_by_priority",
    "Count of non-terminal (pending+running) work-queue items by priority",
    ["priority"],
    registry=REG,
)
g_group_size = Gauge(
    "worktask_queue_group_size",
    "Member count per currently non-empty (non-done-only) group",
    ["group_id"],
    registry=REG,
)
g_running_elapsed = Gauge(
    "worktask_queue_items_running_elapsed_seconds",
    "Elapsed seconds since each currently-running item was registered",
    ["id", "summary"],
    registry=REG,
)
g_has_live_owner = Gauge(
    "worktask_queue_item_has_live_owner",
    (
        "1 if the queue item has a live agent owner, 0 if "
        "orphaned. Source: claude-watch active-agents JSON state file. "
        "Matched by `queue_id` parsed from the agent JSONL's first user "
        "message (`Queue item: q-XXXX` marker). Items with no matching "
        "agent record are normally absent from this gauge (no signal "
        "beats false-alert OR false-healthy) -- EXCEPT `running` items "
        "with no agent record AND a `last_heartbeat_at` older than "
        "ORPHAN_HEARTBEAT_STALE_SECONDS, which emit 0 with agent_id empty "
        "(never-spawned / abandoned-without-binding orphan -- an Agent "
        "was never fired so no transcript ever existed). "
        "The `status` label is the queue item's current state: "
        "`running` (alert candidate) or `blocked` (parked on external "
        "blocker, NOT an alert candidate -- no live agent expected by "
        "design). Alert rules MUST filter on {status='running'} to "
        "avoid firing on blocked items."
    ),
    ["id", "summary", "agent_id", "status"],
    registry=REG,
)
g_agent_jsonl_age = Gauge(
    "worktask_queue_item_agent_jsonl_age_seconds",
    (
        "Age in seconds of the owning agent's JSONL transcript, mirrored "
        "from claude-watch active-agents. Useful for graphing transcript "
        "freshness and tuning the alive threshold. The `status` label "
        "mirrors `worktask_queue_item_has_live_owner` (`running` or "
        "`blocked`)."
    ),
    ["id", "summary", "agent_id", "status"],
    registry=REG,
)
g_ready_age = Gauge(
    "worktask_queue_item_ready_age_seconds",
    (
        "Seconds since `created_at` for queue items that are pending AND "
        "group_head=true AND NOT scope-locked AND have empty `dep_blockers` "
        "(i.e. genuinely waiting for the main loop to spawn). Drives the "
        "WorkQueueReadyStuck alert. Items waiting on an upstream depends_on "
        "task are intentionally serialized, not stuck, and are omitted."
    ),
    ["id", "summary"],
    registry=REG,
)
g_locked_age = Gauge(
    "worktask_queue_item_locked_age_seconds",
    (
        "Seconds since `created_at` for queue items that are pending AND "
        "group_head=true AND whose scope intersects locked_scopes. These "
        "are intentionally held by `session-task queue lock` and MUST NOT "
        "trigger the WorkQueueReadyStuck alert. The `lock_scope` label "
        "is the first matching locked scope token for context."
    ),
    ["id", "summary", "lock_scope"],
    registry=REG,
)
g_progress_age = Gauge(
    "worktask_queue_item_progress_age_seconds",
    (
        "Seconds since the per-workload progress heartbeat file at "
        "WORKLOAD_HEARTBEAT_DIR/<label>.heartbeat was last touched. "
        "Emitted ONLY for running queue items whose `scope` includes a "
        "`workload:<label>` token. The heartbeat is progress-driven "
        "(claude-watch PR #209): the wrapper sidecar re-touches the "
        "file only when the wrapped command's .output file grows, so "
        "a hung command yields a stale heartbeat. The same gauge is "
        "also emitted for `hostjob:<label>` items (the `examples/compose/bin/hostjob` "
        "hostjob runner touches HOSTJOB_HEARTBEAT_DIR/<label>/heartbeat); "
        "the `workload_label` dimension carries the hostjob label in that "
        "case (the metric/join key is `id`, so the label is informational). "
        "WorkQueueStuck joins "
        "this gauge against worktask_queue_items_running_elapsed_seconds "
        "to require BOTH long runtime AND stale progress before firing, "
        "eliminating false-positives on healthy long-running tasks. "
        "Absent if the heartbeat file is missing."
    ),
    ["id", "summary", "workload_label"],
    registry=REG,
)
g_file_mtime = Gauge(
    "worktask_queue_file_last_modified",
    "Unix mtime of queue.json",
    registry=REG,
)
g_agent_state_mtime = Gauge(
    "worktask_queue_agent_state_last_modified",
    (
        "Unix mtime of the claude-watch active-agents.json state file. "
        "0 when the file is missing — alert if this stays 0, claude-watch "
        "isn't publishing the state file."
    ),
    registry=REG,
)

c_scope_conflicts = Counter(
    "worktask_queue_scope_conflicts",
    "Items added with forced_enqueue=true (scope-conflict bypasses)",
    registry=REG,
)
c_done_by_creator = Counter(
    "worktask_queue_done",
    "Completed work-queue items, labelled by creator",
    ["created_by"],
    registry=REG,
)
c_scrape_errors = Counter(
    "worktask_queue_scrape_errors",
    "Number of failed queue.json reads",
    registry=REG,
)

# Histogram buckets tuned for agent-task durations: seconds → tens of minutes.
DURATION_BUCKETS = (
    5, 15, 30, 60, 120, 300, 600, 1200, 1800, 3600, 7200, 14400, float("inf"),
)
h_duration = Histogram(
    "worktask_queue_duration_seconds",
    "Wall-clock seconds per work-queue item phase",
    ["phase"],
    buckets=DURATION_BUCKETS,
    registry=REG,
)

# Track which (id, event-type) pairs we've already observed so the counters
# and histogram don't double-count on repeated scrapes.
_seen_forced_ids = set()
_seen_done_ids_by_creator = set()
_seen_duration_ids = set()


def _workload_label_from_scope(scope):
    """Return the workload label from a `workload:<label>` scope token,
    or None if `scope` doesn't include one.

    `scope` is the queue item's scope list (e.g. ["workload:stv-promote",
    "repo:media-tools"]). Workload items have exactly one such token by
    construction (claude-watch workload.rs builds `format!("workload:{label}")`)
    but defensively we return the first match.
    """
    if not scope:
        return None
    for token in scope:
        if isinstance(token, str) and token.startswith(WORKLOAD_SCOPE_PREFIX):
            label = token[len(WORKLOAD_SCOPE_PREFIX):]
            if label:
                return label
    return None


def _hostjob_label_from_scope(scope):
    """Return the hostjob label from a `hostjob:<label>` scope token, or
    None if `scope` doesn't include one.

    Parallel to `_workload_label_from_scope`. The `examples/compose/bin/hostjob` hostjob
    runner builds the scope token as `hostjob:<label>`. Returns the first
    match defensively.
    """
    if not scope:
        return None
    for token in scope:
        if isinstance(token, str) and token.startswith(HOSTJOB_SCOPE_PREFIX):
            label = token[len(HOSTJOB_SCOPE_PREFIX):]
            if label:
                return label
    return None


def _parse_ts(s):
    if not s:
        return None
    try:
        return datetime.fromisoformat(s).astimezone(timezone.utc)
    except (ValueError, TypeError):
        return None


def _load_agent_state_with_mtime():
    """Read claude-watch's active-agents JSON and return ({qid: rec}, mtime).

    Wraps the shared `claude_agents.load_agent_state` + `agents_by_queue_id`
    so the file mtime can be exposed as its own gauge (used to alert when
    claude-watch stops publishing the state file).
    """
    try:
        st = os.stat(AGENT_STATE_PATH)
        mtime = st.st_mtime
    except OSError:
        mtime = 0.0
    state = load_agent_state(AGENT_STATE_PATH)
    return agents_by_queue_id(state), mtime


def collect():
    """Re-read queue.json + agent state and refresh all metrics."""
    try:
        st = os.stat(QUEUE_PATH)
        g_file_mtime.set(st.st_mtime)
        with open(QUEUE_PATH, "r") as f:
            data = json.load(f)
    except (OSError, json.JSONDecodeError) as e:
        log.error("Failed to read %s: %s", QUEUE_PATH, e)
        c_scrape_errors.inc()
        return

    agent_by_qid, agent_mtime = _load_agent_state_with_mtime()
    g_agent_state_mtime.set(agent_mtime)

    items = data.get("items", [])
    # Top-level locked_scopes dict: {scope_token: {reason, locked_at, ...}}
    locked_scopes = set(data.get("locked_scopes", {}).keys())

    # Reset gauges — they may have had stale labels from previous scrapes.
    g_items_total.clear()
    g_items_priority.clear()
    g_group_size.clear()
    g_running_elapsed.clear()
    g_has_live_owner.clear()
    g_agent_jsonl_age.clear()
    g_ready_age.clear()
    g_locked_age.clear()
    g_progress_age.clear()

    status_counts = {
        "pending": 0, "running": 0, "wedged": 0, "blocked": 0,
        "done": 0, "abandoned": 0,
    }
    priority_counts = {}
    group_counts = {}
    now = datetime.now(timezone.utc)

    for it in items:
        status = it.get("status", "unknown")
        status_counts[status] = status_counts.get(status, 0) + 1

        gid = it.get("group_id") or "none"
        g_info = group_counts.setdefault(gid, {"total": 0, "non_done": 0})
        g_info["total"] += 1
        if status not in ("done", "abandoned"):
            g_info["non_done"] += 1

        if status in ("pending", "running"):
            pri = str(it.get("priority", ""))
            priority_counts[pri] = priority_counts.get(pri, 0) + 1

        if it.get("forced_enqueue") and it.get("id") not in _seen_forced_ids:
            _seen_forced_ids.add(it.get("id"))
            c_scope_conflicts.inc()

        # Running-item elapsed gauge + agent liveness gauges. We emit the
        # liveness gauges for BOTH `running` AND `blocked` items but the
        # `status` label distinguishes them so the WorkQueueOrphaned alert
        # rule can filter to `{status="running"}` and not fire on the
        # blocked case (which by design has no live agent).
        if status in ("running", "blocked"):
            reg_ts = _parse_ts(it.get("registered_at") or it.get("started_at"))
            summary = (it.get("summary") or "")[:80] or "(no summary)"
            iid = it.get("id", "")
            if reg_ts and status == "running":
                # running_elapsed stays running-only -- a blocked item
                # isn't burning agent time, so its "elapsed" is the
                # wrong shape for the dashboard panel that consumes
                # this metric.
                elapsed = max(0.0, (now - reg_ts).total_seconds())
                g_running_elapsed.labels(id=iid, summary=summary).set(elapsed)

                # Workload progress heartbeat — emitted only for running
                # items with a `workload:<label>` scope token. The wrapper
                # sidecar (claude-watch PR #209) re-touches the heartbeat
                # file ONLY when the wrapped command's .output file grows,
                # so a stale mtime means "no real progress" -- the load-
                # bearing signal WorkQueueStuck needs to distinguish a
                # healthy long-running rsync from a wedged one.
                workload_label = _workload_label_from_scope(it.get("scope"))
                if workload_label:
                    hb_path = os.path.join(
                        WORKLOAD_HEARTBEAT_DIR, f"{workload_label}.heartbeat"
                    )
                    try:
                        hb_mtime = os.stat(hb_path).st_mtime
                        progress_age = max(0.0, time.time() - hb_mtime)
                        g_progress_age.labels(
                            id=iid, summary=summary,
                            workload_label=workload_label,
                        ).set(progress_age)
                    except OSError:
                        # Heartbeat file missing -- could be a workload
                        # in startup before the sidecar lands, or one
                        # that exited but didn't `queue done` yet, or a
                        # workload run under a uid that couldn't write
                        # to /run/claude/workloads (fail-soft per PR #208).
                        # Stay silent rather than emit a misleading
                        # "infinite age" series; WorkQueueStuck's
                        # `unless` clause handles the absence.
                        pass

                # Hostjob progress heartbeat — parallels the workload block
                # above. The hostjob runner (`examples/compose/bin/hostjob`) touches
                # HOSTJOB_HEARTBEAT_DIR/<label>/heartbeat (per-label DIR,
                # not a flat file). Reuse the same generic
                # worktask_queue_item_progress_age_seconds gauge so
                # WorkQueueStuck (which joins on `id`) covers hostjob items
                # for free; the hostjob label rides in the workload_label
                # dimension (informational — the join key is `id`).
                hostjob_label = _hostjob_label_from_scope(it.get("scope"))
                if hostjob_label:
                    hj_hb_path = os.path.join(
                        HOSTJOB_HEARTBEAT_DIR, hostjob_label, "heartbeat"
                    )
                    try:
                        hj_mtime = os.stat(hj_hb_path).st_mtime
                        hj_progress_age = max(0.0, time.time() - hj_mtime)
                        g_progress_age.labels(
                            id=iid, summary=summary,
                            workload_label=hostjob_label,
                        ).set(hj_progress_age)
                    except OSError:
                        # Heartbeat missing -- hostjob in startup, exited
                        # but not yet flipped, or no progress heartbeat
                        # emitted. Fail-soft (same posture as workload).
                        pass

            # Look up agent by queue_id. Emit has_live_owner ONLY when we
            # have an agent record -- silent on no-signal items.
            agent = agent_by_qid.get(iid)
            if agent is not None:
                aid = agent.get("agent_id", "")
                alive = 1 if agent.get("alive") else 0
                g_has_live_owner.labels(
                    id=iid, summary=summary, agent_id=aid, status=status,
                ).set(alive)
                age = agent.get("jsonl_age_seconds")
                if age is not None:
                    g_agent_jsonl_age.labels(
                        id=iid, summary=summary, agent_id=aid, status=status,
                    ).set(age)
            elif status == "running":
                # Never-spawned / abandoned-without-binding orphan -- a
                # `running` item whose Agent was never fired has NO agent
                # record at all (vs died-after-spawn, which has a record
                # with alive=0 handled above). Without this branch such an
                # item emits no has_live_owner series, so the
                # WorkQueueOrphaned {status=running} == 0 alert matches
                # nothing and never fires. Fall back to heartbeat
                # staleness -- if the item has not heartbeat in
                # ORPHAN_HEARTBEAT_STALE_SECONDS, flag it orphaned with
                # agent_id empty (the empty agent_id distinguishes a
                # no-binding orphan from a died-after-spawn one). ONLY
                # `running` -- `blocked` items legitimately have no live
                # agent by design. A fresh or unparseable heartbeat stays
                # SILENT to preserve no-false-alert on a just-spawned
                # item. No g_agent_jsonl_age is emitted -- no transcript.
                hb_ts = _parse_ts(
                    it.get("last_heartbeat_at")
                    or it.get("registered_at")
                    or it.get("started_at")
                )
                if hb_ts is not None:
                    hb_age = (now - hb_ts).total_seconds()
                    if hb_age >= ORPHAN_HEARTBEAT_STALE_SECONDS:
                        g_has_live_owner.labels(
                            id=iid, summary=summary, agent_id="",
                            status="running",
                        ).set(0)

        # Ready-stuck / locked-age gauges.
        # A pending group-head may be intentionally held by a scope lock
        # OR waiting on an upstream depends_on task (dep_blockers non-empty).
        # Both kinds of items are intentionally blocked, not stuck — they
        # MUST NOT drive the WorkQueueReadyStuck alert. Scope-locked items
        # go to g_locked_age (visible but silent). dep_blockers-blocked
        # items are simply omitted from both gauges; they are already
        # observable via the `dep_blockers` field in queue.json and the
        # upstream item's own running/pending state.
        if status == "pending" and it.get("group_head") and not it.get("dep_blockers"):
            created_ts = _parse_ts(it.get("created_at"))
            if created_ts:
                age = max(0.0, (now - created_ts).total_seconds())
                summary = (it.get("summary") or "")[:80] or "(no summary)"
                iid = it.get("id", "")
                item_scopes = it.get("scope") or []
                # Find first scope token that matches a locked scope, if any.
                lock_match = next(
                    (s for s in item_scopes if s in locked_scopes), None
                )
                if lock_match:
                    # Intentionally held — visible but NOT alertable.
                    g_locked_age.labels(
                        id=iid, summary=summary, lock_scope=lock_match
                    ).set(age)
                else:
                    # Genuinely waiting for the main loop to spawn.
                    g_ready_age.labels(id=iid, summary=summary).set(age)

        # Done-item handling: counter by creator + histogram observations.
        if status == "done":
            iid = it.get("id")
            if iid and iid not in _seen_done_ids_by_creator:
                _seen_done_ids_by_creator.add(iid)
                c_done_by_creator.labels(created_by=it.get("created_by") or "unknown").inc()

            created = _parse_ts(it.get("created_at"))
            registered = _parse_ts(it.get("registered_at") or it.get("started_at"))
            completed = _parse_ts(it.get("completed_at"))

            if registered and created:
                key = (iid, "wait")
                if key not in _seen_duration_ids:
                    _seen_duration_ids.add(key)
                    h_duration.labels(phase="wait").observe(
                        max(0.0, (registered - created).total_seconds())
                    )
            if registered and completed:
                key = (iid, "run")
                if key not in _seen_duration_ids:
                    _seen_duration_ids.add(key)
                    h_duration.labels(phase="run").observe(
                        max(0.0, (completed - registered).total_seconds())
                    )
            if created and completed:
                key = (iid, "total")
                if key not in _seen_duration_ids:
                    _seen_duration_ids.add(key)
                    h_duration.labels(phase="total").observe(
                        max(0.0, (completed - created).total_seconds())
                    )

    for s, n in status_counts.items():
        g_items_total.labels(status=s).set(n)
    for p, n in priority_counts.items():
        g_items_priority.labels(priority=p).set(n)
    for gid, info in group_counts.items():
        if info["non_done"] > 0:
            g_group_size.labels(group_id=gid).set(info["total"])


class MetricsHandler(BaseHTTPRequestHandler):
    def do_GET(self):
        if self.path.split("?", 1)[0] != "/metrics":
            self.send_response(404)
            self.end_headers()
            self.wfile.write(b"not found\n")
            return
        collect()
        body = generate_latest(REG)
        self.send_response(200)
        self.send_header("Content-Type", CONTENT_TYPE_LATEST)
        self.send_header("Content-Length", str(len(body)))
        self.end_headers()
        self.wfile.write(body)

    def log_message(self, fmt, *args):
        log.debug(fmt, *args)


def main():
    log.info("Starting work-queue exporter on :%d (queue=%s, agent_state=%s)",
             PORT, QUEUE_PATH, AGENT_STATE_PATH)
    collect()
    HTTPServer(("0.0.0.0", PORT), MetricsHandler).serve_forever()


if __name__ == "__main__":
    main()
