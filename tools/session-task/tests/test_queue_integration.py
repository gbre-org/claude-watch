#!/usr/bin/env python3
"""End-to-end integration tests for session-task migration into claude-watch.

Covers the full lifecycle the migration spec required:

  * queue add (with --json)
  * register (atomic claim)
  * done (state transition + group head advance)
  * abandon (state transition + reason)
  * scope grouping (overlapping scopes -> same group)
  * ready_now serialization (second item in group blocked)
  * spawn-check (ok / blocked / not-found exit codes)

All tests run against a temp HOME so the live ~/.config/session/queue.json is
never touched.

Run:
    uv run --python 3.11 --with pytest \
        pytest tools/session-task/tests/test_queue_integration.py -v
"""

import json
import os
import subprocess
import sys
import tempfile
from pathlib import Path

SESSION_TASK = Path(__file__).resolve().parent.parent / "session-task"


def _env_for_tmp(tmp):
    """Return env with HOME pointed at tmp and pingme/claude-event suppressed."""
    env = os.environ.copy()
    env["HOME"] = tmp
    # Suppress side-effect notifications -- we only care about queue state.
    env["PINGME_SESSION_TASK"] = "0"
    env["CLAUDE_EVENT_SESSION_TASK"] = "0"
    return env


def _run(env, *argv, check=False, expect_exit=None):
    cmd = [sys.executable, str(SESSION_TASK)] + list(argv)
    r = subprocess.run(cmd, capture_output=True, text=True, env=env, timeout=15)
    if check and r.returncode != 0:
        raise RuntimeError(
            f"cmd failed rc={r.returncode}: argv={argv}\n"
            f"stdout={r.stdout!r}\nstderr={r.stderr!r}"
        )
    if expect_exit is not None and r.returncode != expect_exit:
        raise RuntimeError(
            f"expected exit {expect_exit} got {r.returncode}: argv={argv}\n"
            f"stdout={r.stdout!r}\nstderr={r.stderr!r}"
        )
    return r


def _add(env, desc, scopes, *extra):
    args = ["queue", "add", desc, "--json"]
    for s in scopes:
        args.extend(["--scope", s])
    args.extend(extra)
    r = _run(env, *args, check=True)
    return json.loads(r.stdout)


def _show(env, qid):
    r = _run(env, "queue", "show", qid, check=True)
    return json.loads(r.stdout)


def test_full_lifecycle_add_register_done():
    """Smoke test: add an item, register it, mark done."""
    with tempfile.TemporaryDirectory() as tmp:
        env = _env_for_tmp(tmp)

        added = _add(env, "lifecycle smoke", ["repo:test-a"],
                     "--summary", "smoke")
        assert added["ready_now"] is True
        assert added["scope"] == ["repo:test-a"]
        qid = added["id"]

        # Register: should mark running.
        rr = _run(env, "queue", "register", qid, "--json", check=True)
        registered = json.loads(rr.stdout)
        assert registered["status"] == "running"
        assert registered["id"] == qid

        # Done: transitions to done.
        _run(env, "queue", "done", qid, check=True)

        shown = _show(env, qid)
        assert shown["status"] == "done"


def test_abandon_with_reason():
    """abandon records the reason on the item."""
    with tempfile.TemporaryDirectory() as tmp:
        env = _env_for_tmp(tmp)
        added = _add(env, "to abandon", ["repo:abandon"],
                     "--summary", "abandon")
        qid = added["id"]
        _run(env, "queue", "register", qid, check=True)
        _run(env, "queue", "abandon", qid, "--reason", "bored", check=True)

        shown = _show(env, qid)
        assert shown["status"] == "abandoned"
        assert shown["abandon_reason"] == "bored"


def test_overlapping_scopes_share_group():
    """Two pending items with overlapping scope land in the same group."""
    with tempfile.TemporaryDirectory() as tmp:
        env = _env_for_tmp(tmp)

        a = _add(env, "first", ["repo:shared"], "--summary", "a")
        # Second item: queue add hard-fails on running scope conflict, but
        # since A hasn't been registered yet (still pending), there's no
        # running conflict. Pending+pending = same group, second is blocked.
        b = _add(env, "second", ["repo:shared"], "--summary", "b")

        assert a["group_id"] == b["group_id"], (
            "overlapping scope should merge groups"
        )
        # First-in is ready, second-in is blocked behind it.
        assert a["ready_now"] is True
        assert b["ready_now"] is False
        assert a["id"] in b["serialized_after"]


def test_disjoint_scopes_different_groups():
    """Disjoint scopes -> independent groups, both ready."""
    with tempfile.TemporaryDirectory() as tmp:
        env = _env_for_tmp(tmp)
        a = _add(env, "alpha", ["repo:foo"], "--summary", "a")
        b = _add(env, "bravo", ["repo:bar"], "--summary", "b")
        assert a["group_id"] != b["group_id"]
        assert a["ready_now"] is True
        assert b["ready_now"] is True


def test_spawn_check_ready_returns_zero():
    """spawn-check on a ready item exits 0."""
    with tempfile.TemporaryDirectory() as tmp:
        env = _env_for_tmp(tmp)
        added = _add(env, "spawn ok", ["repo:sc-ok"], "--summary", "sc-ok")
        r = _run(env, "queue", "spawn-check", added["id"], "--json")
        assert r.returncode == 0, r.stderr
        info = json.loads(r.stdout)
        assert info["ok"] is True
        assert "register" in info["spawn_instruction"]


def test_spawn_check_blocked_returns_two():
    """spawn-check on a blocked item exits 2 with ALL CAPS stderr banner."""
    with tempfile.TemporaryDirectory() as tmp:
        env = _env_for_tmp(tmp)
        a = _add(env, "first", ["repo:sb"], "--summary", "first")
        b = _add(env, "second", ["repo:sb"], "--summary", "second")
        # B is blocked behind A.
        r = _run(env, "queue", "spawn-check", b["id"])
        assert r.returncode == 2
        # Banner present.
        assert "SPAWN-CHECK FAILED" in r.stderr
        assert "DO NOT SPAWN" in r.stderr


def test_spawn_check_not_found_returns_two():
    """spawn-check on a missing id exits 2."""
    with tempfile.TemporaryDirectory() as tmp:
        env = _env_for_tmp(tmp)
        r = _run(env, "queue", "spawn-check", "q-does-not-exist")
        assert r.returncode == 2
        assert "NOT FOUND" in r.stderr


def test_register_advances_group_head_on_done():
    """When item A finishes, item B becomes ready and registerable."""
    with tempfile.TemporaryDirectory() as tmp:
        env = _env_for_tmp(tmp)
        a = _add(env, "first", ["repo:advance"], "--summary", "first")
        b = _add(env, "second", ["repo:advance"], "--summary", "second")

        # Register + complete A.
        _run(env, "queue", "register", a["id"], check=True)
        _run(env, "queue", "done", a["id"], check=True)

        # Now B should be ready.
        sc = _run(env, "queue", "spawn-check", b["id"], "--json", check=True)
        info = json.loads(sc.stdout)
        assert info["ok"] is True

        # And register-able.
        _run(env, "queue", "register", b["id"], check=True)
        shown = _show(env, b["id"])
        assert shown["status"] == "running"


def test_add_soft_serializes_on_running_scope_conflict():
    """If A is RUNNING and B has overlapping scope, queue add soft-serializes.

    Before 2026-05-19 this hard-failed (exit 3, "QUEUE ADD REFUSED"). Andrew
    flagged the wording as misleading: enqueuing behind active scope is
    normal serialization. New contract: exit 0, item enqueued,
    ready_now=false, serialized_after records the running peer.
    """
    with tempfile.TemporaryDirectory() as tmp:
        env = _env_for_tmp(tmp)
        a = _add(env, "first", ["repo:hard"], "--summary", "first")
        _run(env, "queue", "register", a["id"], check=True)

        # Second add with overlapping scope soft-serializes (exit 0).
        cmd = [sys.executable, str(SESSION_TASK), "queue", "add",
               "second", "--scope", "repo:hard", "--summary", "blocked",
               "--json"]
        r = subprocess.run(cmd, capture_output=True, text=True, env=env,
                           timeout=15)
        assert r.returncode == 0, (
            f"expected exit 0 (soft-serialize), got {r.returncode}\n"
            f"stderr={r.stderr!r}"
        )
        d = json.loads(r.stdout)
        assert d["ready_now"] is False, d
        assert a["id"] in d["serialized_after"], d
        assert d["spawn_instruction"].startswith("BLOCKED:"), d
        # No "REFUSED" panic wording, but still tell main loop to wait.
        assert "REFUSED" not in r.stderr, r.stderr


def test_queue_list_outputs_pending_and_running():
    """queue list (default) shows pending + running, not done."""
    with tempfile.TemporaryDirectory() as tmp:
        env = _env_for_tmp(tmp)
        a = _add(env, "ll-a", ["repo:ll-a"], "--summary", "a")
        b = _add(env, "ll-b", ["repo:ll-b"], "--summary", "b")
        _run(env, "queue", "register", a["id"], check=True)
        _run(env, "queue", "done", a["id"], check=True)

        r = _run(env, "queue", "list", "--json", check=True)
        items = json.loads(r.stdout)
        ids = {it["id"] for it in items}
        assert b["id"] in ids
        assert a["id"] not in ids, "done items should not appear in default list"


def test_resume_action_set_get_clear():
    """Layer 1 single-slot resume action."""
    with tempfile.TemporaryDirectory() as tmp:
        env = _env_for_tmp(tmp)
        # No action yet -> get exits 1.
        r = _run(env, "get")
        assert r.returncode == 1

        _run(env, "set", "do the thing", check=True)
        r = _run(env, "get", check=True)
        data = json.loads(r.stdout)
        assert data["task"] == "do the thing"

        _run(env, "clear", check=True)
        r = _run(env, "get")
        assert r.returncode == 1


def test_json_schema_unchanged():
    """Migrated binary writes the SAME schema_version + field shape.

    Sentinels:
      * schema_version == 2
      * top-level "items" array
      * per-item required fields: id, description, summary, scope, group_id,
        group_head, status, priority, created_at, created_by
    """
    with tempfile.TemporaryDirectory() as tmp:
        env = _env_for_tmp(tmp)
        _add(env, "schema-test", ["repo:schema"], "--summary", "schema")
        queue_path = Path(tmp) / ".config" / "session" / "queue.json"
        assert queue_path.exists()
        data = json.loads(queue_path.read_text())
        assert data["schema_version"] == 2
        assert isinstance(data["items"], list)
        assert len(data["items"]) == 1
        item = data["items"][0]
        for required in ("id", "description", "summary", "scope", "group_id",
                          "group_head", "status", "priority",
                          "created_at", "created_by"):
            assert required in item, f"item missing required field: {required}"
        assert item["status"] == "pending"
        assert item["scope"] == ["repo:schema"]


if __name__ == "__main__":
    sys.exit(subprocess.call([
        sys.executable, "-m", "pytest", "-v", __file__,
    ]))
