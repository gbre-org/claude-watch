"""Live agent activity stats — tool calls + tokens per Claude Code subagent.

The transcript survey / fold LIBRARY behind ``cw-agent-stats`` (the sibling
CLI in this directory). Stdlib only; importable by the CLI and by the tests.

History: written for botchat's header badge (botchat #2955/#2956, Andrew,
2026-08-21: *"live updating counter of agent tool calls (on all views,
including compact)"* + *"token count too if possible"*), then the feature was
pulled out of botchat entirely (2026-08-22, Andrew: botchat carries none of
it; "move what you have to into cw"). This module is that move: it used to be
``botchat/src/botchat/agentstats.py`` and ``cw-agent-stats`` reached it by
pointing ``sys.path`` at the botchat checkout. It is now vendored here and
claude-watch owns it outright — producer (this module + the CLI) and
consumer (``queue-minisite/app.py`` ``_load_agent_stats``) live in ONE repo.

WHAT THIS IS
------------
* **Producer** (``cw-agent-stats`` on the HOST): scan the live Claude Code
  subagent transcripts, fold each one into per-agent counters, and write ONE
  JSON snapshot atomically (``write_snapshot``: tmp + ``os.replace``) to the
  CLI's ``--out`` path — by default ``agent-stats.json`` inside claude-watch's
  state dir (``$CLAUDE_WATCH_STATE_DIR``, else ``/var/lib/claude-watch``),
  the same dir the daemon's ``active-agents.json`` lives in and which the
  compose stack already bind-mounts into the queue-minisite at
  ``/agents-state``.

* **Consumer** (the queue-minisite, ``app.py`` ``_load_agent_stats``): stat +
  read the snapshot, JOIN ``agents[].queue_id`` onto the running queue rows,
  treat a snapshot older than ``QUEUE_MINISITE_AGENT_STATS_STALE_SECONDS`` as
  stale (blank cells + an ``agents n/a`` pill, never a frozen number). The
  consumer keeps its own reader; nothing in this module is imported by it.
  The snapshot schema below is the contract between the two — it is pinned
  by ``queue-minisite/test_agent_stats.py`` on the consumer side and by
  ``tests/test_agent_stats.py`` here on the producer side.

WHY THE TRANSCRIPTS (and not claude-watch / agent-ctl / the exporter)
--------------------------------------------------------------------
Surveyed 2026-08-21, cheapest-first:

1. ``claude-watch status --json`` — has the MAIN loop's context tokens +
   ``active_agents`` COUNT only; no per-agent tool/token fields. Also ~1.4s
   of CPU per call, too heavy for a 4s tick.
2. ``agent-ctl list`` — per-agent description + "Last JSONL write", but no
   ``--json`` and no tool/token counts at all.
3. ``work-queue-exporter`` (:9099) — queue-item gauges only, nothing per agent.
4. The subagent JSONL transcripts under
   ``~/.claude/projects/<slug>/<session>/subagents/agent-*.jsonl`` are the ONLY
   source that carries, per agent, every ``tool_use`` block AND every
   ``usage`` block. This is exactly what claude-watch itself reads for its
   token figures (``token_usage.rs``) and for agent liveness
   (``active_agents.rs``), so the numbers agree with the dashboard.

The producer keeps a per-file byte OFFSET and only parses what was appended
since the last tick, so a tick is a handful of ``stat``s + a few KB of JSON
even with multi-MB transcripts. A cron-driven loop (``--loop``) re-parses the
live files from 0 once a minute on process start, which is ~100ms for a
handful of 1-2MB files — fine.

DEFINITIONS (keep these in sync with queue-minisite/README.md "Agent activity counters")
-----------------------------------------------------------------------------------------
* **tool calls** — the number of ``tool_use`` content blocks across the
  agent's ``assistant`` entries (one per tool invocation).
* **context tokens** — the agent's CURRENT context size: ``input_tokens +
  cache_creation_input_tokens + cache_read_input_tokens`` from its LATEST
  ``usage`` block (the same sum claude-watch shows as ``Tokens: N /
  1,000,000`` for the main loop).
* **output tokens** — the SUM of each API message's FINAL ``output_tokens``
  (Claude Code splits one message into several consecutive JSONL entries —
  thinking / text / tool_use — whose ``usage.output_tokens`` is CUMULATIVE
  within the ``message.id``; the fold adds the per-message delta).
* **live** — the transcript was written within ``live_window_secs`` (default
  900s) AND the last entry is not a terminal ``assistant`` turn
  (``stop_reason == "end_turn"`` with no tool_use => the agent has returned).
  claude-watch's own liveness is mtime-only with a 120s window; ours is wider
  because a single build/test tool call routinely takes minutes and a counter
  that drops an agent mid-build reads as a bug.
* **main loop context tokens** — the latest ``usage`` in the newest top-level
  session transcript (``~/.claude/projects/<slug>/<session>.jsonl``), same sum
  as above. Tail-read only (last 256KB), cached on (mtime, size).
"""

from __future__ import annotations

import json
import os
import re
import socket
import tempfile
import time
from collections.abc import Iterable
from dataclasses import dataclass, field
from pathlib import Path

# File name of the snapshot inside the state dir (``<state dir>/agent-stats.json``).
SNAPSHOT_FILENAME = "agent-stats.json"

# Snapshot schema version — bump when the shape changes so a stale producer and
# a newer consumer (or vice versa) can tell.
#
# v2 (2026-08-22, claude-watch #663): added the fleet-wide ``tool_totals`` /
# ``model_totals`` / ``tool_model_totals`` breakdowns to the snapshot (see
# ``Collector.tick``) so the node-exporter textfile producer (cw-agent-stats)
# can expose ``claude_tool_use_total`` / ``claude_model_use_total`` — the
# devbar-equivalent model + tool-use telemetry — without a second transcript
# survey pass.
#
# v3 (2026-08-22, cw dashboard panels, Andrew #5370): added the fleet-wide
# ``model_output_tokens_totals`` breakdown (OUTPUT tokens by model id, same
# windowed-gauge semantics as ``tool_totals``/``model_totals``) and
# ``totals.agents_spawned`` (distinct subagent transcripts seen in the live
# window) so cw-agent-stats can expose ``claude_output_tokens_total{model=}``
# and ``claude_agent_spawned_count``.
SNAPSHOT_VERSION = 3

# Producer: a transcript not written within this window is not live.
DEFAULT_LIVE_WINDOW_SECS = 900.0

# Producer: how much of the main-loop transcript tail to read for its latest
# usage block. Main-loop entries (tool results) can be large; 256KB covers a
# generous number of them.
MAIN_TAIL_BYTES = 256 * 1024

_QUEUE_RE = re.compile(r"Queue item:\s*(q-[0-9A-Za-z-]+)")


def default_projects_dir() -> Path:
    """``~/.claude/projects`` (override via ``CLAUDE_PROJECTS_DIR`` for tests)."""
    env = os.environ.get("CLAUDE_PROJECTS_DIR", "").strip()
    if env:
        return Path(env)
    return Path(os.path.expanduser("~/.claude/projects"))


# ---------------------------------------------------------------------------
# Producer: per-agent incremental transcript folding
# ---------------------------------------------------------------------------


@dataclass
class AgentState:
    """Running counters for ONE subagent transcript (incremental, offset-based)."""

    path: str
    agent_id: str
    session_id: str | None = None
    offset: int = 0
    partial: bytes = b""
    tool_calls: int = 0
    output_tokens: int = 0
    context_tokens: int = 0
    started_at: str | None = None
    last_ts: str | None = None
    last_tool: str | None = None
    last_entry_type: str | None = None
    last_stop_reason: str | None = None
    last_entry_had_tool_use: bool = False
    last_usage_msg_id: str | None = None
    _last_msg_out: int = 0
    # Fleet telemetry (claude-watch #663, the devbar-equivalent model/tool-use
    # counters): folded in the SAME feed_entry pass as tool_calls/output_tokens
    # above — no extra parsing. ``tool_counts`` is "calls by tool name";
    # ``tool_model_counts`` additionally breaks each tool down by the model
    # that issued the call (free: the model id is a sibling field on the same
    # ``message`` object already in hand); ``model_counts`` is "assistant
    # turns by model", incremented once per distinct message id (mirrors the
    # output_tokens delta-by-message-id logic below).
    tool_counts: dict[str, int] = field(default_factory=dict)
    tool_model_counts: dict[str, dict[str, int]] = field(default_factory=dict)
    model_counts: dict[str, int] = field(default_factory=dict)
    # Per-model OUTPUT-token attribution (claude-watch dashboard panels,
    # 2026-08-22, Andrew #5370: tokens = cumulative OUTPUT, split BY MODEL).
    # Same delta-by-message-id logic as ``output_tokens`` below, just also
    # bucketed by the model that produced the message.
    model_output_tokens: dict[str, int] = field(default_factory=dict)
    queue_id: str | None = None
    first_user_text: str | None = None
    description: str | None = None
    agent_type: str | None = None
    meta_loaded: bool = False
    mtime: float = 0.0
    size: int = 0
    parse_errors: int = 0

    # -- folding ---------------------------------------------------------

    def feed_entry(self, entry: dict) -> None:
        """Fold one parsed JSONL entry into the counters."""
        etype = entry.get("type")
        ts = entry.get("timestamp")
        if isinstance(ts, str):
            if self.started_at is None:
                self.started_at = ts
            self.last_ts = ts
        if self.session_id is None and isinstance(entry.get("sessionId"), str):
            self.session_id = entry["sessionId"]

        if etype == "user":
            self.last_entry_type = "user"
            self.last_entry_had_tool_use = False
            if self.first_user_text is None:
                text = _content_text(entry.get("message", {}).get("content"))
                if text:
                    self.first_user_text = text
                    m = _QUEUE_RE.search(text)
                    if m:
                        self.queue_id = m.group(1)
            return

        if etype != "assistant":
            # attachments / system / summary rows: not a turn, ignore for the
            # finished check too (they never follow the final text).
            return

        msg = entry.get("message") or {}
        content = msg.get("content")
        model = msg.get("model")
        model = model if isinstance(model, str) and model else None
        had_tool = False
        if isinstance(content, list):
            for block in content:
                if isinstance(block, dict) and block.get("type") == "tool_use":
                    self.tool_calls += 1
                    had_tool = True
                    name = block.get("name")
                    if isinstance(name, str):
                        self.last_tool = name
                        self.tool_counts[name] = self.tool_counts.get(name, 0) + 1
                        if model:
                            per_model = self.tool_model_counts.setdefault(name, {})
                            per_model[model] = per_model.get(model, 0) + 1
        usage = msg.get("usage")
        if isinstance(usage, dict):
            ctx = (
                _int(usage.get("input_tokens"))
                + _int(usage.get("cache_creation_input_tokens"))
                + _int(usage.get("cache_read_input_tokens"))
            )
            if ctx > 0:
                self.context_tokens = ctx
            # Claude Code splits ONE API message into several consecutive JSONL
            # entries (thinking / text / tool_use blocks), each carrying the
            # usage AS OF that block — i.e. output_tokens is CUMULATIVE within a
            # message id. Fold the per-message delta so the sum counts each
            # message's FINAL output once (summing every entry overcounts, taking
            # only the first undercounts ~5x — measured on a real transcript).
            mid = msg.get("id")
            out = _int(usage.get("output_tokens"))
            if isinstance(mid, str) and mid == self.last_usage_msg_id:
                if out > self._last_msg_out:
                    delta = out - self._last_msg_out
                    self.output_tokens += delta
                    if model:
                        self.model_output_tokens[model] = (
                            self.model_output_tokens.get(model, 0) + delta
                        )
                    self._last_msg_out = out
            else:
                self.output_tokens += out
                self.last_usage_msg_id = mid if isinstance(mid, str) else None
                self._last_msg_out = out
                if model:
                    self.model_output_tokens[model] = (
                        self.model_output_tokens.get(model, 0) + out
                    )
                # Count once per distinct message id (i.e. once per assistant
                # turn, same granularity as the output_tokens delta above) —
                # NOT once per split JSONL entry, which would inflate the
                # model-use total ~5x for the same reason output_tokens would.
                if model:
                    self.model_counts[model] = self.model_counts.get(model, 0) + 1
        self.last_entry_type = "assistant"
        self.last_entry_had_tool_use = had_tool
        sr = msg.get("stop_reason")
        self.last_stop_reason = sr if isinstance(sr, str) else None

    def feed_bytes(self, data: bytes) -> None:
        """Fold raw appended bytes; keeps an incomplete trailing line buffered."""
        buf = self.partial + data
        lines = buf.split(b"\n")
        self.partial = lines.pop()  # b"" when data ended with a newline
        for raw in lines:
            raw = raw.strip()
            if not raw:
                continue
            try:
                entry = json.loads(raw)
            except ValueError:
                self.parse_errors += 1
                continue
            if isinstance(entry, dict):
                self.feed_entry(entry)

    def refresh(self, st: os.stat_result | None = None) -> bool:
        """Read anything appended since the last call. Returns True if it read."""
        try:
            st = st or os.stat(self.path)
        except OSError:
            return False
        self.mtime = st.st_mtime
        if st.st_size < self.offset:
            # Truncated / rewritten: start over.
            self._reset_counters()
        if st.st_size == self.offset:
            return False
        with open(self.path, "rb") as f:
            f.seek(self.offset)
            data = f.read()
        self.offset += len(data)
        self.size = self.offset
        self.feed_bytes(data)
        return True

    def _reset_counters(self) -> None:
        self.offset = 0
        self.partial = b""
        self.tool_calls = 0
        self.output_tokens = 0
        self.context_tokens = 0
        self.started_at = None
        self.last_ts = None
        self.last_tool = None
        self.last_entry_type = None
        self.last_stop_reason = None
        self.last_entry_had_tool_use = False
        self.last_usage_msg_id = None
        self._last_msg_out = 0
        self.tool_counts = {}
        self.tool_model_counts = {}
        self.model_counts = {}
        self.model_output_tokens = {}

    def load_meta(self) -> None:
        """Read the sibling ``<agent-id>.meta.json`` (description, agentType) once."""
        if self.meta_loaded:
            return
        meta_path = os.path.splitext(self.path)[0] + ".meta.json"
        try:
            with open(meta_path) as f:
                meta = json.load(f)
        except (OSError, ValueError):
            return
        self.meta_loaded = True
        if isinstance(meta, dict):
            d = meta.get("description")
            if isinstance(d, str) and d.strip():
                self.description = d.strip()
            t = meta.get("agentType")
            if isinstance(t, str) and t.strip():
                self.agent_type = t.strip()

    # -- derived ---------------------------------------------------------

    @property
    def finished(self) -> bool:
        """True once the agent has RETURNED: its last entry is a terminal turn."""
        return (
            self.last_entry_type == "assistant"
            and self.last_stop_reason == "end_turn"
            and not self.last_entry_had_tool_use
        )

    def display_description(self) -> str:
        if self.description:
            return self.description
        if self.first_user_text:
            first = self.first_user_text.strip().splitlines()
            for line in first:
                line = line.strip()
                if line and not _QUEUE_RE.match(line):
                    return line[:80]
        return self.agent_id

    def to_record(self, now: float) -> dict:
        age = max(0.0, now - self.mtime) if self.mtime else None
        return {
            "agent_id": self.agent_id,
            "session_id": self.session_id,
            "description": self.display_description(),
            "agent_type": self.agent_type,
            "queue_id": self.queue_id,
            "tool_calls": self.tool_calls,
            "context_tokens": self.context_tokens,
            "output_tokens": self.output_tokens,
            "last_tool": self.last_tool,
            "started_at": self.started_at,
            "last_write_at": self.last_ts,
            "age_seconds": round(age, 1) if age is not None else None,
            "finished": self.finished,
        }


def _int(v) -> int:
    return v if isinstance(v, int) and not isinstance(v, bool) else 0


def _content_text(content) -> str:
    if isinstance(content, str):
        return content
    if isinstance(content, list):
        parts = []
        for block in content:
            if isinstance(block, dict) and block.get("type") == "text":
                t = block.get("text")
                if isinstance(t, str):
                    parts.append(t)
        return "\n".join(parts)
    return ""


# ---------------------------------------------------------------------------
# Producer: the collector (scan + fold + snapshot)
# ---------------------------------------------------------------------------


def iter_subagent_transcripts(projects_dir: Path) -> Iterable[tuple[str, os.stat_result]]:
    """Yield ``(path, stat)`` for every ``*/*/subagents/agent-*.jsonl``.

    Uses ``os.scandir`` (one syscall per directory level + one stat per file);
    cheap even with hundreds of finished transcripts lying around.
    """
    try:
        slugs = list(os.scandir(projects_dir))
    except OSError:
        return
    for slug in slugs:
        if not slug.is_dir(follow_symlinks=False):
            continue
        try:
            sessions = list(os.scandir(slug.path))
        except OSError:
            continue
        for sess in sessions:
            if not sess.is_dir(follow_symlinks=False):
                continue
            sub = os.path.join(sess.path, "subagents")
            try:
                files = list(os.scandir(sub))
            except OSError:
                continue
            for f in files:
                name = f.name
                if not (name.startswith("agent-") and name.endswith(".jsonl")):
                    continue
                try:
                    st = f.stat()
                except OSError:
                    continue
                yield f.path, st


def find_main_transcript(projects_dir: Path) -> tuple[str, os.stat_result] | None:
    """Newest top-level ``<slug>/<session>.jsonl`` = the active main loop."""
    best: tuple[str, os.stat_result] | None = None
    try:
        slugs = list(os.scandir(projects_dir))
    except OSError:
        return None
    for slug in slugs:
        if not slug.is_dir(follow_symlinks=False):
            continue
        try:
            entries = list(os.scandir(slug.path))
        except OSError:
            continue
        for e in entries:
            if not e.name.endswith(".jsonl") or not e.is_file(follow_symlinks=False):
                continue
            try:
                st = e.stat()
            except OSError:
                continue
            if best is None or st.st_mtime > best[1].st_mtime:
                best = (e.path, st)
    return best


def parse_main_tail(path: str, tail_bytes: int = MAIN_TAIL_BYTES) -> dict | None:
    """Latest ``usage`` in the transcript tail -> ``{context_tokens, last_write_at}``."""
    try:
        size = os.path.getsize(path)
        with open(path, "rb") as f:
            if size > tail_bytes:
                f.seek(size - tail_bytes)
                f.readline()  # drop the partial first line
            data = f.read()
    except OSError:
        return None
    ctx = None
    ts = None
    for raw in data.split(b"\n"):
        raw = raw.strip()
        if not raw or b'"usage"' not in raw:
            continue
        try:
            entry = json.loads(raw)
        except ValueError:
            continue
        if not isinstance(entry, dict) or entry.get("type") != "assistant":
            continue
        usage = (entry.get("message") or {}).get("usage")
        if not isinstance(usage, dict):
            continue
        val = (
            _int(usage.get("input_tokens"))
            + _int(usage.get("cache_creation_input_tokens"))
            + _int(usage.get("cache_read_input_tokens"))
        )
        if val > 0:
            ctx = val
            t = entry.get("timestamp")
            ts = t if isinstance(t, str) else ts
    if ctx is None:
        return None
    return {"context_tokens": ctx, "last_write_at": ts}


class Collector:
    """Holds per-agent incremental state across ticks; builds snapshots."""

    def __init__(
        self,
        projects_dir: Path | None = None,
        live_window_secs: float = DEFAULT_LIVE_WINDOW_SECS,
        host: str | None = None,
    ):
        self.projects_dir = Path(projects_dir) if projects_dir else default_projects_dir()
        self.live_window_secs = float(live_window_secs)
        self.host = host or socket.gethostname()
        self.agents: dict[str, AgentState] = {}
        self._main_cache: tuple[tuple[str, float, int] | None, dict | None] = (None, None)
        # Bounded-tail AgentState for the main-loop transcript itself (see
        # ``_main`` below) -- folds the main loop's OWN tool/model usage into
        # ``tool_totals``/``model_totals`` the same way subagent transcripts
        # already do. Kept separate from ``self.agents`` (which is keyed by
        # subagent path and forgotten once a path falls out of the live
        # window) because the main transcript is a single, open-ended file
        # that never "falls out of window" the way a finished subagent does.
        self._main_state: AgentState | None = None

    def tick(self, now: float | None = None) -> dict:
        now = time.time() if now is None else now
        cutoff = now - self.live_window_secs
        live: list[AgentState] = []
        seen_paths: set[str] = set()

        for path, st in iter_subagent_transcripts(self.projects_dir):
            if st.st_mtime < cutoff:
                continue
            seen_paths.add(path)
            state = self.agents.get(path)
            if state is None:
                agent_id = os.path.basename(path)[len("agent-"):-len(".jsonl")]
                state = AgentState(path=path, agent_id=agent_id)
                self.agents[path] = state
            state.refresh(st)
            state.load_meta()
            if state.finished:
                continue
            live.append(state)

        # Forget state for transcripts that fell out of the window (bounded memory).
        for path in list(self.agents):
            if path not in seen_paths:
                del self.agents[path]

        live.sort(key=lambda s: s.started_at or "", reverse=False)
        records = [s.to_record(now) for s in live]

        main = self._main(now)

        totals = {
            "agents": len(records),
            # Distinct subagent transcripts observed in the live/recent window
            # (``seen_paths``: live AND recently-finished) -- a windowed proxy
            # for "agents spawned", same semantics as tool_totals/model_totals
            # below (recomputed each tick, not a monotonic lifetime counter).
            "agents_spawned": len(seen_paths),
            "tool_calls": sum(r["tool_calls"] for r in records),
            "context_tokens": sum(r["context_tokens"] for r in records),
            "output_tokens": sum(r["output_tokens"] for r in records),
        }

        # Fleet-wide tool/model breakdowns (claude-watch #663) — summed over
        # every transcript still IN the live window (``seen_paths``: live AND
        # recently-finished), not just the still-running ``live`` list, so a
        # tool call doesn't vanish from the totals the instant its agent
        # returns. Same "bounded by the live window" semantics as everything
        # else in this snapshot: these are gauges recomputed each tick, not
        # monotonic counters that survive a transcript falling out of window
        # or a producer restart.
        tool_totals: dict[str, int] = {}
        model_totals: dict[str, int] = {}
        tool_model_totals: dict[str, dict[str, int]] = {}
        model_output_tokens_totals: dict[str, int] = {}
        for path in seen_paths:
            state = self.agents[path]
            for name, cnt in state.tool_counts.items():
                tool_totals[name] = tool_totals.get(name, 0) + cnt
            for model, cnt in state.model_counts.items():
                model_totals[model] = model_totals.get(model, 0) + cnt
            for name, per_model in state.tool_model_counts.items():
                dest = tool_model_totals.setdefault(name, {})
                for model, cnt in per_model.items():
                    dest[model] = dest.get(model, 0) + cnt
            for model, tok in state.model_output_tokens.items():
                model_output_tokens_totals[model] = model_output_tokens_totals.get(model, 0) + tok

        # The main loop's OWN turns (claude-watch #663 follow-up, 2026-08-22):
        # the loop above only ever walks subagent transcripts, so a solo
        # operator running the main loop on one model (e.g. Opus) and
        # subagents on another (e.g. Sonnet, per the "always Sonnet for
        # subagents" convention) got a ``claude_model_use_total`` that showed
        # ONLY the subagent model -- the main loop's turns were silently
        # excluded, not merely undercounted. ``_main`` (below) keeps
        # ``self._main_state`` fed off the SAME bounded tail window used for
        # ``main.context_tokens``, through the identical per-entry
        # tool/model-folding path subagents use, so fold it in here too.
        main_state = self._main_state
        if main_state is not None:
            for name, cnt in main_state.tool_counts.items():
                tool_totals[name] = tool_totals.get(name, 0) + cnt
            for model, cnt in main_state.model_counts.items():
                model_totals[model] = model_totals.get(model, 0) + cnt
            for name, per_model in main_state.tool_model_counts.items():
                dest = tool_model_totals.setdefault(name, {})
                for model, cnt in per_model.items():
                    dest[model] = dest.get(model, 0) + cnt
            for model, tok in main_state.model_output_tokens.items():
                model_output_tokens_totals[model] = model_output_tokens_totals.get(model, 0) + tok

        return {
            "version": SNAPSHOT_VERSION,
            "host": self.host,
            "generated_at": now,
            "generated_at_iso": _iso(now),
            "live_window_seconds": self.live_window_secs,
            "main": main,
            "agents": records,
            "totals": totals,
            "tool_totals": tool_totals,
            "model_totals": model_totals,
            "tool_model_totals": tool_model_totals,
            "model_output_tokens_totals": model_output_tokens_totals,
        }

    def _main(self, now: float) -> dict | None:
        found = find_main_transcript(self.projects_dir)
        if found is None:
            self._main_cache = (None, None)
            self._main_state = None
            return None
        path, st = found
        key = (path, st.st_mtime, st.st_size)
        cached_key, cached = self._main_cache
        if cached_key == key and cached is not None:
            parsed = cached
        else:
            parsed = parse_main_tail(path)
            self._main_cache = (key, parsed)

        # Fold the same bounded tail window through the per-entry tool/model
        # counters (see the ``_main_state`` comment in ``__init__`` and the
        # fold-in at the end of ``tick``). Seed a fresh ``AgentState``'s
        # offset to the tail window the FIRST time this path is seen (or when
        # the active session rolls over to a new file) so this never re-parses
        # more than ``MAIN_TAIL_BYTES`` of what can be a many-MB, open-ended
        # transcript; every tick after that is ``AgentState.refresh``'s normal
        # incremental read of just the newly appended bytes -- no extra cost
        # over the read ``parse_main_tail`` above was already doing.
        main_state = self._main_state
        if main_state is None or main_state.path != path:
            main_state = AgentState(path=path, agent_id="main")
            main_state.offset = max(0, st.st_size - MAIN_TAIL_BYTES)
            self._main_state = main_state
        main_state.refresh(st)

        if parsed is None:
            return None
        session_id = os.path.splitext(os.path.basename(path))[0]
        return {
            "session_id": session_id,
            "context_tokens": parsed["context_tokens"],
            "last_write_at": parsed.get("last_write_at"),
            "age_seconds": round(max(0.0, now - st.st_mtime), 1),
        }


def _iso(ts: float) -> str:
    return time.strftime("%Y-%m-%dT%H:%M:%S", time.gmtime(ts)) + "Z"


def write_snapshot(snapshot: dict, path: Path | str) -> None:
    """Atomically replace ``path`` with the JSON snapshot (tmp + ``os.replace``).

    The consumer (queue-minisite) re-reads this file on every poll; a
    half-written file would surface as a JSON error, so never write in place.
    Callers bind-mounting the snapshot into a container must mount its
    DIRECTORY, not the file — the rename gives the file a new inode on every
    write and a single-file bind mount pins the old one.
    """
    path = Path(path)
    path.parent.mkdir(parents=True, exist_ok=True)
    fd, tmp = tempfile.mkstemp(prefix=".agent-stats.", suffix=".tmp", dir=str(path.parent))
    try:
        with os.fdopen(fd, "w") as f:
            json.dump(snapshot, f, separators=(",", ":"))
            f.write("\n")
        os.chmod(tmp, 0o644)
        os.replace(tmp, path)
    except BaseException:
        try:
            os.unlink(tmp)
        except OSError:
            pass
        raise
