#!/usr/bin/env python3
"""Tests for `session-task queue update-scope ID TOKEN [TOKEN...]`.

Added 2026-05-20 alongside the `workload run` auto-injection fix
(q-2026-05-20-7482). Background: when the main loop queues an item
with a non-workload scope (e.g. `resource:promote-4-shows`) and then
fires `workload run LABEL -- CMD --queue-id <qid>`, the queue item
has no `workload:<label>` scope token. The work-queue-exporter uses
that token to find the heartbeat file under `/run/claude/workloads/`
and emit `worktask_queue_progress_age_seconds`. Without the token
`WorkQueueStuck` + `WorkQueueStuckSoft` false-fire after 1h on a
healthy active workload. `workload run` now calls this subcommand to
append the token; this test suite covers the subcommand contract.

Coverage:
    1. Add appends the token to existing scope (no replace)
    2. Add is idempotent (no duplicate, exit 0)
    3. Add multiple tokens in one call
    4. Comma-joined tokens accepted (--syntax parity with `queue add --scope`)
    5. --remove strips listed tokens
    6. --remove is idempotent (token absent → no-op, exit 0)
    7. Missing item ID exits 1 with stderr `not found`
    8. --json shape contains id/scope/added/removed
    9. The hard-fail on manual `workload:` adds (cmd_queue_add) does NOT
       apply here — `update-scope` is the inverse: the workload runner
       NEEDS to inject its own token.

Run:
    uv run --python 3.11 --with pytest \\
        pytest tools/session-task/tests/test_queue_update_scope.py -v
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


def _run(env, *argv, check=False, timeout=15):
    cmd = [sys.executable, str(SESSION_TASK)] + list(argv)
    r = subprocess.run(
        cmd, capture_output=True, text=True, env=env, timeout=timeout
    )
    if check and r.returncode != 0:
        raise RuntimeError(
            f"command failed rc={r.returncode}\n"
            f"  cmd: {' '.join(argv)}\n"
            f"  stdout: {r.stdout}\n"
            f"  stderr: {r.stderr}"
        )
    return r


def _add(env, desc, *scope_tokens):
    cmd = ["queue", "add", desc, "--summary", "t", "--json"]
    for s in scope_tokens:
        cmd.extend(["--scope", s])
    r = _run(env, *cmd, check=True)
    return json.loads(r.stdout)["id"]


def _scope(env, qid):
    r = _run(env, "queue", "scope", qid, check=True)
    return [line for line in r.stdout.splitlines() if line.strip()]


# ---------------------------------------------------------------------------
# 1. Add appends to existing scope (no replace)
# ---------------------------------------------------------------------------


def test_add_appends_without_replacing():
    with tempfile.TemporaryDirectory() as tmp:
        env = _env_for_tmp(tmp)
        qid = _add(env, "rsync four shows", "resource:promote-4-shows")
        r = _run(
            env, "queue", "update-scope", qid, "workload:promote-3-shows",
            check=True,
        )
        assert "added to" in r.stdout
        scope = _scope(env, qid)
        assert "resource:promote-4-shows" in scope
        assert "workload:promote-3-shows" in scope


# ---------------------------------------------------------------------------
# 2. Add is idempotent
# ---------------------------------------------------------------------------


def test_add_is_idempotent():
    with tempfile.TemporaryDirectory() as tmp:
        env = _env_for_tmp(tmp)
        qid = _add(env, "x", "resource:foo")
        _run(env, "queue", "update-scope", qid, "workload:bar", check=True)
        r = _run(env, "queue", "update-scope", qid, "workload:bar", check=True)
        assert "no-op" in r.stdout
        scope = _scope(env, qid)
        # Token must appear exactly once.
        assert scope.count("workload:bar") == 1


# ---------------------------------------------------------------------------
# 3. Add multiple tokens in one call
# ---------------------------------------------------------------------------


def test_add_multiple_tokens():
    with tempfile.TemporaryDirectory() as tmp:
        env = _env_for_tmp(tmp)
        qid = _add(env, "x", "resource:foo")
        _run(
            env, "queue", "update-scope", qid,
            "workload:a", "workload:b", "repo:claude-watch",
            check=True,
        )
        scope = _scope(env, qid)
        assert "workload:a" in scope
        assert "workload:b" in scope
        assert "repo:claude-watch" in scope
        assert "resource:foo" in scope


# ---------------------------------------------------------------------------
# 4. Comma-joined tokens accepted
# ---------------------------------------------------------------------------


def test_comma_joined_tokens_accepted():
    with tempfile.TemporaryDirectory() as tmp:
        env = _env_for_tmp(tmp)
        qid = _add(env, "x", "resource:foo")
        _run(
            env, "queue", "update-scope", qid,
            "workload:a,workload:b",
            check=True,
        )
        scope = _scope(env, qid)
        assert "workload:a" in scope
        assert "workload:b" in scope


# ---------------------------------------------------------------------------
# 5. --remove strips listed tokens
# ---------------------------------------------------------------------------


def test_remove_strips_listed_tokens():
    with tempfile.TemporaryDirectory() as tmp:
        env = _env_for_tmp(tmp)
        qid = _add(env, "x", "resource:foo")
        _run(env, "queue", "update-scope", qid, "workload:bar", check=True)
        r = _run(
            env, "queue", "update-scope", qid, "workload:bar", "--remove",
            check=True,
        )
        assert "removed" in r.stdout
        scope = _scope(env, qid)
        assert "workload:bar" not in scope
        # The pre-existing token is untouched.
        assert "resource:foo" in scope


# ---------------------------------------------------------------------------
# 6. --remove is idempotent on missing tokens
# ---------------------------------------------------------------------------


def test_remove_is_idempotent_on_missing_token():
    with tempfile.TemporaryDirectory() as tmp:
        env = _env_for_tmp(tmp)
        qid = _add(env, "x", "resource:foo")
        r = _run(
            env, "queue", "update-scope", qid, "workload:nope", "--remove",
            check=True,
        )
        assert "no-op" in r.stdout
        scope = _scope(env, qid)
        assert scope == ["resource:foo"]


# ---------------------------------------------------------------------------
# 7. Missing item ID exits 1
# ---------------------------------------------------------------------------


def test_missing_item_id_exits_1():
    with tempfile.TemporaryDirectory() as tmp:
        env = _env_for_tmp(tmp)
        r = _run(
            env, "queue", "update-scope", "q-does-not-exist", "workload:x",
        )
        assert r.returncode == 1
        assert "not found" in r.stderr


# ---------------------------------------------------------------------------
# 8. --json shape
# ---------------------------------------------------------------------------


def test_json_shape_on_add():
    with tempfile.TemporaryDirectory() as tmp:
        env = _env_for_tmp(tmp)
        qid = _add(env, "x", "resource:foo")
        r = _run(
            env, "queue", "update-scope", qid, "workload:bar", "--json",
            check=True,
        )
        d = json.loads(r.stdout)
        assert d["id"] == qid
        assert "workload:bar" in d["scope"]
        assert "resource:foo" in d["scope"]
        assert d["added"] == ["workload:bar"]
        assert d["removed"] == []


def test_json_shape_on_remove():
    with tempfile.TemporaryDirectory() as tmp:
        env = _env_for_tmp(tmp)
        qid = _add(env, "x", "resource:foo")
        _run(env, "queue", "update-scope", qid, "workload:bar", check=True)
        r = _run(
            env, "queue", "update-scope", qid, "workload:bar",
            "--remove", "--json",
            check=True,
        )
        d = json.loads(r.stdout)
        assert d["id"] == qid
        assert "workload:bar" not in d["scope"]
        assert d["removed"] == ["workload:bar"]
        assert d["added"] == []


# ---------------------------------------------------------------------------
# 9. The cmd_queue_add manual-workload-scope HARD-DENY does NOT apply
#    to update-scope. This is the whole point of the subcommand: the
#    workload runner can inject its own token onto a pre-registered
#    item that the main loop queued with a non-workload scope.
# ---------------------------------------------------------------------------


def test_workload_scope_injection_is_allowed_without_force_enqueue():
    with tempfile.TemporaryDirectory() as tmp:
        env = _env_for_tmp(tmp)
        # Mimic the q-2026-05-20-13b9 case: item created with a
        # resource: scope, no workload: token, no --force-enqueue.
        qid = _add(env, "rsync 4 shows", "resource:promote-4-shows")
        # No --force-enqueue, no QUEUE_GATE_BYPASS. update-scope MUST
        # still allow injecting the workload: token.
        r = _run(
            env, "queue", "update-scope", qid, "workload:promote-3-shows",
        )
        assert r.returncode == 0, (
            f"update-scope must accept workload: token without "
            f"--force-enqueue (stderr={r.stderr})"
        )
        scope = _scope(env, qid)
        assert "workload:promote-3-shows" in scope
