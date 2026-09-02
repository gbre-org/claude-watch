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
waiting_human. This mirrors PSI's rule of not counting time nobody wanted to
run. ``overhead`` counts as productive-self, not a stall — it is never one of
the STALL categories — but it does get its own some/full pair, because
inference + tool + overhead partition active time: with all three emitted a
fleet panel accounts for every active second instead of leaving the remainder
implicit.

SCOPES
------
some/full form a hierarchy:

* single agent — serial, so ``some == full == its duty-cycle`` for a
  category. Emitted implicitly via the per-agent duty-cycle gauges.
* subtree — an agent plus its live sub-agents. Here we scope it to a session:
  the main-loop transcript plus every live sub-agent transcript under that
  session. ``some``/``full`` diverge once >1 member is live.
* fleet — every live SUB-AGENT transcript across all sessions. The main loop is
  a dispatcher, mostly parked between turns; blending its idle-heavy profile
  into the worker fleet pollutes the signal, so it is EXCLUDED from ``fleet``
  and reported under its own ``main`` scope side-by-side. Fleet ``full`` on
  inference is the money metric: every live worker stalled on the model at once
  means we are API / rate-limit bound and more parallelism buys nothing. Fleet
  pressure is also computed per model family (the same math restricted to the
  workers on that model), so a single model's rate-limiting is isolatable.

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

A closed gap longer than ``MAX_GAP_SECONDS`` that does NOT end at a tool_result
is reclassified ``idle`` (a dormant / resumed-session gap should not read as a
multi-minute inference stall; a genuinely long tool — a slow build — DOES end
at a tool_result and stays ``tool``, uncapped). A trailing open interval from
the last moment to ``now`` captures the agent's CURRENT state so the live fleet
PSI reflects "are we blocked right now". That trailing state is decided first,
then the cap applies only to an inference/overhead tail: a parked-idle tail (a
dispatcher between turns) and an in-flight-tool tail (a foreground blocking Bash
wait) are counted at their TRUE wall-clock length, never truncated — capping
the idle tail was the main-loop idle undercount. An inference tail is likewise
uncapped while it reads as an API stall (see the next-but-one section).

This is a first-cut classifier, refined against the live Grafana series, not
a perfect offline model.

PRODUCTIVE vs STALLED INFERENCE (throughput split)
--------------------------------------------------
An ``inference`` interval only tells us the loop was blocked on the model — it
cannot, on wall-time alone, tell steady token generation apart from a stall
(429 back-off, network / TTFT latency, server-side queueing). But the assistant
entry that BOUNDS the end of a closed inference gap carries
``message.usage.output_tokens`` (which already folds in
``output_tokens_details.thinking_tokens`` — extended thinking is productive
output), so for that gap we can compute an effective throughput:

    tok/s = output_tokens(of the turn that ended the gap) / gap_duration

A gap whose throughput is far BELOW a normal generation rate means the
wall-time was mostly stall, not generation, and we tag the interval
``stalled``. Guards: gaps shorter than ``MIN_STALL_GAP_SECONDS`` are never
tagged (a 1s / 100-token gap is obviously productive and TTFT dominates a short
gap), and a gap with no ``output_tokens`` datum is left un-tagged (we do not cry
wolf without evidence).

``stalled`` is a SUBSET flag on ``inference`` intervals, never a new category:
the existing ``inference_some``/``inference_full`` remain the TOTAL inference
pressure, and ``compute_stalled_inference_pressure`` reports the stalled slice
of it with the same some/full semantics.

API-STALL: RETRY BACK-OFF AND FAILED REQUESTS
---------------------------------------------
The throughput split above is turn-granular: it can only judge a gap once an
assistant turn CLOSES it. That left the loudest stall of all invisible — a
client sitting in API retry back-off, showing "Waiting for API response · will
retry in 1m 14s · check your network" for minutes at a time while the
transcript stays silent. Two rules close that hole, both from evidence the
transcript does carry:

1. A gap that ENDS at an API-error entry (Claude Code writes a synthetic
   assistant line with ``isApiErrorMessage: true`` when a request finally
   fails — "API Error: 529 Overloaded", "Login expired", …) is inference the
   whole way: that wall-time was the client in the request/retry loop, so it
   is never re-read as a dormant gap, and it is always ``stalled`` (zero
   output tokens landed). Measured on a real overload episode, these gaps run
   ~220-240s each — right against the 300s dormancy cap, so the longer ones
   were silently disappearing into ``idle``.

2. The TRAILING open inference interval — an in-flight turn whose token count
   is not yet known — is tagged ``stalled`` once it exceeds
   ``API_STALL_TAIL_SECONDS``. Nothing has been produced for that long, so its
   throughput-so-far is 0 tok/s by construction. The threshold is the
   hysteresis that keeps a normal turn (and a one-shot retry blip) out of the
   metric: across 8.5k real inference gaps the p99 was 29s and the longest was
   68s, so a 120s default sits well clear of healthy generation.

Both rules stop at ``API_STALL_MAX_SECONDS``: past that, a silent transcript is
better explained by a dormant / resumed / killed session than by an API wait,
and the old dormancy cap applies. It matches the exporter's live window, past
which the transcript drops out of the fleet anyway.
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
# The categories that are a genuine STALL: the loop blocked on something
# outside itself.
STALL_CATEGORIES = (INFERENCE, TOOL)
# Categories we compute some/full pressure for. ``overhead`` is not a stall (it
# is the loop's own between-turn bookkeeping — productive-self), but it is the
# remainder of the active denominator, so it gets the same some/full treatment
# and a fleet panel can account for all of active time. Same set as
# ACTIVE_CATEGORIES, spelled as "the stalls, plus the non-stall remainder".
PRESSURE_CATEGORIES = STALL_CATEGORIES + (OVERHEAD,)

# A gap longer than this that does not end at a tool_result is treated as
# idle rather than a stall (dormant / resumed-session gap). Tool gaps are
# never capped — a long build is real tool pressure.
DEFAULT_MAX_GAP_SECONDS = 300.0
# A transcript not written within this window is not a live agent and is
# excluded from fleet/subtree pressure (matches cw-agent-stats' notion).
DEFAULT_LIVE_WINDOW_SECONDS = 900.0

# Effective-throughput floor (output tokens / second over an inference gap)
# below which the gap's wall-time is judged to be mostly STALL (429 back-off /
# network / TTFT / queueing) rather than generation. Claude's real output rate,
# thinking tokens included, runs in the tens of tok/s; 8 tok/s sits well under
# that floor so a healthy-but-slow turn is not mis-flagged, while a gap that
# yielded only a handful of tokens over several seconds reads as the stall it
# is. Tunable via AGENT_PSI_STALLED_TOKENS_PER_SEC.
DEFAULT_STALLED_TOKENS_PER_SEC = 8.0
# Inference gaps shorter than this are never tagged stalled: a sub-second /
# few-second gap is dominated by fixed TTFT overhead, and a small-but-fast gap
# (e.g. 100 tokens in 1s) is obviously productive. Guards divide-by-tiny noise.
DEFAULT_MIN_STALL_GAP_SECONDS = 5.0

# An in-flight (trailing, open) inference interval longer than this is tagged
# STALLED: no output has landed for that long, so its throughput-so-far is 0
# tok/s. This is the hysteresis knob for API retry back-off — a quick retry
# blip, and every healthy turn, must stay under it. Measured against 8.5k real
# inference gaps: p99 = 29s, max = 68s, so 120s clears normal generation by
# ~2x while catching a client parked in "will retry in 1m 14s".
# Tunable via AGENT_PSI_API_STALL_TAIL_SECONDS.
DEFAULT_API_STALL_TAIL_SECONDS = 120.0
# Ceiling on how much silent wall-time may be attributed to an API stall
# (both the trailing in-flight interval and a gap ending at an API-error
# entry). Past this, a silent transcript is better explained by a dormant /
# resumed / killed session than by an API wait, and the max-gap dormancy cap
# applies as before. Defaults to the exporter's live window, past which the
# transcript leaves the live fleet anyway.
# Tunable via AGENT_PSI_API_STALL_MAX_SECONDS.
DEFAULT_API_STALL_MAX_SECONDS = 900.0

# Decaying-window sizes (seconds) the exporter emits pressure for. Phase 1
# uses fixed sliding windows ending at ``now``; a true exponential decay is a
# phase-2 refinement.
DEFAULT_WINDOWS = (10, 60, 300)

Interval = namedtuple("Interval", ("start", "end", "category", "stalled"))
# ``stalled`` is meaningful only on an ``inference`` interval: True when the
# gap's effective throughput fell below the stall floor. Defaults False so every
# existing 3-arg Interval(...) construction (and the tests') stays valid and
# reads as productive/not-applicable.
Interval.__new__.__defaults__ = (False,)
# A transcript's parsed intervals plus identity/liveness metadata. ``model`` is
# the short model family the agent ran on (opus / sonnet / haiku / fable /
# ...), fixed for the agent's lifetime, so per-model pressure = the same
# some/full math restricted to agents sharing a ``model``.
# ``running`` is True when the transcript does NOT end in a completed final turn
# (see ``is_running_transcript``) — the accurate "still executing right now"
# signal, so a finished sub-agent drops from the live count immediately instead
# of lingering for the whole file-mtime live window.
Transcript = namedtuple(
    "Transcript",
    ("agent_id", "session_id", "is_main_loop", "model", "mtime", "intervals",
     "running"),
)
Transcript.__new__.__defaults__ = (True,)

# Known short model families, matched as substrings of the raw transcript model
# string (``claude-opus-5`` / ``claude-sonnet-5`` / bare ``opus`` all fold to a
# family). Kept explicit so an unrecognised value is surfaced verbatim rather
# than silently bucketed.
MODEL_FAMILIES = ("opus", "sonnet", "haiku", "fable")
# Placeholder the transcript uses for injected / non-inference assistant lines;
# never a real model, so it must not count toward an agent's model.
_SYNTHETIC_MODEL = "<synthetic>"
UNKNOWN_MODEL = "unknown"

# Internal moment: one timestamped point on the transcript timeline.
# ``output_tokens`` is the assistant turn's usage.output_tokens (thinking
# included), used to judge inference-gap throughput; None on non-assistant
# moments and when the usage datum is absent. ``api_error`` marks the
# synthetic assistant line Claude Code writes when a request finally fails
# (``isApiErrorMessage: true``) — proof that the gap ending here was the
# client in the API request/retry loop, not a dormant session.
_Moment = namedtuple(
    "_Moment",
    ("ts", "kind", "stop_reason", "tool_use_ids", "output_tokens", "api_error"),
)
_Moment.__new__.__defaults__ = (None, False)
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


def model_family(raw):
    """Map a raw transcript model string to a short family label, or None.

    ``claude-opus-5`` / ``claude-opus-4-8`` / bare ``opus`` -> ``"opus"``. The
    ``<synthetic>`` placeholder and empty / non-string values return None (not a
    real model). An unrecognised but non-empty value is returned lower-cased and
    verbatim so it is visible rather than silently dropped.
    """
    if not raw or not isinstance(raw, str):
        return None
    s = raw.strip().lower()
    if not s or s == _SYNTHETIC_MODEL:
        return None
    for fam in MODEL_FAMILIES:
        if fam in s:
            return fam
    return s


def extract_model(entries):
    """Dominant real model family across an entries list, or ``UNKNOWN_MODEL``.

    The model is fixed for an agent's lifetime; we still take the most common
    real (non-synthetic) family seen across the assistant turns so a stray
    placeholder line can't shift the label.
    """
    from collections import Counter

    counts = Counter()
    for e in entries:
        if e.get("type") != "assistant":
            continue
        msg = e.get("message")
        if not isinstance(msg, dict):
            continue
        fam = model_family(msg.get("model"))
        if fam:
            counts[fam] += 1
    if not counts:
        return UNKNOWN_MODEL
    return counts.most_common(1)[0][0]


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
        out_tokens = None
        msg = entry.get("message")
        if isinstance(msg, dict):
            stop = msg.get("stop_reason")
            usage = msg.get("usage")
            if isinstance(usage, dict):
                ot = usage.get("output_tokens")
                if isinstance(ot, (int, float)):
                    out_tokens = ot
        tu_ids = [
            b.get("id")
            for b in _content_blocks(entry)
            if isinstance(b, dict) and b.get("type") == "tool_use"
        ]
        return _Moment(
            ts,
            _ASST,
            stop,
            [i for i in tu_ids if i],
            out_tokens,
            entry.get("isApiErrorMessage") is True,
        )

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


def _classify_gap(prev, cur, max_gap, api_stall_max=DEFAULT_API_STALL_MAX_SECONDS):
    """Category for the interval [prev.ts, cur.ts], bounded by ``cur``."""
    d = cur.ts - prev.ts
    if cur.kind == _RESULT:
        # A tool ran during the gap — real tool time, never capped.
        return TOOL
    if cur.kind == _ASST and cur.api_error and d <= api_stall_max:
        # The gap ended in an API error, so the client spent it inside the
        # request/retry loop — measured API stall, not a dormant gap. Exempt
        # from the max-gap cap up to api_stall_max (a real overload episode
        # runs ~4 min per failed request, right against the 300s cap).
        return INFERENCE
    if d > max_gap:
        # Dormant / resumed-session gap; not a multi-minute inference stall.
        return IDLE
    if cur.kind == _ASST:
        return INFERENCE
    if cur.kind == _PROMPT:
        return WAITING_HUMAN if prev.kind == _ASST else IDLE
    return OVERHEAD  # _BOOK


def _is_stalled_inference(duration, output_tokens, stalled_tps, min_gap,
                          api_error=False):
    """True when an inference gap's effective throughput marks it a STALL.

    ``output_tokens`` is the token count of the assistant turn that ENDED the
    gap. Returns False (productive / not-judged) for: a gap shorter than
    ``min_gap`` (TTFT-dominated, too short to judge), a non-positive duration
    (divide-by-zero guard), or a missing token datum (no evidence). Otherwise
    the gap is stalled iff output_tokens / duration < ``stalled_tps``.

    ``api_error`` (the gap ended at an ``isApiErrorMessage`` line) is stall by
    definition — the request produced nothing at all — so it is judged without
    needing a usage datum, which those synthetic entries do not always carry.
    """
    if duration < min_gap or duration <= 0:
        return False
    if api_error:
        return True
    if output_tokens is None:
        return False
    return (output_tokens / duration) < stalled_tps


def _tail_interval(last, now, pending_tools, max_gap,
                   api_stall_tail=DEFAULT_API_STALL_TAIL_SECONDS,
                   api_stall_max=DEFAULT_API_STALL_MAX_SECONDS):
    """Open interval from the last moment to ``now`` = the agent's current
    state, or None if ``now`` precedes the last moment.

    The current state is decided FIRST, from what the loop last did, and only
    then is the max-gap cap applied — and only to an inference/overhead tail.
    Two states are counted at their TRUE wall-clock length, never truncated:

    * ``idle`` — a loop parked between turns (a returned ``end_turn`` with no
      tool dispatched) is genuinely idle for however long it waits. Capping it
      was the main-loop idle undercount: a dispatcher is mostly idle, and a
      multi-minute parked wait must read as multi-minutes of idle, not 300s.
    * ``tool`` — a dispatched-but-unfinished tool is a real ``tool_use`` in
      flight (a foreground blocking Bash wait — an ``until``-loop, a ``sleep``,
      a slow build — is the loop actively IN a tool). It is tool time, not
      idle, no matter how long it runs.

    An ``inference`` tail is the in-flight turn, and it is where API retry
    back-off shows up — a client parked on "will retry in 1m 14s" writes
    nothing at all. Past ``api_stall_tail`` seconds with no output it is
    counted at true wall-clock length and tagged ``stalled`` (0 tok/s so far),
    up to ``api_stall_max``; the threshold is the hysteresis that keeps normal
    turns and one-shot retry blips out of the stalled series.

    Only an ``overhead`` tail, or an ``inference`` tail past ``api_stall_max``,
    is capped to idle: a silent stretch that long with no tool running and no
    return-of-control is far more likely a dormant / resumed / killed session
    than a genuine model stall, and must not read as sustained inference
    pressure.
    """
    if now is None or now <= last.ts:
        return None
    if last.kind == _ASST:
        if pending_tools:
            category = TOOL  # tools dispatched, results not yet in.
        elif last.stop_reason == "end_turn":
            category = IDLE  # returned control; parked, waiting.
        else:
            category = INFERENCE
    elif last.kind == _RESULT:
        category = INFERENCE  # model generating the next turn.
    elif last.kind == _PROMPT:
        category = INFERENCE
    else:
        category = OVERHEAD
    if category in (IDLE, TOOL):
        # Parked-idle and in-flight tool are true wall-clock, never capped.
        return Interval(last.ts, now, category)
    d = now - last.ts
    if category == INFERENCE and api_stall_tail <= d <= api_stall_max:
        # In-flight turn with nothing produced for this long: an API stall
        # (retry back-off / network / queueing), counted at true length.
        return Interval(last.ts, now, INFERENCE, True)
    if d > max_gap:
        # Dormant / resumed-session gap; not a multi-minute model stall.
        return Interval(last.ts, min(now, last.ts + max_gap), IDLE)
    return Interval(last.ts, now, category)


def parse_intervals(entries, now=None, max_gap=DEFAULT_MAX_GAP_SECONDS,
                    stalled_tps=DEFAULT_STALLED_TOKENS_PER_SEC,
                    min_stall_gap=DEFAULT_MIN_STALL_GAP_SECONDS,
                    api_stall_tail=DEFAULT_API_STALL_TAIL_SECONDS,
                    api_stall_max=DEFAULT_API_STALL_MAX_SECONDS):
    """Ordered JSONL entries (dicts) -> list[Interval].

    ``entries`` need not be sorted; moments are sorted by timestamp. When
    ``now`` is given a trailing open interval is appended for the agent's
    current state (what makes the live fleet PSI reflect the present).

    Each ``inference`` interval also carries a ``stalled`` flag set from the
    ending assistant turn's output-token throughput (see
    ``_is_stalled_inference``), from the gap having ended in an API error, or
    — for the trailing open interval — from the in-flight turn having produced
    nothing for ``api_stall_tail`` seconds (API retry back-off).
    """
    moments = [m for m in (_entry_to_moment(e) for e in entries) if m is not None]
    moments.sort(key=lambda m: m.ts)
    if not moments:
        return []

    intervals = []
    for prev, cur in zip(moments, moments[1:]):
        if cur.ts <= prev.ts:
            continue
        cat = _classify_gap(prev, cur, max_gap, api_stall_max)
        stalled = cat == INFERENCE and _is_stalled_inference(
            cur.ts - prev.ts, cur.output_tokens, stalled_tps, min_stall_gap,
            api_error=cur.api_error,
        )
        intervals.append(Interval(prev.ts, cur.ts, cat, stalled))

    result_ids = _all_result_ids(entries)
    last = moments[-1]
    pending = [i for i in last.tool_use_ids if i not in result_ids]
    tail = _tail_interval(
        last, now, pending, max_gap, api_stall_tail, api_stall_max
    )
    if tail is not None and tail.end > tail.start:
        intervals.append(tail)
    return intervals


def is_running_transcript(entries):
    """True if the transcript does NOT end in a completed final turn.

    Liveness by state, not by file mtime: a sub-agent that returned its final
    answer ends with an assistant ``end_turn`` that dispatched no still-pending
    tool -> FINISHED (drop it from the live count immediately). Every other
    trailing state is a running agent and stays live:

    * last line is a ``tool_result`` — the next inference turn is pending;
    * last line is an assistant turn with tools still in flight (their results
      not yet in) — a blocking tool / bash wait that can legitimately leave the
      transcript un-written for up to ``max_gap`` seconds;
    * last line is an assistant turn whose ``stop_reason`` is not ``end_turn``
      (still mid-turn), or a human/injected prompt.

    An empty / unreadable transcript is not running.
    """
    moments = [m for m in (_entry_to_moment(e) for e in entries) if m is not None]
    if not moments:
        return False
    moments.sort(key=lambda m: m.ts)
    last = moments[-1]
    if last.kind != _ASST:
        return True
    result_ids = _all_result_ids(entries)
    pending = [i for i in last.tool_use_ids if i not in result_ids]
    if pending:
        return True
    return last.stop_reason != "end_turn"


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


def _interval_at(intervals, t):
    """The interval covering instant ``t`` (start<=t<end), or None."""
    for iv in intervals:
        if iv.start <= t < iv.end:
            return iv
    return None


def compute_pressure(agent_intervals, window_start, window_end):
    """some/full pressure per pressure category over [window_start, window_end].

    ``agent_intervals`` maps agent_id -> list[Interval]. Returns a dict keyed
    by (category, kind) for every category in ``PRESSURE_CATEGORIES``, with
    kind in {"some", "full"} -> ratio in [0, 1].

    Exact via a boundary sweep: between consecutive interval boundaries every
    agent's state is constant, so we sample the midpoint of each sub-slice.
    ``some`` accumulates a slice when >=1 agent is in the category; ``full``
    when there is >=1 ACTIVE agent and every active agent is in the category
    (idle / waiting_human / absent agents don't count as active, matching
    PSI's "every non-idle task stalled").

    ``overhead`` runs through the identical math even though it is not a stall:
    an agent in overhead is active, so it already broke ``inference_full`` /
    ``tool_full``, and its own some/full is what makes the three lines add up
    over active time.
    """
    W = window_end - window_start
    acc = {(c, k): 0.0 for c in PRESSURE_CATEGORIES for k in ("some", "full")}
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
        for c in PRESSURE_CATEGORIES:
            if any(s == c for s in active_states):
                acc[(c, "some")] += d
            if active_states and all(s == c for s in active_states):
                acc[(c, "full")] += d

    return {k: v / W for k, v in acc.items()}


def compute_stalled_inference_pressure(agent_intervals, window_start, window_end):
    """some/full pressure for the STALLED slice of inference over the window.

    Mirrors ``compute_pressure`` exactly (same boundary sweep, same active-set
    and PSI ``some``/``full`` semantics) but restricted to inference intervals
    flagged ``stalled``. Returns {"some": x, "full": y} in [0, 1].

    * ``some`` — fraction of the window in which >=1 agent is in a stalled
      inference gap.
    * ``full`` — fraction in which there is >=1 ACTIVE agent and EVERY active
      agent is in a stalled inference gap (an agent in tool / productive
      inference / overhead is active-but-not-stalled and so breaks ``full``).

    Fleet ``full`` here is the "everyone is rate-limited right now" signal,
    disentangled from "everyone generating hard" which ``inference_full``
    conflated.
    """
    W = window_end - window_start
    acc = {"some": 0.0, "full": 0.0}
    if W <= 0:
        return acc

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
        active = 0
        stalled = 0
        for ivs in agent_intervals.values():
            iv = _interval_at(ivs, mid)
            if iv is None or iv.category not in ACTIVE_CATEGORIES:
                continue
            active += 1
            if iv.category == INFERENCE and iv.stalled:
                stalled += 1
        if stalled >= 1:
            acc["some"] += d
        if active >= 1 and stalled == active:
            acc["full"] += d

    return {k: v / W for k, v in acc.items()}


# --- transcript discovery + reading -------------------------------------
def _agent_id_from_path(path):
    base = os.path.basename(path)
    if base.startswith("agent-") and base.endswith(".jsonl"):
        return base[len("agent-"):-len(".jsonl")]
    return None


def read_transcript(path, now=None, max_gap=DEFAULT_MAX_GAP_SECONDS,
                    is_main_loop=False, session_id=None,
                    stalled_tps=DEFAULT_STALLED_TOKENS_PER_SEC,
                    min_stall_gap=DEFAULT_MIN_STALL_GAP_SECONDS,
                    api_stall_tail=DEFAULT_API_STALL_TAIL_SECONDS,
                    api_stall_max=DEFAULT_API_STALL_MAX_SECONDS):
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
    intervals = parse_intervals(
        entries, now=now, max_gap=max_gap,
        stalled_tps=stalled_tps, min_stall_gap=min_stall_gap,
        api_stall_tail=api_stall_tail, api_stall_max=api_stall_max,
    )
    model = extract_model(entries)
    running = is_running_transcript(entries)
    return Transcript(
        agent_id, session_id, is_main_loop, model, mtime, intervals, running
    )


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
                             live_window=DEFAULT_LIVE_WINDOW_SECONDS,
                             stalled_tps=DEFAULT_STALLED_TOKENS_PER_SEC,
                             min_stall_gap=DEFAULT_MIN_STALL_GAP_SECONDS,
                             api_stall_tail=DEFAULT_API_STALL_TAIL_SECONDS,
                             api_stall_max=DEFAULT_API_STALL_MAX_SECONDS):
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
            stalled_tps=stalled_tps, min_stall_gap=min_stall_gap,
            api_stall_tail=api_stall_tail, api_stall_max=api_stall_max,
        )
        if t is not None:
            live.append(t)
    return live
