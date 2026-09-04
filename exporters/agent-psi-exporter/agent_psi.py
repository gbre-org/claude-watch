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

FLEET COMPOSITION: WHY ``full`` IS NOT "HOW BUSY IS THE FLEET"
--------------------------------------------------------------
``some``/``full`` answer latency and throughput-loss questions, and both are
normalized over the ACTIVE agents only. That makes ``full`` a UNANIMITY signal,
not an occupancy one, and it behaves badly as a fleet-composition series in the
two regimes that matter most:

* ONE active worker, tool-bound -> ``tool_full`` = 1.0. Arithmetically right
  ("every active agent is in a tool"), but it renders as a saturated fleet when
  the honest statement is "one agent, in a tool".
* FOUR active workers doing DIFFERENT things -> every ``*_full`` collapses
  toward 0 even though the fleet is 100% busy, because no category holds all of
  them at once. A stack of ``*_full`` series is therefore anti-correlated with
  fleet busyness exactly when concurrency is high.

``compute_mean_agents`` is the occupancy counterpart: for each state, the MEAN
NUMBER OF AGENTS in that state over the window (agent-seconds / window). It is
denominator-free — nothing is divided by an agent count — so one busy agent
reads as 1.0 and four busy agents read as 4.0, and the states stack to the mean
number of LIVE agents in the scope rather than to a fraction of a shifting
denominator. ``idle`` and ``waiting_human`` are first-class states there, which
is what stops a quiet fleet (or the idle-by-design dispatcher) from rendering
as saturation.

OBSERVED vs UNOBSERVABLE (the third state)
------------------------------------------
Every state above is inferred from transcript writes, so a host that GOES AWAY
mid-run — a laptop lid closing on a workbot, a suspended VM, a killed client —
freezes the transcript with the last observed state still "busy". Read
literally, an in-flight tool tail is then counted as tool time forever (until
the file-mtime live window drops the agent entirely, at which point it silently
vanishes). Neither "perpetually busy" nor "quietly idle" is true; the truth is
that we stopped being able to observe it.

So a trailing active interval is SPLIT at ``STALE_AFTER_SECONDS`` of silence:
the first part keeps its category and ``observed=True``, and the remainder
carries the same category with ``observed=False``. Splitting rather than
re-categorizing is deliberate — every category-based metric
(``duty_seconds``, ``compute_pressure``, the stalled-inference pressure) sees
byte-identical coverage, so nothing about the existing some/full series moves —
while ``compute_mean_agents`` reports the unobserved remainder under its own
``unobservable`` state. Note what the flag does and does not claim: a genuinely
long blocking tool and a frozen host are INDISTINGUISHABLE from the transcript,
and ``unobservable`` is exactly that admission, not an assertion that the agent
is gone.

A TERMINATED SUB-AGENT HAS NO CURRENT STATE
-------------------------------------------
The trailing open interval above says "this is what the agent is doing right
now", which presupposes there still IS an agent. For a SUB-AGENT that is not
true once it returns: a worker whose transcript ends in a completed final turn
(assistant ``end_turn``, no tool left in flight — ``is_running_transcript``)
has handed its answer back and is gone. Synthesizing a tail for it read as
``idle`` (a returned ``end_turn`` is the parked-dispatcher shape) and, because
an idle tail is deliberately never capped, that phantom accrued one agent-second
per second until the file-mtime live window finally dropped the file — up to
``LIVE_WINDOW_SECONDS`` of a terminated worker sitting in
``compute_mean_agents``' ``idle`` band. The live-agent count already consulted
``is_running_transcript`` and said 0 while the composition panel showed a flat
stack: the same fleet, described two incompatible ways.

So a terminated sub-agent's timeline simply ENDS at its last transcript write:
``parse_intervals(..., terminated=True)`` emits no tail. Its closed intervals
are untouched, so the work it did before returning still counts for whatever
part of a trailing window it really occupied (an agent that finished 20s into
the last 60s contributed 20 agent-seconds, and should), and it then falls out
of every window on its own within one window length rather than lingering.

The MAIN LOOP is deliberately exempt. Its transcript ends in exactly the same
completed final turn whenever it is between turns, but the dispatcher is still
there, parked and waiting — that IS its steady state, and suppressing its idle
tail would re-create the main-loop idle undercount the uncap fixed. Termination
is a claim we only make about workers, whose return is an exit.
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
# Occupancy states for ``compute_mean_agents``. NOT categories: they are how a
# category+flag pair is rendered for the fleet-composition panel. ``inference``
# there means PRODUCTIVE inference (the stalled slice is its own state, so the
# bands stack without a subtraction), and ``unobservable`` overrides whatever
# the frozen transcript last said (see the module docstring).
INFERENCE_STALLED = "inference_stalled"
UNOBSERVABLE = "unobservable"
AGENT_STATES = (
    INFERENCE, INFERENCE_STALLED, TOOL, OVERHEAD, IDLE, WAITING_HUMAN,
    UNOBSERVABLE,
)
# The subset of AGENT_STATES that is an agent doing work. ``unobservable`` is
# deliberately NOT busy and NOT idle — it is the admission that we cannot say.
BUSY_STATES = (INFERENCE, INFERENCE_STALLED, TOOL, OVERHEAD)
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

# Silence, on a trailing ACTIVE interval, past which we stop asserting the
# state and call the remainder ``unobservable``: a host that went away mid-run
# (laptop lid, suspended VM, killed client) is indistinguishable from a long
# blocking tool, and claiming either would be a guess. Matches the dormancy cap
# so the two "we have lost the thread" thresholds agree.
# Tunable via AGENT_PSI_STALE_AFTER_SECONDS.
DEFAULT_STALE_AFTER_SECONDS = 300.0

# Decaying-window sizes (seconds) the exporter emits pressure for. Phase 1
# uses fixed sliding windows ending at ``now``; a true exponential decay is a
# phase-2 refinement.
DEFAULT_WINDOWS = (10, 60, 300)

Interval = namedtuple(
    "Interval", ("start", "end", "category", "stalled", "observed")
)
# ``stalled`` is meaningful only on an ``inference`` interval: True when the
# gap's effective throughput fell below the stall floor. Defaults False so every
# existing 3-arg Interval(...) construction (and the tests') stays valid and
# reads as productive/not-applicable.
# ``observed`` is False on the remainder of a trailing ACTIVE interval past
# ``STALE_AFTER_SECONDS`` of transcript silence — the category is kept (so every
# category-based metric is unchanged) but the fleet-composition view renders it
# as ``unobservable`` instead of asserting the frozen state.
Interval.__new__.__defaults__ = (False, True)
# A transcript's parsed intervals plus identity/liveness metadata. ``model`` is
# the short model family the agent ran on (opus / sonnet / haiku / fable /
# ...), fixed for the agent's lifetime, so per-model pressure = the same
# some/full math restricted to agents sharing a ``model``.
# ``running`` is True when the transcript does NOT end in a completed final turn
# (see ``is_running_transcript``) — the accurate "still executing right now"
# signal, so a finished sub-agent drops from the live count immediately instead
# of lingering for the whole file-mtime live window.
# ``api_errors`` is the list of (timestamp, kind) pairs for every API-error
# entry in the transcript (see ``classify_api_error``) — the per-model
# upstream-capacity evidence, since the agent's model is fixed for its lifetime.
Transcript = namedtuple(
    "Transcript",
    ("agent_id", "session_id", "is_main_loop", "model", "mtime", "intervals",
     "running", "api_errors"),
)
Transcript.__new__.__defaults__ = (True, ())

# Known short model families, matched as substrings of the raw transcript model
# string (``claude-opus-5`` / ``claude-sonnet-5`` / bare ``opus`` all fold to a
# family). Kept explicit so an unrecognised value is surfaced verbatim rather
# than silently bucketed.
MODEL_FAMILIES = ("opus", "sonnet", "haiku", "fable")
# Placeholder the transcript uses for injected / non-inference assistant lines;
# never a real model, so it must not count toward an agent's model.
_SYNTHETIC_MODEL = "<synthetic>"
UNKNOWN_MODEL = "unknown"

# --- API-error classes ---------------------------------------------------
# Claude Code writes a synthetic assistant line with ``isApiErrorMessage: true``
# whenever a request finally fails, and the text says WHY. One "api errors"
# number would be useless for the question these metrics exist to answer — how
# much a given MODEL is costing us upstream — because the causes map to
# completely different knobs:
#
#   capacity          provider is out of headroom (529 / 5xx / overloaded)
#                     -> knob: which model / when to run, retry budget
#   rate_limit        we are over our own quota (429 / rate limit)
#                     -> knob: concurrency, fleet size
#   network           transport failed before a verdict (timeout / connection)
#                     -> knob: ours to chase, not the provider's
#   context_overflow  the request itself was too big (prompt is too long)
#                     -> knob: context management; the retry cost is self-
#                        inflicted, so it must never read as provider trouble
#   refusal           safeguards / content filtering blocked it
#                     -> not a performance signal at all
#   auth              401 / login expired -> our credentials
#   other             recognised as an API error, cause unmatched -- a VISIBLE
#                     catch-all, never a silent fold into capacity
#
# The class set is deliberately provider-agnostic: the same (model x class)
# schema applies to a Bedrock / gateway-fronted fleet whose failure mix is
# different, so panels built on it transfer without a schema change.
API_ERR_CAPACITY = "capacity"
API_ERR_RATE_LIMIT = "rate_limit"
API_ERR_NETWORK = "network"
API_ERR_CONTEXT_OVERFLOW = "context_overflow"
API_ERR_REFUSAL = "refusal"
API_ERR_AUTH = "auth"
API_ERR_OTHER = "other"
API_ERROR_KINDS = (
    API_ERR_CAPACITY, API_ERR_RATE_LIMIT, API_ERR_NETWORK,
    API_ERR_CONTEXT_OVERFLOW, API_ERR_REFUSAL, API_ERR_AUTH, API_ERR_OTHER,
)

# Substring probes, tried IN ORDER, matched case-insensitively. Sourced from the
# messages actually present in this fleet's transcripts ("API Error: 529
# Overloaded. This is a server-side issue...", "Prompt is too long", "Login
# expired · Please run /login", "Please run /login · API Error: 401 OAuth access
# token has expired", "...safeguards flagged this message", "Output blocked by
# content filtering policy", "API Error: Server error mid-response").
_API_ERROR_PROBES = (
    # Auth / input / refusal first: their messages can carry an incidental
    # status code ("Please run /login · API Error: 401 ...") and must not
    # inflate the capacity series the per-model upstream panels read.
    (API_ERR_AUTH, ("login expired", "oauth", "re-authenticate", "401")),
    (API_ERR_CONTEXT_OVERFLOW, ("prompt is too long", "too many tokens",
                                "context length", "context window")),
    (API_ERR_REFUSAL, ("safeguards", "content filtering", "aup")),
    (API_ERR_RATE_LIMIT, ("rate limit", "rate_limit", "429", "quota")),
    (API_ERR_NETWORK, ("connection error", "connection reset", "timeout",
                       "timed out", "econnreset", "network")),
    (API_ERR_CAPACITY, ("overloaded", "529", "503", "502", "500",
                        "server error", "internal server", "capacity")),
)


def classify_api_error(text):
    """Classify one API-error entry's text into an ``API_ERROR_KINDS`` bucket.

    See ``_API_ERROR_PROBES`` for the ordering rationale: the causes that are
    OURS (auth, oversized context, a refusal) are matched before the ones that
    are the provider's, so a message mentioning both is attributed to the cause
    that actually needs acting on.
    """
    s = (text or "").lower()
    for kind, needles in _API_ERROR_PROBES:
        if any(n in s for n in needles):
            return kind
    return API_ERR_OTHER


def _api_error_text(entry):
    """Human-readable text of an API-error entry ('' when absent)."""
    msg = entry.get("message")
    if not isinstance(msg, dict):
        return ""
    content = msg.get("content")
    if isinstance(content, str):
        return content
    if isinstance(content, list):
        parts = [
            b.get("text", "")
            for b in content
            if isinstance(b, dict) and isinstance(b.get("text"), str)
        ]
        return " ".join(parts)
    return ""


def extract_api_errors(entries):
    """[(timestamp, kind), ...] for every API-error entry, sorted by time.

    An entry with no parseable timestamp is skipped — it cannot be placed in a
    window, and a windowed count is the only thing this feeds.
    """
    out = []
    for e in entries:
        if e.get("isApiErrorMessage") is not True:
            continue
        ts = parse_ts(e.get("timestamp"))
        if ts is None:
            continue
        out.append((ts, classify_api_error(_api_error_text(e))))
    out.sort(key=lambda p: p[0])
    return tuple(out)


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


def _split_stale(interval, stale_after):
    """[interval] or [observed_part, unobservable_part] for an ACTIVE tail.

    An idle / waiting_human tail is never split: absence of writes is exactly
    what idle looks like, so silence CONFIRMS it rather than undermining it.
    Only an active tail makes a claim that silence stops supporting.
    """
    if interval.category not in ACTIVE_CATEGORIES:
        return [interval]
    cut = interval.start + stale_after
    if cut >= interval.end:
        return [interval]
    return [
        interval._replace(end=cut),
        interval._replace(start=cut, observed=False),
    ]


def _tail_interval(last, now, pending_tools, max_gap,
                   api_stall_tail=DEFAULT_API_STALL_TAIL_SECONDS,
                   api_stall_max=DEFAULT_API_STALL_MAX_SECONDS,
                   stale_after=DEFAULT_STALE_AFTER_SECONDS):
    """Open interval(s) from the last moment to ``now`` = the agent's current
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

    Returns a LIST of 0-2 intervals. An ACTIVE tail longer than ``stale_after``
    is split there: the first part keeps ``observed=True``, the remainder keeps
    the SAME category with ``observed=False``. Same category on both halves is
    the point — every category-based metric sees identical coverage, and only
    the fleet-composition view distinguishes "in a tool" from "was in a tool
    when we last heard from it" (a lid-closed host, an unbounded blocking
    command — indistinguishable from here, and labelled as such).
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
        return _split_stale(Interval(last.ts, now, category), stale_after)
    d = now - last.ts

    if category == INFERENCE and api_stall_tail <= d <= api_stall_max:
        # In-flight turn with nothing produced for this long: an API stall
        # (retry back-off / network / queueing), counted at true length.
        return _split_stale(
            Interval(last.ts, now, INFERENCE, True), stale_after
        )
    if d > max_gap:
        # Dormant / resumed-session gap; not a multi-minute model stall.
        return [Interval(last.ts, min(now, last.ts + max_gap), IDLE)]
    return _split_stale(Interval(last.ts, now, category), stale_after)


def parse_intervals(entries, now=None, max_gap=DEFAULT_MAX_GAP_SECONDS,
                    stalled_tps=DEFAULT_STALLED_TOKENS_PER_SEC,
                    min_stall_gap=DEFAULT_MIN_STALL_GAP_SECONDS,
                    api_stall_tail=DEFAULT_API_STALL_TAIL_SECONDS,
                    api_stall_max=DEFAULT_API_STALL_MAX_SECONDS,
                    stale_after=DEFAULT_STALE_AFTER_SECONDS,
                    terminated=False):
    """Ordered JSONL entries (dicts) -> list[Interval].

    ``entries`` need not be sorted; moments are sorted by timestamp. When
    ``now`` is given a trailing open interval is appended for the agent's
    current state (what makes the live fleet PSI reflect the present) —
    UNLESS ``terminated`` says the agent is gone, in which case the timeline
    ends at its last write and no current state is invented. Callers set that
    for a sub-agent whose transcript ended in a completed final turn; see the
    module docstring's terminated-sub-agent section for why the main loop,
    whose transcript looks identical while it is merely parked, is exempt.

    Each ``inference`` interval also carries a ``stalled`` flag set from the
    ending assistant turn's output-token throughput (see
    ``_is_stalled_inference``), from the gap having ended in an API error, or
    — for the trailing open interval — from the in-flight turn having produced
    nothing for ``api_stall_tail`` seconds (API retry back-off).

    An ACTIVE trailing interval past ``stale_after`` seconds of silence is
    split, the remainder carrying ``observed=False`` (same category) — see
    ``_split_stale``.
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

    if terminated:
        # The agent returned and exited: there is no "right now" to describe.
        return intervals

    result_ids = _all_result_ids(entries)
    last = moments[-1]
    pending = [i for i in last.tool_use_ids if i not in result_ids]
    tail = _tail_interval(
        last, now, pending, max_gap, api_stall_tail, api_stall_max, stale_after
    )
    for iv in tail or ():
        if iv.end > iv.start:
            intervals.append(iv)
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


# --- fleet composition (occupancy, not pressure) -------------------------
def interval_state(iv):
    """The ``AGENT_STATES`` label for one interval.

    ``observed=False`` wins over everything: once the transcript went silent we
    are reporting our own blindness, not the state we last saw. Otherwise a
    stalled inference interval reports ``inference_stalled`` and a productive
    one ``inference``, so the two never double-count in a stack.
    """
    if not iv.observed:
        return UNOBSERVABLE
    if iv.category == INFERENCE:
        return INFERENCE_STALLED if iv.stalled else INFERENCE
    return iv.category


def compute_mean_agents(agent_intervals, window_start, window_end):
    """Mean number of agents per state over [window_start, window_end].

    Returns a dict keyed by every ``AGENT_STATES`` label -> agent-seconds in
    that state / window length. The unit is AGENTS: 1.0 means "one agent, the
    whole window"; 4.0 means four. Summing the returned values gives the mean
    number of agents present in the scope at all.

    This is the honest fleet-composition series, and the reason it exists is
    that ``compute_pressure``'s ``full`` cannot be one. ``full`` normalizes over
    the ACTIVE agents, so a lone tool-bound worker reads 1.0 ("100% of the
    fleet") while four workers doing four different things read ~0.0 in every
    category at once. Nothing here is divided by an agent count, so neither
    distortion is possible, and ``idle`` / ``waiting_human`` / ``unobservable``
    are first-class: a quiet fleet reads as quiet instead of vanishing into a
    shrinking denominator.
    """
    W = window_end - window_start
    acc = {s: 0.0 for s in AGENT_STATES}
    if W <= 0:
        return acc
    for ivs in agent_intervals.values():
        for iv in ivs:
            lo = max(iv.start, window_start)
            hi = min(iv.end, window_end)
            if hi <= lo:
                continue
            state = interval_state(iv)
            if state in acc:
                acc[state] += hi - lo
    return {s: v / W for s, v in acc.items()}


def is_unobservable_now(intervals):
    """True when the agent's CURRENT (latest) interval is unobserved.

    "We have not heard from this agent in ``stale_after`` seconds and the last
    thing it was doing was work" — the suspended-host / runaway-command state,
    which is neither busy nor idle.
    """
    if not intervals:
        return False
    latest = max(intervals, key=lambda iv: iv.end)
    return not latest.observed


def count_api_errors(transcripts, window_start, window_end):
    """{kind: count} of API-error entries in [window_start, window_end).

    ``transcripts`` is an iterable of ``Transcript``. Every kind in
    ``API_ERROR_KINDS`` is present (0 when unseen) so a series does not blink
    out of existence between storms.
    """
    acc = {k: 0 for k in API_ERROR_KINDS}
    for t in transcripts:
        for ts, kind in t.api_errors or ():
            if window_start <= ts < window_end:
                acc[kind] = acc.get(kind, 0) + 1
    return acc


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
                    api_stall_max=DEFAULT_API_STALL_MAX_SECONDS,
                    stale_after=DEFAULT_STALE_AFTER_SECONDS):
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
    running = is_running_transcript(entries)
    # A SUB-AGENT that is not running has returned and exited, so it gets no
    # trailing "current state" interval — otherwise its returned-end_turn tail
    # reads as (uncapped) idle and the terminated worker keeps occupying the
    # fleet-composition series until the file-mtime live window expires. The
    # main loop's transcript has the same shape whenever it is parked between
    # turns, but the dispatcher is still there, so it keeps its idle tail.
    terminated = not is_main_loop and not running
    intervals = parse_intervals(
        entries, now=now, max_gap=max_gap,
        stalled_tps=stalled_tps, min_stall_gap=min_stall_gap,
        api_stall_tail=api_stall_tail, api_stall_max=api_stall_max,
        stale_after=stale_after, terminated=terminated,
    )
    model = extract_model(entries)
    return Transcript(
        agent_id, session_id, is_main_loop, model, mtime, intervals, running,
        extract_api_errors(entries),
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
                             api_stall_max=DEFAULT_API_STALL_MAX_SECONDS,
                             stale_after=DEFAULT_STALE_AFTER_SECONDS):
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
            stale_after=stale_after,
        )
        if t is not None:
            live.append(t)
    return live
