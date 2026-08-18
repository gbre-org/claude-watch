#!/usr/bin/env python3
"""Tests for `session-task queue force-start --new-scope` (re-scope mode).

Andrew #4713: from the queue-minisite force-start confirm modal the operator
can tick "force start with a new scope". When set, force-start reassigns the
item a NEW DISTINCT scope so it no longer overlaps the running peer -- BOTH
keep running in parallel and the peer is NOT autostopped (re-scope implies
co-run). Unchecked preserves the default interrupt/autostop behavior.

Covers:
  * bare `--new-scope` leaves the same-scope running peer RUNNING.
  * the promoted item is assigned a distinct scope (original token gone,
    ':forced-<shortid>' suffix present) and no longer overlaps the peer.
  * an explicit `--new-scope <token>` sets the scope verbatim.
  * re-scope implies co-run (force_started_co_run True, nothing autostopped).
  * provenance lands on the record + the audit log.
  * default force-start (no flag) still autostops the peer and records
    force_started_rescoped=False (regression guard).
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


def test_new_scope_leaves_running_peer_running():
    with tempfile.TemporaryDirectory() as tmp:
        env = _env_for_tmp(tmp)
        peer, target = _setup_running_peer_and_blocked_sibling(env, "scope:foo")
        rs = _run(
            env, "queue", "force-start", target["id"],
            "--reason", "run alongside on a new scope", "--new-scope", "--json",
        )
        assert rs.returncode == 0, rs.stderr
        promoted = json.loads(rs.stdout)
        assert promoted["status"] == "running"
        assert promoted.get("force_started_rescoped") is True, promoted
        assert promoted.get("force_started_co_run") is True, promoted
        assert promoted.get("force_started_autostopped_peers", []) == [], promoted
        peer_after = _show(env, peer["id"])
        assert peer_after["status"] == "running", peer_after
        assert "autostopped_by_force_start" not in peer_after, peer_after


def test_new_scope_assigns_distinct_non_overlapping_scope():
    with tempfile.TemporaryDirectory() as tmp:
        env = _env_for_tmp(tmp)
        peer, target = _setup_running_peer_and_blocked_sibling(env, "scope:bar")
        rs = _run(
            env, "queue", "force-start", target["id"],
            "--reason", "distinct scope", "--new-scope", "--json",
        )
        assert rs.returncode == 0, rs.stderr
        promoted = json.loads(rs.stdout)
        new_scope = promoted.get("scope", [])
        assert new_scope != ["scope:bar"], promoted
        assert "scope:bar" not in new_scope, promoted
        assert any(":forced-" in t for t in new_scope), promoted
        assert promoted.get("force_started_original_scope") == ["scope:bar"], promoted
        assert promoted.get("force_started_rescoped_to") == new_scope, promoted
        overridden = [
            b["id"] for b in promoted.get("force_started_blockers_overridden", [])
        ]
        assert peer["id"] in overridden, overridden


def test_new_scope_explicit_value_sets_scope_verbatim():
    with tempfile.TemporaryDirectory() as tmp:
        env = _env_for_tmp(tmp)
        peer, target = _setup_running_peer_and_blocked_sibling(env, "scope:baz")
        rs = _run(
            env, "queue", "force-start", target["id"],
            "--reason", "operator-chosen scope",
            "--new-scope", "scope:custom-lane", "--json",
        )
        assert rs.returncode == 0, rs.stderr
        promoted = json.loads(rs.stdout)
        assert promoted.get("scope", []) == ["scope:custom-lane"], promoted
        assert promoted.get("force_started_rescoped") is True, promoted
        assert _show(env, peer["id"])["status"] == "running"


def test_new_scope_recorded_in_audit_log():
    with tempfile.TemporaryDirectory() as tmp:
        env = _env_for_tmp(tmp)
        log_path = Path(env["QUEUE_FORCE_START_LOG"])
        peer, target = _setup_running_peer_and_blocked_sibling(env, "scope:aud")
        rs = _run(
            env, "queue", "force-start", target["id"],
            "--reason", "audit-rescope", "--new-scope", "--json",
        )
        assert rs.returncode == 0, rs.stderr
        rows = [
            json.loads(line)
            for line in log_path.read_text().splitlines() if line.strip()
        ]
        row = rows[-1]
        assert row["queue_id"] == target["id"]
        assert row.get("rescoped") is True, row
        assert row.get("co_run") is True, row
        assert row.get("original_scope") == ["scope:aud"], row
        assert row.get("autostopped_peers", []) == [], row


def test_default_force_start_records_rescoped_false_and_autostops():
    with tempfile.TemporaryDirectory() as tmp:
        env = _env_for_tmp(tmp)
        peer, target = _setup_running_peer_and_blocked_sibling(env, "scope:def")
        rs = _run(
            env, "queue", "force-start", target["id"],
            "--reason", "interrupt as before", "--json",
        )
        assert rs.returncode == 0, rs.stderr
        promoted = json.loads(rs.stdout)
        assert promoted.get("force_started_rescoped") is False, promoted
        assert promoted.get("scope", []) == ["scope:def"], promoted
        assert peer["id"] in promoted.get("force_started_autostopped_peers", []), promoted
        assert _show(env, peer["id"])["status"] == "abandoned"


def _all_tests():
    return [
        test_new_scope_leaves_running_peer_running,
        test_new_scope_assigns_distinct_non_overlapping_scope,
        test_new_scope_explicit_value_sets_scope_verbatim,
        test_new_scope_recorded_in_audit_log,
        test_default_force_start_records_rescoped_false_and_autostops,
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
