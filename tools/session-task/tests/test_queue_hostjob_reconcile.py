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

The same sweep also GCs the EXIT-QUARANTINE leak: a hostjob that exits
nonzero makes the reaper run `queue abandon`, which (because abandon on a
scope-owning row is deliberately non-terminal) lands the row in
`quarantined`, pinning its `hostjob:<label>` scope forever. reconcile now
flips such rows to terminal `abandoned` (scope freed) when status.json is
positive proof the worker is gone — while leaving a still-`running` hostjob,
and any non-hostjob quarantine, untouched.

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


def _quarantined_hostjob_row(qid, label, *, age_seconds, reason="hostjob exit 127"):
    """A hostjob row that the reaper `queue abandon`'d into quarantine on a
    nonzero worker exit — the exit-quarantine leak state. Still owns its
    `hostjob:<label>` scope until reconcile (or a human `queue release`)
    frees it."""
    started = datetime.now(timezone.utc) - timedelta(seconds=age_seconds)
    iso = started.isoformat()
    return {
        "id": qid,
        "group_id": f"g-{label}",
        "description": f"{label}: some cmd",
        "summary": f"hostjob: {label}",
        "scope": [f"hostjob:{label}"],
        "status": "quarantined",
        "created_by": "hostjob",
        "created_at": iso,
        "registered_at": iso,
        "started_at": iso,
        "quarantined_at": iso,
        "quarantined_from": "running",
        "quarantine_reason": reason,
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
        # No status.json for this label, but the state dir is visible and
        # holds another label (proof the surface is readable).
        _write_status(env, "otherjob", {"status": "running", "rc": None})
        r = _run(env, "queue", "reconcile-hostjobs", "--json")
        assert r.returncode == 0, r.stderr
        it = _read_item(env, "q-c")
        assert it["status"] == "done", it


# ---------------------------------------------------------------------------
# state dir INVISIBLE (absent / empty, e.g. CLI in a different filesystem view
# than the runner) + old row -> keep running; a missing status.json is no
# evidence the job finished.
# ---------------------------------------------------------------------------
def test_invisible_state_dir_past_window_keeps_running():
    with tempfile.TemporaryDirectory() as tmp:
        env = _env_for_tmp(tmp)
        assert not Path(env["HOSTJOB_LOG_DIR"]).exists()
        _write_queue(env, [_running_hostjob_row("q-iv", "livejob", age_seconds=3600)])
        r = _run(env, "queue", "reconcile-hostjobs", "--json")
        assert r.returncode == 0, r.stderr
        assert json.loads(r.stdout) == []
        assert _read_item(env, "q-iv")["status"] == "running"


def test_empty_state_dir_past_window_keeps_running():
    with tempfile.TemporaryDirectory() as tmp:
        env = _env_for_tmp(tmp)
        Path(env["HOSTJOB_LOG_DIR"]).mkdir(parents=True)
        _write_queue(env, [_running_hostjob_row("q-ev", "livejob", age_seconds=3600)])
        r = _run(env, "queue", "reconcile-hostjobs", "--json")
        assert r.returncode == 0, r.stderr
        assert json.loads(r.stdout) == []
        assert _read_item(env, "q-ev")["status"] == "running"


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


# ===========================================================================
# Exit-quarantine GC: a nonzero-exit hostjob quarantine whose worker is
# CONFIRMED dead is reaped to terminal `abandoned` (scope freed).
# ===========================================================================
def test_quarantine_terminal_status_reaped_to_abandoned():
    with tempfile.TemporaryDirectory() as tmp:
        env = _env_for_tmp(tmp)
        _write_queue(
            env, [_quarantined_hostjob_row("q-qa", "crashjob", age_seconds=600)]
        )
        # Reaper recorded the worker's terminal exit (rc 127).
        _write_status(env, "crashjob", {"status": "done", "rc": 127})
        r = _run(env, "queue", "reconcile-hostjobs", "--json")
        assert r.returncode == 0, r.stderr
        out = json.loads(r.stdout)
        assert out == [{"id": "q-qa", "status": "abandoned"}], out
        it = _read_item(env, "q-qa")
        assert it["status"] == "abandoned"
        assert it["reconciled_hostjob"] is True
        assert it.get("abandoned_at")
        assert it.get("quarantine_released_at")
        assert it.get("quarantine_released_by") == "reconcile-hostjobs"
        # Original quarantine reason is carried forward into abandon_reason.
        assert "hostjob exit 127" in it.get("abandon_reason", "")
        assert it["group_head"] is False


# A "crashed" status.json (reaper itself threw) is also confirmed-dead.
def test_quarantine_crashed_status_reaped():
    with tempfile.TemporaryDirectory() as tmp:
        env = _env_for_tmp(tmp)
        _write_queue(
            env, [_quarantined_hostjob_row("q-qc", "reaperdied", age_seconds=600)]
        )
        _write_status(env, "reaperdied", {"status": "crashed", "rc": 127})
        r = _run(env, "queue", "reconcile-hostjobs", "--json")
        assert r.returncode == 0, r.stderr
        assert _read_item(env, "q-qc")["status"] == "abandoned"


# An absent state dir past the starting window (hostjob clean ran) is
# positive proof the worker reached terminal — reap the quarantine.
def test_quarantine_missing_state_dir_past_window_reaped():
    with tempfile.TemporaryDirectory() as tmp:
        env = _env_for_tmp(tmp)
        _write_queue(
            env, [_quarantined_hostjob_row("q-qm", "cleanedjob", age_seconds=3600)]
        )
        # No status.json for this label; state dir visible via another label.
        _write_status(env, "otherjob", {"status": "running", "rc": None})
        r = _run(env, "queue", "reconcile-hostjobs", "--json")
        assert r.returncode == 0, r.stderr
        assert _read_item(env, "q-qm")["status"] == "abandoned"


# An INVISIBLE state dir proves nothing: leave the quarantine alone.
def test_quarantine_invisible_state_dir_left_alone():
    with tempfile.TemporaryDirectory() as tmp:
        env = _env_for_tmp(tmp)
        _write_queue(
            env, [_quarantined_hostjob_row("q-qi", "livejob", age_seconds=3600)]
        )
        r = _run(env, "queue", "reconcile-hostjobs", "--json")
        assert r.returncode == 0, r.stderr
        assert json.loads(r.stdout) == []
        assert _read_item(env, "q-qi")["status"] == "quarantined"


# SAFETY: a quarantine whose runner still reports `running` is a LIVE
# hostjob — never reap it (the worker is alive and holding the scope).
def test_quarantine_status_running_left_alone():
    with tempfile.TemporaryDirectory() as tmp:
        env = _env_for_tmp(tmp)
        _write_queue(
            env, [_quarantined_hostjob_row("q-qr", "stilllive", age_seconds=600)]
        )
        _write_status(
            env, "stilllive", {"status": "running", "rc": None, "pid": 999999}
        )
        r = _run(env, "queue", "reconcile-hostjobs", "--json")
        assert r.returncode == 0, r.stderr
        assert json.loads(r.stdout) == []
        assert _read_item(env, "q-qr")["status"] == "quarantined"


# A NON-hostjob quarantine (repo:/service: scope) needs human judgment and
# must never be auto-reaped, even with an old row.
def test_non_hostjob_quarantine_untouched():
    with tempfile.TemporaryDirectory() as tmp:
        env = _env_for_tmp(tmp)
        row = _quarantined_hostjob_row("q-qn", "x", age_seconds=3600)
        row["scope"] = ["repo:claude-watch"]
        row["created_by"] = "main-loop"
        _write_queue(env, [row])
        r = _run(env, "queue", "reconcile-hostjobs", "--json")
        assert r.returncode == 0, r.stderr
        assert json.loads(r.stdout) == []
        assert _read_item(env, "q-qn")["status"] == "quarantined"


# queue list auto-heals a leaked exit-quarantine before rendering.
def test_queue_list_auto_reaps_quarantine():
    with tempfile.TemporaryDirectory() as tmp:
        env = _env_for_tmp(tmp)
        _write_queue(
            env, [_quarantined_hostjob_row("q-ql", "leaked", age_seconds=600)]
        )
        _write_status(env, "leaked", {"status": "done", "rc": 101})
        r = _run(env, "queue", "list", "--all")
        assert r.returncode == 0, r.stderr
        assert _read_item(env, "q-ql")["status"] == "abandoned"


if __name__ == "__main__":
    import pytest
    raise SystemExit(pytest.main([__file__, "-v"]))


# ---------------------------------------------------------------------------
# revive: a reconcile-closed row whose worker is verifiably alive is reopened
# ---------------------------------------------------------------------------
def _closed_by_reconcile(row, status="done"):
    row = dict(row)
    row["status"] = status
    row["reconciled_hostjob"] = True
    row["completed_at"] = row["created_at"]
    return row


def test_reconciled_done_row_with_live_worker_is_revived():
    with tempfile.TemporaryDirectory() as tmp:
        env = _env_for_tmp(tmp)
        row = _closed_by_reconcile(_running_hostjob_row("q-r", "livejob", age_seconds=900))
        _write_queue(env, [row])
        _write_status(env, "livejob", {"status": "running", "rc": None, "queue_id": "q-r"})
        r = _run(env, "queue", "reconcile-hostjobs", "--json")
        assert r.returncode == 0, r.stderr
        assert json.loads(r.stdout) == [{"id": "q-r", "status": "running"}]
        it = _read_item(env, "q-r")
        assert it["status"] == "running"
        assert it["revived_from_status"] == "done"
        assert "completed_at" not in it


def test_revive_requires_runner_to_name_this_queue_id():
    with tempfile.TemporaryDirectory() as tmp:
        env = _env_for_tmp(tmp)
        row = _closed_by_reconcile(_running_hostjob_row("q-old", "reused", age_seconds=900))
        _write_queue(env, [row])
        _write_status(env, "reused", {"status": "running", "queue_id": "q-new"})
        r = _run(env, "queue", "reconcile-hostjobs", "--json")
        assert r.returncode == 0, r.stderr
        assert _read_item(env, "q-old")["status"] == "done"


def test_non_reconciled_done_row_is_not_revived():
    with tempfile.TemporaryDirectory() as tmp:
        env = _env_for_tmp(tmp)
        row = _closed_by_reconcile(_running_hostjob_row("q-n", "normal", age_seconds=900))
        row.pop("reconciled_hostjob")
        _write_queue(env, [row])
        _write_status(env, "normal", {"status": "running", "queue_id": "q-n"})
        _run(env, "queue", "reconcile-hostjobs", "--json")
        assert _read_item(env, "q-n")["status"] == "done"


# ---------------------------------------------------------------------------
# REMOTE-origin rows (hostjob_host set): runner state lives on another host,
# so a missing local state dir is NOT proof of death.
# ---------------------------------------------------------------------------
def _remote_row(qid, label, *, age_seconds, **extra):
    row = _running_hostjob_row(qid, label, age_seconds=age_seconds)
    row["hostjob_host"] = "ssh://remote-mac"
    row.update(extra)
    return row


def _push_log(env, label, text="x\n"):
    d = Path(env["HOME"], ".config", "session", "hostjob-logs", label)
    d.mkdir(parents=True, exist_ok=True)
    (d / "log").write_text(text)


def test_remote_row_no_local_state_dir_stays_open_even_when_visible():
    with tempfile.TemporaryDirectory() as tmp:
        env = _env_for_tmp(tmp)
        _write_queue(env, [_remote_row("q-rm", "macjob", age_seconds=3600)])
        _push_log(env, "macjob")
        # Local state dir is visible and non-empty (other local jobs) but has
        # no entry for this label -- the exact case that used to false-close.
        _write_status(env, "otherjob", {"status": "running", "rc": None})
        r = _run(env, "queue", "reconcile-hostjobs", "--json")
        assert r.returncode == 0, r.stderr
        assert json.loads(r.stdout) == []
        assert _read_item(env, "q-rm")["status"] == "running"


def test_remote_row_ignores_local_terminal_status_json():
    with tempfile.TemporaryDirectory() as tmp:
        env = _env_for_tmp(tmp)
        _write_queue(env, [_remote_row("q-rt", "macjob2", age_seconds=3600)])
        _push_log(env, "macjob2")
        _write_status(env, "macjob2", {"status": "done", "rc": 0})
        _run(env, "queue", "reconcile-hostjobs", "--json")
        assert _read_item(env, "q-rt")["status"] == "running"


def test_remote_row_recent_heartbeat_keeps_open_despite_old_registration():
    with tempfile.TemporaryDirectory() as tmp:
        env = _env_for_tmp(tmp)
        beat = (datetime.now(timezone.utc) - timedelta(minutes=5)).isoformat()
        _write_queue(env, [_remote_row(
            "q-rh", "macjob3", age_seconds=3 * 86400, last_heartbeat_at=beat)])
        _run(env, "queue", "reconcile-hostjobs", "--json")
        assert _read_item(env, "q-rh")["status"] == "running"


def test_remote_row_stale_over_24h_is_gcd_to_abandoned():
    with tempfile.TemporaryDirectory() as tmp:
        env = _env_for_tmp(tmp)
        _probe(env, tmp, 1)
        _write_queue(env, [_remote_row("q-rs", "macjob4", age_seconds=25 * 3600)])
        _push_log(env, "macjob4")
        r = _run(env, "queue", "reconcile-hostjobs", "--json")
        assert r.returncode == 0, r.stderr
        it = _read_item(env, "q-rs")
        assert it["status"] == "abandoned", it
        assert "stale GC" in it["abandon_reason"]


def test_remote_row_closed_by_reconcile_is_not_revived_from_local_state():
    with tempfile.TemporaryDirectory() as tmp:
        env = _env_for_tmp(tmp)
        row = _closed_by_reconcile(_remote_row("q-rr", "macjob5", age_seconds=900))
        _write_queue(env, [row])
        _write_status(env, "macjob5", {"status": "running", "queue_id": "q-rr"})
        _run(env, "queue", "reconcile-hostjobs", "--json")
        assert _read_item(env, "q-rr")["status"] == "done"


def test_queue_add_remote_host_stamps_hostjob_host():
    with tempfile.TemporaryDirectory() as tmp:
        env = _env_for_tmp(tmp)
        r = _run(env, "queue", "add", "d", "--scope", "hostjob:x",
                 "--created-by", "hostjob", "--remote-host", "ssh://m",
                 "--force-enqueue", "--json")
        assert r.returncode == 0, r.stderr
        qid = json.loads(r.stdout)["id"]
        assert _read_item(env, qid)["hostjob_host"] == "ssh://m"


def test_local_row_without_marker_unchanged_missing_dir_closes():
    with tempfile.TemporaryDirectory() as tmp:
        env = _env_for_tmp(tmp)
        _write_queue(env, [_running_hostjob_row("q-lc", "localjob", age_seconds=3600)])
        _write_status(env, "otherjob", {"status": "running", "rc": None})
        _run(env, "queue", "reconcile-hostjobs", "--json")
        assert _read_item(env, "q-lc")["status"] == "done"


# ---------------------------------------------------------------------------
# Launch-failed class: a REMOTE hostjob whose reaper never connected the log
# sink and never heartbeat ("launch failed / no log"). Must become a terminal
# abandon (scope freed) AND an ACTIONABLE claude-event.
# ---------------------------------------------------------------------------
def _events(env):
    d = Path(env["HOME"], "claude-events")
    out = []
    if d.is_dir():
        for f in sorted(d.rglob("*.json")):
            try:
                out.append(json.loads(f.read_text()))
            except ValueError:
                pass
    return out


def _probe(env, tmp, rc):
    """Install a host-probe command that exits `rc` (0 alive / 1 gone)."""
    sh = Path(tmp, "probe.sh")
    sh.write_text("#!/bin/sh\nexit %d\n" % rc)
    sh.chmod(0o755)
    env["HOSTJOB_HOST_PROBE_CMD"] = str(sh)


def _launch_env(tmp):
    env = _env_for_tmp(tmp)
    env.pop("HOSTJOB_HOST_PROBE_CMD", None)
    for k in ("CLAUDE_EVENT_QUEUE", "CRON_EVENT_QUEUE"):
        env.pop(k, None)
    env["CLAUDE_EVENT_SESSION_TASK"] = "1"
    return env


def test_remote_no_log_no_heartbeat_past_grace_is_abandoned_with_event():
    with tempfile.TemporaryDirectory() as tmp:
        env = _launch_env(tmp)
        _probe(env, tmp, 1)
        _write_queue(env, [_remote_row("q-lf", "dubjobs", age_seconds=300)])
        r = _run(env, "queue", "reconcile-hostjobs", "--json")
        assert r.returncode == 0, r.stderr
        it = _read_item(env, "q-lf")
        assert it["status"] == "abandoned", it
        assert "launch failed" in it["abandon_reason"]
        assert it.get("death_evidence")
        evs = [e for e in _events(env) if "hostjob-launch-failed" in json.dumps(e)]
        assert evs, _events(env)
        blob = json.dumps(evs[0])
        assert "actionable" in blob and "q-lf" in blob and "dubjobs" in blob


def test_remote_launch_within_grace_untouched():
    with tempfile.TemporaryDirectory() as tmp:
        env = _launch_env(tmp)
        _write_queue(env, [_remote_row("q-lg", "fresh", age_seconds=30)])
        _run(env, "queue", "reconcile-hostjobs", "--json")
        assert _read_item(env, "q-lg")["status"] == "running"
        assert not _events(env)


def test_remote_pushed_log_for_this_run_keeps_running():
    with tempfile.TemporaryDirectory() as tmp:
        env = _launch_env(tmp)
        _write_queue(env, [_remote_row("q-ll", "quiet", age_seconds=600)])
        _push_log(env, "quiet", "sink connected\n")
        _run(env, "queue", "reconcile-hostjobs", "--json")
        assert _read_item(env, "q-ll")["status"] == "running"


def test_remote_stale_pushed_log_from_older_run_does_not_count():
    with tempfile.TemporaryDirectory() as tmp:
        env = _launch_env(tmp)
        _probe(env, tmp, 1)
        _write_queue(env, [_remote_row("q-ls", "rerun", age_seconds=300)])
        _push_log(env, "rerun", "old run\n")
        old = datetime.now(timezone.utc).timestamp() - 86400
        os.utime(Path(env["HOME"], ".config/session/hostjob-logs/rerun/log"), (old, old))
        _run(env, "queue", "reconcile-hostjobs", "--json")
        assert _read_item(env, "q-ls")["status"] == "abandoned"


def test_remote_heartbeat_after_registration_keeps_running_without_log():
    with tempfile.TemporaryDirectory() as tmp:
        env = _launch_env(tmp)
        beat = (datetime.now(timezone.utc) - timedelta(seconds=60)).isoformat()
        _write_queue(env, [_remote_row(
            "q-lb", "beating", age_seconds=300, last_heartbeat_at=beat)])
        _run(env, "queue", "reconcile-hostjobs", "--json")
        assert _read_item(env, "q-lb")["status"] == "running"


def test_queue_list_reaps_launch_failed_and_emits_once():
    with tempfile.TemporaryDirectory() as tmp:
        env = _launch_env(tmp)
        _probe(env, tmp, 1)
        _write_queue(env, [_remote_row("q-l1", "dead1", age_seconds=300)])
        _run(env, "queue", "list", "--all")
        _run(env, "queue", "list", "--all")
        assert _read_item(env, "q-l1")["status"] == "abandoned"
        evs = [e for e in _events(env) if "hostjob-launch-failed" in json.dumps(e)]
        assert len(evs) == 1, evs


def test_log_sink_creates_log_immediately():
    with tempfile.TemporaryDirectory() as tmp:
        env = _launch_env(tmp)
        r = subprocess.run(
            [sys.executable, str(SESSION_TASK), "queue", "hostjob-log-sink", "sinky"],
            input=b"", capture_output=True, env=env, timeout=15)
        assert r.returncode == 0, r.stderr
        log = Path(env["HOME"], ".config/session/hostjob-logs/sinky/log")
        assert log.exists() and b"log sink connected" in log.read_bytes()


def test_reaper_nonzero_exit_abandon_emits_actionable_hostjob_failed():
    with tempfile.TemporaryDirectory() as tmp:
        env = _launch_env(tmp)
        _write_queue(env, [_running_hostjob_row("q-hf", "crashy", age_seconds=60)])
        r = _run(env, "queue", "abandon", "q-hf", "--confirmed-dead", "--force",
                 "--reason", "hostjob exit 127 (log: /x)")
        assert r.returncode == 0, r.stderr
        evs = [e for e in _events(env) if "hostjob-failed" in json.dumps(e)]
        assert evs and "actionable" in json.dumps(evs[0])
        # exit 0 / other reasons stay quiet
        _write_queue(env, [_running_hostjob_row("q-hg", "fine", age_seconds=60)])
        n = len([e for e in _events(env) if "hostjob-failed" in json.dumps(e)])
        _run(env, "queue", "abandon", "q-hg", "--confirmed-dead", "--force",
             "--reason", "operator stop")
        assert len([e for e in _events(env) if "hostjob-failed" in json.dumps(e)]) == n


# ---------------------------------------------------------------------------
# A LIVE remote worker whose telemetry never reaches the queue must NEVER be
# abandoned on queue-side silence alone (incident: long-lived dub job alive on
# the host while its heartbeat/log RPCs were denied by the ssh allow-list).
# ---------------------------------------------------------------------------
def _tag_events(env, tag):
    return [e for e in _events(env) if tag in json.dumps(e)]


def test_live_worker_without_pushed_log_is_not_abandoned_and_alerts_once():
    with tempfile.TemporaryDirectory() as tmp:
        env = _launch_env(tmp)
        _probe(env, tmp, 0)  # host says: alive
        _write_queue(env, [_remote_row("q-al", "alive1", age_seconds=4 * 3600)])
        for _ in range(2):
            r = _run(env, "queue", "reconcile-hostjobs", "--json")
            assert r.returncode == 0, r.stderr
        it = _read_item(env, "q-al")
        assert it["status"] == "running", it
        evs = _tag_events(env, "hostjob-telemetry-broken")
        assert len(evs) == 1 and "actionable" in json.dumps(evs[0])
        assert not _tag_events(env, "hostjob-launch-failed")


def test_unprobeable_host_quarantines_not_abandons():
    with tempfile.TemporaryDirectory() as tmp:
        env = _launch_env(tmp)  # no probe configured -> unknown
        _write_queue(env, [_remote_row("q-un", "unk1", age_seconds=300)])
        _run(env, "queue", "reconcile-hostjobs", "--json")
        it = _read_item(env, "q-un")
        assert it["status"] == "quarantined", it  # scope still held
        assert len(_tag_events(env, "hostjob-telemetry-broken")) == 1
        # Re-sweeping neither abandons nor re-alerts.
        _run(env, "queue", "reconcile-hostjobs", "--json")
        assert _read_item(env, "q-un")["status"] == "quarantined"
        assert len(_tag_events(env, "hostjob-telemetry-broken")) == 1


def test_probe_failure_exit_code_is_unknown_not_gone():
    with tempfile.TemporaryDirectory() as tmp:
        env = _launch_env(tmp)
        _probe(env, tmp, 255)  # ssh failure
        _write_queue(env, [_remote_row("q-pf", "pf1", age_seconds=300)])
        _run(env, "queue", "reconcile-hostjobs", "--json")
        assert _read_item(env, "q-pf")["status"] == "quarantined"


def test_stale_24h_remote_row_without_probe_is_not_abandoned():
    with tempfile.TemporaryDirectory() as tmp:
        env = _launch_env(tmp)
        _push_log(env, "stale1")
        _write_queue(env, [_remote_row("q-st", "stale1", age_seconds=25 * 3600)])
        _run(env, "queue", "reconcile-hostjobs", "--json")
        assert _read_item(env, "q-st")["status"] == "quarantined"
