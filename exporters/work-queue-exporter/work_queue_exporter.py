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

Upstream-API death cost (rev 2026-09-03 — task-level retry cost):

  A queue item can burn several agent runs before it produces anything:
  the agent is spawned, the upstream API returns a capacity error
  (529 Overloaded, 500, 503, a mid-response server error), the run dies,
  the main loop respawns it with continuation context, and the cycle can
  repeat up to the respawn cap before the item is quarantined and finally
  abandoned. The wall-clock and agent-run cost of that is real and it is
  invisible in every existing series here: the item just reads `running`
  for a while and then flips to `abandoned`, indistinguishable from an
  item abandoned for any other reason.

  Process-level pressure sampling cannot supply this number either. It
  can see that a process is retrying, but "how many AGENT RUNS did this
  QUEUE ITEM lose" is inter-agent semantics — it lives in the work-queue
  layer, which is the only place that knows an agent run belonged to a
  task and that a later run continued the same task.

  The durable evidence is the per-item transcript archive: `session-task`
  copies the owning agent's JSONL to
  `<QUEUE_LOG_ARCHIVE_DIR>/<queue-id>.jsonl` when an item is finalized
  (done / abandoned), and stamps `log_archive_path` on the item. A run
  killed by an upstream API error leaves a machine-readable terminal
  record in that transcript: an assistant line with
  `isApiErrorMessage: true`, an `apiErrorStatus` (the HTTP status, when
  the client captured one) and an `error` class. Counting those lines per
  archive IS the per-item count of agent runs lost to the API.

  Three properties of this source are load-bearing and are NOT papered
  over anywhere below:

    1. It is POST-HOC, not live. An archive appears when the item is
       finalized, so a task currently burning runs is not yet visible
       here. This is a cost/accounting series, not an incident alert —
       alert on the retry-storm and stall series instead, and read these
       to answer "what did that storm cost us".
    2. MODEL ATTRIBUTION IS BEST-EFFORT. The error line's own `model`
       field reads `<synthetic>` — the message is composed client-side,
       not by a model — so the model must be recovered from a real
       assistant turn elsewhere in the same archive. One archive holds
       exactly one agent run, so any real model in it identifies the run.
       But a run that died on its FIRST turn never produced a real
       assistant message, and for that run the model is genuinely
       unrecoverable: it is labelled `model="unknown"` rather than
       guessed. That is the worst case for the question being asked (the
       hardest-hit runs are the least attributable), and pretending
       otherwise would be worse than saying so.
    3. RESPAWN CHAINS ARE NOT RECONSTRUCTABLE. When a quarantined item
       is released and re-queued under a NEW id, nothing structured links
       the two — the relationship survives only in free-text reasons. The
       queue's `resurrected_from` / `resurrected_as` fields are a
       different mechanism (recovering orphans after a restart) and are
       deliberately not reported as API respawns. The honest per-task
       cost number available here is deaths-per-item, and that is what is
       exported; no cross-item respawn chain is synthesised.

  The scan is bounded to archives modified within API_DEATH_WINDOW_DAYS
  and memoised on (mtime, size), so a scrape re-reads only archives that
  changed. Archives without the error marker are rejected on a substring
  test without being parsed.

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
  - worktask_queue_agent_api_deaths_total{status_code,error_class,model}
        counter (agent runs lost to an upstream API error, discovered in
        the per-item transcript archives. `status_code` is the HTTP status
        the client recorded, or `none` when it recorded none — a stream
        idle timeout and an over-long prompt both land there. `error_class`
        is `transient` for the capacity/availability statuses worth
        respawning into (408/429/500/502/503/504/529 and unclassified
        server errors) and `non_transient` otherwise. `model` is the model
        the dead run was using, or `unknown` when it died before emitting
        a real assistant turn.)
  - worktask_queue_item_api_deaths{id,summary,model} gauge (count of agent
        runs THIS item lost to API errors, within the scan window. The
        per-task cost number: an item that burned three runs reads 3.
        Emitted only for items with at least one death.)
  - worktask_queue_items_with_api_deaths     gauge (how many items in the
        window lost at least one run. Unlabelled, so it is always one
        series and cannot go absent — a 0 here is only evidence of a clean
        window when archives_scanned > 0 and input_available = 1.)
  - worktask_queue_api_death_archives_scanned gauge (archives considered
        in the window. This is the disambiguator: a wrong or unmounted
        QUEUE_LOG_ARCHIVE_DIR reads 0 here, while items_with_api_deaths
        reads an identical-looking 0 either way.)
  - worktask_queue_api_death_input_available gauge (1 when
        QUEUE_LOG_ARCHIVE_DIR is readable, 0 otherwise, with a loud log
        line on each transition. The per-ITEM death series go ABSENT while
        this reads 0; the two unlabelled scalars above cannot, so alert on
        this gauge rather than trusting their zeros.)
  - worktask_queue_item_quarantined_age_seconds{id,summary} gauge (seconds
        since `quarantined_at` for items currently quarantined — the
        containment state an item lands in when its agent runs kept dying.
        Not terminal: a quarantined item still holds its scope lock, so a
        long-lived one is worth alerting on. The count is already available
        as worktask_queue_items_total{status="quarantined"}.)
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
# Directory of per-item agent transcript archives. `session-task` copies the
# owning agent's JSONL to `<dir>/<queue-id>.jsonl` when an item is finalized
# and stamps `log_archive_path` on the item; `session-task queue rotate`
# prunes the directory by age and count, so the corpus is already bounded.
#
# The default is derived from QUEUE_JSON's own directory rather than
# hard-coded, because the archive dir is that file's SIBLING on the host
# (`~/.config/session/queue.json` next to `~/.config/session/queue-logs/`).
# Deriving it means any deployment that already bind-mounts the session dir
# to reach queue.json gets the archives for free, under whatever mount point
# it chose. Override explicitly when the two are mounted separately; the env
# var name matches the one queue-minisite already uses for the same dir.
QUEUE_LOG_ARCHIVE_DIR = os.environ.get("QUEUE_LOG_ARCHIVE_DIR") or os.path.join(
    os.path.dirname(os.path.abspath(QUEUE_PATH)), "queue-logs"
)
# How far back to scan archives for API deaths. Bounds the work AND defines
# the window the counters describe. 30d comfortably exceeds any dashboard
# range that would ask this question while keeping a warm scrape's scan to a
# substring test over files that have not changed since the last one.
API_DEATH_WINDOW_DAYS = int(os.environ.get("API_DEATH_WINDOW_DAYS", "30"))

# --- upstream-API error classification -----------------------------------
#
# Restated here rather than imported: the exporter is a standalone container
# that vendors only its own sources, and the same split is applied by the
# queue-side monitor that decides whether to respawn. Keeping the two in
# agreement matters more than sharing the literal, so the reasoning is
# written out instead of referenced.
#
# `transient` means "the upstream was busy or briefly broken, a respawn of
# the same work is reasonable". Those are the capacity and availability
# statuses. Anything else — a malformed request, an over-long prompt, an
# auth failure — would fail identically on respawn, so it is classed
# non-transient and counted separately: a task that lost runs to 529s is a
# capacity cost, while one that lost runs to an over-long prompt is a bug in
# how it was briefed, and averaging those together would hide both.
TRANSIENT_API_STATUS_CODES = frozenset(
    {"408", "429", "500", "502", "503", "504", "529"}
)
NON_TRANSIENT_API_STATUS_CODES = frozenset(
    {"400", "401", "403", "404", "413", "422"}
)
# When no status code was captured, fall back to the client's own error
# class. `server_error` (a mid-response stream failure, an idle timeout) is
# the same kind of upstream flakiness as a 5xx and respawns fine; an
# `invalid_request` (canonically "Prompt is too long") does not.
TRANSIENT_API_ERROR_CLASSES = frozenset({"server_error", "overloaded_error"})
# Substring that must appear in a raw archive for it to be worth parsing.
# Cheap reject for the overwhelming majority of archives, which record no
# API error at all.
#
# Deliberately the KEY only, not `"isApiErrorMessage":true`. JSON separator
# spacing is a writer's choice — the transcripts on disk are compact, but a
# tool that re-serialised one with `json.dump` defaults would emit
# `"isApiErrorMessage": true` and a value-inclusive marker would silently
# skip the file, reporting zero deaths for an archive full of them. The
# truthiness is re-checked per record after parsing, so the loose marker
# costs at most a few needless parses and cannot produce a false death.
API_ERROR_MARKER = b'"isApiErrorMessage"'
# The model field on an API-error line. The message is composed client-side
# rather than by a model, so this value identifies nothing and must never be
# reported as the dead run's model.
SYNTHETIC_MODEL = "<synthetic>"
MODEL_UNKNOWN = "unknown"

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
g_item_api_deaths = Gauge(
    "worktask_queue_item_api_deaths",
    (
        "Agent runs THIS queue item lost to an upstream API error, counted "
        "from its archived transcript within the scan window. The per-task "
        "cost number: an item whose agent was killed by 529 Overloaded "
        "three times before it was quarantined reads 3. Emitted only for "
        "items with at least one death, so the series set is small. "
        "`model` is the model the dead runs were using, or \"unknown\" when "
        "the run died before emitting a real assistant turn and the model "
        "is genuinely unrecoverable -- see the module docstring; it is "
        "never guessed. POST-HOC: an archive is written when the item is "
        "finalized, so an item currently burning runs is not here yet."
    ),
    ["id", "summary", "model"],
    registry=REG,
)
g_items_with_api_deaths = Gauge(
    "worktask_queue_items_with_api_deaths",
    (
        "How many queue items in the scan window lost at least one agent "
        "run to an upstream API error. Unlabelled, so this is always "
        "exactly one series and can never go absent -- which means a 0 is "
        "evidence of a clean window ONLY alongside "
        "worktask_queue_api_death_archives_scanned > 0 and "
        "worktask_queue_api_death_input_available == 1. Read on its own it "
        "cannot distinguish a quiet month from an unmounted directory."
    ),
    registry=REG,
)
g_api_death_archives_scanned = Gauge(
    "worktask_queue_api_death_archives_scanned",
    (
        "Number of per-item transcript archives considered in the current "
        "window. Exists so a zero death count can be told apart from a scan "
        "that saw no files at all -- a wrong QUEUE_LOG_ARCHIVE_DIR reads 0 "
        "here while every death series reads a plausible zero."
    ),
    registry=REG,
)
g_api_death_input_available = Gauge(
    "worktask_queue_api_death_input_available",
    (
        "1 when QUEUE_LOG_ARCHIVE_DIR is readable, 0 when it is missing or "
        "unreadable, with a loud log line naming the path on each "
        "transition. Same posture as worktask_queue_owner_input_available: "
        "a container that never got the bind mount would otherwise publish "
        "a confident, permanent \"nothing was ever lost\". The per-item "
        "death series go absent while this reads 0; the unlabelled scalars "
        "cannot, so ALERT ON THIS GAUGE rather than trusting their zeros."
    ),
    registry=REG,
)
g_quarantined_age = Gauge(
    "worktask_queue_item_quarantined_age_seconds",
    (
        "Seconds since `quarantined_at` for items currently in the "
        "`quarantined` state -- where an item lands when its agent runs "
        "kept dying and the main loop stopped respawning. Quarantine is "
        "NOT terminal and the item still holds its scope lock, so a "
        "long-lived one blocks its whole scope and is worth alerting on. "
        "The count is already available as "
        "worktask_queue_items_total{status=\"quarantined\"}; this adds the "
        "age and the identity."
    ),
    ["id", "summary"],
    registry=REG,
)
c_api_deaths = Counter(
    "worktask_queue_agent_api_deaths",
    (
        "Agent runs lost to an upstream API error, discovered in the "
        "per-item transcript archives. `status_code` is the HTTP status the "
        "client recorded, or \"none\" when it recorded none (a stream idle "
        "timeout, an over-long prompt). `error_class` is `transient` for "
        "the capacity/availability failures a respawn can reasonably "
        "retry, `non_transient` for the ones that would fail identically. "
        "`model` is the dead run's model, or \"unknown\" when unrecoverable. "
        "Counter semantics: the process counts each (item, timestamp) death "
        "once, and a restart re-counts the window from scratch -- normal "
        "counter-reset behaviour that rate()/increase() already handle."
    ),
    ["status_code", "error_class", "model"],
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


# --- upstream-API death accounting ---------------------------------------
#
# Memo of parsed archives, keyed by path -> (mtime, size, deaths). An archive
# is immutable once its item is finalized, but it can be REWRITTEN while the
# item is still churning (the 529 case appends another error line each time a
# run dies), so the memo is invalidated on either mtime or size changing
# rather than on existence alone.
_archive_cache = {}
# (queue_id, death timestamp) pairs already counted into c_api_deaths, so a
# re-parse of a grown archive re-counts only its NEW deaths. Same
# once-per-fact discipline as _seen_done_ids_by_creator above.
_seen_api_deaths = set()
# Last-reported readability of the archive dir, so the warning below is loud
# once per transition rather than once per scrape.
_api_death_input_last_status = [None]


def _classify_api_error(status_code, error_class):
    """Return `transient` / `non_transient` for one API error record.

    `transient` means the failure was upstream capacity or availability and
    respawning the same work is reasonable; `non_transient` means a respawn
    would fail the same way. The split is what makes the counter useful:
    runs lost to 529 Overloaded are a capacity cost worth pricing, runs lost
    to an over-long prompt are a briefing bug, and one number covering both
    would answer neither question.

    Status code decides when there is one. With no status code we fall back
    to the client's error class, and an UNRECOGNISED value is classed
    non_transient -- the conservative direction, since over-reporting
    capacity loss is the failure mode that would mislead the decision this
    metric exists to inform.
    """
    if status_code in TRANSIENT_API_STATUS_CODES:
        return "transient"
    if status_code in NON_TRANSIENT_API_STATUS_CODES:
        return "non_transient"
    if status_code == "none" and error_class in TRANSIENT_API_ERROR_CLASSES:
        return "transient"
    return "non_transient"


def _parse_archive_deaths(path):
    """Parse one transcript archive; return a list of death records.

    Each record is {timestamp, status_code, error_class, model}.

    Model attribution, which is the delicate part: the API-error line's own
    `model` reads `<synthetic>` because the message is composed client-side,
    so the real model has to come from an ordinary assistant turn in the
    same file. One archive holds exactly one agent run, so ANY real model in
    it identifies the run -- we prefer the nearest turn BEFORE the death and
    fall back to any real model in the file. When the run died before ever
    completing an assistant turn there is no such line and the model is
    genuinely unrecoverable; those deaths are labelled `unknown` rather than
    inferred from anything else, because every remaining signal (the item's
    text, the fleet's usual model) would be a guess dressed as a fact.
    """
    try:
        with open(path, "rb") as fh:
            raw = fh.read()
    except OSError:
        return []
    # Cheap reject: the overwhelming majority of archives record no API
    # error, and this avoids JSON-parsing megabytes of ordinary transcript.
    if API_ERROR_MARKER not in raw:
        return []

    deaths = []
    preceding_model = None
    any_model = None
    for line in raw.splitlines():
        if b'"model"' not in line:
            continue
        try:
            rec = json.loads(line)
        except (json.JSONDecodeError, UnicodeDecodeError):
            continue
        msg = rec.get("message")
        if not isinstance(msg, dict):
            continue
        model = msg.get("model")
        is_real_model = isinstance(model, str) and model and model != SYNTHETIC_MODEL
        if not rec.get("isApiErrorMessage"):
            if is_real_model:
                preceding_model = model
                if any_model is None:
                    any_model = model
            continue
        status = rec.get("apiErrorStatus")
        deaths.append({
            "timestamp": rec.get("timestamp") or "",
            "status_code": str(status) if status is not None else "none",
            "error_class": str(rec.get("error") or "unknown"),
            # Resolved after the pass so a death that precedes the only real
            # assistant turn still gets attributed to it.
            "model": preceding_model,
        })
        if is_real_model:
            preceding_model = model
            if any_model is None:
                any_model = model

    for d in deaths:
        d["model"] = d["model"] or any_model or MODEL_UNKNOWN
    return deaths


def _report_api_death_input(status):
    """Publish + log the readability of the archive directory.

    Returns True when the directory is readable. While it is NOT, every
    api-death series is suppressed by the caller: a container that never got
    the bind mount would otherwise publish a confident and permanent "no
    task has ever lost a run", which is exactly the false-healthy this
    exporter refuses to emit for owner attribution either.
    """
    g_api_death_input_available.set(1 if status else 0)
    if _api_death_input_last_status[0] == status:
        return status
    previous = _api_death_input_last_status[0]
    _api_death_input_last_status[0] = status
    if status:
        if previous is not None:
            log.info(
                "api-death input recovered (%s)", QUEUE_LOG_ARCHIVE_DIR,
            )
    else:
        log.warning(
            "API-DEATH INPUT UNREADABLE at %s (set QUEUE_LOG_ARCHIVE_DIR to "
            "fix). Agent runs lost to upstream API errors CANNOT be counted; "
            "the worktask_queue_*api_death* series are suppressed rather "
            "than reported as zero. Container deployments must bind-mount "
            "the session-task queue-logs directory.",
            QUEUE_LOG_ARCHIVE_DIR,
        )
    return status


def _scan_api_deaths(now_ts):
    """Scan the archive dir; return (deaths_by_qid, available, scanned).

    `deaths_by_qid` maps queue id -> list of death records. Bounded to
    archives modified within API_DEATH_WINDOW_DAYS and memoised on
    (mtime, size), so a warm scrape re-reads only archives that changed
    since the last one.
    """
    try:
        entries = list(os.scandir(QUEUE_LOG_ARCHIVE_DIR))
    except OSError:
        _report_api_death_input(False)
        return {}, False, 0
    _report_api_death_input(True)

    cutoff = now_ts - API_DEATH_WINDOW_DAYS * 86400
    deaths_by_qid = {}
    scanned = 0
    live_paths = set()
    for entry in entries:
        name = entry.name
        if not name.endswith(".jsonl"):
            continue
        try:
            st = entry.stat()
        except OSError:
            continue
        if st.st_mtime < cutoff:
            continue
        scanned += 1
        path = entry.path
        live_paths.add(path)
        key = (st.st_mtime, st.st_size)
        cached = _archive_cache.get(path)
        if cached is not None and cached[0] == key:
            deaths = cached[1]
        else:
            deaths = _parse_archive_deaths(path)
            _archive_cache[path] = (key, deaths)
        if deaths:
            deaths_by_qid[name[: -len(".jsonl")]] = deaths

    # Drop memo entries for archives that rotated out of the window, so the
    # cache tracks the corpus instead of growing for the process's lifetime.
    for stale in set(_archive_cache) - live_paths:
        del _archive_cache[stale]

    return deaths_by_qid, True, scanned


def _publish_api_deaths(summaries, now_ts):
    """Refresh every api-death series from a fresh archive scan."""
    deaths_by_qid, available, scanned = _scan_api_deaths(now_ts)

    # The per-ITEM gauge is the one that can genuinely go absent, and it
    # does: with no readable archive dir there is no item to name, so the
    # series set is empty rather than a set of confident zeros.
    g_item_api_deaths.clear()
    # The two scalars are unlabelled, so they cannot be withdrawn from the
    # registry the way a labelled family can -- an unlabelled Gauge is one
    # permanent series by construction. Rather than reach into
    # prometheus_client internals to fake an absence, they read 0 and
    # `worktask_queue_api_death_archives_scanned` carries the disambiguation
    # in the open: 0 archives scanned is the visible difference between "a
    # clean window" and "the exporter cannot see the archives at all", and
    # `worktask_queue_api_death_input_available` says which.
    g_api_death_archives_scanned.set(scanned)
    g_items_with_api_deaths.set(len(deaths_by_qid))
    if not available:
        return

    for qid, deaths in deaths_by_qid.items():
        # One model per archive by construction (one archive = one agent
        # run), but attribute defensively: if a file somehow carries deaths
        # under more than one model, the item gauge reports the one the most
        # deaths landed under rather than silently dropping a series.
        model_counts = {}
        for d in deaths:
            model_counts[d["model"]] = model_counts.get(d["model"], 0) + 1
            fact = (qid, d["timestamp"], d["status_code"])
            if fact in _seen_api_deaths:
                continue
            _seen_api_deaths.add(fact)
            c_api_deaths.labels(
                status_code=d["status_code"],
                error_class=_classify_api_error(
                    d["status_code"], d["error_class"]
                ),
                model=d["model"],
            ).inc()
        item_model = max(model_counts.items(), key=lambda kv: kv[1])[0]
        g_item_api_deaths.labels(
            id=qid,
            summary=summaries.get(qid, "(no summary)"),
            model=item_model,
        ).set(len(deaths))


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
    g_quarantined_age.clear()
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
    # queue_id -> summary, so the api-death gauges below can name an item
    # without a second pass over `items`. Built for every status: an item
    # that burned agent runs is typically abandoned by the time its archive
    # exists, so restricting this to live items would leave exactly the
    # interesting rows unnamed.
    summaries = {}

    for it in items:
        status = it.get("status", "unknown")
        iid_any = it.get("id")
        if iid_any:
            summaries[iid_any] = (it.get("summary") or "")[:80] or "(no summary)"
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

        # Quarantine age. `quarantined` is where an item lands when its
        # agent runs kept dying and the main loop stopped respawning --
        # the containment state at the end of the retry-cost story the
        # api-death series price. It is NOT terminal and the item still
        # holds its scope lock, so a forgotten one silently parks every
        # other task in that scope; the age is what makes that alertable.
        if status == "quarantined":
            q_ts = _parse_ts(it.get("quarantined_at"))
            if q_ts:
                g_quarantined_age.labels(
                    id=it.get("id", ""),
                    summary=(it.get("summary") or "")[:80] or "(no summary)",
                ).set(max(0.0, (now - q_ts).total_seconds()))

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

    # Agent runs lost to upstream API errors, read from the per-item
    # transcript archives. Deliberately last: it is the only input outside
    # queue.json + agent state, and an unreadable archive dir must degrade
    # exactly one family of series rather than cost the scrape its
    # queue-shaped ones.
    _publish_api_deaths(summaries, time.time())

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
    log.info("API-death scan: archives=%s window=%dd",
             QUEUE_LOG_ARCHIVE_DIR, API_DEATH_WINDOW_DAYS)
    log.info("Build: commit=%s version=%s source=%s",
             EXPORTER_COMMIT, EXPORTER_VERSION, EXPORTER_SOURCE)
    collect()
    HTTPServer(("0.0.0.0", PORT), MetricsHandler).serve_forever()


if __name__ == "__main__":
    main()
