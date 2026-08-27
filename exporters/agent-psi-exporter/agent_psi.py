"""Agent PSI — pressure-stall metrics for a Claude Code agent fleet.

Pure, stdlib-only parsing + math library behind ``agent_psi_exporter.py``
(the sibling Prometheus exporter in this directory). Importable by the
exporter and by the test suite; it touches no globals and does no I/O beyond
``read_transcript`` reading one file, so every classifier / pressure case is
unit-testable against synthetic transcripts.

WHAT PSI IS, APPLIED TO AGENTS
------------------------------
Linux Pressure Stall Information asks "how much productive work is lost to
waiting on a resource", reporting two tracks per resource:

* ``some`` — fraction of wall-time in which AT LEAST ONE task is stalled on
  the resource (a latency signal).
* ``full`` — fraction in which EVERY non-idle task is stalled at once, so no
  work advances (a throughput-loss signal).

A Claude Code agent's turn loop is, at any instant, blocked on exactly one
thing, so it is a serial state machine. We map its states onto categories:

* ``inference`` — the model is generating the next turn (network RTT folded
  in; deliberately NOT split from inference).
* ``tool``      — a ``tool_use`` block is executing (Bash / Read / MCP /
  sub-agent) up to its matching ``tool_result``.
* ``idle``      — the loop is parked waiting on an external event / debounce.
* ``waiting_human`` — the loop is blocked on a human reply.
* ``overhead``  — the loop's own between-turn bookkeeping.

Pressure is computed over ACTIVE wall-time only = total − idle −
waiting_human. ``overhead`` counts as productive-self, not a stall, so it is
IN the active denominator but is NOT one of the stall categories we compute
some/full for. This mirrors PSI's rule of not counting time nobody wanted to
run.

SCOPES
------
some/full form a hierarchy:

* single agent — serial, so ``some == full == its duty-cycle`` for a
  category. Emitted implicitly via the per-agent duty-cycle gauges.
* subtree — an agent plus its live sub-agents. Here we scope it to a session:
  the main-loop transcript plus every live sub-agent transcript under that
  session. ``some``/``full`` diverge once >1 member is live.
* fleet — every live transcript across all sessions. Fleet ``full`` on
  inference is the money metric: every live agent stalled on the model at
  once means we are API / rate-limit bound and more parallelism buys nothing.

CLASSIFIER (deliberately rough — phase 1)
-----------------------------------------
We reconstruct intervals from transcript "moments" (assistant turns, tool
results, human prompts, and any other line as bookkeeping), then classify
each gap by what BOUNDS its end:

* gap ending at an assistant turn  -> ``inference`` (the model produced it).
* gap ending at a tool_result      -> ``tool`` (a tool ran during the gap).
* gap ending at a human prompt      -> ``waiting_human`` if the loop had just
  produced output, else ``idle``.
* gap ending at a bookkeeping line   -> ``overhead``.

A gap longer than ``MAX_GAP_SECONDS`` that does NOT end at a tool_result is
reclassified ``idle`` (a dormant / resumed-session gap should not read as a
multi-minute inference stall; a genuinely long tool — a slow build — DOES end
at a tool_result and stays ``tool``, uncapped). A trailing open interval from
the last moment to ``now`` captures the agent's CURRENT state so the live
fleet PSI reflects "are we blocked right now".

This is a first-cut classifier, refined against the live Grafana series, not
a perfect offline model.
"""

from __future__ import annotations

import json
import os
from collections import namedtuple

# --- categories ----------------------------------------------------------
INFERENCE = "inference"
TOOL = "tool"
IDLE = "idle"
WAITING_HUMAN = "waiting_human"
OVERHEAD = "overhead"

CATEGORIES = (INFERENCE, TOOL, IDLE, WAITING_HUMAN, OVERHEAD)
# Categories that count as an agent "wanting to run" (the PSI denominator and
# the set that can make a full-pressure slice). idle / waiting_human are the
# time nobody wanted to run and are excluded.
ACTIVE_CATEGORIES = (INFERENCE, TOOL, OVERHEAD)
# Categories we compute some/full pressure for.
STALL_CATEGORIES = (INFERENCE, TOOL)

# A gap longer than this that does not end at a tool_result is treated as
# idle rather than a stall (dormant / resumed-session gap). Tool gaps are
# never capped — a long build is real tool pressure.
DEFAULT_MAX_GAP_SECONDS = 300.0
# A transcript not written within this window is not a live agent and is
# excluded from fleet/subtree pressure (matches cw-agent-stats' notion).
DEFAULT_LIVE_WINDOW_SECONDS = 900.0

# Decaying-window sizes (seconds) the exporter emits pressure for. Phase 1
# uses fixed sliding windows ending at ``now``; a true exponential decay is a
# phase-2 refinement.
DEFAULT_WINDOWS = (10, 60, 300)

Interval = namedtuple("Interval", ("start", "end", "category"))
# A transcript's parsed intervals plus identity/liveness metadata.
Transcript = namedtuple(
    "Transcript",
    ("agent_id", "session_id", "is_main_loop", "mtime", "intervals"),
)

# Internal moment: one timestamped point on the transcript timeline.
_Moment = namedtuple("_Moment", ("ts", "kind", "stop_reason", "tool_use_ids"))
_ASST = "asst"
_RESULT = "result"
_PROMPT = "prompt"
_BOOK = "book"


def parse_ts(value):
    """ISO-8601 (with a trailing ``Z``) -> epoch seconds, or None."""
    if not value or not isinstance(value, str):
        return None
    try:
        from datetime import datetime

        return datetime.fromisoformat(value.replace("Z", "+00:00")).timestamp()
    except (ValueError, TypeError):
        return None


def _content_blocks(entry):
    msg = entry.get("message")
    if not isinstance(msg, dict):
        return []
    content = msg.get("content")
    return content if isinstance(content, list) else []


def _entry_to_moment(entry):
    """Map one parsed JSONL entry to a timeline moment, or None to skip."""
    ts = parse_ts(entry.get("timestamp"))
    if ts is None:
        return None
    etype = entry.get("type")

    if etype == "assistant":
        stop = None
        msg = entry.get("message")
        if isinstance(msg, dict):
            stop = msg.get("stop_reason")
        tu_ids = [
            b.get("id")
            for b in _content_blocks(entry)
            if isinstance(b, dict) and b.get("type") == "tool_use"
        ]
        return _Moment(ts, _ASST, stop, [i for i in tu_ids if i])

    if etype == "user":
        blocks = _content_blocks(entry)
        is_result = entry.get("toolUseResult") is not None or any(
            isinstance(b, dict) and b.get("type") == "tool_result" for b in blocks
        )
        if is_result:
            return _Moment(ts, _RESULT, None, [])
        # A real human/injected prompt (skip Claude Code's meta user lines).
        if not entry.get("isMeta"):
            return _Moment(ts, _PROMPT, None, [])
        return _Moment(ts, _BOOK, None, [])

    # Any other line (system / queue-operation / attachment / ...) is the
    # loop's own bookkeeping.
    return _Moment(ts, _BOOK, None, [])


def _all_result_ids(entries):
    ids = set()
    for e in entries:
        if e.get("type") != "user":
            continue
        for b in _content_blocks(e):
            if isinstance(b, dict) and b.get("type") == "tool_result":
                tid = b.get("tool_use_id")
                if tid:
                    ids.add(tid)
    return ids


def _classify_gap(prev, cur, max_gap):
    """Category for the interval [prev.ts, cur.ts], bounded by ``cur``."""
    d = cur.ts - prev.ts
    if cur.kind == _RESULT:
        # A tool ran during the gap — real tool time, never capped.
        return TOOL
    if d > max_gap:
        # Dormant / resumed-session gap; not a multi-minute inference stall.
        return IDLE
    if cur.kind == _ASST:
        return INFERENCE
    if cur.kind == _PROMPT:
        return WAITING_HUMAN if prev.kind == _ASST else IDLE
    return OVERHEAD  # _BOOK


def _tail_interval(last, now, pending_tools, max_gap):
    """Open interval from the last moment to ``now`` = the agent's current
    state, or None if the agent is dormant / ``now`` precedes it."""
    if now is None or now <= last.ts:
        return None
    end = now
    if now - last.ts > max_gap:
        # Dormant since the last event — call it idle up to the cap, no more.
        return Interval(last.ts, min(now, last.ts + max_gap), IDLE)
    if last.kind == _ASST:
        if pending_tools:
            category = TOOL  # tools dispatched, results not yet in.
        elif last.stop_reason == "end_turn":
            category = IDLE  # returned control; waiting.
        else:
            category = INFERENCE
    elif last.kind == _RESULT:
        category = INFERENCE  # model generating the next turn.
    elif last.kind == _PROMPT:
        category = INFERENCE
    else:
        category = OVERHEAD
    return Interval(last.ts, end, category)


def parse_intervals(entries, now=None, max_gap=DEFAULT_MAX_GAP_SECONDS):
    """Ordered JSONL entries (dicts) -> list[Interval].

    ``entries`` need not be sorted; moments are sorted by timestamp. When
    ``now`` is given a trailing open interval is appended for the agent's
    current state (what makes the live fleet PSI reflect the present).
    """
    moments = [m for m in (_entry_to_moment(e) for e in entries) if m is not None]
    moments.sort(key=lambda m: m.ts)
    if not moments:
        return []

    intervals = []
    for prev, cur in zip(moments, moments[1:]):
        if cur.ts <= prev.ts:
            continue
        intervals.append(Interval(prev.ts, cur.ts, _classify_gap(prev, cur, max_gap)))

    result_ids = _all_result_ids(entries)
    last = moments[-1]
    pending = [i for i in last.tool_use_ids if i not in result_ids]
    tail = _tail_interval(last, now, pending, max_gap)
    if tail is not None and tail.end > tail.start:
        intervals.append(tail)
    return intervals


# --- duty cycle ----------------------------------------------------------
def duty_seconds(intervals):
    """Seconds accumulated per category over a set of intervals."""
    secs = {c: 0.0 for c in CATEGORIES}
    for iv in intervals:
        if iv.category in secs:
            secs[iv.category] += max(0.0, iv.end - iv.start)
    return secs


def duty_cycle(intervals):
    """Return (per_category_seconds, total, active, ratios).

    ``active`` = total − idle − waiting_human. ``ratios`` gives, for each
    ACTIVE category (inference/tool/overhead), its share of active time — the
    per-agent duty-cycle (and, for a serial agent, its some==full pressure).
    """
    secs = duty_seconds(intervals)
    total = sum(secs.values())
    active = total - secs[IDLE] - secs[WAITING_HUMAN]
    ratios = {}
    if active > 0:
        for c in ACTIVE_CATEGORIES:
            ratios[c] = secs[c] / active
    else:
        for c in ACTIVE_CATEGORIES:
            ratios[c] = 0.0
    return secs, total, active, ratios


# --- pressure (some / full) ---------------------------------------------
def _state_at(intervals, t):
    """Category of the interval covering instant ``t`` (start<=t<end), or
    None if no interval covers it."""
    for iv in intervals:
        if iv.start <= t < iv.end:
            return iv.category
    return None


def compute_pressure(agent_intervals, window_start, window_end):
    """some/full pressure per stall category over [window_start, window_end].

    ``agent_intervals`` maps agent_id -> list[Interval]. Returns a dict keyed
    by (category, kind) with kind in {"some", "full"} -> ratio in [0, 1].

    Exact via a boundary sweep: between consecutive interval boundaries every
    agent's state is constant, so we sample the midpoint of each sub-slice.
    ``some`` accumulates a slice when >=1 agent is in the category; ``full``
    when there is >=1 ACTIVE agent and every active agent is in the category
    (idle / waiting_human / absent agents don't count as active, matching
    PSI's "every non-idle task stalled").
    """
    W = window_end - window_start
    acc = {(c, k): 0.0 for c in STALL_CATEGORIES for k in ("some", "full")}
    if W <= 0:
        return {k: 0.0 for k in acc}

    points = {window_start, window_end}
    for ivs in agent_intervals.values():
        for iv in ivs:
            if iv.end <= window_start or iv.start >= window_end:
                continue
            points.add(max(iv.start, window_start))
            points.add(min(iv.end, window_end))
    points = sorted(p for p in points if window_start <= p <= window_end)

    for a, b in zip(points, points[1:]):
        d = b - a
        if d <= 0:
            continue
        mid = (a + b) / 2.0
        active_states = []
        for ivs in agent_intervals.values():
            cat = _state_at(ivs, mid)
            if cat in ACTIVE_CATEGORIES:
                active_states.append(cat)
        for c in STALL_CATEGORIES:
            if any(s == c for s in active_states):
                acc[(c, "some")] += d
            if active_states and all(s == c for s in active_states):
                acc[(c, "full")] += d

    return {k: v / W for k, v in acc.items()}


# --- transcript discovery + reading -------------------------------------
def _agent_id_from_path(path):
    base = os.path.basename(path)
    if base.startswith("agent-") and base.endswith(".jsonl"):
        return base[len("agent-"):-len(".jsonl")]
    return None


def read_transcript(path, now=None, max_gap=DEFAULT_MAX_GAP_SECONDS,
                    is_main_loop=False, session_id=None):
    """Read one transcript file -> Transcript, tolerant of malformed lines.

    Returns None if the file can't be read at all. Individual bad JSONL lines
    are skipped (a live file's last line can be a partial write).
    """
    try:
        mtime = os.stat(path).st_mtime
        entries = []
        with open(path, "r") as fh:
            for line in fh:
                line = line.strip()
                if not line:
                    continue
                try:
                    entries.append(json.loads(line))
                except json.JSONDecodeError:
                    continue
    except OSError:
        return None

    if is_main_loop:
        agent_id = "main_loop:" + (session_id or "")[:8]
    else:
        agent_id = _agent_id_from_path(path) or os.path.basename(path)
    intervals = parse_intervals(entries, now=now, max_gap=max_gap)
    return Transcript(agent_id, session_id, is_main_loop, mtime, intervals)


def discover_transcripts(projects_dir):
    """Yield (path, session_id, is_main_loop) for every session transcript and
    every ``*/subagents/agent-*.jsonl`` under ``projects_dir``.

    Layout (Claude Code): ``<projects_dir>/<slug>/<session>.jsonl`` is a main
    loop; ``<projects_dir>/<slug>/<session>/subagents/agent-<id>.jsonl`` are
    its sub-agents.
    """
    try:
        slugs = list(os.scandir(projects_dir))
    except OSError:
        return
    for slug in slugs:
        if not slug.is_dir(follow_symlinks=False):
            continue
        try:
            slug_entries = list(os.scandir(slug.path))
        except OSError:
            continue
        for entry in slug_entries:
            if entry.is_file(follow_symlinks=False) and entry.name.endswith(".jsonl"):
                session_id = entry.name[: -len(".jsonl")]
                yield entry.path, session_id, True
            elif entry.is_dir(follow_symlinks=False):
                session_id = entry.name
                sub = os.path.join(entry.path, "subagents")
                try:
                    sub_entries = list(os.scandir(sub))
                except OSError:
                    continue
                for s in sub_entries:
                    if (
                        s.is_file(follow_symlinks=False)
                        and s.name.startswith("agent-")
                        and s.name.endswith(".jsonl")
                    ):
                        yield s.path, session_id, False


def collect_live_transcripts(projects_dir, now, max_gap=DEFAULT_MAX_GAP_SECONDS,
                             live_window=DEFAULT_LIVE_WINDOW_SECONDS):
    """Read every transcript whose file was modified within ``live_window`` of
    ``now``. Returns list[Transcript]."""
    live = []
    for path, session_id, is_main in discover_transcripts(projects_dir):
        try:
            mtime = os.stat(path).st_mtime
        except OSError:
            continue
        if now - mtime > live_window:
            continue
        t = read_transcript(
            path, now=now, max_gap=max_gap,
            is_main_loop=is_main, session_id=session_id,
        )
        if t is not None:
            live.append(t)
    return live
