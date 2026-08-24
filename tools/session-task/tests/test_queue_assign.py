#!/usr/bin/env python3
"""Tests for ``session-task queue assign`` -- owner retrofit on in-flight items.

Why a standalone verb exists (rather than reusing ``register``):

  * ``register --agent-id --if-absent`` SHORT-CIRCUITS at the
    ``already running`` branch and stamps nothing, so the flag silently
    does nothing on exactly the items that need the retrofit.
  * ``register`` WITHOUT ``--if-absent`` refuses a running item by design
    -- a register against a running item is the double-spawn signal, and
    weakening it to permit an owner edit would blunt that guard.

``assign`` therefore touches ONLY the three owner fields
(``agent_id`` / ``agent_id_source`` / ``agent_id_stamped_at``), on the
in-flight statuses where an agent genuinely exists to be named
(running / wedged / blocked). Terminal items (done / abandoned /
quarantined) and not-yet-spawned pending items are refused.

All tests run against a temp HOME so the live queue.json is never touched.

Run::

    uv run --python 3.11 --with pytest \\
        pytest tools/session-task/tests/test_queue_assign.py -v
"""

import json
import os
import subprocess
import sys
import tempfile
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
    return json.loads(_run(env, *args).stdout)


def _show(env, qid):
    return json.loads(_run(env, "queue", "show", qid).stdout)


def _running_item(env, scope="repo:foo", desc="do work"):
    """Add + register an item, returning its id (owner deliberately unset)."""
    qid = _add(env, desc, [scope])["id"]
    _run(env, "queue", "register", qid)
    assert "agent_id" not in _show(env, qid)
    return qid


# ---------------------------------------------------------------------------
# Happy path
# ---------------------------------------------------------------------------


def test_assign_stamps_owner_on_running_item():
    with tempfile.TemporaryDirectory() as tmp:
        env = _env_for_tmp(tmp)
        qid = _running_item(env)
        _run(env, "queue", "assign", qid, "--agent", "aresumed12345678")
        shown = _show(env, qid)
        assert shown["agent_id"] == "aresumed12345678"
        assert shown["agent_id_source"] == "assign"
        assert shown.get("agent_id_stamped_at")
        # Status untouched -- assign is a metadata edit, not a transition.
        assert shown["status"] == "running"


def test_assign_accepts_agent_id_alias():
    """`--agent-id` is accepted for symmetry with `queue register`."""
    with tempfile.TemporaryDirectory() as tmp:
        env = _env_for_tmp(tmp)
        qid = _running_item(env)
        _run(env, "queue", "assign", qid, "--agent-id", "aalias0000000000")
        assert _show(env, qid)["agent_id"] == "aalias0000000000"


def test_assign_json_emits_the_item():
    with tempfile.TemporaryDirectory() as tmp:
        env = _env_for_tmp(tmp)
        qid = _running_item(env)
        r = _run(env, "queue", "assign", qid, "--agent", "ajson0000000000a",
                 "--json")
        payload = json.loads(r.stdout)
        assert payload["id"] == qid
        assert payload["agent_id"] == "ajson0000000000a"
        assert payload["agent_id_source"] == "assign"


def test_assign_rebinds_an_existing_owner():
    """A wrong / stale owner can be corrected; source flips to `assign`."""
    with tempfile.TemporaryDirectory() as tmp:
        env = _env_for_tmp(tmp)
        qid = _add(env, "work", ["repo:foo"])["id"]
        _run(env, "queue", "register", qid, "--agent-id", "aoriginal0000000")
        assert _show(env, qid)["agent_id_source"] == "register"
        _run(env, "queue", "assign", qid, "--agent", "arebound000000000")
        shown = _show(env, qid)
        assert shown["agent_id"] == "arebound000000000"
        assert shown["agent_id_source"] == "assign"


def test_assign_does_not_bump_heartbeat():
    """An owner stamp is not evidence of liveness -- last_heartbeat_at must
    stay where it was, so a stalled item is not laundered into a fresh one."""
    with tempfile.TemporaryDirectory() as tmp:
        env = _env_for_tmp(tmp)
        qid = _running_item(env)
        before = _show(env, qid)["last_heartbeat_at"]
        _run(env, "queue", "assign", qid, "--agent", "anobeat000000000")
        assert _show(env, qid)["last_heartbeat_at"] == before


def test_assign_works_on_wedged_and_blocked_items():
    """Wedged / blocked items are in flight and still have a real owner."""
    with tempfile.TemporaryDirectory() as tmp:
        env = _env_for_tmp(tmp)
        wedged = _running_item(env, scope="repo:wedge")
        _run(env, "queue", "wedge", wedged, "--reason", "stuck on a lock")
        _run(env, "queue", "assign", wedged, "--agent", "awedged000000000")
        assert _show(env, wedged)["agent_id"] == "awedged000000000"

        blocked = _running_item(env, scope="repo:block")
        _run(env, "queue", "block", blocked, "--reason", "waiting on Andrew")
        _run(env, "queue", "assign", blocked, "--agent", "ablocked00000000")
        assert _show(env, blocked)["agent_id"] == "ablocked00000000"


# ---------------------------------------------------------------------------
# Refusals
# ---------------------------------------------------------------------------


def test_assign_refuses_done_item():
    with tempfile.TemporaryDirectory() as tmp:
        env = _env_for_tmp(tmp)
        qid = _running_item(env)
        _run(env, "queue", "done", qid)
        r = _run(env, "queue", "assign", qid, "--agent", "adone00000000000",
                 expect_exit=1)
        assert "assign refused" in r.stderr
        assert "done" in r.stderr
        assert "agent_id" not in _show(env, qid)


def test_assign_refuses_abandoned_item():
    with tempfile.TemporaryDirectory() as tmp:
        env = _env_for_tmp(tmp)
        qid = _running_item(env)
        _run(env, "queue", "abandon", qid, "--reason", "no longer needed")
        r = _run(env, "queue", "assign", qid, "--agent", "aaband0000000000",
                 expect_exit=1)
        assert "assign refused" in r.stderr
        assert "terminal" in r.stderr
        assert "agent_id" not in _show(env, qid)


def test_assign_refuses_pending_item():
    """Nothing has been spawned -- stamping an owner would invent one."""
    with tempfile.TemporaryDirectory() as tmp:
        env = _env_for_tmp(tmp)
        qid = _add(env, "not started", ["repo:foo"])["id"]
        r = _run(env, "queue", "assign", qid, "--agent", "apending00000000",
                 expect_exit=1)
        assert "assign refused" in r.stderr
        assert "register" in r.stderr
        assert "agent_id" not in _show(env, qid)


def test_assign_refuses_unknown_id():
    with tempfile.TemporaryDirectory() as tmp:
        env = _env_for_tmp(tmp)
        _add(env, "some item", ["repo:foo"])
        r = _run(env, "queue", "assign", "q-9999-99-99-zzzz",
                 "--agent", "amissing00000000", expect_exit=1)
        assert "not found" in r.stderr


def test_assign_refuses_empty_agent_id():
    with tempfile.TemporaryDirectory() as tmp:
        env = _env_for_tmp(tmp)
        qid = _running_item(env)
        r = _run(env, "queue", "assign", qid, "--agent", "   ", expect_exit=1)
        assert "non-empty" in r.stderr
        assert "agent_id" not in _show(env, qid)


def test_assign_requires_agent_flag():
    """`--agent` is required: an assign without an owner is meaningless."""
    with tempfile.TemporaryDirectory() as tmp:
        env = _env_for_tmp(tmp)
        qid = _running_item(env)
        _run(env, "queue", "assign", qid, expect_exit=2)  # argparse usage error


# ---------------------------------------------------------------------------
# The gap this verb closes
# ---------------------------------------------------------------------------


def test_register_if_absent_still_stamps_nothing_on_a_running_item():
    """The motivating defect, pinned: `register --agent-id --if-absent`
    short-circuits at `already running` and never stamps the owner. If this
    ever starts passing, `assign` is no longer the only retrofit path --
    but until then, this is why it exists."""
    with tempfile.TemporaryDirectory() as tmp:
        env = _env_for_tmp(tmp)
        qid = _running_item(env)
        _run(env, "queue", "register", qid, "--if-absent",
             "--agent-id", "awouldbe00000000")
        assert "agent_id" not in _show(env, qid)
        # assign is the path that actually works.
        _run(env, "queue", "assign", qid, "--agent", "awouldbe00000000")
        assert _show(env, qid)["agent_id"] == "awouldbe00000000"


if __name__ == "__main__":
    import pytest

    raise SystemExit(pytest.main([__file__, "-v"]))
