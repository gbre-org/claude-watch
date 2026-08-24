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

  For each running queue.json item we attribute an owner through three
  steps, in the SAME order queue-minisite's `_classify_owner` uses so the
  dashboard and the alert can never disagree:

    1. an active-agents record keyed on THIS item's `queue_id`
    2. the item's register-time `agent_id` stamp (`session-task queue
       register` #3615/#3617) — the only owner signal for an agent
       RESUMED onto a rotated queue id, whose transcript still carries the
       ORIGINAL `Queue item:` marker
    3. the arm-hook binding (`queue_id -> agent_id`), written
       synchronously at spawn, which beats the 60s active-agents poll

  Step 1 reports that record's `alive` flag directly. Steps 2 and 3 name
  an owner that was definitively spawned; their liveness is recovered from
  any active-agents record carrying that agent_id, and when none resolves
  we emit 1 (owner known, liveness ambiguous) rather than page on a
  live agent. Liveness is ALWAYS active-agents' `alive` — never a pid.
  Subagents share the parent Claude Code PID and a container-spawned agent
  has no pid this exporter could resolve at all, so any pid-shaped check
  fails exactly the agents it most needs to see. `alive` already folds in
  the post-#690 in-flight-tool-use grace, so an agent sitting inside one
  long foreground call reads alive with a ten-minute-old transcript.

  With no owner attributed we normally stay silent (silence beats either
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

Fail-loud on missing inputs (rev 2026-08-22):

  Both owner inputs are files the exporter must be able to SEE — in a
  container that means bind mounts. An unmounted path yields an empty map
  that is indistinguishable from "nothing is running", so the old code
  turned a deployment fault into a queue-wide orphan storm, silently. Now
  each input's readability is published as
  `worktask_queue_owner_input_available{input=...}` and logged loudly on
  every state change (naming the path AND the env var), and while NEITHER
  input is readable the never-spawned-orphan fallback is suppressed
  entirely: `worktask_queue_item_has_live_owner` goes ABSENT rather than 0.
  Alert on the input gauge; never read the orphan metric's silence as
  health.

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
  - worktask_queue_item_owner_unknown_age_seconds{id,summary} gauge
        (seconds since registration for a RUNNING item that no owner can
        be attributed to -- no active-agents record on the qid, no
        register-time `agent_id` stamp, no arm-hook binding. OWNER-UNKNOWN
        is not ORPHANED: orphaned = a known owner is gone; owner-unknown =
        nobody can be named at all, the "queue entry meant for an agent
        that never got one assigned" case. Unlike the never-spawned-orphan
        branch of has_live_owner, this carries NO
        ORPHAN_HEARTBEAT_STALE_SECONDS precondition -- that staleness gate
        is exactly why a live-but-ownerless item is invisible today.
        Suppressed when no owner input is readable; `workload:`/`hostjob:`
        scoped items and items with an explicit `pid` are exempt (system
        jobs owned by a process, not an agent). Drives
        WorkQueueOwnerUnknown; remedy is `session-task queue assign`.)
  - worktask_queue_owner_unknown_items       gauge  (count of the above;
        always emitted, 0 when every running item is attributable)
  - worktask_queue_file_last_modified        gauge  (mtime of queue.json)
  - worktask_queue_agent_state_last_modified gauge  (mtime of active-agents.json,
        OR 0 if file missing — useful for alerting when claude-watch
        stops publishing the state file)
  - worktask_queue_owner_input_available{input} gauge (1 when the named
        owner-attribution input — `agent_state` or `agent_queue_bindings`
        — is readable and well-formed, 0 otherwise. Both 0 means the
        exporter has no owner signal at all and is deliberately silent on
        has_live_owner.)
  - worktask_queue_scrape_errors_total       counter (reads that failed)
  - worktask_exporter_build_info{commit,version,source} gauge, always 1.
        Build identity of the exporter ITSELF, so "is the deployed
        exporter the commit I think it is" has an answer that is not a
        container create time. Stamped at image build via
        `--build-arg CW_BUILD_COMMIT` / `CW_BUILD_VERSION`, or at
        runtime via $WORKTASK_EXPORTER_COMMIT / _VERSION / _SOURCE.
        ALWAYS emitted — commit="unknown" when nothing stamped it, so
        an ABSENT series means only "exporter older than this metric".
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
from claude_agents import (
    INPUT_OK,
    agents_by_agent_id,
    agents_by_queue_id,
    load_agent_queue_bindings_status,
    load_agent_state_status,
)

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
# Path to the arm-hook agent_id -> queue_id bindings file
# (`~/.config/claude/agent-queue-bindings.json` on the host, written by
# post-tool-agent-arm-hook the instant the main loop spawns an Agent).
# Bind-mounted read-only. Consulted to attribute an owner to a running item
# the moment it is spawned -- BEFORE claude-watch's active-agents poller
# (60s) publishes a transcript record -- so a genuinely-owned item is never
# mistaken for a never-spawned orphan during the poll-lag window (the false
# WorkQueueOrphaned this closes). A running item with NO binding AND no agent
# record still falls through to the never-spawned orphan path. Override for
# tests.
AGENT_QUEUE_BINDINGS_PATH = os.environ.get(
    "AGENT_QUEUE_BINDINGS_JSON",
    "/queue-home/.config/claude/agent-queue-bindings.json",
)

# --- build identity ------------------------------------------------------
#
# `worktask_exporter_build_info` exists to answer one question an operator
# could not previously answer at all: is the exporter I am scraping built
# from the commit I think it is? Every other `worktask_*` series describes
# the QUEUE; none described the exporter, so a deploy could only be
# confirmed from container create times and file mtimes, neither of which
# names a revision.
#
# The values come from the environment, and the environment gets them from
# one of two places:
#
#   1. Image build. `Dockerfile` accepts `--build-arg CW_BUILD_COMMIT` /
#      `CW_BUILD_VERSION` and freezes them into ENV alongside
#      `WORKTASK_EXPORTER_SOURCE=image`. That mirrors how the Rust daemon's
#      `claude_watch_build_info` is stamped (the Makefile resolves the
#      commit on the HOST and feeds it in as a build arg) and for the same
#      reason: `.dockerignore` prunes `.git/` from the build context and
#      the slim base image has no `git`, so nothing INSIDE the build can
#      run `git rev-parse` — the identity has to be handed in from outside.
#   2. Runtime. A host-run exporter, or a container whose out-of-tree
#      builder cannot pass build args, sets `WORKTASK_EXPORTER_COMMIT` /
#      `WORKTASK_EXPORTER_VERSION` / `WORKTASK_EXPORTER_SOURCE` in the
#      process environment. A runtime value naturally wins over the image's
#      baked ENV default, which is what makes the same variable serve both.
#
# When NOTHING stamps it we still emit the series, with `commit="unknown"`.
# Omitting it would make its absence ambiguous between "exporter too old to
# have this metric" and "current exporter that failed to get stamped" — and
# removing exactly that ambiguity is the whole point of the metric.
EXPORTER_COMMIT = os.environ.get("WORKTASK_EXPORTER_COMMIT", "").strip() or "unknown"
EXPORTER_VERSION = os.environ.get("WORKTASK_EXPORTER_VERSION", "").strip() or "0.0.0"
EXPORTER_SOURCE = os.environ.get("WORKTASK_EXPORTER_SOURCE", "").strip() or "host"

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
g_owner_unknown_age = Gauge(
    "worktask_queue_item_owner_unknown_age_seconds",
    (
        "Seconds since a RUNNING queue item was registered while NO owner "
        "can be attributed to it -- no active-agents record keyed on the "
        "qid, no register-time `agent_id` stamp, and no arm-hook binding. "
        "This is the `owner unknown` case, and it is NOT the same as "
        "orphaned: orphaned means a KNOWN owner is gone (has_live_owner=0), "
        "while owner-unknown means the item is not attributable to anyone "
        "at all -- classically a queue entry meant for an agent that never "
        "got one assigned, or an agent RESUMED onto a rotated qid whose "
        "owner stamp was never retrofitted. Deliberately NOT gated on "
        "ORPHAN_HEARTBEAT_STALE_SECONDS: the never-spawned-orphan branch "
        "requires a STALE heartbeat, which is exactly why a live-but-"
        "ownerless item is invisible today. Emitted only when at least one "
        "owner-attribution input is readable (see "
        "worktask_queue_owner_input_available) -- with no inputs every item "
        "looks ownerless and that is a deployment fault, not a queue fact. "
        "Items carrying a `workload:` / `hostjob:` scope token or an "
        "explicit `pid` stamp are EXEMPT: those are system jobs owned by a "
        "process, not by an agent, so they have no agent owner to be "
        "missing. Remedy: `session-task queue assign <id> --agent <id>`."
    ),
    ["id", "summary"],
    registry=REG,
)
g_owner_unknown_count = Gauge(
    "worktask_queue_owner_unknown_items",
    (
        "Count of running queue items with no attributable owner (the "
        "series count of worktask_queue_item_owner_unknown_age_seconds). "
        "0 when every running item is attributable -- the series is always "
        "emitted, so its ABSENCE means an exporter predating this metric "
        "rather than a healthy queue."
    ),
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

g_owner_input_available = Gauge(
    "worktask_queue_owner_input_available",
    (
        "1 when the named owner-attribution input is readable and "
        "well-formed, 0 when it is missing / unreadable / malformed. "
        "`input` is `agent_state` (claude-watch active-agents.json) or "
        "`agent_queue_bindings` (the arm-hook queue_id -> agent_id file). "
        "When BOTH read 0 the exporter has NO owner signal at all and "
        "SUPPRESSES worktask_queue_item_has_live_owner entirely rather "
        "than reporting every running item as orphaned -- so alert on "
        "this gauge, not on the silence of the orphan metric."
    ),
    ["input"],
    registry=REG,
)

g_build_info = Gauge(
    "worktask_exporter_build_info",
    (
        "Build identity of the RUNNING work-queue-exporter process; always "
        "1, the labels carry the payload. `commit` is the short git SHA, "
        "`version` a semver, `source` is `image` for a container that baked "
        "them at build time and `host` for a directly-run process. "
        "commit=\"unknown\" means nothing stamped this build -- the series "
        "is emitted anyway so that its ABSENCE means only one thing: an "
        "exporter predating this metric. Verify a deploy with "
        "`curl -s localhost:9099/metrics | grep build_info`."
    ),
    ["commit", "version", "source"],
    registry=REG,
)
# Set once at import rather than per-scrape in collect(): build identity
# cannot change while the process runs, and stamping it here means the
# series is present even on a scrape whose collect() bails on a bad
# queue.json -- the deploy question stays answerable when the queue is not.
g_build_info.labels(
    commit=EXPORTER_COMMIT,
    version=EXPORTER_VERSION,
    source=EXPORTER_SOURCE,
).set(1)

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
    """Read active-agents JSON: ({qid: rec}, {agent_id: rec}, mtime, status).

    Wraps the shared `claude_agents` helpers (`agents_by_queue_id` +
    `agents_by_agent_id`) so the file mtime can be exposed as its own gauge
    (used to alert when claude-watch stops publishing the state file). The
    by-agent_id map lets us resolve the liveness of an owner discovered via
    the arm-hook bindings or the item's register-time stamp, whose agent may
    be keyed under a different queue id in active-agents.

    `status` is the `read_json_input` classification (`ok` / `missing` /
    `unreadable` / `malformed`) so the caller can tell "claude-watch says
    no agents are running" from "I cannot see claude-watch's state file at
    all" — the two produce an identical empty map, and only the first one
    licenses calling anything orphaned.
    """
    try:
        st = os.stat(AGENT_STATE_PATH)
        mtime = st.st_mtime
    except OSError:
        mtime = 0.0
    state, status = load_agent_state_status(AGENT_STATE_PATH)
    return (
        agents_by_queue_id(state),
        agents_by_agent_id(state),
        mtime,
        status,
    )


# Last-reported status per owner-attribution input, so the warning below
# is LOUD ONCE per transition instead of once per scrape (a 15s scrape
# interval would otherwise bury the log). Keyed by the short input name.
_owner_input_last_status = {}

# (short name, env var, path) for each owner-attribution input, used by
# `_report_owner_inputs` for both the gauge and the warning text.
_OWNER_INPUTS = (
    ("agent_state", "AGENT_STATE_JSON", lambda: AGENT_STATE_PATH),
    (
        "agent_queue_bindings",
        "AGENT_QUEUE_BINDINGS_JSON",
        lambda: AGENT_QUEUE_BINDINGS_PATH,
    ),
)


def _report_owner_inputs(agent_state_status, bindings_status):
    """Publish + log the readability of each owner-attribution input.

    Returns True when AT LEAST ONE input is `ok` — i.e. the exporter has
    some trustworthy owner signal and may legitimately conclude that a
    running item has no owner. When BOTH are unusable the caller must stay
    SILENT on has_live_owner: with no inputs, "no record for this item" is
    not evidence of an orphan, it is evidence of a broken deployment, and
    emitting 0 would page on every running item at once.

    A container deployment that never got the bind mounts produces exactly
    that state and produces it silently, which is why this logs a loud
    warning naming the path AND the env var to fix.
    """
    statuses = {
        "agent_state": agent_state_status,
        "agent_queue_bindings": bindings_status,
    }
    for name, env_var, path_fn in _OWNER_INPUTS:
        status = statuses[name]
        g_owner_input_available.labels(input=name).set(
            1 if status == INPUT_OK else 0
        )
        if _owner_input_last_status.get(name) == status:
            continue
        previous = _owner_input_last_status.get(name)
        _owner_input_last_status[name] = status
        if status == INPUT_OK:
            if previous is not None:
                log.info(
                    "owner-attribution input %s recovered (%s)",
                    name, path_fn(),
                )
            continue
        log.warning(
            "OWNER-ATTRIBUTION INPUT %s IS %s at %s (set %s to fix). "
            "Owner attribution for running queue items is DEGRADED; "
            "queue items owned by agents this exporter cannot see will "
            "read agent_id=\"\". Container deployments must bind-mount "
            "this path from the host.",
            name, status.upper(), path_fn(), env_var,
        )
    return any(st == INPUT_OK for st in statuses.values())


def _stamped_owner_agent_id(item):
    """Return the item's register-time `agent_id` stamp, or None.

    `session-task queue register` stamps the invoking subagent's agent_id
    onto the item (`agent_id_source: "register"`). That stamp is the ONLY
    owner signal for an agent RESUMED onto a rotated queue id: its
    transcript still carries the ORIGINAL `Queue item:` marker, so
    active-agents keys it under the old qid and the arm-hook binding names
    the old qid too. queue-minisite's `_classify_owner` has honoured this
    stamp since #3615/#3617; the exporter did not, which is how a
    demonstrably-live agent surfaced as `has_live_owner=0, agent_id=""`.
    """
    aid = item.get("agent_id")
    if isinstance(aid, str) and aid:
        return aid
    return None


def _resolve_owner(iid, item, agent_by_qid, agent_by_aid, owner_bindings):
    """Attribute an owner to a queue item. Returns a dict or None.

    Precedence mirrors queue-minisite's `_classify_owner` exactly, so the
    dashboard and the alert can never disagree about who owns an item:

      1. an active-agents record keyed on THIS queue id
      2. the item's register-time `agent_id` stamp  (#3615/#3617)
      3. the arm-hook binding (queue_id -> agent_id)

    Steps 2 and 3 name an owner that was DEFINITIVELY spawned; their
    liveness is recovered from any active-agents record carrying that
    agent_id. When no such record resolves, `alive` is None — owner known,
    liveness ambiguous — which the caller renders as 1, not 0. Presuming a
    known owner alive is the only safe default: if it really died,
    active-agents publishes alive=false for that agent_id on its next poll
    and the gauge flips honestly.

    NOTE: liveness comes exclusively from active-agents' `alive` field,
    which already folds in the post-#690 in-flight-tool-use grace (an agent
    inside one long foreground call writes nothing for minutes and is still
    alive). There is deliberately NO pid probe anywhere on this path:
    subagents share the parent Claude Code PID, and a container-spawned
    agent has no pid this exporter could resolve even in principle.

    Returns None when no owner can be attributed at all.
    """
    agent = agent_by_qid.get(iid)
    if agent is not None:
        return {
            "agent_id": agent.get("agent_id", ""),
            "alive": bool(agent.get("alive")),
            "age": agent.get("jsonl_age_seconds"),
        }

    for candidate in (
        _stamped_owner_agent_id(item),
        owner_bindings.get(iid),
    ):
        if not candidate:
            continue
        rec = agent_by_aid.get(candidate)
        if rec is not None:
            return {
                "agent_id": candidate,
                "alive": bool(rec.get("alive")),
                "age": rec.get("jsonl_age_seconds"),
            }
        return {"agent_id": candidate, "alive": None, "age": None}

    return None


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

    (
        agent_by_qid,
        agent_by_aid,
        agent_mtime,
        agent_state_status,
    ) = _load_agent_state_with_mtime()
    g_agent_state_mtime.set(agent_mtime)
    # Arm-hook owner bindings (queue_id -> agent_id). The earliest +
    # authoritative "this item is owned" signal; see AGENT_QUEUE_BINDINGS_PATH.
    owner_bindings, bindings_status = load_agent_queue_bindings_status(
        AGENT_QUEUE_BINDINGS_PATH
    )
    # True when at least one owner-attribution input is readable. With NO
    # readable input the never-spawned-orphan fallback is suppressed: an
    # absent input makes every running item look ownerless, and reporting
    # them all orphaned is worse than reporting nothing.
    have_owner_signal = _report_owner_inputs(agent_state_status, bindings_status)

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
    g_owner_unknown_age.clear()
    g_owner_unknown_count.set(0)

    # Seeded so every known status reports a 0 series rather than
    # disappearing. `quarantined` = abandoned on a guess that the agent
    # died, still holding its scope lock; it is NOT terminal, so a
    # long-lived one is something an operator should be able to alert on.
    status_counts = {
        "pending": 0, "running": 0, "wedged": 0, "blocked": 0,
        "quarantined": 0, "done": 0, "abandoned": 0,
    }
    priority_counts = {}
    group_counts = {}
    # Running items we could not attribute an owner to at all (see
    # g_owner_unknown_age). Summed across the loop and published as a
    # single count so an operator can alert on "any at all" without
    # per-series aggregation.
    owner_unknown_count = 0
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

            # Attribute an owner: active-agents record keyed on this qid,
            # then the register-time `agent_id` stamp, then the arm-hook
            # binding -- the same precedence queue-minisite's
            # `_classify_owner` uses, so the dashboard and the alert can
            # never disagree. Liveness always comes from active-agents'
            # `alive` flag (post-#690: already tolerant of an agent sitting
            # inside one long foreground tool call); never from a pid,
            # which subagents and container-spawned agents do not have.
            owner = _resolve_owner(
                iid, it, agent_by_qid, agent_by_aid, owner_bindings,
            )
            if owner is not None:
                aid = owner["agent_id"]
                # alive is None for a KNOWN owner whose liveness could not
                # be resolved (state lag / between transcript writes).
                # Presume alive: a named owner was definitively spawned, and
                # active-agents will publish alive=false honestly if it died.
                g_has_live_owner.labels(
                    id=iid, summary=summary, agent_id=aid, status=status,
                ).set(0 if owner["alive"] is False else 1)
                if owner["age"] is not None:
                    g_agent_jsonl_age.labels(
                        id=iid, summary=summary, agent_id=aid, status=status,
                    ).set(owner["age"])
            elif status == "running" and have_owner_signal:
                # OWNER UNKNOWN -- the item is running and nothing can say
                # who owns it. Distinct from ORPHANED, which means a KNOWN
                # owner is gone; here there is no owner to have lost. The
                # canonical producer is a queue entry meant for an agent
                # that never got one assigned, and the second is an agent
                # RESUMED onto a rotated qid whose owner stamp was never
                # retrofitted (`session-task queue assign` is the fix for
                # that one).
                #
                # Emitted with NO staleness precondition, deliberately. The
                # never-spawned-orphan branch just below requires a
                # heartbeat older than ORPHAN_HEARTBEAT_STALE_SECONDS, and
                # that gate is precisely why a live-but-ownerless item is
                # invisible today: an item heartbeating happily while
                # belonging to nobody never trips it, so nothing ever says
                # the owner is missing. Age here is measured from
                # registration, not from the last heartbeat, for the same
                # reason -- heartbeats say the work is alive, never who
                # owns it, and resetting the clock on each beat would
                # re-hide exactly the live-but-ownerless case.
                #
                # Exemptions mirror the queue-check cron's: an item whose
                # scope carries a `workload:` or `hostjob:` token is a
                # system job owned by a PROCESS in the tasks session, and
                # an item with an explicit `pid` stamp names its owning
                # process directly. Neither has an agent owner that could
                # be missing, so flagging them would be noise, not signal.
                exempt = (
                    _workload_label_from_scope(it.get("scope")) is not None
                    or _hostjob_label_from_scope(it.get("scope")) is not None
                    or it.get("pid") is not None
                )
                if not exempt and reg_ts is not None:
                    owner_unknown_count += 1
                    g_owner_unknown_age.labels(id=iid, summary=summary).set(
                        max(0.0, (now - reg_ts).total_seconds())
                    )

                # Never-spawned / abandoned-without-binding orphan -- a
                # `running` item whose Agent was never fired has NO agent
                # record AND no binding (vs died-after-spawn, which has a
                # record with alive=0 handled above). Without this branch
                # such an item emits no has_live_owner series, so the
                # WorkQueueOrphaned {status=running} == 0 alert matches
                # nothing and never fires. Fall back to heartbeat
                # staleness -- if the item has not heartbeat in
                # ORPHAN_HEARTBEAT_STALE_SECONDS, flag it orphaned with
                # agent_id empty (the empty agent_id distinguishes a
                # no-binding orphan from a died-after-spawn one). ONLY
                # `running` -- `blocked` items legitimately have no live
                # agent by design. A fresh or unparseable heartbeat stays
                # SILENT to preserve no-false-alert on a just-spawned item.
                # Gated on `have_owner_signal`: with BOTH owner inputs
                # unreadable (a container missing its bind mounts) EVERY
                # running item lands here, and flagging the whole queue
                # orphaned at once is a deployment fault masquerading as an
                # incident. In that state we stay silent and let
                # `worktask_queue_owner_input_available` carry the alarm.
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

    g_owner_unknown_count.set(owner_unknown_count)

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
    log.info("Build: commit=%s version=%s source=%s",
             EXPORTER_COMMIT, EXPORTER_VERSION, EXPORTER_SOURCE)
    collect()
    HTTPServer(("0.0.0.0", PORT), MetricsHandler).serve_forever()


if __name__ == "__main__":
    main()
