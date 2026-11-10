#!/usr/bin/env python3
"""Tests for the "one queue item per agent" rule on owner stamping.

Bundling two queue items onto one agent made the dashboard tail the same
agent twice. ``queue assign`` and ``queue register --agent-id`` therefore
REFUSE to stamp an agent id that already owns another non-terminal item.

Run::

    uv run --python 3.11 --with pytest \\
        pytest tools/session-task/tests/test_queue_one_item_per_agent.py -v
"""

import tempfile

from test_queue_assign import _add, _env_for_tmp, _run, _show

AID = "abundled00000001"
MSG = "one queue item per agent"


def _running(env, scope, desc="work"):
    qid = _add(env, desc, [scope])["id"]
    _run(env, "queue", "register", qid)
    return qid


def test_assign_second_item_refused():
    with tempfile.TemporaryDirectory() as tmp:
        env = _env_for_tmp(tmp)
        a = _running(env, "repo:foo")
        b = _running(env, "repo:bar")
        _run(env, "queue", "assign", a, "--agent", AID)
        r = _run(env, "queue", "assign", b, "--agent", AID, expect_exit=1)
        assert a in r.stderr and MSG in r.stderr
        assert "agent_id" not in _show(env, b)


def test_register_agent_id_second_item_refused_and_not_started():
    with tempfile.TemporaryDirectory() as tmp:
        env = _env_for_tmp(tmp)
        a = _running(env, "repo:foo")
        _run(env, "queue", "assign", a, "--agent", AID)
        b = _add(env, "other", ["repo:bar"])["id"]
        r = _run(env, "queue", "register", b, "--agent-id", AID, expect_exit=1)
        assert a in r.stderr and MSG in r.stderr
        assert _show(env, b)["status"] == "pending"


def test_idempotent_restamp_same_item_same_agent():
    with tempfile.TemporaryDirectory() as tmp:
        env = _env_for_tmp(tmp)
        a = _running(env, "repo:foo")
        _run(env, "queue", "assign", a, "--agent", AID)
        _run(env, "queue", "assign", a, "--agent", AID)
        assert _show(env, a)["agent_id"] == AID


def test_move_allowed_when_old_item_terminal():
    with tempfile.TemporaryDirectory() as tmp:
        env = _env_for_tmp(tmp)
        a = _running(env, "repo:foo")
        _run(env, "queue", "assign", a, "--agent", AID)
        _run(env, "queue", "done", a)
        b = _add(env, "next", ["repo:bar"])["id"]
        _run(env, "queue", "register", b, "--agent-id", AID)
        assert _show(env, b)["agent_id"] == AID
        c = _running(env, "repo:baz")
        _run(env, "queue", "assign", c, "--agent", AID, expect_exit=1)
