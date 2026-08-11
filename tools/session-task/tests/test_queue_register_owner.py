#!/usr/bin/env python3
"""Tests for owner attribution stamped by ``session-task queue register``.

Owner-attribution gap (#3615/#3617): the spawn-time
``post-tool-agent-arm-hook`` binds ``agent_id -> queue_id`` only for the
ORIGINAL spawn marker. When an agent is RESUMED onto a ROTATED queue id
(e.g. a d6cb -> e8cd rebind) via a mid-flight ``session-task queue
register``, no PostToolUse:Agent hook re-fires, so the new item never gets
an owner and the dashboard shows "owner unknown" though the agent is
actively running it.

Fix: ``register`` stamps the invoking subagent's ``agent_id`` on the item.
The agent_id is resolved either from an explicit ``--agent-id`` flag or,
best-effort, from the live JSONL transcript that references the new queue
id (Claude Code flushes the register tool_use frame to the subagent
transcript BEFORE the Bash tool runs, so the invoking agent's transcript
is the only subagent transcript mentioning the brand-new id).

Must be a SAFE no-op for main-loop registers: no subagent transcript
references the id, so nothing is stamped and no existing binding is
clobbered.

All tests run against a temp HOME so the live queue.json is never touched.

Run::

    uv run --python 3.11 --with pytest \\
        pytest tools/session-task/tests/test_queue_register_owner.py -v
"""

import json
import os
import subprocess
import sys
import tempfile
import time
from pathlib import Path

SESSION_TASK = Path(__file__).resolve().parent.parent / "session-task"


def _env_for_tmp(tmp):
    tmp = Path(tmp)
    env = os.environ.copy()
    env["HOME"] = str(tmp)
    env["PINGME_SESSION_TASK"] = "0"
    env["CLAUDE_EVENT_SESSION_TASK"] = "0"
    env["QUEUE_LOG_ARCHIVE_DIR"] = str(tmp / "queue-logs")
    env["CLAUDE_AGENTS_STATE"] = str(tmp / "active-agents.json")
    env["CLAUDE_AGENTS_JSONL_ROOT"] = str(tmp / "projects")
    return env


def _run(env, *argv, expect_exit=0):
    cmd = [sys.executable, str(SESSION_TASK)] + list(argv)
    r = subprocess.run(cmd, capture_output=True, text=True, env=env, timeout=15)
    if expect_exit is not None and r.returncode != expect_exit:
        raise RuntimeError(
            f"unexpected exit {r.returncode} (want {expect_exit}): argv={argv}\n"
            f"stdout={r.stdout!r}\nstderr={r.stderr!r}"
        )
    return r


def _add(env, desc, scopes, *extra):
    args = ["queue", "add", desc, "--json"]
    for s in scopes:
        args.extend(["--scope", s])
    args.extend(extra)
    r = _run(env, *args)
    return json.loads(r.stdout)


def _show(env, qid):
    r = _run(env, "queue", "show", qid)
    return json.loads(r.stdout)


def _write_transcript(env, session_uuid, agent_id, text, *, mtime=None):
    """Write a synthetic subagent transcript whose body contains ``text``.

    Returns the path. Optionally back-dates the mtime so freshness-window
    behaviour can be exercised.
    """
    sub = Path(env["CLAUDE_AGENTS_JSONL_ROOT"]) / session_uuid / "subagents"
    sub.mkdir(parents=True, exist_ok=True)
    path = sub / f"agent-{agent_id}.jsonl"
    frame = {
        "type": "assistant",
        "message": {
            "role": "assistant",
            "content": [
                {
                    "type": "tool_use",
                    "name": "Bash",
                    "input": {"command": text},
                }
            ],
        },
    }
    path.write_text(json.dumps(frame) + "\n")
    if mtime is not None:
        os.utime(path, (mtime, mtime))
    return path


# ---------------------------------------------------------------------------
# Explicit --agent-id
# ---------------------------------------------------------------------------


def test_explicit_agent_id_is_stamped():
    with tempfile.TemporaryDirectory() as tmp:
        env = _env_for_tmp(tmp)
        item = _add(env, "do work", ["repo:foo"])
        qid = item["id"]
        _run(env, "queue", "register", qid, "--agent-id", "aexplicit1234abcd")
        shown = _show(env, qid)
        assert shown["status"] == "running"
        assert shown["agent_id"] == "aexplicit1234abcd"
        assert shown["agent_id_source"] == "register"
        assert shown.get("agent_id_stamped_at")


def test_explicit_agent_id_overrides_transcript_scan():
    """An explicit --agent-id wins over any transcript-resolved id."""
    with tempfile.TemporaryDirectory() as tmp:
        env = _env_for_tmp(tmp)
        item = _add(env, "do work", ["repo:foo"])
        qid = item["id"]
        # A transcript references the id (would otherwise resolve to atrans).
        _write_transcript(
            env, "sess-a", "atranscript99999",
            f"session-task queue register {qid}", mtime=time.time(),
        )
        _run(env, "queue", "register", qid, "--agent-id", "aflagwins00000000")
        assert _show(env, qid)["agent_id"] == "aflagwins00000000"


# ---------------------------------------------------------------------------
# Transcript-based resolution (the rebind case)
# ---------------------------------------------------------------------------


def test_transcript_resolution_stamps_rebound_owner():
    """Resumed agent: its live transcript references the NEW (rotated) id,
    so register attributes the item to that agent."""
    with tempfile.TemporaryDirectory() as tmp:
        env = _env_for_tmp(tmp)
        item = _add(env, "resumed work", ["repo:foo"])
        qid = item["id"]
        _write_transcript(
            env, "sess-resumed", "aresumed12345678",
            # The register tool_use frame Claude Code flushes before exec.
            f"session-task queue register {qid} --json",
            mtime=time.time(),
        )
        _run(env, "queue", "register", qid)
        shown = _show(env, qid)
        assert shown["agent_id"] == "aresumed12345678"
        assert shown["agent_id_source"] == "register"


def test_transcript_resolution_prefers_freshest_transcript():
    """When several transcripts reference the id, the freshest (the agent
    that just ran register) wins."""
    with tempfile.TemporaryDirectory() as tmp:
        env = _env_for_tmp(tmp)
        item = _add(env, "work", ["repo:foo"])
        qid = item["id"]
        now = time.time()
        # An older transcript also mentions the id (e.g. a peer that read a
        # status line); the freshly-written one is the true invoker.
        _write_transcript(
            env, "sess-old", "aold000000000000",
            f"session-task queue spawn-check {qid}", mtime=now - 300,
        )
        _write_transcript(
            env, "sess-new", "anew111111111111",
            f"session-task queue register {qid}", mtime=now,
        )
        _run(env, "queue", "register", qid)
        assert _show(env, qid)["agent_id"] == "anew111111111111"


def test_stale_transcript_beyond_window_is_ignored():
    """A transcript older than the freshness window is not scanned, so a
    stale reference does not mis-attribute the owner."""
    with tempfile.TemporaryDirectory() as tmp:
        env = _env_for_tmp(tmp)
        item = _add(env, "work", ["repo:foo"])
        qid = item["id"]
        _write_transcript(
            env, "sess-stale", "astale0000000000",
            f"session-task queue register {qid}",
            mtime=time.time() - 24 * 3600,  # far beyond the 600s window
        )
        _run(env, "queue", "register", qid)
        shown = _show(env, qid)
        assert "agent_id" not in shown


# ---------------------------------------------------------------------------
# Main-loop safety (no false stamping / no clobber)
# ---------------------------------------------------------------------------


def test_main_loop_register_leaves_owner_unset():
    """No subagent transcript references the id (a main-loop register /
    first spawn): register must not invent an owner."""
    with tempfile.TemporaryDirectory() as tmp:
        env = _env_for_tmp(tmp)
        item = _add(env, "main loop work", ["repo:foo"])
        qid = item["id"]
        # Some unrelated subagent transcript exists but does NOT mention qid.
        _write_transcript(
            env, "sess-other", "aother0000000000",
            "session-task queue register q-2020-01-01-zzzz", mtime=time.time(),
        )
        _run(env, "queue", "register", qid)
        shown = _show(env, qid)
        assert shown["status"] == "running"
        assert "agent_id" not in shown


def test_missing_transcript_root_is_safe():
    """No transcript tree at all: register still succeeds, owner unset."""
    with tempfile.TemporaryDirectory() as tmp:
        env = _env_for_tmp(tmp)
        # Point the root at a nonexistent dir.
        env["CLAUDE_AGENTS_JSONL_ROOT"] = str(Path(tmp) / "does-not-exist")
        item = _add(env, "work", ["repo:foo"])
        qid = item["id"]
        _run(env, "queue", "register", qid)
        shown = _show(env, qid)
        assert shown["status"] == "running"
        assert "agent_id" not in shown


if __name__ == "__main__":
    import pytest

    raise SystemExit(pytest.main([__file__, "-v"]))
