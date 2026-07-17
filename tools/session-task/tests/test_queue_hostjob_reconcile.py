#!/usr/bin/env python3
"""Tests for CLI-side hostjob reconciliation (session-task queue).

Background (Andrew, botchat #2029, earlier #1527): the `hostjob` detached
runner (examples/compose/bin/hostjob) creates a first-class queue row
(scope `hostjob:<label>`, created-by `hostjob`) and, on worker exit, its
reaper flips the row done/abandon. That flip is FAIL-SOFT, so a dropped flip
— or a later `hostjob clean` that removes the runner's `<HOSTJOB_LOG_DIR>/
<label>/status.json` state dir — leaves the row stuck `running` forever.

The q-site minisite already compensates by reconciling at RENDER time against
status.json; the CLI showed raw queue.json, so `queue list` diverged from the
web view (stuck rows only in the CLI). These tests cover the CLI-side fix that
heals the SHARED store so both agree:

  * `queue reconcile-hostjobs` transitions stuck-`running` hostjob rows to
    their authoritative terminal state (done / abandoned), persisting to
    queue.json.
  * `queue list` runs the same sweep automatically before rendering.
  * A hostjob whose status.json still says `running` is NOT touched.
  * A just-launched hostjob (row younger than the starting window) with no
    status.json yet is NOT mis-flipped.
  * A non-hostjob running item is never touched.

Run:
    python3 test_queue_hostjob_reconcile.py
Or: pytest test_queue_hostjob_reconcile.py -v
"""

import json
import os
import subprocess
import sys
import tempfile
from datetime import datetime, timedelta, timezone
from pathlib import Path

SESSION_TASK = Path(__file__).resolve().parent.parent / "session-task"


def _env_for_tmp(tmp):
    env = dict(os.environ)
    env["HOME"] = str(tmp)
    env["PINGME_SESSION_TASK"] = "0"
    # Point the runner-status lookup at a tmp hostjob dir the test controls.
    env["HOSTJOB_LOG_DIR"] = str(Path(tmp, ".cache", "hostjob"))
    Path(tmp, ".config/session").mkdir(parents=True, exist_ok=True)
    return env


def _run(env, *argv, timeout=15):
    cmd = [sys.executable, str(SESSION_TASK)] + list(argv)
    return subprocess.run(
        cmd, capture_output=True, text=True, env=env, timeout=timeout
    )


def _queue_path(env):
    return Path(env["HOME"], ".config", "session", "queue.json")


def _write_queue(env, items):
    data = {"schema_version": 2, "items": items, "locked_scopes": {}}
    _queue_path(env).write_text(json.dumps(data))


def _read_item(env, qid):
    data = json.loads(_queue_path(env).read_text())
    return next(it for it in data["items"] if it["id"] == qid)


def _write_status(env, label, status_dict):
    d = Path(env["HOSTJOB_LOG_DIR"], label)
    d.mkdir(parents=True, exist_ok=True)
    (d / "status.json").write_text(json.dumps(status_dict))


def _running_hostjob_row(qid, label, *, age_seconds):
    started = datetime.now(timezone.utc) - timedelta(seconds=age_seconds)
    iso = started.isoformat()
    return {
        "id": qid,
        "group_id": f"g-{label}",
        "description": f"{label}: some cmd",
        "summary": f"hostjob: {label}",
        "scope": [f"hostjob:{label}"],
        "status": "running",
        "created_by": "hostjob",
        "created_at": iso,
        "registered_at": iso,
        "started_at": iso,
        "priority": 5,
    }


# ---------------------------------------------------------------------------
# reconcile-hostjobs: done rc=0 -> done
# ---------------------------------------------------------------------------
def test_status_done_rc0_reconciles_to_done():
    with tempfile.TemporaryDirectory() as tmp:
        env = _env_for_tmp(tmp)
        _write_queue(env, [_running_hostjob_row("q-a", "okjob", age_seconds=300)])
        _write_status(env, "okjob", {"status": "done", "rc": 0})
        r = _run(env, "queue", "reconcile-hostjobs", "--json")
        assert r.returncode == 0, r.stderr
        out = json.loads(r.stdout)
        assert out == [{"id": "q-a", "status": "done"}], out
        it = _read_item(env, "q-a")
        assert it["status"] == "done"
        assert it["reconciled_hostjob"] is True
        assert it.get("completed_at")
        assert it["group_head"] is False


# ---------------------------------------------------------------------------
# reconcile-hostjobs: nonzero rc -> abandoned
# ---------------------------------------------------------------------------
def test_status_nonzero_rc_reconciles_to_abandoned():
    with tempfile.TemporaryDirectory() as tmp:
        env = _env_for_tmp(tmp)
        _write_queue(env, [_running_hostjob_row("q-b", "failjob", age_seconds=300)])
        _write_status(env, "failjob", {"status": "done", "rc": 2})
        r = _run(env, "queue", "reconcile-hostjobs", "--json")
        assert r.returncode == 0, r.stderr
        it = _read_item(env, "q-b")
        assert it["status"] == "abandoned"
        assert it["reconciled_hostjob"] is True
        assert it.get("abandoned_at")
        assert "exit=2" in it.get("abandon_reason", "")


# ---------------------------------------------------------------------------
# absent state dir (cleaned) + old row -> done
# ---------------------------------------------------------------------------
def test_missing_state_dir_past_window_reconciles_to_done():
    with tempfile.TemporaryDirectory() as tmp:
        env = _env_for_tmp(tmp)
        _write_queue(env, [_running_hostjob_row("q-c", "gonejob", age_seconds=3600)])
        # No status.json written at all.
        r = _run(env, "queue", "reconcile-hostjobs", "--json")
        assert r.returncode == 0, r.stderr
        it = _read_item(env, "q-c")
        assert it["status"] == "done", it


# ---------------------------------------------------------------------------
# absent state dir + fresh row (launch race) -> keep running
# ---------------------------------------------------------------------------
def test_missing_state_dir_within_window_keeps_running():
    with tempfile.TemporaryDirectory() as tmp:
        env = _env_for_tmp(tmp)
        _write_queue(env, [_running_hostjob_row("q-d", "freshjob", age_seconds=5)])
        r = _run(env, "queue", "reconcile-hostjobs", "--json")
        assert r.returncode == 0, r.stderr
        assert json.loads(r.stdout) == []
        it = _read_item(env, "q-d")
        assert it["status"] == "running", it


# ---------------------------------------------------------------------------
# status.json still running -> keep running (trust the reaper)
# ---------------------------------------------------------------------------
def test_status_running_keeps_running():
    with tempfile.TemporaryDirectory() as tmp:
        env = _env_for_tmp(tmp)
        _write_queue(env, [_running_hostjob_row("q-e", "livejob", age_seconds=300)])
        _write_status(env, "livejob", {"status": "running", "rc": None, "pid": 999999})
        r = _run(env, "queue", "reconcile-hostjobs", "--json")
        assert r.returncode == 0, r.stderr
        assert json.loads(r.stdout) == []
        assert _read_item(env, "q-e")["status"] == "running"


# ---------------------------------------------------------------------------
# non-hostjob running item is never touched
# ---------------------------------------------------------------------------
def test_non_hostjob_running_untouched():
    with tempfile.TemporaryDirectory() as tmp:
        env = _env_for_tmp(tmp)
        row = _running_hostjob_row("q-f", "x", age_seconds=3600)
        row["scope"] = ["repo:something"]
        row["created_by"] = "main-loop"
        _write_queue(env, [row])
        r = _run(env, "queue", "reconcile-hostjobs", "--json")
        assert r.returncode == 0, r.stderr
        assert json.loads(r.stdout) == []
        assert _read_item(env, "q-f")["status"] == "running"


# ---------------------------------------------------------------------------
# queue list auto-heals before rendering
# ---------------------------------------------------------------------------
def test_queue_list_auto_reconciles():
    with tempfile.TemporaryDirectory() as tmp:
        env = _env_for_tmp(tmp)
        _write_queue(env, [_running_hostjob_row("q-g", "bkt", age_seconds=300)])
        _write_status(env, "bkt", {"status": "done", "rc": 0})
        # Default `queue list` should heal the row (it drops out of the
        # in-flight default view once terminal).
        r = _run(env, "queue", "list")
        assert r.returncode == 0, r.stderr
        # The store is now healed regardless of what the view prints.
        assert _read_item(env, "q-g")["status"] == "done"


if __name__ == "__main__":
    import pytest
    raise SystemExit(pytest.main([__file__, "-v"]))
