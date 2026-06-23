#!/usr/bin/env python3
"""Tests for `queue reprioritize` and priority-first spawn ordering.

Covers:

  * Spawn-readiness ordering within a group honors PRIORITY, not just
    insertion order: a higher-priority item added LATER overtakes an
    earlier lower-priority same-scope peer and becomes ready first --
    without abandon+re-add.
  * `queue reprioritize <id> --priority N` sets a pending item's priority
    in place, recomputes group heads, and surfaces the new ready_now.
  * `queue reprioritize` is REFUSED on non-pending items (exit 1).
  * `queue reprioritize` on a missing id exits 1.
  * `queue bump` (alias of `promote`) raises a pending item above its
    group head so it becomes ready.

Run:
    uv run --python 3.11 --with pytest \\
        pytest tests/test_queue_reprioritize.py -v

Or directly:
    python3 tests/test_queue_reprioritize.py
"""

import json
import os
import subprocess
import sys
import tempfile
from pathlib import Path

SESSION_TASK = Path(__file__).resolve().parent.parent / "session-task"


def _env_for_tmp(tmp):
    env = dict(os.environ)
    env["HOME"] = str(tmp)
    env["PINGME_SESSION_TASK"] = "0"
    Path(tmp, ".config/session").mkdir(parents=True, exist_ok=True)
    return env


def _run(env, *argv, timeout=15):
    cmd = [sys.executable, str(SESSION_TASK)] + list(argv)
    return subprocess.run(cmd, capture_output=True, text=True, env=env,
                          timeout=timeout)


def _add(env, desc, scopes, *extra):
    cmd = ["queue", "add", desc, "--json"]
    for s in scopes:
        cmd.extend(["--scope", s])
    cmd.extend(extra)
    return json.loads(_run(env, *cmd).stdout)


def _spawn_check(env, qid):
    r = _run(env, "queue", "spawn-check", qid, "--json")
    return json.loads(r.stdout), r.returncode


# ---------------------------------------------------------------------------
# 1. Priority-first spawn ordering: later higher-priority item overtakes.
# ---------------------------------------------------------------------------


def test_higher_priority_added_later_becomes_ready_first():
    with tempfile.TemporaryDirectory() as tmp:
        env = _env_for_tmp(tmp)
        a = _add(env, "A low prio", ["repo:foo"])  # default priority 5
        b = _add(env, "B high prio", ["repo:foo"], "--priority", "9")
        # A added first but lower priority; B added later at higher priority.
        # B's add-time report sees itself as the head (nothing outranks it).
        assert b["ready_now"] is True, "B (higher priority) should be ready first"
        assert b["serialized_after"] == [], "nothing outranks B"

        # The authoritative, recomputed gate is spawn-check (what the
        # obligation hook consults). B's arrival demotes the earlier
        # lower-priority A behind it -- priority-first, not insertion order.
        sca, rca = _spawn_check(env, a["id"])
        scb, rcb = _spawn_check(env, b["id"])
        assert scb["ok"] is True and rcb == 0, "B must be clear to spawn"
        assert sca["ok"] is False and rca == 2, "A must be blocked behind B"


# ---------------------------------------------------------------------------
# 2. Equal priority falls back to FIFO (insertion order).
# ---------------------------------------------------------------------------


def test_equal_priority_uses_fifo_tiebreak():
    with tempfile.TemporaryDirectory() as tmp:
        env = _env_for_tmp(tmp)
        a = _add(env, "A first", ["repo:foo"])
        b = _add(env, "B second", ["repo:foo"])
        assert a["ready_now"] is True
        assert b["ready_now"] is False
        assert a["id"] in b["serialized_after"]


# ---------------------------------------------------------------------------
# 3. reprioritize bumps a blocked peer to ready without abandon+re-add.
# ---------------------------------------------------------------------------


def test_reprioritize_makes_blocked_item_ready():
    with tempfile.TemporaryDirectory() as tmp:
        env = _env_for_tmp(tmp)
        a = _add(env, "A", ["repo:foo"])
        b = _add(env, "B", ["repo:foo"])
        assert a["ready_now"] is True and b["ready_now"] is False

        r = _run(env, "queue", "reprioritize", b["id"], "--priority", "9",
                 "--json")
        assert r.returncode == 0
        out = json.loads(r.stdout)
        assert out["old_priority"] == 5
        assert out["priority"] == 9
        assert out["ready_now"] is True

        scb, rcb = _spawn_check(env, b["id"])
        sca, rca = _spawn_check(env, a["id"])
        assert scb["ok"] is True
        assert sca["ok"] is False


# ---------------------------------------------------------------------------
# 4. reprioritize refuses non-pending items.
# ---------------------------------------------------------------------------


def test_reprioritize_refuses_running():
    with tempfile.TemporaryDirectory() as tmp:
        env = _env_for_tmp(tmp)
        a = _add(env, "A", ["repo:foo"])
        _run(env, "queue", "register", a["id"])
        r = _run(env, "queue", "reprioritize", a["id"], "--priority", "9")
        assert r.returncode == 1
        assert "can only reprioritize pending items" in r.stderr


def test_reprioritize_missing_id():
    with tempfile.TemporaryDirectory() as tmp:
        env = _env_for_tmp(tmp)
        r = _run(env, "queue", "reprioritize", "q-nope", "--priority", "7")
        assert r.returncode == 1
        assert "not found" in r.stderr


# ---------------------------------------------------------------------------
# 5. bump alias (promote) raises a pending item above its group head.
# ---------------------------------------------------------------------------


def test_bump_alias_promotes_to_head():
    with tempfile.TemporaryDirectory() as tmp:
        env = _env_for_tmp(tmp)
        a = _add(env, "A", ["repo:foo"])
        b = _add(env, "B", ["repo:foo"])
        assert b["ready_now"] is False
        r = _run(env, "queue", "bump", b["id"])
        assert r.returncode == 0
        scb, _ = _spawn_check(env, b["id"])
        assert scb["ok"] is True


if __name__ == "__main__":
    failures = 0
    for name, fn in sorted(globals().items()):
        if name.startswith("test_") and callable(fn):
            try:
                fn()
                print(f"PASS {name}")
            except Exception as e:  # noqa: BLE001
                failures += 1
                print(f"FAIL {name}: {e}")
    sys.exit(1 if failures else 0)
