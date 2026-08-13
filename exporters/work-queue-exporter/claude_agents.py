"""claude_agents — shared helpers for Claude Code subagent identification.

A single source of truth for "what is an agent_id" and "how do I look up
liveness for a queue item's owning agent" across the Python tools that
need to consume `claude-watch active-agents` output:

  - work-queue-exporter       (claude-watch/exporters/work-queue-exporter/)
  - queue-minisite            (claude-watch/queue-minisite/)
  - any external agent-message / cron-queue-check tooling that needs
    to enrich queue rows with the owning agent's liveness state.

Canonical agent_id format: the JSONL filename stem WITHOUT the `agent-`
prefix and without the `.jsonl` suffix. Example:

  ~/.claude/projects/-home-hndrewaall/<session>/subagents/agent-ac9e993a105a6ef41.jsonl
                                                             ^^^^^^^^^^^^^^^^^^
                                                             this is `agent_id`

The same identifier is used by:

  - claude-watch active-agents JSON (`agents[].agent_id`, `agent-` stripped)
  - claude-watch agent list / agent-ctl (`agent-` stripped via load_agents)
  - agent-msg inbox file path (~/.config/claude/agent-inbox/<agent_id>.json)
  - agent-msg index entries

Functions:

  load_agent_state(path)
      Read the JSON written by `claude-watch active-agents --write-state`.
      Returns the parsed dict (always has `subagents`/`workloads`/`agents`
      keys, even on failure — empty arrays).

  agents_by_queue_id(state)
      Build a queue_id -> agent record map. Dedup rule when multiple
      agents reference the same queue_id (rare — happens after a retry):
      live > stale; among same liveness, smaller jsonl_age_seconds wins.

  agent_for_queue(state, queue_id)
      Convenience: load+lookup. Returns None if not found.

This module is INTENTIONALLY pure-Python with NO third-party deps so it
vendors cleanly into Docker images that don't get a full uv venv. Stick
to stdlib.
"""

from __future__ import annotations

import json
import re
from typing import Any, Optional

DEFAULT_AGENT_STATE_PATH = "/var/lib/claude-watch/active-agents.json"


def load_agent_state(path: str = DEFAULT_AGENT_STATE_PATH) -> dict[str, Any]:
    """Read claude-watch's active-agents JSON state file.

    Returns a dict with keys `subagents`, `workloads`, `agents` (always
    present, defaulting to empty lists). Failures (missing file, parse
    error) yield the empty-shape dict so callers can treat the file as
    "no signal" without try/except.
    """
    empty = {"subagents": [], "workloads": [], "agents": []}
    try:
        with open(path, "r") as f:
            data = json.load(f)
    except (OSError, json.JSONDecodeError):
        return empty
    if not isinstance(data, dict):
        return empty
    # Normalize missing keys.
    return {
        "subagents": list(data.get("subagents") or []),
        "workloads": list(data.get("workloads") or []),
        "agents": list(data.get("agents") or []),
    }


def agents_by_queue_id(state: dict[str, Any]) -> dict[str, dict[str, Any]]:
    """Map queue_id -> agent record from a loaded state dict.

    Dedup rule when the same queue_id appears on multiple records:
      1. live > stale
      2. among same liveness, smaller jsonl_age_seconds wins
      3. if both have age=None and same liveness, first-seen wins

    Records without a queue_id are skipped.
    """
    by_qid: dict[str, dict[str, Any]] = {}
    for rec in state.get("agents", []):
        if not isinstance(rec, dict):
            continue
        qid = rec.get("queue_id")
        if not qid:
            continue
        prev = by_qid.get(qid)
        if prev is None:
            by_qid[qid] = rec
            continue
        prev_alive = bool(prev.get("alive"))
        rec_alive = bool(rec.get("alive"))
        if rec_alive and not prev_alive:
            by_qid[qid] = rec
            continue
        if rec_alive == prev_alive:
            prev_age = prev.get("jsonl_age_seconds")
            rec_age = rec.get("jsonl_age_seconds")
            if (
                rec_age is not None
                and (prev_age is None or rec_age < prev_age)
            ):
                by_qid[qid] = rec
    return by_qid


def agent_for_queue(
    queue_id: str,
    path: str = DEFAULT_AGENT_STATE_PATH,
) -> Optional[dict[str, Any]]:
    """One-shot helper: load state file, return the record for `queue_id`."""
    state = load_agent_state(path)
    return agents_by_queue_id(state).get(queue_id)


def agents_by_agent_id(state: dict[str, Any]) -> dict[str, dict[str, Any]]:
    """Map agent_id -> agent record from a loaded state dict.

    Companion to ``agents_by_queue_id``. Where the queue-id map answers
    "which agent owns this queue item (by the transcript-parsed marker)",
    this map answers "what is the liveness of THIS agent_id" -- the join
    needed to resolve an owner discovered via the arm-hook bindings file
    (agent_id -> queue_id), whose agent may be keyed in active-agents under
    a DIFFERENT (original-spawn) queue id than the one we are asking about.

    Dedup rule mirrors ``agents_by_queue_id``: live > stale; among same
    liveness, smaller jsonl_age_seconds wins. Records without an agent_id
    are skipped.
    """
    by_aid: dict[str, dict[str, Any]] = {}
    for rec in state.get("agents", []):
        if not isinstance(rec, dict):
            continue
        aid = rec.get("agent_id")
        if not aid:
            continue
        prev = by_aid.get(aid)
        if prev is None:
            by_aid[aid] = rec
            continue
        prev_alive = bool(prev.get("alive"))
        rec_alive = bool(rec.get("alive"))
        if rec_alive and not prev_alive:
            by_aid[aid] = rec
            continue
        if rec_alive == prev_alive:
            prev_age = prev.get("jsonl_age_seconds")
            rec_age = rec.get("jsonl_age_seconds")
            if rec_age is not None and (prev_age is None or rec_age < prev_age):
                by_aid[aid] = rec
    return by_aid


_QUEUE_ID_RE = re.compile(r"^q-[a-z0-9-]{4,64}$")


def load_agent_queue_bindings(path: str) -> dict[str, str]:
    """Map queue_id -> agent_id from the arm-hook bindings file.

    ``post-tool-agent-arm-hook`` (PostToolUse:Agent) writes
    ``{"bindings": {"<agent_id>": {"queue_id": "q-XXXX",
    "registered_at": <epoch>, ...}}}`` the instant the main loop spawns an
    Agent -- BEFORE claude-watch's active-agents poller (60s cadence) has
    published a transcript-derived record for it. It is therefore the
    earliest AND most authoritative owner signal: an item carrying a
    binding is definitively OWNED even while active-agents shows no record
    keyed under its queue id (spawn-to-poll lag, or a SendMessage-rotated
    queue id whose transcript marker still points at the original id).

    Returns a queue_id -> agent_id map. When several agents bound the same
    queue id over the item's life (a re-register / retry), the NEWEST
    binding (largest ``registered_at``) wins -- that is the current owner.
    Fail-soft: missing file, unreadable, bad JSON, or an unexpected shape
    all yield ``{}`` so a missing mount degrades to the legacy behaviour.
    """
    try:
        with open(path, "r") as f:
            data = json.load(f)
    except (OSError, json.JSONDecodeError):
        return {}
    if not isinstance(data, dict):
        return {}
    bindings = data.get("bindings")
    if not isinstance(bindings, dict):
        return {}
    best: dict[str, tuple[float, str]] = {}
    for aid, rec in bindings.items():
        if not isinstance(aid, str) or not aid:
            continue
        if isinstance(rec, dict):
            qid = rec.get("queue_id")
            reg = rec.get("registered_at")
        elif isinstance(rec, str):
            qid, reg = rec, None
        else:
            continue
        if not isinstance(qid, str) or not _QUEUE_ID_RE.match(qid):
            continue
        try:
            reg_f = float(reg) if reg is not None else 0.0
        except (TypeError, ValueError):
            reg_f = 0.0
        prev = best.get(qid)
        if prev is None or reg_f >= prev[0]:
            best[qid] = (reg_f, aid)
    return {qid: aid for qid, (_reg, aid) in best.items()}
