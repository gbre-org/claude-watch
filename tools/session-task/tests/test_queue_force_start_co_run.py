#!/usr/bin/env python3
"""Tests for `session-task queue force-start --no-interrupt` (co-run mode).

Andrew #4529: force-starting an item that shares a scope with a RUNNING peer
autostops/abandons that peer by default. `--no-interrupt` (alias `--co-run`)
force-starts the item ALONGSIDE the same-scope running peer WITHOUT abandoning
it -- for the case where the operator has judged the overlapping scopes safe
to run concurrently.

Covers:
  * co-run leaves the same-scope running peer RUNNING (the core deliverable).
  * the `--co-run` alias behaves identically.
  * default force-start (NO flag) still autostops the peer (regression guard).
  * co_run provenance lands on the promoted record + audit log + claude-event.

Run directly:
    python3 tools/session-task/tests/test_queue_force_start_co_run.py
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
    Path(tmp, ".config/session").mkdir(parents=True, exist_ok=True)
    Path(tmp, ".config/claude").mkdir(parents=True, exist_ok=True)
    Path(tmp, "claude-events").mkdir(parents=True, exist_ok=True)
    env["QUEUE_FORCE_START_LOG"] = str(
        Path(tmp) / ".config" / "claude" / "queue-force-start.log"
    )
    env["FORCE_START_BUNDLE_DIR"] = str(
        Path(tmp) / ".config" / "session-task" / "force-start-bundles"
    )
    env["PINGME_DISABLE"] = "1"
    env["OBLIGATIONS_FORCE_START"] = "0"
    # A live container session exports CLAUDE_EVENT_QUEUE (pointing at the
    # real event bus); drop it so emitted events land in this test's
    # HOME/claude-events sandbox rather than leaking to the host queue.
    env.pop("CLAUDE_EVENT_QUEUE", None)
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
    return _run(env, *cmd)


def _register(env, qid):
    return _run(env, "queue", "register", qid, "--json")


def _show(env, qid):
    r = _run(env, "queue", "show", qid)
    assert r.returncode == 0, r.stderr
    return json.loads(r.stdout)


def _setup_running_peer_and_blocked_sibling(env, scope):
    """Return (running_peer, blocked_sibling) dicts sharing `scope`."""
    r1 = _add(env, "the-running-peer", [scope])
    d1 = json.loads(r1.stdout)
    assert _register(env, d1["id"]).returncode == 0
    r2 = _add(env, "the-interrupter", [scope], "--force-enqueue")
    d2 = json.loads(r2.stdout)
    assert d2["ready_now"] is False, "expected blocked-pending sibling"
    return d1, d2


def test_co_run_leaves_running_peer_running():
    with tempfile.TemporaryDirectory() as tmp:
        env = _env_for_tmp(tmp)
        peer, target = _setup_running_peer_and_blocked_sibling(env, "scope:foo")

        rs = _run(
            env, "queue", "force-start", target["id"],
            "--reason", "scopes safe to overlap", "--no-interrupt", "--json",
        )
        assert rs.returncode == 0, rs.stderr
        promoted = json.loads(rs.stdout)

        assert promoted["status"] == "running"
        assert promoted.get("force_started_co_run") is True, promoted
        assert promoted.get("force_started_autostopped_peers", []) == [], (
            promoted
        )
        peer_after = _show(env, peer["id"])
        assert peer_after["status"] == "running", peer_after
        assert "autostopped_by_force_start" not in peer_after, peer_after
        assert "abandon_reason" not in peer_after, peer_after
        overridden = [
            b["id"] for b in promoted.get(
                "force_started_blockers_overridden", []
            )
        ]
        assert peer["id"] in overridden, overridden


def test_co_run_alias_leaves_peer_running():
    with tempfile.TemporaryDirectory() as tmp:
        env = _env_for_tmp(tmp)
        peer, target = _setup_running_peer_and_blocked_sibling(env, "scope:bar")

        rs = _run(
            env, "queue", "force-start", target["id"],
            "--reason", "alias path", "--co-run", "--json",
        )
        assert rs.returncode == 0, rs.stderr
        promoted = json.loads(rs.stdout)
        assert promoted.get("force_started_co_run") is True
        assert _show(env, peer["id"])["status"] == "running"


def test_default_force_start_still_autostops_peer():
    with tempfile.TemporaryDirectory() as tmp:
        env = _env_for_tmp(tmp)
        peer, target = _setup_running_peer_and_blocked_sibling(env, "scope:baz")

        rs = _run(
            env, "queue", "force-start", target["id"],
            "--reason", "interrupt as before", "--json",
        )
        assert rs.returncode == 0, rs.stderr
        promoted = json.loads(rs.stdout)
        assert promoted.get("force_started_co_run") is False, promoted
        assert peer["id"] in promoted.get(
            "force_started_autostopped_peers", []
        ), promoted
        peer_after = _show(env, peer["id"])
        assert peer_after["status"] == "abandoned", peer_after
        assert peer_after.get("autostopped_by_force_start") == target["id"]


def test_co_run_recorded_in_audit_log():
    with tempfile.TemporaryDirectory() as tmp:
        env = _env_for_tmp(tmp)
        log_path = Path(env["QUEUE_FORCE_START_LOG"])
        peer, target = _setup_running_peer_and_blocked_sibling(env, "scope:aud")

        rs = _run(
            env, "queue", "force-start", target["id"],
            "--reason", "audit-co-run", "--no-interrupt", "--json",
        )
        assert rs.returncode == 0, rs.stderr

        rows = [
            json.loads(line)
            for line in log_path.read_text().splitlines() if line.strip()
        ]
        row = rows[-1]
        assert row["queue_id"] == target["id"]
        assert row.get("co_run") is True, row
        assert row.get("autostopped_peers", []) == [], row


def test_co_run_recorded_in_claude_event():
    with tempfile.TemporaryDirectory() as tmp:
        env = _env_for_tmp(tmp)
        events_dir = Path(tmp) / "claude-events"
        peer, target = _setup_running_peer_and_blocked_sibling(env, "scope:evt")
        for f in events_dir.iterdir():
            f.unlink()

        rs = _run(
            env, "queue", "force-start", target["id"],
            "--reason", "evt-co-run", "--no-interrupt", "--json",
        )
        assert rs.returncode == 0, rs.stderr

        emitted = []
        for f in sorted(events_dir.iterdir()):
            try:
                emitted.append(json.loads(f.read_text()))
            except (OSError, ValueError):
                continue

        def _truthy(v):
            return str(v).lower() in ("true", "1", "yes")

        force_events = [e for e in emitted if e.get("tag") == "force-start"]
        assert force_events, [e.get("tag") for e in emitted]
        data = force_events[0].get("data") or {}
        assert _truthy(data.get("co_run")), data


def test_co_run_disjoint_peer_untouched():
    with tempfile.TemporaryDirectory() as tmp:
        env = _env_for_tmp(tmp)
        r1 = _add(env, "unrelated", ["scope:other"])
        d1 = json.loads(r1.stdout)
        assert _register(env, d1["id"]).returncode == 0

        r2 = _add(env, "interrupter", ["scope:mine"])
        d2 = json.loads(r2.stdout)
        rs = _run(
            env, "queue", "force-start", d2["id"],
            "--reason", "no-conflict co-run", "--no-interrupt", "--json",
        )
        assert rs.returncode == 0, rs.stderr
        assert _show(env, d1["id"])["status"] == "running"


def _all_tests():
    return [
        test_co_run_leaves_running_peer_running,
        test_co_run_alias_leaves_peer_running,
        test_default_force_start_still_autostops_peer,
        test_co_run_recorded_in_audit_log,
        test_co_run_recorded_in_claude_event,
        test_co_run_disjoint_peer_untouched,
    ]


if __name__ == "__main__":
    fail = 0
    for t in _all_tests():
        try:
            t()
            print(f"PASS: {t.__name__}")
        except Exception as e:  # noqa: BLE001
            fail += 1
            print(f"FAIL: {t.__name__}: {e}")
    sys.exit(0 if fail == 0 else 1)
