"""cw-agent-stats tests — the vendored ``agentstats`` survey library + the CLI.

Vendored from botchat (``tests/test_agent_stats.py``) when the agent-activity
feature left botchat for claude-watch (2026-08-22). Covers:

* the PRODUCER fold (transcript JSONL -> per-agent tool calls / context tokens /
  output tokens / liveness), incremental re-reads, the live window, the
  main-loop tail parse, the atomic snapshot write;
* the snapshot SCHEMA the queue-minisite consumer joins on (top-level keys +
  per-agent record keys) — a byte-compatibility pin for the move;
* the ``cw-agent-stats`` CLI: ``--out`` default resolution (new
  ``CW_AGENT_STATS_OUT`` / ``CLAUDE_WATCH_STATE_DIR`` knobs, the deprecated
  ``BOTCHAT_DATA_DIR`` fallback), the Prometheus textfile rendering, and an
  end-to-end ``--once`` run against a fixture tree.

All against tmp fixtures — never the real ``~/.claude/projects`` or state dir.

Run::

    make test-cw-agent-stats
    # or: uv run --python 3.11 --with pytest pytest tools/cw-agent-stats/tests/ -v
"""

from __future__ import annotations

import importlib.machinery
import importlib.util
import json
import os
import subprocess
import sys
import time
from pathlib import Path

import pytest

HERE = Path(__file__).resolve().parent
TOOL_DIR = HERE.parent
CLI = TOOL_DIR / "cw-agent-stats"

sys.path.insert(0, str(TOOL_DIR))

import agentstats  # noqa: E402


def _load_cli():
    """Import the extension-less CLI script as a module (for unit-level tests)."""
    loader = importlib.machinery.SourceFileLoader("cw_agent_stats_cli", str(CLI))
    spec = importlib.util.spec_from_loader("cw_agent_stats_cli", loader)
    mod = importlib.util.module_from_spec(spec)
    loader.exec_module(mod)
    return mod


# ---------------------------------------------------------------------------
# Fixture: a synthetic ~/.claude/projects tree
# ---------------------------------------------------------------------------


def _usage(inp=100, cc=0, cr=0, out=0):
    return {
        "input_tokens": inp,
        "cache_creation_input_tokens": cc,
        "cache_read_input_tokens": cr,
        "output_tokens": out,
    }


def _user(text, ts="2026-08-21T20:00:00.000Z", session="sess-1"):
    return {"type": "user", "timestamp": ts, "sessionId": session,
            "message": {"role": "user", "content": text}}


def _tool_result(ts):
    return {"type": "user", "timestamp": ts,
            "message": {"role": "user", "content": [{"type": "tool_result", "content": "ok"}]}}


def _assistant(blocks, *, mid, stop, usage, ts, model=None):
    msg = {"id": mid, "role": "assistant", "content": blocks,
           "stop_reason": stop, "usage": usage}
    if model is not None:
        msg["model"] = model
    return {"type": "assistant", "timestamp": ts, "message": msg}


def _write_jsonl(path: Path, entries, *, append=False):
    path.parent.mkdir(parents=True, exist_ok=True)
    with open(path, "a" if append else "w") as f:
        f.writelines(json.dumps(e) + "\n" for e in entries)


@pytest.fixture
def projects(tmp_path, monkeypatch):
    """A projects dir with one session: a main transcript + two subagents.

    * ``agent-running``: user(Queue item) -> thinking/tool_use (same message,
      cumulative usage) -> tool_result -> tool_use (new message)  => LIVE.
    * ``agent-done``:    user -> tool_use -> tool_result -> text end_turn => FINISHED.
    """
    root = tmp_path / "projects"
    slug = root / "-home-x"
    sess = slug / "sess-1"
    sub = sess / "subagents"

    # Main-loop transcript (top-level <session>.jsonl): two usage blocks, the
    # last one wins.
    _write_jsonl(slug / "sess-1.jsonl", [
        _user("hello"),
        _assistant([{"type": "text", "text": "hi"}], mid="m-main-1", stop="end_turn",
                   usage=_usage(10, 20, 1000, 5), ts="2026-08-21T20:00:01.000Z"),
        _assistant([{"type": "text", "text": "again"}], mid="m-main-2", stop="end_turn",
                   usage=_usage(2, 469, 488286, 40), ts="2026-08-21T20:00:02.000Z"),
    ])

    running = sub / "agent-running.jsonl"
    _write_jsonl(running, [
        _user("Queue item: q-2026-08-21-abcd\n\nYou are a coding agent on ~/repos/x."),
        # ONE api message split into two entries; usage.output_tokens cumulative (7 -> 180).
        _assistant([{"type": "thinking", "thinking": "…"}], mid="m1", stop=None,
                   usage=_usage(101, 946, 77464, 7), ts="2026-08-21T20:00:05.000Z"),
        _assistant([{"type": "tool_use", "id": "t1", "name": "Bash", "input": {}}], mid="m1",
                   stop="tool_use", usage=_usage(101, 946, 77464, 180), ts="2026-08-21T20:00:06.000Z"),
        _tool_result("2026-08-21T20:00:07.000Z"),
        _assistant([{"type": "tool_use", "id": "t2", "name": "Read", "input": {}}], mid="m2",
                   stop="tool_use", usage=_usage(101, 1224, 78410, 50), ts="2026-08-21T20:00:08.000Z"),
    ])
    (sub / "agent-running.meta.json").write_text(json.dumps({
        "agentType": "general-purpose", "description": "botchat live counter"}))

    done = sub / "agent-done.jsonl"
    _write_jsonl(done, [
        _user("Queue item: q-2026-08-21-ffff\nDo a thing."),
        _assistant([{"type": "tool_use", "id": "t9", "name": "Bash", "input": {}}], mid="d1",
                   stop="tool_use", usage=_usage(50, 0, 0, 20), ts="2026-08-21T20:00:05.000Z"),
        _tool_result("2026-08-21T20:00:06.000Z"),
        _assistant([{"type": "text", "text": "all done"}], mid="d2", stop="end_turn",
                   usage=_usage(60, 0, 0, 30), ts="2026-08-21T20:00:07.000Z"),
    ])
    # no meta.json for agent-done on purpose -> description falls back to the prompt.

    monkeypatch.setenv("CLAUDE_PROJECTS_DIR", str(root))
    return root


# ---------------------------------------------------------------------------
# Producer: fold + liveness + incremental + main tail + snapshot write
# ---------------------------------------------------------------------------


def test_collector_folds_live_agents_and_excludes_finished(projects):
    c = agentstats.Collector(projects_dir=projects, host="testhost")
    snap = c.tick()

    assert snap["version"] == agentstats.SNAPSHOT_VERSION
    assert snap["host"] == "testhost"
    assert [a["agent_id"] for a in snap["agents"]] == ["running"]
    a = snap["agents"][0]
    assert a["description"] == "botchat live counter"       # from meta.json
    assert a["agent_type"] == "general-purpose"
    assert a["queue_id"] == "q-2026-08-21-abcd"
    assert a["tool_calls"] == 2
    # context = LATEST usage's input + cache_creation + cache_read
    assert a["context_tokens"] == 101 + 1224 + 78410
    # output = per-message FINAL output (m1: 180 cumulative, not 7+180; m2: 50)
    assert a["output_tokens"] == 180 + 50
    assert a["last_tool"] == "Read"
    assert a["started_at"] == "2026-08-21T20:00:00.000Z"
    assert a["last_write_at"] == "2026-08-21T20:00:08.000Z"
    assert a["finished"] is False
    assert a["session_id"] == "sess-1"

    assert snap["totals"] == {
        "agents": 1, "agents_spawned": 2, "tool_calls": 2,
        "context_tokens": 101 + 1224 + 78410, "output_tokens": 230,
    }
    # main loop: the LAST usage in the newest top-level transcript
    assert snap["main"]["session_id"] == "sess-1"
    assert snap["main"]["context_tokens"] == 2 + 469 + 488286
    assert snap["main"]["last_write_at"] == "2026-08-21T20:00:02.000Z"


def test_finished_agent_state_and_description_fallback(projects):
    c = agentstats.Collector(projects_dir=projects)
    c.tick()
    done = c.agents[str(projects / "-home-x" / "sess-1" / "subagents" / "agent-done.jsonl")]
    assert done.finished is True
    assert done.tool_calls == 1
    assert done.output_tokens == 50
    assert done.queue_id == "q-2026-08-21-ffff"
    # no meta.json -> first non-"Queue item" line of the prompt
    assert done.display_description() == "Do a thing."


def test_collector_is_incremental_on_append(projects):
    c = agentstats.Collector(projects_dir=projects)
    c.tick()
    path = projects / "-home-x" / "sess-1" / "subagents" / "agent-running.jsonl"
    state = c.agents[str(path)]
    offset_before = state.offset
    assert offset_before == path.stat().st_size

    _write_jsonl(path, [
        _tool_result("2026-08-21T20:00:09.000Z"),
        _assistant([{"type": "tool_use", "id": "t3", "name": "Edit", "input": {}}], mid="m3",
                   stop="tool_use", usage=_usage(101, 10, 80000, 9), ts="2026-08-21T20:00:10.000Z"),
    ], append=True)
    # Force a visible mtime change even on coarse filesystems.
    now = time.time()
    os.utime(path, (now, now))

    snap = c.tick()
    a = snap["agents"][0]
    assert a["tool_calls"] == 3
    assert a["output_tokens"] == 230 + 9
    assert a["context_tokens"] == 101 + 10 + 80000
    assert a["last_tool"] == "Edit"
    assert state.offset > offset_before            # only the tail was read
    assert state.parse_errors == 0


def test_partial_trailing_line_is_buffered(projects):
    c = agentstats.Collector(projects_dir=projects)
    c.tick()
    path = projects / "-home-x" / "sess-1" / "subagents" / "agent-running.jsonl"
    entry = json.dumps(_assistant(
        [{"type": "tool_use", "id": "t4", "name": "Grep", "input": {}}], mid="m4",
        stop="tool_use", usage=_usage(1, 0, 0, 1), ts="2026-08-21T20:00:11.000Z"))
    cut = len(entry) // 2
    with open(path, "a") as f:
        f.write(entry[:cut])              # half a line, no newline
    c.tick()
    assert c.agents[str(path)].tool_calls == 2     # not counted yet, not an error
    assert c.agents[str(path)].parse_errors == 0
    with open(path, "a") as f:
        f.write(entry[cut:] + "\n")
    now = time.time() + 1
    os.utime(path, (now, now))
    c.tick()
    assert c.agents[str(path)].tool_calls == 3


def test_live_window_excludes_old_transcripts(projects):
    path = projects / "-home-x" / "sess-1" / "subagents" / "agent-running.jsonl"
    old = time.time() - 3600
    os.utime(path, (old, old))
    c = agentstats.Collector(projects_dir=projects, live_window_secs=900)
    snap = c.tick()
    assert snap["agents"] == []
    assert snap["totals"]["agents"] == 0
    assert str(path) not in c.agents               # state dropped, bounded memory


def test_parse_main_tail_handles_large_prefix(tmp_path):
    path = tmp_path / "sess.jsonl"
    filler = [_user("x" * 5000, ts=f"2026-08-21T19:00:{i % 60:02d}.000Z") for i in range(200)]
    _write_jsonl(path, filler + [
        _assistant([{"type": "text", "text": "t"}], mid="z", stop="end_turn",
                   usage=_usage(5, 5, 90, 1), ts="2026-08-21T20:30:00.000Z"),
    ])
    assert path.stat().st_size > agentstats.MAIN_TAIL_BYTES
    got = agentstats.parse_main_tail(str(path))
    assert got == {"context_tokens": 100, "last_write_at": "2026-08-21T20:30:00.000Z"}


def test_main_loop_model_and_tools_are_folded_into_totals(tmp_path, monkeypatch):
    """The main loop's OWN turns must land in tool_totals/model_totals too.

    Regression for the bug where ``claude_model_use_total`` only ever showed
    the SUBAGENT model (e.g. Sonnet, per the "always Sonnet for subagents"
    convention): the fleet-wide fold walked ``iter_subagent_transcripts``
    only, and the main-loop transcript was scanned solely via
    ``parse_main_tail`` for its latest context-token reading -- never through
    the per-entry tool/model-folding path subagents get. A solo operator
    running the main loop on a different model than their subagents (Opus vs
    Sonnet) saw the main loop's model silently excluded, not merely
    undercounted.
    """
    root = tmp_path / "projects"
    slug = root / "-home-x"
    sub = slug / "sess-1" / "subagents"

    _write_jsonl(slug / "sess-1.jsonl", [
        _user("hello"),
        _assistant([{"type": "tool_use", "id": "t1", "name": "WebSearch", "input": {}}],
                   mid="m-main-1", stop="tool_use", usage=_usage(10, 20, 1000, 5),
                   ts="2026-08-21T20:00:01.000Z", model="claude-opus-4-8"),
        _assistant([{"type": "text", "text": "done"}], mid="m-main-2", stop="end_turn",
                   usage=_usage(2, 469, 488286, 40), ts="2026-08-21T20:00:02.000Z",
                   model="claude-opus-4-8"),
    ])

    running = sub / "agent-running.jsonl"
    _write_jsonl(running, [
        _user("Queue item: q-2026-08-21-abcd\n\nYou are a coding agent."),
        _assistant([{"type": "tool_use", "id": "t2", "name": "Bash", "input": {}}], mid="m1",
                   stop="tool_use", usage=_usage(101, 946, 77464, 7),
                   ts="2026-08-21T20:00:05.000Z", model="claude-sonnet-5"),
    ])

    monkeypatch.setenv("CLAUDE_PROJECTS_DIR", str(root))
    snap = agentstats.Collector(projects_dir=root).tick()

    assert snap["model_totals"] == {"claude-opus-4-8": 2, "claude-sonnet-5": 1}
    assert snap["tool_totals"] == {"WebSearch": 1, "Bash": 1}
    assert snap["tool_model_totals"] == {
        "WebSearch": {"claude-opus-4-8": 1},
        "Bash": {"claude-sonnet-5": 1},
    }
    # Output tokens, folded by model, must ALSO include the main loop's own
    # turns (same regression class as model_totals above): main loop emits
    # 5 + 40 output tokens across two distinct messages on claude-opus-4-8;
    # the subagent emits 7 output tokens on claude-sonnet-5.
    assert snap["model_output_tokens_totals"] == {"claude-opus-4-8": 45, "claude-sonnet-5": 7}
    assert snap["totals"]["agents_spawned"] == 1


def test_main_loop_model_fold_is_incremental_not_full_reparse(tmp_path, monkeypatch):
    """Cold start seeds the main AgentState offset to the MAIN_TAIL_BYTES tail
    (never a full re-parse of a potentially many-MB, open-ended main
    transcript); later ticks only cost the newly appended bytes."""
    root = tmp_path / "projects"
    slug = root / "-home-x"
    path = slug / "sess-1.jsonl"

    filler = [_user("x" * 5000, ts=f"2026-08-21T19:00:{i % 60:02d}.000Z") for i in range(200)]
    _write_jsonl(path, filler + [
        _assistant([{"type": "text", "text": "t"}], mid="z1", stop="end_turn",
                   usage=_usage(5, 5, 90, 1), ts="2026-08-21T20:30:00.000Z",
                   model="claude-opus-4-8"),
    ])
    assert path.stat().st_size > agentstats.MAIN_TAIL_BYTES

    monkeypatch.setenv("CLAUDE_PROJECTS_DIR", str(root))
    c = agentstats.Collector(projects_dir=root)
    snap = c.tick()
    assert snap["model_totals"] == {"claude-opus-4-8": 1}
    assert c._main_state.offset >= path.stat().st_size - agentstats.MAIN_TAIL_BYTES

    _write_jsonl(path, [
        _assistant([{"type": "text", "text": "t2"}], mid="z2", stop="end_turn",
                   usage=_usage(5, 5, 90, 2), ts="2026-08-21T20:31:00.000Z",
                   model="claude-opus-4-8"),
    ], append=True)
    snap = c.tick()
    assert snap["model_totals"] == {"claude-opus-4-8": 2}


def test_model_output_tokens_delta_by_message_id_not_double_counted(tmp_path, monkeypatch):
    """Per-model output-token folding must use the SAME delta-by-message-id
    logic as ``output_tokens`` (claude-watch dashboard panels, Andrew #5370:
    tokens = cumulative OUTPUT, split BY MODEL) — summing every cumulative
    usage reading for a split message would overcount ~5x, same failure mode
    the plain ``output_tokens`` delta logic already guards against.
    """
    root = tmp_path / "projects"
    slug = root / "-home-x"
    sub = slug / "sess-1" / "subagents"

    running = sub / "agent-running.jsonl"
    _write_jsonl(running, [
        _user("Queue item: q-2026-08-22-mmmm\n\nDo a thing."),
        # ONE api message split into two entries; usage.output_tokens cumulative
        # (7 -> 180) -- only the 173-token DELTA should land in the model bucket.
        _assistant([{"type": "thinking", "thinking": "…"}], mid="m1", stop=None,
                   usage=_usage(101, 946, 77464, 7), ts="2026-08-21T20:00:05.000Z",
                   model="claude-sonnet-5"),
        _assistant([{"type": "tool_use", "id": "t1", "name": "Bash", "input": {}}], mid="m1",
                   stop="tool_use", usage=_usage(101, 946, 77464, 180), ts="2026-08-21T20:00:06.000Z",
                   model="claude-sonnet-5"),
        _tool_result("2026-08-21T20:00:07.000Z"),
        # A second, distinct message on a DIFFERENT model.
        _assistant([{"type": "tool_use", "id": "t2", "name": "Read", "input": {}}], mid="m2",
                   stop="tool_use", usage=_usage(101, 1224, 78410, 50), ts="2026-08-21T20:00:08.000Z",
                   model="claude-opus-4-8"),
    ])

    monkeypatch.setenv("CLAUDE_PROJECTS_DIR", str(root))
    snap = agentstats.Collector(projects_dir=root).tick()

    assert snap["model_output_tokens_totals"] == {"claude-sonnet-5": 180, "claude-opus-4-8": 50}
    # Sanity: matches the plain (model-agnostic) output_tokens total for the agent.
    assert snap["totals"]["output_tokens"] == 230


def test_write_snapshot_is_atomic_and_readable(tmp_path, projects):
    out = tmp_path / "data" / "agent-stats.json"
    snap = agentstats.Collector(projects_dir=projects).tick()
    agentstats.write_snapshot(snap, out)
    assert json.loads(out.read_text())["totals"]["agents"] == 1
    # no temp files left behind
    assert [p.name for p in out.parent.iterdir()] == ["agent-stats.json"]
    # overwrite works (os.replace onto an existing file)
    agentstats.write_snapshot(snap, out)
    assert json.loads(out.read_text())["version"] == agentstats.SNAPSHOT_VERSION


# ---------------------------------------------------------------------------
# Snapshot schema pin — the contract with queue-minisite/app.py
# (``_load_agent_stats`` joins ``agents[].queue_id`` onto running rows and
# reads the keys below; queue-minisite/test_agent_stats.py pins the same
# shape from the consumer side). Changing this list means bumping
# SNAPSHOT_VERSION and touching both tests.
# ---------------------------------------------------------------------------

SNAPSHOT_TOP_LEVEL_KEYS = {
    "version", "host", "generated_at", "generated_at_iso", "live_window_seconds",
    "main", "agents", "totals", "tool_totals", "model_totals", "tool_model_totals",
    "model_output_tokens_totals",
}
AGENT_RECORD_KEYS = {
    "agent_id", "session_id", "description", "agent_type", "queue_id",
    "tool_calls", "context_tokens", "output_tokens", "last_tool",
    "started_at", "last_write_at", "age_seconds", "finished",
}
MAIN_KEYS = {"session_id", "context_tokens", "last_write_at", "age_seconds"}
TOTALS_KEYS = {"agents", "agents_spawned", "tool_calls", "context_tokens", "output_tokens"}


def test_snapshot_schema_is_pinned(projects):
    snap = agentstats.Collector(projects_dir=projects, host="h").tick()
    assert set(snap) == SNAPSHOT_TOP_LEVEL_KEYS
    assert snap["version"] == 3 == agentstats.SNAPSHOT_VERSION
    assert set(snap["agents"][0]) == AGENT_RECORD_KEYS
    assert set(snap["main"]) == MAIN_KEYS
    assert set(snap["totals"]) == TOTALS_KEYS
    # fleet breakdowns fold over live AND recently-finished transcripts
    assert snap["tool_totals"] == {"Bash": 2, "Read": 1}
    assert snap["tool_model_totals"] == {}           # fixture entries carry no model id
    assert snap["model_totals"] == {}
    assert snap["model_output_tokens_totals"] == {}  # fixture entries carry no model id
    # agent-running (live) + agent-done (finished but still in the window)
    assert snap["totals"]["agents_spawned"] == 2


# ---------------------------------------------------------------------------
# CLI: --out default resolution, prom rendering, end-to-end --once
# ---------------------------------------------------------------------------

_ENV_KNOBS = ("CW_AGENT_STATS_OUT", "CLAUDE_WATCH_STATE_DIR", "BOTCHAT_DATA_DIR",
              "CW_AGENT_STATS_PROM_FILE", "CLAUDE_WATCH_PROM_FILE", "PYTHONPATH")


@pytest.fixture
def cli(monkeypatch):
    for var in _ENV_KNOBS:
        monkeypatch.delenv(var, raising=False)
    return _load_cli()


def _clean_env(**extra):
    env = {k: v for k, v in os.environ.items() if k not in _ENV_KNOBS}
    env.update(extra)
    return env


def test_default_out_is_claude_watch_state_dir(cli):
    assert cli._default_out() == Path("/var/lib/claude-watch/agent-stats.json")


def test_default_out_precedence(cli, monkeypatch, capsys):
    monkeypatch.setenv("BOTCHAT_DATA_DIR", "/legacy")
    assert cli._default_out() == Path("/legacy/agent-stats.json")
    assert "deprecated" in capsys.readouterr().err        # one-release fallback, warns
    monkeypatch.setenv("CLAUDE_WATCH_STATE_DIR", "/state")
    assert cli._default_out() == Path("/state/agent-stats.json")
    assert capsys.readouterr().err == ""                  # no warning on the supported path
    monkeypatch.setenv("CW_AGENT_STATS_OUT", "/explicit/snap.json")
    assert cli._default_out() == Path("/explicit/snap.json")


def test_default_out_ignores_botchat_config_file(cli, monkeypatch, tmp_path):
    """The old ~/.config/botchat/config lookup is gone: only env + default apply."""
    home = tmp_path / "home"
    (home / ".config" / "botchat").mkdir(parents=True)
    (home / ".config" / "botchat" / "config").write_text("BOTCHAT_DATA_DIR=/var/apps/botchat\n")
    monkeypatch.setenv("HOME", str(home))
    assert cli._default_out() == Path("/var/lib/claude-watch/agent-stats.json")


def test_default_prom_file_precedence(cli, monkeypatch):
    assert cli._default_prom_file() == Path("/var/lib/node-exporter/textfile/claude_agent_stats.prom")
    monkeypatch.setenv("CLAUDE_WATCH_PROM_FILE", "/tf/claude_watch.prom")
    assert cli._default_prom_file() == Path("/tf/claude_agent_stats.prom")   # sibling
    monkeypatch.setenv("CW_AGENT_STATS_PROM_FILE", "/x/y.prom")
    assert cli._default_prom_file() == Path("/x/y.prom")


def test_prom_lines_render_gauges(cli, projects):
    snap = agentstats.Collector(projects_dir=projects, host="h").tick()
    lines = cli._prom_lines(snap)
    text = "\n".join(lines)
    assert "claude_agent_live_count 1" in lines
    assert "claude_agent_spawned_count 2" in lines        # running + done, both in-window
    assert 'claude_agent_calls_total{agent_id="running"} 2' in lines
    assert 'claude_agent_tokens_total{agent_id="running",kind="context"} %d' % (101 + 1224 + 78410) in lines
    assert 'claude_agent_tokens_total{agent_id="running",kind="output"} 230' in lines
    assert "claude_agent_main_context_tokens %d" % (2 + 469 + 488286) in lines
    assert 'claude_tool_use_total{tool="Bash"} 2' in lines
    assert 'claude_tool_use_total{tool="Read"} 1' in lines
    assert "claude_model_use_total" not in text          # no model ids in the fixture
    assert "claude_output_tokens_total" not in text       # no model ids in the fixture
    # label escaping: backslash, double-quote, newline
    assert cli._esc_label('a"b\\c\n') == 'a\\"b\\\\c\\n'


def test_prom_lines_render_output_tokens_by_model(cli, tmp_path, monkeypatch):
    root = tmp_path / "projects"
    slug = root / "-home-x"
    sub = slug / "sess-1" / "subagents"
    _write_jsonl(sub / "agent-running.jsonl", [
        _user("Queue item: q-2026-08-22-nnnn\n\nDo a thing."),
        _assistant([{"type": "tool_use", "id": "t1", "name": "Bash", "input": {}}], mid="m1",
                   stop="tool_use", usage=_usage(101, 946, 77464, 30), ts="2026-08-21T20:00:05.000Z",
                   model="claude-sonnet-5"),
    ])
    monkeypatch.setenv("CLAUDE_PROJECTS_DIR", str(root))
    snap = agentstats.Collector(projects_dir=root, host="h").tick()
    lines = cli._prom_lines(snap)
    assert 'claude_output_tokens_total{model="claude-sonnet-5"} 30' in lines


def test_cli_once_writes_snapshot_and_prom(projects, tmp_path):
    """End-to-end: run the script the way cron does (minus --loop), no botchat anywhere."""
    out = tmp_path / "state" / "agent-stats.json"
    prom = tmp_path / "tf" / "claude_agent_stats.prom"
    env = _clean_env(CLAUDE_PROJECTS_DIR=str(projects))
    res = subprocess.run(
        [sys.executable, str(CLI), "--once", "--out", str(out), "--prom-file", str(prom), "--print"],
        capture_output=True, text=True, env=env, timeout=30,
    )
    assert res.returncode == 0, res.stderr
    snap = json.loads(out.read_text())
    assert set(snap) == SNAPSHOT_TOP_LEVEL_KEYS
    assert snap["totals"]["agents"] == 1
    assert json.loads(res.stdout)["totals"] == snap["totals"]     # --print mirrors the file
    assert "claude_agent_live_count 1" in prom.read_text().splitlines()
    assert [p.name for p in out.parent.iterdir()] == ["agent-stats.json"]   # atomic, no tmp left


def test_cli_default_out_honours_state_dir_env(projects, tmp_path):
    state = tmp_path / "cw-state"
    env = _clean_env(CLAUDE_PROJECTS_DIR=str(projects), CLAUDE_WATCH_STATE_DIR=str(state))
    res = subprocess.run([sys.executable, str(CLI), "--once", "--no-prom"],
                         capture_output=True, text=True, env=env, timeout=30)
    assert res.returncode == 0, res.stderr
    assert (state / "agent-stats.json").is_file()
    assert "deprecated" not in res.stderr


def test_cli_does_not_import_botchat():
    """The whole point of the vendoring: no sys.path pointing at a botchat checkout."""
    text = CLI.read_text()
    assert "from botchat import" not in text
    assert "import botchat" not in text
    assert "CW_AGENT_STATS_BOTCHAT_SRC" not in text
    assert "repos/botchat" not in text
