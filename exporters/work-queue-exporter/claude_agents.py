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

  read_json_input(path)
      Read + parse a JSON input file, classifying WHY it yielded nothing
      (`ok` / `missing` / `unreadable` / `malformed`). Callers that must
      distinguish "the file says there are no agents" from "I cannot see
      the file at all" use this instead of the fail-soft loaders below —
      a container that never got the bind mount looks IDENTICAL to an
      idle fleet otherwise, and reporting every running item as orphaned
      on a missing mount is the failure mode that costs the most trust.

  load_agent_state(path) / load_agent_state_status(path)
      Read the JSON written by `claude-watch active-agents --write-state`.
      Returns the parsed dict (always has `subagents`/`workloads`/`agents`
      keys, even on failure — empty arrays). The `_status` variant also
      returns the `read_json_input` status so a caller can tell an empty
      fleet from an unreadable file.

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

# `read_json_input` status values. `INPUT_OK` is the ONLY one that means
# "this input is a trustworthy source of truth right now"; every other
# value means the caller is flying blind on that input and must NOT
# convert its emptiness into a negative assertion.
INPUT_OK = "ok"
INPUT_MISSING = "missing"
INPUT_UNREADABLE = "unreadable"
INPUT_MALFORMED = "malformed"


def read_json_input(path: str) -> tuple[Any, str]:
    """Read + parse a JSON file, returning ``(data, status)``.

    ``status`` is one of ``INPUT_OK`` / ``INPUT_MISSING`` /
    ``INPUT_UNREADABLE`` / ``INPUT_MALFORMED``; ``data`` is ``None``
    unless the status is ``INPUT_OK``.

    The fail-soft loaders below collapse every failure into an empty
    result, which is right for rendering but WRONG for alerting: an
    unmounted path and a genuinely idle fleet produce the identical
    empty dict. Anything that turns "no record" into "orphaned" needs
    this distinction.
    """
    try:
        with open(path, "r") as f:
            raw = f.read()
    except FileNotFoundError:
        return None, INPUT_MISSING
    except OSError:
        return None, INPUT_UNREADABLE
    try:
        return json.loads(raw), INPUT_OK
    except (ValueError, TypeError):
        return None, INPUT_MALFORMED


def load_agent_state_status(
    path: str = DEFAULT_AGENT_STATE_PATH,
) -> tuple[dict[str, Any], str]:
    """``load_agent_state`` + the `read_json_input` status of the file.

    A file that parses but is not a JSON object reports
    ``INPUT_MALFORMED`` — the shape is as unusable as a parse error.
    """
    empty = {"subagents": [], "workloads": [], "agents": []}
    data, status = read_json_input(path)
    if status != INPUT_OK:
        return empty, status
    if not isinstance(data, dict):
        return empty, INPUT_MALFORMED
    # Normalize missing keys.
    return {
        "subagents": list(data.get("subagents") or []),
        "workloads": list(data.get("workloads") or []),
        "agents": list(data.get("agents") or []),
    }, INPUT_OK


def load_agent_state(path: str = DEFAULT_AGENT_STATE_PATH) -> dict[str, Any]:
    """Read claude-watch's active-agents JSON state file.

    Returns a dict with keys `subagents`, `workloads`, `agents` (always
    present, defaulting to empty lists). Failures (missing file, parse
    error) yield the empty-shape dict so callers can treat the file as
    "no signal" without try/except. Use `load_agent_state_status` when
    "no signal" and "no agents" must be told apart.
    """
    return load_agent_state_status(path)[0]


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
    Use ``load_agent_queue_bindings_status`` when a caller must tell a
    missing mount apart from a genuinely empty bindings file.
    """
    return load_agent_queue_bindings_status(path)[0]


def load_agent_queue_bindings_status(path: str) -> tuple[dict[str, str], str]:
    """``load_agent_queue_bindings`` + the file's `read_json_input` status.

    A parsed file whose top level is not an object, or whose ``bindings``
    key is not an object, reports ``INPUT_MALFORMED``: the file exists but
    carries no usable owner relation, which is a deployment fault, not an
    empty fleet. A well-formed file with zero bindings is ``INPUT_OK`` —
    "nothing is currently bound" is real, trustworthy signal.
    """
    data, status = read_json_input(path)
    if status != INPUT_OK:
        return {}, status
    if not isinstance(data, dict):
        return {}, INPUT_MALFORMED
    bindings = data.get("bindings")
    if not isinstance(bindings, dict):
        return {}, INPUT_MALFORMED
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
    return {qid: aid for qid, (_reg, aid) in best.items()}, INPUT_OK
