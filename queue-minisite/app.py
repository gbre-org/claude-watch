"""queue-minisite — session-task work-queue viewer + stop button.

Auth is expected to be handled UPSTREAM (e.g. oauth2-proxy / nginx
``auth_request``). This container trusts ``X-Auth-Request-Email`` for
display purposes only — the upstream proxy is responsible for enforcing
any access control before requests reach us. The ``location /`` block in
the reverse proxy covers EVERY route below it, including
``/api/queue/stop`` — there is no per-route auth in this app.

Read path: ``GET /api/queue`` (and ``GET /``) read ``queue.json`` from a
host bind mount (``$QUEUE_JSON``, default ``/queue/queue.json``).

Write paths (both shell out to a vendored copy of
``session-task queue abandon <id>``):
  - ``POST /api/queue/stop``    — Stop button on RUNNING items
    (added 2026-05-01)
  - ``POST /api/queue/abandon`` — Abandon button on PENDING items
    (added 2026-05-01)

Both endpoints differ only in which current statuses they accept; the
underlying queue.json mutation, group-head recompute, completed-tasks
log, and ``queue-abandoned`` claude-event emit are byte-identical to
the host-side CLI's behavior. The vendored binary mutates the SAME
queue.json (now mounted rw under ``$HOME/.config/session/``) and emits
a ``queue-abandoned`` claude-event, exactly as the host-side CLI would.
The abandoned status flips ``group_head=False`` and (for running items)
triggers the obligation-gate path: the running agent's next non-exempt
tool call sees its queue id is no longer ``running`` and is denied —
that's the "kill" mechanism. We do NOT call ``claude-watch agent kill``
because (a) the Rust binary is glibc and the container is alpine, and
(b) ``agent kill`` only works while a child process is actually
running, which is racy and unreliable. Abandon-only is the documented
mechanism. For pending items there's no agent yet — abandoning simply
removes the item before any work begins.

Owner-liveness logic mirrors the work-queue-exporter at
``monitoring/work-queue-exporter/work_queue_exporter.py`` — keep them in
sync if the queue.json schema evolves. Source of truth (post-2026-05-01-v3):
``claude-watch active-agents`` JSON state file (``AGENT_STATE_JSON``).
For each running queue item we look up the agent record by ``queue_id``
parsed from the agent's first user message and use its ``alive`` flag.

Cached for ``CACHE_TTL_SECONDS`` (default 5s) so the front-end's 5s
auto-refresh doesn't re-stat the queue file every tick.
"""

from __future__ import annotations

import json
import os
import re
import subprocess
import time
from dataclasses import dataclass, field
from datetime import datetime, timezone
from pathlib import Path
from typing import Any, Iterator

from flask import Flask, Response, jsonify, render_template, request, stream_with_context

# Shared loader / dedup logic — see claude_agents.py alongside this file.
from claude_agents import agents_by_queue_id as _agents_by_qid
from claude_agents import load_agent_state as _load_state

QUEUE_PATH = os.environ.get("QUEUE_JSON", "/queue/queue.json")
AGENT_STATE_PATH = os.environ.get(
    "AGENT_STATE_JSON", "/agents-state/active-agents.json"
)
# Root of `~/.claude/projects/` (mounted read-only so we can tail the
# active subagent JSONL transcripts for the live-log view). Each session
# directory contains a `subagents/` subdir with `agent-<id>.jsonl` files.
AGENTS_JSONL_ROOT = os.environ.get(
    "AGENTS_JSONL_ROOT", "/agents-jsonl"
)
# Persistent archive directory for spawning-subagent transcripts. The
# vendored session-task `_archive_agent_transcript` copies the active
# JSONL into this directory at queue-done / queue-abandon time and
# stamps `log_archive_path` (the relative filename) on the queue item.
# We read those archives back via `GET /api/queue/<id>/archive` to
# render the same live-log modal in archive mode for historical
# entries. Default mirrors the host path inside the container's HOME.
QUEUE_LOG_ARCHIVE_DIR = os.environ.get(
    "QUEUE_LOG_ARCHIVE_DIR", "/queue-home/.config/session/queue-logs"
)
# Workload output directory (tail target for workload-bound queue items).
# `workload run <label> -- <cmd>` writes:
#   /tmp/claude-workloads/<label>.output  (line-oriented stdout/stderr)
#   /tmp/claude-workloads/<label>.exit    (created on workload exit)
# The host path is /tmp/claude-workloads; we bind-mount that dir read-only
# at /workloads inside the container. Override via WORKLOAD_LOG_DIR.
WORKLOAD_LOG_DIR = os.environ.get("WORKLOAD_LOG_DIR", "/workloads")
# Label format: same as queue-id-ish — letters, digits, dots, dashes,
# underscores. Path-traversal guard for the tail endpoint.
_WORKLOAD_LABEL_RE = re.compile(r"^[A-Za-z0-9._-]{1,128}$")
CACHE_TTL_SECONDS = float(os.environ.get("CACHE_TTL_SECONDS", "5"))
# Cap on the recent done / abandoned tails — full list is hundreds of items.
RECENT_DONE_LIMIT = int(os.environ.get("RECENT_DONE_LIMIT", "30"))
RECENT_ABANDONED_LIMIT = int(os.environ.get("RECENT_ABANDONED_LIMIT", "20"))

# "Starting" window — a queue item is RUNNING per session-task once
# `queue register` lands, but the harness may not have spawned the
# Agent tool yet, OR the agent may have spawned but not yet written
# its first JSONL event that claude-watch's active-agents poller can
# see. During this window we render a dedicated STARTING pill so the
# zero-content state is visually distinct from a steady-state RUNNING
# item. Heuristic: status==running AND no agent record in
# active-agents.json AND registered_at is within this many seconds.
# Beyond the window the item is treated as a regular running item with
# `owner unknown` (i.e. the existing orphan-ish path) — that's the
# bug-detection signal we don't want to swallow.
STARTING_WINDOW_SECONDS = float(os.environ.get("STARTING_WINDOW_SECONDS", "60"))

# Path to the vendored session-task script inside the container. Same
# Python-stdlib-only implementation as ~/repos/config/session-task on the
# host; copied in at Docker build time. See Dockerfile.
SESSION_TASK_BIN = os.environ.get("SESSION_TASK_BIN", "/app/session-task")
# Stop endpoint timeout — the abandon op is local file I/O + a single
# claude-event emit, well under a second in practice.
STOP_TIMEOUT_SECONDS = float(os.environ.get("STOP_TIMEOUT_SECONDS", "10"))
# Queue id format: "q-" followed by lowercase hex/digit/dash chunks.
# Mirrors the canonical format used by session-task. Strict pattern keeps
# subprocess invocation safe even if upstream gating ever drifts.
_QUEUE_ID_RE = re.compile(r"^q-[a-z0-9-]{4,64}$")
# Reason length cap — the value is stored verbatim in queue.json; no
# need to allow paragraphs.
_MAX_REASON_LEN = 500

# Whitelabel branding hooks. All values are read from the environment
# with empty / generic defaults so the public open-source build renders
# cleanly without any private branding bleeding through. A deployer
# overrides them via an env_file mounted on the container.
#
#   QUEUE_SITE_TITLE        — <title> + header label. Default: "queue".
#   QUEUE_SITE_LOGO_URL     — header logo image URL. May be an absolute
#                             URL or a relative path served by this app
#                             (anything under /static/). Empty = no logo
#                             rendered, unless QUEUE_SITE_LOGO_DEFAULT=1
#                             is set in which case the bundled
#                             claude-watch eye glyph at
#                             /static/claude-watch-logo.png is used.
#   QUEUE_SITE_BRAND        — short brand string rendered in the
#                             footer. Empty = no brand text.
#   QUEUE_SITE_FAVICON_URL  — favicon override. Empty falls back to
#                             the bundled generic favicon.
SITE_TITLE = os.environ.get("QUEUE_SITE_TITLE", "queue").strip() or "queue"
SITE_LOGO_URL = os.environ.get("QUEUE_SITE_LOGO_URL", "").strip()
SITE_BRAND = os.environ.get("QUEUE_SITE_BRAND", "").strip()
SITE_FAVICON_URL = os.environ.get("QUEUE_SITE_FAVICON_URL", "").strip()
SITE_LOGO_DEFAULT = os.environ.get("QUEUE_SITE_LOGO_DEFAULT", "").strip() in (
    "1",
    "true",
    "yes",
)

app = Flask(__name__)


# ---------------------------------------------------------------------------
# Static-asset cache-busting
# ---------------------------------------------------------------------------
# The reverse proxy passes upstream Cache-Control through, and we serve
# ``Cache-Control: no-cache`` for /static/* (Flask default for ``send_file``
# with ``cache_timeout=0``). That's fine for a cold reload — the browser
# revalidates via If-None-Match. BUT the modal's live-log EventSource holds
# a reference to the live-log.js renderer in memory across deploys: a tab
# opened before a JS update keeps running the OLD code, even after the
# container restarts and EventSource reconnects. q-2026-05-13-d57b
# (PR #133 follow-up) caught this — after deploying \r-transient render
# support the modal STILL stacked rsync progress lines for any tab opened
# pre-deploy because that tab's JS didn't know about the new ``transient``
# field. Hard-refresh worked, but that's a manual step we shouldn't need.
#
# Fix: append ``?v=<mtime>`` to every ``url_for('static', ...)`` call so a
# new-mtime deploy produces a new URL the browser MUST refetch. Mtime is
# stat'd lazily and cached forever per-path within a process — a deploy
# restarts the container so the cache is wiped naturally. Missing files
# (typo / wrong filename) silently fall through with no version param so
# the normal Flask 404 still surfaces.
_STATIC_MTIME_CACHE: dict[str, str] = {}


def _static_mtime_version(filename: str) -> str:
    """Return a short version token derived from the file's mtime.

    Empty string on stat failure so the caller renders the bare URL.
    """
    cached = _STATIC_MTIME_CACHE.get(filename)
    if cached is not None:
        return cached
    try:
        # ``app.static_folder`` is set by Flask to ``<app_root>/static`` by
        # default; resolve filename against it so we can't escape via "..".
        # ``send_static_file`` does the same safe-join — we mirror it here.
        full = os.path.join(app.static_folder or "", filename)
        # Reject path-traversal: the resolved path must live under the
        # static folder.
        full_real = os.path.realpath(full)
        root_real = os.path.realpath(app.static_folder or "")
        if not full_real.startswith(root_real + os.sep) and full_real != root_real:
            _STATIC_MTIME_CACHE[filename] = ""
            return ""
        mtime = int(os.path.getmtime(full_real))
        version = format(mtime, "x")
    except OSError:
        version = ""
    _STATIC_MTIME_CACHE[filename] = version
    return version


@app.url_defaults
def _add_static_version(endpoint: str, values: dict[str, Any]) -> None:
    """Inject ``v=<mtime>`` into every ``url_for('static', ...)`` call."""
    if endpoint != "static":
        return
    filename = values.get("filename")
    if not filename or "v" in values:
        return
    version = _static_mtime_version(filename)
    if version:
        values["v"] = version


@dataclass
class _Cache:
    payload: dict[str, Any] = field(default_factory=dict)
    fetched_at: float = 0.0
    error: str | None = None


_cache = _Cache()


def _empty_queue() -> dict[str, Any]:
    """Canonical empty queue.json skeleton.

    Mirrors `_queue_empty()` in tools/session-task/session-task so the
    front-end and downstream readers (`_render_payload`, `_classify_owner`,
    etc.) see the same shape as a freshly-written queue with no items.

    `schema_version` is intentionally omitted here — it's only meaningful
    on a real on-disk queue. Callers that need it read via
    `data.get("schema_version")`, which returns None for the empty case.
    """
    return {"items": [], "locked_scopes": {}}


def _read_queue() -> tuple[dict[str, Any], str | None]:
    """Read queue.json from the bind mount. Returns (data, error).

    A missing queue.json (ENOENT) is treated as an empty queue — this is
    the legitimate fresh-install / fresh-laptop state, not an error. The
    front-end renders an empty list rather than a red error banner.
    Other OSErrors (permission denied, I/O errors) still surface as errors.
    """
    try:
        with open(QUEUE_PATH, "r") as f:
            data = json.load(f)
        if not isinstance(data, dict):
            return {}, f"unexpected queue.json shape: {type(data).__name__}"
        return data, None
    except FileNotFoundError:
        return _empty_queue(), None
    except OSError as exc:
        return {}, f"queue.json unreadable: {exc}"
    except ValueError as exc:
        return {}, f"queue.json non-JSON: {exc}"


def _cached_queue() -> tuple[dict[str, Any], str | None]:
    now = time.time()
    if now - _cache.fetched_at < CACHE_TTL_SECONDS and _cache.fetched_at > 0:
        return _cache.payload, _cache.error
    data, err = _read_queue()
    if err is None:
        _cache.payload = data
    _cache.error = err
    _cache.fetched_at = now
    return _cache.payload, _cache.error


def _parse_iso(value: str | None) -> datetime | None:
    if not value:
        return None
    try:
        return datetime.fromisoformat(value.replace("Z", "+00:00")).astimezone(
            timezone.utc
        )
    except (ValueError, TypeError):
        return None


def _humanize_age(seconds: float | None) -> str:
    if seconds is None:
        return "?"
    secs = int(seconds)
    if secs < 0:
        secs = abs(secs)
        suffix = "from now"
    else:
        suffix = "ago"
    if secs < 60:
        return f"{secs}s {suffix}"
    if secs < 3600:
        return f"{secs // 60}m {suffix}"
    if secs < 86400:
        h, m = divmod(secs, 3600)
        return f"{h}h {m // 60}m {suffix}"
    d, rem = divmod(secs, 86400)
    return f"{d}d {rem // 3600}h {suffix}"


def _load_agent_state() -> dict[str, dict[str, Any]]:
    """Map queue_id -> agent record from claude-watch's active-agents JSON.

    Thin wrapper over the shared `claude_agents` helpers — kept as a
    one-liner so the call site reads naturally. See `claude_agents.py`
    alongside this module for the implementation.
    """
    return _agents_by_qid(_load_state(AGENT_STATE_PATH))


def _classify_owner(
    item: dict[str, Any],
    now: datetime,
    agent_by_qid: dict[str, dict[str, Any]],
) -> dict[str, Any]:
    """Compute owner liveness for a running item — mirrors work-queue-exporter.

    Source of truth (post-2026-05-01-v3): claude-watch active-agents JSON.
    Joined on `queue_id` parsed from each agent's first user message.

    Returns dict with keys:
      mode: 'agent' | 'unknown'
      alive: bool | None  (None when we have no agent record)
      agent_id: str  (empty when no record)
      jsonl_age_seconds: float | None
      jsonl_age: humanized string
      is_starting: bool — True when status==running but the agent has
        not yet appeared in the active-agents map AND `registered_at`
        is within STARTING_WINDOW_SECONDS. This is the gap between
        `session-task queue register` flipping the row to running and
        the spawned Agent's first JSONL write being picked up by
        claude-watch's poller. After the window expires the item is
        no longer "starting" — it's a regular running item with no
        owner, which surfaces as the existing "owner unknown" path
        (a real anomaly worth showing).
    """
    iid = item.get("id", "")
    agent = agent_by_qid.get(iid)
    if agent is not None:
        age = agent.get("jsonl_age_seconds")
        return {
            "mode": "agent",
            "alive": bool(agent.get("alive")),
            "agent_id": agent.get("agent_id", ""),
            "jsonl_age_seconds": age,
            "jsonl_age": _humanize_age(age),
            "is_starting": False,
        }

    # No agent record yet — could be (a) spawn race (STARTING) or
    # (b) genuinely orphaned / agent-state-stale (owner unknown).
    # Disambiguate via registered_at recency.
    registered = _parse_iso(
        item.get("registered_at") or item.get("started_at")
    )
    is_starting = False
    if registered is not None:
        age_since_register = (now - registered).total_seconds()
        if 0 <= age_since_register <= STARTING_WINDOW_SECONDS:
            is_starting = True
    return {
        "mode": "unknown",
        "alive": None,
        "agent_id": "",
        "jsonl_age_seconds": None,
        "jsonl_age": "?",
        "is_starting": is_starting,
    }


def _has_dep_cycle(items: list[dict[str, Any]], root_id: str) -> bool:
    """Detect a dependency cycle reachable from ``root_id``.

    Mirrors session-task's ``_has_dep_cycle`` (iterative DFS, lazy at read
    time). Used by ``_compute_ready_now`` so the SPA's READY badge flips
    False the moment a cycle is reachable, matching the dispatcher's
    spawn-gate behavior.
    """
    by_id = {it.get("id"): it for it in items if isinstance(it.get("id"), str)}
    visited: set[str] = set()
    on_path: set[str] = set()
    walk: list[tuple[str, Any]] = [
        (root_id, iter((by_id.get(root_id, {}).get("depends_on") or [])))
    ]
    on_path.add(root_id)
    while walk:
        _node, child_iter = walk[-1]
        try:
            child = next(child_iter)
        except StopIteration:
            popped, _ = walk.pop()
            on_path.discard(popped)
            visited.add(popped)
            continue
        if not isinstance(child, str):
            continue
        if child in on_path:
            return True
        if child in visited:
            continue
        child_item = by_id.get(child)
        if child_item is None:
            visited.add(child)
            continue
        on_path.add(child)
        walk.append((child, iter(child_item.get("depends_on") or [])))
    return False


def _compute_ready_now(items: list[dict[str, Any]], item: dict[str, Any]) -> bool:
    """Backend-authoritative ``ready_now`` for a queue item.

    Mirrors ``_item_is_ready`` in
    ``~/repos/claude-watch/tools/session-task/session-task`` byte-for-byte.
    Kept in lockstep so the SPA's READY badge agrees with the dispatcher's
    spawn-gate decision (Bug q-1b89: SPA was using FIFO-only ``group_head``
    which ignored ``depends_on``).

    Predicate:
      1. status == "pending"
      2. item is the head of its serialization group (oldest pending in
         the group, no running peers)
      3. every entry in ``depends_on`` resolves to an item with
         status == "done"; missing/abandoned/cycle = permanent block
    """
    if item.get("status") != "pending":
        return False
    group_id = item.get("group_id")
    members = [
        it
        for it in items
        if it.get("group_id") == group_id
        and it.get("status") in ("pending", "running")
    ]
    if not members:
        return False
    if any(m.get("status") == "running" for m in members):
        return False

    # Group head: oldest pending by (priority desc, created_at asc).
    def _sort_key(m: dict[str, Any]) -> tuple[int, str]:
        try:
            prio = -int(m.get("priority", 5))
        except (TypeError, ValueError):
            prio = -5
        return (prio, str(m.get("created_at", "")))

    pending_members = [m for m in members if m.get("status") == "pending"]
    if not pending_members:
        return False
    head = sorted(pending_members, key=_sort_key)[0]
    if head.get("id") != item.get("id"):
        return False

    # Explicit dep edges.
    deps = item.get("depends_on") or []
    if deps:
        if _has_dep_cycle(items, item.get("id", "")):
            return False
        items_by_id = {
            it.get("id"): it for it in items if isinstance(it.get("id"), str)
        }
        for dep_id in deps:
            dep = items_by_id.get(dep_id)
            if dep is None:
                return False
            if dep.get("status") != "done":
                return False
    return True


def _shape(
    item: dict[str, Any],
    now: datetime,
    agent_by_qid: dict[str, dict[str, Any]],
    items: list[dict[str, Any]] | None = None,
) -> dict[str, Any]:
    status = item.get("status", "unknown")
    created = _parse_iso(item.get("created_at"))
    started = _parse_iso(item.get("registered_at") or item.get("started_at"))
    completed = _parse_iso(item.get("completed_at"))
    abandoned = _parse_iso(item.get("abandoned_at"))

    blocked_at = _parse_iso(item.get("blocked_at"))

    # Pick the most-relevant "age anchor" for the visible age string.
    if status == "running" and started:
        age_anchor = started
        age_label = "running"
    elif status == "blocked" and blocked_at:
        age_anchor = blocked_at
        age_label = "blocked"
    elif status == "pending" and created:
        age_anchor = created
        age_label = "pending"
    elif status == "done" and completed:
        age_anchor = completed
        age_label = "done"
    elif status == "abandoned" and abandoned:
        age_anchor = abandoned
        age_label = "abandoned"
    elif created:
        age_anchor = created
        age_label = "created"
    else:
        age_anchor = None
        age_label = ""

    age_secs = (now - age_anchor).total_seconds() if age_anchor else None

    summary = item.get("summary") or item.get("description") or "(no summary)"
    summary = summary[:200]

    # depends_on — list of queue ids this item is blocked on. Field
    # does not yet exist in queue.json (model side pending); surface as
    # an empty list so the front-end's depends-on badge renders cleanly
    # the moment the field lands.
    raw_deps = item.get("depends_on") or []
    if isinstance(raw_deps, list):
        depends_on = [d for d in raw_deps if isinstance(d, str)]
    else:
        depends_on = []

    # depends_on_status — per-edge resolution so the SPA can render
    # the dep chip with its target's current state ("done" deps fade,
    # "running" deps highlight, "missing" deps red-flag the chain).
    # ``items`` may be None during legacy callers that haven't been
    # ported to pass the full list — we degrade gracefully to bare ids.
    depends_on_status: list[dict[str, str]] = []
    if items is not None and depends_on:
        items_by_id = {
            it.get("id"): it for it in items if isinstance(it.get("id"), str)
        }
        for dep_id in depends_on:
            target = items_by_id.get(dep_id)
            depends_on_status.append(
                {
                    "id": dep_id,
                    "status": target.get("status", "missing")
                    if isinstance(target, dict)
                    else "missing",
                }
            )

    # Archive presence: the queue.json field `log_archive_path` is the
    # relative filename inside QUEUE_LOG_ARCHIVE_DIR. Two shapes today:
    #   * ``q-XXX.jsonl``         — subagent transcript (agent items)
    #   * ``q-XXX.workload.txt``  — workload stdout (workload items)
    # Surface it to the front-end as a boolean ``has_archive`` so the
    # View-log button can render conditionally without the template
    # needing to know paths. The actual fetch goes through
    # ``GET /api/queue/<id>/archive`` — we never expose the raw
    # filesystem path.
    raw_archive = item.get("log_archive_path")
    has_archive = False
    if isinstance(raw_archive, str) and (
        raw_archive.endswith(".jsonl") or raw_archive.endswith(".workload.txt")
    ):
        archive_path = os.path.join(QUEUE_LOG_ARCHIVE_DIR, raw_archive)
        try:
            has_archive = os.path.isfile(archive_path)
        except OSError:
            has_archive = False

    shaped = {
        "id": item.get("id", ""),
        "summary": summary,
        # Full agent prompt — passed verbatim to `session-task queue add`
        # as the first positional. Surfaced on every card (collapsed by
        # default) so the "what was this agent told to do?" answer is
        # always one click away regardless of state.
        "description": item.get("description", "") or "",
        "scope": item.get("scope") or [],
        "group_id": item.get("group_id", ""),
        "group_head": bool(item.get("group_head")),
        # Backend-authoritative: True when this item is the head of its
        # group AND every depends_on edge resolves to a `done` item AND
        # no peer is running. Computed here (not read off queue.json)
        # because session-task only persists `group_head` — the
        # dependency check is read-time / lazy.
        "ready_now": _compute_ready_now(items, item) if items is not None else False,
        "status": status,
        "priority": item.get("priority", ""),
        "created_by": item.get("created_by", ""),
        "abandon_reason": item.get("abandon_reason", ""),
        "block_reason": item.get("block_reason", ""),
        "depends_on": depends_on,
        "depends_on_status": depends_on_status,
        "created_at_iso": item.get("created_at", ""),
        "started_at_iso": (item.get("registered_at") or item.get("started_at") or ""),
        "completed_at_iso": item.get("completed_at", ""),
        "abandoned_at_iso": item.get("abandoned_at", ""),
        "blocked_at_iso": item.get("blocked_at", ""),
        "age": _humanize_age(age_secs),
        "age_label": age_label,
        "age_seconds": age_secs,
        "has_archive": has_archive,
    }

    if status == "running":
        owner = _classify_owner(item, now, agent_by_qid)
        shaped["owner"] = owner
        # Surface STARTING as a top-level boolean for the template +
        # API consumers. STARTING items count as running for the
        # totals (queue.json says they're running), but render with
        # a distinct pill + suppress orphan badging during the window.
        shaped["is_starting"] = bool(owner.get("is_starting"))
    else:
        shaped["is_starting"] = False
    # Workload label — derived from scope. Items created by `workload run`
    # have a scope entry of the form `workload:<label>`. Surface the bare
    # label so the front-end can render `data-log-mode="workload"` on
    # running items (the live-log endpoint dispatches on this).
    shaped["workload_label"] = _extract_workload_label(shaped["scope"])
    return shaped


def _extract_workload_label(scope: list[Any]) -> str:
    """Return the workload label encoded in a queue item's scope, or "".

    `workload run <label>` creates a queue item with scope
    `["workload:<label>"]`. We only honor the first match — items aren't
    expected to be bound to more than one workload, and a multi-binding
    would be a bug elsewhere we don't want to silently paper over here.
    """
    if not isinstance(scope, list):
        return ""
    for s in scope:
        if isinstance(s, str) and s.startswith("workload:"):
            label = s[len("workload:") :]
            if _WORKLOAD_LABEL_RE.match(label):
                return label
    return ""


def _load_workload_script_capture(label: str) -> dict[str, Any] | None:
    """Return the workload's captured script content, or ``None``.

    The ``workload run`` CLI writes
    ``/tmp/claude-workloads/<label>.script.json`` at workload-START time
    when the command parses as ``<interpreter> <path>`` for a known
    scripting interpreter (bash/sh/python/ruby/node/perl/...). Capture-
    at-start is robust against later edits / deletes of the underlying
    script — the modal would otherwise show stale or empty content.

    Returns the parsed JSON dict, or ``None`` when:

      * the label fails the label-regex (path-traversal guard)
      * the sidecar file doesn't exist (older workloads, non-script
        commands, capture was refused for safety)
      * the file can't be parsed as JSON
      * the parsed shape isn't the expected dict

    Fail-soft — any error path returns ``None`` so the modal falls back
    to its existing behaviour (no "Script contents" row).
    """
    if not _WORKLOAD_LABEL_RE.match(label):
        return None
    path = Path(WORKLOAD_LOG_DIR) / f"{label}.script.json"
    if not path.is_file():
        return None
    try:
        with open(path, "r", encoding="utf-8", errors="replace") as f:
            data = json.load(f)
    except (OSError, ValueError):
        return None
    if not isinstance(data, dict):
        return None
    # Defensive shape check — we don't want a malformed sidecar to
    # crash the meta endpoint or the frontend. Only require the
    # always-present fields; optional/nullable fields are passed
    # through as-is.
    if not isinstance(data.get("path"), str):
        return None
    if not isinstance(data.get("interpreter"), str):
        return None
    if not isinstance(data.get("sha256"), str):
        return None
    return data


def _render_payload() -> dict[str, Any]:
    data, err = _cached_queue()
    items = data.get("items", []) if isinstance(data, dict) else []
    now = datetime.now(timezone.utc)
    agent_by_qid = _load_agent_state()

    running, pending, blocked, done, abandoned = [], [], [], [], []
    for it in items:
        # Pass the full items list so _shape can compute ready_now
        # (depends_on resolution requires the full graph) and decorate
        # depends_on_status per-edge.
        s = _shape(it, now, agent_by_qid, items=items)
        st = s["status"]
        if st == "running":
            running.append(s)
        elif st == "blocked":
            blocked.append(s)
        elif st == "pending":
            pending.append(s)
        elif st == "done":
            done.append(s)
        elif st == "abandoned":
            abandoned.append(s)

    # Order:
    #   running   — oldest-running first (most concerning)
    #   pending   — group-heads first, then by priority asc, then by age desc
    #   done      — most-recently-completed first
    #   abandoned — most-recently-abandoned first
    running.sort(key=lambda a: (-(a["age_seconds"] or 0)))
    # Blocked order: oldest-blocked first (longest-waiting external blocker
    # is the most likely to need operator attention).
    blocked.sort(key=lambda a: (-(a["age_seconds"] or 0)))
    # Pending order:
    #   1. ready_now=True items first (operator can spawn now)
    #   2. then non-ready group-heads (FIFO leader, blocked by deps)
    #   3. then everything else
    #   4. tie-break by priority asc, then age desc
    pending.sort(
        key=lambda a: (
            0 if a["ready_now"] else (1 if a["group_head"] else 2),
            int(a["priority"]) if str(a["priority"]).isdigit() else 99,
            -(a["age_seconds"] or 0),
        )
    )
    done.sort(key=lambda a: a.get("completed_at_iso") or "", reverse=True)
    abandoned.sort(key=lambda a: a.get("abandoned_at_iso") or "", reverse=True)

    done_recent = done[:RECENT_DONE_LIMIT]
    abandoned_recent = abandoned[:RECENT_ABANDONED_LIMIT]

    # Orphan tally drives the header pill. STARTING items (no agent
    # record yet, but inside the spawn-race window) are deliberately
    # excluded — they are "expected to have no owner for a few
    # seconds", not actually orphaned.
    orphan_count = sum(
        1 for r in running
        if r.get("owner", {}).get("alive") is False
        and not r.get("is_starting")
    )
    # Surface a starting tally so the topbar can show it next to
    # "running"/"pending" for visibility.
    starting_count = sum(1 for r in running if r.get("is_starting"))

    return {
        "running": running,
        "blocked": blocked,
        "pending": pending,
        "done_recent": done_recent,
        "abandoned_recent": abandoned_recent,
        "totals": {
            "running": len(running),
            "blocked": len(blocked),
            "pending": len(pending),
            "done": len(done),
            "abandoned": len(abandoned),
        },
        "orphan_count": orphan_count,
        "starting_count": starting_count,
        "fetched_at": datetime.fromtimestamp(_cache.fetched_at, timezone.utc).isoformat()
        if _cache.fetched_at
        else "",
        "cache_age_seconds": int(time.time() - _cache.fetched_at)
        if _cache.fetched_at
        else None,
        "error": err,
        "user": request.headers.get("X-Auth-Request-Email", ""),
        "queue_path": QUEUE_PATH,
        "schema_version": data.get("schema_version") if isinstance(data, dict) else None,
        "recent_done_limit": RECENT_DONE_LIMIT,
        "recent_abandoned_limit": RECENT_ABANDONED_LIMIT,
        # Whitelabel branding — see QUEUE_SITE_* env vars above.
        "site_title": SITE_TITLE,
        "site_logo_url": SITE_LOGO_URL,
        "site_logo_default": SITE_LOGO_DEFAULT,
        "site_brand": SITE_BRAND,
        "site_favicon_url": SITE_FAVICON_URL,
    }


@app.route("/")
def index() -> str:
    return render_template("index.html", **_render_payload())


@app.route("/api/queue")
def api_queue() -> Any:
    payload = _render_payload()
    payload.pop("user", None)
    return jsonify(payload)


def _ids_by_status(*statuses: str) -> dict[str, str]:
    """Map id -> status for items currently in any of the given statuses.

    Bypasses the 5s read cache — we want the freshest state to refuse
    abandons on items that already transitioned (race).
    """
    data, err = _read_queue()
    if err is not None or not isinstance(data, dict):
        return {}
    wanted = set(statuses)
    out: dict[str, str] = {}
    for it in data.get("items", []) or []:
        if not isinstance(it, dict):
            continue
        st = it.get("status")
        if st in wanted:
            iid = it.get("id")
            if isinstance(iid, str):
                out[iid] = st
    return out


def _do_abandon(
    payload: Any,
    *,
    allowed_statuses: tuple[str, ...],
    action_label: str,
) -> Any:
    """Shared implementation for ``/api/queue/stop`` and ``/api/queue/abandon``.

    Both endpoints shell out to the vendored ``session-task queue abandon``
    — the only difference is which current statuses are accepted (a stop
    targets ``running`` items; an abandon targets ``pending``). The
    underlying queue.json mutation, group-head recompute, completed-tasks
    log, and ``queue-abandoned`` claude-event emit are byte-identical
    to the host-side CLI's behavior.

    For running items the owning agent dies on its next non-exempt tool
    call (queue id no longer ``running`` -> obligations-gate denies).
    For pending items there's no agent yet — abandon just removes it
    from the queue. No process kill is attempted in either case.

    Returns:
      200 ``{"ok": true, "id": ..., "reason": ..., "stdout": ...}``
      400 ``{"ok": false, "error": "..."}`` on validation failure
      404 ``{"ok": false, "error": "not <status>"}`` on stale-state race
      500 ``{"ok": false, "error": "...", "stderr": ...}`` on subprocess fail
    """
    if not isinstance(payload, dict):
        return jsonify({"ok": False, "error": "body must be a JSON object"}), 400

    qid = payload.get("id")
    if not isinstance(qid, str) or not _QUEUE_ID_RE.match(qid):
        return (
            jsonify({"ok": False, "error": "invalid or missing 'id'"}),
            400,
        )

    raw_reason = payload.get("reason", "")
    if raw_reason is None:
        raw_reason = ""
    if not isinstance(raw_reason, str):
        return (
            jsonify({"ok": False, "error": "'reason' must be a string"}),
            400,
        )
    reason = raw_reason.strip()[:_MAX_REASON_LEN]

    # Default reason carries identity of the requester for the audit log.
    user = request.headers.get("X-Auth-Request-Email", "ui")
    verb_past = f"{action_label}ed"  # "stopped" / "abandoned"
    if not reason:
        reason = f"{verb_past} via UI by {user}"
    else:
        reason = f"{reason} (via UI by {user})"

    # Refuse the call early if the item isn't in an allowed status.
    # Avoids spurious "abandon" of done items + cuts the subprocess on
    # bad input.
    eligible = _ids_by_status(*allowed_statuses)
    if qid not in eligible:
        allowed_str = "/".join(allowed_statuses) if allowed_statuses else "<none>"
        return (
            jsonify(
                {
                    "ok": False,
                    "error": f"not {allowed_str}",
                    "id": qid,
                    "eligible_now": sorted(eligible),
                }
            ),
            404,
        )

    try:
        proc = subprocess.run(
            [
                "python3",
                SESSION_TASK_BIN,
                "queue",
                "abandon",
                qid,
                "--reason",
                reason,
            ],
            capture_output=True,
            text=True,
            timeout=STOP_TIMEOUT_SECONDS,
        )
    except subprocess.TimeoutExpired:
        return (
            jsonify(
                {
                    "ok": False,
                    "error": f"session-task abandon timed out after {STOP_TIMEOUT_SECONDS}s",
                    "id": qid,
                }
            ),
            500,
        )
    except OSError as exc:
        return (
            jsonify(
                {"ok": False, "error": f"failed to invoke session-task: {exc}", "id": qid}
            ),
            500,
        )

    if proc.returncode != 0:
        return (
            jsonify(
                {
                    "ok": False,
                    "error": "session-task abandon failed",
                    "id": qid,
                    "returncode": proc.returncode,
                    "stdout": proc.stdout,
                    "stderr": proc.stderr,
                }
            ),
            500,
        )

    # Bust the read cache so the very next /api/queue or page reload
    # reflects the abandon transition without the usual 5s lag.
    _cache.fetched_at = 0.0

    return jsonify(
        {
            "ok": True,
            "id": qid,
            "action": action_label,
            "reason": reason,
            "stdout": proc.stdout,
            "stderr": proc.stderr,
            "kill_mechanism": "abandon-only",
            "kill_note": (
                "The owning agent (if any) will be denied on its next "
                "non-exempt tool call by the obligations gate (queue id "
                "no longer in 'running' status). No process kill is "
                "attempted."
            ),
        }
    )


@app.route("/api/queue/stop", methods=["POST"])
def api_queue_stop() -> Any:
    """Abandon a RUNNING queue item (Stop button on the front-end).

    Body: ``{"id": "q-XXXX", "reason": "..."}``. The id must match
    ``_QUEUE_ID_RE`` and currently be in ``running`` status. See
    ``_do_abandon`` for the shared implementation.
    """
    return _do_abandon(
        request.get_json(silent=True) or {},
        allowed_statuses=("running",),
        action_label="stop",
    )


@app.route("/api/queue/abandon", methods=["POST"])
def api_queue_abandon() -> Any:
    """Abandon a PENDING queue item (Abandon button on the front-end).

    Body: ``{"id": "q-XXXX", "reason": "..."}``. The id must match
    ``_QUEUE_ID_RE`` and currently be in ``pending`` status — i.e. it
    was queued but no agent has been spawned for it yet, so abandoning
    it loses no work. See ``_do_abandon`` for the shared implementation.
    """
    return _do_abandon(
        request.get_json(silent=True) or {},
        allowed_statuses=("pending",),
        action_label="abandon",
    )


@app.route("/api/queue/<queue_id>/force-start", methods=["POST"])
def api_queue_force_start(queue_id: str) -> Any:
    """Manually promote a PENDING queue item to running, bypassing scope-
    conflict serialization.

    Body: ``{"reason": "..."}``. Reason is required and is auditable —
    every successful force-start appends a row to
    ``~/.config/claude/queue-force-start.log`` (host) /
    ``$QUEUE_FORCE_START_LOG`` (container). The id must match
    ``_QUEUE_ID_RE`` and currently be in ``pending`` status.

    Returns:
      200 ``{"ok": true, "id": ..., "reason": ..., "stdout": ...}``
      400 on missing/empty reason or invalid id format
      404 if the item is not currently pending (race / wrong state)
      500 on subprocess failure
    """
    if not _QUEUE_ID_RE.match(queue_id):
        return (
            jsonify({"ok": False, "error": "invalid id format", "id": queue_id}),
            400,
        )

    payload = request.get_json(silent=True) or {}
    if not isinstance(payload, dict):
        return jsonify({"ok": False, "error": "body must be a JSON object"}), 400

    raw_reason = payload.get("reason", "")
    if raw_reason is None:
        raw_reason = ""
    if not isinstance(raw_reason, str):
        return (
            jsonify({"ok": False, "error": "'reason' must be a string"}),
            400,
        )
    reason = raw_reason.strip()[:_MAX_REASON_LEN]
    if not reason:
        return (
            jsonify(
                {
                    "ok": False,
                    "error": "'reason' is required for force-start",
                    "id": queue_id,
                }
            ),
            400,
        )

    # Append the requester identity for the audit log, mirroring the abandon
    # path. Upstream auth is via ``X-Auth-Request-Email`` from oauth2-proxy.
    user = request.headers.get("X-Auth-Request-Email", "ui")
    annotated_reason = f"{reason} (via UI by {user})"

    eligible = _ids_by_status("pending")
    if queue_id not in eligible:
        return (
            jsonify(
                {
                    "ok": False,
                    "error": "not pending",
                    "id": queue_id,
                    "eligible_now": sorted(eligible),
                }
            ),
            404,
        )

    try:
        proc = subprocess.run(
            [
                "python3",
                SESSION_TASK_BIN,
                "queue",
                "force-start",
                queue_id,
                "--reason",
                annotated_reason,
            ],
            capture_output=True,
            text=True,
            timeout=STOP_TIMEOUT_SECONDS,
        )
    except subprocess.TimeoutExpired:
        return (
            jsonify(
                {
                    "ok": False,
                    "error": (
                        f"session-task force-start timed out after "
                        f"{STOP_TIMEOUT_SECONDS}s"
                    ),
                    "id": queue_id,
                }
            ),
            500,
        )
    except OSError as exc:
        return (
            jsonify(
                {
                    "ok": False,
                    "error": f"failed to invoke session-task: {exc}",
                    "id": queue_id,
                }
            ),
            500,
        )

    if proc.returncode != 0:
        return (
            jsonify(
                {
                    "ok": False,
                    "error": "session-task force-start failed",
                    "id": queue_id,
                    "returncode": proc.returncode,
                    "stdout": proc.stdout,
                    "stderr": proc.stderr,
                }
            ),
            500,
        )

    # Bust the read cache so the very next /api/queue or page reload
    # reflects the pending -> running transition without the usual 5s lag.
    _cache.fetched_at = 0.0

    return jsonify(
        {
            "ok": True,
            "id": queue_id,
            "action": "force-start",
            "reason": annotated_reason,
            "stdout": proc.stdout,
            "stderr": proc.stderr,
        }
    )


@app.route("/api/queue/depend", methods=["POST"])
def api_queue_depend() -> Any:
    """Register a dependency edge between two queue items.

    Body: ``{"dragged_id": "q-XXXX", "target_id": "q-YYYY"}``.

    Semantics: ``dragged`` becomes blocked-on ``target`` — i.e. the
    drag-onto-target gesture means "this follow-up waits for the in-
    flight work". Adjacency-list storage; ``ready_now`` flips True only
    when every dep is in ``done`` state. Cross-group deps are allowed.

    Architecture (per the 2026-05-02 perf brief): WRITE is just a set
    add on the dragged item. NO transitive-closure cache, NO cascade
    work — read-time compute resolves ``ready_now`` lazily.

      200 — edge added (or already present, idempotent)
      400 — invalid id format, dragged == target, or non-pending state
      404 — either id doesn't exist
      500 — queue.json unreadable / session-task subprocess failure
    """
    payload = request.get_json(silent=True) or {}
    if not isinstance(payload, dict):
        return jsonify({"ok": False, "error": "body must be a JSON object"}), 400

    dragged = payload.get("dragged_id")
    target = payload.get("target_id")

    if not isinstance(dragged, str) or not _QUEUE_ID_RE.match(dragged):
        return (
            jsonify({"ok": False, "error": "invalid or missing 'dragged_id'"}),
            400,
        )
    if not isinstance(target, str) or not _QUEUE_ID_RE.match(target):
        return (
            jsonify({"ok": False, "error": "invalid or missing 'target_id'"}),
            400,
        )
    if dragged == target:
        return (
            jsonify({"ok": False, "error": "an item cannot depend on itself"}),
            400,
        )

    # Existence + state check — bypass the cache for freshness. The
    # dragged item must be ``pending`` (running items are already in
    # flight; done/abandoned items can't acquire new dependencies). The
    # target may be ``pending`` or ``running`` — depending on a running
    # item is the common "wait for the in-flight work to finish" case.
    data, err = _read_queue()
    if err is not None:
        return (
            jsonify({"ok": False, "error": f"queue.json unreadable: {err}"}),
            500,
        )
    by_id: dict[str, dict[str, Any]] = {}
    for it in data.get("items", []) or []:
        if isinstance(it, dict):
            iid = it.get("id")
            if isinstance(iid, str):
                by_id[iid] = it

    if dragged not in by_id:
        return (
            jsonify({"ok": False, "error": "dragged_id not found", "id": dragged}),
            404,
        )
    if target not in by_id:
        return (
            jsonify({"ok": False, "error": "target_id not found", "id": target}),
            404,
        )

    dragged_status = by_id[dragged].get("status")
    target_status = by_id[target].get("status")
    if dragged_status != "pending":
        return (
            jsonify(
                {
                    "ok": False,
                    "error": f"dragged item must be pending (is {dragged_status!r})",
                    "id": dragged,
                }
            ),
            400,
        )
    if target_status not in ("pending", "running"):
        return (
            jsonify(
                {
                    "ok": False,
                    "error": (
                        f"target item must be pending or running "
                        f"(is {target_status!r})"
                    ),
                    "id": target,
                }
            ),
            400,
        )

    # Shell out to session-task — the canonical writer holds the
    # fcntl.flock on queue.json, so we never race with concurrent
    # writes from the host CLI or other endpoint hits.
    try:
        proc = subprocess.run(
            [
                "python3",
                SESSION_TASK_BIN,
                "queue",
                "depend",
                dragged,
                "--add",
                target,
                "--json",
            ],
            capture_output=True,
            text=True,
            timeout=STOP_TIMEOUT_SECONDS,
        )
    except subprocess.TimeoutExpired:
        return (
            jsonify(
                {
                    "ok": False,
                    "error": (
                        f"session-task depend timed out after "
                        f"{STOP_TIMEOUT_SECONDS}s"
                    ),
                    "id": dragged,
                }
            ),
            500,
        )
    except OSError as exc:
        return (
            jsonify(
                {
                    "ok": False,
                    "error": f"failed to invoke session-task: {exc}",
                    "id": dragged,
                }
            ),
            500,
        )

    if proc.returncode != 0:
        return (
            jsonify(
                {
                    "ok": False,
                    "error": "session-task depend failed",
                    "id": dragged,
                    "returncode": proc.returncode,
                    "stdout": proc.stdout,
                    "stderr": proc.stderr,
                }
            ),
            500,
        )

    try:
        cli_out = json.loads(proc.stdout) if proc.stdout.strip() else {}
    except json.JSONDecodeError:
        cli_out = {"raw": proc.stdout}

    # Bust the read cache so the new edge surfaces on the next /api/queue.
    _cache.fetched_at = 0.0

    return jsonify(
        {
            "ok": True,
            "dragged_id": dragged,
            "target_id": target,
            "depends_on": cli_out.get("depends_on", []),
            "ready_now": cli_out.get("ready_now"),
            "dep_cycle": cli_out.get("dep_cycle", False),
        }
    )


@app.route("/api/queue/<queue_id>/depend", methods=["DELETE"])
def api_queue_depend_remove(queue_id: str) -> Any:
    """Remove a single dependency edge from a queue item.

    Body: ``{"target_id": "q-YYYY"}`` — removes ``target_id`` from
    ``queue_id.depends_on``.

    Companion to ``POST /api/queue/depend``. The UI surfaces the
    "x" affordance on dep badges so an operator can untangle a
    mis-dragged edge without opening a terminal.
    """
    if not _QUEUE_ID_RE.match(queue_id):
        return (
            jsonify({"ok": False, "error": "invalid id format", "id": queue_id}),
            400,
        )
    payload = request.get_json(silent=True) or {}
    target = payload.get("target_id") if isinstance(payload, dict) else None
    if not isinstance(target, str) or not _QUEUE_ID_RE.match(target):
        return (
            jsonify({"ok": False, "error": "invalid or missing 'target_id'"}),
            400,
        )

    try:
        proc = subprocess.run(
            [
                "python3",
                SESSION_TASK_BIN,
                "queue",
                "depend",
                queue_id,
                "--remove",
                target,
                "--json",
            ],
            capture_output=True,
            text=True,
            timeout=STOP_TIMEOUT_SECONDS,
        )
    except subprocess.TimeoutExpired:
        return (
            jsonify(
                {
                    "ok": False,
                    "error": "session-task depend --remove timed out",
                    "id": queue_id,
                }
            ),
            500,
        )

    if proc.returncode != 0:
        return (
            jsonify(
                {
                    "ok": False,
                    "error": "session-task depend --remove failed",
                    "id": queue_id,
                    "stdout": proc.stdout,
                    "stderr": proc.stderr,
                }
            ),
            500,
        )

    try:
        cli_out = json.loads(proc.stdout) if proc.stdout.strip() else {}
    except json.JSONDecodeError:
        cli_out = {}

    _cache.fetched_at = 0.0
    return jsonify(
        {
            "ok": True,
            "id": queue_id,
            "removed_target": target,
            "depends_on": cli_out.get("depends_on", []),
            "ready_now": cli_out.get("ready_now"),
        }
    )


# ---------------------------------------------------------------------------
# Live-log streaming (SSE)
# ---------------------------------------------------------------------------
#
# `GET /api/queue/<id>/stream` — Server-Sent Events stream of the agent
# transcript for the queue item's currently-running owner. Each new line
# appended to the agent JSONL is emitted as a single SSE `data:` event.
#
# Why SSE (not websockets):
#   - One-way (server -> client) tail-only stream: no client uplink needed.
#   - Plain HTTP, traverses oauth2-proxy + nginx without protocol upgrade
#     gymnastics.
#   - Works behind the existing gunicorn sync+threads worker model
#     (one connection ties up one thread; we only have 1-2 viewers).
#
# nginx default: `proxy_buffering on` would buffer the SSE stream. We set
# `X-Accel-Buffering: no` on the response which nginx honors as an
# explicit per-response disable. Operators running their own reverse
# proxy should also set `proxy_buffering off` (and `gzip off` if their
# vhost gzips `text/event-stream`) on the SSE location as a belt-and-
# suspenders measure — the `X-Accel-Buffering` header only fires the
# nginx-internal `proxy_buffering` disable; it does NOT defeat the gzip
# filter or any third-party output filter that aggregates writes.
#
# All Response() instances below pass `direct_passthrough=True` so that
# werkzeug emits each `yield`-ed string to the WSGI socket verbatim
# instead of buffering them through its iter-wrap. Without this, small
# SSE frames can sit in werkzeug's internal buffer until the generator
# yields enough bytes to trigger a flush.

# How long to wait between file polls when EOF is hit.
SSE_TAIL_POLL_SECONDS = float(os.environ.get("SSE_TAIL_POLL_SECONDS", "0.5"))
# How long to keep streaming with no new data before giving up. Defaults
# to 30 minutes — long enough for a dormant agent to wake up, short
# enough that abandoned tabs eventually free their thread.
SSE_TAIL_MAX_IDLE_SECONDS = float(os.environ.get("SSE_TAIL_MAX_IDLE_SECONDS", "1800"))
# Hard cap on the total stream lifetime (per connection). Browsers
# auto-reconnect EventSource on abrupt close, so this caps thread leaks
# from misbehaving clients while still letting a viewer watch a long
# agent run by reconnecting.
SSE_TAIL_MAX_LIFETIME_SECONDS = float(
    os.environ.get("SSE_TAIL_MAX_LIFETIME_SECONDS", "3600")
)
# How often to send an SSE comment as a keep-alive while we're waiting
# for new lines. Prevents intermediaries from closing the connection.
SSE_KEEPALIVE_SECONDS = float(os.environ.get("SSE_KEEPALIVE_SECONDS", "15"))
# Don't replay more than this many lines of historical context when a
# client first connects — for a long agent transcript we don't need to
# ship the entire log; we just want the recent tail + everything
# appended after.
SSE_TAIL_BACKFILL_LINES = int(os.environ.get("SSE_TAIL_BACKFILL_LINES", "200"))


def _find_agent_jsonl(agent_id: str) -> Path | None:
    """Locate the JSONL transcript for ``agent_id`` under AGENTS_JSONL_ROOT.

    Layout (mirrors host ``~/.claude/projects/<project-slug>/``):

        <root>/<session-uuid>/subagents/agent-<agent_id>.jsonl

    Returns the most-recently-modified match (handles agent_id reuse
    across sessions, though that's rare). Returns None if the agent_id
    is not safe (path traversal guard) or no file is found.
    """
    # Guard against path traversal — agent_ids are 17-char lowercase hex
    # in practice; allow a slightly broader alphanum/dash set to handle
    # the `acompact-XXX` variant and stay forward-compatible. Anything
    # else means the request is bogus and we refuse.
    if not re.match(r"^[a-z0-9-]{4,64}$", agent_id):
        return None
    root = Path(AGENTS_JSONL_ROOT)
    if not root.is_dir():
        return None
    needle = f"agent-{agent_id}.jsonl"
    best: tuple[float, Path] | None = None
    try:
        session_dirs = list(root.iterdir())
    except OSError:
        return None
    for session in session_dirs:
        candidate = session / "subagents" / needle
        try:
            mtime = candidate.stat().st_mtime
        except OSError:
            continue
        if best is None or mtime > best[0]:
            best = (mtime, candidate)
    return best[1] if best else None


def _format_sse(data: dict[str, Any]) -> bytes:
    """Encode a dict as a single SSE `data:` event.

    Returns bytes so the surrounding Response can use
    ``direct_passthrough=True`` (which requires byte chunks per WSGI
    PEP-3333). Without this, gunicorn raises
    ``TypeError('...' is not a byte')`` on every yielded frame.
    """
    return ("data: " + json.dumps(data, separators=(",", ":")) + "\n\n").encode("utf-8")


def _format_sse_comment(text: str) -> bytes:
    """SSE comment line — used as a keep-alive ping. Browsers ignore it.

    Returns bytes for ``direct_passthrough=True`` compatibility (same
    rationale as ``_format_sse``).
    """
    return f": {text}\n\n".encode("utf-8")


def _collapse_transient_runs(
    segments: list[tuple[str, bool]],
) -> list[tuple[str, bool]]:
    """Collapse consecutive ``transient=True`` segments to their LAST frame.

    Used ONLY by the backfill code path — the live-tail keeps every
    transient frame on the wire so the front-end can render the in-place
    rewrite animation. For backfill the user has just opened the modal:
    showing 1000s of historical rsync progress frames is noise and
    starves the line budget. We want them to see the FINAL state of each
    transient run (the last `\\r`-terminated frame before a `\\n`
    graduates the line, or the bare trailing transient at EOF) plus all
    permanent lines.

    Input: ``[(text, transient)]`` from ``_split_cr_lf_segments``.
    Output: same shape, with runs of ``transient=True`` collapsed.

    Examples (``T``=transient, ``F``=permanent)::

        T T T F  ->  T F        (keep last transient before the LF graduates)
        T T T    ->  T          (bare trailing transient run at EOF)
        F T F    ->  F T F      (single transient untouched)
        F F      ->  F F        (no transients, no change)
    """
    if not segments:
        return segments
    out: list[tuple[str, bool]] = []
    last_transient: tuple[str, bool] | None = None
    for text, transient in segments:
        if transient:
            last_transient = (text, transient)
        else:
            if last_transient is not None:
                out.append(last_transient)
                last_transient = None
            out.append((text, transient))
    if last_transient is not None:
        out.append(last_transient)
    return out


def _split_cr_lf_segments(
    buf: str, *, flush_remainder: bool = False
) -> tuple[list[tuple[str, bool]], str]:
    """Split a text buffer on BOTH ``\\r`` and ``\\n`` terminators.

    Used by the workload tail/replay paths so single-line in-place
    progress updates (``rsync --info=progress2``, ``curl``, bare
    ``printf "\\r"`` loops) surface to the client as transient segments
    that the front-end can render as one updating row rather than
    thousands of stacked rows.

    Returns ``(segments, remainder)`` where each segment is
    ``(text, transient)``:
      * ``transient=True``  — segment ended with a ``\\r`` (progress
        update; the next segment should REPLACE this one in the UI).
      * ``transient=False`` — segment ended with a ``\\n`` (or
        ``\\r\\n`` — collapsed to a single ``\\n`` terminator); the
        segment "graduates" to permanent.

    ``remainder`` is the unterminated tail; the caller is expected to
    prepend it to the next chunk so terminators that straddle a chunk
    boundary (notably ``\\r\\n`` arriving across two reads) collapse
    correctly.

    ``flush_remainder=True`` emits any unterminated tail as a final
    ``transient=False`` segment — used at workload exit / archive EOF
    so the last line of a producer that didn't end with a newline is
    still surfaced. (We treat it as permanent because there will be no
    further segments to replace it.) ``remainder`` is empty in that case.
    """
    segments: list[tuple[str, bool]] = []
    i = 0
    start = 0
    n = len(buf)
    while i < n:
        ch = buf[i]
        if ch == "\n":
            segments.append((buf[start:i], False))
            i += 1
            start = i
        elif ch == "\r":
            # Lookahead for \r\n. If the buffer ends in a bare \r AND
            # we're not flushing, defer — the next chunk might start
            # with \n, in which case it would be a single \r\n terminator.
            if i + 1 < n:
                if buf[i + 1] == "\n":
                    segments.append((buf[start:i], False))
                    i += 2
                    start = i
                else:
                    segments.append((buf[start:i], True))
                    i += 1
                    start = i
            else:
                # Bare \r at end-of-buffer.
                if flush_remainder:
                    segments.append((buf[start:i], True))
                    i += 1
                    start = i
                else:
                    # Stop here; carry \r + everything after as remainder
                    # so the next read can fuse a possible \n.
                    break
        else:
            i += 1
    remainder = buf[start:]
    if flush_remainder and remainder:
        segments.append((remainder, False))
        remainder = ""
    return segments, remainder


def _tail_jsonl(path: Path) -> Iterator[bytes]:
    """Generator yielding SSE events for a tailed agent JSONL.

    Opens the file in tail mode: backfill the last ``SSE_TAIL_BACKFILL_LINES``
    so the viewer has immediate context, then poll for new lines.
    Emits a comment every ``SSE_KEEPALIVE_SECONDS`` while idle so the
    EventSource stays open through proxies. Stops after
    ``SSE_TAIL_MAX_IDLE_SECONDS`` of no new data, or
    ``SSE_TAIL_MAX_LIFETIME_SECONDS`` total.

    Each yielded chunk is a complete SSE frame; the caller emits them
    verbatim into the response body.
    """
    started = time.monotonic()
    last_data_at = started
    last_keepalive_at = started

    # Open + seek. We use line-by-line reads from the start so we can
    # honor the backfill cap without buffering the whole file. For the
    # initial backfill we read the file once, take the tail N lines,
    # emit them, then seek to EOF and start polling.
    yield _format_sse({
        "type": "meta",
        "kind": "stream-start",
        "path": str(path),
        "ts": time.time(),
    })

    try:
        f = open(path, "r", encoding="utf-8", errors="replace")
    except OSError as exc:
        yield _format_sse({"type": "error", "kind": "open-failed", "error": str(exc)})
        return

    try:
        # Backfill: read all, take tail.
        try:
            backfill = f.readlines()[-SSE_TAIL_BACKFILL_LINES:]
        except OSError as exc:
            yield _format_sse({"type": "error", "kind": "read-failed", "error": str(exc)})
            return

        if backfill:
            yield _format_sse({
                "type": "meta",
                "kind": "backfill-begin",
                "lines": len(backfill),
            })
            for line in backfill:
                line = line.rstrip("\n")
                if line:
                    yield _format_sse(_parse_jsonl_line(line))
                    last_data_at = time.monotonic()
            yield _format_sse({"type": "meta", "kind": "backfill-end"})

        # Tail loop.
        while True:
            line = f.readline()
            if line:
                line = line.rstrip("\n")
                if line:
                    yield _format_sse(_parse_jsonl_line(line))
                    last_data_at = time.monotonic()
                continue
            now = time.monotonic()
            if now - last_data_at > SSE_TAIL_MAX_IDLE_SECONDS:
                yield _format_sse({
                    "type": "meta",
                    "kind": "idle-timeout",
                    "idle_seconds": int(now - last_data_at),
                })
                return
            if now - started > SSE_TAIL_MAX_LIFETIME_SECONDS:
                yield _format_sse({
                    "type": "meta",
                    "kind": "lifetime-timeout",
                    "seconds": int(now - started),
                })
                return
            if now - last_keepalive_at > SSE_KEEPALIVE_SECONDS:
                yield _format_sse_comment(f"keep-alive {int(now - started)}s")
                last_keepalive_at = now
            time.sleep(SSE_TAIL_POLL_SECONDS)
    except GeneratorExit:
        # Client disconnected — nothing to clean up; the file handle
        # closes via the finally below.
        return
    finally:
        try:
            f.close()
        except OSError:
            pass


def _tail_workload_output(label: str) -> Iterator[bytes]:
    """Generator yielding SSE events for a tailed workload output file.

    Workload files are line-oriented plain text (NOT JSONL); each appended
    line becomes one ``{"type":"event","kind":"workload_line","text":...}``
    SSE frame. The companion ``<label>.exit`` file is written by the
    workload runner when the process exits — its presence (after we've
    drained the output file) terminates the stream with a single
    ``workload-end`` meta frame so the front-end can flip the status to
    ``done``.

    Same backfill / keepalive / timeout knobs as ``_tail_jsonl``.
    """
    started = time.monotonic()
    last_data_at = started
    last_keepalive_at = started

    out_path = Path(WORKLOAD_LOG_DIR) / f"{label}.output"
    exit_path = Path(WORKLOAD_LOG_DIR) / f"{label}.exit"

    yield _format_sse({
        "type": "meta",
        "kind": "stream-start",
        "mode": "workload",
        "path": str(out_path),
        "label": label,
        "ts": time.time(),
    })

    try:
        # newline="" disables Python's universal-newlines translation so
        # bare \r bytes survive into the buffer for _split_cr_lf_segments
        # to see. Without this, the default newline=None mode rewrites
        # both \r\n AND lone \r to \n during read(), erasing the very
        # signal we need to detect rsync-style progress frames.
        f = open(out_path, "r", encoding="utf-8", errors="replace", newline="")
    except OSError as exc:
        yield _format_sse({"type": "error", "kind": "open-failed", "error": str(exc)})
        return

    def _emit_end(reason: str) -> Iterator[bytes]:
        # Best-effort exit code surfacing — short read of the .exit file
        # if present. Non-fatal if read fails.
        exit_code: Any = None
        try:
            with open(exit_path, "r", encoding="utf-8", errors="replace") as ef:
                raw = ef.read().strip()
                if raw:
                    try:
                        exit_code = int(raw.split()[0])
                    except (ValueError, IndexError):
                        exit_code = raw[:32]
        except OSError:
            pass
        yield _format_sse({
            "type": "meta",
            "kind": "workload-end",
            "label": label,
            "reason": reason,
            "exit_code": exit_code,
        })

    # Read buffer carried across iterations so CR/LF terminators that
    # straddle a chunk boundary fuse correctly (notably \r\n split across
    # two reads). _split_cr_lf_segments returns (segments, remainder).
    pending = ""
    # Chunk size for the byte-level read loop. The producer writes line
    # at a time so anything in the 4–64 KB range is fine; 8 KB matches
    # typical pipe buffer sizes.
    READ_CHUNK = 8192

    try:
        # Backfill: read the whole file, split on \r AND \n, take tail N
        # segments. For an archived rsync-style log this means the user
        # sees the FINAL progress line + the trailing "done" rather than
        # 1000s of stacked progress rows.
        try:
            initial = f.read()
        except OSError as exc:
            yield _format_sse({"type": "error", "kind": "read-failed", "error": str(exc)})
            return
        # For backfill we flush any unterminated tail as a permanent
        # segment — the file at this point is whatever the producer has
        # written so far; if it's mid-line we'd rather surface it than
        # hide it. The tail loop below starts fresh from the live EOF.
        backfill_segments, _ = _split_cr_lf_segments(initial, flush_remainder=True)
        # Collapse consecutive transient (\r-terminated) frames to their
        # LAST member BEFORE applying the line-budget trim. Without this,
        # a long rsync (1000s of progress frames in the .output) eats the
        # entire 200-line budget with mid-flight progress percentages and
        # the actual context (stv-promote header, file completions,
        # earlier shows) falls off the top. q-2026-05-13-65b0. The live
        # tail path is UNCHANGED — every transient still streams so the
        # front-end can animate in-place rewrites.
        backfill_segments = _collapse_transient_runs(backfill_segments)
        if len(backfill_segments) > SSE_TAIL_BACKFILL_LINES:
            backfill_segments = backfill_segments[-SSE_TAIL_BACKFILL_LINES:]

        if backfill_segments:
            yield _format_sse({
                "type": "meta",
                "kind": "backfill-begin",
                "lines": len(backfill_segments),
            })
            for text, transient in backfill_segments:
                yield _format_sse({
                    "type": "event",
                    "kind": "workload_line",
                    "text": text,
                    "transient": transient,
                })
                last_data_at = time.monotonic()
            yield _format_sse({"type": "meta", "kind": "backfill-end"})

        # Tail loop. Terminate as soon as we observe the .exit file AND
        # have drained the output file at EOF — that ordering avoids
        # cutting off the last line of a workload that exits between
        # reads.
        while True:
            chunk = f.read(READ_CHUNK)
            if chunk:
                pending += chunk
                segments, pending = _split_cr_lf_segments(pending)
                for text, transient in segments:
                    yield _format_sse({
                        "type": "event",
                        "kind": "workload_line",
                        "text": text,
                        "transient": transient,
                    })
                    last_data_at = time.monotonic()
                continue
            # EOF — check if the workload has exited.
            if exit_path.exists():
                # One more read in case bytes were appended between the
                # last poll and the exit check.
                trailing = f.read()
                if trailing:
                    pending += trailing
                segments, _ = _split_cr_lf_segments(pending, flush_remainder=True)
                pending = ""
                for text, transient in segments:
                    yield _format_sse({
                        "type": "event",
                        "kind": "workload_line",
                        "text": text,
                        "transient": transient,
                    })
                yield from _emit_end("exit")
                return
            now = time.monotonic()
            if now - last_data_at > SSE_TAIL_MAX_IDLE_SECONDS:
                yield _format_sse({
                    "type": "meta",
                    "kind": "idle-timeout",
                    "idle_seconds": int(now - last_data_at),
                })
                return
            if now - started > SSE_TAIL_MAX_LIFETIME_SECONDS:
                yield _format_sse({
                    "type": "meta",
                    "kind": "lifetime-timeout",
                    "seconds": int(now - started),
                })
                return
            if now - last_keepalive_at > SSE_KEEPALIVE_SECONDS:
                yield _format_sse_comment(f"keep-alive {int(now - started)}s")
                last_keepalive_at = now
            time.sleep(SSE_TAIL_POLL_SECONDS)
    except GeneratorExit:
        return
    finally:
        try:
            f.close()
        except OSError:
            pass


def _parse_jsonl_line(line: str) -> dict[str, Any]:
    """Parse a single transcript JSONL line into a stream event payload.

    Pretty-printing (tool calls, tool results, text deltas) is the
    front-end's responsibility — we just hand it the parsed JSON record
    plus a small ``kind`` hint so the renderer can pick a template
    without re-parsing the structure. Falling back to ``raw`` keeps the
    stream useful even when the schema drifts.
    """
    try:
        rec = json.loads(line)
    except (ValueError, TypeError):
        return {"type": "raw", "line": line}
    kind = "unknown"
    if isinstance(rec, dict):
        rtype = rec.get("type")
        if rtype == "user":
            # Tool result if message.content is a list with tool_result;
            # image if it's an image block; otherwise plain user text.
            msg = rec.get("message") or {}
            content = msg.get("content") if isinstance(msg, dict) else None
            if isinstance(content, list) and any(
                isinstance(c, dict) and c.get("type") == "tool_result" for c in content
            ):
                kind = "tool_result"
            elif isinstance(content, list) and any(
                isinstance(c, dict) and c.get("type") == "image" for c in content
            ):
                kind = "user_image"
            else:
                kind = "user"
        elif rtype == "assistant":
            msg = rec.get("message") or {}
            content = msg.get("content") if isinstance(msg, dict) else None
            # Pick the *first* content-block type as the kind hint. The
            # front-end always re-walks `message.content` to render every
            # block (text + tool_use + thinking can co-occur in one
            # record), so this is just a hint for default styling /
            # iconography. Order matters when an assistant turn mixes a
            # `thinking` block followed by a `tool_use` — we surface the
            # tool_use as the headline because that's the actionable bit.
            if isinstance(content, list) and any(
                isinstance(c, dict) and c.get("type") == "tool_use" for c in content
            ):
                kind = "tool_use"
            elif isinstance(content, list) and any(
                isinstance(c, dict) and c.get("type") == "thinking" for c in content
            ):
                kind = "thinking"
            elif isinstance(content, list) and any(
                isinstance(c, dict) and c.get("type") == "text" for c in content
            ):
                kind = "assistant_text"
            else:
                kind = "assistant"
        elif rtype == "attachment":
            kind = "attachment"
        elif rtype == "system":
            kind = "system"
        elif rtype == "progress":
            kind = "progress"
    return {"type": "event", "kind": kind, "rec": rec}


@app.route("/api/queue/<qid>/stream")
def api_queue_stream(qid: str) -> Any:
    """Server-Sent Events stream of the agent transcript for queue item ``qid``.

    Looks up the owning agent via claude-watch's active-agents JSON,
    then tails the matching ``agent-<id>.jsonl`` under AGENTS_JSONL_ROOT.

    Response: ``text/event-stream`` with ``X-Accel-Buffering: no`` so
    nginx flushes per-event. Browsers auto-reconnect on close.

    Errors are emitted as in-stream events (``type: error``) rather than
    HTTP 4xx/5xx so the front-end can surface them in the modal without
    blowing up the EventSource. The exception is the format guard on
    ``qid`` itself, where a 400 short-circuits before we open the
    stream.
    """
    if not _QUEUE_ID_RE.match(qid):
        return jsonify({"ok": False, "error": "invalid queue id format"}), 400

    headers = {
        "Content-Type": "text/event-stream",
        "Cache-Control": "no-cache",
        "Connection": "keep-alive",
        # nginx-specific: disable response buffering so each event is
        # flushed end-to-end without waiting for an internal buffer to
        # fill. Equivalent to `proxy_buffering off` per-request.
        "X-Accel-Buffering": "no",
    }

    # Workload dispatch — items created by `workload run <label>` carry
    # scope `["workload:<label>"]` and have NO entry in the active-agents
    # map (workloads aren't subagents). For these we tail
    # /tmp/claude-workloads/<label>.output (bind-mounted at WORKLOAD_LOG_DIR)
    # using the dedicated SSE generator. q-fb55 (saoirse-logical-200x)
    # exposed this gap — without this branch the agent-based lookup falls
    # through to ``_no_agent`` and the user sees a placeholder error.
    queue_data, _qerr = _read_queue()
    workload_label = ""
    if isinstance(queue_data, dict):
        for it in queue_data.get("items", []) or []:
            if isinstance(it, dict) and it.get("id") == qid:
                workload_label = _extract_workload_label(it.get("scope") or [])
                break
    if workload_label:
        return Response(
            stream_with_context(_tail_workload_output(workload_label)),
            headers=headers,
            direct_passthrough=True,
        )

    agent_by_qid = _agents_by_qid(_load_state(AGENT_STATE_PATH))
    rec = agent_by_qid.get(qid)

    if rec is None:
        # No agent record for this queue id — emit a one-shot error
        # event then close. Stream-shaped error keeps the client logic
        # simple (it always opens an EventSource).
        def _no_agent() -> Iterator[bytes]:
            yield _format_sse({
                "type": "error",
                "kind": "no-agent",
                "queue_id": qid,
                "error": (
                    "No active agent record found for this queue id. "
                    "The agent may have already exited, or the "
                    "active-agents.json state file may be stale."
                ),
            })

        return Response(
            stream_with_context(_no_agent()),
            headers=headers,
            direct_passthrough=True,
        )

    agent_id = rec.get("agent_id", "")
    jsonl_path = _find_agent_jsonl(agent_id) if agent_id else None

    if jsonl_path is None:
        def _no_jsonl() -> Iterator[bytes]:
            yield _format_sse({
                "type": "error",
                "kind": "no-jsonl",
                "queue_id": qid,
                "agent_id": agent_id,
                "error": (
                    f"Agent transcript not found for agent_id={agent_id!r}. "
                    "The JSONL file may not yet exist or is outside the "
                    "configured AGENTS_JSONL_ROOT."
                ),
            })

        return Response(
            stream_with_context(_no_jsonl()),
            headers=headers,
            direct_passthrough=True,
        )

    return Response(
        stream_with_context(_tail_jsonl(jsonl_path)),
        headers=headers,
        direct_passthrough=True,
    )


# ---------------------------------------------------------------------------
# Archive replay (SSE) — historical transcripts for done / abandoned items.
# ---------------------------------------------------------------------------
#
# `GET /api/queue/<id>/archive` — Server-Sent Events stream of an
# already-archived agent transcript. Reads the JSONL out of
# QUEUE_LOG_ARCHIVE_DIR (populated by session-task at queue-done /
# queue-abandon time), parses each line through the same `_parse_jsonl_line`
# the live tail uses, and emits SSE frames in the same envelope so the
# front-end's modal logic is reusable. The connection closes once we
# hit EOF — no tail loop, no keepalive comments.
#
# Why SSE-shaped (not just text/plain): the front-end already knows how
# to consume `data: {...}` frames and pretty-print events. Reusing the
# wire format keeps the JS tiny and the look-and-feel identical between
# live and archived modes.


def _replay_jsonl(path: Path) -> Iterator[bytes]:
    """Generator that streams an archived JSONL as SSE frames and stops at EOF.

    Reads the file line-by-line so we never load the whole transcript
    into memory (long agent runs can exceed several MB). Emits a
    ``stream-start`` meta frame, then one ``event`` frame per line, then
    an ``archive-end`` meta frame, then closes — no tailing, no
    backfill cap, no keepalive.
    """
    yield _format_sse({
        "type": "meta",
        "kind": "stream-start",
        "mode": "archive",
        "path": str(path),
        "ts": time.time(),
    })

    line_count = 0
    try:
        with open(path, "r", encoding="utf-8", errors="replace") as f:
            for raw in f:
                raw = raw.rstrip("\n")
                if not raw:
                    continue
                yield _format_sse(_parse_jsonl_line(raw))
                line_count += 1
    except OSError as exc:
        yield _format_sse({
            "type": "error",
            "kind": "read-failed",
            "error": str(exc),
        })
        return
    except GeneratorExit:
        return

    yield _format_sse({
        "type": "meta",
        "kind": "archive-end",
        "lines": line_count,
    })


def _replay_workload_output(path: Path) -> Iterator[bytes]:
    """Generator that streams an archived workload .output file as SSE frames.

    Workload files are plain line-oriented stdout/stderr — NOT JSONL.
    Each non-empty line becomes one ``workload_line`` SSE event, mirroring
    the live-stream wire format produced by ``_tail_workload_output``.
    The front-end log modal flips on ``meta.mode == "workload"`` /
    ``kind == "workload_line"`` and renders the same way for archived
    runs as it does for live tails.

    Bookends:
      * Open with a ``stream-start`` meta frame (``mode=archive-workload``
        so the client knows it's a finished archive, not a live tail).
      * Close with an ``archive-end`` meta frame — same ``kind`` the
        agent variant uses so the client's stream-close handler is
        shared.
    """
    yield _format_sse({
        "type": "meta",
        "kind": "stream-start",
        "mode": "archive-workload",
        "path": str(path),
        "ts": time.time(),
    })

    line_count = 0
    try:
        # newline="" disables universal-newlines translation so \r bytes
        # survive into the buffer for _split_cr_lf_segments. See the
        # matching note in _tail_workload_output.
        with open(path, "r", encoding="utf-8", errors="replace", newline="") as f:
            data = f.read()
        # Replay an entire archive in one pass — splitting on \r AND \n
        # so producers that wrote in-place progress updates (rsync,
        # curl) surface their final state instead of replaying every
        # \r-terminated frame as a stacked permanent row.
        segments, _ = _split_cr_lf_segments(data, flush_remainder=True)
        # Collapse consecutive transient (\r-terminated) frames to their
        # LAST member — archived workloads see the final state of each
        # progress run, not every mid-flight frame. Same rationale as
        # the live backfill path in _tail_workload_output.
        segments = _collapse_transient_runs(segments)
        for text, transient in segments:
            yield _format_sse({
                "type": "event",
                "kind": "workload_line",
                "text": text,
                "transient": transient,
            })
            line_count += 1
    except OSError as exc:
        yield _format_sse({
            "type": "error",
            "kind": "read-failed",
            "error": str(exc),
        })
        return
    except GeneratorExit:
        return

    yield _format_sse({
        "type": "meta",
        "kind": "archive-end",
        "lines": line_count,
    })


def _archive_path_for(qid: str) -> tuple[Path, str] | None:
    """Resolve a queue id to its archive file path + kind, or None if missing.

    Cross-checks the queue.json record (must list ``log_archive_path``)
    AND the file's actual presence on disk. Both gates exist so the
    endpoint never returns content for an item the queue has no record
    of, AND never tries to open a stale path that's been GC'd.

    Returns ``(Path, kind)`` where ``kind`` is one of:
      * ``"jsonl"``     — agent transcript (filename ends ``.jsonl``)
      * ``"workload"``  — workload stdout (filename ends ``.workload.txt``)

    Returns None for unknown / missing / malformed records.
    """
    if not _QUEUE_ID_RE.match(qid):
        return None
    data, err = _read_queue()
    if err is not None or not isinstance(data, dict):
        return None
    item = next(
        (
            it for it in (data.get("items") or [])
            if isinstance(it, dict) and it.get("id") == qid
        ),
        None,
    )
    if item is None:
        return None
    raw = item.get("log_archive_path")
    if not isinstance(raw, str):
        return None
    # Strict: the stored value should be a bare filename, not a path.
    if "/" in raw or ".." in raw:
        return None
    if raw.endswith(".workload.txt"):
        kind = "workload"
    elif raw.endswith(".jsonl"):
        kind = "jsonl"
    else:
        return None
    p = Path(QUEUE_LOG_ARCHIVE_DIR) / raw
    if not p.is_file():
        return None
    return (p, kind)


@app.route("/api/queue/<qid>/archive")
def api_queue_archive(qid: str) -> Any:
    """Server-Sent Events replay of the archived transcript for queue item ``qid``.

    Returns 400 on bad-format ids and 404 when no archive exists for the
    item. Otherwise streams the archive as SSE and closes on EOF — same
    wire format as ``/stream`` so the front-end modal logic is shared.

    Two archive shapes:
      * Subagent JSONL transcript — replayed via ``_replay_jsonl``,
        emitting one ``event`` frame per parsed JSONL line.
      * Workload .output file (plain line-oriented stdout/stderr) —
        replayed via ``_replay_workload_output``, emitting one
        ``workload_line`` event per line. Same wire format as the
        live ``_tail_workload_output`` stream.
    """
    if not _QUEUE_ID_RE.match(qid):
        return jsonify({"ok": False, "error": "invalid queue id format"}), 400

    resolved = _archive_path_for(qid)
    if resolved is None:
        return (
            jsonify(
                {
                    "ok": False,
                    "error": "no archive for this queue id",
                    "id": qid,
                }
            ),
            404,
        )

    path, kind = resolved
    headers = {
        "Content-Type": "text/event-stream",
        "Cache-Control": "no-cache",
        "Connection": "keep-alive",
        "X-Accel-Buffering": "no",
    }
    if kind == "workload":
        return Response(
            stream_with_context(_replay_workload_output(path)),
            headers=headers,
            direct_passthrough=True,
        )
    return Response(
        stream_with_context(_replay_jsonl(path)),
        headers=headers,
        direct_passthrough=True,
    )


# ---------------------------------------------------------------------------
# Per-queue-item metadata endpoint (top-of-modal summary lines).
# ---------------------------------------------------------------------------
#
# The home page lists every queue item with its summary, scope, age, etc.
# The live-log modal historically only echoed the row's `summary` and
# `description` data attributes — it omitted nearly everything else the
# Claude Code curses TUI surfaces about a subagent (return text, token
# usage, runtime, dep graph, status pill, completion time, etc.).
#
# `GET /api/queue/<qid>/meta` returns ALL of that in a single JSON blob
# so the front-end can render a per-modal "Summary" header without
# coupling to the home page's row dataset.
#
# Two data sources are joined:
#
#   1. queue.json — authoritative for queue-state fields (status, scope,
#      depends_on, timestamps, summary, description, abandon_reason,
#      group_id, priority, created_by).
#
#   2. Parent-session JSONL (the harness writes a `queue-operation`
#      record of type `enqueue` with a `<task-notification>` XML
#      payload when a background subagent completes). That payload
#      carries the agent's full return text + token / tool-use /
#      duration counters. Resolved via the archived subagent JSONL's
#      `sessionId` + `agentId` fields. Best-effort: returns null when
#      the archive is gone or the parent JSONL has been rotated.

_TASK_NOTIF_RESULT_RE = re.compile(r"<result>(.*?)</result>", re.DOTALL)
_TASK_NOTIF_STATUS_RE = re.compile(r"<status>(.*?)</status>")
_TASK_NOTIF_SUMMARY_RE = re.compile(r"<summary>(.*?)</summary>")
_TASK_NOTIF_USAGE_RE = re.compile(r"<usage>(.*?)</usage>", re.DOTALL)
_TASK_NOTIF_TOKENS_RE = re.compile(r"<total_tokens>(\d+)</total_tokens>")
_TASK_NOTIF_TOOLS_RE = re.compile(r"<tool_uses>(\d+)</tool_uses>")
_TASK_NOTIF_DURATION_RE = re.compile(r"<duration_ms>(\d+)</duration_ms>")


def _extract_agent_anchor_from_archive(archive_path: Path) -> tuple[str, str] | None:
    """Read the FIRST record of an archived subagent JSONL to get (sessionId, agentId).

    The subagent transcript writes `sessionId` (parent session UUID) and
    `agentId` on every record, so the first line is enough. Cheap — we
    don't stream the whole file.

    Returns None on any parse failure / missing fields.
    """
    try:
        with open(archive_path, "r", encoding="utf-8", errors="replace") as f:
            first = f.readline()
    except OSError:
        return None
    if not first.strip():
        return None
    try:
        rec = json.loads(first)
    except (ValueError, TypeError):
        return None
    if not isinstance(rec, dict):
        return None
    session_id = rec.get("sessionId")
    agent_id = rec.get("agentId")
    if not (isinstance(session_id, str) and isinstance(agent_id, str)):
        return None
    return session_id, agent_id


def _fetch_agent_completion(
    session_id: str, agent_id: str
) -> dict[str, Any] | None:
    """Scan the parent session JSONL for the background-agent completion record.

    When an `Agent` tool call runs in `run_in_background: true` mode the
    parent's tool_result is just an "Async agent launched" stub. The
    actual return value lands LATER as a synthetic record:

        {"type": "queue-operation", "operation": "enqueue", ..., "content": "<task-notification>...</task-notification>"}

    where the `<result>` element holds the agent's final text and the
    trailing `<usage>` block holds `<total_tokens>`, `<tool_uses>`, and
    `<duration_ms>`. We linear-scan the parent JSONL looking for the
    record whose `<task-id>` matches our `agent_id`.

    Foreground (synchronous) Agent tool calls don't produce this
    record — their return text is inlined into the parent's
    `tool_result` directly. Those agents return `None` here; the meta
    endpoint then falls back to "no agent return text recorded" (the
    JSONL transcript itself stays visible in the log stream).

    Returns a dict with `return_text`, `return_status`, `summary`,
    `usage_total_tokens`, `usage_tool_uses`, `usage_duration_ms`, or
    None on miss.
    """
    if not re.match(r"^[A-Za-z0-9-]{4,64}$", agent_id):
        return None
    if not re.match(r"^[A-Za-z0-9-]{4,64}$", session_id):
        return None
    parent_path = Path(AGENTS_JSONL_ROOT) / f"{session_id}.jsonl"
    if not parent_path.is_file():
        return None
    needle = f"<task-id>{agent_id}</task-id>"
    try:
        with open(parent_path, "r", encoding="utf-8", errors="replace") as f:
            for line in f:
                # Cheap pre-filter — the per-line JSON parse is the slow
                # part, and only a handful of records match this agent.
                if needle not in line:
                    continue
                try:
                    rec = json.loads(line)
                except (ValueError, TypeError):
                    continue
                if not isinstance(rec, dict):
                    continue
                if rec.get("type") != "queue-operation":
                    continue
                if rec.get("operation") != "enqueue":
                    continue
                content = rec.get("content", "")
                if not isinstance(content, str) or needle not in content:
                    continue
                return _parse_task_notification(content)
    except OSError:
        return None
    return None


def _parse_task_notification(content: str) -> dict[str, Any]:
    """Extract the agent-return fields from a `<task-notification>` payload."""
    out: dict[str, Any] = {
        "return_text": None,
        "return_status": None,
        "result_summary": None,
        "usage_total_tokens": None,
        "usage_tool_uses": None,
        "usage_duration_ms": None,
    }
    m = _TASK_NOTIF_RESULT_RE.search(content)
    if m:
        # Unescape the only two XML entities the harness emits in
        # task-notification bodies (`&lt;` / `&gt;` / `&amp;`).
        text = m.group(1)
        text = text.replace("&lt;", "<").replace("&gt;", ">").replace("&amp;", "&")
        out["return_text"] = text.strip()
    m = _TASK_NOTIF_STATUS_RE.search(content)
    if m:
        out["return_status"] = m.group(1).strip()
    m = _TASK_NOTIF_SUMMARY_RE.search(content)
    if m:
        out["result_summary"] = m.group(1).strip()
    usage_block = _TASK_NOTIF_USAGE_RE.search(content)
    usage_str = usage_block.group(1) if usage_block else content
    m = _TASK_NOTIF_TOKENS_RE.search(usage_str)
    if m:
        try:
            out["usage_total_tokens"] = int(m.group(1))
        except ValueError:
            pass
    m = _TASK_NOTIF_TOOLS_RE.search(usage_str)
    if m:
        try:
            out["usage_tool_uses"] = int(m.group(1))
        except ValueError:
            pass
    m = _TASK_NOTIF_DURATION_RE.search(usage_str)
    if m:
        try:
            out["usage_duration_ms"] = int(m.group(1))
        except ValueError:
            pass
    return out


def _dependents_of(items: list[dict[str, Any]], qid: str) -> list[str]:
    """Return the ids of items that depend on `qid`.

    Two encodings are accepted:
      * `depends_on: ["q-..."]` — legacy explicit field
      * `scope: ["task:q-..."]` — canonical encoding since 2026-05-08
        (`--depends-on` is parser sugar that appends a `task:` scope
        token; see CLAUDE.md "Cross-queue deps unified with scope").
    """
    task_token = "task:" + qid
    out: list[str] = []
    for it in items:
        if not isinstance(it, dict):
            continue
        other = it.get("id")
        if not isinstance(other, str):
            continue
        deps = it.get("depends_on") or []
        if isinstance(deps, list) and qid in deps:
            out.append(other)
            continue
        scope = it.get("scope") or []
        if isinstance(scope, list) and any(
            isinstance(s, str) and s == task_token for s in scope
        ):
            out.append(other)
    return out


@app.route("/api/queue/<qid>/meta")
def api_queue_meta(qid: str) -> Any:
    """Return a JSON blob with full queue-item metadata + agent return-text.

    Feeds the per-modal Summary header in the front-end so the live-log
    modal shows the same top-level info the curses TUI does (status,
    scope, runtime, return text, token usage, dependents). Cheap — one
    queue.json read (cached) plus, for done items with an archived
    transcript, a single-pass scan of the parent session JSONL.
    """
    if not _QUEUE_ID_RE.match(qid):
        return jsonify({"ok": False, "error": "invalid queue id format"}), 400

    data, err = _cached_queue()
    if err is not None:
        return jsonify({"ok": False, "error": err}), 503
    items = data.get("items", []) if isinstance(data, dict) else []
    item = next(
        (it for it in items if isinstance(it, dict) and it.get("id") == qid),
        None,
    )
    if item is None:
        return jsonify({"ok": False, "error": "queue id not found"}), 404

    # Reuse the home-page shape for the queue-state fields so the
    # front-end gets consistent values (ready_now, depends_on_status,
    # has_archive, age, etc.). Then layer in the agent-return data.
    now = datetime.now(timezone.utc)
    agent_by_qid = _load_agent_state()
    shaped = _shape(item, now, agent_by_qid, items=items)

    # Runtime: started → completed (or now if still running). For
    # pending items there's no started_at yet → null.
    started = _parse_iso(item.get("registered_at") or item.get("started_at"))
    completed = _parse_iso(item.get("completed_at"))
    abandoned = _parse_iso(item.get("abandoned_at"))
    end_anchor = completed or abandoned or (now if shaped["status"] == "running" else None)
    runtime_seconds: float | None = None
    if started and end_anchor:
        runtime_seconds = max(0.0, (end_anchor - started).total_seconds())

    dependents = _dependents_of(items, qid)

    # Pull the per-agent return-value block. Only attempt when there's
    # an archive on disk — running items use the live SSE stream for
    # transcript content, and the return text doesn't exist until the
    # agent terminates.
    agent_info: dict[str, Any] | None = None
    raw_archive = item.get("log_archive_path")
    if (
        isinstance(raw_archive, str)
        and raw_archive.endswith(".jsonl")
        and "/" not in raw_archive
        and ".." not in raw_archive
    ):
        archive_path = Path(QUEUE_LOG_ARCHIVE_DIR) / raw_archive
        if archive_path.is_file():
            anchor = _extract_agent_anchor_from_archive(archive_path)
            if anchor is not None:
                session_id, agent_id = anchor
                completion = _fetch_agent_completion(session_id, agent_id)
                agent_info = {
                    "agent_id": agent_id,
                    "parent_session_id": session_id,
                }
                if completion is not None:
                    agent_info.update(completion)

    # Captured script content for workload-bound items, when the
    # workload command parsed as `<interpreter> <path>` and the
    # `workload run` CLI was able to snapshot the script at start
    # time. Items without a workload label, items whose command
    # didn't match the interpreter pattern, and pre-feature workloads
    # all yield None — the front-end omits the section entirely in
    # that case.
    script_capture: dict[str, Any] | None = None
    if shaped["workload_label"]:
        script_capture = _load_workload_script_capture(shaped["workload_label"])

    payload: dict[str, Any] = {
        "ok": True,
        "id": shaped["id"],
        "status": shaped["status"],
        "summary": shaped["summary"],
        "description": shaped["description"],
        "scope": shaped["scope"],
        "priority": shaped["priority"],
        "created_by": shaped["created_by"],
        "abandon_reason": shaped["abandon_reason"],
        "created_at": shaped["created_at_iso"],
        "started_at": shaped["started_at_iso"],
        "completed_at": shaped["completed_at_iso"],
        "abandoned_at": shaped["abandoned_at_iso"],
        "group_id": shaped["group_id"],
        "group_head": shaped["group_head"],
        "ready_now": shaped["ready_now"],
        "depends_on": shaped["depends_on"],
        "depends_on_status": shaped["depends_on_status"],
        "dependents": dependents,
        "has_archive": shaped["has_archive"],
        "is_starting": shaped["is_starting"],
        "workload_label": shaped["workload_label"],
        "age": shaped["age"],
        "age_label": shaped["age_label"],
        "runtime_seconds": runtime_seconds,
        "agent": agent_info,
        "script_capture": script_capture,
    }
    if shaped.get("owner") is not None:
        payload["owner"] = shaped["owner"]
    return jsonify(payload)


@app.route("/healthz")
def healthz() -> Any:
    return jsonify({"ok": True, "ts": time.time()})


if __name__ == "__main__":
    app.run(host="0.0.0.0", port=8000, debug=False)
