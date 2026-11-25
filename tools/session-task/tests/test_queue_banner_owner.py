#!/usr/bin/env python3
"""`queue banner` names running items that have no attributable owner.

Reproduces the post-wedge case: an item `register`ed, the Agent never fired,
then the session cleared. The resume banner must call it out, not just count it.

Run:
    uv run --python 3.11 --with pytest \\
        pytest tools/session-task/tests/test_queue_banner_owner.py -v
"""

import sys
import tempfile
from pathlib import Path

import pytest

sys.path.insert(0, str(Path(__file__).resolve().parent))
from test_queue_waiting import _env, _run, _running, _set  # noqa: E402


@pytest.fixture()
def env():
    with tempfile.TemporaryDirectory() as tmp:
        e = _env(tmp)
        e["CLAUDE_AGENTS_STATE"] = str(Path(tmp) / "no-agents.json")
        e["CLAUDE_AGENTS_STATE_FALLBACK_BIN"] = ""
        yield e


def test_ownerless_running_item_is_named(env):
    q = _running(env)
    out = _run(env, "queue", "banner").stdout
    assert "In progress (running): 1" in out
    assert "NO OWNER" in out
    assert q in out


def test_owned_running_item_is_not_flagged(env):
    q = _running(env)
    _set(env, q, agent_id="a1234")
    out = _run(env, "queue", "banner").stdout
    assert "In progress (running): 1" in out
    assert "NO OWNER" not in out


def test_workload_scope_item_is_exempt(env):
    q = _running(env)
    _set(env, q, scope=["workload:foo"])
    out = _run(env, "queue", "banner").stdout
    assert "NO OWNER" not in out
