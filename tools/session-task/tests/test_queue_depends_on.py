#!/usr/bin/env python3
"""Tests for explicit interjob dependencies on the session-task queue.

Adjacency-list storage + lazy read-time compute (per Andrew's perf brief
2026-05-02). Covered:

  * queue add --depends-on attaches the field; ready_now=False until
    every dep is done.
  * queue depend <id> --add q-AAA / --remove q-BBB / --clear edits
    edges atomically.
  * Cross-group deps are allowed (different groups, different scopes).
  * Cycle detection on read: A->B->A keeps both ready_now=False, sets
    dep_cycle=true on queue show.
  * Self-deps are refused at write time (queue add --depends-on, queue
    depend --add).
  * Adding a dep onto an unknown queue id is refused.
  * spawn_instruction surfaces "deps:" blockers separately from queue
    head serialization.
  * queue show surfaces dependents (reverse edges).

Run:
    uv run --python 3.11 --with pytest \\
        pytest tests/test_queue_depends_on.py -v
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
    # Disable noisy pingme + claude-event side-effects during tests.
    env["PINGME_SESSION_TASK"] = "0"
    env["CLAUDE_EVENT_SESSION_TASK"] = "0"
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


def _add(env, desc, scopes, *extra):
    cmd = ["queue", "add", desc, "--json", "--summary", desc]
    for s in scopes:
        cmd.extend(["--scope", s])
    cmd.extend(extra)
    r = _run(env, *cmd)
    if r.returncode != 0:
        raise RuntimeError(f"add failed: {r.stderr}")
    return json.loads(r.stdout)


def _register(env, qid):
    return _run(env, "queue", "register", qid, check=True)


def _done(env, qid):
    return _run(env, "queue", "done", qid, check=True)


def _show(env, qid):
    r = _run(env, "queue", "show", qid, check=True)
    return json.loads(r.stdout)


# ---------------------------------------------------------------------------
# 1. queue add --depends-on attaches the field; ready flips on done
# ---------------------------------------------------------------------------
def test_add_with_depends_on_blocks_until_done():
    with tempfile.TemporaryDirectory() as tmp:
        env = _env_for_tmp(tmp)
        a = _add(env, "first", ["repo:a"])
        b = _add(env, "second", ["repo:b"], "--depends-on", a["id"])

        assert b["depends_on"] == [a["id"]]
        assert b["ready_now"] is False
        assert b["dep_blockers"] == [a["id"]]
        assert "deps:" in b["spawn_instruction"]

        # finishing A should flip B to ready
        _register(env, a["id"])
        _done(env, a["id"])

        b_show = _show(env, b["id"])
        assert b_show["ready_now"] is True
        assert b_show["depends_on"] == [a["id"]]
        assert b_show["dep_blockers"] == []
        assert b_show["depends_on_status"] == [
            {"id": a["id"], "status": "done"}
        ]


# ---------------------------------------------------------------------------
# 2. cross-group deps are allowed
# ---------------------------------------------------------------------------
def test_cross_group_deps_allowed():
    with tempfile.TemporaryDirectory() as tmp:
        env = _env_for_tmp(tmp)
        a = _add(env, "a-job", ["repo:lib-a"])
        b = _add(env, "b-job", ["repo:lib-b"], "--depends-on", a["id"])

        # Different groups (disjoint scope), but the dep is honored.
        assert a["group_id"] != b["group_id"]
        assert b["ready_now"] is False
        b_show = _show(env, b["id"])
        assert b_show["depends_on"] == [a["id"]]


# ---------------------------------------------------------------------------
# 3. queue depend --add / --remove / --clear
# ---------------------------------------------------------------------------
def test_depend_add_remove_clear():
    with tempfile.TemporaryDirectory() as tmp:
        env = _env_for_tmp(tmp)
        a = _add(env, "a", ["repo:a"])
        b = _add(env, "b", ["repo:b"])
        c = _add(env, "c", ["repo:c"])

        # add a -> b, c (b depends on a + c)
        r = _run(env, "queue", "depend", b["id"],
                 "--add", a["id"],
                 "--add", c["id"],
                 "--json", check=True)
        out = json.loads(r.stdout)
        assert sorted(out["depends_on"]) == sorted([a["id"], c["id"]])
        assert out["ready_now"] is False

        # remove c
        r = _run(env, "queue", "depend", b["id"],
                 "--remove", c["id"],
                 "--json", check=True)
        out = json.loads(r.stdout)
        assert out["depends_on"] == [a["id"]]

        # clear all
        r = _run(env, "queue", "depend", b["id"], "--clear",
                 "--json", check=True)
        out = json.loads(r.stdout)
        assert out["depends_on"] == []
        assert out["ready_now"] is True  # head of group, no deps


# ---------------------------------------------------------------------------
# 4. cycle PREVENTION: the edit that would close a loop is refused (2026-06-17)
# ---------------------------------------------------------------------------
def test_dep_cycle_refused_at_write_time():
    with tempfile.TemporaryDirectory() as tmp:
        env = _env_for_tmp(tmp)
        a = _add(env, "a", ["repo:a"])
        b = _add(env, "b", ["repo:b"])

        # b -> a (fine; acyclic)
        _run(env, "queue", "depend", b["id"], "--add", a["id"], check=True)
        # a -> b would create a cycle a->b->a — REFUSED, nothing persisted.
        r = _run(env, "queue", "depend", a["id"], "--add", b["id"])
        assert r.returncode != 0
        assert "cycle" in r.stderr.lower()
        # The offending dep is named in the diagnostic.
        assert b["id"] in r.stderr

        # The refused edge was NOT written: a has no deps, graph acyclic,
        # both items still make progress (a is ready, b waits on a).
        a_show = _show(env, a["id"])
        b_show = _show(env, b["id"])
        assert a_show.get("depends_on") in (None, [])
        assert a_show["dep_cycle"] is False
        assert b_show["dep_cycle"] is False
        assert a_show["ready_now"] is True
        assert b_show["depends_on"] == [a["id"]]
        assert b_show["ready_now"] is False


def test_transitive_dep_cycle_refused():
    # a->b, b->c already exist; adding c->a closes a 3-node loop -> refused.
    with tempfile.TemporaryDirectory() as tmp:
        env = _env_for_tmp(tmp)
        a = _add(env, "a", ["repo:a"])
        b = _add(env, "b", ["repo:b"])
        c = _add(env, "c", ["repo:c"])

        _run(env, "queue", "depend", a["id"], "--add", b["id"], check=True)
        _run(env, "queue", "depend", b["id"], "--add", c["id"], check=True)
        r = _run(env, "queue", "depend", c["id"], "--add", a["id"])
        assert r.returncode != 0
        assert "cycle" in r.stderr.lower()
        # c acquired no dep.
        c_show = _show(env, c["id"])
        assert c_show.get("depends_on") in (None, [])


def test_remove_breaks_preexisting_cycle_not_blocked():
    # A pre-existing cycle (constructed by editing scope on disk-ish via
    # two adds that DON'T themselves close the loop is impossible now, so
    # we verify the inverse: a --remove is never blocked by the cycle
    # guard. Build a->b, then removing it is always allowed.
    with tempfile.TemporaryDirectory() as tmp:
        env = _env_for_tmp(tmp)
        a = _add(env, "a", ["repo:a"])
        b = _add(env, "b", ["repo:b"])
        _run(env, "queue", "depend", a["id"], "--add", b["id"], check=True)
        # Removing is always allowed (loosening a gate is safe).
        r = _run(env, "queue", "depend", a["id"], "--remove", b["id"],
                 "--json", check=True)
        out = json.loads(r.stdout)
        assert out["depends_on"] == []
        assert out["ready_now"] is True
        assert out["unmet"] == []


# ---------------------------------------------------------------------------
# 5. self-dep refused
# ---------------------------------------------------------------------------
def test_self_dep_refused_on_add():
    with tempfile.TemporaryDirectory() as tmp:
        env = _env_for_tmp(tmp)
        a = _add(env, "a", ["repo:a"])
        r = _run(env, "queue", "depend", a["id"], "--add", a["id"])
        assert r.returncode != 0
        assert "itself" in r.stderr.lower() or "self" in r.stderr.lower()


def test_self_dep_refused_at_queue_add():
    with tempfile.TemporaryDirectory() as tmp:
        env = _env_for_tmp(tmp)
        # The new id can never reference itself at add time (it doesn't
        # exist yet) — the unknown-id check catches it. Verify by
        # passing a deliberately wrong dep.
        r = _run(env, "queue", "add", "x", "--summary", "x",
                 "--scope", "repo:x", "--depends-on", "q-2099-99-99-zzzz")
        assert r.returncode != 0
        assert "unknown" in r.stderr.lower()


# ---------------------------------------------------------------------------
# 6. unknown-id refused on add and depend
# ---------------------------------------------------------------------------
def test_unknown_dep_refused_on_depend():
    with tempfile.TemporaryDirectory() as tmp:
        env = _env_for_tmp(tmp)
        a = _add(env, "a", ["repo:a"])
        r = _run(env, "queue", "depend", a["id"],
                 "--add", "q-2099-01-01-aaaa")
        assert r.returncode != 0
        assert "unknown" in r.stderr.lower()


# ---------------------------------------------------------------------------
# 7. show surfaces dependents (reverse edges)
# ---------------------------------------------------------------------------
def test_show_surfaces_dependents():
    with tempfile.TemporaryDirectory() as tmp:
        env = _env_for_tmp(tmp)
        a = _add(env, "a", ["repo:a"])
        b = _add(env, "b", ["repo:b"], "--depends-on", a["id"])
        c = _add(env, "c", ["repo:c"], "--depends-on", a["id"])

        a_show = _show(env, a["id"])
        assert sorted(a_show["dependents"]) == sorted([b["id"], c["id"]])
        # a itself has no deps
        assert a_show.get("depends_on") in (None, [])


# ---------------------------------------------------------------------------
# 8. dangling dep blocks ready_now (target abandoned)
# ---------------------------------------------------------------------------
def test_abandoned_dep_keeps_blocked():
    with tempfile.TemporaryDirectory() as tmp:
        env = _env_for_tmp(tmp)
        a = _add(env, "a", ["repo:a"])
        b = _add(env, "b", ["repo:b"], "--depends-on", a["id"])

        _run(env, "queue", "abandon", a["id"], "--reason", "test",
             check=True)

        b_show = _show(env, b["id"])
        # a is abandoned, not done — b stays blocked until the operator
        # explicitly removes the dep.
        assert b_show["ready_now"] is False
        assert b_show["dep_blockers"] == [a["id"]]


# ---------------------------------------------------------------------------
# 9. safe-edit semantics on running / done items (2026-06-17)
# ---------------------------------------------------------------------------
def test_add_dep_on_running_item_refused_without_force():
    with tempfile.TemporaryDirectory() as tmp:
        env = _env_for_tmp(tmp)
        a = _add(env, "a", ["repo:a"])
        b = _add(env, "b", ["repo:b"])
        _register(env, b["id"])  # b -> running

        # ADD on a running item is refused by default.
        r = _run(env, "queue", "depend", b["id"], "--add", a["id"])
        assert r.returncode != 0
        assert "force" in r.stderr.lower()
        # Nothing was written.
        b_show = _show(env, b["id"])
        assert b_show.get("depends_on") in (None, [])

        # --force lets the operator record the edge anyway.
        r = _run(env, "queue", "depend", b["id"], "--add", a["id"],
                 "--force", "--json", check=True)
        out = json.loads(r.stdout)
        assert out["depends_on"] == [a["id"]]
        # The running item is NOT silently re-gated — status stays running.
        b_show = _show(env, b["id"])
        assert b_show["status"] == "running"


def test_remove_dep_on_running_item_allowed():
    # Loosening a gate is always safe — no --force needed, even mid-flight.
    with tempfile.TemporaryDirectory() as tmp:
        env = _env_for_tmp(tmp)
        a = _add(env, "a", ["repo:a"])
        b = _add(env, "b", ["repo:b"], "--depends-on", a["id"])
        # Satisfy the dep so b can register, then b is running with the
        # (now-met) dep edge still on it.
        _register(env, a["id"])
        _done(env, a["id"])
        _register(env, b["id"])  # b -> running, still carries task:a
        b_show = _show(env, b["id"])
        assert b_show["status"] == "running"
        assert b_show["depends_on"] == [a["id"]]
        # Removing the edge from the running item needs no --force.
        r = _run(env, "queue", "depend", b["id"], "--remove", a["id"],
                 "--json", check=True)
        out = json.loads(r.stdout)
        assert out["depends_on"] == []


def test_edit_deps_on_done_item_refused():
    with tempfile.TemporaryDirectory() as tmp:
        env = _env_for_tmp(tmp)
        a = _add(env, "a", ["repo:a"])
        b = _add(env, "b", ["repo:b"])
        _register(env, b["id"])
        _done(env, b["id"])  # b -> done (terminal)
        # Even a remove/clear is refused on a terminal item.
        for argv in (["--add", a["id"]], ["--clear"]):
            r = _run(env, "queue", "depend", b["id"], *argv)
            assert r.returncode != 0
            assert "terminal" in r.stderr.lower() or "done" in r.stderr.lower()


# ---------------------------------------------------------------------------
# 10. JSON output contract parity with `queue add` / `queue show`
# ---------------------------------------------------------------------------
def test_depend_json_reports_unmet_and_dep_blockers():
    with tempfile.TemporaryDirectory() as tmp:
        env = _env_for_tmp(tmp)
        a = _add(env, "a", ["repo:a"])
        b = _add(env, "b", ["repo:b"])

        r = _run(env, "queue", "depend", b["id"], "--add", a["id"],
                 "--json", check=True)
        out = json.loads(r.stdout)
        # mirror show: a is not done -> unmet
        assert out["depends_on"] == [a["id"]]
        assert out["unmet"] == [a["id"]]
        assert out["dep_blockers"] == [a["id"]]  # alias of unmet
        assert out["ready_now"] is False
        assert out["dep_cycle"] is False

        # finishing a clears unmet
        _register(env, a["id"])
        _done(env, a["id"])
        r = _run(env, "queue", "depend", b["id"], "--add", a["id"],
                 "--json", check=True)  # idempotent re-add
        out = json.loads(r.stdout)
        assert out["unmet"] == []
        assert out["dep_blockers"] == []
        assert out["ready_now"] is True


if __name__ == "__main__":
    import pytest

    sys.exit(pytest.main([__file__, "-v"]))
