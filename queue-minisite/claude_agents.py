"""claude_agents — shared helpers for Claude Code subagent identification.

A single source of truth for "what is an agent_id" and "how do I look up
liveness for a queue item's owning agent" across the Python tools that
need to consume `claude-watch active-agents` output (queue-minisite,
work-queue-exporter, agent-msg, cron-queue-check, etc.).

Canonical agent_id format: the JSONL filename stem WITHOUT the `agent-`
prefix and without the `.jsonl` suffix. Example:

  ~/.claude/projects/<project-slug>/<session>/subagents/agent-ac9e993a105a6ef41.jsonl
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

  agents_by_queue_id(state, parent_of=None)
      Build a queue_id -> OWNING agent record map. Several agents share a
      queue id whenever an agent spawns subagents (they inherit its
      ``Queue item:`` marker), and after a retry. Selection: a spawn
      DESCENDANT of another candidate is never the owner (needs
      ``parent_of``, the child -> parent spawn graph); then live > stale;
      then smaller jsonl_age_seconds wins.

  agent_records_by_queue_id(state)
      The un-collapsed queue_id -> [records] view, for callers that need
      to know a queue id is contested before paying to build ``parent_of``.

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


def agents_by_queue_id(
    state: dict[str, Any],
    parent_of: Optional[dict[str, str]] = None,
) -> dict[str, dict[str, Any]]:
    """Map queue_id -> the OWNING agent record from a loaded state dict.

    Several records can carry the same queue id, because the agent ->
    queue mapping is derived from the ``Queue item: q-XXXX`` marker in
    each transcript's first user message and a subagent INHERITS that
    line from the prompt of the agent that spawned it. So an agent and
    every agent it spawns are all recorded under ONE queue id.

    Selection rule:
      0. an agent that is a SPAWN DESCENDANT of another candidate for the
         same queue id is never the owner (requires ``parent_of``)
      1. live > stale
      2. among same liveness, smaller jsonl_age_seconds wins
      3. if both have age=None and same liveness, first-seen wins

    Rules 1-3 alone pick whichever co-bound agent happens to have written
    most recently at snapshot time. For a parent and its own children that
    is a race the parent LOSES whenever it is parked inside a long tool
    call (or simply waiting on those children) — so an item's owner flips,
    mid-run, to one of its own subagents. Rule 0 is what stops that: pass
    ``parent_of`` (``child_agent_id -> parent_agent_id``, reconstructed
    from the transcripts' Agent/Task launch records) and any candidate
    reachable UP that graph to another candidate is dropped before the
    liveness/freshness tiebreak runs. Callers with no spawn-graph signal
    omit it and keep the historical behaviour.

    Records without a queue_id are skipped.
    """
    by_qid: dict[str, dict[str, Any]] = {}
    for qid, records in agent_records_by_queue_id(state).items():
        owner = pick_owner_record(records, parent_of)
        if owner is not None:
            by_qid[qid] = owner
    return by_qid


def agent_records_by_queue_id(
    state: dict[str, Any],
) -> dict[str, list[dict[str, Any]]]:
    """Map queue_id -> EVERY agent record carrying it, in file order.

    The un-collapsed view behind ``agents_by_queue_id``. A queue id with
    more than one record is a queue item whose agents are contested: the
    owner plus, usually, the subagents it spawned (which inherit its
    ``Queue item:`` marker). Callers use this to detect that case cheaply
    before paying for a spawn-graph reconstruction.
    """
    out: dict[str, list[dict[str, Any]]] = {}
    for rec in state.get("agents", []):
        if not isinstance(rec, dict):
            continue
        qid = rec.get("queue_id")
        if not qid:
            continue
        out.setdefault(qid, []).append(rec)
    return out


def _is_spawn_descendant_of_any(
    agent_id: str,
    candidate_ids: set[str],
    parent_of: dict[str, str],
) -> bool:
    """True when walking ``agent_id``'s spawn parents reaches a candidate.

    Cycle-guarded (a child has exactly one parent on disk, so a cycle can
    only come from a corrupt map). An empty / missing parent link ends the
    walk with False.
    """
    seen = {agent_id}
    cur = agent_id
    while True:
        parent = parent_of.get(cur)
        if not parent or parent in seen:
            return False
        if parent in candidate_ids:
            return True
        seen.add(parent)
        cur = parent


def _outranks(rec: dict[str, Any], best: dict[str, Any]) -> bool:
    """Liveness/freshness comparison: live > stale, then youngest jsonl."""
    best_alive = bool(best.get("alive"))
    rec_alive = bool(rec.get("alive"))
    if rec_alive and not best_alive:
        return True
    if rec_alive != best_alive:
        return False
    best_age = best.get("jsonl_age_seconds")
    rec_age = rec.get("jsonl_age_seconds")
    return rec_age is not None and (best_age is None or rec_age < best_age)


def pick_owner_record(
    records: list[dict[str, Any]],
    parent_of: Optional[dict[str, str]] = None,
) -> Optional[dict[str, Any]]:
    """Choose the owning agent record among records sharing one queue id.

    See ``agents_by_queue_id`` for the rule set. ``parent_of`` is the
    spawn graph (``child -> parent``); when it is supplied and some (but
    not all) candidates are descendants of other candidates, only the
    spawn ROOTS are eligible. Falls back to the whole set if every
    candidate is a descendant (a corrupt/cyclic map), so this can never
    return None for a non-empty input.
    """
    recs = [r for r in records if isinstance(r, dict)]
    if not recs:
        return None
    if parent_of and len(recs) > 1:
        candidate_ids = {
            str(r.get("agent_id")) for r in recs if r.get("agent_id")
        }
        roots = [
            r
            for r in recs
            if not _is_spawn_descendant_of_any(
                str(r.get("agent_id") or ""), candidate_ids, parent_of
            )
        ]
        if roots:
            recs = roots
    best = recs[0]
    for rec in recs[1:]:
        if _outranks(rec, best):
            best = rec
    return best


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
